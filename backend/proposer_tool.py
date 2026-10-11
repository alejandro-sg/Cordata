"""Proactive counterpart to skill_forge_tool.py's reactive self-healing
loop: given an observation (not necessarily a caught error), drafts either a
new backend/skills_generated/*.py tool or a full-content replacement for an
existing tracked file, gated by a static guard, a deterministic test run
against a throwaway copy of backend/, and an LLM critic — in that order —
before the draft is ever shown to a human for approval.

Like skill_forge_tool.py, none of the sub-agents below is ever given
file_write/editor. See that module's docstring for the full reasoning: a
sub-agent built here has no AG-UI transport wrapping it, so it has no
mechanism to be interrupted — only the *top-level* agent's own
file_write/editor call, made on the draft this tool returns, is genuinely
gated by ag_ui_strands' StrandsInterruptHook. This tool never writes
anything to disk itself.

skill_validation_hooks.py's GUARDRAIL_FILES (skill_validation_hooks.py and
this file itself) is the one thing that must never be reachable through a
Proposer-authored change — a drafted change targeting either is refused in
_run_static_gate, in Python the model cannot edit through this tool's own
contract, not as a rule it's merely asked to follow. A bad proposal must
never be able to remove the check meant to catch bad proposals.
"""

from __future__ import annotations

import ast
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from strands import Agent, tool
from strands_tools import file_read

import skill_validation_hooks as svh
from prompt_security import SecretRedactionHooks

_BACKEND_DIR = Path(__file__).resolve().parent

# Re-run against the sandboxed copy in _run_test_gate — an explicit list
# (not "every _*_smoke_test.py glob") so adding a new smoke test to the repo
# doesn't silently change what gates a harness change without a deliberate
# decision to include it here.
_REGRESSION_SMOKE_TESTS = [
    "_subagent_denylist_smoke_test.py",
    "_secret_guard_smoke_test.py",
    "_sub_agent_model_smoke_test.py",
    "_agent_memory_smoke_test.py",
    "_local_graph_structure_smoke_test.py",
]

# Text a drafted replacement for main.py/agent_common.py must still contain
# — best-effort, not sound (a determined rewrite could dodge these exact
# tokens while still weakening the gate in spirit), but catches the
# straightforward "the diff just deletes the gating line" case outright.
_GUARDRAIL_MARKERS = ["_APPROVAL_REQUIRED_TOOLS", "ToolBehavior(interrupt_on_call=True)", "SkillValidationHooks()"]

_ANALYST_PROMPT = (
    "You are an improvement analyst for this codebase. You will be given an "
    "observation -- a recurring pattern, or a specific human ask. Reason "
    "it through, then propose ONE concrete, narrow change: either a new tool for "
    "skills_generated/, or a specific edit to one existing tracked file. "
    "Respond with: which file, why, and exactly what should change. Never "
    "propose editing skill_validation_hooks.py or proposer_tool.py -- those "
    "are permanently off-limits, no exceptions."
)

_WRITER_PROMPT = (
    "You are a drafting specialist. You will be given a proposed change. "
    "Sanity-check it, then draft the FULL replacement content "
    "for the target file -- not a diff or patch, the complete file, ready "
    "to write as-is. You may call file_read to see a tracked file's current "
    "content before drafting your replacement of it, but never write "
    "anything yourself -- you have no way to show a human what you wrote "
    "before it lands on disk, so drafting it as text is the only safe "
    "option. Respond with exactly this format and nothing else before or "
    "after:\n"
    "FILE: <path relative to backend/, e.g. agent_common.py or "
    "skills_generated/foo.py>\n"
    "---\n"
    "<the complete file content>"
)

_CRITIC_PROMPT = (
    "You are a skeptical critic reviewing a proposed code change, alongside "
    "confirmation that it already passed a static guard and the regression "
    "test suite. Reason it through, then respond with exactly one word first -- "
    "PASS or FAIL -- followed by one or two sentences of reasoning. FAIL if "
    "the change is broader than what was asked, touches something "
    "unrelated to the stated goal, or you're not confident it's correct --"
    " when in doubt, FAIL and let a human decide instead of guessing."
)


def _run_analyst(build_model, observation: str) -> str:
    agent = Agent(model=build_model(), system_prompt=_ANALYST_PROMPT, tools=[], callback_handler=None)
    return str(agent(observation))


def _run_writer(build_model, proposal: str) -> str:
    # No file_write/editor here — see module docstring.
    agent = Agent(
        model=build_model(),
        system_prompt=_WRITER_PROMPT,
        tools=[file_read],
        hooks=[SecretRedactionHooks()],
        callback_handler=None,
    )
    return str(agent(proposal))


def _run_critic(build_model, proposal: str, draft: str) -> str:
    agent = Agent(model=build_model(), system_prompt=_CRITIC_PROMPT, tools=[], callback_handler=None)
    return str(agent(f"Proposal:\n{proposal}\n\nDraft (already passed the static guard and regression tests):\n{draft}"))


def _parse_draft(draft: str) -> tuple[str, str] | None:
    """Pull (relative_path, content) out of a writer's FILE:/---/content reply."""
    if "FILE:" not in draft or "---" not in draft:
        return None
    header, _, content = draft.partition("---")
    path_line = next((line for line in header.splitlines() if line.strip().startswith("FILE:")), None)
    if path_line is None:
        return None
    path = path_line.split("FILE:", 1)[1].strip()
    if not path:
        return None
    return path, content.strip("\n")


def _dotted_call_name(node: ast.Call) -> str:
    func = node.func
    parts: list[str] = []
    while isinstance(func, ast.Attribute):
        parts.append(func.attr)
        func = func.value
    if isinstance(func, ast.Name):
        parts.append(func.id)
    return ".".join(reversed(parts))


def _run_static_gate(path: str, content: str) -> str | None:
    """Returns None if the draft passes, else a human-readable refusal."""
    normalized = path.replace("\\", "/").lstrip("/")
    basename = normalized.rsplit("/", 1)[-1]
    if basename in svh.GUARDRAIL_FILES or normalized in svh.GUARDRAIL_FILES:
        return f"Refused: '{path}' is a permanently off-limits guardrail file — no drafted change may ever target it."
    if ".." in Path(normalized).parts:
        return f"Refused: target path '{path}' escapes the backend/ directory."

    try:
        tree = ast.parse(content)
    except SyntaxError as exc:
        return f"Refused: drafted content for '{path}' is not valid Python ({exc})."

    banned_reason = svh.find_banned_usage(tree)
    if banned_reason:
        return f"Refused: drafted content for '{path}' {banned_reason}."

    if normalized in {"main.py", "agent_common.py"}:
        missing = [marker for marker in _GUARDRAIL_MARKERS if marker not in content]
        if missing:
            return (
                f"Refused: drafted replacement for '{path}' is missing required guardrail text: "
                f"{', '.join(missing)}."
            )

    return None


def _run_test_gate(path: str, content: str) -> str | None:
    """Applies the draft to a throwaway copy of backend/ and runs the
    regression smoke tests against it. Returns None if all pass, else a
    failure summary. Never touches the real backend/ directory — every
    write here is inside a fresh tempfile.TemporaryDirectory, deleted on
    exit regardless of outcome.
    """
    with tempfile.TemporaryDirectory(prefix="proposer_gate_") as tmp:
        sandbox = Path(tmp) / "backend"
        shutil.copytree(
            _BACKEND_DIR,
            sandbox,
            ignore=shutil.ignore_patterns(".venv", "__pycache__", "sessions", "workspace", "repl_state", ".git"),
        )
        target = sandbox / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)

        failures = []
        for test_name in _REGRESSION_SMOKE_TESTS:
            test_path = sandbox / test_name
            if not test_path.exists():
                continue
            try:
                result = subprocess.run(
                    [sys.executable, str(test_path)], cwd=str(sandbox), capture_output=True, text=True, timeout=120
                )
            except subprocess.TimeoutExpired:
                failures.append(f"{test_name}: timed out after 120s")
                continue
            if result.returncode != 0:
                tail = (result.stdout[-800:] + result.stderr[-800:]).strip()
                failures.append(f"{test_name}: {tail}")

        if failures:
            return "Refused: regression test(s) failed against the drafted change:\n" + "\n---\n".join(failures)
        return None


@tool
def proposer(observation: str) -> str:
    """Propose a change — a new skills_generated/ tool, or a full-content
    replacement for an existing tracked file — for a human to review.

    Unlike skill_forge (which reacts to one specific tool-call failure),
    use this for any concrete improvement idea: a pattern you've noticed
    repeat, or something a human explicitly asked you to look into. Runs
    five fixed stages in order — analyst, writer, a static guard, a
    regression-test gate against a sandboxed copy of backend/, and a critic
    — and returns a draft only if the static guard, test gate, AND critic
    all pass; a refusal at any stage returns why, with nothing drafted.

    This tool never writes anything to disk itself — a sub-agent built here
    has no way to pause for your approval, so it never gets file_write or
    editor directly (see this module's docstring for the full reasoning).
    If it returns a draft, read it yourself, and only if you agree, call
    file_write or editor yourself with exactly that path and content, as
    your own next tool call — that real, top-level write is what actually
    pauses for human approval; nothing before it does.

    skill_validation_hooks.py and proposer_tool.py itself are permanently
    off-limits as a target, enforced here in Python — no drafted change can
    talk its way past that check.

    Args:
        observation: What prompted this — a recurring pattern you've
            noticed, or an explicit human request. Be specific; a fresh
            sub-agent sees only what you pass here.
    """
    from agent_common import build_model

    proposal = _run_analyst(build_model, observation)
    draft = _run_writer(build_model, proposal)

    parsed = _parse_draft(draft)
    if parsed is None:
        return (
            f"Proposal:\n{proposal}\n\n"
            f"Refused: writer did not respond in the required FILE:/---/content format, "
            f"so nothing was checked or drafted:\n{draft}"
        )
    path, content = parsed

    static_result = _run_static_gate(path, content)
    if static_result is not None:
        return f"Proposal:\n{proposal}\n\n{static_result}"

    test_result = _run_test_gate(path, content)
    if test_result is not None:
        return f"Proposal:\n{proposal}\n\n{test_result}"

    critic = _run_critic(build_model, proposal, draft)
    if not critic.strip().upper().startswith("PASS"):
        return f"Proposal:\n{proposal}\n\nRefused: critic did not approve this change:\n{critic}"

    # path is relative-to-backend/ throughout the gates above (that's what
    # _run_static_gate/_run_test_gate's own sandbox-copy logic expects) —
    # but the orchestrator's own cwd is backend/workspace/ (agent_common.py's
    # os.chdir), not backend/, so a bare relative path handed to a real
    # file_write/editor call would resolve to the wrong location entirely
    # and silently miss every gate that already ran against it. Only this
    # final, orchestrator-facing line needs the absolute form.
    absolute_path = (_BACKEND_DIR / path).resolve()
    return (
        f"Proposal:\n{proposal}\n\n"
        f"Static gate: passed\n"
        f"Test gate: passed (regression smoke tests still pass against a sandboxed copy)\n"
        f"Critic: {critic}\n\n"
        f"Drafted change (NOT written yet — call file_write or editor yourself with this exact path and content):\n"
        f"FILE: {absolute_path}\n---\n{content}"
    )

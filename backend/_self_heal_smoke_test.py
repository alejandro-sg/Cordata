"""Self-healing loop, end to end with real tools on a throwaway project; the
models are scripted, so it runs offline and deterministically.

The project: ``calc.py`` subtracts where it should add; ``check.py`` fails.

1. heal: the orchestrator runs the failing check -> the hook asks the Debugger
   -> the patch is verified in a sandbox copy (the real file untouched) -> the
   note reaches the model -> it applies the patch -> re-runs -> healed.
2. iterate: the first patch is wrong, the sandbox rejects it, the Debugger is
   asked again *with the failed attempt* and the second patch is verified.
3. auto-continue: the model ends its turn without fixing -> AfterInvocation
   resume -> it continues on its own and heals.
4. escalate: every patch fails -> after maxAttempts the model is told to stop.
5. the real Debugger agent (scripted model, real read-only tools) produces a
   parseable, dry-run-checked diagnosis.
6. autoApply pre-approves only the verified patch and the failing command.
7. state mismatch: a failed edit gets a re-read instruction.

    backend/.venv/bin/python _self_heal_smoke_test.py
"""

import os
import re
import sys
import tempfile
from pathlib import Path

os.environ["SIERRALAB_STATE_DB"] = os.path.join(tempfile.mkdtemp(prefix="heal-ledger-"), "state.db")  # remembered fixes go to a private ledger
os.environ.setdefault("BYPASS_TOOL_CONSENT", "true")
os.environ.setdefault("STRANDS_NON_INTERACTIVE", "true")

from strands import Agent  # noqa: E402
from strands import tool as strands_tool  # noqa: E402
from strands_tools import editor, shell  # noqa: E402

import harness_events  # noqa: E402
import patch_tool  # noqa: E402
import specialists  # noqa: E402
from harness_testing import ScriptedModel, check, last_text, text, tool as call  # noqa: E402
from self_heal import HealConfig, SelfHealHooks, current_excerpt  # noqa: E402

CHECK = f"{sys.executable} check.py"
BUGGY = "def add(a, b):\n    return a - b\n"
GOOD_PATCH = "--- a/calc.py\n+++ b/calc.py\n@@ -1,2 +1,2 @@\n def add(a, b):\n-    return a - b\n+    return a + b\n"
WRONG_PATCH = "--- a/calc.py\n+++ b/calc.py\n@@ -1,2 +1,2 @@\n def add(a, b):\n-    return a - b\n+    return a * b\n"


def project() -> Path:
    root = Path(tempfile.mkdtemp(prefix="heal-proj-"))
    (root / "calc.py").write_text(BUGGY)
    (root / "check.py").write_text("from calc import add\nassert add(2, 3) == 5, f'add(2, 3) = {add(2, 3)}'\nprint('ok')\n")
    os.chdir(root)
    return root


class FakeDiagnose:
    def __init__(self, patches: list[str | None]) -> None:
        self.patches = patches
        self.calls: list[tuple[str, list[str]]] = []

    def __call__(self, failure, previous, root=None, session_id=None):
        self.calls.append((failure, list(previous)))
        patch = self.patches[min(len(self.calls) - 1, len(self.patches) - 1)]
        return specialists.parse_diagnosis(
            f"ROOT CAUSE: add() subtracts\nPATCH:\n```diff\n{patch}```\nVERIFY: {CHECK}" if patch else "ROOT CAUSE: missing interpreter\nPATCH: none"
        )


def patch_from_note(messages, *_):
    found = re.search(r"```diff\n(.*?)```", last_text(messages), re.DOTALL)
    assert found, f"no patch in the note: {last_text(messages)[-300:]!r}"
    return call("apply_patch", {"patch": found.group(1)})


def run(steps, diagnose, config=None):
    events: list = []
    unsubscribe = harness_events.subscribe(lambda kind, payload: kind == "heal" and events.append(payload))
    hooks = SelfHealHooks(config=config or HealConfig(max_attempts=3), diagnose=diagnose)
    agent = Agent(model=ScriptedModel(steps), tools=[shell, patch_tool.apply_patch, editor], hooks=[hooks], callback_handler=None)
    result = agent("make the check pass")
    unsubscribe()
    return agent, hooks, events, result


def phases(events):
    return [e["phase"] for e in events]


def tool_texts(agent):
    return ["\n".join(c.get("text", "") for c in b["toolResult"]["content"]) for m in agent.messages for b in m["content"] if "toolResult" in b]


def test_heal() -> None:
    root = project()
    diagnose = FakeDiagnose([GOOD_PATCH])
    seen_buggy_during_verify = []

    original_apply = patch_tool.apply

    def spy(patch, where):
        seen_buggy_during_verify.append((root / "calc.py").read_text() == BUGGY)
        return original_apply(patch, where)

    patch_tool.apply = spy
    try:
        agent, hooks, events, _ = run([call("shell", {"command": CHECK}), patch_from_note, call("shell", {"command": CHECK}), text("fixed")], diagnose)
    finally:
        patch_tool.apply = original_apply
    texts = tool_texts(agent)
    check("Exit Code: 1" in texts[0] and "[self-heal] Debugger diagnosis" in texts[0], "the failing run's result carries the diagnosis")
    check("passed there" in texts[0] and "```diff" in texts[0], "…with a sandbox-verified patch")
    check(seen_buggy_during_verify[0], "sandbox verification ran while the real file was still untouched")
    check((root / "calc.py").read_text() == "def add(a, b):\n    return a + b\n", "the model's apply_patch fixed the real file")
    check("[self-heal] Verified" in texts[2], "the re-run closes the episode")
    check(phases(events) == ["detected", "diagnosing", "proposed", "verifying", "verified", "healed"], f"phases in order ({phases(events)})")
    check(not hooks.open_episodes(), "no episode left open")
    check(CHECK in diagnose.calls[0][0] and "AssertionError" in diagnose.calls[0][0], "the Debugger got the command and the stderr trace")


def test_iterates_on_a_failed_verification() -> None:
    root = project()
    diagnose = FakeDiagnose([WRONG_PATCH, GOOD_PATCH])
    agent, hooks, events, _ = run([call("shell", {"command": CHECK}), patch_from_note, call("shell", {"command": CHECK}), text("fixed")], diagnose)
    check(phases(events)[:7] == ["detected", "diagnosing", "proposed", "verifying", "verify_failed", "diagnosing", "proposed"], f"a rejected patch is re-diagnosed ({phases(events)})")
    check(len(diagnose.calls) == 2 and "a * b" in diagnose.calls[1][1][0], "the second round sees the failed attempt (stateless specialist)")
    check("add(2, 3) = 6" in diagnose.calls[1][0], "…and the sandbox's new failure output")
    check((root / "calc.py").read_text().endswith("a + b\n"), "only the verified patch reached the real tree")


def test_auto_continue() -> None:
    root = project()
    agent, hooks, events, result = run(
        [call("shell", {"command": CHECK}), text("I found the bug."), patch_from_note_from_history, call("shell", {"command": CHECK}), text("fixed now")],
        FakeDiagnose([GOOD_PATCH]),
    )
    check("continuing" in phases(events) and phases(events)[-1] == "healed", f"the agent resumed on its own and healed ({phases(events)})")
    check(str(result).strip() == "fixed now" and (root / "calc.py").read_text().endswith("a + b\n"), "the final answer comes after the autonomous continuation")


def patch_from_note_from_history(messages, *_):
    for message in reversed(messages):
        for block in message.get("content") or []:
            for c in (block.get("toolResult") or {}).get("content") or []:
                found = re.search(r"```diff\n(.*?)```", c.get("text", ""), re.DOTALL)
                if found:
                    return call("apply_patch", {"patch": found.group(1)})
    raise AssertionError("no patch in history")


def test_escalates() -> None:
    project()
    agent, hooks, events, _ = run([call("shell", {"command": CHECK}), text("giving up")], FakeDiagnose([WRONG_PATCH]), HealConfig(max_attempts=2))
    texts = tool_texts(agent)
    check(phases(events).count("verify_failed") == 2 and phases(events)[-1] == "escalated", f"bounded: escalates after maxAttempts ({phases(events)})")
    check("could not produce a verified fix in 2 attempt(s)" in texts[0], "the model is told to stop and report")
    check(not hooks.open_episodes(), "an escalated episode doesn't auto-continue")


def test_environmental_failure() -> None:
    project()
    diagnose = FakeDiagnose([None])
    agent, hooks, events, _ = run([call("shell", {"command": f"{sys.executable} -c 'import sys; sys.exit(3)'"}), text("ok")], diagnose)
    check(len(diagnose.calls) == 1 and "no_patch" in phases(events), "an environmental failure isn't retried in a loop")
    diagnose = FakeDiagnose([GOOD_PATCH])
    agent, hooks, events, _ = run([call("shell", {"command": "apply_patch"}), text("ok")], diagnose)
    check(diagnose.calls == [] and phases(events) == ["command_missing"], "exit 127 (command not found) skips the Debugger entirely")
    check("call the tool directly" in tool_texts(agent)[0], "…and tells the model it probably meant a tool")


def test_real_debugger_agent() -> None:
    root = project()
    model = ScriptedModel(
        [
            call("file_read", {"path": "calc.py", "mode": "view"}),
            text(f"ROOT CAUSE: add() uses - instead of +\nPATCH:\n```diff\n{GOOD_PATCH}```\nVERIFY: {CHECK}"),
        ]
    )
    diagnosis = specialists.diagnose("Command: python check.py\nAssertionError", model=model, root=str(root))
    check(diagnosis.actionable and diagnosis.verify == CHECK, f"the Debugger's answer parses and its patch dry-runs ({diagnosis.root_cause!r})")
    check(model.calls[0]["tools"] and "apply_patch" not in model.calls[0]["tools"] and "shell" not in model.calls[0]["tools"], "proposal mode has read-only tools only")
    check((root / "calc.py").read_text() == BUGGY, "diagnosing never touches the tree")
    bad = ScriptedModel([text("ROOT CAUSE: x\nPATCH:\n```diff\n--- a/calc.py\n+++ b/calc.py\n@@ -1 +1 @@\n-nope\n+x\n```"), text(f"ROOT CAUSE: fixed\nPATCH:\n```diff\n{GOOD_PATCH}```\nVERIFY: {CHECK}")])
    diagnosis = specialists.diagnose("failure", model=bad, root=str(root))
    check(diagnosis.actionable and "does not apply" in bad.calls[1]["last_text"], "a patch that doesn't apply is sent back once with the error")


def test_preapproval() -> None:
    hooks = SelfHealHooks(config=HealConfig(auto_apply=True), diagnose=FakeDiagnose([GOOD_PATCH]))
    from self_heal import Episode

    hooks.episodes["x"] = Episode(command=CHECK, patch=GOOD_PATCH, verified=True, verify_command=CHECK)
    check(hooks.preapproved("apply_patch", {"patch": GOOD_PATCH}), "autoApply pre-approves the verified patch")
    check(not hooks.preapproved("apply_patch", {"patch": WRONG_PATCH}), "…not any other patch")
    check(hooks.preapproved("shell", {"command": CHECK}) and not hooks.preapproved("shell", {"command": "rm -rf /"}), "…and only the failing command")
    hooks.episodes["x"].verified = False
    check(not hooks.preapproved("apply_patch", {"patch": GOOD_PATCH}), "an unverified patch is never pre-approved")
    off = SelfHealHooks(config=HealConfig(auto_apply=False), diagnose=FakeDiagnose([GOOD_PATCH]))
    off.episodes["x"] = Episode(command=CHECK, patch=GOOD_PATCH, verified=True)
    check(not off.preapproved("apply_patch", {"patch": GOOD_PATCH}), "without autoApply nothing is pre-approved")


def test_state_mismatch() -> None:
    project()
    agent, hooks, events, _ = run(
        [
            call("apply_patch", {"patch": "--- a/calc.py\n+++ b/calc.py\n@@ -1 +1 @@\n-nothing like this\n+x\n"}),
            call("apply_patch", {"patch": "just some prose, not a diff"}),
            call("editor", {"command": "view_range", "path": "calc.py"}),
            text("ok"),
        ],
        FakeDiagnose([GOOD_PATCH]),
    )
    texts = tool_texts(agent)
    check("State mismatch" in texts[0] and "state_mismatch" in phases(events), "a hunk that doesn't match the file → a state-mismatch note")
    check("Current calc.py, lines 1" in texts[0] and "def add(a, b):" in texts[0] and "return a - b" in texts[0], "…carrying the file's current lines, so the next patch is written against what is there")
    check("Use a unified diff" in texts[1] and "State mismatch" not in texts[1], "a patch in an unreadable format → a format note, not 'state mismatch'")
    check("[self-heal]" not in texts[2], "an unknown editor command gets no misleading note")
    check(current_excerpt(Path.cwd(), "../../etc/passwd", 1) is None, "the excerpt never reaches outside the project")
    (Path.cwd() / ".env").write_text("TOKEN=abc\n")
    check(current_excerpt(Path.cwd(), ".env", 1) is None, "…or into a secrets file, even one that exists")


def test_claude_code_failing_check() -> None:
    root = project()
    (root / "run_tests.sh").write_text(f"#!/bin/sh\n{CHECK}\n")

    @strands_tool(name="claude_code_engineer")
    def fake_claude(task: str) -> dict:
        """Stand-in for the Claude Code CLI wrapper: it edited, ran its tests, and they failed."""
        steps = [{"tool": "Edit", "detail": "calc.py", "ok": True}, {"tool": "Bash", "detail": "bash run_tests.sh", "ok": False}]
        return {"status": "success", "content": [{"text": "I changed calc.py; the tests still fail."}, {"json": {"tool": "claude_code_engineer", "steps": steps}}]}

    events: list = []
    unsubscribe = harness_events.subscribe(lambda kind, payload: kind == "heal" and events.append(payload))
    hooks = SelfHealHooks(config=HealConfig(max_attempts=3), diagnose=FakeDiagnose([GOOD_PATCH]))
    agent = Agent(
        model=ScriptedModel([call("claude_code_engineer", {"task": "make the tests pass"}), patch_from_note, call("shell", {"command": CHECK}), text("fixed")]),
        tools=[fake_claude, patch_tool.apply_patch, shell],
        hooks=[hooks],
        callback_handler=None,
    )
    agent("have Claude Code fix it")
    unsubscribe()
    texts = tool_texts(agent)
    check("Claude Code finished with `bash run_tests.sh` still failing" in texts[0] and "```diff" in texts[0], "a check Claude Code left failing is reproduced in a copy and healed")
    check(phases(events)[:2] == ["detected", "diagnosing"] and "verified" in phases(events), f"the same loop as a shell failure ({phases(events)})")
    check((root / "calc.py").read_text().endswith("a + b\n"), "…and the model applied the verified patch")

    # A check that fails inside the CLI's session but passes in a clean copy is reported as such, not "fixed".
    (root / "calc.py").write_text("def add(a, b):\n    return a + b\n")
    diagnose = FakeDiagnose([GOOD_PATCH])
    hooks = SelfHealHooks(config=HealConfig(max_attempts=3), diagnose=diagnose)
    agent = Agent(model=ScriptedModel([call("claude_code_engineer", {"task": "x"}), text("ok")]), tools=[fake_claude], hooks=[hooks], callback_handler=None)
    agent("again")
    check(diagnose.calls == [] and "passes in a clean copy" in tool_texts(agent)[0], "when it passes in a clean copy the Debugger is not engaged, and the model is told to check for itself")


def main() -> None:
    test_heal()
    test_iterates_on_a_failed_verification()
    test_auto_continue()
    test_escalates()
    test_environmental_failure()
    test_real_debugger_agent()
    test_preapproval()
    test_state_mismatch()
    test_claude_code_failing_check()
    print("\nPASS: self-healing — diagnose, sandbox-verify, iterate, auto-continue, escalate, pre-approval bounds")


if __name__ == "__main__":
    main()

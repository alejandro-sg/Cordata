"""Diagnose-draft-validate loop for self-healing: three fresh, stateless
sub-agents run in a fixed sequence, wrapped behind one @tool per Strands'
Functional Wrapping pattern.

docs/self-healing-architecture.md §3: each sub-agent is "wrap[ped] ... inside
a Python function decorated with @tool ... Because the agent is created
inside the function, it starts completely fresh on every call" and "set the
sub-agent's callback handler to None" (strandsagents.com, Lesson 10:
Multi-Agent Patterns: Agents as Tools). Sequencing here is this file's own
Python control flow, not the model's — "you cannot guarantee a strict
execution order between specialists" if the orchestrator were given all
three as separate tools instead (same lesson).

None of the three sub-agents below is given file_write/editor. A
sub-agent built here is a plain strands.Agent, constructed directly in
Python with no AG-UI transport wrapping it — main.py's
ToolBehavior(interrupt_on_call=True) is implemented by ag_ui_strands'
StrandsInterruptHook, which is only ever attached to the one Agent instance
StrandsAgent wraps (see ag_ui_strands/agent.py's StrandsAgent.__init__:
`self._hooks = [StrandsInterruptHook(interrupt_tools), *self._hooks]`) —
never to an Agent built elsewhere. A sub-agent calling file_write directly
would therefore write to disk with no human approval at all, silently
contradicting the human-in-the-loop guarantee this whole self-healing
feature depends on (this was exactly that bug, here, until it was found and
fixed — the writer sub-agent used to be given file_write/editor directly,
and its docstring incorrectly claimed that write was already gated). The fix
is the same one sub_agent_common.py's DENIED_TOOLS comment already argues
for sub-agents: never hand a sub-agent a tool it
can't be interrupted on. The writer below drafts the file as text instead;
skill_forge returns that draft to the *top-level* agent, whose own next
file_write call is the one genuinely gated by StrandsInterruptHook (see
skills/self-healing/SKILL.md for the exact contract the orchestrator follows).

build_model() is imported lazily inside skill_forge, not at module level:
agent_common.py imports this module to register skill_forge on TOOLS, so a
top-level `from agent_common import build_model` here would be a circular
import at module-load time. By the time skill_forge is actually invoked as a
real tool call, agent_common has already finished importing.
"""

from pathlib import Path

from strands import Agent, tool
from strands_tools import file_read, python_repl

# The SDK's vended http_request (core_tools), not strands_tools' — deprecated in strands-agents-tools 0.8.9, an error
# log in 0.9.0; the rest of the harness moved off it in the context-diet work and this one was left behind.
from core_tools import http_request

from prompt_security import SecretRedactionHooks, UntrustedToolResultHooks

# The one directory every relative path in this file's own comments/prompts
# means "relative to" — computed from this file's own location, not the
# process's current working directory. That distinction matters here
# specifically: agent_common.py's own os.chdir(WORKSPACE_DIR) means the
# *live* cwd during a real conversation is backend/workspace/, not backend/
# — so a bare relative "skills_generated/<name>.py" handed to a real
# file_write call (as the writer's draft literally says) would resolve
# against workspace/ and silently land at
# backend/workspace/skills_generated/<name>.py, a path
# discover_generated_tools() never scans. Confirmed reading
# strands_tools/file_write.py directly: it does
# `path = expanduser(tool_input["path"])` and nothing else — no repo-root
# resolution of its own. _rewrite_draft_path_absolute below is the fix:
# the orchestrator is handed a real, unambiguous absolute path instead.
_BACKEND_DIR = Path(__file__).resolve().parent

# Kept short deliberately, same reasoning as agent_common.SYSTEM_PROMPT: a
# longer prompt measurably hurt this small local model's tool-calling
# reliability, and each sub-agent here has exactly one job.
_DEBUGGER_PROMPT = (
    "You are a debugging specialist. You will be given a failing tool name, "
    "the arguments it was called with, and a stack trace. Reason first, "
    "then respond with: root cause, whether it looks transient or "
    "code-level, and a concrete fix sketch. Do not write any files yourself."
)

_WRITER_PROMPT = (
    "You are a tool-writing specialist. You will be given a diagnosis and "
    "fix sketch. Check the fix against the diagnosis, then "
    "draft exactly one new tool: a single @tool-decorated Python function, "
    "with a docstring, that implements the fix. Do not call file_write or "
    "any other tool to save it — you have no way to create a file that a "
    "human can actually review before it lands on disk, so drafting it as "
    "text for the orchestrator to write instead is the only safe option. "
    "Respond with exactly this format and nothing else before or after:\n"
    "FILE: skills_generated/<tool_name>.py\n"
    "---\n"
    "<the complete file content, ready to write as-is>"
)

_VALIDATOR_PROMPT = (
    "You are a validation specialist. You will be given a drafted tool — a "
    "file path and its full content, not yet written to disk. Call "
    "python_repl to sanity-check the code is syntactically valid Python and "
    "does roughly what it claims to — don't assume it works just because it "
    "reads plausibly. Respond with PASS or FAIL and one sentence of "
    "reasoning."
)


def _run_debugger(build_model, stack_trace: str, failing_tool: str, attempted_args: str) -> str:
    # The only one of skill_forge's three sub-agents with http_request — it
    # may fetch arbitrary web content while diagnosing (docs, error message
    # lookups). hooks=[UntrustedToolResultHooks()] wraps whatever it fetches
    # before this agent's own model reads it, same as every other tool
    # construction path in this codebase — see prompt_security.py.
    agent = Agent(
        model=build_model(),
        system_prompt=_DEBUGGER_PROMPT,
        tools=[file_read, http_request],
        hooks=[UntrustedToolResultHooks(), SecretRedactionHooks()],
        callback_handler=None,
    )
    prompt = f"Failing tool: {failing_tool}\nAttempted args: {attempted_args}\nStack trace:\n{stack_trace}"
    return str(agent(prompt))


def _run_writer(build_model, diagnosis: str) -> str:
    # No file_write/editor here — see module docstring and skill_forge's own
    # docstring for why. No tools at all.
    agent = Agent(
        model=build_model(),
        system_prompt=_WRITER_PROMPT,
        tools=[],
        callback_handler=None,
    )
    return str(agent(diagnosis))


def _rewrite_draft_path_absolute(draft: str) -> str:
    """Rewrite the writer's `FILE: <relative path>` line to an absolute
    one, anchored at _BACKEND_DIR — see the module-level comment on why a
    relative path here is actively wrong, not just less explicit. Leaves
    the draft untouched (rather than raising) if it isn't in the expected
    `FILE: ...\\n---\\n...` shape; _parse errors surface downstream (the
    validator/orchestrator will see a draft with no usable FILE: line and
    can say so) rather than this function guessing at a fix.
    """
    if "FILE:" not in draft:
        return draft
    header, sep, rest = draft.partition("\n")
    if not header.strip().startswith("FILE:"):
        return draft
    relative = header.split("FILE:", 1)[1].strip()
    if not relative or Path(relative).is_absolute():
        return draft
    absolute = (_BACKEND_DIR / relative).resolve()
    return f"FILE: {absolute}{sep}{rest}"


def _run_validator(build_model, draft: str) -> str:
    # No file_read — the draft isn't on disk yet, it's passed in directly.
    agent = Agent(
        model=build_model(),
        system_prompt=_VALIDATOR_PROMPT,
        tools=[python_repl],
        hooks=[SecretRedactionHooks()],
        callback_handler=None,
    )
    return str(agent(draft))


@tool
def skill_forge(stack_trace: str, failing_tool: str, attempted_args: str) -> str:
    """Diagnose a failing tool call and draft a fixed tool for skills_generated/.

    Runs three fresh sub-agents in a fixed sequence — debug, draft, validate
    — per docs/self-healing-architecture.md §3. Each hop is plain string in,
    string out; nothing is shared between them beyond what this function
    explicitly passes along, since a fresh sub-agent has no memory of the
    calling conversation or of the other sub-agents.

    This does NOT write anything to disk — see the module docstring for why
    a sub-agent built here can never be given file_write safely. The result
    includes the drafted file as `FILE: <path>` followed by `---` and the
    full content: call file_write yourself, with exactly that path and
    content, as your own very next tool call. That real top-level file_write
    call is what actually pauses for human approval — skill_forge returning
    is not itself approval of anything. The resulting tool is gated again on
    its first real invocation by SkillValidationHooks, same as before.

    Args:
        stack_trace: The captured error/traceback from the failing tool call.
        failing_tool: Name of the tool that failed.
        attempted_args: The arguments passed when it failed, as a string.
    """
    from agent_common import build_model

    diagnosis = _run_debugger(build_model, stack_trace, failing_tool, attempted_args)
    draft = _run_writer(build_model, diagnosis)
    # Validated against the writer's own relative FILE: line (irrelevant to
    # the validator, which only reads the content after the --- separator)
    # before rewriting it absolute for the orchestrator — order doesn't
    # matter here, but keeping the validator's input identical to what the
    # writer actually produced is one less thing to explain if its opinion
    # ever needs debugging.
    validation = _run_validator(build_model, draft)
    draft = _rewrite_draft_path_absolute(draft)
    return (
        f"Diagnosis:\n{diagnosis}\n\n"
        f"Drafted tool (NOT written yet — call file_write yourself with this exact path and content):\n{draft}\n\n"
        f"Validator opinion:\n{validation}"
    )

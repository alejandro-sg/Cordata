"""AG-UI front door: the Strands agent (agent_common.py) over HTTP, for the
CopilotKit web client. See acp_server.py for the ACP/editor front door onto
the same agent definition.

Local-only research skeleton — no Okta/O4AA, no AgentCore, no cloud calls.
Swap OMLX_BASE_URL / OMLX_MODEL_ID in agent_common.py for a real Bedrock or
Anthropic model later; the agent/tool code doesn't change.

Tools granted here have real capability on this machine (shell, python_repl,
editor, file_read, file_write). Confirmation prompts are bypassed at the
process level (BYPASS_TOOL_CONSENT) because a headless server has no TTY to
prompt on — but the three most dangerous tools (shell, python_repl,
file_write) are gated by a real human-in-the-loop approval instead, via
ag_ui_strands's ``interrupt_on_call``: the tool call pauses, the frontend
shows an approve/deny card (frontend/app/page.tsx, useHumanInTheLoop), and
the tool only runs if the human approves. Tools default to a relative-path
CWD of backend/workspace/, but nothing stops an absolute path from leaving
it. Don't expose this beyond localhost.
"""

from ag_ui_strands import StrandsAgent, StrandsAgentConfig, ToolBehavior, create_strands_app

# Imported before agent_common: importing agent_common chdirs the process
# into backend/workspace/, which breaks any later bare-module import relying
# on the '' (cwd) entry in sys.path — see agent_common.py's own comment on
# why its early tool imports precede that chdir, for the same reason.
# sandbox_functions_api is a bare local module too (backend/sandbox_functions_
# api.py); it must precede the agent_common import below, not just live
# somewhere in this block — the chdir is a side effect of agent_common's
# first import, wherever that's triggered from, so this line has to be the
# one that triggers it (sandbox_functions_api's own `from agent_common
# import WORKSPACE_DIR` does the loading transitively). Putting it after
# agent_common's own import here breaks with exactly the ModuleNotFoundError
# this comment warns about, even though both look equally "early".
from skill_validation_hooks import SkillValidationHooks
from model_control_tool import _context_window_limit, _fetch_available_models
from sandbox_functions_api import register_sandbox_routes
from agent_common import GENERATED_TOOL_NAMES, build_plugins, build_template_agent

# Tools whose real-world impact (arbitrary shell/code execution, overwriting
# files) is worth a human confirming before they actually run. `editor` is
# here for the same reason as file_write — per its own module docstring it
# can Create/Replace/Insert file content, real host-filesystem writes, not
# just view them. acp_server.py's own APPROVAL_REQUIRED_TOOLS already gated
# it for the ACP/Zed front door and flagged this front door as the
# not-yet-actioned gap; this closes it. Extended with every tool name loaded
# from skills_generated/ — the self-healing loop's own output is exactly as
# untrusted as shell/python_repl/file_write until a human has approved its
# first real call (docs/self-healing-architecture.md §4, hand-off #2).
_APPROVAL_REQUIRED_TOOLS = ["shell", "python_repl", "file_write", "editor", *GENERATED_TOOL_NAMES]


def _agent_config_payload() -> dict:
    """The subset of agent_common's ACTIVE_* globals worth showing the
    frontend — shared by /models, _model_state, and _context_state so the
    three never drift out of sync with each other.
    """
    import agent_common
    import sandbox_mode_tool

    return {
        "active_model": agent_common.ACTIVE_MODEL_ID,
        "available_models": _fetch_available_models(),
        "temperature": agent_common.ACTIVE_TEMPERATURE,
        "memory_window": agent_common.ACTIVE_MEMORY_WINDOW,
        "context_window_limit": _context_window_limit(agent_common.ACTIVE_MODEL_ID),
        "sandbox_mode": sandbox_mode_tool.CURRENT_SANDBOX_MODE,
    }


def _model_state(_ctx) -> dict:
    """AG-UI shared-state payload after list_models/set_active_model/
    set_temperature/set_memory_window runs.

    Re-derives state from scratch (the ACTIVE_* globals + a fresh oMLX
    /models query) instead of parsing the tool's own text return — decouples
    the shared-state shape from whatever prose the tool happens to return.
    Read on the frontend via useAgent({agentId: "sierralab_agent"}).state.
    """
    return _agent_config_payload()


def _context_state(ctx) -> dict:
    """AG-UI shared-state payload after context_usage runs.

    Unlike _model_state, this genuinely needs the tool's own return value —
    token usage lives in the per-thread Agent's message history, which
    nothing outside a running tool call (this callback included) has a
    handle on. context_usage returns a JSON object, so ag_ui_strands has
    already parsed ctx.result_data into a plain dict by the time this runs.
    """
    if isinstance(ctx.result_data, dict):
        return {"context_usage": ctx.result_data}
    return {}


def _style_context_builder(input_data, user_message: str) -> str:
    """Read the frontend's tone/length picks (agent.setState) and prefix a
    one-line style hint onto the user's message.

    This is CopilotKit's own documented "Agent Config" pattern (tone,
    expertise, response length as shared state the agent rebuilds its
    behavior from each turn) — deterministic and tool-call-free, unlike the
    ACTIVE_* config above: no LLM decision is involved in applying it, so it
    carries none of this app's usual local-model tool-calling-reliability
    risk. What it does carry: every extra token here is a token added to
    every single turn's prompt, and agent_common.SYSTEM_PROMPT is kept
    deliberately short because a longer prompt measurably hurt this small
    model's own tool-calling reliability — so this stays a single short
    line, not a rebuilt system prompt.
    """
    state = getattr(input_data, "state", None) or {}
    tone = state.get("tone") if isinstance(state, dict) else None
    length = state.get("length") if isinstance(state, dict) else None
    if not tone and not length:
        return user_message
    bits = [b for b in (tone, length and f"{length} replies") if b]
    return f"[Style preference: {', '.join(bits)}]\n{user_message}"


def session_manager_provider(run_input):
    """Give each AG-UI thread its own durable Strands session on local disk."""
    from agent_common import SESSIONS_DIR
    from strands.session.file_session_manager import FileSessionManager

    return FileSessionManager(session_id=run_input.thread_id, storage_dir=str(SESSIONS_DIR))


agui_agent = StrandsAgent(
    agent=build_template_agent(),
    name="sierralab_agent",
    description="Local Strands agent with shell/file/code tools and durable per-thread memory.",
    # Plugins (AgentSkills) don't survive the template agent -> per-thread
    # agent cloning ag_ui_strands does internally — they have to be given to
    # StrandsAgent directly, per its own runtime warning.
    plugins=build_plugins(),
    # AST-checks, dry-runs, and (on failure) rejects any tool call selected
    # from backend/skills_generated/ before it executes — the second,
    # independent gate on generated tools, on top of the approval below.
    hooks=[SkillValidationHooks()],
    config=StrandsAgentConfig(
        session_manager_provider=session_manager_provider,
        # Deterministic, tool-call-free config: applied to every turn's
        # prompt regardless of whether the model decides to do anything.
        state_context_builder=_style_context_builder,
        tool_behaviors={
            **{name: ToolBehavior(interrupt_on_call=True) for name in _APPROVAL_REQUIRED_TOOLS},
            # Not approval-gated: these are benign config toggles / reads,
            # not code execution — but every call pushes fresh state to the
            # frontend.
            "list_models": ToolBehavior(state_from_result=_model_state),
            "set_active_model": ToolBehavior(state_from_result=_model_state),
            "set_temperature": ToolBehavior(state_from_result=_model_state),
            "set_memory_window": ToolBehavior(state_from_result=_model_state),
            "context_usage": ToolBehavior(state_from_result=_context_state),
            "set_sandbox_mode": ToolBehavior(state_from_result=_model_state),
        },
    ),
)

app = create_strands_app(
    agui_agent,
    "/",
    origins=["http://localhost:3000", "https://agent.home", "https://agent.sierralab.net"],
)


@app.get("/models")
def get_agent_config() -> dict:
    """Plain REST escape hatch for the frontend's initial config values —
    independent of the chat/tool-call loop, so the UI can populate its
    controls before the agent has run any tool this session. Same
    underlying values _model_state pushes as AG-UI state once a config tool
    actually runs. (Route name kept as /models for compatibility with the
    original model-only version; payload has grown since.)
    """
    return _agent_config_payload()


# /sandbox/* — the REST bridge for openGenerativeUI.sandboxFunctions
# (frontend/app/page.tsx), not the chat agent's own tool-call loop. See
# sandbox_functions_api.py's module docstring for why these calls need their
# own client-side approval gate instead of reusing interrupt_on_call.
register_sandbox_routes(app, _agent_config_payload)


if __name__ == "__main__":
    import uvicorn

    uvicorn.run("main:app", host="0.0.0.0", port=8001, reload=True)

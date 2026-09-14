"""Shared Strands agent construction for both front doors this backend serves:

- ``main.py``       — AG-UI over HTTP, for the CopilotKit web client
- ``acp_server.py``  — Agent Client Protocol over stdio, for ACP editors (e.g. Zed)

One agent definition (model, tools, system prompt), two transports. Each
front door gives every thread/session its own on-disk FileSessionManager, so
conversation history and whatever the agent has learned persist across
backend restarts regardless of which door was used.
"""

import importlib.util
import os
from pathlib import Path

from strands import Agent
from strands.agent.conversation_manager import SummarizingConversationManager
from strands.models.openai import OpenAIModel
from strands.session.file_session_manager import FileSessionManager
from strands.tools.decorator import DecoratedFunctionTool
from strands.vended_plugins.skills import AgentSkills
from strands_tools import calculator, current_time, editor, file_read, file_write, http_request, python_repl, shell, think

from agent_memory_tool import agent_memory
from local_graph_tool import local_graph
from local_swarm_tool import local_swarm
from settings import OMLX_BASE_URL, OMLX_MODEL_ID

# Runtime agent config, all mutated live by model_control_tool.py's tools
# (`agent_common.ACTIVE_* = ...`) when the agent switches them mid-session,
# so any *new* session started afterward inherits the switch too — without
# touching the original env-derived defaults above, or needing a restart.
ACTIVE_MODEL_ID = OMLX_MODEL_ID
ACTIVE_TEMPERATURE: float | None = None  # None = provider default
ACTIVE_MEMORY_WINDOW = 10  # SummarizingConversationManager.preserve_recent_messages

# Well above oMLX's own server-side default — without this, a genuinely
# long reply hits MaxTokensReachedException mid-generation and gets
# silently truncated, leaving only a partial message in history (observed
# live during a multi-step dashboard-building task). Doesn't affect latency
# until the model actually needs that many tokens.
_DEFAULT_MAX_TOKENS = 16384


def build_model() -> OpenAIModel:
    from model_control_tool import _context_window_limit

    params: dict = {"max_tokens": _DEFAULT_MAX_TOKENS}
    if ACTIVE_TEMPERATURE is not None:
        params["temperature"] = ACTIVE_TEMPERATURE

    return OpenAIModel(
        client_args={"api_key": "not-needed", "base_url": OMLX_BASE_URL},
        model_id=ACTIVE_MODEL_ID,
        params=params,
        context_window_limit=_context_window_limit(ACTIVE_MODEL_ID),
    )


# Imported here, before os.chdir() below moves the process (and the ''
# sys.path entry every bare-module import below relies on) into workspace/ —
# same reason agent_memory_tool/local_swarm_tool are imported above, before
# chdir. Also after build_model is defined: skill_forge_tool.py calls back
# into this module for it lazily, inside its own skill_forge function.
from skill_forge_tool import skill_forge  # noqa: E402
from model_control_tool import context_usage, list_models, set_active_model, set_memory_window, set_temperature  # noqa: E402
from sandbox_mode_tool import set_sandbox_mode  # noqa: E402

# No TTY in either server process: let these tools run without a confirmation
# prompt instead of hanging forever waiting on stdin.
os.environ.setdefault("BYPASS_TOOL_CONSENT", "true")
os.environ.setdefault("STRANDS_NON_INTERACTIVE", "true")

BACKEND_DIR = Path(__file__).parent.resolve()
WORKSPACE_DIR = BACKEND_DIR / "workspace"
SESSIONS_DIR = BACKEND_DIR / "sessions"
SKILLS_GENERATED_DIR = BACKEND_DIR / "skills_generated"
WORKSPACE_DIR.mkdir(exist_ok=True)
SESSIONS_DIR.mkdir(exist_ok=True)
os.chdir(WORKSPACE_DIR)


def discover_generated_tools() -> list:
    """Import every @tool-decorated function under skills_generated/*.py.

    AgentSkills only discovers markdown SKILL.md guidance once at session
    start (see skills/skill-authoring/SKILL.md) — it has no hot-reload path
    for executable code the self-healing loop writes mid-run
    (docs/self-healing-architecture.md §5). So a generated tool has to be
    imported and appended to TOOLS directly, here, at process start; a file
    written after this call isn't callable until the process next restarts.
    """
    tools: list = []
    if not SKILLS_GENERATED_DIR.exists():
        return tools
    for path in sorted(SKILLS_GENERATED_DIR.glob("*.py")):
        spec = importlib.util.spec_from_file_location(f"skills_generated.{path.stem}", path)
        if spec is None or spec.loader is None:
            continue
        module = importlib.util.module_from_spec(spec)
        try:
            spec.loader.exec_module(module)
        except Exception:
            continue  # a broken generated file just doesn't get registered
        for attr in vars(module).values():
            if isinstance(attr, DecoratedFunctionTool):
                tools.append(attr)
    return tools


_GENERATED_TOOLS = discover_generated_tools()

TOOLS = [
    shell,
    editor,
    file_read,
    file_write,
    python_repl,
    http_request,
    current_time,
    calculator,
    think,
    agent_memory,
    local_swarm,
    local_graph,
    skill_forge,
    list_models,
    set_active_model,
    set_temperature,
    set_memory_window,
    context_usage,
    set_sandbox_mode,
    *_GENERATED_TOOLS,
]

# Tool names loaded from skills_generated/ — main.py extends its
# approval-required gating with these, same as _APPROVAL_REQUIRED_TOOLS.
GENERATED_TOOL_NAMES = {generated_tool.tool_name for generated_tool in _GENERATED_TOOLS}

SKILLS_DIR = BACKEND_DIR / "skills"


def build_plugins() -> list:
    return [AgentSkills(skills=[str(SKILLS_DIR)])]


def build_conversation_manager() -> SummarizingConversationManager:
    """Context management: summarize older turns instead of just dropping them.

    No ``summarization_agent`` given, so Strands uses the parent agent's own
    model to write the summary — same local oMLX model, no extra config. This
    also helps the oMLX memory-guard issue documented earlier: a long thread's
    prefill stays bounded instead of growing every turn.
    """
    return SummarizingConversationManager(preserve_recent_messages=ACTIVE_MEMORY_WINDOW)


# Kept short deliberately: tool-specific guidance (agent_memory, local_swarm,
# writing new skills) lives in backend/skills/ instead, where AgentSkills only
# injects it on demand rather than bloating every request's system prompt —
# a longer prompt measurably hurt this small model's tool-calling reliability.
SYSTEM_PROMPT = (
    "You are a local research agent running entirely on-device via oMLX. "
    f"Your shell, file, and code tools default to the working directory {WORKSPACE_DIR}; "
    "use relative paths to stay inside it. shell, python_repl, file_write, and editor "
    "each require the user's approval before they run."
)


def build_template_agent() -> Agent:
    """A session-less template Agent.

    Used by the AG-UI (web) front door: ag_ui_strands derives one real,
    session-bound Agent per thread from this template plus the
    ``session_manager_provider`` it's given (see main.py) — it owns the
    per-thread FileSessionManager wiring itself.

    No ``plugins=`` here on purpose: ag_ui_strands does not carry constructor
    params like ``plugins`` over to the per-thread agents it derives from this
    template (it says so itself at runtime) — pass plugins to ``StrandsAgent``
    directly instead (see main.py).
    """
    return Agent(
        model=build_model(),
        system_prompt=SYSTEM_PROMPT,
        tools=TOOLS,
        conversation_manager=build_conversation_manager(),
    )


def build_agent_for_session(
    session_id: str,
    interventions: list | None = None,
    system_prompt: str | None = None,
) -> Agent:
    """A fully session-bound Agent, for front doors that manage their own
    per-session instances (the ACP server — nothing derives this one for us).

    Same durable on-disk store as the web path, so a conversation started in
    one front door resumes identically if the same session id reaches the
    other.

    ``interventions`` is how a front door with no AG-UI runtime underneath it
    (the ACP server) wires up the same real approval-before-execution gate on
    shell/python_repl/file_write/etc. that main.py gets for free from
    ag_ui_strands's ``ToolBehavior(interrupt_on_call=True)`` — see
    acp_server.py's ``_ApprovalHandler``.

    ``system_prompt`` overrides the default WORKSPACE_DIR-scoped prompt —
    needed by the ACP server, whose sessions operate on whatever project the
    editor has open (see acp_server.py's own comment on why the sandboxed
    ``backend/workspace/`` framing this defaults to would be actively wrong
    there).
    """
    return Agent(
        model=build_model(),
        system_prompt=system_prompt or SYSTEM_PROMPT,
        tools=TOOLS,
        plugins=build_plugins(),
        conversation_manager=build_conversation_manager(),
        session_manager=FileSessionManager(session_id=session_id, storage_dir=str(SESSIONS_DIR)),
        interventions=interventions,
    )

"""Shared Strands agent construction for both front doors this backend serves:

- ``main.py``       — AG-UI over HTTP, for the CopilotKit web client
- ``acp_server.py``  — Agent Client Protocol over stdio, for ACP editors (e.g. Zed)

One agent definition (model, tools, system prompt), two transports. Each
front door gives every thread/session its own on-disk FileSessionManager, so
conversation history and whatever the agent has learned persist across
backend restarts regardless of which door was used.
"""

import asyncio
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
from mcp_manager import load_configured_mcp_clients
from prompt_security import UNTRUSTED_CONTEXT_POLICY, UntrustedToolResultHooks
from settings import (
    ANTHROPIC_API_KEY,
    ANTHROPIC_MODEL_ID,
    CLOUD_OPENAI_API_KEY,
    CLOUD_OPENAI_BASE_URL,
    CLOUD_OPENAI_MODEL_ID,
    MODEL_PROVIDER,
    OMLX_BASE_URL,
    OMLX_MODEL_ID,
)

# Runtime agent config, all mutated live by model_control_tool.py's tools
# (`agent_common.ACTIVE_* = ...`) when the agent switches them mid-session,
# so any *new* session started afterward inherits the switch too — without
# touching the original env-derived defaults above, or needing a restart.
ACTIVE_MODEL_ID = OMLX_MODEL_ID
ACTIVE_TEMPERATURE: float | None = None  # None = provider default
ACTIVE_MEMORY_WINDOW = 10  # SummarizingConversationManager.preserve_recent_messages

# Same runtime-mutable-global shape as the three above, but mutated by
# provider_settings.py's set_provider_config (a plain REST call from the
# settings UI, not a chat tool call — no ToolContext to reach through, so
# provider_settings.py writes these module globals directly instead of the
# tool_context.agent.model.update_config() dance model_control_tool.py's
# tools use). "local" | "anthropic" | "openai_compatible".
ACTIVE_PROVIDER = MODEL_PROVIDER
ACTIVE_ANTHROPIC_API_KEY = ANTHROPIC_API_KEY
ACTIVE_ANTHROPIC_MODEL_ID = ANTHROPIC_MODEL_ID
ACTIVE_CLOUD_BASE_URL = CLOUD_OPENAI_BASE_URL
ACTIVE_CLOUD_API_KEY = CLOUD_OPENAI_API_KEY
ACTIVE_CLOUD_MODEL_ID = CLOUD_OPENAI_MODEL_ID

# oMLX's /v1/models introspection (model_control_tool._context_window_limit)
# has no cloud equivalent — neither Anthropic nor an arbitrary OpenAI-
# compatible endpoint expose max context length over their API. Prefix-
# matched fallback so the context-usage display (main.py's _context_state)
# still shows a "/ limit" for known model families; an unrecognized model id
# just shows usage with no limit rather than a guessed-wrong number.
_KNOWN_CONTEXT_LIMITS = {
    "claude-": 200_000,
    "gpt-": 128_000,
    "o1": 128_000,
    "o3": 128_000,
}


def _known_context_limit(model_id: str) -> int | None:
    for prefix, limit in _KNOWN_CONTEXT_LIMITS.items():
        if model_id.startswith(prefix):
            return limit
    return None

# Well above oMLX's own server-side default — without this, a genuinely
# long reply hits MaxTokensReachedException mid-generation and gets
# silently truncated, leaving only a partial message in history (observed
# live during a multi-step dashboard-building task). Doesn't affect latency
# until the model actually needs that many tokens.
_DEFAULT_MAX_TOKENS = 16384


def build_model():
    # Anthropic's own AnthropicConfig wants max_tokens as a top-level,
    # required kwarg, not inside params (unlike OpenAIModel below, which
    # takes it as a regular sampling param) — confirmed reading
    # strands/models/anthropic.py's AnthropicConfig TypedDict directly. Kept
    # as its own branch instead of trying to unify the two shapes.
    if ACTIVE_PROVIDER == "anthropic":
        from strands.models.anthropic import AnthropicModel

        anthropic_params: dict = {}
        if ACTIVE_TEMPERATURE is not None:
            anthropic_params["temperature"] = ACTIVE_TEMPERATURE
        return AnthropicModel(
            client_args={"api_key": ACTIVE_ANTHROPIC_API_KEY},
            model_id=ACTIVE_ANTHROPIC_MODEL_ID,
            max_tokens=_DEFAULT_MAX_TOKENS,
            params=anthropic_params,
            context_window_limit=_known_context_limit(ACTIVE_ANTHROPIC_MODEL_ID),
        )

    params: dict = {"max_tokens": _DEFAULT_MAX_TOKENS}
    if ACTIVE_TEMPERATURE is not None:
        params["temperature"] = ACTIVE_TEMPERATURE

    if ACTIVE_PROVIDER == "openai_compatible":
        return OpenAIModel(
            client_args={"api_key": ACTIVE_CLOUD_API_KEY, "base_url": ACTIVE_CLOUD_BASE_URL},
            model_id=ACTIVE_CLOUD_MODEL_ID,
            params=params,
            context_window_limit=_known_context_limit(ACTIVE_CLOUD_MODEL_ID),
        )

    from model_control_tool import _context_window_limit

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
from proposer_tool import proposer  # noqa: E402
from skill_validation_hooks import SkillValidationHooks  # noqa: E402
from model_control_tool import context_usage, list_models, set_active_model, set_memory_window, set_temperature  # noqa: E402
from sandbox_mode_tool import set_sandbox_mode  # noqa: E402
from game_tool import open_game  # noqa: E402

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

# One MCPClient (a ToolProvider — Agent(tools=...) loads its real tools
# directly, no manual extraction) per configured, enabled server in
# backend/mcp_servers.json. Computed once at process start, same as
# _GENERATED_TOOLS — unlike skills_generated/*.py, an MCP server list has no
# "changed mid-process by a hot-reload" case to stay fresh against
# (mcp_manager.add_server/remove_server only edit the on-disk config; see
# their own docstrings), so both front doors below share this one snapshot
# rather than build_agent_for_session re-running discover_generated_tools()
# style freshness for it too.
_MCP_CLIENTS = load_configured_mcp_clients()


def _mcp_tool_names() -> frozenset[str]:
    """Every tool name across all configured MCP clients, for
    UntrustedToolResultHooks' extra_untrusted_tools (prompt_security.py)
    and _APPROVAL_REQUIRED_TOOLS (main.py/acp_server.py) — a user-configured
    MCP server can be literally anything, so its tools default to both
    untrusted-output-wrapped and approval-gated, same reasoning
    GENERATED_TOOL_NAMES already applies to skills_generated/*.py.

    Deliberately calls load_tools() — the real ToolProvider entrypoint
    Strands itself calls when Agent(tools=[client, ...]) below consumes
    these same client objects — not start()/list_tools_sync() directly.
    MCPClient.start() isn't idempotent (a second call raises "the client
    session is currently running"), and load_tools() tracks its own
    _tool_provider_started flag rather than checking session state, so a
    direct start() here would desync that flag from the real session and
    make Strands' own later load_tools() call try to start an
    already-running client and fail — for every tool that server provides,
    not just its name-list entry, breaking the actual agent, not just this
    approval/wrapping metadata. load_tools() is documented as caching its
    result for reuse instead, so calling it here once and letting the Agent
    call it again later on the same object is the intended, safe path — a
    server with its own continue_on_error already returns [] from
    load_tools() on a connection failure rather than raising; the try/except
    below only catches the remaining case (no continue_on_error set, or a
    non-connection failure), consistent with the load_servers() call above
    not being the only place a bad server needs to not take the rest down.
    """
    names: set[str] = set()
    for client in _MCP_CLIENTS:
        try:
            tools = asyncio.run(client.load_tools())
        except Exception:
            continue
        names.update(tool.tool_name for tool in tools)
    return frozenset(names)


MCP_TOOL_NAMES = _mcp_tool_names()

# Everything but skills_generated/*.py — split out so build_agent_for_session
# (below) can rebuild the generated portion fresh per call instead of reusing
# this module's one process-start snapshot; TOOLS itself (and everything
# derived from it below) still is that one static snapshot, which is exactly
# right for build_template_agent's use case — see that function's docstring.
_STATIC_TOOLS = [
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
    proposer,
    list_models,
    set_active_model,
    set_temperature,
    set_memory_window,
    context_usage,
    set_sandbox_mode,
    open_game,
]

TOOLS = [*_STATIC_TOOLS, *_GENERATED_TOOLS, *_MCP_CLIENTS]

# Tool names loaded from skills_generated/ at process start — main.py and
# acp_server.py extend their approval-required gating with this, same as
# _APPROVAL_REQUIRED_TOOLS. A static, import-time snapshot on purpose: see
# build_agent_for_session's docstring for why closing that gap for a
# *hot-loaded* tool (skill_validation_hooks.py's hot_reload_generated_skill)
# needs more than this repo has verified carefully enough to do yet.
GENERATED_TOOL_NAMES = {generated_tool.tool_name for generated_tool in _GENERATED_TOOLS}


def current_generated_tool_names() -> set[str]:
    """Live version of GENERATED_TOOL_NAMES — a fresh directory listing
    (just filenames, no import/exec needed for names alone), not the
    process-start snapshot. Used only where a stale snapshot is a real
    safety gap and not just a "needs a restart for full parity" one: see
    sub_agent_common.resolve_tools, which must never hand a sub-agent a
    generated tool regardless of whether it existed at process start or was
    hot-loaded five seconds ago — a sub-agent has no human-in-the-loop gate
    at all (DENIED_TOOLS' own reasoning), so "denylist reflects an outdated
    world" is a strictly worse failure mode there than it is for the
    approval-required sets above, which degrade to "one extra gate missing
    on one tool's first call" rather than "a sub-agent silently got a tool
    it should never have been given."
    """
    if not SKILLS_GENERATED_DIR.exists():
        return set()
    return {path.stem for path in SKILLS_GENERATED_DIR.glob("*.py")}

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
    "each require the user's approval before they run. " + UNTRUSTED_CONTEXT_POLICY
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

    ``hooks=[SkillValidationHooks(), UntrustedToolResultHooks()]`` — main.py
    gets both via StrandsAgent's own ``hooks=`` kwarg (a real wrapper-level
    mechanism there, unlike ``plugins=``: confirmed reading
    ag_ui_strands/agent.py, StrandsAgent stores it on self._hooks and it
    survives per-thread agent derivation). ACP has no such wrapper — this
    function builds the one real Agent a session gets directly — so both
    have to go on the Agent constructor itself. Before this, a
    skills_generated/*.py tool reached through the ACP/Zed front door
    skipped the AST/banned-import/dry-run gate entirely; only the
    human-approval interrupt (ACP's own APPROVAL_REQUIRED_TOOLS) protected
    it there, never the second, independent check the web path always had.

    ``tools=`` deliberately does *not* reuse the shared, computed-once
    ``TOOLS`` constant below — it re-runs ``discover_generated_tools()``
    fresh on every call instead. TOOLS/_GENERATED_TOOLS are computed once at
    import time (the AG-UI path's ``build_template_agent()`` genuinely needs
    that — it's called exactly once, at process start, to build the one
    template ``agui_agent`` in main.py derives every thread from). This
    function, in contrast, is called fresh for *every new ACP session* — so
    there's no reason to hand a new session a stale, process-start snapshot
    of skills_generated/ when re-scanning it costs nothing extra here and
    means a tool written by an earlier session (this process's lifetime,
    restart or not) is available to every session opened after it, not just
    ones started after the next restart.
    """
    return Agent(
        model=build_model(),
        system_prompt=system_prompt or SYSTEM_PROMPT,
        tools=[*_STATIC_TOOLS, *discover_generated_tools(), *_MCP_CLIENTS],
        plugins=build_plugins(),
        hooks=[SkillValidationHooks(), UntrustedToolResultHooks(extra_untrusted_tools=MCP_TOOL_NAMES)],
        conversation_manager=build_conversation_manager(),
        session_manager=FileSessionManager(session_id=session_id, storage_dir=str(SESSIONS_DIR)),
        interventions=interventions,
    )

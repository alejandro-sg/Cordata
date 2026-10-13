"""Shared Strands agent construction for both front doors this backend serves:

- ``main.py``       — AG-UI over HTTP, for the CopilotKit web client
- ``acp_server.py``  — Agent Client Protocol over stdio, for ACP editors (e.g. Zed)

One agent definition (model, tools, system prompt), two transports. Each
front door gives every thread/session its own on-disk session (session_store.py: snapshots), so
conversation history and whatever the agent has learned persist across
backend restarts regardless of which door was used.
"""

import asyncio
import importlib.util
import logging
import os
import sys
from pathlib import Path
from typing import Any

from strands import Agent
from strands.agent.conversation_manager import SummarizingConversationManager
from strands.models.openai import OpenAIModel
from strands.tools.decorator import DecoratedFunctionTool
from strands_tools import file_read, file_write, python_repl

import code_nav
import code_search
import core_tools
import session_store
# Imported here, before the chdir below, because their callers import them lazily (`build_plugins`, the offloader's
# store path, the skill index cache) — and a bare-module import after the process has moved into workspace/ finds nothing
# when the entry point put a *relative* directory on sys.path (the `uvicorn main:app` of the Dockerfile).
import settings_store  # noqa: F401
import skill_search  # noqa: F401
import sub_agent_common  # noqa: F401
from core_tools import CoreToolHooks, HarnessSandbox, editor, http_request, shell, web_fetch

from agent_memory_tool import agent_memory
from a2a_client_tool import a2a_client
from mcp_manager import load_configured_mcp_clients
from laya_triage import triage_hooks
from prompt_security import UNTRUSTED_CONTEXT_POLICY, SecretRedactionHooks, UntrustedToolResultHooks
from secret_guard import reveal
from settings import (
    ANTHROPIC_API_KEY,
    ANTHROPIC_MODEL_ID,
    CLOUD_OPENAI_API_KEY,
    CLOUD_OPENAI_BASE_URL,
    CLOUD_OPENAI_MODEL_ID,
    MODEL_PROVIDER,
    OLLAMA_MODEL_ID,
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
# tools use). "local" | "anthropic" | "openai_compatible" | "ollama_cloud".
ACTIVE_PROVIDER = MODEL_PROVIDER
ACTIVE_ANTHROPIC_API_KEY = ANTHROPIC_API_KEY
ACTIVE_ANTHROPIC_MODEL_ID = ANTHROPIC_MODEL_ID
ACTIVE_CLOUD_BASE_URL = CLOUD_OPENAI_BASE_URL
ACTIVE_CLOUD_API_KEY = CLOUD_OPENAI_API_KEY
ACTIVE_CLOUD_MODEL_ID = CLOUD_OPENAI_MODEL_ID

# Where a model's context window comes from: oMLX reports it (model_control_tool._context_window_limit, from
# /v1/models); for Anthropic and OpenAI ids the SDK's own table (strands.models._defaults, refreshed every release —
# 1.57.2 added Sonnet 5.5, Opus 5.5, Fable 5.1) fills it in when the config does not set one; an Ollama-cloud model
# gets `ollama_cloud.DEFAULT_CONTEXT_WINDOW`; anything else gets `UNKNOWN_CONTEXT_WINDOW` (below), so the context-usage
# display (main.py's _context_state) and proactive compression always have a number to measure against.
def _with_window(model):
    if model.context_window_limit is None:
        model.update_config(context_window_limit=UNKNOWN_CONTEXT_WINDOW)
    return model


# Well above oMLX's own server-side default — without this, a genuinely
# long reply hits MaxTokensReachedException mid-generation and gets
# silently truncated, leaving only a partial message in history (observed
# live during a multi-step dashboard-building task). Doesn't affect latency
# until the model actually needs that many tokens.
_DEFAULT_MAX_TOKENS = 16384

# What a model's context window is assumed to be when nothing says. Strands' proactive compression (below) needs *a*
# number — without one it falls back to 200,000 and logs a warning on every model call — and 128k is the smallest window
# among the current flagship models, so a guess can make summarisation come a little early but never lets it come
# too late for a model that is bigger. A model whose real limit is known (oMLX reports it, the Anthropic/OpenAI tables
# have it) uses that.
UNKNOWN_CONTEXT_WINDOW = 128_000


def build_model(model_id: str | None = None):
    """The orchestrator's model for the ACTIVE provider. ``model_id`` overrides
    that provider's configured model id — how sub-agents get a cheaper or
    stronger tier (sub_agent_common) while still using the same provider, the
    same endpoint and the same API key (read via secret_guard.reveal here and
    nowhere an agent tool can reach).
    """
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
        return _with_window(
            AnthropicModel(
                client_args={"api_key": reveal(ACTIVE_ANTHROPIC_API_KEY)},
                model_id=model_id or ACTIVE_ANTHROPIC_MODEL_ID,
                max_tokens=_DEFAULT_MAX_TOKENS,
                params=anthropic_params,
            )
        )

    params: dict = {"max_tokens": _DEFAULT_MAX_TOKENS}
    if ACTIVE_TEMPERATURE is not None:
        params["temperature"] = ACTIVE_TEMPERATURE

    if ACTIVE_PROVIDER == "ollama_cloud":
        import ollama_cloud

        return ollama_cloud.build_single(
            "orchestrator", model=model_id or OLLAMA_MODEL_ID or None, params=params
        )

    if ACTIVE_PROVIDER == "openai_compatible":
        return _with_window(
            OpenAIModel(
                client_args={"api_key": reveal(ACTIVE_CLOUD_API_KEY), "base_url": ACTIVE_CLOUD_BASE_URL},
                model_id=model_id or ACTIVE_CLOUD_MODEL_ID,
                params=params,
            )
        )

    from model_control_tool import _context_window_limit

    return OpenAIModel(
        client_args={"api_key": "not-needed", "base_url": OMLX_BASE_URL},
        model_id=model_id or ACTIVE_MODEL_ID,
        params=params,
        context_window_limit=_context_window_limit(model_id or ACTIVE_MODEL_ID) or UNKNOWN_CONTEXT_WINDOW,
    )


# Imported here, before os.chdir() below moves the process (and the ''
# sys.path entry every bare-module import below relies on) into workspace/ —
# same reason agent_memory_tool is imported above, before
# chdir. Also after build_model is defined: skill_forge_tool.py calls back
# into this module for it lazily, inside its own skill_forge function.
from skill_forge_tool import skill_forge  # noqa: E402
from proposer_tool import proposer  # noqa: E402
from skill_validation_hooks import SkillValidationHooks  # noqa: E402
from model_control_tool import context_usage, list_models, set_active_model, set_memory_window, set_temperature  # noqa: E402
from sandbox_mode_tool import set_sandbox_mode  # noqa: E402

# No TTY in either server process: let these tools run without a confirmation
# prompt instead of hanging forever waiting on stdin.
os.environ.setdefault("BYPASS_TOOL_CONSENT", "true")
os.environ.setdefault("STRANDS_NON_INTERACTIVE", "true")

BACKEND_DIR = Path(__file__).parent.resolve()
WORKSPACE_DIR = BACKEND_DIR / "workspace"
#: Where conversations are persisted. `CORDATA_SESSIONS_DIR` moves them out of the checkout: a packaged app runs
#: this backend from a read-only bundle, and a scratch `CORDATA_HOME` for a test should not share the real ones.
SESSIONS_DIR = Path(os.environ["CORDATA_SESSIONS_DIR"]).expanduser().resolve() if os.environ.get("CORDATA_SESSIONS_DIR") else BACKEND_DIR / "sessions"
SKILLS_GENERATED_DIR = BACKEND_DIR / "skills_generated"
WORKSPACE_DIR.mkdir(exist_ok=True)
SESSIONS_DIR.mkdir(parents=True, exist_ok=True)
# A relative sys.path entry ('' or '.', from `python -m` or PYTHONPATH=.) means "the directory we started in";
# pin it to that directory before moving, or every bare-module import after the chdir looks in workspace/ instead.
sys.path[:] = [os.path.abspath(entry) if not os.path.isabs(entry) else entry for entry in sys.path]
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
    """Every tool name across all configured MCP servers, for
    UntrustedToolResultHooks' extra_untrusted_tools (prompt_security.py)
    and _APPROVAL_REQUIRED_TOOLS (main.py/acp_server.py) — a user-configured
    MCP server can be literally anything, so its tools default to both
    untrusted-output-wrapped and approval-gated, same reasoning
    GENERATED_TOOL_NAMES already applies to skills_generated/*.py.

    Builds and connects its own throwaway MCPClient objects (a fresh
    load_configured_mcp_clients() call) rather than reusing _MCP_CLIENTS —
    an earlier version of this function called load_tools() directly on
    _MCP_CLIENTS's own client objects, reasoning that load_tools()'s
    documented caching meant a later, real call on the same object would
    just reuse the cached result. Confirmed live in production that this
    was wrong: every real pihole tool call afterward failed with
    MCPClientInitializationError("the client session is not running"),
    even though the priming call itself had appeared to succeed and cache
    the tool names correctly. load_tools()/start()'s session and
    add_consumer/remove_consumer bookkeeping isn't meant to be driven by
    anything other than the real Agent's own ToolProvider usage — an
    additional untracked caller against the same object corrupts state
    that later real usage depends on. Using independent client objects
    here, then stopping them immediately after listing, means this
    function can never affect the objects _MCP_CLIENTS/TOOLS below actually
    hand to Agent(tools=...), at the cost of connecting to each configured
    server twice at process start (once here, once for real).
    """
    names: set[str] = set()
    for client in load_configured_mcp_clients():
        try:
            tools = asyncio.run(client.load_tools())
            names.update(tool.tool_name for tool in tools)
        except Exception:
            continue
        finally:
            try:
                client.stop(None, None, None)
            except Exception:
                pass
    return frozenset(names)


MCP_TOOL_NAMES = _mcp_tool_names()

_MCP_CLIENTS_STARTED = False


async def start_mcp_clients() -> None:
    """Establishes each configured MCP client's real session, from inside
    whichever asyncio event loop actually serves tool calls afterward.

    Necessary despite Agent(tools=TOOLS) below already trying to do exactly
    this itself: Strands' own tool registry resolves a ToolProvider like
    MCPClient via strands._async.run_async() — which runs the async
    load_tools() call in a *separate* thread, inside *its own* throwaway
    asyncio.run() loop, torn down immediately after. Confirmed live in
    production: every real MCP tool call afterward failed with
    MCPClientInitializationError("the client session is not running") — not
    after some later teardown, on the very first tool call this process
    ever attempted, right after that construction-time resolution. Calling
    client.stop() first, defensively, resets whatever partial/broken state
    that attempt left (stop() is documented as safe against partial-init
    states); the load_tools() call right after runs inside the real,
    persistent event loop request handling actually uses — main.py calls
    this from a FastAPI startup event (its own event loop, not a throwaway
    one), acp_server.py from inside its own long-running asyncio.run(_main()).

    Idempotent — a no-op after the first successful call, since both front
    doors share _MCP_CLIENTS and both need to call this once at startup.
    """
    global _MCP_CLIENTS_STARTED
    if _MCP_CLIENTS_STARTED or not _MCP_CLIENTS:
        return
    for client in _MCP_CLIENTS:
        try:
            client.stop(None, None, None)
        except Exception:
            pass
        try:
            await client.load_tools()
        except Exception:
            continue  # one bad server shouldn't block the rest — same reasoning continue_on_error already applies
    _MCP_CLIENTS_STARTED = True


# Everything but skills_generated/*.py — split out so build_agent_for_session
# (below) can rebuild the generated portion fresh per call instead of reusing
# this module's one process-start snapshot; TOOLS itself (and everything
# derived from it below) still is that one static snapshot, which is exactly
# right for build_template_agent's use case — see that function's docstring.
_STATIC_TOOLS = [
    shell,
    editor,
    file_read,
    code_search.code_search,
    code_nav.code_outline,
    file_write,
    python_repl,
    http_request,
    web_fetch,
    agent_memory,
    a2a_client,
    skill_forge,
    proposer,
    list_models,
    set_active_model,
    set_temperature,
    set_memory_window,
    context_usage,
    set_sandbox_mode,
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


def build_plugins(
    extra_skill_dirs: list[str] | None = None,
    *,
    decider: Any = None,
    preferences: Any = None,
    notify: Any = None,
) -> list:
    """The skills a session can reach: the ones that ship, plus ``extra_skill_dirs`` (the learned skills that have been
    promoted for the session's project — ``learned.live_dirs``) and any named in ``CORDATA_EXTRA_SKILLS`` (the
    promotion gate's A/B runs put a candidate there).

    Not listed in the prompt wholesale — 49 skills were 19 KB on every model call. ``skill_search`` lists the few that
    fit the request (embedding search, with Laya deciding when ``decider`` is given) and gives the agent
    ``search_skills`` for the rest; every skill stays one search away. ``context.skillSearch: false`` lists them all."""
    import skill_search

    from_env = [p for p in os.environ.get("CORDATA_EXTRA_SKILLS", "").split(os.pathsep) if p]
    # `from_env` are candidates under measurement (promote.py's A/B arms): pinned, so the arm really shows the model the lesson.
    plugins = skill_search.build_skill_plugins(
        [str(SKILLS_DIR), *(extra_skill_dirs or [])], decider=decider, preferences=preferences, notify=notify, pinned_sources=from_env
    )
    # What `current_time` was for, without the tool: the date and time ride along with each message (core_tools.clock).
    return [*plugins, core_tools.clock()]


def _fast_model():
    """The fast tier's model (`MODEL_TIER_FAST`), or None when none is configured — callers then use the session's own.
    For the jobs that rewrite or read text rather than reason: a compaction summary, a web page's analyst."""
    import sub_agent_common  # noqa: PLC0415 — imports the tool modules that import this one

    if not sub_agent_common.tier_model_id("fast"):
        return None
    try:
        return sub_agent_common.build_sub_agent_model("fast")
    except Exception:
        logging.getLogger(__name__).debug("could not build the fast-tier model", exc_info=True)
        return None


def _summarizer_agent(which: str) -> Agent | None:
    """A dedicated agent for writing summaries on the *fast* model tier (`MODEL_TIER_FAST`), or None for "the session's own
    model" (what Strands does when it is given none).

    A summary is rewritten history, not new reasoning — a smaller model does it well, and it is a model call the person
    pays for each time a long chat is compacted. Nothing changes until a fast tier is configured: with none, the summary
    comes from the session's own model exactly as before. `context.summaryModel: main` opts out even then."""
    if which == "main":
        return None
    model = _fast_model()
    return Agent(model=model, callback_handler=None) if model is not None else None


def build_conversation_manager(summarizer: str = "fast") -> SummarizingConversationManager:
    """Context management: summarize older turns instead of just dropping them.

    No ``summarization_agent`` given, so Strands uses the parent agent's own
    model to write the summary — same local oMLX model, no extra config. This
    also helps the oMLX memory-guard issue documented earlier: a long thread's
    prefill stays bounded instead of growing every turn.

    ``proactive_compression`` (Strands docs: conversation-management → Proactive Context Compression): by default a
    manager only reduces *after* the provider rejects a request for being too long — a whole wasted round trip (and on a
    metered model, its tokens). With it, the projected input is compared with the model's context window before every
    call and the history is summarised first once it passes 70%.
    """
    return SummarizingConversationManager(
        preserve_recent_messages=ACTIVE_MEMORY_WINDOW, proactive_compression=True, summarization_agent=_summarizer_agent(summarizer)
    )


# Kept short deliberately: tool-specific guidance (agent_memory,
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
    per-thread session wiring itself.

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


def _cloud_key_rejection() -> str | None:
    """Ask the cloud endpoint whether it accepts the key, at no token cost.

    POST {base}/chat/completions with an empty body: a valid key gets 400 (bad
    request), a bad or missing one 401/403 — verified against Ollama Cloud. Only
    a 401/403 counts; a network error, timeout or any other status says nothing
    about the key, so the session just proceeds.
    """
    import urllib.error
    import urllib.request

    key = reveal(ACTIVE_CLOUD_API_KEY)
    request = urllib.request.Request(
        f"{ACTIVE_CLOUD_BASE_URL.rstrip('/')}/chat/completions",
        data=b"{}",
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
        method="POST",
    )
    try:
        urllib.request.urlopen(request, timeout=5)
    except urllib.error.HTTPError as error:
        if error.code in (401, 403):
            source = getattr(ACTIVE_CLOUD_API_KEY, "source", "") or "an unknown source"
            return (
                f"the model endpoint rejected the API key (HTTP {error.code}). The key the backend loaded came "
                f"from {source} and is {len(key)} characters long (real keys are ~60). A stale exported "
                "OLLAMA_API_KEY beats the Keychain; otherwise re-store it with `make key` and check it with "
                "`scripts/check-key.sh`."
            )
    except Exception:
        return None
    return None


def ensure_provider_ready() -> None:
    """Fail fast, with the fix, when the active cloud provider has no credentials.

    Without this the first prompt dies with a bare "401" or "Connection error"
    from deep inside the model client. Raised at ACP session creation, where it
    reaches the client as an error it can show.
    """
    if ACTIVE_PROVIDER == "openai_compatible":
        missing = [
            name
            for name, value in (
                ("an API key", reveal(ACTIVE_CLOUD_API_KEY)),
                ("CLOUD_OPENAI_BASE_URL", ACTIVE_CLOUD_BASE_URL),
                ("CLOUD_OPENAI_MODEL_ID", ACTIVE_CLOUD_MODEL_ID),
            )
            if not value
        ]
        if missing:
            raise RuntimeError(
                f"MODEL_PROVIDER=openai_compatible but {', '.join(missing)} is not set. For the key, run `make key` in the Cordata "
                "folder (a hidden prompt; it is stored in the macOS Keychain, never in a file) or export OLLAMA_API_KEY in the shell "
                "that starts the client. To use a model without a key instead, set MODEL_PROVIDER=ollama_cloud (after `ollama signin`) "
                "or MODEL_PROVIDER=local in backend/.env."
            )
        rejection = _cloud_key_rejection()
        if rejection:
            raise RuntimeError(f"MODEL_PROVIDER=openai_compatible: {rejection}")
    elif ACTIVE_PROVIDER == "ollama_cloud":
        import ollama_cloud

        problem = ollama_cloud.daemon_problem()
        if problem:
            raise RuntimeError(f"MODEL_PROVIDER=ollama_cloud: {problem}")
    elif ACTIVE_PROVIDER == "anthropic" and not reveal(ACTIVE_ANTHROPIC_API_KEY):
        raise RuntimeError(
            "MODEL_PROVIDER=anthropic but no API key was found. Export ANTHROPIC_API_KEY, or store it in the macOS "
            'Keychain:  security add-generic-password -a "$USER" -s cordata-anthropic-api-key -w'
        )


def build_agent_for_session(
    session_id: str,
    interventions: list | None = None,
    system_prompt: str | None = None,
    triage_notify=None,
    *,
    extra_tools: list | None = None,
    extra_hooks: list | None = None,
    exclude_tools: set[str] | frozenset[str] = frozenset(),
    use_triage: bool = True,
    untrusted_tools=None,
    checkpointing: bool = False,
    skill_dirs: list[str] | None = None,
    skill_options: dict | None = None,
    offloader: Any = None,
    summarizer: str = "fast",
    project_root: str | None = None,
    web_fetch_mode: str = "agentic",
    extra_plugins: list | None = None,
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

    ``triage_notify`` receives Laya's live decision/outcome events (see
    laya_triage.LayaTriageHooks) — acp_server forwards them to the client.

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

    The agentic harness (acp_server) passes the rest: ``extra_tools`` (the
    specialists, apply_patch), ``extra_hooks`` (router, self-heal, LSP),
    ``exclude_tools`` (tools a plugin manages instead — the memory store and the
    LSP tools are hot-swappable plugins there), ``use_triage=False`` (its Laya
    router replaces the advisory triage hooks), ``untrusted_tools`` (a live
    container: plugin tools come and go) and ``checkpointing`` (Strands pauses
    at every ReAct cycle boundary so a turn can be resumed after a crash).

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
    ensure_provider_ready()
    # `web_fetch` is built per session: its analyst runs on the fast model tier when one is configured, else on this
    # session's own model (core_tools.py), and `context.webFetch` picks agentic (an answer) or markdown (the page).
    static = [t for t in _STATIC_TOOLS if getattr(t, "tool_name", None) not in {*exclude_tools, "web_fetch"}]
    model = build_model()
    web_usage = core_tools.UsageTally()  # what the web_fetch analyst spends — its own agent, so outside the session's metrics
    if "web_fetch" not in exclude_tools:
        static.append(core_tools.web_fetch_tool(mode=web_fetch_mode, model=_fast_model() or model, tally=web_usage))
    # With a plugin host (untrusted_tools given) MCP servers are hot-swappable
    # plugins it registers itself; the process-start MCP snapshot is not added.
    mcp = [] if untrusted_tools is not None else _MCP_CLIENTS
    # The SDK's local environment, whose running commands also end when the turn is cancelled (core_tools.HarnessSandbox).
    sandbox = HarnessSandbox()
    agent = Agent(
        model=model,
        sandbox=sandbox,
        system_prompt=system_prompt or SYSTEM_PROMPT,
        tools=[*static, *discover_generated_tools(), *mcp, *(extra_tools or [])],
        plugins=[*build_plugins(skill_dirs, **(skill_options or {})), *([offloader] if offloader is not None else []), *(extra_plugins or [])],
        hooks=[
            CoreToolHooks(project_root),
            # A whole-file read of a long file becomes its outline (context.outlineLines), ahead of the offloader.
            code_nav.LargeFileHooks(code_nav.threshold_from(lambda: settings_store.current(project_root or os.getcwd())), root=project_root),
            SkillValidationHooks(),
            UntrustedToolResultHooks(extra_untrusted_tools=untrusted_tools if untrusted_tools is not None else MCP_TOOL_NAMES),
            SecretRedactionHooks(),
            *(triage_hooks(notify=triage_notify) if use_triage else []),
            *(extra_hooks or []),
        ],
        conversation_manager=build_conversation_manager(summarizer),
        # Snapshots (session_store.py); a conversation from the old layout is migrated on first use.
        session_manager=session_store.session_manager(session_id, SESSIONS_DIR),
        interventions=interventions,
        checkpointing=checkpointing,
        # The default handler prints the streamed reply to stdout — which for the
        # ACP front door is the protocol channel (see acp_server._ProtocolStdout).
        # Events reach the client through stream_async, not through printing.
        callback_handler=None,
    )
    sandbox.bind(agent)
    agent.web_usage = web_usage  # read by the per-turn receipt and the turn budget (acp_server)
    session_store.forget_prompt_bookkeeping(agent)  # a restored state may name a prompt block this fresh prompt does not have
    return agent

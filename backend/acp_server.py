"""ACP front door: the same Strands agent (agent_common.py) over the Agent
Client Protocol (https://agentclientprotocol.com), for editors like Zed that
speak ACP over stdio. See main.py for the AG-UI/web front door.

Run directly — an ACP client spawns this process and talks JSON-RPC over
its stdin/stdout, so don't print anything else to stdout:

    python acp_server.py

A conversation resumes across restarts and across front doors: this uses
the same on-disk session (session_store.py) keyed by session id that main.py uses
keyed by AG-UI threadId (see agent_common.py).

Parity with the AG-UI front door, and with Zed's own bundled Claude Code
integration: real tool-call visibility, real approval-before-execution on
dangerous tools, session modes (Plan/Build/Auto), model + temperature as
native config-option dropdowns, session titles, and @-mention/resource
content actually reaching the model instead of being silently dropped. Every
mechanism below is wired to something this backend can already really do
(model_control_tool.py's model/temperature switching, the same tool set
main.py gates) — nothing here is a cosmetic stand-in for a capability that
doesn't exist.

The agentic harness on top (docs/agentic-harness.md), per session:

* **Laya router** (laya_router.py) — one typed forward pass per user turn picks
  the target (orchestrator / architect / debugger / an MCP plugin), with
  abstention that fails closed (escalate to the frontier tier, never dispatch on
  a guess); confident specialist routes are dispatched directly; outcomes feed
  online calibration.
* **Specialists** (specialists.py) — Architect and Debugger as Strands
  agents-as-tools sharing this session's approval handler.
* **Plugins** (plugin_host.py) — MCP servers, the memory store and the LSP tools
  as hot-swappable plugins (``_cordata/plugins/*`` ext methods).
* **LSP** (lsp_bridge.py) — language servers started for the project; edits get
  fresh diagnostics appended.
* **Self-healing** (self_heal.py) — failing commands are diagnosed, patched in a
  sandbox copy, verified, and the fix applied through the normal gate.
* **Checkpoints** — Strands pauses at every ReAct cycle boundary; the checkpoint
  is written next to the session so an interrupted turn can be resumed.
* Live state (tasks/progress, heal phases, plugin and LSP changes) goes to the
  client as ``_cordata/<kind>`` extension notifications.
"""

import asyncio
import atexit
import calendar
import json
import logging
import os
import threading
import time
import uuid
from pathlib import Path

import acp
import acp.stdio

import checkpoints
import edit_preview
import bandit
import claude_code_tool
import context_diet
import context_offload
import goal as goal_module
import session_extras
import session_history
import session_store
import task_tool
import ask_user
import plan_mode
import mode_advisor
import skill_search
import fix_memory
import context_digest_tool
import harness_events
import intervention_view
import kg
import lab as lab_module
import learned
import outcome_hooks
import outcomes
import stall_guard
import lsp_bridge
import ollama_resource_tool
import omlx_provider  # noqa: F401  (registers the Prism lease release at exit)
import omlx_resource_tool
import patch_tool
import prompts
import reflect
import settings as app_settings
import settings_store
import specialists
import secret_guard
import secret_patterns
import state_store
import steering as steering_module
import orchestrate_tool
import trace_store
import vault_proxy
import workspace_context
import adaptive_effort
import code_nav
import code_search
import env_profile
import os_context
import plugin_host
import rejection_memory
from laya_router import BASE_TARGETS, LayaRouter, RouteDecision, RouterHooks, plugin_targets, summarize_stats
from plugin_host import PluginHost, PluginSpec, run_blocking
from self_heal import HealConfig, SelfHealHooks

# Last: importing agent_common chdirs the process into backend/workspace/, after
# which bare-module imports only resolve when this file runs as a script.
from agent_common import SESSIONS_DIR, build_agent_for_session, start_mcp_clients  # noqa: E402
from model_control_tool import _context_window_limit, _fetch_available_models  # noqa: E402

# Split out of this file (docs/orchestrator.md, "Lean"); every name is re-exported so callers and tests keep working.
from acp_learning import (  # noqa: E402,F401
    _bandit_policy,
    _baseline_reward,
    _bandit_route_arms,
    _bandit_stub,
    _maintain,
    _bandit_learn,
    _optimize_artifacts,
    _note_answer,
    _laya_loaded,
    _route_of,
)
from acp_protocol import (  # noqa: E402,F401
    _tool_kind,
    _TOOL_KIND,
    _text_of,
    _turn_context,
    _REASONING_TAIL_CHARS,
    _SpecialistProgress,
    _recent_context,
    _extract_prompt_text,
    _prompt_images,
    _ProtocolStdout,
    _protect_protocol_stdout,
    STDIO_LINE_LIMIT,
    _TEMPERATURE_PRESETS,
    _closest_temperature_preset,
    _SPECIALIST_CONTEXT_CHARS,
)
from acp_approval import (  # noqa: E402,F401
    APPROVAL_REQUIRED_TOOLS,
    PLAN_MODE_TOOLS,
    _EDITOR_READ_COMMANDS,
    _READ_ONLY_ACTIONS,
    _read_only_call,
    _forced_ask,
    _ARMABLE_TOOLS,
    _ALLOW_KINDS,
    _permission_approved,
    _answered_by,
    _answer_note,
    _settled_intervention,
    _MODES,
    _MODE_IDS,
    _DEFAULT_MODE,
    _ApprovalHandler,
)
from acp_session import (  # noqa: E402,F401
    _replay_history,
    _project_system_prompt,
    _is_chat_scratch,
    _chat_system_prompt,
    _apply_generation,
    _apply_temperature,
    _apply_startup_preferences,
    _router_policy,
    _heal_config,
    _Session,
    _settle_cancelled_turn,
    _LAYA_DECIDE_TIMEOUT,
    _model_for_vision,
)


def _config_options(agent) -> list:
    """The live config-option list for one session's agent — always derived
    fresh from the agent's actual model config, never a separately-tracked
    copy that could drift from what's really active.
    """
    config = agent.model.get_config()
    current_model = config.get("model_id")
    current_temperature = (config.get("params") or {}).get("temperature")

    import ollama_cloud

    if isinstance(agent.model, ollama_cloud.OllamaCloudModel):
        # The registry (cloud tags, plus a local one already on disk), not oMLX's list.
        available_models = [spec.wire_id for spec in ollama_cloud.routable(agent.model.base_url)]
    else:
        available_models = _fetch_available_models()
    if current_model and current_model not in available_models:
        available_models = [current_model, *available_models]

    return [
        acp.schema.SessionConfigOptionSelect(
            type="select",
            id="model",
            name="Model",
            category="model",
            current_value=current_model or "",
            options=[acp.schema.SessionConfigSelectOption(value=model_id, name=model_id) for model_id in available_models],
        ),
        acp.schema.SessionConfigOptionSelect(
            type="select",
            id="temperature",
            name="Temperature",
            category="model_config",
            current_value=_closest_temperature_preset(current_temperature),
            options=[
                acp.schema.SessionConfigSelectOption(value=preset_id, name=label)
                for preset_id, _value, label in _TEMPERATURE_PRESETS
            ],
        ),
    ]


def _apply_model(agent, value) -> bool:
    """Switch the session's model; False if the provider doesn't offer it."""
    if not isinstance(value, str):
        return False
    import ollama_cloud

    if isinstance(agent.model, ollama_cloud.OllamaCloudModel):
        # Through the registry, so a local-weights tag that is not installed is
        # refused with a reason instead of 404-ing on the next request.
        try:
            spec = agent.model.switch_model(value)
        except (ollama_cloud.UnknownModel, ollama_cloud.ModelUnavailable) as error:
            logging.getLogger(__name__).warning("model=<%s> | %s, keeping the current model", value, error)
            return False
        import agent_common

        agent_common.OLLAMA_MODEL_ID = spec.alias  # new sessions inherit the switch
        return True
    available = _fetch_available_models()
    if available and value not in available:
        logging.getLogger(__name__).warning("model=<%s> | not offered by the provider, keeping the current model", value)
        return False
    config_update: dict = {"model_id": value}
    limit = _context_window_limit(value)
    if limit is not None:
        config_update["context_window_limit"] = limit
    agent.model.update_config(**config_update)

    import agent_common

    agent_common.ACTIVE_MODEL_ID = value
    return True


# The marker a client sends (acp-tui's /resume-turn) to continue a turn that a
# crash or kill interrupted between two checkpoints.
RESUME_TURN_MARKER = "[resume interrupted turn]"
#: What a client sends when it resumes on its own (``selfHeal.autoResume``): the same turn, but counted, so a
#: turn that crashes the process every time it is resumed is tried twice and then left for a person.
AUTO_RESUME_MARKER = "[auto-resume interrupted turn]"
MAX_AUTO_RESUMES = 2
#: After a cancel, how long the running turn is given to stop on its own (Strands' next checkpoint, a tool that polls the
#: cancel signal) before it is abandoned.
CANCEL_GRACE_SECONDS = 3.0
#: Model calls one turn may make before it pauses (`context.maxSteps`; 0 = no cap). Enough for a large task, small
#: enough that a loop which will not converge does not run on spending the person's tokens.
DEFAULT_MAX_STEPS = 50
AUTO_RESUME_MAX_AGE = 30 * 60


def _is_resume(text: str) -> bool:
    return text.strip() in (RESUME_TURN_MARKER, AUTO_RESUME_MARKER)


def _resume_marker_path(session_id: str) -> Path:
    return SESSIONS_DIR / f"session_{session_id}.resume.json"


def _checkpoint_fingerprint(session_id: str) -> str | None:
    try:
        info = _checkpoint_path(session_id).stat()
    except OSError:
        return None
    return f"{info.st_mtime_ns}:{info.st_size}"


def _auto_resume_attempts(session_id: str) -> int:
    """Automatic resumes already spent on *this* checkpoint. A turn that got further before dying wrote a new
    checkpoint, so its count starts again — only a turn that dies at the same point every time runs out."""
    try:
        data = json.loads(_resume_marker_path(session_id).read_text())
    except (OSError, ValueError):
        return 0
    return int(data.get("attempts", 0)) if data.get("fingerprint") == _checkpoint_fingerprint(session_id) else 0


def _count_auto_resume(session_id: str) -> None:
    try:
        _resume_marker_path(session_id).write_text(json.dumps({"fingerprint": _checkpoint_fingerprint(session_id), "attempts": _auto_resume_attempts(session_id) + 1}))
    except OSError:
        pass


def _checkpoint_path(session_id: str) -> Path:
    return SESSIONS_DIR / f"session_{session_id}.checkpoint.json"


#: Conversations per ``session/list`` page.
LIST_PAGE = 50


def _read_index() -> dict:
    """The legacy ``sessions/index.json`` (id -> cwd / title / updatedAt), read-only now: conversations are recorded in
    ``state.db``'s ``sessions`` table (``state_store.touch_session``), and this file only fills in the ones from before."""
    try:
        data = json.loads((SESSIONS_DIR / "index.json").read_text(encoding="utf-8"))
        return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        return {}


def _touch_index(session_id: str, cwd: str, title: str | None = None, *, turn: bool = False) -> None:
    state_store.touch_session(session_id, cwd=cwd, title=title, turn=turn)


def _iso(ts: float | None) -> str | None:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ts)) if ts else None


def _require_absolute(cwd: str) -> str:
    """ACP says a session's cwd is an absolute path. A relative one means "wherever this process happens to be", which is
    how 174 conversations ended up listed under ".": refused, so the client sends what it means."""
    if not cwd or not os.path.isabs(cwd):
        raise acp.RequestError.invalid_params({"error": f"cwd must be an absolute path, not {cwd!r}"})
    return cwd


def _plan_update(items: list) -> object:
    """A checklist (``task_tool``) as ACP's ``plan`` update."""
    entries = []
    for item in items:
        if not isinstance(item, dict) or not item.get("content"):
            continue
        status = str(item.get("status") or "pending")
        entries.append(acp.plan_entry(str(item["content"]), status=status if status in ("pending", "in_progress", "completed") else "pending"))
    return acp.update_plan(entries)


def _lsp_diagnostics(cwd: str, path: str, text) -> dict:
    """Diagnostics for one file of the project (``text``: its unsaved contents), as the editor draws them."""
    root = os.path.realpath(cwd)
    target = os.path.realpath(os.path.join(root, path))
    if not target.startswith(root + os.sep):
        return {"ok": False, "reason": "outside the project"}
    if not lsp_bridge.folder_trusted(root):
        return {"ok": False, "reason": "language servers start only in a trusted folder", "needs": "trust"}
    manager = lsp_bridge.manager_for(root)
    if manager.spec_for(target) is None:
        return {"ok": True, "path": target, "diagnostics": [], "server": None}
    try:
        if isinstance(text, str):
            client = manager.client_for(target)
            diagnostics = client.check_text(target, text) if client is not None else []
        else:
            diagnostics, client = manager.diagnostics(target)
    except Exception as error:  # a server that dies mid-request is a missing squiggle, not a failed turn
        return {"ok": False, "reason": f"language server error: {error}"}
    rows = []
    for item in diagnostics or []:
        rng = item.get("range") or {}
        rows.append({
            "start": rng.get("start") or {"line": 0, "character": 0},
            "end": rng.get("end") or rng.get("start") or {"line": 0, "character": 0},
            "severity": int(item.get("severity") or 1),
            "message": secret_patterns.scrub(str(item.get("message") or ""))[0][:1000],
            "source": item.get("source"),
        })
    return {"ok": True, "path": target, "diagnostics": rows, "server": getattr(getattr(client, "spec", None), "name", None) if client is not None else None}


def _project_settings(cwd: str) -> dict:
    """The project's own settings file, flattened (``.cordata/settings.json``), whatever trust does with it."""
    try:
        data = json.loads(settings_store.settings_path("project", cwd).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    out: dict = {}

    def walk(prefix: str, node) -> None:
        if isinstance(node, dict):
            for key, value in node.items():
                walk(f"{prefix}.{key}" if prefix else key, value)
        else:
            out[prefix] = node

    walk("", data)
    return out


def _events_size(run_id: str) -> int:
    """Where a run's events file ends now: a resumed run's watcher starts there, not at events already shown."""
    try:
        from orchestrator.control import run_dir

        return (run_dir(run_id) / "events.ndjson").stat().st_size
    except OSError:
        return 0


def _vision_model_configured() -> bool:
    try:
        return bool(str(settings_store.current(os.getcwd()).get("capture.visionModel") or "").strip())
    except Exception:
        return False


#: Conversations whose lease this process holds (``state_store.claim_session``); given back at exit.
_LEASED: set[str] = set()


def _release_leases() -> None:
    for session_id in list(_LEASED):
        state_store.release_session(session_id)
    _LEASED.clear()


atexit.register(_release_leases)


_PLUGIN_HOSTS: dict[tuple[str, bool], PluginHost] = {}
_ROUTER: LayaRouter | None = None


def _plugin_host(cwd: str, reserved: set[str], traces_enabled: bool = False) -> PluginHost:
    """One host per project per process. The memory store and the LSP tools are
    built-in plugins: listed, swappable and disable-able like an MCP server.

    `traces_enabled` is part of the cache key rather than a runtime toggle: a
    trace tool with nothing behind it is worse than no trace tool, so the plugin
    only exists when `memory.traces` says the corpus does. That means `/plugins`
    lists it exactly when it works."""
    host = _PLUGIN_HOSTS.get((cwd, traces_enabled))
    if host is None:
        from agent_memory_tool import agent_memory

        builtins = [
            PluginSpec(
                name="memory",
                kind="memory",
                transport="builtin",
                source="built-in",
                description="long-term semantic memory (Qdrant); swap in another memory MCP server by disabling this",
                builtin_tools=lambda: [agent_memory],
            ),
            PluginSpec(
                name="lsp",
                kind="lsp",
                transport="builtin",
                source="built-in",
                description="language-server tools: diagnostics, definitions, references, hover, symbols",
                builtin_tools=lambda: list(lsp_bridge.LSP_TOOLS),
            ),
        ]
        if traces_enabled:
            builtins.append(
                PluginSpec(
                    name="traces",
                    kind="memory",
                    transport="builtin",
                    source="built-in",
                    description="search this project's recorded execution traces (memory.traces)",
                    # Built on first use: `_tool` imports strands and builds the
                    # @tool wrapper, which a plugin that is switched off must
                    # not pay for.
                    builtin_tools=lambda: [trace_store._tool()],
                    # `allow` (not `ask`): searching the corpus changes nothing,
                    # and a built-in does not need trust for it to be honoured.
                    # `untrusted_output` stays at its default: a trace holds
                    # command output and file contents, which is exactly the
                    # text the guard exists to fence off.
                    approval="allow",
                )
            )
        host = _PLUGIN_HOSTS[(cwd, traces_enabled)] = PluginHost(cwd, builtins=builtins, reserved_names=reserved)
    return host


def _router() -> LayaRouter:
    global _ROUTER
    if _ROUTER is None:
        _ROUTER = LayaRouter()
    return _ROUTER


class CordataAcpAgent:
    """Implements initialize, new_session, load_session, list_sessions, prompt,
    cancel, set_session_mode, set_config_option and the ``_cordata/*``
    extension methods. fork_session/resume_session/authenticate aren't wired up
    (nothing here needs forking or auth).
    """

    def __init__(self) -> None:
        self._conn: acp.Client | None = None
        self._sessions: dict[str, _Session] = {}
        self._cancelled: set[str] = set()
        #: session -> the agent whose invocation is running right now (see `cancel`).
        self._streaming: dict[str, Agent] = {}
        self._loop: asyncio.AbstractEventLoop | None = None
        self._unsubscribe = None
        self._last_session: str | None = None
        #: session -> the goal being pursued across turns (``goal.py``, ``/goal``).
        self._goals: dict[str, goal_module.Goal] = {}
        # What the person typed while a turn ran (steering.py), per session.
        self._steering: dict[str, steering_module.Steering] = {}
        #: Sessions with a ``session/prompt`` in flight. One at a time: a second one used to reset the first turn's
        #: recorder, checkpoint, cancel flag and stream handle before Strands refused it, leaving Stop unable to stop.
        self._busy: set[str] = set()
        #: (session, run) pairs whose events are being forwarded (`_watch_run`).
        self._watching: set[tuple[str, str]] = set()
        #: The client asks the person before `auto`/`edit` and says so (`initialize`).
        self._confirm_modes = False

    def on_connect(self, conn: acp.Client) -> None:
        self._conn = conn

    async def initialize(self, protocol_version: int, **kwargs) -> acp.InitializeResponse:
        # A client that declares `clientCapabilities._meta.cordata.confirmModes` promises to ask the person before it
        # switches to `auto` or `edit`, and says so with `_meta.cordata.confirmed` — from then on a switch without it is
        # refused (docs/app.md §6.4). Clients that declare nothing (Zed, older terminals) are unchanged.
        capabilities = kwargs.get("client_capabilities")
        meta = (getattr(capabilities, "field_meta", None) or {}) if capabilities is not None else {}
        self._confirm_modes = bool(((meta or {}).get("cordata") or {}).get("confirmModes"))
        return acp.InitializeResponse(
            protocolVersion=acp.PROTOCOL_VERSION,
            agentCapabilities=acp.schema.AgentCapabilities(
                load_session=True,
                session_capabilities=acp.schema.SessionCapabilities(list=acp.schema.SessionListCapabilities()),
                # Embedded resources are inlined (`_extract_prompt_text`); images only when a vision model is configured
                # (`capture.visionModel`, the same gate as the menu bar's screenshot) — advertising them otherwise would
                # invite an attachment the turn then has to refuse.
                prompt_capabilities=acp.schema.PromptCapabilities(image=_vision_model_configured(), embedded_context=True),
            ),
        )

    # ------------------------------------------------------- live events

    def _send_ext(self, method: str, payload: dict) -> None:
        """Thread-safe: schedule one extension notification on the ACP loop."""
        loop, conn = self._loop, self._conn
        if loop is None or conn is None or loop.is_closed():
            return

        async def send() -> None:
            try:
                await conn.ext_notification(method, payload)
            except Exception:
                logging.getLogger(__name__).debug("%s notification not delivered", method, exc_info=True)

        loop.call_soon_threadsafe(lambda: loop.create_task(send()))

    def _send_update(self, session_id: str, update) -> None:
        """Thread-safe: schedule one ``session/update`` on the ACP loop (the harness's events arrive on tool threads)."""
        loop, conn = self._loop, self._conn
        if loop is None or conn is None or loop.is_closed():
            return

        async def send() -> None:
            try:
                await conn.session_update(session_id=session_id, update=update)
            except Exception:
                logging.getLogger(__name__).debug("session/update not delivered", exc_info=True)

        loop.call_soon_threadsafe(lambda: loop.create_task(send()))

    def _forward_event(self, kind: str, payload: dict) -> None:
        if "sessionId" not in payload and self._last_session:
            payload = {**payload, "sessionId": self._last_session}
        self._send_ext(f"cordata/{kind}", payload)
        if kind == "tasklist" and payload.get("sessionId"):
            # ACP's own plan, so any client shows the checklist (Zed's plan view); `_cordata/tasklist` stays for ours.
            self._send_update(str(payload["sessionId"]), _plan_update(payload.get("items") or []))

    def _usage_update(self, session_id: str, state: "_Session") -> None:
        """ACP ``usage_update``: the context window's fill, for any client's gauge. Skipped when the window is unknown
        (ACP needs a size). Token counts and the estimate flag ride in ``_meta.cordata``; no money here — only the
        orchestrator's pools have a price, and they report it themselves."""
        try:
            snapshot = session_extras.context_snapshot(state.agent)
            if not snapshot.get("window"):
                return
            tokens_in, tokens_out = self._tokens_used(state)
            update = acp.schema.UsageUpdate(
                used=int(snapshot["used"]),
                size=int(snapshot["window"]),
                session_update="usage_update",
                field_meta={"cordata": {"pct": snapshot.get("pct"), "estimated": bool(snapshot.get("estimated")), "messages": snapshot.get("messages"), "tokensIn": tokens_in, "tokensOut": tokens_out}},
            )
            self._send_update(session_id, update)
        except Exception:
            logging.getLogger(__name__).debug("usage update failed", exc_info=True)

    def _laya_notifier(self, session_id: str):
        """A thread-safe callback that forwards Laya's live triage events to the
        client as an ACP *extension notification* (method ``_cordata/laya``).
        The ACP spec says a client that doesn't understand an extension
        notification must ignore it, so Zed and other clients are unaffected;
        acp-tui renders it as the live "◆ laya" line.
        """

        if self._loop is None:
            self._loop = asyncio.get_running_loop()

        def notify(payload: dict) -> None:
            self._send_ext("cordata/laya", {"sessionId": session_id, **payload})

        return notify

    # ----------------------------------------------------------- sessions

    def _open_session(self, session_id: str, cwd: str) -> _Session:
        """``_build_session`` for a request from a client. A setup problem a person can fix (no model key, an unreachable
        provider) is raised as a JSON-RPC error that *carries its message*; left alone it reaches the client as
        "Internal error" and the reason is only in this process's log."""
        holder = state_store.claim_session(session_id, cwd=cwd)
        if holder is not None:
            raise acp.RequestError(
                -32000,
                f"This conversation is open in another process (pid {holder}); a second copy would fork its history. "
                "Attach through the daemon, or close the other one first.",
                {"error": "session_leased", "pid": holder},
            )
        _LEASED.add(session_id)
        try:
            return self._build_session(session_id, cwd)
        except BaseException as error:
            _LEASED.discard(session_id)
            state_store.release_session(session_id)
            if isinstance(error, RuntimeError):
                raise acp.RequestError(-32000, str(error), {"error": str(error)}) from error
            raise

    def _build_session(self, session_id: str, cwd: str) -> _Session:
        # Process-wide, not per-agent: strands_tools' file_read/file_write/
        # editor have no per-call working-directory parameter at all (only
        # shell does, via its own work_dir kwarg) — they just resolve
        # relative paths against os.getcwd(). A real chdir is the only fix
        # that covers all of them uniformly. Safe here specifically because
        # this ACP server process is per-Zed-connection (one project at a
        # time from the user's perspective), unlike main.py's AG-UI process,
        # which serves many concurrent web threads and deliberately never
        # touches cwd after its own one-time startup chdir into workspace/.
        try:
            os.chdir(cwd)
        except OSError as error:
            logging.getLogger(__name__).warning("cwd=<%s> | failed to chdir, tools will use the previous cwd: %s", cwd, error)
        if self._loop is None:
            self._loop = asyncio.get_running_loop()
        if self._unsubscribe is None:
            self._unsubscribe = harness_events.subscribe(self._forward_event)
        self._last_session = session_id

        preferences = settings_store.current(cwd)
        for warning in preferences.warnings:
            logging.getLogger(__name__).warning("settings: %s", warning)
        _apply_startup_preferences(preferences)

        # After `_apply_startup_preferences` and after `agent_common` has been
        # imported, which is where `settings.py` takes this harness's own
        # provider keys out of the environment — see `vault_proxy.install` for
        # why the order matters to the placeholders it writes.
        vault = vault_proxy.install(preferences)
        if vault.enabled:
            logging.getLogger(__name__).info("vault: %s", vault.reason)
            harness_events.emit(
                "vault",
                {
                    "event": "vault",
                    "phase": "blocked" if vault.blocks_egress else "ready",
                    "addr": vault.addr,
                    "reason": vault.reason,
                    "credentialed": vault.credentialed,
                },
            )

        # The project the search and outline tools read (process-wide, like the chdir above).
        code_search.configure(cwd)
        code_nav.configure(cwd)
        router = _router()
        router.policy = _router_policy(preferences)
        lsp_enabled = preferences.get("lsp.enabled", True) is not False
        manager = lsp_bridge.manager_for(cwd)
        manager.enabled = lsp_enabled

        reserved = {
            "shell", "editor", "file_read", "file_write", "python_repl", "http_request", "web_fetch", "apply_patch", "architect", "debugger",
            claude_code_tool.TOOL_NAME, orchestrate_tool.TOOL_NAME, "omlx_resource_manager", "ollama_resource_manager", "context_digest", "enable_tools", "search_skills",
            context_offload.RETRIEVE_TOOL, "code_search", "code_outline", "environment_profile", plugin_host.RESOURCE_TOOL, plan_mode.TOOL_NAME, ask_user.TOOL_NAME,
        }
        plugins = _plugin_host(cwd, reserved, trace_store.enabled(preferences))
        heal = SelfHealHooks(config=lambda: _heal_config(settings_store.current(cwd)), session_id=session_id, root=cwd)
        router_hooks = RouterHooks(router, notify=self._laya_notifier(session_id), plugin_host=plugins)
        steer = self._steering.setdefault(session_id, steering_module.Steering())
        handler = _ApprovalHandler(
            cwd,
            preferences.get("permissions.defaultMode", _DEFAULT_MODE),
            plugins=plugins,
            heal=heal,
            router_hooks=router_hooks,
            vault=vault,
        )
        specialist_tools, specialist_agents = specialists.specialist_tools(
            interventions=[handler],
            # A specialist's kit is fixed, so it gets the two history levers (cap one result, age old output)
            # and not the cold-tool scoping.
            hooks_factory=lambda: [
                code_nav.LargeFileHooks(code_nav.threshold_from(lambda: settings_store.current(cwd)), root=cwd),
                lsp_bridge.LspEditHooks(), router_hooks, outcome_hooks.OutcomeHooks(session_id), stall_guard.StallHooks(session_id),
                context_diet.ContextDiet(lambda: settings_store.current(cwd), scope_tools=False),
            ],
        )
        instructions = settings_store.load_instructions(cwd)
        context = workspace_context.summary(cwd, specialists=list(specialist_agents))
        system_prompt = (_chat_system_prompt(cwd) if _is_chat_scratch(cwd) else _project_system_prompt(cwd)) + f"\n\n{context}" + (f"\n\n{instructions}" if instructions else "") + prompts.live_text(cwd)
        # Order matters for AfterInvocationEvent (callbacks run in reverse):
        # self-heal is registered last so it runs first and can set `resume`
        # before the router decides whether the turn is over.
        agent = build_agent_for_session(
            session_id,
            interventions=[handler],
            system_prompt=system_prompt,
            # The Meta-Harness (Phase 6/7): the Claude Code CLI wrapper, the two memory
            # managers and the local digest. ACP-only, like the specialists — the web front
            # door has no vault status to route the CLI's egress through, no approval gate for it.
            extra_tools=[
                patch_tool.apply_patch,
                *specialist_tools,
                claude_code_tool.claude_code_engineer,
                orchestrate_tool.orchestrate,
                omlx_resource_tool.omlx_resource_manager,
                ollama_resource_tool.ollama_resource_manager,
                context_digest_tool.context_digest,
                context_diet.enable_tools,
                env_profile.environment_profile,
                plugin_host.resources_tool(plugins),
                task_tool.todo_write,
                plan_mode.present_plan,
                ask_user.ask_user,
            ],
            # A result over the limit is set aside whole — written to ~/.cordata/offloaded/<session>/, a head+tail preview and
            # a reference in context, `retrieve_offloaded_content` for the slice the agent wants (Strands' ContextOffloader).
            # What a specialist or the Claude Code CLI returns is the answer itself, so those are never set aside.
            offloader=context_offload.build(session_id, preferences=lambda: settings_store.current(cwd), never=set(specialist_agents)),
            summarizer=str(preferences.get("context.summaryModel", "fast")),
            compact_at=preferences.get("context.autoCompact"),
            project_root=cwd,
            # What each message carries along (ephemeral, never stored): the MCP servers this project has *now* — a
            # server discovered in the workspace and started in the background is part of the agent's picture from the
            # next message on — and the rules this person's rejections taught (rejection_memory.py).
            extra_plugins=[plugin_host.capabilities_injector(plugins), rejection_memory.constraints_injector(cwd, lambda: settings_store.current(cwd)), os_context.injector(session_id)],
            web_fetch_mode=str(preferences.get("context.webFetch", "agentic")),
            extra_hooks=[
                # Ollama Cloud's reasoning_effort per model call, from the project's size and Laya's reading of the turn.
                adaptive_effort.AdaptiveEffort(
                    cwd,
                    complexity=lambda agent: (lambda decision: decision.tier() if decision is not None else None)(router_hooks.current(agent)),
                    enabled=lambda: settings_store.current(cwd).get("reasoning.adaptive", True) is not False,
                    notify=lambda payload: harness_events.emit("effort", {**payload, "sessionId": session_id}),
                ),
                steer,
                lsp_bridge.LspEditHooks(), router_hooks, heal, outcome_hooks.OutcomeHooks(session_id, "orchestrator"), stall_guard.StallHooks(session_id, "orchestrator"),
                # Last, so it reads the route the router just set (BeforeInvocation runs in order) and caps a
                # tool result before the hooks registered earlier see it (AfterToolCall runs in reverse).
                context_diet.ContextDiet(lambda: settings_store.current(cwd), route=lambda agent: _route_of(router_hooks, agent)),
            ],
            exclude_tools={"agent_memory"},
            use_triage=False,
            untrusted_tools=plugins.untrusted_names,
            skill_dirs=learned.live_dirs(cwd),
            checkpointing=True,
            # Skills: the few that fit the request, found by embedding search; Laya decides whether one clearly
            # does (the router's own forward pass and calibrator — `skill_search.SkillDecider`).
            skill_options={
                "decider": skill_search.SkillDecider(
                    router.predict,
                    router.calibrator,
                    min_confidence=router.policy.min_confidence,
                    ready=_laya_loaded,
                ),
                "preferences": lambda: settings_store.current(cwd),
                "notify": lambda payload: harness_events.emit("skills", payload),
            },
        )
        handler.session_agent = agent
        handler.session_id = session_id
        # What a module-level Strands tool cannot read from its arguments: this session's
        # project, the vault's *live* status (the handler re-probes it), settings, the Lab.
        claude_code_tool.configure(
            cwd=lambda: cwd,
            vault=lambda: handler.vault,
            preferences=lambda: settings_store.current(cwd),
            session_id=lambda: session_id,
            lab=lambda: lab_module.get(),
        )
        orchestrate_tool.configure(cwd=lambda: cwd, vault=lambda: handler.vault, preferences=lambda: settings_store.current(cwd), session_id=lambda: session_id)
        task_tool.configure(lambda: session_id)
        self._wire_lab(session_id, cwd, agent, handler, specialist_agents)
        if preferences.get("model"):
            _apply_model(agent, preferences.get("model"))
        if preferences.get("temperature"):
            _apply_temperature(agent, preferences.get("temperature"))
        _apply_generation(agent, preferences)

        plugins.attach(agent)  # specialists keep their fixed kit; plugins reach the orchestrator
        threading.Thread(target=self._start_background, args=(plugins, manager, lsp_enabled), name="harness-start", daemon=True).start()
        if app_settings.LAYA_TRIAGE_MODE != "off":
            from prompt_security import warm_up_laya

            warm_up_laya()

        state = _Session(agent, handler, cwd, router_hooks, heal, specialist_agents, plugins, preferences)
        self._sessions[session_id] = state
        # The settings this session actually ran with, frozen at build time.
        # `values` is the merged, validated dotted-key view; `sources` says which
        # file set each key, which is what makes "who loosened that rule?"
        # answerable after the files on disk have moved on.
        state_store.record_session_config(
            session_id,
            cwd=cwd,
            mode=handler.mode,
            settings={"values": preferences.values, "sources": preferences.sources, "trusted": preferences.trusted},
        )
        _touch_index(session_id, cwd)
        return state

    def _wire_lab(self, session_id: str, cwd: str, agent, handler, specialist_agents: dict) -> None:
        """Register the orchestrator and the specialists as Lab pods and point the
        Lab's providers (threat reading, MCP list) at this session."""
        lab = lab_module.get()
        if lab.session_id not in (None, session_id):
            lab.reset()  # a different session: its pods are not the previous one's (isolation included)
        lab.session_id = session_id
        handler.lab = lab
        lab.connect_models()

        def risk_event(agent, tool_use=None) -> dict | None:
            return handler.reading_for((tool_use or {}).get("toolUseId"), agent)

        def mcp_names() -> list[str]:
            import mcp_manager

            return [str(server.get("name")) for server in mcp_manager.list_servers()]

        lab.risk_provider = risk_event
        lab.mcp_provider = mcp_names
        lab.mode_provider = lambda: handler.mode
        agent.hooks.add_hook(lab.attach(agent, "orchestrator", role="orchestrator"))
        for name, specialist in specialist_agents.items():
            specialist.hooks.add_hook(lab.attach(specialist, name, role=name, parent="orchestrator"))
        lab.emit(force=True)

    @staticmethod
    def _start_background(plugins: PluginHost, manager, lsp_enabled: bool) -> None:
        """Plugins and language servers start off the request path; their
        progress arrives as task events (progress bars in acp-tui)."""
        try:
            if not any(row["status"] in ("running", "starting") for row in plugins.status()):
                plugins.start_autostart()
            if lsp_enabled:
                manager.start_all()
        except Exception:
            logging.getLogger(__name__).warning("background start failed", exc_info=True)

    async def new_session(self, cwd: str, **kwargs) -> acp.NewSessionResponse:
        session_id = str(uuid.uuid4())
        state = self._open_session(session_id, _require_absolute(cwd))
        _touch_index(session_id, cwd)
        return acp.NewSessionResponse(
            sessionId=session_id,
            modes=acp.schema.SessionModeState(current_mode_id=state.handler.mode, available_modes=_MODES),
            config_options=_config_options(state.agent),
        )

    async def load_session(self, cwd: str, session_id: str, **kwargs) -> acp.LoadSessionResponse:
        # A session this process already holds is *attached to*, not rebuilt: the daemon's
        # conversation hub runs one child per conversation and lets a second front door (a
        # terminal) load it while it is live — possibly mid-turn. Rebuilding would replace the
        # running agent with last turn's snapshot under it. Its history is replayed from memory.
        attached = session_id in self._sessions
        state = self._sessions.get(session_id) or self._open_session(session_id, _require_absolute(cwd))

        if self._conn is not None:
            for update in _replay_history(state.agent.messages):
                await self._conn.session_update(session_id=session_id, update=update)
            checklist = task_tool.restore(session_id)
            if checklist:
                await self._conn.session_update(session_id=session_id, update=_plan_update(checklist))
            self._usage_update(session_id, state)
            checkpoint = _checkpoint_path(session_id)
            if checkpoint.exists():
                try:
                    data = json.loads(checkpoint.read_text())
                    where = f"{data['checkpoint'].get('position')}, cycle {data['checkpoint'].get('cycle_index')}"
                except (OSError, ValueError, KeyError):
                    where = "unknown point"
                age = time.time() - checkpoint.stat().st_mtime
                attempts = _auto_resume_attempts(session_id)
                wanted = settings_store.current(cwd).get("selfHeal.autoResume", True) is not False
                auto = wanted and age <= AUTO_RESUME_MAX_AGE and attempts < MAX_AUTO_RESUMES
                if auto:
                    advice = "Resuming it automatically."
                elif not wanted:
                    advice = "Send /resume-turn to continue it from there."
                elif attempts >= MAX_AUTO_RESUMES:
                    advice = f"It was resumed automatically {attempts} times from this point and stopped again, so it is not being retried on its own. Send /resume-turn to try once more."
                else:
                    advice = "It is too old to resume on its own (over 30 minutes). Send /resume-turn to continue it from there."
                await self._conn.session_update(
                    session_id=session_id,
                    update=acp.update_agent_message_text(f"\n[This session's last turn was interrupted at a checkpoint ({where}). {advice}]\n"),
                )
                self._send_ext("cordata/resume", {"sessionId": session_id, "available": True, "auto": bool(auto), "where": where, "ageSeconds": int(age), "attempts": attempts, "max": MAX_AUTO_RESUMES})
            # Approvals this session was parked on when its process went away.
            # The child that was blocked on the prompt is gone, so the request
            # will never arrive again — the ledger row is all that is left of
            # it, and saying so beats a resume that silently drops the question.
            # Only when this process has just rebuilt the session: one it already hosts may be parked on one of these
            # right now (a second front door attaching mid-turn), and that approval is live, not stale.
            pending = [] if attached else state_store.pending_interventions(session_id)
            if pending:
                asked = "\n".join(
                    f"  · {row.get('tool') or 'a tool'}"
                    + (f" — risk {row['risk_score']:.2f}" if row.get("risk_score") is not None else "")
                    + (f" ({row['note']})" if row.get("note") else "")
                    for row in pending
                )
                await self._conn.session_update(
                    session_id=session_id,
                    update=acp.update_agent_message_text(
                        f"\n[A previous run of this session was stopped while waiting for approval of "
                        f"{len(pending)} call{'s' if len(pending) != 1 else ''}, and it will not be asked again:\n"
                        f"{asked}\nSend the request again to be asked a second time.]\n"
                    ),
                )
                # Said once. Left staged they were announced again on every load, forever, and counted as waiting
                # for an answer by every front door that lists pending approvals.
                for row in pending:
                    state_store.resolve_intervention(str(row.get("id")), "expired", decided_by="load_session", note="its process ended before it was answered")
            if state.handler.vault is not None and state.handler.vault.blocks_egress:
                # Worth saying at resume rather than only at the first refusal:
                # the model can do a lot of work before it wants the network,
                # and "why is nothing able to reach anything" should not be a
                # question the user answers from the logs.
                vault = state.handler.vault
                await self._conn.session_update(
                    session_id=session_id,
                    update=acp.update_agent_message_text(
                        f"\n[Egress is blocked this session: {vault.reason}. shell, python_repl and "
                        "http_request will be refused until the vault is reachable, or until "
                        "vault.egressPolicy is set to \"proxy\".]\n"
                    ),
                )

        # Mode isn't persisted across restarts (the session manager only
        # persists agent messages) — a reloaded session comes back in the
        # configured default (permissions.defaultMode, "build" unless the user
        # chose otherwise), never a previously-chosen plan/auto.
        return acp.LoadSessionResponse(
            modes=acp.schema.SessionModeState(current_mode_id=state.handler.mode, available_modes=_MODES),
            config_options=_config_options(state.agent),
        )

    async def list_sessions(self, cwd: str | None = None, cursor: str | None = None, **kwargs) -> acp.schema.ListSessionsResponse:
        """Newest first, ``LIST_PAGE`` at a time; ``nextCursor`` is the last row's position, so a client pages back through
        all of them (this used to stop at 50 and ignore the cursor). Rows come from ``state.db``; conversations from before
        it are filled in from the legacy ``sessions/index.json``."""
        try:
            offset = max(0, int(cursor)) if cursor else 0
        except ValueError:
            raise acp.RequestError.invalid_params({"error": f"unknown cursor {cursor!r}"}) from None
        wanted = os.path.realpath(cwd) if cwd else None
        #: id -> (updated, cwd as given, title)
        rows: dict[str, tuple[float, str, str | None]] = {}
        for row in state_store.list_sessions(cwd=cwd, limit=100_000):
            rows[row["session_id"]] = (float(row.get("updated_ts") or 0.0), row.get("cwd_given") or row["cwd_real"], row.get("title"))
        for sid, meta in _read_index().items():
            if sid in rows or not isinstance(meta, dict):
                continue
            given = str(meta.get("cwd") or "")
            if wanted is not None and (not os.path.isabs(given) or os.path.realpath(given) != wanted):
                continue
            try:
                updated = float(calendar.timegm(time.strptime(str(meta.get("updatedAt")), "%Y-%m-%dT%H:%M:%SZ")))
            except (TypeError, ValueError):
                updated = 0.0
            rows[sid] = (updated, given, meta.get("title"))
        ordered = sorted(((sid, *rest) for sid, rest in rows.items() if session_store.exists(sid, SESSIONS_DIR)), key=lambda r: (r[1], r[0]), reverse=True)
        page = ordered[offset : offset + LIST_PAGE]
        more = offset + LIST_PAGE < len(ordered)
        return acp.schema.ListSessionsResponse(
            sessions=[acp.schema.SessionInfo(session_id=sid, cwd=given, title=title, updated_at=_iso(updated)) for sid, updated, given, title in page],
            next_cursor=str(offset + LIST_PAGE) if more else None,
        )

    def _resolve(self, session_id: str | None) -> tuple[str | None, "_Session | None"]:
        """``_known`` plus the id this process holds the session under — what ``_goals``, ``_streaming`` and the ledger
        are keyed by. ``(session_id, None)`` when there is nothing to resolve to."""
        state = self._known(session_id) if session_id else (self._sessions.get(self._last_session) if self._last_session else None)
        if state is None:
            return session_id, None
        return next((sid for sid, s in self._sessions.items() if s is state), session_id), state

    def _known(self, session_id: str) -> "_Session | None":
        """The session this id names — or, for an id this process never minted, the one it is hosting.

        Under the daemon a front door that is not the driving client (the GUI's mode switch, the
        Discord bot) knows the *daemon's* id for this child, not the ACP id `new_session` made here.
        An id nobody here owns can only mean "the session in this process", so it is resolved to the
        newest one rather than silently doing nothing. An id that *is* known is never remapped."""
        state = self._sessions.get(session_id)
        if state is None and self._last_session:
            state = self._sessions.get(self._last_session)
        return state

    async def set_session_mode(self, session_id: str, mode_id: str, **kwargs) -> acp.SetSessionModeResponse | None:
        state = self._known(session_id)
        session_id = next((sid for sid, s in self._sessions.items() if s is state), session_id)
        if state is None or mode_id not in _MODE_IDS:
            return None
        if mode_id in ("auto", "edit") and mode_id != state.handler.mode:
            confirmed = bool((((kwargs.get("field_meta") or {}).get("cordata")) or {}).get("confirmed"))
            if self._confirm_modes and not confirmed:
                raise acp.RequestError.invalid_params({"error": f"switching to {mode_id} needs the person's confirmation", "needs": "confirm_mode"})
            state_store.append_event("mode", {"from": state.handler.mode, "to": mode_id, "confirmed": confirmed, "via": "acp"}, session_id=session_id, source="acp_server")
        if mode_id != state.handler.mode:
            state.adaptive_hold = mode_advisor.HOLD_TURNS  # the person chose: adaptive mode keeps out of the way for a while
        state.handler.mode = mode_id
        # Every client on this session hears it — the one that asked included, so all of them read the same stream.
        self._send_update(session_id, acp.schema.CurrentModeUpdate(current_mode_id=mode_id, session_update="current_mode_update"))
        # ACP has no push for a client-initiated change, so another front end watching this
        # session (the Lab) would never learn of it: publish a snapshot, which carries the mode.
        lab = lab_module.get()
        if lab.session_id == session_id:
            lab.emit(force=True)
        return acp.schema.SetSessionModeResponse()

    async def set_config_option(self, config_id: str, session_id: str, value, **kwargs):
        state = self._known(session_id)
        if state is None:
            return None
        if config_id == "model" and isinstance(value, str):
            _apply_model(state.agent, value)
        elif config_id == "temperature" and isinstance(value, str):
            _apply_temperature(state.agent, value)
        options = _config_options(state.agent)
        owner = next((sid for sid, s in self._sessions.items() if s is state), session_id)
        self._send_update(owner, acp.schema.ConfigOptionUpdate(config_options=options, session_update="config_option_update"))
        return acp.schema.SetSessionConfigOptionResponse(config_options=options)

    async def cancel(self, session_id: str, **kwargs) -> None:
        session_id = self._resolve(session_id)[0] or session_id
        self._cancelled.add(session_id)
        # Strands' own cancellation (docs: agent-loop → Cancellation): the loop stops at its next checkpoint — before the
        # next model call, before the next tool — with a `cancelled` result, and a tool that polls
        # `tool_context.cancel_signal` (the Claude Code run, which spends the person's usage) stops mid-flight.
        # Only an agent that is running has anything to cancel: the signal is cleared when an invocation *completes*,
        # so one set on an idle agent would silently cancel its next turn (lab.isolate guards the same way).
        agent = self._streaming.get(session_id)
        if agent is not None:
            try:
                agent.cancel()
            except Exception:
                logging.getLogger(__name__).debug("agent.cancel failed", exc_info=True)

    # ------------------------------------------------------ ext methods

    async def ext_method(self, method: str, params: dict) -> dict:
        """``_cordata/*`` requests from acp-tui: plugins, LSP, Laya stats,
        self-heal state. Unknown methods raise (JSON-RPC method-not-found)."""
        name = method.removeprefix("_").removeprefix("cordata/")
        # Resolved like the mode switch: a front door under the daemon knows the daemon's id for this child, not the
        # one ``new_session`` minted here, and used to get "no such session" from every method below.
        session_id, state = self._resolve(params.get("sessionId") or self._last_session)
        cwd = state.cwd if state else os.getcwd()
        if name == "plugins/list":
            host = state.plugins if state else _plugin_host(cwd, set())
            return {"plugins": host.status(), "warnings": host.warnings}
        if name in ("plugins/enable", "plugins/disable"):
            host = state.plugins if state else _plugin_host(cwd, set())
            target = str(params.get("name") or "")
            persist = bool(params.get("persist", True))
            fn = host.enable if name.endswith("enable") else host.disable
            row = await run_blocking(fn, target, persist)
            return row
        if name == "plugins/reload":
            host = state.plugins if state else _plugin_host(cwd, set())
            host.discover()
            return {"plugins": host.status(), "warnings": host.warnings}
        if name == "lsp/status":
            manager = lsp_bridge.manager_for(cwd)
            return {"servers": manager.status(), "enabled": manager.enabled, "trusted": lsp_bridge.folder_trusted(cwd)}
        if name == "lsp/diagnostics":
            # The editor's squiggles, from the same trust-gated language servers the agent's tools use (one set of
            # servers per project; none starts in an untrusted folder). `text` checks unsaved contents without writing.
            return await run_blocking(_lsp_diagnostics, cwd, str(params.get("path") or ""), params.get("text"))
        if name == "orchestrator/status":
            # Quota pools, recent runs and handoffs, read from the ledger — no model call (orchestrator/status.py).
            from orchestrator.status import status_data

            return await run_blocking(status_data, cwd)
        if name.startswith("orchestrator/"):
            return await self._orchestrator_control(name.removeprefix("orchestrator/"), params, session_id, state, cwd)
        if name.startswith("settings/") or name.startswith("trust/"):
            return await self._settings_control(name, params, session_id, state, str(params.get("cwd") or cwd))
        if name == "doctor":
            from orchestrator import doctor

            checks = await run_blocking(doctor.run_checks, os.path.realpath(str(params.get("cwd") or cwd)))
            return {"checks": [check.to_json() for check in checks]}
        if name == "auto/readiness":
            return await run_blocking(self._auto_readiness, state, str(params.get("cwd") or cwd))
        if name.startswith("artifacts/"):
            from orchestrator import control

            run_id = str(params.get("runId") or "")
            if name == "artifacts/list":
                return {"runId": run_id, "artifacts": await run_blocking(control.artifacts_list, run_id)}
            if name == "artifacts/read":
                return await run_blocking(lambda: control.artifacts_read(run_id, str(params.get("name") or ""), offset=int(params.get("offset") or 0), limit=int(params.get("limit") or control.READ_LIMIT)))
            raise acp.RequestError.method_not_found(method)
        if name == "context/usage":
            return session_extras.context_snapshot(state.agent) if state else {"used": 0, "window": None, "pct": None, "messages": 0, "estimated": True}
        if name == "context/compact":
            return await self._compact(session_id, state, str(params.get("focus") or ""))
        if name == "steer":
            # A note for the running turn (steering.py): reaches the agent at its next model call.
            steer = self._steering.get(session_id or "")
            if steer is None:
                return {"ok": False, "reason": "no session"}
            accepted = steer.add(str(params.get("text") or ""))
            return {"ok": accepted, "pending": steer.pending(), **({} if accepted else {"reason": "empty, or too many notes waiting"})}
        if name == "steer/drain":
            steer = self._steering.get(session_id or "")
            return {"notes": steer.drain() if steer is not None else []}
        if name == "btw":
            return await self._aside(state, str(params.get("question") or ""))
        if name == "goal/status":
            goal = self._goals.get(session_id) if session_id else None
            return {"goal": goal.public() if goal else None}
        if name == "goal/clear":
            return {"cleared": self._clear_goal(session_id, "cleared by the person") if session_id else False}
        if name == "tasks/list":
            return {"items": task_tool.restore(session_id) if session_id else []}
        if name == "session/history":
            # A page of a conversation's history, newest first, read from its snapshot — no agent is built for it, so
            # this answers for a conversation no process is running too (`session_history.page`).
            wanted = str(params.get("sessionId") or session_id or "")
            if not wanted:
                raise acp.RequestError.invalid_params({"error": "session/history needs a sessionId"})
            cursor = params.get("cursor")
            return session_history.page(wanted, SESSIONS_DIR, cursor=int(cursor) if cursor is not None else None, limit=int(params.get("limit") or session_history.PAGE_DEFAULT), kind_of=_tool_kind)
        if name == "laya/stats":
            router = _router()
            return {
                "summary": summarize_stats(router.calibrator),
                "stats": router.calibrator.stats(),
                "weights": router.calibrator.weights(),
                "policy": router.policy.__dict__,
            }
        if name == "os-context":
            # The menu bar's capture, held for this session's next turn (os_context.py). Registered
            # here rather than sent inside the prompt so the person's words stay the prompt and the
            # screen arrives as wrapped untrusted data — and so it is gone at the turn's end.
            if not session_id:
                raise acp.RequestError.invalid_params("os-context needs a sessionId")
            payload = {key: value for key, value in params.items() if key != "sessionId"}
            os_context.register(session_id, payload)
            return {
                "ok": True,
                "app": str(payload.get("app_name") or ""),
                "denied": os_context.is_denied(str(payload.get("app_bundle_id") or "")),
            }
        if name.startswith("lab/"):
            return await self._lab_control(name.removeprefix("lab/"), params, session_id)
        if name.startswith("skills/"):
            return await self._skills_control(name.removeprefix("skills/"), params, cwd)
        if name.startswith("checkpoints/"):
            return await self._checkpoint_control(name.removeprefix("checkpoints/"), params, session_id, cwd)
        if name.startswith("conversation/"):
            return await self._conversation_control(name.removeprefix("conversation/"), params, session_id, state)
        if name == "heal/status":
            heal = state.heal if state else None
            episodes = [
                {"command": ep.command, "status": ep.status, "attempts": len(ep.attempts), "root_cause": ep.root_cause, "verified": ep.verified}
                for ep in (heal.episodes.values() if heal else [])
            ]
            config = heal.config if heal else HealConfig()
            memory = await run_blocking(fix_memory.stats, cwd) if config.memory else None
            return {"episodes": episodes, "config": config.__dict__, "memory": memory}
        if name.startswith("ollama/"):
            action = name.removeprefix("ollama/")
            model = str(params.get("model") or "")
            needed_gb = float(params.get("needed_gb") or 0.0)
            return await run_blocking(ollama_resource_tool.ollama_resource_manager, action, model, needed_gb)
        raise acp.RequestError.method_not_found(method)

    async def _orchestrator_control(self, action: str, params: dict, session_id: str | None, state: "_Session | None", cwd: str) -> dict:
        """Mission Control's requests (docs/app.md §6.2). A run started here is a detached process (it outlives this
        session, and the daemon reaping it); its events are forwarded to this session as ``_cordata/orchestrator`` while
        it runs, and any front door catches up from its events file. Every change is audited before it takes effect."""
        from orchestrator import control

        run_id = str(params.get("runId") or "")
        if action == "runs":
            states = params.get("states")
            return {"runs": await run_blocking(lambda: control.list_runs(cwd=str(params.get("cwd") or cwd), session_id=params.get("sessionId") if params.get("mine") else None, states=list(states) if isinstance(states, list) else None, limit=int(params.get("limit") or 30), before=params.get("before")))}
        if action == "show":
            shown = await run_blocking(control.show_run, run_id)
            return shown if shown is not None else {"ok": False, "reason": f"no run {run_id}"}
        if action == "events":
            return await run_blocking(lambda: control.read_events(run_id, int(params.get("offset") or 0), int(params.get("limit") or 500)))
        if action == "watch":
            self._watch_run(session_id, run_id, int(params.get("offset") or 0))
            return {"ok": True, "runId": run_id}
        if action in ("pause", "cancel"):
            now, restore = bool(params.get("now")), bool(params.get("restore")) and action == "cancel"
            state_store.append_event("orchestrator_control", {"action": action, "run_id": run_id, "now": now, "restore": restore}, session_id=session_id, source="acp_server")
            return await run_blocking(lambda: control.request(run_id, action, now=now, restore=restore, by="person"))
        if action == "resume":
            direct = bool(params.get("allowDirect"))
            if direct and not params.get("confirmed"):
                return {"ok": False, "reason": "direct network access needs the person's confirmation (a trust card), not just a flag", "needs": "confirm_direct"}
            state_store.append_event("orchestrator_control", {"action": "resume", "run_id": run_id, "allow_direct": direct}, session_id=session_id, source="acp_server")
            result = await run_blocking(lambda: control.resume_run(run_id, session_id=session_id, allow_direct=direct))
            if result.get("ok"):
                self._watch_run(session_id, run_id, _events_size(run_id))
            return result
        if action == "start":
            # `start` does not pass through the Strands approval handler (no tool call asks for it), so the gates that
            # handler would apply are applied here.
            goal = str(params.get("goal") or "").strip()
            if not goal:
                return {"ok": False, "reason": "a run needs a goal"}
            if state is not None and state.handler.mode == "plan":
                return {"ok": False, "reason": "plan mode runs nothing — leave plan mode (approve a plan) to start a run"}
            target = os.path.realpath(str(params.get("cwd") or cwd))
            if state is not None and target != os.path.realpath(state.cwd):
                return {"ok": False, "reason": "a run starts in this session's project"}
            direct = bool(params.get("allowDirect"))
            if direct and not params.get("confirmed"):
                # Coding CLIs never get direct network access by a flag alone: the vault fails closed unless the person
                # has said so on a trust card.
                return {"ok": False, "reason": "direct network access needs the person's confirmation (a trust card), not just a flag", "needs": "confirm_direct"}
            options = {k: params.get(k) for k in ("workers", "maxUsd", "maxParallel", "research", "harden", "conformance", "tier2", "dispatch") if params.get(k) is not None}
            options["allowDirect"] = direct
            run_id = control.new_run_id()
            state_store.append_event("orchestrator_control", {"action": "start", "run_id": run_id, "goal": secret_patterns.scrub(goal)[0][:300], "options": options}, session_id=session_id, source="acp_server")
            result = await run_blocking(lambda: control.spawn_run(goal, target, session_id=session_id, run_id=run_id, options=options))
            if result.get("ok"):
                self._watch_run(session_id, run_id, 0)
            return result
        raise acp.RequestError.method_not_found(f"_cordata/orchestrator/{action}")

    async def _settings_control(self, name: str, params: dict, session_id: str | None, state: "_Session | None", cwd: str) -> dict:
        """The settings and trust a front door shows and edits (docs/app.md §6.4) — through `settings_store`, so the
        validation, the trust gate and the user-only groups are the backend's one implementation, not a third copy.
        Every write is audited. `trust/grant` exists only here (an ext method): no tool can call it, so the model cannot
        trust a folder for itself, and it takes the person's `confirmed: true` (the trust card's answer)."""
        cwd = os.path.realpath(cwd)
        if name == "settings/get":
            loaded = await run_blocking(settings_store.current, cwd)
            return {
                "cwd": cwd,
                "trusted": loaded.trusted,
                "values": loaded.values,
                "sources": loaded.sources,
                "rules": [{"kind": r.kind, "text": r.text, "scope": r.scope, "active": r.active} for r in loaded.rules],
                "warnings": loaded.warnings,
                "paths": loaded.paths,
            }
        if name == "settings/schema":
            return {
                "keys": [
                    {"key": key, "kind": spec[0], "args": list(spec[1:]) if len(spec) > 1 else [], "trustGated": key in settings_store.TRUST_GATED_KEYS, "userOnly": settings_store.user_only(key)}
                    for key, spec in settings_store.SCHEMA.items()
                ],
                "ruleKinds": list(settings_store.RULE_KINDS),
            }
        if name == "settings/set":
            scope, key, value = str(params.get("scope") or "user"), str(params.get("key") or ""), params.get("value")
            if scope not in settings_store.SCOPES:
                return {"ok": False, "reason": f"scope must be one of {', '.join(settings_store.SCOPES)}"}
            try:
                path = await run_blocking(settings_store.set_value, scope, key, value, cwd)
            except ValueError as error:
                return {"ok": False, "reason": str(error)}
            state_store.append_event("settings", {"action": "set", "scope": scope, "key": key, "value": value if not isinstance(value, str) else value[:200]}, session_id=session_id, source="acp_server")
            loaded = settings_store.current(cwd)
            applied = loaded.sources.get(key) == scope or (value is None and key not in loaded.sources)
            return {"ok": True, "path": str(path), "applied": applied, "warnings": [w for w in loaded.warnings if key in w]}
        if name == "settings/rules/add":
            kind, rule, scope = str(params.get("kind") or ""), str(params.get("rule") or ""), str(params.get("scope") or "user")
            try:
                path = await run_blocking(settings_store.add_rule, kind, rule, scope, cwd)
            except ValueError as error:
                return {"ok": False, "reason": str(error)}
            state_store.append_event("settings", {"action": "rule_add", "kind": kind, "rule": rule[:200], "scope": scope}, session_id=session_id, source="acp_server")
            return {"ok": True, "path": str(path)}
        if name == "settings/rules/remove":
            rule = str(params.get("rule") or "")
            changed = await run_blocking(settings_store.remove_rule, rule, cwd)
            state_store.append_event("settings", {"action": "rule_remove", "rule": rule[:200]}, session_id=session_id, source="acp_server")
            return {"ok": bool(changed), "paths": [str(p) for p in changed]}
        if name == "trust/status":
            return await run_blocking(self._trust_status, state, cwd)
        if name == "trust/list":
            return {"projects": await run_blocking(settings_store.trusted_projects, cwd)}
        if name == "trust/revoke":
            target = str(params.get("path") or cwd)
            if not os.path.isabs(target):
                return {"ok": False, "reason": "path must be absolute"}
            state_store.append_event("trust", {"action": "revoke", "cwd": os.path.realpath(target)}, session_id=session_id, source="acp_server")
            written = await run_blocking(settings_store.untrust_project, target)
            if state is not None and os.path.realpath(state.cwd) == os.path.realpath(target):
                # What trust unlocked (project MCP servers, allow rules, gated keys) stops applying from now on.
                await run_blocking(state.plugins.discover)
            return {"ok": True, "changed": written is not None, "trusted": settings_store.current(cwd).trusted}
        if name == "trust/grant":
            if not params.get("confirmed"):
                return {"ok": False, "reason": "trusting a folder is the person's decision — answer the trust card", "needs": "confirm_trust"}
            state_store.append_event("trust", {"action": "grant", "cwd": cwd}, session_id=session_id, source="acp_server")
            path = await run_blocking(settings_store.trust_project, cwd)
            if state is not None and os.path.realpath(state.cwd) == cwd:
                # What trust unlocks (project MCP servers, allow rules, gated keys) applies from now on.
                await run_blocking(state.plugins.discover)
            return {"ok": True, "path": str(path), "trusted": settings_store.current(cwd).trusted}
        raise acp.RequestError.method_not_found(f"_cordata/{name}")

    @staticmethod
    def _trust_status(state: "_Session | None", cwd: str) -> dict:
        """What trusting this folder would turn on, so the card shows it before the person decides: the settings and
        allow rules the project sets that are ignored now, the project MCP servers held back (their exact commands), and
        the instruction files the agent reads."""
        loaded = settings_store.current(cwd)
        ignored = [w for w in loaded.warnings if "not trusted" in w]
        host = state.plugins if state is not None and os.path.realpath(state.cwd) == cwd else _plugin_host(cwd, set())
        blocked = []
        for row in host.status():
            spec = getattr(host, "_specs", {}).get(row["name"])
            # Held back: a project's server in an untrusted folder, whether or not anything tried to start it yet.
            if spec is None or not (row.get("status") == "blocked" or (spec.needs_trust and not loaded.trusted and row.get("status") != "running")):
                continue
            config = spec.config or {}
            blocked.append({
                "name": row["name"],
                "source": row.get("source"),
                "file": spec.path,
                "command": config.get("command") or config.get("url"),
                "args": [str(a) for a in config.get("args") or []],
                "envKeys": sorted((config.get("env") or {}).keys()),
                "reason": row.get("error") or "folder not trusted",
            })
        project_values = {key: value for key, value in _project_settings(cwd).items()}
        return {
            "cwd": cwd,
            "trusted": loaded.trusted,
            "ignored": ignored,
            "blockedPlugins": blocked,
            "instructions": [{"scope": scope, "path": str(path)} for scope, path in settings_store.instruction_files(cwd)],
            "gatedKeys": sorted(k for k in project_values if k in settings_store.TRUST_GATED_KEYS or k in ("permissions.defaultMode", "selfHeal.autoApply")),
            "projectSettings": {k: v for k, v in project_values.items() if k in ("permissions.defaultMode", "selfHeal.autoApply", "preflight.tests")},
            "allowRules": [r.text for r in loaded.rules if r.kind == "allow" and r.scope in ("project", "local")],
        }

    def _auto_readiness(self, state: "_Session | None", cwd: str) -> dict:
        """Before `auto`: what will still protect the project (docs/app.md §6.4) — so a confirmation card states it."""
        cwd = os.path.realpath(cwd)
        loaded = settings_store.current(cwd)
        unviable = checkpoints._unviable(cwd)
        enabled = bool(loaded.get("checkpoints.enabled", True))
        vault = state.handler.vault if state is not None else None
        return {
            "cwd": cwd,
            "checkpoints": {"enabled": enabled, "viable": unviable is None, "reason": unviable},
            "guard": {"callGuard": loaded.get("laya.callGuard", "on") != "off", "riskDestroy": loaded.get("laya.riskDestroy")},
            "vault": {"enabled": bool(vault and getattr(vault, "enabled", False)), "blocksEgress": bool(vault and getattr(vault, "blocks_egress", False)), "reason": getattr(vault, "reason", None) if vault else None},
            "trusted": loaded.trusted,
            "denyRules": [r.text for r in loaded.rules if r.kind == "deny" and r.active],
            "stillAsks": ["calls Laya rates destructive", "anything a deny rule names", "pulling a model"],
            "ready": enabled and unviable is None,
        }

    def _watch_run(self, session_id: str | None, run_id: str, offset: int) -> None:
        """Forward a run's events to this session's client while it runs (a thread tailing its events file). Ends when
        the run reaches a terminal or paused state and its file has been read to the end."""
        from orchestrator import control

        if not session_id or not run_id:
            return
        key = (session_id, run_id)
        if key in self._watching:
            return
        self._watching.add(key)

        def run() -> None:
            position, idle = offset, 0
            try:
                while True:
                    batch = control.read_events(run_id, position, 200)
                    position = batch["next"]
                    for event in batch["events"]:
                        forwarded = orchestrate_tool._forwardable(event) if isinstance(event, dict) else None
                        if forwarded is not None:
                            self._send_ext("cordata/orchestrator", {"sessionId": session_id, **forwarded})
                    if batch["events"]:
                        idle = 0
                        continue
                    record_state = (control.show_run(run_id) or {}).get("live")
                    if record_state in ("done", "failed", "budget_exhausted", "cancelled", "paused", "interrupted"):
                        idle += 1
                        if idle >= 3:
                            return
                    time.sleep(0.5)
            except Exception:
                logging.getLogger(__name__).debug("watching run %s failed", run_id, exc_info=True)
            finally:
                self._watching.discard(key)

        threading.Thread(target=run, name=f"watch-{run_id}", daemon=True).start()

    async def _skills_control(self, action: str, params: dict, cwd: str) -> dict:
        """``skills/list`` · ``skills/show`` · ``skills/promote`` · ``skills/reject`` · ``skills/retire``: the learned skills
        for this project. A person's promote goes through the same filter as the gate's; it skips the evaluation,
        and the ledger says who did it."""
        name = str(params.get("name") or "")
        if action == "list":
            rows = await run_blocking(learned.list_skills, cwd)
            return {"skills": [{k: r.get(k) for k in ("name", "status", "scope", "description", "created", "uses", "reason", "signals", "evals", "promoted_by")} for r in rows[:60]],
                    "promotion": settings_store.current(cwd).get("learning.promotion", "off")}
        if action == "show":
            row = learned.find(name, cwd)
            if row is None:
                return {"ok": False, "reason": f"no learned skill {name!r}"}
            return {"ok": True, "skill": {k: row.get(k) for k in ("name", "status", "scope", "description", "signals", "evals", "uses", "rewards", "reason")}, "text": learned.read_text(row)}
        if action == "promote":
            return await run_blocking(learned.promote, name, cwd, evidence={"by": "person"}, by="person")
        if action == "reject":
            return await run_blocking(learned.reject, name, cwd, str(params.get("reason") or "rejected by a person"))
        if action == "retire":
            return await run_blocking(learned.retire, name, cwd, str(params.get("reason") or "retired by a person"), "person")
        raise acp.RequestError.method_not_found(f"_cordata/skills/{action}")

    async def _checkpoint_control(self, action: str, params: dict, session_id: str | None, cwd: str) -> dict:
        """``checkpoints/list`` · ``checkpoints/undo`` · ``checkpoints/restore``. Anything that changes files is a
        dry run unless the caller says ``apply`` — and is refused while a turn is running, because a restore under a
        working agent is two writers on one tree. Applied restores are audited in the ledger."""
        if action == "list":
            preferences = settings_store.current(cwd)
            return {
                "enabled": bool(preferences.get("checkpoints.enabled", True)),
                "project": cwd,
                "turns": await run_blocking(checkpoints.turns, cwd, int(params.get("limit") or 8)),
            }
        if action not in ("undo", "restore"):
            raise acp.RequestError.method_not_found(f"_cordata/checkpoints/{action}")
        apply = bool(params.get("apply"))
        if apply and outcomes.active(session_id) is not None:
            return {"ok": False, "reason": "a turn is running — stop it first, then undo"}
        paths = [str(p) for p in params.get("paths") or []] or None
        if action == "undo":
            result = await run_blocking(lambda: checkpoints.undo_turn(cwd, params.get("turn") or None, dry_run=not apply, paths=paths, session_id=session_id or ""))
        else:
            result = await run_blocking(lambda: checkpoints.restore(cwd, str(params.get("id") or ""), paths=paths, dry_run=not apply, session_id=session_id or ""))
        if apply:
            audit = {"action": action, "ok": result.get("ok"), "target": result.get("target"), "turn": result.get("turn"), "files": result.get("count"), "undo_with": result.get("undo_with")}
            state_store.append_event("checkpoint_restore", audit, session_id=session_id, source="checkpoints")
            harness_events.emit("checkpoint", {"event": "checkpoint", "phase": "restored", "files": result.get("count"), "ok": result.get("ok")})
        return result

    async def _conversation_control(self, action: str, params: dict, session_id: str | None, state: "_Session | None") -> dict:
        """``conversation/list`` · ``conversation/rewind`` — take the *conversation* back (the agent forgets the last turn or
        turns). The files are not touched: that is ``checkpoints/undo``. Refused while a turn is running (a rewind under a
        working agent is two writers on one history). The state it leaves is saved as a checkpoint, so it can be undone."""
        if state is None or session_id is None:
            return {"ok": False, "reason": "no such session"}
        if action == "list":
            return session_store.listing(state.agent, SESSIONS_DIR, int(params.get("limit") or 15))
        if action == "fork":
            # A new conversation from this one as it stood before turn `turn` (or at `checkpoint`, or now). Files are not
            # touched, and the source keeps running: only its snapshots are read.
            turn = params.get("turn")
            result = session_history.fork(session_id, SESSIONS_DIR, checkpoint=str(params.get("checkpoint") or "") or None, turn=int(turn) if turn else None)
            if result.get("ok"):
                title = state_store.get_session(session_id) or {}
                state_store.touch_session(result["sessionId"], cwd=state.cwd, title=f"fork of {title.get('title') or session_id[:8]}", origin="fork")
                state_store.append_event("conversation_fork", {"from": session_id, "to": result["sessionId"], "turn": turn, "checkpoint": params.get("checkpoint")}, session_id=session_id, source="session_history")
            return result
        if action != "rewind":
            raise acp.RequestError.method_not_found(f"_cordata/conversation/{action}")
        if session_id in self._streaming or outcomes.active(session_id) is not None:
            return {"ok": False, "reason": "a turn is running — stop it first, then go back"}
        index = params.get("index")
        result = await session_store.go_back(state.agent, SESSIONS_DIR, turns=int(params.get("turns") or 1), to=str(params.get("to") or "") or None, index=int(index) if index else None)
        if result.get("ok"):
            # What a half-finished turn left beside the history it belonged to is stale now.
            _checkpoint_path(session_id).unlink(missing_ok=True)
            _resume_marker_path(session_id).unlink(missing_ok=True)
            audit = {"turn": result.get("turn"), "now": result.get("now"), "forgot": len(result.get("forgot") or []), "restored": result.get("to"), "undo_with": result.get("undo_with")}
            state_store.append_event("conversation_rewind", audit, session_id=session_id, source="session_store")
            self._send_ext("cordata/conversation", {"sessionId": session_id, "event": "rewound", "now": result.get("now"), "forgot": result.get("forgot") or []})
            self._usage_update(session_id, state)
        return result

    async def _lab_control(self, action: str, params: dict, session_id: str | None) -> dict:
        """The Lab Master's controls. Each is audited in the ledger before it takes
        effect — an E-Stop or a loosened dial is exactly what "who did that?" is for."""
        lab = lab_module.get()
        pod = str(params.get("pod") or "")
        if action == "state":
            return lab.snapshot()
        try:
            if action == "select":
                lab.select(pod or None)
                return {"focus": lab.focus}
            if action not in ("isolate", "release", "dial", "inject", "effort"):
                raise acp.RequestError.method_not_found(f"_cordata/lab/{action}")
            audit = {"action": action, "pod": pod}
            if action in ("dial", "effort"):
                audit["value"] = params.get("value")
            if action == "inject":
                audit["chars"] = len(str(params.get("text") or ""))
            state_store.append_event("lab", audit, session_id=session_id, source="lab")
            if action == "isolate":
                # The pod and everything it spawned: stopping an orchestrator while
                # its specialist keeps working would not be a stop.
                reply = lab.isolate(pod)
                reply["also"] = []
                for descendant in lab.descendants(pod):
                    lab.isolate(descendant, reason=f"parent {pod} isolated")
                    reply["also"].append(descendant)
                if pod == "orchestrator" and session_id:
                    self._cancelled.add(session_id)
                return reply
            if action == "release":
                return lab.release(pod)
            if action == "dial":
                return lab.set_dial(pod, float(params.get("value")))
            if action == "effort":
                return lab.set_effort(pod, str(params.get("value") or ""))
            return lab.inject(pod, str(params.get("text") or ""))
        except KeyError as error:
            raise acp.RequestError.invalid_params({"error": str(error.args[0] if error.args else error)})
        except (ValueError, TypeError, PermissionError) as error:
            raise acp.RequestError.invalid_params({"error": str(error)})

    async def ext_notification(self, method: str, params: dict) -> None:
        return None

    # ------------------------------------------------ context, aside, goals

    async def _compact(self, session_id: str | None, state: "_Session | None", focus: str) -> dict:
        if state is None or session_id is None:
            return {"ok": False, "reason": "no such session"}
        if session_id in self._streaming or outcomes.active(session_id) is not None:
            return {"ok": False, "reason": "a turn is running — stop it first, then compact"}
        result = await run_blocking(session_extras.compact, state.agent, focus)
        if result.get("ok"):
            state_store.append_event("conversation_compact", {"before": result["before"], "after": result["after"], "focus": bool(focus)}, session_id=session_id, source="session_extras")
            self._send_ext("cordata/context", {"sessionId": session_id, **result["after"]})
            self._usage_update(session_id, state)
        return result

    async def _aside(self, state: "_Session | None", question: str) -> dict:
        """``/btw``: answered from a copy of the conversation by a tool-less model; nothing is written back."""
        if state is None:
            return {"ok": False, "reason": "no such session"}
        if not question.strip():
            return {"ok": False, "reason": "ask something: /btw <question>"}
        messages = list(state.agent.messages)
        try:
            answer = await run_blocking(session_extras.ask_aside, question, messages, session_extras.one_shot)
        except Exception as error:
            return {"ok": False, "reason": f"could not answer: {error}"}
        return {"ok": True, "answer": answer}

    def _announce_goal(self, session_id: str, goal: "goal_module.Goal", event: str, **extra) -> None:
        self._send_ext("cordata/goal", {"sessionId": session_id, "event": event, **goal.public(), **extra})

    def _stop_goal(self, session_id: str, reason: str) -> None:
        goal = self._goals.get(session_id)
        if goal is not None and goal.state == "active":
            goal.state, goal.reason = "stopped", reason
            self._announce_goal(session_id, goal, "stopped")

    def _clear_goal(self, session_id: str, reason: str) -> bool:
        goal = self._goals.pop(session_id, None)
        if goal is None:
            return False
        goal.state, goal.reason = "stopped", reason
        self._announce_goal(session_id, goal, "stopped")
        return True

    async def _attached_images(self, session_id: str, state: "_Session", prompt: list) -> dict | None:
        """Images attached to the prompt (a pasted screenshot, a dragged file in the app) as a vision turn — the same
        gate and the same detour as the menu bar's screenshot: the model named by ``capture.visionModel``, local unless
        ``capture.screenshotCloud``, the image stripped from the history when the turn ends. ``None`` (and the person
        told why) when there are none or they cannot be sent."""
        images, dropped = _prompt_images(prompt)
        for reason in dropped:
            await self._say(session_id, f"[Not attached: {reason}.]\n")
        if not images:
            return None
        preferences = settings_store.current(state.cwd)
        model = str(preferences.get(os_context.VISION_KEY) or "").strip()
        cloud_ok = bool(preferences.get("capture.screenshotCloud", False))
        allowed, why = os_context.screenshot_plan(True, cloud_ok, model)
        if not allowed:
            await self._say(session_id, f"[{(why or 'the image was not attached').replace('screenshot', 'image')}.]\n")
            return None
        return {"bytes": images[0]["bytes"], "format": images[0]["format"], "images": images, "model": model, "cloud_ok": cloud_ok, "app": "the attached image"}

    async def _say(self, session_id: str, text: str) -> None:
        if self._conn is not None:
            await self._conn.session_update(session_id=session_id, update=acp.update_agent_message_text(text))

    async def _goal_command(self, session_id: str, text: str) -> list | None:
        """``/goal …`` typed as a prompt, in any front door. Returns the prompt to run (the kickoff), or ``None`` when
        the command was fully answered here (status, clear, a mistake)."""
        arguments = text.strip()[len("/goal"):].strip()
        if arguments.lower() in ("clear", "stop", "off", "cancel"):
            await self._say(session_id, "Goal cleared." if self._clear_goal(session_id, "cleared by the person") else "There was no active goal.")
            return None
        parsed = goal_module.parse(arguments)
        if parsed is None:
            current = self._goals.get(session_id)
            await self._say(session_id, "No active goal.\n\n" + goal_module.USAGE if current is None else self._describe_goal(current))
            return None
        if isinstance(parsed, str):
            await self._say(session_id, parsed)
            return None
        self._goals[session_id] = parsed
        self._announce_goal(session_id, parsed, "set")
        return [acp.text_block(goal_module.kickoff(parsed))]

    @staticmethod
    def _describe_goal(goal: "goal_module.Goal") -> str:
        how = f"checked by `{goal.check}`" if goal.check else "judged from what each turn did"
        return f"Goal ({goal.state}): {goal.condition}\n{how} · follow-up turn {goal.turns}/{goal.max_turns}" + (f"\nLast: {goal.reason}" if goal.reason else "")

    async def _pursue_goal(self, session_id: str, response, **kwargs):
        """After a turn: while a goal is active and the turn ended normally, decide whether it is met and, if not, run
        the next turn. Everything the loop decides is announced; every turn is a full turn (approvals, receipts)."""
        while True:
            goal = self._goals.get(session_id)
            state = self._sessions.get(session_id)
            if goal is None or goal.state != "active" or state is None:
                return response
            stop = str(getattr(response, "stop_reason", None) or "end_turn")
            if stop != "end_turn":
                goal.state, goal.reason = "stopped", f"the turn ended: {stop}"
                self._announce_goal(session_id, goal, "stopped")
                return response
            self._announce_goal(session_id, goal, "checking")
            verdict = await run_blocking(
                lambda: goal_module.evaluate(goal, cwd=state.cwd, messages=list(state.agent.messages), ask=lambda prompt: session_extras.one_shot("You are a strict judge. Output only the requested JSON.", prompt))
            )
            goal.reason = verdict.reason
            if session_id in self._cancelled:
                # Stop was pressed while the judge was deciding: nothing was streaming to cancel, so it is honoured here.
                goal.state, goal.reason = "stopped", "cancelled by the person"
                self._announce_goal(session_id, goal, "stopped")
                return acp.PromptResponse(stopReason="cancelled")
            if verdict.met:
                goal.state = "met"
                self._announce_goal(session_id, goal, "met")
                return response
            goal.judge_failures = goal.judge_failures + 1 if verdict.failed else 0
            if goal.judge_failures >= goal_module.MAX_JUDGE_FAILURES:
                goal.state = "stopped"
                self._announce_goal(session_id, goal, "stopped")
                return response
            if goal.turns >= goal.max_turns:
                goal.state = "exhausted"
                self._announce_goal(session_id, goal, "exhausted")
                return response
            goal.turns += 1
            self._announce_goal(session_id, goal, "continue")
            try:
                response = await self._turn(session_id, [acp.text_block(goal_module.continuation(goal, verdict.reason))], **kwargs)
            except asyncio.CancelledError:
                self._stop_goal(session_id, "the turn was cancelled")
                raise

    # ------------------------------------------------------ interrupts

    async def _resolve_interrupts(self, session_id: str, interrupts: list, since: float | None = None) -> list:
        """Turn each unanswered Strands interrupt into a real ACP permission
        prompt, and build the resume payload strands.interrupt._InterruptState.resume
        expects: a list of {"interruptResponse": {"interruptId", "response"}}.

        Every prompt is staged in `state.db` before it is shown and closed out
        with what the user chose, so the prompt and its answer outlive the call
        that raised them — and a call reported twice (this method's caller
        learns about a turn's interrupts from two places) is answered from the
        record instead of being put to the user a second time. See
        `_settled_intervention` for what does and does not count as the same
        call.
        """
        responses = []
        state = self._sessions.get(session_id)
        for interrupt in interrupts:
            risk: dict = {}
            try:
                payload = json.loads(interrupt.reason)
                tool_name = payload.get("name", "tool")
                tool_input = payload.get("input")
                tool_use_id = payload.get("tool_use_id") or interrupt.id
                always_rule = payload.get("always_rule")
                why = payload.get("reason")
                risk = payload.get("risk") or {}
                pod_name = payload.get("pod") or "orchestrator"
            except (TypeError, ValueError):
                pod_name = "orchestrator"
                tool_name, tool_input, tool_use_id, always_rule, why = str(interrupt.reason), None, interrupt.id, None, None

            settled = _settled_intervention(interrupt.id, session_id, tool_name, tool_input, since)
            if settled is not None:
                approved = settled["status"] == "approved"
                state_store.append_event(
                    "intervention",
                    {
                        "phase": "replayed",
                        "interrupt_id": interrupt.id,
                        "tool": tool_name,
                        "outcome": settled["status"],
                        "decided_by": settled.get("decided_by"),
                        "rule": settled.get("rule"),
                    },
                    session_id=session_id,
                    source="approval",
                )
                if not approved and state is not None:
                    state.router_hooks.note_rejection(state.agent)
                _note_answer(session_id, tool_name, approved, interrupt.id)
                responses.append({"interruptResponse": {"interruptId": interrupt.id, "response": approved}})
                continue

            lab_module.get().stage(interrupt.id, pod_name, tool_name, tool_input, risk or None, why)
            state_store.stage_intervention(
                session_id=session_id,
                tool=tool_name,
                tool_input=tool_input,
                intervention_id=interrupt.id,
                risk_score=risk.get("score"),
                # The band, not the ordinal: safe/elevated/destructive is in the
                # same units as the settings thresholds, so a row answers "did
                # this cross the destructive line" without re-deriving it. The
                # full reading — ordinal and confidence included — is in the
                # event.
                risk_label=risk.get("band"),
                note=why,
            )
            state_store.append_event(
                "intervention",
                {
                    "phase": "requested",
                    "interrupt_id": interrupt.id,
                    "tool": tool_name,
                    "input": tool_input,
                    "risk": risk or None,
                    "note": why,
                    "always_rule": always_rule,
                },
                session_id=session_id,
                source="approval",
            )

            approved = False
            option_id = None
            note = ""
            plan_tool = tool_name == plan_mode.TOOL_NAME
            ask_tool = tool_name == ask_user.TOOL_NAME
            answered_by = "user" if self._conn is not None else "no-client"
            if self._conn is not None:
                if plan_tool:
                    options = plan_mode.options()
                elif ask_tool:
                    options = ask_user.options(tool_input)
                else:
                    options = [acp.schema.PermissionOption(option_id="allow", name="Allow", kind="allow_once")]
                    if always_rule:
                        options.append(
                            acp.schema.PermissionOption(option_id="allow_always", name=f"Always allow {always_rule}", kind="allow_always")
                        )
                    options.append(acp.schema.PermissionOption(option_id="reject", name="Reject", kind="reject_once"))
                result = await self._conn.request_permission(
                    session_id=session_id,
                    tool_call=acp.schema.ToolCallUpdate(
                        tool_call_id=tool_use_id,
                        title=f"Run {tool_name}" + (f" — {why}" if why else ""),
                        kind=_tool_kind(tool_name),
                        raw_input=tool_input,
                    ),
                    options=options,
                    # ACP's `_meta` slot: the exact command or diff, secret-masked, plus the pod
                    # and Laya's reading — what the daemon's gRPC bridge hands the GUI's containment
                    # modal. A client that does not know it (Zed, acp-tui) ignores it.
                    cordata=intervention_view.build(
                        tool_name,
                        tool_input,
                        state.cwd if state else os.getcwd(),
                        interrupt_id=interrupt.id,
                        pod=pod_name,
                        risk=risk or None,
                        reason=why,
                        armed=bool(why),
                        project=state.cwd if state else os.getcwd(),
                        **dict(zip(("goal", "said"), _turn_context(state.agent) if state else ("", ""))),
                    ),
                )
                approved = _permission_approved(result.outcome, options)
                option_id = getattr(result.outcome, "option_id", None)
                # Which front door answered. Under the daemon the answer may have come from the
                # Discord bot rather than a client on the socket, and the daemon relays that in
                # ACP's `_meta` slot (supervisor.answer_permission) — the same request the GUI's
                # containment modal would have answered, told apart only here. A client that
                # sends no `_meta` is a person at a TUI or the GUI, which is what "user" has
                # always meant; anything else keeps the door's own name so the ledger can
                # account for who decided what.
                answered_by = _answered_by(result) or answered_by
                note = _answer_note(result)
                if plan_tool:
                    self._settle_plan(session_id, state, option_id, note)
                elif ask_tool:
                    if state is not None:
                        ask_user.record(state.agent, tool_input, option_id, note)
                    # An answered question is not a refusal on the ledger, whatever kind the option carried.
                    approved = option_id is not None and option_id != ask_user.OPTION_DECLINE
                elif approved and always_rule and option_id == "allow_always":
                    self._remember_always(session_id, always_rule)
            if not approved and state is not None and not plan_tool and not ask_tool:  # feedback on a plan, or "decide for me", is not a lesson to remember
                state.router_hooks.note_rejection(state.agent)
                # A person said no: keep the change and their reason, and let the ReflectionAgent turn it into a rule
                # later turns are held to (rejection_memory.py). A fail-closed "no client" rejection teaches nothing.
                rejection_memory.capture(
                    state.cwd,
                    session_id=session_id,
                    tool_name=tool_name,
                    tool_input=tool_input,
                    decided_by=answered_by,
                    note=note,
                    goal=_turn_context(state.agent)[0],
                    preferences=settings_store.current(state.cwd),
                )
            _note_answer(session_id, tool_name, approved, interrupt.id)

            rule = always_rule if (approved and option_id == "allow_always") else None
            lab_module.get().resolve(interrupt.id, "approved" if approved else "rejected", answered_by)
            state_store.resolve_intervention(
                interrupt.id,
                "approved" if approved else "rejected",
                # Honest, and it matters: with no client attached nothing was
                # asked, and a rejection that reads as a user decision would
                # misattribute a fail-closed default to a person. `answered_by`
                # is that name, or the front door that relayed the answer.
                decided_by=answered_by,
                rule=rule,
            )
            state_store.append_event(
                "intervention",
                {
                    "phase": "resolved",
                    "interrupt_id": interrupt.id,
                    "tool": tool_name,
                    "outcome": "approved" if approved else "rejected",
                    "decided_by": answered_by,
                    "rule": rule,
                    "always": option_id == "allow_always",
                },
                session_id=session_id,
                source="approval",
            )

            # The plan tool always runs once answered: it reports the answer (approved, or what to change) to the
            # model as its result, which reads better than the generic confirmation failure a refusal would give.
            responses.append({"interruptResponse": {"interruptId": interrupt.id, "response": approved or ((plan_tool or ask_tool) and self._conn is not None)}})
        return responses

    def _adapt_mode(self, session_id: str, state, text: str, decision) -> None:
        """Adaptive mode (mode_advisor.py): a request that wants a plan first starts the turn in plan mode. Only ever tightens —
        leaving plan is the person's answer to the plan card — and stays out of the way after a mode the person picked."""
        if state.adaptive_hold > 0:
            state.adaptive_hold -= 1
            return
        if settings_store.current(state.cwd).get("modes.adaptive", True) is False:
            return
        route = decision.target.id if decision is not None and getattr(decision, "target", None) else None
        advice = mode_advisor.advise(text, mode=state.handler.mode, route=route)
        if not mode_advisor.should_apply(advice, mode=state.handler.mode, held=state.adaptive_hold):
            return
        previous = state.handler.mode
        state.handler.mode = "plan"
        self._announce_mode(session_id, "plan", previous, "adaptive", advice.reason)

    def _settle_plan(self, session_id: str, state, option_id: str | None, note: str) -> None:
        """The person answered the plan card: record it for the tool, and — if they approved — move the session to the mode
        they chose. This is the only way out of plan mode that is not the person's own mode switch."""
        if state is None:
            return
        mode = plan_mode.record(state.agent, option_id, note)
        if mode is None:
            return
        previous = state.handler.mode
        state.handler.mode = mode
        state.adaptive_hold = mode_advisor.HOLD_TURNS
        self._announce_mode(session_id, mode, previous, "plan_approved", "plan approved")

    def _announce_mode(self, session_id: str, mode: str, previous: str, kind: str, reason: str) -> None:
        """Tell the client the session's mode changed under it: ACP's own update (a client that only knows ACP redraws its
        mode picker) and a `cordata/mode` extension with why (the TUI shows it as a line and flashes the mode pill)."""
        loop, conn = self._loop, self._conn
        if conn is not None and loop is not None and not loop.is_closed():
            async def push() -> None:
                try:
                    await conn.session_update(session_id=session_id, update=acp.schema.CurrentModeUpdate(session_update="current_mode_update", current_mode_id=mode))
                except Exception:
                    logging.getLogger(__name__).debug("current_mode_update not delivered", exc_info=True)

            loop.call_soon_threadsafe(lambda: loop.create_task(push()))
        self._send_ext("cordata/mode", {"sessionId": session_id, "mode": mode, "from": previous, "kind": kind, "reason": reason})
        lab = lab_module.get()
        if lab.session_id == session_id:
            lab.emit(force=True)

    def _remember_always(self, session_id: str, rule: str) -> None:
        """"Always allow": save the rule for this project (settings.local.json,
        this machine only). Choosing it is an explicit decision about the folder,
        so it is also marked trusted — otherwise its own allow rule would be ignored."""
        state = self._sessions.get(session_id)
        cwd = state.cwd if state else os.getcwd()
        try:
            settings_store.add_rule("allow", rule, "local", cwd)
            settings_store.trust_project(cwd)
        except (OSError, ValueError) as error:
            logging.getLogger(__name__).warning("could not save always-allow rule %r: %s", rule, error)

    async def _send_tool_result(self, session_id: str, result: dict, traced: dict | None = None) -> None:
        """One finished tool call as a terminal ACP tool-call update.

        ``traced`` is the in-flight turn's trace collector, when there is one.
        The result is where a call's outcome — and the error text that explains
        it — actually lives, and this is the only place that sees it."""
        tool_use_id = result.get("toolUseId")
        if not tool_use_id or self._conn is None:
            return
        status = "completed" if result.get("status") == "success" else "failed"
        if traced is not None:
            entry = traced.setdefault(tool_use_id, {})
            entry["status"] = status
            text = " ".join(
                block["text"] for block in result.get("content") or []
                if isinstance(block, dict) and block.get("text")
            )
            if status == "failed":
                entry["error"] = text or "tool reported failure"
            elif text:
                entry["result"] = text
        content = [
            acp.tool_content(acp.text_block(block["text"]))
            for block in result.get("content") or []
            if isinstance(block, dict) and block.get("text")
        ]
        await self._conn.session_update(
            session_id=session_id,
            update=acp.update_tool_call(tool_use_id, status=status, content=content or None),
        )

    # ---------------------------------------------------------- turns

    async def _stream(self, session_id: str, agent, agent_input, *, checkpoints: bool) -> tuple[str, object]:
        """`_stream_turn`, with the running agent registered so a cancel can reach it (`cancel`)."""
        agent.cancel_signal.clear()  # idle until now: whatever is set is stale, and would cancel this turn before it began
        self._streaming[session_id] = agent
        try:
            return await self._stream_turn(session_id, agent, agent_input, checkpoints=checkpoints)
        finally:
            if self._streaming.get(session_id) is agent:
                del self._streaming[session_id]

    def _turn_budget(self, session_id: str) -> tuple[int, int]:
        """`(steps, tokens)` one turn may spend — `context.maxSteps` / `context.maxTurnUsage`, 0 meaning no cap."""
        state = self._sessions.get(session_id)
        preferences = settings_store.current(state.cwd) if state is not None else {}
        values = []
        for key, default in (("context.maxSteps", DEFAULT_MAX_STEPS), ("context.maxTurnUsage", 0)):
            try:
                values.append(max(0, int(float(preferences.get(key, default)))))
            except (TypeError, ValueError):
                values.append(default)
        return values[0], values[1]

    async def _stream_turn(self, session_id: str, agent, agent_input, *, checkpoints: bool) -> tuple[str, object]:
        """Run one turn of ``agent`` to its end, translating Strands events into
        ACP updates. Loops through approval interrupts (each becomes a real ACP
        permission prompt) and, with ``checkpoints``, through Strands checkpoint
        pauses — each one is written beside the session before resuming, so a
        crash mid-turn leaves a resumable point. Returns (stop reason, result)."""
        seen_tool_calls: set[str] = set()
        last_progress: dict[str, str] = {}
        specialist_progress: dict[str, _SpecialistProgress] = {}
        # tool_use_id -> what it was called with, what it returned. Fed into
        # trace_store at the end of the turn; empty (and never written) when
        # memory.traces is off, so the collector costs one dict when unused.
        traced: dict[str, dict] = {}
        last_result = None
        # What the user asked, before an approval resume replaces `agent_input`
        # with the interrupt responses — otherwise the turns that hit a prompt,
        # the ones most worth tracing, would be recorded with no prompt at all.
        prompt = agent_input if isinstance(agent_input, str) else None
        # Bounds what `_settled_intervention` may replay: a decision from before
        # this turn began is not this turn's to reuse.
        turn_started = time.time()
        # What this turn may spend (`context.maxSteps`, `context.maxTurnUsage`). Strands checks `limits` at the top of every loop
        # cycle and answers with a `limit_*` stop reason and a history that is still valid — but a limit is *per invocation*,
        # and a turn is several: each approval, and (with `checkpoints`) every cycle, resumes a fresh one whose counters start
        # at zero. So an invocation that runs the whole loop (a specialist's) is handed what is left of the turn's budget as
        # `limits`; a checkpointed turn is checked here, at the `after_tools` pause — the one point where every tool call has
        # its result — against the cycle the checkpoint itself reports.
        metrics = agent.event_loop_metrics
        start_cycles = metrics.cycle_count
        start_tokens = int(metrics.accumulated_usage.get("totalTokens", 0) or 0)

        web = getattr(agent, "web_usage", None)
        start_web = web.total if web is not None else 0

        def used_tokens() -> int:
            return int(metrics.accumulated_usage.get("totalTokens", 0) or 0) - start_tokens + ((web.total - start_web) if web is not None else 0)

        max_steps, max_tokens = self._turn_budget(session_id)
        cancelled_at: float | None = None
        while True:
            stop_reason = "end_turn"
            pending_interrupts: list = []
            checkpoint = None
            limits: dict[str, int] = {}
            if not checkpoints:
                if max_steps:
                    limits["turns"] = max(1, max_steps - (metrics.cycle_count - start_cycles))
                if max_tokens:
                    limits["total_tokens"] = max(1, max_tokens - used_tokens())

            async for event in agent.stream_async(agent_input, limits=limits or None):
                if session_id in self._cancelled:
                    # Strands was told to stop (`cancel`) and does at its next checkpoint, sending a `cancelled` result of
                    # its own — which lets a tool that polls the signal wind down first. A moment is allowed for that; a
                    # tool that never looks is not waited for.
                    cancelled_at = cancelled_at or time.monotonic()
                    if time.monotonic() - cancelled_at > CANCEL_GRACE_SECONDS:
                        _settle_cancelled_turn(agent)
                        return "cancelled", last_result
                if "tool_interrupt_event" in event:
                    interrupts = event["tool_interrupt_event"].get("interrupts") or []
                    if interrupts:
                        stop_reason = "interrupt"
                        pending_interrupts = list(interrupts)
                    continue

                if "result" in event:
                    last_result = event["result"]
                    reason = last_result.stop_reason
                    if reason == "interrupt":
                        # Belt-and-suspenders with the tool_interrupt_event
                        # branch above: AgentResult.interrupts carries the
                        # same list, so this still works even if that event
                        # was ever skipped for some interrupt path.
                        stop_reason = "interrupt"
                        pending_interrupts = pending_interrupts or list(last_result.interrupts or [])
                    elif reason == "checkpoint":
                        stop_reason, checkpoint = "checkpoint", last_result.checkpoint
                    else:
                        stop_reason = reason if reason in ("end_turn", "max_tokens", "cancelled", "max_turn_requests") or str(reason).startswith("limit_") else "end_turn"
                    continue
                if self._conn is None:
                    continue

                event_type = event.get("type")
                if event_type == "tool_use_stream":
                    current = event.get("current_tool_use") or {}
                    tool_use_id = current.get("toolUseId")
                    if tool_use_id and tool_use_id not in seen_tool_calls:
                        seen_tool_calls.add(tool_use_id)
                        tool_name = current.get("name") or "tool"
                        traced.setdefault(tool_use_id, {}).update(name=tool_name, input=current.get("input"))
                        await self._conn.session_update(
                            session_id=session_id,
                            update=acp.start_tool_call(
                                tool_use_id,
                                tool_name,
                                kind=_tool_kind(tool_name),
                                status="pending",
                                raw_input=current.get("input"),
                            ),
                        )
                    continue

                if event_type == "tool_stream":
                    stream_event = event.get("tool_stream_event") or {}
                    tool_use = stream_event.get("tool_use") or {}
                    tool_use_id = tool_use.get("toolUseId")
                    data = stream_event.get("data") or {}
                    rendered = None
                    if tool_use_id and isinstance(data, dict) and isinstance(data.get("progress"), str):
                        rendered = data["progress"]  # a tool that narrates its own progress (claude_code_engineer, orchestrate)
                    elif tool_use_id and isinstance(data, dict) and tool_use.get("name") in self._specialist_names(session_id):
                        progress = specialist_progress.setdefault(tool_use_id, _SpecialistProgress())
                        if progress.feed(data):
                            rendered = progress.render(tool_use.get("name"))
                    # Progress is re-yielded whole on every update; a client only
                    # needs to hear when the rendered text actually changed.
                    if rendered and last_progress.get(tool_use_id) != rendered:
                        last_progress[tool_use_id] = rendered
                        await self._conn.session_update(
                            session_id=session_id,
                            update=acp.update_tool_call(
                                tool_use_id,
                                status="in_progress",
                                content=[acp.tool_content(acp.text_block(rendered))],
                            ),
                        )
                    continue

                if event_type == "tool_result":
                    await self._send_tool_result(session_id, event.get("tool_result") or {}, traced)
                    continue

                # Strands >= 1.5x no longer yields a "tool_result" event from
                # stream_async (confirmed against the installed 1.55.1 and 1.57.2: the
                # only events are tool_use_stream, "message", and "result") —
                # the finished tool's result arrives inside the user-role
                # "message" event that carries it into the next model call.
                # Without this branch every ToolCallStart stayed "pending"
                # forever in the client. Both branches stay: an older Strands
                # that emits both just sends the same terminal update twice.
                if "message" in event:
                    message = event["message"]
                    if isinstance(message, dict) and message.get("role") == "user":
                        for block in message.get("content") or []:
                            tool_result = block.get("toolResult") if isinstance(block, dict) else None
                            if tool_result:
                                await self._send_tool_result(session_id, tool_result, traced)
                    elif isinstance(message, dict) and message.get("role") == "assistant":
                        # The model finished asking for its tools; none has run yet.
                        # Send each call's complete input (the start event only had
                        # what had streamed so far) and, for edits, the change as
                        # ACP diff content read from disk *before* it happens.
                        for block in message.get("content") or []:
                            tool_use = block.get("toolUse") if isinstance(block, dict) else None
                            if tool_use and tool_use.get("toolUseId"):
                                await self._send_tool_preview(session_id, tool_use)
                    continue

                if "data" in event:
                    streaming_pod = lab_module.get().pod_of(agent)
                    if streaming_pod is not None:
                        lab_module.get().stream_text(streaming_pod.name, event["data"])
                    await self._conn.session_update(
                        session_id=session_id,
                        update=acp.update_agent_message_text(event["data"]),
                    )
                elif "reasoningText" in event:
                    await self._conn.session_update(
                        session_id=session_id,
                        update=acp.update_agent_thought_text(event["reasoningText"]),
                    )

            if stop_reason == "cancelled" or (session_id in self._cancelled and stop_reason != "interrupt"):
                _settle_cancelled_turn(agent)
                return "cancelled", last_result
            if stop_reason == "checkpoint" and checkpoint is not None and checkpoint.position == "after_tools":
                if max_steps and checkpoint.cycle_index + 1 >= max_steps:
                    stop_reason = "limit_turns"
                elif max_tokens and used_tokens() >= max_tokens:
                    stop_reason = "limit_total_tokens"
            if stop_reason.startswith("limit_"):
                await self._say_limit(session_id, stop_reason, max_steps, max_tokens)
                self._ingest_turn(session_id, prompt, traced)
                return "max_turn_requests", last_result
            if stop_reason == "checkpoint" and checkpoint is not None:
                payload = {"checkpoint": checkpoint.to_dict(), "ts": time.time()}
                if checkpoints:
                    try:
                        _checkpoint_path(session_id).write_text(json.dumps(payload), encoding="utf-8")
                    except OSError:
                        pass
                agent_input = {"checkpointResume": {"checkpoint": checkpoint.to_dict()}}
                continue
            if stop_reason != "interrupt":
                self._ingest_turn(session_id, prompt, traced)
                return stop_reason, last_result
            agent_input = await self._resolve_interrupts(session_id, pending_interrupts, turn_started)

    async def _say_limit(self, session_id: str, reason: str, max_steps: int, max_tokens: int) -> None:
        """Tell the person *why* the turn stopped, and that nothing was lost — a silent stop reads as a hang."""
        if self._conn is None:
            return
        what = f"{max_steps} steps" if reason == "limit_turns" else f"{max_tokens:,} tokens"
        key = "context.maxSteps" if reason == "limit_turns" else "context.maxTurnUsage"
        await self._conn.session_update(
            session_id=session_id,
            update=acp.update_agent_message_text(
                f"\n\n— Paused after {what} on this turn (`{key}`). Nothing is lost: say \"continue\" and I carry on from here, or raise the limit."
            ),
        )

    def _ingest_turn(self, session_id: str, prompt: str | None, traced: dict) -> None:
        """Feed the finished turn to the local stores, if either is switched on.

        Called only on the terminal exit from `_stream` — not on an interrupt,
        which resumes the same turn on the next loop pass and would otherwise be
        ingested once per approval prompt. Fire-and-forget in a thread: the
        embedding is the slow part of a trace, a graph write is a LanceDB merge,
        and the reply is already on its way to the client."""
        state = self._sessions.get(session_id)
        preferences = getattr(state, "preferences", None)
        tracing = bool(traced) and trace_store.enabled(preferences)
        graphing = kg.enabled(preferences)
        if not tracing and not graphing:
            return
        # `entry` already carries exactly name/input/status/error/result, which
        # is what record_turn reads; pass a copy so the collector is not shared
        # with the ingest thread.
        calls = [dict(entry) for entry in traced.values()]
        cwd = getattr(state, "cwd", None)
        log = logging.getLogger(__name__)

        def ingest() -> None:
            if tracing:
                try:
                    trace_store.record_turn(session_id=session_id, prompt=prompt, tool_calls=calls, preferences=preferences)
                except Exception:  # never let tracing break a turn
                    log.debug("trace ingest failed", exc_info=True)
            if graphing:
                # `kg.ingest_turn` swallows its own failures; this is belt and
                # braces so one store's bug cannot cost the other's rows.
                try:
                    kg.ingest_turn(session_id=session_id, cwd=cwd, tool_calls=calls, preferences=preferences)
                except Exception:
                    log.debug("kg ingest failed", exc_info=True)

        threading.Thread(target=ingest, name="trace-ingest", daemon=True).start()

    async def _send_tool_preview(self, session_id: str, tool_use: dict) -> None:
        if self._conn is None:
            return
        name = tool_use.get("name") or ""
        tool_input = tool_use.get("input")
        diffs = edit_preview.preview(name, tool_input) if name in ("editor", "file_write", "apply_patch") else []
        await self._conn.session_update(
            session_id=session_id,
            update=acp.update_tool_call(
                tool_use["toolUseId"],
                raw_input=tool_input,
                content=[acp.tool_diff_content(path, new_text, old_text) for path, old_text, new_text in diffs] or None,
            ),
        )

    def _specialist_names(self, session_id: str) -> set[str]:
        state = self._sessions.get(session_id)
        return set(state.specialists) if state else set()

    async def _decide(self, state: _Session, text: str) -> RouteDecision | None:
        if app_settings.LAYA_TRIAGE_MODE != "route" or not text:
            return None
        from prompt_security import laya_ready

        router = _router()
        router.policy = _router_policy(settings_store.current(state.cwd))
        targets = [*BASE_TARGETS, *plugin_targets(state.plugins.descriptions() if state.plugins else [])]
        timeout = _LAYA_DECIDE_TIMEOUT if laya_ready() else app_settings.LAYA_TRIAGE_TIMEOUT
        try:
            return await asyncio.wait_for(asyncio.to_thread(router.decide, text, targets), timeout)
        except asyncio.TimeoutError:
            decision = RouteDecision(target=None, error=f"Laya not ready within {timeout:g}s")
            return router._abstain(decision, "no decision: Laya still loading")

    def _apply_bandit(self, state: _Session, decision: RouteDecision, text: str) -> dict:
        """Let the contextual bandit pick among the actions the base policy *already allowed* this turn.

        Runs between ``router.decide`` and ``recorder.set_route``, so what the ledger records — and what the turn is
        later learned from — is the choice actually taken. Additive-only by construction: the arms are the base
        decision's own action and a milder one the policy already permits (`_bandit_route_arms`), never a new
        action, and an abstained decision is left alone — fail-closed is not this module's to reopen. Returns the
        extra route keys (``choices`` / ``bandit_mode``) for the caller's ``set_route``.
        """
        handler = state.handler
        handler._bandit_decision = decision if not (decision is None or decision.abstained) else None
        handler._bandit_route = None
        if decision is None or decision.abstained:
            return {}
        policy = _bandit_policy(settings_store.current(state.cwd))
        if policy["mode"] not in ("shadow", "live"):
            return {}
        recorder = outcomes.active(state.handler.session_id)
        turn = recorder.id if recorder is not None else ""
        stub = _bandit_stub(state.handler.mode, decision)
        handler._bandit_route = stub
        table = bandit.for_project(state.cwd)
        knobs = {"mode": policy["mode"], "explore": policy["explore"], "min_pulls": policy["min_pulls"], "lam": policy["lam"], "mu": policy["mu"]}
        choices: dict = {}
        # engine: the heavy Claude Code path against the orchestrator's own tools. Offered only for a
        # kind="tool" target, whose two actions the policy already weighs by Laya's complexity reading.
        if decision.target is not None and decision.target.kind == "tool":
            arm, info = table.choose("engine", bandit.context(stub, "engine"), ["tool", "none"], decision.action, **knobs)
            bandit.decision_event(info, turn=turn, project=state.cwd)
            choices["engine"] = arm
            if info["acted"]:
                decision.reason = f"{decision.reason} · bandit chose {arm} over {decision.action}"
                decision.action, decision.suppress_hint = arm, arm == "none"
        # route: dispatch vs. hint vs. none, among the ones the base policy allowed.
        arms = _bandit_route_arms(decision)
        if arms:
            arm, info = table.choose("route", bandit.context(stub, "route"), arms, decision.action, **knobs)
            bandit.decision_event(info, turn=turn, project=state.cwd)
            choices["route"] = arm
            if info["acted"]:
                decision.reason = f"{decision.reason} · bandit chose {arm} over {decision.action}"
                decision.action = arm
                if arm == "none":
                    decision.suppress_hint = True
                elif arm == "hint":
                    decision.suppress_hint = False
        decision.choices.update(choices)
        # By reference on purpose: the ``ask`` arm is decided later, on a tool call, and lands in this very dict.
        return {"choices": decision.choices, "bandit_mode": policy["mode"]}

    async def _dispatch(self, session_id: str, state: _Session, decision: RouteDecision, text: str) -> str:
        """Run a confidently-routed specialist directly — the orchestrator's
        model call that would only have decided to delegate is skipped. The
        exchange is then written into the orchestrator's history (and session
        file) so the conversation continues normally next turn."""
        name = decision.target.id
        specialist = state.specialists[name]
        specialist.messages.clear()  # specialists start fresh; context is passed explicitly
        context = _recent_context(state.agent.messages)
        prompt = text if not context else f"{text}\n\nConversation so far (for context):\n{context}"
        state.router_hooks.prime(specialist, decision)
        tool_use_id = f"dispatch-{uuid.uuid4().hex[:8]}"
        if self._conn is not None:
            await self._conn.session_update(
                session_id=session_id,
                update=acp.start_tool_call(tool_use_id, name, kind=_tool_kind(name), status="in_progress", raw_input={"input": text, "routed_by": "laya"}),
            )
        lab_module.get().link("orchestrator", name, "dispatch", label="laya")
        try:
            stop_reason, result = await self._stream(session_id, specialist, prompt, checkpoints=False)
        finally:
            lab_module.get().link("orchestrator", name, "dispatch", active=False, label="laya")
        reply = str(result).strip() if result is not None else ""
        if self._conn is not None:
            await self._conn.session_update(
                session_id=session_id,
                update=acp.update_tool_call(tool_use_id, status="completed" if stop_reason == "end_turn" else "failed", content=None),
            )
        if stop_reason == "end_turn":
            orchestrator = state.agent
            for message in (
                {"role": "user", "content": [{"text": text}]},
                {"role": "assistant", "content": [{"text": f"[{name} specialist, routed by Laya]\n{reply}"}]},
            ):
                orchestrator.messages.append(message)
                manager = getattr(orchestrator, "_session_manager", None)
                if manager is not None:
                    try:
                        session_store.record(manager, message, orchestrator)
                    except Exception:
                        logging.getLogger(__name__).debug("could not persist dispatched turn", exc_info=True)
        return stop_reason

    @staticmethod
    def _tokens_used(state: "_Session") -> tuple[int, int]:
        """Cumulative (input, output) tokens across the orchestrator and every specialist."""
        agents = [state.agent, *[getattr(s, "agent", s) for s in (state.specialists or {}).values()]]
        totals = [outcome_hooks.usage_of(agent) for agent in agents]
        # …and the `web_fetch` analyst, which runs as an agent of its own and so is in nobody's metrics (core_tools.UsageTally).
        web = getattr(state.agent, "web_usage", None)
        extra = (web.input, web.output) if web is not None else (0, 0)
        return sum(t[0] for t in totals) + extra[0], sum(t[1] for t in totals) + extra[1]

    def _learn_from(self, record: "outcomes.TurnRecord", state: "_Session") -> None:
        """After a turn: judge the learned skills and prompt overlays it had in front of it, and — when it cost
        something — draft a lesson from it, and rewrite a live artifact whose turns have gone badly. All of it is
        off the turn's path (a background thread) and none of it can fail the turn."""
        data = record.to_json()
        preferences = settings_store.current(state.cwd)

        def run() -> None:
            try:
                reward = outcomes.score(record).value
                baseline = _baseline_reward(state.cwd, exclude=data["id"])
                if data.get("skills"):
                    learned.record_use(state.cwd, data["skills"], reward, data["id"], baseline=baseline)
                overlay_names = prompts.live_names(state.cwd)
                if overlay_names:
                    prompts.record_use(state.cwd, overlay_names, reward, data["id"], baseline=baseline)
                reflect.reflect(data, state.cwd, session_id=data.get("session_id"), preferences=preferences)
                _optimize_artifacts(state.cwd, preferences)
                # The turn is judged now; if a reconcile landed it has become learnable, fold it in.
                _bandit_learn(state.cwd)
            except Exception:
                logging.getLogger(__name__).debug("learning from a turn failed", exc_info=True)

        threading.Thread(target=run, name="learn-from-turn", daemon=True).start()

    async def prompt(self, session_id: str, prompt: list, **kwargs) -> acp.PromptResponse:
        """The client's prompt: one turn — or, under ``/goal``, as many as it takes (``goal.py``).

        One at a time per session. A second prompt while one is running is refused here, before it touches anything:
        the running turn's recorder, checkpoint, cancel flag and stream handle all belong to it."""
        if session_id in self._busy:
            raise acp.RequestError(
                -32000,
                "A turn is already running in this session. Stop it first, or send this after it finishes.",
                {"error": "busy", "sessionId": session_id},
            )
        self._busy.add(session_id)
        try:
            # A cancel that arrived while nothing ran is spent here, once, rather than at the start of every turn: a goal's
            # follow-up turn must not wipe out a Stop pressed while its judge was deciding.
            self._cancelled.discard(session_id)
            text = _extract_prompt_text(prompt)
            if goal_module.is_command(text):
                kickoff = await self._goal_command(session_id, text)
                if kickoff is None:
                    return acp.PromptResponse(stopReason="end_turn")
                prompt = kickoff
            try:
                response = await self._turn(session_id, prompt, **kwargs)
            except asyncio.CancelledError:
                self._stop_goal(session_id, "the turn was cancelled")
                raise
            return await self._pursue_goal(session_id, response, **kwargs)
        finally:
            self._busy.discard(session_id)

    async def _turn(self, session_id: str, prompt: list, **kwargs) -> acp.PromptResponse:
        """One turn, recorded: the person's prompt to the stop reason becomes one outcome record
        (``outcomes.py``) — what was asked, what ran, how the approvals went, whether the checks passed,
        what changed. Recording is a side channel: nothing in it can fail or alter the turn."""
        state = self._sessions.get(session_id)
        if state is None:
            state = self._build_session(session_id, os.getcwd())
        text = _extract_prompt_text(prompt)
        recorder = None
        started = time.monotonic()
        tokens_before = self._tokens_used(state)
        if text.strip() and not _is_resume(text):
            try:
                recorder = outcomes.begin(session_id, goal=text, mode=state.handler.mode, project=state.cwd)
                recorder.heal_baseline(len(state.heal.episodes))
                # Judge what the person did with earlier turns' changes — off the turn's path.
                threading.Thread(target=_maintain, args=(state.cwd,), name="outcomes-reconcile", daemon=True).start()
            except Exception:
                logging.getLogger(__name__).debug("outcomes: could not begin a turn", exc_info=True)
        stop = "error"
        try:
            response = await self._prompt(session_id, prompt, **kwargs)
            stop = str(getattr(response, "stop_reason", None) or "end_turn")
            try:
                after = self._tokens_used(state)
                spent_in, spent_out = max(0, after[0] - tokens_before[0]), max(0, after[1] - tokens_before[1])
                response.usage = acp.schema.Usage(input_tokens=spent_in, output_tokens=spent_out, total_tokens=spent_in + spent_out)
            except Exception:
                logging.getLogger(__name__).debug("turn usage not attached", exc_info=True)
            return response
        except asyncio.CancelledError:
            stop = "cancelled"
            raise
        finally:
            self._announce_turn(session_id, state, started, tokens_before, stop)
            # One turn only: a screen the person has left must not follow them into the next one.
            os_context.clear(session_id)
            if recorder is not None:
                try:
                    if recorder.checkpointed:
                        # The project as the turn left it: the other end of "what did this turn change?",
                        # and the baseline against which a later edit by the person is recognised.
                        recorder.checkpoint_end(checkpoints.take(state.cwd, kind="end", session_id=session_id, turn=recorder.id).id)
                        if recorder.first_checkpoint:
                            self._announce_diff(session_id, state.cwd, recorder.id, recorder.first_checkpoint)
                    after = self._tokens_used(state)
                    recorder.usage(max(0, after[0] - tokens_before[0]), max(0, after[1] - tokens_before[1]))
                    finished = outcomes.finish(session_id, stop, heal_episodes=list(state.heal.episodes.values()))
                    if finished is not None:
                        self._learn_from(finished, state)
                except Exception:
                    logging.getLogger(__name__).debug("outcomes: could not finish a turn", exc_info=True)

    #: Characters of one turn's diff sent as ``_cordata/diff``; the rest is in the checkpoint store (``checkpoints/*``).
    DIFF_EVENT_CHARS = 400_000

    def _announce_diff(self, session_id: str, cwd: str, turn: str, before: str) -> None:
        """What the turn changed, as ``_cordata/diff``: the daemon keeps the latest few (``Supervisor._tap_diff``) and serves
        them to ``GetDiff`` and the Lab's diff review, which until now only a test fixture ever fed. Computed off the event
        loop; masked here like every other diff body that leaves this process; never raises."""

        def run() -> None:
            try:
                patch, stat = checkpoints.diff_since(cwd, before, max_chars=self.DIFF_EVENT_CHARS)
                if not patch.strip():
                    return
                truncated = patch.rstrip().endswith("(diff truncated)")
                patch = secret_guard.redact(secret_patterns.scrub(patch)[0])
                files = list(dict.fromkeys(line[6:] for line in patch.splitlines() if line.startswith("+++ b/")))
                files += [line[6:] for line in patch.splitlines() if line.startswith("--- a/") and line[6:] not in files]
                self._send_ext(
                    "cordata/diff",
                    {"sessionId": session_id, "id": f"{session_id}/{turn}", "source": "turn", "turn": turn, "files": files, "unified": patch, "stat": stat, "truncated": truncated, "lines": patch.count("\n")},
                )
            except Exception:
                logging.getLogger(__name__).debug("turn diff announcement failed", exc_info=True)

        threading.Thread(target=run, name="turn-diff", daemon=True).start()

    def _announce_turn(self, session_id: str, state: "_Session", started: float, tokens_before: tuple[int, int], stop: str) -> None:
        """What the turn cost, as ``_cordata/turn``: a front door that cannot see tokens (the Discord
        bot, the TUI's status line) shows it without a model call of its own. Never raises."""
        try:
            after = self._tokens_used(state)
            recorder = outcomes.active(session_id)
            self._send_ext(
                "cordata/turn",
                {
                    "sessionId": session_id,
                    "stop": stop,
                    "seconds": round(time.monotonic() - started, 1),
                    "tokensIn": max(0, after[0] - tokens_before[0]),
                    "tokensOut": max(0, after[1] - tokens_before[1]),
                    "calls": recorder.call_count if recorder is not None else 0,
                },
            )
            self._send_ext("cordata/context", {"sessionId": session_id, **session_extras.context_snapshot(state.agent)})
            self._usage_update(session_id, state)
        except Exception:
            logging.getLogger(__name__).debug("turn announcement failed", exc_info=True)

    async def _prompt(self, session_id: str, prompt: list, **kwargs) -> acp.PromptResponse:
        state = self._sessions.get(session_id)
        if state is None:
            # Defensive only — the ACP protocol always calls new_session or
            # load_session (both of which carry the real cwd) before prompt.
            # No cwd is available here, so this just keeps whatever
            # directory the process is already in rather than guessing.
            state = self._build_session(session_id, os.getcwd())
        agent = state.agent
        self._last_session = session_id

        is_first_turn = len(agent.messages) == 0
        text = _extract_prompt_text(prompt)

        if is_first_turn and self._conn is not None and text:
            title = text.strip().splitlines()[0][:60]
            await self._conn.session_update(
                session_id=session_id,
                update=acp.schema.SessionInfoUpdate(title=title, session_update="session_info_update"),
            )
            _touch_index(session_id, state.cwd, title, turn=True)
        else:
            _touch_index(session_id, state.cwd, turn=True)

        if text.strip() and not _is_resume(text):
            lab = lab_module.get()
            if lab.session_id == session_id:
                lab.set_mission(text)

        checkpoint_file = _checkpoint_path(session_id)
        # Set only by a screenshot turn: how to put the session's model back, and what was seen.
        restore = None
        vision: dict | None = None
        if _is_resume(text):
            if not checkpoint_file.exists():
                if self._conn is not None:
                    await self._conn.session_update(session_id=session_id, update=acp.update_agent_message_text("No interrupted turn to resume."))
                return acp.PromptResponse(stopReason="end_turn")
            if text.strip() == AUTO_RESUME_MARKER:
                if _auto_resume_attempts(session_id) >= MAX_AUTO_RESUMES:
                    if self._conn is not None:
                        await self._conn.session_update(session_id=session_id, update=acp.update_agent_message_text(f"Automatic resume already tried {MAX_AUTO_RESUMES} times from this checkpoint; not retrying on its own. /resume-turn tries again."))
                    return acp.PromptResponse(stopReason="end_turn")
                _count_auto_resume(session_id)
            data = json.loads(checkpoint_file.read_text())
            agent_input: object = {"checkpointResume": {"checkpoint": data["checkpoint"]}}
        else:
            checkpoint_file.unlink(missing_ok=True)  # a new turn supersedes an abandoned one
            _resume_marker_path(session_id).unlink(missing_ok=True)
            agent_input = text
            # The conversation as it is before this turn, so "go back" can return to it (session_store.py).
            await session_store.begin_turn(agent, text, SESSIONS_DIR)
            # A screenshot from the menu bar: this turn runs on the vision model with the image beside
            # the person's words, and skips the router — a specialist or the team would never see it.
            vision = os_context.vision_turn(os_context.pending(session_id))
            if vision is None:
                vision = await self._attached_images(session_id, state, prompt)
            if vision is not None:
                restore, why = _model_for_vision(agent, vision)
                if restore is None:
                    os_context.not_sent(session_id, why or "the screenshot was not sent")
                    vision = None
                else:
                    agent_input = [{"text": text}, *[os_context.image_block({**vision, **image}) for image in vision.get("images") or [vision]]]
            decision = None if vision is not None else await self._decide(state, text)
            self._adapt_mode(session_id, state, text, decision)
            if decision is not None:
                extra = self._apply_bandit(state, decision, text)
                recorder = outcomes.active(session_id)
                if recorder is not None:
                    recorder.set_route(
                        {
                            "target": decision.target.id if decision.target else None,
                            "action": decision.action,
                            "tier": decision.tier(),
                            "abstained": decision.abstained,
                            "confidence": round(decision.calibrated, 3),
                            "risk": decision.risk,
                            **extra,
                        }
                    )
                self._send_ext("cordata/laya", {"sessionId": session_id, "mode": "route", "router": "v2", **decision.to_event()})
                if decision.action == "dispatch" and decision.target and decision.target.id in state.specialists:
                    stop_reason = await self._dispatch(session_id, state, decision, text)
                    return acp.PromptResponse(stopReason=stop_reason if stop_reason in ("end_turn", "max_tokens", "cancelled", "max_turn_requests") else "end_turn")
                state.router_hooks.prime(agent, decision)

        try:
            # No mid-turn checkpoints for a screenshot turn: a checkpoint is the history written to disk,
            # and an interrupted one would keep the image there for a resume nobody may ask for.
            stop_reason, _result = await self._stream(session_id, agent, agent_input, checkpoints=restore is None)
        finally:
            if restore is not None:
                # The detour ends with the turn, however it ended: the session's own model back, and
                # the image out of the history before the conversation is saved again.
                restore()
                if os_context.strip_images(agent.messages, (vision or {}).get("app", "the frontmost window")):
                    manager = getattr(agent, "_session_manager", None)
                    if manager is not None:
                        try:
                            manager.sync_agent(agent)
                        except Exception:
                            logging.getLogger(__name__).warning("os-context: the image was removed from memory but the session could not be re-saved", exc_info=True)
        if stop_reason != "cancelled":
            checkpoint_file.unlink(missing_ok=True)
            _resume_marker_path(session_id).unlink(missing_ok=True)
        return acp.PromptResponse(stopReason=stop_reason if stop_reason in ("end_turn", "max_tokens", "cancelled", "max_turn_requests") else "end_turn")


async def _main() -> None:
    # Runs inside this process's one real, long-running event loop — see
    # start_mcp_clients' own docstring for why that matters: whichever
    # CordataAcpAgent session first calls build_agent_for_session already
    # tries to start these same MCP clients itself, from a throwaway
    # thread/loop that doesn't survive to serve a real tool call afterward.
    _protect_protocol_stdout()
    asyncio.create_task(start_mcp_clients())
    # One JSON-RPC message is one line, and asyncio's default line limit is 64 KiB: a prompt with a
    # large embedded file, or the menu bar's screenshot (`_cordata/os-context`, up to 4 MiB of
    # JPEG as base64), would otherwise end the connection. The supervisor's side reads with a far
    # larger limit (`Supervisor.FRAME_LIMIT`); this keeps the two in the same order of magnitude.
    reader, writer = await acp.stdio.stdio_streams(limit=STDIO_LINE_LIMIT)
    # acp.run_agent's params are misleadingly named: the first stream must be
    # a StreamWriter (what the connection writes outgoing messages to), the
    # second a StreamReader (what it reads incoming messages from) — the
    # opposite of the "write to input / read from output" in its docstring.
    await acp.run_agent(CordataAcpAgent(), writer, reader)


if __name__ == "__main__":
    asyncio.run(_main())

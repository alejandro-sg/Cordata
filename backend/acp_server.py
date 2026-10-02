"""ACP front door: the same Strands agent (agent_common.py) over the Agent
Client Protocol (https://agentclientprotocol.com), for editors like Zed that
speak ACP over stdio. See main.py for the AG-UI/web front door.

Run directly — an ACP client spawns this process and talks JSON-RPC over
its stdin/stdout, so don't print anything else to stdout:

    python acp_server.py

A conversation resumes across restarts and across front doors: this uses
the same on-disk FileSessionManager keyed by session id that main.py uses
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
"""

import asyncio
import json
import logging
import os
import uuid

import acp
import acp.stdio
from strands.interventions import Confirm, Deny, InterventionHandler, Proceed

from agent_common import GENERATED_TOOL_NAMES, MCP_TOOL_NAMES, build_agent_for_session, start_mcp_clients
from model_control_tool import _context_window_limit, _fetch_available_models

# Set-identical to main.py's _APPROVAL_REQUIRED_TOOLS — both gate `editor`
# and every MCP-server-provided tool too (main.py's own comment on
# MCP_TOOL_NAMES explains why); this repo has no shared source for the two
# lists, so keep them in sync by hand when either changes.
APPROVAL_REQUIRED_TOOLS = {"shell", "python_repl", "file_write", "editor", *GENERATED_TOOL_NAMES, *MCP_TOOL_NAMES}

_TOOL_KIND: dict[str, str] = {
    "shell": "execute",
    "python_repl": "execute",
    "file_write": "edit",
    "editor": "edit",
    "file_read": "read",
    "http_request": "fetch",
    "think": "think",
    "agent_memory": "other",
    "local_swarm": "other",
    "local_graph": "other",
}


def _tool_kind(tool_name: str) -> str:
    return _TOOL_KIND.get(tool_name, "other")


_ALLOW_KINDS = {"allow_once", "allow_always"}


def _permission_approved(outcome, options: list) -> bool:
    """True only when the client selected an option we offered as an allow kind.

    AllowedOutcome just means outcome == "selected" — it is also what a client
    sends when the user picks "Reject" — so the selected option_id has to be
    looked up in what we offered. Cancelled, unknown option ids, and reject
    kinds all deny.
    """
    if not isinstance(outcome, acp.schema.AllowedOutcome):
        return False
    selected = next((o for o in options if o.option_id == outcome.option_id), None)
    return selected is not None and selected.kind in _ALLOW_KINDS


# How much of a running sub-agent's latest reasoning one progress update carries.
_REASONING_TAIL_CHARS = 120


def _render_swarm_progress(entries: list, reasoning: dict | None = None) -> str:
    """Render local_swarm/local_graph's cumulative progress log (the same
    node_start/node_stop/handoff shape sub_agent_common.progress_entry
    builds for both tools) into plain text for one ACP tool-call update.

    Strands only wraps an async-generator tool's own intermediate yields
    (everything before its final return value) as a top-level {"type":
    "tool_stream", "tool_stream_event": {"tool_use", "data"}} event —
    confirmed against the installed strands/types/_events.py's
    ToolStreamEvent. Before the branch below existed, this file's prompt()
    loop had no case for "tool_stream" at all, so every {"state":
    {"swarm_progress": [...]}} local_swarm/local_graph yields mid-run (see
    their own stream_async loops) was silently dropped here — Zed saw the
    tool start and its final result, nothing in between, unlike the
    CopilotKit frontend's SwarmProgress.tsx, which reads the same state
    through AG-UI's separate StateSnapshotEvent translation
    (ag_ui_strands's ToolStreamEventHandler).
    """
    lines = []
    for entry in entries:
        kind = entry.get("type")
        if kind == "node_start":
            lines.append(f"started: {entry.get('node')}")
        elif kind == "node_stop":
            lines.append(f"finished: {entry.get('node')} - {entry.get('summary', '')}")
        elif kind == "handoff":
            frm = ", ".join(entry.get("from") or [])
            to = ", ".join(entry.get("to") or [])
            lines.append(f"handoff: {frm} -> {to} - {entry.get('message', '')}")

    # Live "what is each running sub-agent thinking right now": the tail of the
    # reasoning each node has streamed so far (sub_agent_common.reasoning_delta
    # feeds it). Only nodes that have started and not finished get a line, so a
    # finished node's long reasoning never lingers. Without this the ACP client
    # saw the orchestrator's own thoughts but nothing of the sub-agents'.
    finished = {e.get("node") for e in entries if e.get("type") == "node_stop"}
    for entry in entries:
        node = entry.get("node") if entry.get("type") == "node_start" else None
        text = (reasoning or {}).get(node) if node and node not in finished else None
        if text:
            tail = " ".join(text.split())[-_REASONING_TAIL_CHARS:]
            lines.append(f"thinking: {node} - {tail}")
    return "\n".join(lines)


# Session Modes (https://agentclientprotocol.com/protocol/session-modes) —
# the same three-way split Claude Code itself exposes (plan / default /
# bypass-permissions), mapped onto real behavioral differences in
# _ApprovalHandler.before_tool_call below, not just relabeled UI state:
#   plan  — no tool actually runs; every call is denied with a message
#           telling the model to describe its plan instead.
#   build — today's default: only APPROVAL_REQUIRED_TOOLS pause for a real
#           permission prompt, everything else runs immediately.
#   auto  — nothing pauses, ever. Opt-in only; never the default.
_MODES = [
    acp.schema.SessionMode(id="plan", name="Plan", description="Discuss and plan only — no tool actually runs."),
    acp.schema.SessionMode(id="build", name="Build", description="Tools run after your approval (default)."),
    acp.schema.SessionMode(id="auto", name="Auto", description="Tools run with no approval prompts. Use with caution."),
]
_MODE_IDS = {mode.id for mode in _MODES}
_DEFAULT_MODE = "build"

# Config option "temperature" exposes strands' set_temperature tool as a
# select instead of a free-float, since SessionConfigOptionSelect only takes
# discrete option ids — matches the granularity a picker in an editor UI
# actually wants (three presets, not a number field).
_TEMPERATURE_PRESETS = [("focused", 0.2, "Focused"), ("balanced", 0.7, "Balanced"), ("creative", 1.2, "Creative")]


def _closest_temperature_preset(temperature: float | None) -> str:
    if temperature is None:
        return "balanced"
    return min(_TEMPERATURE_PRESETS, key=lambda preset: abs(preset[1] - temperature))[0]


def _config_options(agent) -> list:
    """The live config-option list for one session's agent — always derived
    fresh from the agent's actual model config, never a separately-tracked
    copy that could drift from what's really active.
    """
    config = agent.model.get_config()
    current_model = config.get("model_id")
    current_temperature = (config.get("params") or {}).get("temperature")

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


class _ApprovalHandler(InterventionHandler):
    """Gates dangerous tools via Strands' native Interventions system
    (strands.interventions — a transport-agnostic pause/resume mechanism
    built on the same Interrupt/InterruptException primitive ag_ui_strands
    uses internally for the web front door's HITL, but with no AG-UI runtime
    underneath it here).

    One instance per session, held onto directly (see SierraLabAcpAgent's
    _sessions dict) so set_session_mode() can flip ``self.mode`` on the same
    object the running agent's hook registry already points at — no need to
    rebuild the Agent (and lose its conversation state) just to change modes.

    Strands' ``Interrupt`` dataclass carries only ``id``/``name``/``reason``/
    ``response`` — no separate metadata slot — so the tool name/input/
    toolUseId this handler learns from ``event.tool_use`` (only available
    *here*, at interrupt-raise time) is packed into ``reason`` as JSON and
    read back in AcpAgent.prompt() once the interrupt actually resolves.
    """

    name = "acp-approval"

    def __init__(self) -> None:
        self.mode = _DEFAULT_MODE

    def before_tool_call(self, event):
        tool_name = event.tool_use.get("name")

        if self.mode == "plan":
            return Deny(reason=f"Plan mode is active — {tool_name} was not run. Describe the change instead of making it.")
        if self.mode == "auto":
            return Proceed()
        if tool_name not in APPROVAL_REQUIRED_TOOLS:
            return Proceed()

        payload = json.dumps(
            {
                "name": tool_name,
                "input": event.tool_use.get("input"),
                "tool_use_id": event.tool_use.get("toolUseId"),
            }
        )
        return Confirm(prompt=payload)


def _replay_history(messages: list[dict]) -> list:
    """Turn an Agent's restored message history into ACP session/update
    notifications, for load_session — Zed rebuilds its chat view entirely
    from these, since it has no other way to see a conversation that started
    before this process existed.
    """
    updates = []
    for message in messages:
        role = message.get("role")
        for block in message.get("content") or []:
            text = block.get("text") if isinstance(block, dict) else None
            if not text:
                continue
            if role == "user":
                updates.append(acp.update_user_message_text(text))
            elif role == "assistant":
                updates.append(acp.update_agent_message_text(text))
    return updates


def _project_system_prompt(cwd: str) -> str:
    """agent_common.SYSTEM_PROMPT hardcodes backend/workspace/ as the
    sandbox root — correct for the AG-UI/web front door, actively wrong
    here: an ACP client (Zed) is meant to work on whatever project it has
    open, told to us via new_session/load_session's own ``cwd`` — a totally
    different project every time, not one fixed scratch directory. Real bug
    this fixes: the agent used to answer "what project are we in?" by
    describing this repo's own backend, regardless of what Zed had open,
    because nothing here ever read the ``cwd`` the protocol was already
    handing over.
    """
    return (
        "You are a local coding agent running entirely on-device via oMLX, "
        f"working in the project at {cwd}. Your shell, file, and code tools "
        "operate relative to this directory — explore it first if you're not "
        "sure what's there."
    )


def _extract_prompt_text(blocks: list) -> str:
    """Text + @-mentioned content from one prompt turn.

    Previously only TextContentBlock was read — every ResourceContentBlock
    (a plain @-mention, Zed sends just a name/uri/description, no content)
    and EmbeddedResourceContentBlock (Zed inlined the actual file text) was
    silently dropped, so mentioning a file in Zed never actually reached the
    model. An embedded resource's real text is inlined directly; a bare
    reference is turned into a line the model can act on itself (it has
    file_read).
    """
    parts: list[str] = []
    for block in blocks:
        block_type = getattr(block, "type", None)
        if block_type == "text":
            parts.append(block.text)
        elif block_type == "resource_link":
            label = getattr(block, "title", None) or getattr(block, "name", block.uri)
            parts.append(f"[Referenced file: {label} ({block.uri})]")
        elif block_type == "resource":
            resource = block.resource
            uri = getattr(resource, "uri", "unknown")
            text = getattr(resource, "text", None)
            parts.append(f"[Attached: {uri}]\n{text}" if text is not None else f"[Attached binary resource: {uri}]")
    return "\n\n".join(part for part in parts if part)


class SierraLabAcpAgent:
    """Implements initialize, new_session, load_session, prompt, cancel,
    set_session_mode, set_config_option — the subset of the ACP `Agent`
    protocol an interactive Zed session actually exercises. fork_session/
    resume_session/authenticate aren't wired up (nothing here needs forking
    or auth), and list_sessions isn't either (Zed doesn't call it in this
    configuration).
    """

    def __init__(self) -> None:
        self._conn: acp.Client | None = None
        # session_id -> (Agent, _ApprovalHandler) — the handler is kept
        # alongside its agent so set_session_mode can reach the exact
        # instance the agent's hooks already reference.
        self._sessions: dict[str, tuple] = {}
        self._cancelled: set[str] = set()

    def on_connect(self, conn: acp.Client) -> None:
        self._conn = conn

    async def initialize(self, protocol_version: int, **kwargs) -> acp.InitializeResponse:
        return acp.InitializeResponse(
            protocolVersion=acp.PROTOCOL_VERSION,
            agentCapabilities=acp.schema.AgentCapabilities(load_session=True),
        )

    def _laya_notifier(self, session_id: str):
        """A thread-safe callback that forwards Laya's live triage events to the
        client as an ACP *extension notification* (method ``_sierralab/laya``).
        The ACP spec says a client that doesn't understand an extension
        notification must ignore it, so Zed and other clients are unaffected;
        acp-tui renders it as the live "⚡ laya" line.

        Hooks run on the event loop's thread and the classifier's callbacks on
        a worker thread, so the send is scheduled with call_soon_threadsafe.
        """
        loop = asyncio.get_running_loop()

        def notify(payload: dict) -> None:
            conn = self._conn
            if conn is None:
                return

            async def send() -> None:
                try:
                    await conn.ext_notification("sierralab/laya", {"sessionId": session_id, **payload})
                except Exception:
                    logging.getLogger(__name__).debug("laya notification not delivered", exc_info=True)

            loop.call_soon_threadsafe(lambda: loop.create_task(send()))

        return notify

    def _build_session(self, session_id: str, cwd: str) -> tuple:
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
        handler = _ApprovalHandler()
        agent = build_agent_for_session(
            session_id,
            interventions=[handler],
            system_prompt=_project_system_prompt(cwd),
            triage_notify=self._laya_notifier(session_id),
        )
        self._sessions[session_id] = (agent, handler)
        return agent, handler

    async def new_session(self, cwd: str, **kwargs) -> acp.NewSessionResponse:
        session_id = str(uuid.uuid4())
        agent, _handler = self._build_session(session_id, cwd)
        return acp.NewSessionResponse(
            sessionId=session_id,
            modes=acp.schema.SessionModeState(current_mode_id=_DEFAULT_MODE, available_modes=_MODES),
            config_options=_config_options(agent),
        )

    async def load_session(self, cwd: str, session_id: str, **kwargs) -> acp.LoadSessionResponse:
        agent, _handler = self._build_session(session_id, cwd)

        if self._conn is not None:
            for update in _replay_history(agent.messages):
                await self._conn.session_update(session_id=session_id, update=update)

        # Mode isn't persisted across restarts (FileSessionManager only
        # persists agent messages) — a reloaded session always comes back in
        # the safe default (build), never a previously-chosen plan/auto.
        return acp.LoadSessionResponse(
            modes=acp.schema.SessionModeState(current_mode_id=_DEFAULT_MODE, available_modes=_MODES),
            config_options=_config_options(agent),
        )

    async def set_session_mode(self, session_id: str, mode_id: str, **kwargs) -> acp.SetSessionModeResponse | None:
        entry = self._sessions.get(session_id)
        if entry is None or mode_id not in _MODE_IDS:
            return None
        _agent, handler = entry
        handler.mode = mode_id
        return acp.schema.SetSessionModeResponse()

    async def set_config_option(self, config_id: str, session_id: str, value, **kwargs):
        entry = self._sessions.get(session_id)
        if entry is None:
            return None
        agent, _handler = entry

        if config_id == "model" and isinstance(value, str):
            available = _fetch_available_models()
            if not available or value in available:
                config_update: dict = {"model_id": value}
                limit = _context_window_limit(value)
                if limit is not None:
                    config_update["context_window_limit"] = limit
                agent.model.update_config(**config_update)

                import agent_common

                agent_common.ACTIVE_MODEL_ID = value
        elif config_id == "temperature" and isinstance(value, str):
            preset = next((p for p in _TEMPERATURE_PRESETS if p[0] == value), None)
            if preset is not None:
                current_params = dict(agent.model.get_config().get("params") or {})
                current_params["temperature"] = preset[1]
                agent.model.update_config(params=current_params)

                import agent_common

                agent_common.ACTIVE_TEMPERATURE = preset[1]

        return acp.schema.SetSessionConfigOptionResponse(config_options=_config_options(agent))

    async def cancel(self, session_id: str, **kwargs) -> None:
        self._cancelled.add(session_id)

    async def _resolve_interrupts(self, session_id: str, interrupts: list) -> list:
        """Turn each unanswered Strands interrupt into a real ACP permission
        prompt, and build the resume payload strands.interrupt._InterruptState.resume
        expects: a list of {"interruptResponse": {"interruptId", "response"}}.
        """
        responses = []
        for interrupt in interrupts:
            try:
                payload = json.loads(interrupt.reason)
                tool_name = payload.get("name", "tool")
                tool_input = payload.get("input")
                tool_use_id = payload.get("tool_use_id") or interrupt.id
            except (TypeError, ValueError):
                tool_name, tool_input, tool_use_id = str(interrupt.reason), None, interrupt.id

            approved = False
            if self._conn is not None:
                options = [
                    acp.schema.PermissionOption(option_id="allow", name="Allow", kind="allow_once"),
                    acp.schema.PermissionOption(option_id="reject", name="Reject", kind="reject_once"),
                ]
                result = await self._conn.request_permission(
                    session_id=session_id,
                    tool_call=acp.schema.ToolCallUpdate(
                        tool_call_id=tool_use_id,
                        title=f"Run {tool_name}",
                        kind=_tool_kind(tool_name),
                        raw_input=tool_input,
                    ),
                    options=options,
                )
                approved = _permission_approved(result.outcome, options)

            responses.append({"interruptResponse": {"interruptId": interrupt.id, "response": approved}})
        return responses

    async def _send_tool_result(self, session_id: str, result: dict) -> None:
        """One finished tool call as a terminal ACP tool-call update."""
        tool_use_id = result.get("toolUseId")
        if not tool_use_id or self._conn is None:
            return
        status = "completed" if result.get("status") == "success" else "failed"
        content = [
            acp.tool_content(acp.text_block(block["text"]))
            for block in result.get("content") or []
            if isinstance(block, dict) and block.get("text")
        ]
        await self._conn.session_update(
            session_id=session_id,
            update=acp.update_tool_call(tool_use_id, status=status, content=content or None),
        )

    async def prompt(self, session_id: str, prompt: list, **kwargs) -> acp.PromptResponse:
        entry = self._sessions.get(session_id)
        if entry is None:
            # Defensive only — the ACP protocol always calls new_session or
            # load_session (both of which carry the real cwd) before prompt.
            # No cwd is available here, so this just keeps whatever
            # directory the process is already in rather than guessing.
            entry = self._build_session(session_id, os.getcwd())
        agent, _handler = entry

        is_first_turn = len(agent.messages) == 0
        text = _extract_prompt_text(prompt)

        if is_first_turn and self._conn is not None and text:
            title = text.strip().splitlines()[0][:60]
            await self._conn.session_update(
                session_id=session_id,
                update=acp.schema.SessionInfoUpdate(title=title, session_update="session_info_update"),
            )

        agent_input: object = text
        seen_tool_calls: set[str] = set()
        last_progress: dict[str, str] = {}

        self._cancelled.discard(session_id)

        # Loops once per interrupt round-trip: a turn that never gets
        # interrupted runs this exactly once; a gated tool call pauses the
        # inner stream, gets resolved via a real ACP permission prompt, and
        # re-enters with the resume payload as agent_input.
        while True:
            stop_reason = "end_turn"
            pending_interrupts: list = []

            async for event in agent.stream_async(agent_input):
                if session_id in self._cancelled:
                    return acp.PromptResponse(stopReason="cancelled")
                if self._conn is None:
                    continue

                if "tool_interrupt_event" in event:
                    interrupts = event["tool_interrupt_event"].get("interrupts") or []
                    if interrupts:
                        stop_reason = "interrupt"
                        pending_interrupts = list(interrupts)
                    continue

                if "result" in event:
                    reason = event["result"].stop_reason
                    if reason == "interrupt":
                        # Belt-and-suspenders with the tool_interrupt_event
                        # branch above: AgentResult.interrupts carries the
                        # same list, so this still works even if that event
                        # was ever skipped for some interrupt path.
                        stop_reason = "interrupt"
                        pending_interrupts = pending_interrupts or list(event["result"].interrupts or [])
                    else:
                        stop_reason = reason if reason in ("end_turn", "max_tokens") else "end_turn"
                    continue

                event_type = event.get("type")
                if event_type == "tool_use_stream":
                    current = event.get("current_tool_use") or {}
                    tool_use_id = current.get("toolUseId")
                    if tool_use_id and tool_use_id not in seen_tool_calls:
                        seen_tool_calls.add(tool_use_id)
                        tool_name = current.get("name") or "tool"
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
                    tool_use_id = (stream_event.get("tool_use") or {}).get("toolUseId")
                    state = (stream_event.get("data") or {}).get("state") or {}
                    entries = state.get("swarm_progress")
                    if tool_use_id and entries:
                        rendered = _render_swarm_progress(entries, state.get("reasoning"))
                        # The swarm/graph tools re-yield their whole state every
                        # ~24 reasoning characters; a client only needs to hear
                        # when the rendered text actually changed.
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
                    await self._send_tool_result(session_id, event.get("tool_result") or {})
                    continue

                # Strands >= 1.5x no longer yields a "tool_result" event from
                # stream_async (confirmed against the installed 1.55.1: the
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
                                await self._send_tool_result(session_id, tool_result)
                    continue

                if "data" in event:
                    await self._conn.session_update(
                        session_id=session_id,
                        update=acp.update_agent_message_text(event["data"]),
                    )
                elif "reasoningText" in event:
                    await self._conn.session_update(
                        session_id=session_id,
                        update=acp.update_agent_thought_text(event["reasoningText"]),
                    )

            if stop_reason != "interrupt":
                return acp.PromptResponse(stopReason=stop_reason)

            agent_input = await self._resolve_interrupts(session_id, pending_interrupts)


class _ProtocolStdout:
    """``sys.stdout`` while serving ACP: text writes (``print`` — Strands'
    default PrintingCallbackHandler, strands_tools' rich panels, a stray debug
    line) go to stderr; only the binary ``.buffer`` is the real stdout, which
    acp.stdio's transport writes JSON-RPC frames to.

    Why this exists: ACP frames and ``print()`` shared one stream, so streamed
    reply text printed *without a newline* glued itself onto the next frame
    (``pong{"jsonrpc":...}``). The client could not parse that line, answered
    ``-32700 Parse error`` and dropped the frame — every agent_message_chunk
    could vanish, deterministically with PYTHONUNBUFFERED=1 and at random
    (whenever the 8 KB pipe buffer flushed) without it.
    """

    def __init__(self, protocol_file, stray) -> None:
        # `.buffer` is what acp.stdio's Windows transport writes to;
        # `.fileno()` is what its POSIX path hands to loop.connect_write_pipe —
        # both must be the protocol stream, never stderr.
        self.buffer = protocol_file
        self._protocol = protocol_file
        self._stray = stray

    def fileno(self) -> int:
        return self._protocol.fileno()

    def close(self) -> None:
        self._protocol.close()

    def write(self, text: str) -> int:
        return self._stray.write(text)

    def flush(self) -> None:
        self._stray.flush()

    def isatty(self) -> bool:
        return False

    def __getattr__(self, name):
        return getattr(self._stray, name)


def _protect_protocol_stdout() -> None:
    """Reserve the real stdout for ACP frames and send everything else to stderr.

    fd 1 is re-pointed at stderr too, so a child process that inherits stdout
    (or any C-level write) can't reach the protocol stream either.
    """
    import sys

    sys.stdout.flush()
    protocol_fd = os.dup(1)
    os.dup2(2, 1)
    sys.stdout = _ProtocolStdout(os.fdopen(protocol_fd, "wb", buffering=0), sys.stderr)


async def _main() -> None:
    # Runs inside this process's one real, long-running event loop — see
    # start_mcp_clients' own docstring for why that matters: whichever
    # SierraLabAcpAgent session first calls build_agent_for_session already
    # tries to start these same MCP clients itself, from a throwaway
    # thread/loop that doesn't survive to serve a real tool call afterward.
    _protect_protocol_stdout()
    await start_mcp_clients()
    reader, writer = await acp.stdio.stdio_streams()
    # acp.run_agent's params are misleadingly named: the first stream must be
    # a StreamWriter (what the connection writes outgoing messages to), the
    # second a StreamReader (what it reads incoming messages from) — the
    # opposite of the "write to input / read from output" in its docstring.
    await acp.run_agent(SierraLabAcpAgent(), writer, reader)


if __name__ == "__main__":
    asyncio.run(_main())

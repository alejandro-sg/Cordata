"""The daemon's HTTP and WebSocket surface.

Small on purpose. Four routes and one socket:

    GET  /health    is it up, what is it holding, is the observer running
    GET  /sessions  what children are running
    POST /sessions  start one in a given project directory
    POST /observe   queue an observation for the proactive loop (a source, not a trigger)
    WS   /ws        drive one session

Everything a client actually does — prompts, approvals, cancellation, config
changes — is an ACP message that this daemon forwards verbatim. There is no
REST endpoint for "approve a tool call" because inventing one would mean the
daemon parsing and re-encoding ACP, which is the thing `frames.py` exists to
avoid.

Security. Three properties, all of them load-bearing:

* **Loopback only.** The bind address is 127.0.0.1 and is not configurable.
  This process spawns shells in arbitrary project directories; it must not be
  reachable from a network.
* **A token, always.** Every route and the socket require the token from
  `~/.cordata/daemon.json`, mode 0600. Loopback is not an authorization
  boundary — any local process, and any page in a local browser, can reach it.
* **No browser origins.** A WebSocket handshake carries an `Origin` header that
  a script cannot forge. Rejecting any request with one turns a malicious page
  reaching `ws://127.0.0.1` into a failure, which matters because the token is
  stored in a file the page cannot read but the socket does not otherwise
  distinguish.
"""

from __future__ import annotations

import asyncio
import hmac
import json
import logging
import os
import secrets
import stat
import time
from pathlib import Path
from typing import Any

from fastapi import FastAPI, Header, HTTPException, Request, WebSocket, WebSocketDisconnect
from fastapi.responses import JSONResponse

from . import frames
from .supervisor import Supervisor

logger = logging.getLogger(__name__)

DAEMON_HOST = "127.0.0.1"
DEFAULT_PORT = 8765


def token_path() -> Path:
    override = os.environ.get("CORDATA_DAEMON_CONFIG")
    if override:
        return Path(override)
    import settings_store  # noqa: PLC0415 — keeps this module importable standalone

    return settings_store.user_dir() / "daemon.json"


def load_or_create_token(port: int = DEFAULT_PORT, grpc_port: int | None = None) -> str:
    """The daemon's token, generated on first run.

    Written 0600 in the user's private harness directory. Not in `settings.json`
    for the same reason `vault.token` is not: a settings file is layered,
    printed by `/config` and shareable; a credential is none of those.

    The port is rewritten every call rather than only on creation. A client
    finds the daemon by reading this file, so a file naming a port the daemon
    is not listening on is worse than no file: it connects somewhere else, or
    nowhere, and reports the wrong thing. `--port` and `CORDATA_DAEMON_PORT`
    both make that divergence possible, which is why this takes the port the
    server is actually about to bind.
    """
    path = token_path()
    token = ""
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        if isinstance(data.get("token"), str):
            token = data["token"]
    except (OSError, ValueError):
        pass
    if not token:
        token = secrets.token_urlsafe(32)
    path.parent.mkdir(parents=True, exist_ok=True)
    # Written to a temporary file created 0600 and renamed over the old one: a client reading the file mid-write
    # (the GUI connecting while the daemon starts — this runs twice at startup) sees the old file or the new one,
    # never a truncated one. `os.open` applies the mode at creation, so the token is never readable by other users.
    record: dict[str, Any] = {"token": token, "port": port}
    if grpc_port:
        record["grpcPort"] = grpc_port  # where the GUI's gRPC client finds the bridge
    temp = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, stat.S_IRUSR | stat.S_IWUSR)
    try:
        os.fchmod(fd, stat.S_IRUSR | stat.S_IWUSR)
    except OSError:
        logger.warning("daemon: could not restrict %s to 0600", temp)
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        handle.write(json.dumps(record, indent=2) + "\n")
    os.replace(temp, path)
    return token


class State:
    """What the app carries between requests."""

    def __init__(self, port: int = DEFAULT_PORT) -> None:
        from .discord_bot import DiscordRemote  # noqa: PLC0415 — imports discord lazily, in start()
        from .observer import Observer  # noqa: PLC0415 — observer imports settings lazily

        self.supervisor = Supervisor()
        self.token = load_or_create_token(port)
        self.port = port
        self.grpc_port: int | None = None  # set once the gRPC bridge has bound (daemon/main.py)
        self.started = time.time()
        self._subscribers: set[str] = set()
        # The observer runs only while nobody is attached; `busy` is the daemon's
        # answer to "is a person here", which is what `Observer.wake` needs and
        # what keeps a cycle off a live turn's project.
        self.observer = Observer(busy=self.busy)
        # The Discord front door (discord_bot.py). Constructed here so `/health` can
        # report it, started by daemon/main.py once there is a loop to run it on —
        # and inert unless the environment names a token, an operator and a channel.
        # The conversation hub (chat.py): every front door's conversations, owned by the daemon so a
        # terminal, Discord, the menu bar and the web app can all continue the same one. Started by
        # daemon/main.py with the loop; Discord is one of its users, not its owner.
        from .chat import Chats, chat_store  # noqa: PLC0415

        self.chats = Chats(self.supervisor, store=chat_store())
        self.remote = DiscordRemote(self.supervisor, chats=self.chats)

    def busy(self) -> bool:
        return bool(self._subscribers)

    def valid(self, supplied: str | None) -> bool:
        """Constant-time: a token comparison that returns early leaks its
        prefix to anything that can time the request."""
        return bool(supplied) and hmac.compare_digest(supplied, self.token)

    def check(self, supplied: str | None) -> None:
        if not self.valid(supplied):
            raise HTTPException(status_code=401, detail="bad or missing daemon token")


async def _session_for_cwd(supervisor: Supervisor, cwd: str) -> str | None:
    """The id of a live session in `cwd`, or of one started there now.

    A client that reconnects asks by directory because that is what it knows;
    the session id was minted here and never had to be written down. An exited
    session for the same directory is dropped rather than reattached to: its
    backlog is a finished conversation, and the client's own `session/load` is
    what brings history back — not a socket that can no longer answer.
    """
    existing = supervisor.find_by_cwd(cwd)
    if existing is not None:
        if existing.alive:
            return existing.session_id
        await supervisor.stop(existing.session_id)
    try:
        return (await supervisor.spawn(cwd)).session_id
    except ValueError:
        return None


_ADOPTABLE = __import__("re").compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{7,63}$")


async def _conversation_for(state: "State", ref: str, cwd: str | None, origin: str, title: str, adopt: str = ""):
    """The hub conversation `ref` names — a handle, `latest` (the newest in `cwd`, else a new one) or
    `new` — with its agent running and its session open. Raises `ChatError` with a sentence to show."""
    from .chat import ChatError, resolve_dir  # noqa: PLC0415

    chats = state.chats
    conv = None
    if ref not in ("latest", "new"):
        conv = chats.resolve(ref)
        if conv is None:
            raise ChatError(f"no conversation named {ref!r}")
    else:
        if not cwd:
            raise ChatError("a new conversation needs a folder (cwd)")
        folder = resolve_dir(cwd)
        if ref == "latest":
            conv = chats.latest(folder)
        if conv is None:
            conv = chats.create(cwd=folder, title=title, origin=origin)
            if adopt:
                # A conversation that already exists as an ACP session (a fork): the agent loads it instead of starting fresh.
                if not _ADOPTABLE.match(adopt):
                    raise ChatError("that is not a session id")
                conv.acp_id = adopt
    return await chats.ensure(conv.key)


def _admit_terminal_turn(state: "State", conv: Any, line: str) -> str | None:
    """A frame a terminal attached to a conversation is sending. A `session/prompt` while another
    door's turn runs is refused with a JSON-RPC error the terminal shows as a failed turn; one that is
    admitted is recorded as the terminal's turn, so the other doors queue behind it. Anything else
    passes. Returns the refusal frame, or None to forward."""
    if '"session/prompt"' not in line:
        return None
    try:
        message = json.loads(line)
    except ValueError:
        return None
    if not isinstance(message, dict) or message.get("method") != "session/prompt" or "id" not in message:
        return None
    if conv.busy:
        from .chat import door_name  # noqa: PLC0415

        who = door_name(conv.turn_origin)
        return json.dumps({
            "jsonrpc": "2.0",
            "id": message["id"],
            "error": {"code": -32000, "message": f"a turn from {who} is already running in this conversation — it will answer here; send yours when it finishes"},
        })
    blocks = ((message.get("params") or {}).get("prompt") or [])
    text = " ".join(str(b.get("text") or "") for b in blocks if isinstance(b, dict) and b.get("type") == "text")
    state.chats.begin_external(conv, str(message["id"]), text, origin="terminal")
    return None


def _maybe_end_terminal_turn(state: "State", conv: Any, line: str) -> None:
    """The child's response to the terminal's `session/prompt` is passing back: that turn is over."""
    if '"id"' not in line or '"method"' in line[:200]:
        return  # a notification or a request: only a response can end the turn
    try:
        message = json.loads(line)
    except ValueError:
        return
    if isinstance(message, dict) and "method" not in message and str(message.get("id")) == conv.external_turn:
        stop = str(((message.get("result") or {}).get("stopReason")) or ("error" if "error" in message else "end_turn"))
        state.chats.end_external(conv, stop)


async def _observe_socket(websocket: WebSocket, state: "State", session: str | None, conversation: str | None) -> None:
    """``/ws?…&role=observer``: watch a live session (by id, or a hub conversation by handle) without driving it. Any
    number of observers; the driver is never displaced; an observer that writes is closed (4405) — answering an approval
    is the driver's, and "first answer wins" holds among drivers only."""
    supervisor = state.supervisor
    if conversation:
        conv = state.chats.resolve(conversation)
        session = conv.daemon_id if conv is not None else None
    rows = {row["sessionId"] for row in supervisor.sessions()}
    if not session or session not in rows:
        await websocket.close(code=4404, reason="no such live session")
        return
    queue = supervisor.watch_session(session)
    await websocket.accept()

    async def send() -> None:
        while True:
            await websocket.send_text(await queue.get())

    sender = asyncio.ensure_future(send())
    try:
        message = await websocket.receive()
        if message.get("type") != "websocket.disconnect":
            await websocket.close(code=4405, reason="observers are read-only")
    except (WebSocketDisconnect, RuntimeError):
        pass
    finally:
        sender.cancel()
        supervisor.unwatch_session(session, queue)


def create_app(port: int = DEFAULT_PORT) -> FastAPI:
    app = FastAPI(title="cordata daemon", version="1")
    state = State(port)
    app.state.daemon = state

    def authorised(x_daemon_token: str | None) -> None:
        state.check(x_daemon_token)

    @app.get("/health")
    async def health(x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        authorised(x_daemon_token)
        sessions = state.supervisor.sessions()
        return {
            "ok": True,
            "port": state.port,
            "grpc_port": state.grpc_port,
            "uptime": time.time() - state.started,
            "sessions": len(sessions),
            "alive": sum(1 for session in sessions if session["alive"]),
            "observer": state.observer.status(),
            # Ids and reachability, never the token: a credential in a health payload is a
            # credential in whatever scrapes it.
            "discord": state.remote.status(),
        }

    def observer_pod() -> dict[str, Any] | None:
        """The observer is a child process of the daemon, not a pod in any session's
        process, so the daemon contributes its pod to the Lab view itself. Absent
        when the observer is switched off (it is off by default)."""
        status = state.observer.status()
        if not status.get("enabled"):
            return None
        return {
            "id": "observer",
            "name": "observer",
            "role": "observer",
            "parent": None,
            "status": "synthesizing" if status.get("cycling") else "idle",
            "isolated": False,
            "cycles": status.get("cycles"),
            "queued": status.get("queued"),
            "scope": "daemon",
        }

    @app.get("/lab")
    async def lab_snapshot(x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        """The Experimental Lab as JSON: the newest state of every session's pods,
        links, threat gauge and HITL station, plus the daemon's own observer pod.
        Read-only — a control (E-Stop, dial, guidance) is an ACP extension request
        on the session's own connection, never an unauthenticated REST verb."""
        authorised(x_daemon_token)
        return {"sessions": state.supervisor.lab_states(), "observer": observer_pod()}

    @app.websocket("/lab/ws")
    async def lab_socket(websocket: WebSocket, session: str | None = None) -> None:
        """A read-only stream of Lab state for a remote canvas. Same rules as `/ws`
        — loopback, token, no browser origins — because the frames carry tool inputs
        and approval prompts. Doesn't attach to (or displace) the driving client."""
        if websocket.headers.get("origin"):
            await websocket.close(code=4403, reason="browser origins are not accepted")
            return
        if not state.valid(websocket.query_params.get("token")):
            await websocket.close(code=4401, reason="bad or missing daemon token")
            return
        await websocket.accept()
        supervisor = state.supervisor
        feed = supervisor.watch_lab()
        try:
            snapshot = {sid: lab for sid, lab in supervisor.lab_states().items() if session in (None, sid)}
            await websocket.send_text(json.dumps({"type": "snapshot", "sessions": snapshot, "observer": observer_pod()}))
            last_observer = observer_pod()
            while True:
                reader = asyncio.create_task(websocket.receive_text())
                writer = asyncio.create_task(feed.get())
                done, pending = await asyncio.wait({reader, writer}, timeout=2.0, return_when=asyncio.FIRST_COMPLETED)
                for task in pending:
                    task.cancel()
                if reader in done:
                    reader.result()  # raises WebSocketDisconnect when the canvas goes away; anything it sends is ignored
                if writer in done:
                    frame = writer.result()
                    if session in (None, frame["sessionId"]):
                        await websocket.send_text(json.dumps(frame))
                current = observer_pod()
                if current != last_observer:
                    last_observer = current
                    await websocket.send_text(json.dumps({"type": "observer", "observer": current}))
        except (WebSocketDisconnect, RuntimeError):
            pass
        except Exception:
            logger.exception("daemon: lab socket failed")
        finally:
            supervisor.unwatch_lab(feed)

    @app.post("/observe")
    async def observe(payload: dict, x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        """Feed the observer from outside: a deploy hook, a CI job, a log watcher.

        A *source*, not a trigger. The text is queued and read by the next cycle,
        which will not run while a client is attached and will not run at all
        unless `observer.enabled` — so this returns whether the observer is even
        on, and how deep the queue is, rather than implying the observation was
        acted on. The token is a header (not a query parameter like the socket's,
        which only exists because a browser `WebSocket` cannot set headers): a
        cross-origin page cannot send a custom header without a preflight this
        app does not answer, so there is no CSRF path to it.
        """
        authorised(x_daemon_token)
        text = payload.get("text")
        if not isinstance(text, str) or not text.strip():
            raise HTTPException(status_code=400, detail="text is required")
        project = payload.get("project")
        depth = state.observer.observe(text, project=project if isinstance(project, str) and project else None)
        return {"queued": depth, "enabled": state.observer.enabled()}

    @app.get("/sessions")
    async def list_sessions(x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        authorised(x_daemon_token)
        return {"sessions": state.supervisor.sessions()}

    @app.post("/sessions")
    async def open_session(payload: dict, x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        authorised(x_daemon_token)
        cwd = payload.get("cwd")
        if not isinstance(cwd, str) or not cwd:
            raise HTTPException(status_code=400, detail="cwd is required")
        requested = payload.get("sessionId")
        if requested is not None and not (isinstance(requested, str) and requested):
            raise HTTPException(status_code=400, detail="sessionId must be a non-empty string")
        try:
            session = await state.supervisor.spawn(cwd, session_id=requested)
        except ValueError as error:
            raise HTTPException(status_code=400, detail=str(error)) from error
        return session.to_json()

    # ------------------------------------------------------------- conversations (the hub)

    @app.get("/conversations")
    async def list_conversations(cwd: str | None = None, x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        """Every door's conversations, newest first — what a terminal's `/conversations`, the menu
        bar's picker and the web app list. `cwd` narrows it to one project."""
        authorised(x_daemon_token)
        return {"conversations": state.chats.rows(cwd)}

    @app.post("/conversations")
    async def open_conversation(payload: dict, x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        """Resolve or open a conversation and make sure its agent is up: `{"conversation": "<handle>"}`,
        `{"conversation": "latest", "cwd": …}` (that project's newest, or a new one), or
        `{"conversation": "new", "cwd": …}`. Answers with its row — including the ACP session id a
        terminal `session/load`s to attach."""
        authorised(x_daemon_token)
        from .chat import ChatError  # noqa: PLC0415

        ref = str(payload.get("conversation") or "latest")
        origin = str(payload.get("origin") or "terminal")[:20]
        try:
            adopt = str(payload.get("adopt") or "") if ref == "new" else ""
            conv = await _conversation_for(state, ref, payload.get("cwd"), origin, str(payload.get("title") or ""), adopt)
        except ChatError as error:
            raise HTTPException(status_code=400, detail=str(error)) from error
        return conv.row()

    @app.post("/conversations/{handle}/rename")
    async def rename_conversation(handle: str, payload: dict, x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        """Rename a conversation (the app's sidebar, ⌘K). The title is one line, masked, at most 120 characters."""
        authorised(x_daemon_token)
        row = state.chats.rename(handle, str(payload.get("title") or ""))
        if row is None:
            raise HTTPException(status_code=404, detail="no such conversation, or an empty title")
        return row

    @app.post("/conversations/{handle}/archive")
    async def archive_conversation(handle: str, payload: dict, x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        """Hide a conversation from the lists (`{"archived": true}`), or bring it back. Nothing is deleted."""
        authorised(x_daemon_token)
        row = state.chats.set_archived(handle, bool(payload.get("archived", True)))
        if row is None:
            raise HTTPException(status_code=404, detail="no such conversation")
        return row

    @app.get("/conversations/search")
    async def search_conversations(q: str, cwd: str | None = None, limit: int = 20, x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        """Full-text search of conversations (the person's and the assistant's words, not tool output), newest first:
        titles first, then what was said. Snippets are masked; reads are bounded (session_history.search)."""
        authorised(x_daemon_token)
        import session_history  # noqa: PLC0415

        needle = (q or "").strip().lower()
        if len(needle) < 2:
            return {"results": []}
        rows = state.chats.rows(cwd, 500) + state.chats.rows(cwd, 500, archived=True)
        by_acp = {r["acpSessionId"]: r for r in rows if r.get("acpSessionId")}
        titled = [{**r, "snippet": r["title"], "where": "title"} for r in rows if needle and needle in (r.get("title") or "").lower()]
        sessions_dir = os.environ.get("CORDATA_SESSIONS_DIR") or str(Path(__file__).resolve().parent.parent / "sessions")
        said = await asyncio.to_thread(session_history.search, list(by_acp), sessions_dir, q, limit=limit)
        seen = {r["handle"] for r in titled}
        hits = titled + [{**by_acp[m["sessionId"]], "snippet": m["snippet"], "where": m["role"]} for m in said if by_acp[m["sessionId"]]["handle"] not in seen]
        return {"results": hits[: max(1, min(limit, 50))]}

    # ------------------------------------------------------------- automations (scheduled prompts)

    def automations_runner():
        from . import automations  # noqa: PLC0415

        if getattr(state, "automations", None) is None:
            import settings_store  # noqa: PLC0415

            store = automations.Store(Path(settings_store.user_dir()) / "automations.json")
            state.automations = automations.Runner(store, state.chats)
            state.automations.start()
        return state.automations

    @app.on_event("startup")
    async def start_automations() -> None:
        if getattr(state, "chats", None) is not None:
            automations_runner()

    @app.get("/automations")
    async def list_automations(x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        authorised(x_daemon_token)
        runner = automations_runner()
        return {"automations": [a.public() for a in sorted(runner.store.items.values(), key=lambda a: a.created)]}

    @app.post("/automations")
    async def save_automation(payload: dict, x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        """Create or update one (`id` to update). Validated: an existing folder, a schedule no tighter than 15 minutes."""
        authorised(x_daemon_token)
        from .automations import AutomationError  # noqa: PLC0415

        try:
            return automations_runner().store.upsert(payload).public()
        except AutomationError as error:
            raise HTTPException(status_code=400, detail=str(error)) from error

    @app.delete("/automations/{item_id}")
    async def delete_automation(item_id: str, x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        authorised(x_daemon_token)
        return {"deleted": automations_runner().store.delete(item_id)}

    @app.post("/automations/{item_id}/run")
    async def run_automation(item_id: str, x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        """Run one now, in the background: answers at once; the run is a new conversation every door can watch."""
        authorised(x_daemon_token)
        runner = automations_runner()
        if item_id not in runner.store.items:
            raise HTTPException(status_code=404, detail="no such automation")
        asyncio.ensure_future(runner.run(item_id))
        return {"started": True}

    # ------------------------------------------------------------- projects and their conversations (the app's rail)

    @app.get("/projects")
    async def list_projects(limit: int = 60, x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        """Every project the harness has worked in, newest-active first: its git repository and branch (a worktree says
        whose), whether it is trusted, its live sessions, conversations and approvals waiting (daemon/projects.py)."""
        authorised(x_daemon_token)
        from . import projects  # noqa: PLC0415

        hub = state.chats.rows(None, 500) if hasattr(state, "chats") and state.chats is not None else []
        return {"projects": await asyncio.to_thread(projects.projects, hub_rows=hub, live=state.supervisor.sessions(), limit=limit)}

    @app.get("/history")
    async def project_history(cwd: str, limit: int = 50, cursor: float | None = None, x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        """One project's conversations, newest first, paged by ``cursor`` — any of them, live or not."""
        authorised(x_daemon_token)
        if not os.path.isabs(cwd):
            raise HTTPException(status_code=400, detail="cwd must be an absolute path")
        from . import projects  # noqa: PLC0415

        sessions_dir = os.environ.get("CORDATA_SESSIONS_DIR") or str(Path(__file__).resolve().parent.parent / "sessions")
        return await asyncio.to_thread(lambda: projects.history(cwd, limit=limit, cursor=cursor, sessions_dir=sessions_dir))

    # ------------------------------------------------------------- orchestrator runs (Mission Control)

    @app.get("/orchestrator/runs")
    async def orchestrator_runs(cwd: str | None = None, limit: int = 30, before: float | None = None, x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        """Runs newest first, each with its liveness (running / paused / interrupted / terminal) — read from the ledger
        and the runs' own folders, no agent needed (orchestrator/control.py)."""
        authorised(x_daemon_token)
        from orchestrator import control  # noqa: PLC0415

        return {"runs": await asyncio.to_thread(control.list_runs, cwd=cwd, limit=limit, before=before)}

    @app.get("/orchestrator/runs/{run_id}")
    async def orchestrator_run(run_id: str, x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        authorised(x_daemon_token)
        from orchestrator import control  # noqa: PLC0415

        shown = await asyncio.to_thread(control.show_run, run_id)
        if shown is None:
            raise HTTPException(status_code=404, detail=f"no run {run_id}")
        return shown

    @app.get("/orchestrator/runs/{run_id}/events")
    async def orchestrator_events(run_id: str, offset: int = 0, limit: int = 500, x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        """A run's events after byte ``offset`` (``next`` continues): a door that attaches late catches up here."""
        authorised(x_daemon_token)
        from orchestrator import control  # noqa: PLC0415

        return await asyncio.to_thread(control.read_events, run_id, offset, limit)

    @app.post("/orchestrator/runs/{run_id}/control")
    async def orchestrator_control(run_id: str, payload: dict, x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        """``{"cmd": "pause" | "cancel" | "resume", "now", "restore"}``. Audited before it takes effect. Resume never
        grants direct network access from here — that is a trust card, in a session."""
        authorised(x_daemon_token)
        from orchestrator import control  # noqa: PLC0415

        import state_store  # noqa: PLC0415

        cmd = str(payload.get("cmd") or "")
        if cmd not in ("pause", "cancel", "resume"):
            raise HTTPException(status_code=400, detail="cmd must be pause, cancel or resume")
        state_store.append_event("orchestrator_control", {"action": cmd, "run_id": run_id, "now": bool(payload.get("now")), "restore": bool(payload.get("restore")), "via": "daemon"}, source="daemon")
        if cmd == "resume":
            return await asyncio.to_thread(control.resume_run, run_id)
        return await asyncio.to_thread(lambda: control.request(run_id, cmd, now=bool(payload.get("now")), restore=bool(payload.get("restore")) and cmd == "cancel", by="daemon"))

    @app.delete("/sessions/{session_id}")
    async def close_session(session_id: str, x_daemon_token: str | None = Header(default=None)) -> dict[str, Any]:
        """Stop a session and its process group.

        A client that closes its socket leaves the session running — that is the
        point of the daemon, the child survives to be reattached to — so there
        has to be a way to say "done with this one" that is not "shut the daemon
        down". Stopping a session that is already gone is not an error: the
        caller's intent (it is not running) is already satisfied.
        """
        authorised(x_daemon_token)
        state._subscribers.discard(session_id)
        stopped = await state.supervisor.stop(session_id)
        return {"sessionId": session_id, "stopped": stopped}

    @app.websocket("/ws")
    async def session_socket(
        websocket: WebSocket,
        session: str | None = None,
        cwd: str | None = None,
        conversation: str | None = None,
        role: str | None = None,
    ) -> None:
        # The token arrives as a query parameter because a browser WebSocket
        # constructor cannot set headers — and rejecting browsers (below) is
        # what makes that acceptable.
        origin = websocket.headers.get("origin")
        if origin:
            await websocket.close(code=4403, reason="browser origins are not accepted")
            return
        # Closed rather than raised: an `HTTPException` in a socket handler does
        # not go through the HTTP error handler, it just tears the connection
        # down with no reason on the wire — and the client needs to be told
        # whether it was the token or the session that was wrong.
        if not state.valid(websocket.query_params.get("token")):
            await websocket.close(code=4401, reason="bad or missing daemon token")
            return
        supervisor = state.supervisor
        #: The hub conversation this socket attached to, when it named one (`conversation=`).
        conv = None

        if role == "observer":
            await _observe_socket(websocket, state, session, conversation)
            return

        if conversation:
            # Attach to a conversation the daemon holds — the way one conversation is shared by every
            # door. The terminal then `session/load`s its ACP session id (from `POST /conversations`)
            # and sees the history; turns from any door appear in it as they run.
            from .chat import ChatError  # noqa: PLC0415

            try:
                conv = await _conversation_for(state, conversation, cwd, "terminal", "")
            except ChatError as error:
                await websocket.close(code=4404, reason=str(error)[:120])
                return
            session = conv.daemon_id
        elif session is None and cwd:
            # Attach-or-create by directory. The client's first message is
            # `session/new` either way, so it does not need the daemon's id to
            # begin — and a client reconnecting after a dropped terminal does
            # not have one to offer. The directory it would work in is the
            # thing it actually knows.
            session = await _session_for_cwd(supervisor, cwd)
            if session is None:
                await websocket.close(code=4404, reason=f"could not start a session in {cwd}")
                return
        elif session is None:
            await websocket.close(code=4400, reason="session is required")
            return

        if session in state._subscribers:
            # Two clients on one stdin would interleave answers to prompts that
            # only one of them can see.
            reason = "this conversation is open in another terminal" if conv is not None else "session already attached"
            await websocket.close(code=4409, reason=reason)
            return
        rows = {row["sessionId"]: row for row in supervisor.sessions()}
        if session not in rows:
            await websocket.close(code=4404, reason="no such session")
            return
        if rows[session].get("owner") and conv is None:
            # A hub conversation, reached by its session id rather than its handle. The handle is the
            # way in (`conversation=`): that path is the one that keeps turns from two doors apart.
            await websocket.close(code=4409, reason="that session belongs to a conversation — attach with conversation=<handle>")
            return

        # Claimed before `accept`, which awaits: two clients arriving together
        # would both pass the check above and then both be attached.
        state._subscribers.add(session)
        try:
            await websocket.accept()
        except BaseException:
            state._subscribers.discard(session)
            raise
        # A person is here. Any observer cycle in flight is abandoned — it is
        # killed, not asked to stop — because a briefing written over a project
        # someone is actively working in is the one thing this must not do.
        state.observer.wake(f"client attached to {session}")
        queue = supervisor.subscribe(session)
        # Replay what the child emitted while nobody was attached. This is the
        # half of reattaching that matters: a session parked on an approval
        # prompt will not re-emit the prompt, so a client that missed it would
        # otherwise be stuck with no way to answer.
        try:
            # An attaching client that did not know the id learns it here, and
            # learns it before the backlog: everything after this frame belongs
            # to the session it just named.
            if session != websocket.query_params.get("session"):
                await websocket.send_text(json.dumps({
                    "jsonrpc": "2.0",
                    "method": "_cordata/session",
                    "params": {"sessionId": session},
                }))
            for line in supervisor.drain_backlog(session):
                await websocket.send_text(line)
            while True:
                reader = asyncio.create_task(websocket.receive_text())
                writer = asyncio.create_task(queue.get())
                done, pending = await asyncio.wait({reader, writer}, return_when=asyncio.FIRST_COMPLETED)
                for task in pending:
                    task.cancel()
                for task in done:
                    if task is reader:
                        line = task.result()
                        if conv is not None:
                            refusal = _admit_terminal_turn(state, conv, line)
                            if refusal is not None:
                                # Another door's turn is running: answered here, never forwarded —
                                # one turn at a time, whoever asked.
                                await websocket.send_text(refusal)
                                continue
                        if not await supervisor.write(session, line):
                            # The child is gone. Dropping the frame silently
                            # leaves a client typing into a session that will
                            # never answer; closing tells it to start over.
                            await websocket.close(code=4410, reason="session has exited")
                            return
                    else:
                        line = task.result()
                        if conv is not None and conv.external_turn:
                            _maybe_end_terminal_turn(state, conv, line)
                        await websocket.send_text(line)
        except (WebSocketDisconnect, RuntimeError):
            pass
        except Exception:
            logger.exception("daemon: socket for %s failed", session)
        finally:
            state._subscribers.discard(session)
            supervisor.unsubscribe(session)

    @app.exception_handler(HTTPException)
    async def http_error(_: Request, exc: HTTPException) -> JSONResponse:
        return JSONResponse(status_code=exc.status_code, content={"error": exc.detail})

    return app

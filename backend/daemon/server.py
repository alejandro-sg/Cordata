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
  `~/.sierralab/daemon.json`, mode 0600. Loopback is not an authorization
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
    override = os.environ.get("SIERRALAB_DAEMON_CONFIG")
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
    nowhere, and reports the wrong thing. `--port` and `SIERRALAB_DAEMON_PORT`
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
    # Created 0600 rather than chmod-ed after the write: `write_text` makes the
    # file with the umask's mode first, which leaves a window in which the token
    # is readable by other local users. `os.open` applies the mode at creation;
    # the `fchmod` covers a file that already existed with looser permissions.
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, stat.S_IRUSR | stat.S_IWUSR)
    try:
        os.fchmod(fd, stat.S_IRUSR | stat.S_IWUSR)
    except OSError:
        logger.warning("daemon: could not restrict %s to 0600", path)
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        record: dict[str, Any] = {"token": token, "port": port}
        if grpc_port:
            record["grpcPort"] = grpc_port  # where the GUI's gRPC client finds the bridge
        handle.write(json.dumps(record, indent=2) + "\n")
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
        self.remote = DiscordRemote(self.supervisor)

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


def create_app(port: int = DEFAULT_PORT) -> FastAPI:
    app = FastAPI(title="sierralab daemon", version="1")
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

        if session is None and cwd:
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
            await websocket.close(code=4409, reason="session already attached")
            return
        if session not in {row["sessionId"] for row in supervisor.sessions()}:
            await websocket.close(code=4404, reason="no such session")
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
                    "method": "_sierralab/session",
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
                        if not await supervisor.write(session, task.result()):
                            # The child is gone. Dropping the frame silently
                            # leaves a client typing into a session that will
                            # never answer; closing tells it to start over.
                            await websocket.close(code=4410, reason="session has exited")
                            return
                    else:
                        await websocket.send_text(task.result())
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

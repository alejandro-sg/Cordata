"""Session registry: one `acp_server.py` child per session, and what to do when
one dies.

The invariant this module exists to hold: **the daemon never imports
`agent_common`, and never hosts an agent in its own process.** `agent_common`
calls `os.chdir(WORKSPACE_DIR)` at import time (`agent_common.py`), and
`acp_server._build_session` does a second, per-session `os.chdir(cwd)`. Both are
process-wide. A daemon that built agents in-process would have every session's
tools running against whichever directory was set last — silently, and only for
the sessions that happen to differ. Spawning is not an implementation detail
here; it is the thing that makes two sessions for two projects possible at all.

Children are spawned with `cwd=` set to the session's project directory so that
anything the child does *before* its own chdir (argument parsing, an import
that reads a relative path) is already in the right place.
"""

from __future__ import annotations

import asyncio
import itertools
import json
import logging
import os
import signal
import sys
import time
import uuid
from collections import OrderedDict
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

logger = logging.getLogger(__name__)

BACKEND_DIR = Path(__file__).resolve().parent.parent


def default_entrypoint() -> list[str]:
    """How to start a session process. Overridable for tests."""
    override = os.environ.get("SIERRALAB_ACP_ENTRYPOINT")
    if override:
        return override.split()
    return [sys.executable, str(BACKEND_DIR / "acp_server.py")]


@dataclass
class PendingPermission:
    """An ACP ``session/request_permission`` the child has sent and is blocked on.

    Held so a second front door — the GUI's containment modal, over gRPC — can answer it.
    The daemon still parses nothing it does not need: the JSON-RPC id (to answer with),
    the options the child *offered* (the only ids an answer may name), the tool call, and
    the ``_meta.sierralab`` view the child attached (pod, risk, the exact command or diff)."""

    session_id: str
    request_id: Any
    options: list[dict[str, Any]]
    tool_call_id: str | None
    title: str | None
    kind: str | None
    raw_input: Any
    meta: dict[str, Any]
    ts: float = field(default_factory=time.time)

    @property
    def key(self) -> str:
        return str(self.request_id)

    @property
    def public_id(self) -> str:
        """What a gRPC client sees. Session-qualified: JSON-RPC ids restart per child."""
        return f"{self.session_id}/{self.request_id}"


@dataclass
class Session:
    """One live child, and the frames waiting to reach it."""

    session_id: str
    cwd: str
    process: asyncio.subprocess.Process
    started: float = field(default_factory=time.time)
    #: Frames from the child that no client has claimed yet. Kept so a client
    #: that reconnects to a running session is not blind to what it missed —
    #: an approval prompt in particular, which the child has already emitted
    #: and is now blocked on. Frames a subscriber was handed never land here:
    #: replaying them would show a reattaching client prompts and answers it
    #: already dealt with.
    backlog: list[str] = field(default_factory=list)
    exited: bool = False
    #: The newest `_sierralab/lab` state the child sent (the Experimental Lab's
    #: pods, links, threat and HITL rows), kept so a remote canvas that attaches
    #: late is drawn from the current state rather than waiting for the next change.
    lab: dict[str, Any] | None = None
    #: Permission requests the child is waiting on, by JSON-RPC id (stringified).
    pending: dict[str, PendingPermission] = field(default_factory=dict)
    #: Requests already answered, id -> who ("gui" | "tui"), newest last and bounded. A late
    #: answer from the *other* front door for an id in here is dropped, not forwarded.
    answered: "OrderedDict[str, str]" = field(default_factory=OrderedDict)
    #: Requests the daemon itself sent the child (knobs, isolate), awaiting their response.
    calls: dict[str, "asyncio.Future[dict[str, Any]]"] = field(default_factory=dict)
    #: Who drives this child, when it is not a client on the WebSocket: `"discord"` for a conversation
    #: the Discord front door opened. An owned session has no subscriber by design, so nothing is
    #: backlogged for one (nobody will ever drain it), a directory lookup never hands it to a terminal
    #: (`find_by_cwd`), and the socket refuses to attach to it — two writers to one stdin.
    owner: str | None = None

    @property
    def alive(self) -> bool:
        return not self.exited and self.process.returncode is None

    def to_json(self) -> dict[str, Any]:
        return {
            "sessionId": self.session_id,
            "cwd": self.cwd,
            "pid": self.process.pid,
            "started": self.started,
            "alive": self.alive,
            "backlog": len(self.backlog),
            "owner": self.owner,
        }


class Supervisor:
    """Owns the child processes. One instance per daemon."""

    #: How many frames of a detached session's output to keep for a client that
    #: reconnects. A turn can emit hundreds of small updates; a few thousand is
    #: minutes of work, and a bounded list is what keeps a long-running session
    #: from growing without limit while nobody is watching.
    BACKLOG_LIMIT = 4000

    #: Frames a subscriber's queue will hold before the oldest are dropped.
    #: A client that stops reading must not be able to grow the daemon's memory
    #: without bound, and a client 2000 frames behind on a live session is
    #: better served by the backlog on reconnect than by an ever-growing queue.
    QUEUE_LIMIT = 2000

    #: The most one line of a child's stdout may hold. asyncio's StreamReader
    #: defaults to 64 KiB and `readline` raises past it, which would end `_pump`
    #: and report a healthy child as exited — and a file_read result or a diff
    #: is a single ACP line far bigger than that. Generous because the cost of
    #: the cap is a dropped session, and the cost of room is address space.
    FRAME_LIMIT = 64 * 1024 * 1024

    def __init__(self, *, entrypoint: Callable[[], list[str]] | None = None) -> None:
        self._sessions: dict[str, Session] = {}
        self._entrypoint = entrypoint or default_entrypoint
        self._subscribers: dict[str, asyncio.Queue[str]] = {}
        self._lab_watchers: set[asyncio.Queue[dict[str, Any]]] = set()
        self._listeners: dict[str, list[Callable[[dict[str, Any]], None]]] = {}
        self._call_ids = itertools.count(1)
        self.diffs: "OrderedDict[str, dict[str, Any]]" = OrderedDict()

    def subscribe(self, session_id: str) -> asyncio.Queue[str]:
        """A queue of every line this session emits from now on.

        One subscriber per session: a second client attaching to a session
        someone else is already driving would be two writers to one stdin, and
        ACP has no notion of that. The server refuses the second one."""
        queue: asyncio.Queue[str] = asyncio.Queue(maxsize=self.QUEUE_LIMIT)
        self._subscribers[session_id] = queue
        return queue

    def unsubscribe(self, session_id: str) -> None:
        """Detach the subscriber. Whatever it was handed but never sent goes back
        to the front of the backlog, so a frame that was in flight when the
        socket dropped is delivered to the next client rather than lost."""
        queue = self._subscribers.pop(session_id, None)
        session = self._sessions.get(session_id)
        if queue is None or session is None:
            return
        unsent: list[str] = []
        while True:
            try:
                unsent.append(queue.get_nowait())
            except asyncio.QueueEmpty:
                break
        if unsent:
            session.backlog[:0] = unsent
            if len(session.backlog) > self.BACKLOG_LIMIT:
                del session.backlog[: len(session.backlog) - self.BACKLOG_LIMIT]

    # ------------------------------------------------------------------- lab

    #: A frame is only parsed for the Lab when it carries this method name — the
    #: other 99.9% of lines (token deltas, tool updates) cost one substring test.
    LAB_MARK = '"_sierralab/lab"'
    LAB_QUEUE_LIMIT = 64

    def watch_lab(self) -> asyncio.Queue[dict[str, Any]]:
        """A read-only feed of Lab state changes, for any number of watchers.
        Unlike `subscribe` it never claims the session: a canvas looking at a
        session must not displace the terminal that is driving it."""
        queue: asyncio.Queue[dict[str, Any]] = asyncio.Queue(maxsize=self.LAB_QUEUE_LIMIT)
        self._lab_watchers.add(queue)
        return queue

    def unwatch_lab(self, queue: asyncio.Queue[dict[str, Any]]) -> None:
        self._lab_watchers.discard(queue)

    def lab_states(self) -> dict[str, dict[str, Any]]:
        return {sid: session.lab for sid, session in self._sessions.items() if session.lab is not None}

    def _tap_lab(self, session_id: str, line: str) -> None:
        """Remember and fan out a Lab state frame. The frame itself is still
        delivered to the client untouched (`_deliver` forwards the original bytes);
        this only reads a copy. Never raises: a frame it cannot parse is not its
        business, and the daemon's transport must not depend on it."""
        try:
            message = json.loads(line)
            params = message.get("params") if isinstance(message, dict) else None
            if not isinstance(params, dict) or message.get("method") != "_sierralab/lab":
                return
            session = self._sessions.get(session_id)
            if session is None:
                return
            session.lab = params
            frame = {"type": "state", "sessionId": session_id, "state": params}
            for queue in list(self._lab_watchers):
                try:
                    queue.put_nowait(frame)
                except asyncio.QueueFull:
                    # Only the newest state matters to a canvas: drop the oldest.
                    try:
                        queue.get_nowait()
                        queue.put_nowait(frame)
                    except (asyncio.QueueEmpty, asyncio.QueueFull):
                        pass
        except Exception:
            logger.debug("daemon: lab tap failed for %s", session_id, exc_info=True)

    # ------------------------------------------------------- listeners / taps

    PERMISSION_MARK = '"session/request_permission"'
    DIFF_MARK = '"_sierralab/diff"'
    MEMORY_MARK = '"_sierralab/memory"'
    TASK_MARK = '"_sierralab/task"'
    LAYA_MARK = '"_sierralab/laya"'
    UPDATE_MARK = '"session/update"'
    TURN_MARK = '"_sierralab/turn"'
    DAEMON_ID_PREFIX = "daemon-"
    MAX_DIFFS = 8
    MAX_ANSWERED = 256

    def listen(self, topic: str, callback: Callable[[dict[str, Any]], None]) -> Callable[[], None]:
        """Be told about ``intervention`` (requested / resolved), ``diff`` and ``memory``
        events, on the event loop. A callback that raises is logged and skipped."""
        self._listeners.setdefault(topic, []).append(callback)

        def stop() -> None:
            if callback in self._listeners.get(topic, []):
                self._listeners[topic].remove(callback)

        return stop

    def _notify(self, topic: str, payload: dict[str, Any]) -> None:
        for callback in list(self._listeners.get(topic, [])):
            try:
                callback(payload)
            except Exception:
                logger.debug("daemon: %s listener failed", topic, exc_info=True)

    def _tap_permission(self, session_id: str, line: str) -> None:
        try:
            message = json.loads(line)
            if not isinstance(message, dict) or message.get("method") != "session/request_permission" or message.get("id") is None:
                return
            session = self._sessions.get(session_id)
            params = message.get("params") or {}
            if session is None or not isinstance(params, dict):
                return
            tool_call = params.get("toolCall") or {}
            meta = (params.get("_meta") or {}).get("sierralab") or {}
            pending = PendingPermission(
                session_id=session_id,
                request_id=message["id"],
                options=[opt for opt in params.get("options") or [] if isinstance(opt, dict)],
                tool_call_id=tool_call.get("toolCallId"),
                title=tool_call.get("title"),
                kind=tool_call.get("kind"),
                raw_input=tool_call.get("rawInput"),
                meta=meta if isinstance(meta, dict) else {},
            )
            session.pending[pending.key] = pending
            self._notify("intervention", {"event": "requested", "pending": pending})
        except Exception:
            logger.debug("daemon: permission tap failed for %s", session_id, exc_info=True)

    def _tap_diff(self, session_id: str, line: str) -> None:
        try:
            message = json.loads(line)
            params = message.get("params") if isinstance(message, dict) else None
            if not isinstance(params, dict) or message.get("method") != "_sierralab/diff" or not params.get("id"):
                return
            entry = {**params, "session_id": session_id, "ts": time.time()}
            self.diffs[str(params["id"])] = entry
            while len(self.diffs) > self.MAX_DIFFS:
                self.diffs.popitem(last=False)
            self._notify("diff", entry)
        except Exception:
            logger.debug("daemon: diff tap failed for %s", session_id, exc_info=True)

    def _tap_task(self, session_id: str, line: str) -> None:
        """A harness task bar (``harness_events.Task``: the long step an agent is inside —
        a team plan, a self-heal attempt, a Claude Code run, an oMLX load). Remembered and
        fanned out, never consumed: a remote front door is what listens, and it is a no-op
        when nobody is."""
        try:
            message = json.loads(line)
            params = message.get("params") if isinstance(message, dict) else None
            if isinstance(params, dict) and message.get("method") == "_sierralab/task":
                self._notify("task", {**params, "session_id": session_id})
        except Exception:
            logger.debug("daemon: task tap failed for %s", session_id, exc_info=True)

    def _tap_laya(self, session_id: str, line: str) -> None:
        """Laya's live triage line for a turn (route, risk, destructiveness). Fanned out to
        listeners so a remote front door can show it beside the session it belongs to."""
        try:
            message = json.loads(line)
            params = message.get("params") if isinstance(message, dict) else None
            if isinstance(params, dict) and message.get("method") == "_sierralab/laya":
                self._notify("laya", {**params, "session_id": session_id})
        except Exception:
            logger.debug("daemon: laya tap failed for %s", session_id, exc_info=True)

    def _tap_update(self, session_id: str, line: str) -> None:
        """An ACP ``session/update`` (a chunk of the agent's reply, a tool call starting or ending).
        Fanned out only to a listener that asked for it — the Discord front door, to stream a reply —
        and never consumed: the frame still goes to whoever owns the session."""
        try:
            message = json.loads(line)
            params = message.get("params") if isinstance(message, dict) else None
            if isinstance(params, dict) and message.get("method") == "session/update" and isinstance(params.get("update"), dict):
                self._notify("update", {"session_id": session_id, "acp_session_id": params.get("sessionId"), "update": params["update"]})
        except Exception:
            logger.debug("daemon: update tap failed for %s", session_id, exc_info=True)

    def _tap_turn(self, session_id: str, line: str) -> None:
        """What a finished turn cost (``_sierralab/turn``: seconds, tool calls, tokens in and out)."""
        try:
            message = json.loads(line)
            params = message.get("params") if isinstance(message, dict) else None
            if isinstance(params, dict) and message.get("method") == "_sierralab/turn":
                self._notify("turn", {**params, "session_id": session_id})
        except Exception:
            logger.debug("daemon: turn tap failed for %s", session_id, exc_info=True)

    def _tap_memory(self, session_id: str, line: str) -> None:
        try:
            message = json.loads(line)
            params = message.get("params") if isinstance(message, dict) else None
            if isinstance(params, dict) and message.get("method") == "_sierralab/memory":
                self._notify("memory", {**params, "session_id": session_id})
        except Exception:
            logger.debug("daemon: memory tap failed for %s", session_id, exc_info=True)

    def _tap_call_response(self, session: Session, line: str) -> bool:
        """Is this the child's answer to a request the *daemon* sent? Resolves the waiter and
        returns True — the frame is the daemon's, not the client's, so it is not forwarded."""
        try:
            message = json.loads(line)
        except ValueError:
            return False
        if not isinstance(message, dict) or "method" in message:
            return False
        waiter = session.calls.get(str(message.get("id")))
        if waiter is None:
            return False
        if not waiter.done():
            waiter.set_result(message)
        return True

    def all_pending(self) -> list[PendingPermission]:
        return sorted((p for session in self._sessions.values() for p in session.pending.values()), key=lambda p: p.ts)

    def live_cwds(self) -> set[str]:
        return {session.cwd for session in self._sessions.values() if session.alive}

    def _settle(self, session: Session, key: str, outcome: str, decided_by: str) -> PendingPermission | None:
        pending = session.pending.pop(key, None)
        if pending is None:
            return None
        session.answered[key] = decided_by
        while len(session.answered) > self.MAX_ANSWERED:
            session.answered.popitem(last=False)
        self._notify("intervention", {"event": "resolved", "pending": pending, "outcome": outcome, "decided_by": decided_by})
        return pending

    def _admit_client_frame(self, session: Session, line: str) -> bool:
        """The driving client (a TUI over the WebSocket) is about to write ``line`` to the
        child. Returns False when it is an answer the GUI already gave: forwarding a second
        response to one request would reach a child that has moved on.

        Also the place the daemon learns the *driver* answered first (so the GUI's modal can
        close) or cancelled the turn (so a pause that will never be answered is not left open)."""
        try:
            message = json.loads(line)
        except ValueError:
            return True
        if not isinstance(message, dict):
            return True
        if message.get("method") == "session/cancel":
            for key in list(session.pending):
                self._settle(session, key, "aborted", "tui")
            return True
        if "method" in message or "id" not in message:
            return True
        key = str(message["id"])
        if key in session.pending:
            self._settle(session, key, _outcome_of(message, session.pending[key]), "tui")
            return True
        # Dropped when *another front door* already answered it — the GUI's modal or the Discord
        # bot, whichever was first. (This once only knew "gui", so a late terminal answer to a
        # prompt Discord had settled was forwarded to a child that had already moved on.)
        return session.answered.get(key) in (None, "tui")

    async def answer_permission(self, session_id: str, key: str, approve: bool, *, decided_by: str = "gui", always: bool = False) -> PendingPermission:
        """Relay a human's choice into the paused ACP request, as the child's own client would.

        Raises ``LookupError`` when there is nothing pending under that id (already answered by
        the other front door, or the session ended) and ``ValueError`` when the child offered no
        option of the kind asked for — the answer is only ever one of the child's own options,
        never an id the caller made up.

        ``always`` picks the child's own ``allow_always`` option ("always allow <rule>") instead of
        ``allow_once`` — only ever an option the child offered, and the child offers none for a call
        the gate armed, so an always-allow can never be made for something destructive.

        ``decided_by`` names the front door that answered. More than one can be attached now
        (the GUI's containment modal, a TUI, the Discord bot), so which one a person used is
        recorded rather than assumed — a decision attributed to the wrong door is a decision
        nobody can account for afterwards."""
        session = self._sessions.get(session_id)
        if session is None or key not in session.pending:
            raise LookupError(f"no pending permission {key!r} in session {session_id!r}")
        pending = session.pending[key]
        wanted = ("allow_always" if always else "allow_once") if approve else "reject_once"
        option = next((o for o in pending.options if o.get("kind") == wanted), None)
        if option is None or not option.get("optionId"):
            raise ValueError(f"the session offered no {wanted} option")
        response = {
            "jsonrpc": "2.0",
            "id": pending.request_id,
            "result": {
                "outcome": {"outcome": "selected", "optionId": option["optionId"]},
                # The child cannot tell which front door answered — every answer arrives as the
                # same JSON-RPC response — so the door is named here, in ACP's `_meta` slot, and
                # the child puts it on the intervention ledger. Without this a decision made on a
                # phone reads as a person at the GUI, which is an audit trail nobody can trust.
                "_meta": {"sierralab": {"decidedBy": decided_by}},
            },
        }
        if not await self.write_raw(session_id, json.dumps(response)):
            raise LookupError(f"session {session_id!r} has exited")
        self._settle(session, key, "approved" if approve else "rejected", decided_by)
        # Tell the driving client its prompt is moot. It ignores a method it does not know;
        # a client that knows it can dismiss the prompt instead of leaving it to be answered late.
        self._deliver(
            session_id,
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "method": "_sierralab/resolved",
                    "params": {"sessionId": session_id, "requestId": pending.request_id, "toolCallId": pending.tool_call_id, "outcome": "approved" if approve else "rejected", "decidedBy": decided_by},
                }
            ),
        )
        return pending

    async def call(
        self, session_id: str, method: str, params: dict[str, Any] | None = None, *, timeout: float = 5.0, qualify: bool = True
    ) -> dict[str, Any]:
        """Send the child a request of the daemon's own and wait for its answer (the Lab's
        ``_sierralab/lab/*`` controls). The answer is returned; it never reaches a client.

        ``qualify`` adds the daemon's session id to the params, which the ``_sierralab/*``
        extension methods read. The core ACP methods (``initialize``, ``session/new``,
        ``session/prompt``) are typed and take exactly their own fields, so a caller driving
        a session — the Discord conversation — passes ``qualify=False`` and its own ids."""
        session = self._sessions.get(session_id)
        if session is None or not session.alive:
            raise LookupError(f"no live session {session_id!r}")
        ident = f"{self.DAEMON_ID_PREFIX}{next(self._call_ids)}"
        waiter: asyncio.Future[dict[str, Any]] = asyncio.get_running_loop().create_future()
        session.calls[ident] = waiter
        try:
            body = {"sessionId": session_id, **(params or {})} if qualify else dict(params or {})
            request = {"jsonrpc": "2.0", "id": ident, "method": method, "params": body}
            if not await self.write_raw(session_id, json.dumps(request)):
                raise LookupError(f"session {session_id!r} has exited")
            return await asyncio.wait_for(waiter, timeout)
        finally:
            session.calls.pop(ident, None)

    async def notify_child(self, session_id: str, method: str, params: dict[str, Any] | None = None) -> bool:
        """A JSON-RPC *notification* to the child (no id, no answer): ``session/cancel``."""
        return await self.write_raw(session_id, json.dumps({"jsonrpc": "2.0", "method": method, "params": params or {}}))

    # ------------------------------------------------------------- lifecycle

    async def spawn(self, cwd: str, *, session_id: str | None = None, owner: str | None = None) -> Session:
        """Start one session process.

        `session_id` may be supplied to reattach the daemon's bookkeeping to a
        session the child already knows about (a `--resume`); otherwise the
        daemon mints one and the child adopts it from the first frame, since
        ACP's `session/new` is the client's call, not the daemon's.
        """
        ident = session_id or str(uuid.uuid4())
        if ident in self._sessions and self._sessions[ident].alive:
            return self._sessions[ident]
        target = str(Path(cwd).expanduser().resolve())
        if not Path(target).is_dir():
            raise ValueError(f"no such directory: {target!r}")
        argv = self._entrypoint()
        logger.info("daemon: spawning session %s in %s", ident, target)
        process = await asyncio.create_subprocess_exec(
            *argv,
            cwd=target,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=None,  # the child's logs go to the daemon's own stderr
            env=self._child_env(),
            limit=self.FRAME_LIMIT,
            # Its own process group: cancelling a turn must not signal the
            # daemon, and shutting the daemon down must be able to reach every
            # child at once with one killpg.
            start_new_session=True,
        )
        session = Session(ident, target, process, owner=owner)
        self._sessions[ident] = session
        asyncio.create_task(self._pump(ident), name=f"daemon-pump-{ident}")
        return session

    @staticmethod
    def _child_env() -> dict[str, str]:
        """The daemon's environment, minus what the daemon itself is for.

        `PYTHONUNBUFFERED` is the one real addition: the child writes
        newline-delimited JSON to a pipe, and a block-buffered stdout would
        hold a permission prompt until the pipe filled — which, for a prompt
        the child is blocked on, is forever."""
        env = dict(os.environ)
        env["PYTHONUNBUFFERED"] = "1"
        return env

    async def _pump(self, session_id: str) -> None:
        """Read the child's stdout, line by line, until it exits.

        Read with `readline` rather than `read(n)`: the child's protocol is
        newline-delimited, and a chunked read would hand this loop half a JSON
        object, which it would then forward as a frame the client cannot parse.
        """
        session = self._sessions.get(session_id)
        if session is None or session.process.stdout is None:
            return
        stream = session.process.stdout
        try:
            while True:
                raw = await stream.readline()
                if not raw:
                    break
                line = raw.decode("utf-8", errors="replace").rstrip("\r\n")
                if not line:
                    continue
                if len(line) > 1_000_000:
                    logger.warning("daemon: %s emitted a %d-byte frame", session_id, len(line))
                self._deliver(session_id, line)
        except asyncio.CancelledError:
            raise
        except Exception:
            logger.exception("daemon: reading session %s failed", session_id)
        finally:
            session.exited = True
            for key in list(session.pending):
                self._settle(session, key, "aborted", "session-ended")
            for waiter in list(session.calls.values()):
                if not waiter.done():
                    # An exception, not `cancel()`: a cancelled future makes `wait_for` raise
                    # CancelledError into the *caller*, which reads as the caller's own task being
                    # cancelled. `call` documents LookupError for a session that is gone.
                    waiter.set_exception(LookupError(f"session {session_id!r} exited"))
            logger.info("daemon: session %s exited (rc=%s)", session_id, session.process.returncode)
            self._notify("session", {"event": "exited", "session_id": session_id, "returncode": session.process.returncode, "owner": session.owner})
            self._deliver(session_id, json.dumps({
                "jsonrpc": "2.0",
                "method": "_sierralab/daemon",
                "params": {"sessionId": session_id, "state": "exited", "returncode": session.process.returncode},
            }))

    def _deliver(self, session_id: str, line: str) -> None:
        """Hand a line to whoever is listening, or remember it for later.

        The backlog is what makes reattaching work. A client that drops while
        the child is parked on an approval prompt has missed the prompt itself,
        and on reconnect there is nothing to re-request it with — the child
        will not re-emit it, it is waiting for the answer. It holds only what
        no one has been handed: a frame a client already received is that
        client's, and replaying it to the next would re-ask answered prompts."""
        if self.LAB_MARK in line:
            self._tap_lab(session_id, line)
        if self.PERMISSION_MARK in line:
            self._tap_permission(session_id, line)
        if self.DIFF_MARK in line:
            self._tap_diff(session_id, line)
        if self.MEMORY_MARK in line:
            self._tap_memory(session_id, line)
        if self.TASK_MARK in line:
            self._tap_task(session_id, line)
        if self.LAYA_MARK in line:
            self._tap_laya(session_id, line)
        if self.TURN_MARK in line:
            self._tap_turn(session_id, line)
        if self.UPDATE_MARK in line and self._listeners.get("update"):
            self._tap_update(session_id, line)
        owner = self._sessions.get(session_id)
        if owner is not None and owner.calls and self.DAEMON_ID_PREFIX in line and self._tap_call_response(owner, line):
            return
        queue = self._subscribers.get(session_id)
        if queue is None:
            session = self._sessions.get(session_id)
            if session is not None and session.owner is None:
                session.backlog.append(line)
                if len(session.backlog) > self.BACKLOG_LIMIT:
                    del session.backlog[: len(session.backlog) - self.BACKLOG_LIMIT]
            return
        try:
            queue.put_nowait(line)
        except asyncio.QueueFull:
            # Drop the oldest rather than the newest: the client is behind, and
            # the frame it most needs is the one that just arrived.
            try:
                queue.get_nowait()
                queue.put_nowait(line)
            except (asyncio.QueueEmpty, asyncio.QueueFull):
                logger.warning("daemon: %s subscriber queue overflowed", session_id)

    async def write(self, session_id: str, line: str) -> bool:
        """A line from the *driving client*, to the child's stdin. False if there is no child.
        True (and nothing sent) for an answer the GUI already gave to the same request."""
        session = self._sessions.get(session_id)
        if session is not None and (session.pending or session.answered) and not self._admit_client_frame(session, line):
            return session.alive
        return await self.write_raw(session_id, line)

    async def write_raw(self, session_id: str, line: str) -> bool:
        """Send one line to a child's stdin, no questions asked. False if there is no child."""
        from . import frames  # noqa: PLC0415 — avoids a cycle with frames' own imports

        session = self._sessions.get(session_id)
        if session is None or session.process.stdin is None or not session.alive:
            return False
        session.process.stdin.write(frames.forward_line(line))
        await session.process.stdin.drain()
        return True

    def backlog(self, session_id: str) -> list[str]:
        session = self._sessions.get(session_id)
        return list(session.backlog) if session else []

    def find_by_cwd(self, cwd: str) -> Session | None:
        """The session already running in this directory, if there is one.

        Directory, not session id, is what a reconnecting client knows. The
        comparison is against the resolved path the session was spawned with,
        because two spellings of one directory are one directory.
        """
        try:
            target = str(Path(cwd).expanduser().resolve())
        except (OSError, RuntimeError, ValueError):  # unresolvable, or a NUL byte in it
            return None
        for session in self._sessions.values():
            if session.cwd == target and session.owner is None:
                return session
        return None

    def session(self, session_id: str) -> Session | None:
        """The live session under this id, for a front door that needs its cwd or liveness
        (the Discord bot reads a project's settings to know what its destructive line is)."""
        return self._sessions.get(session_id)

    def drain_backlog(self, session_id: str) -> list[str]:
        """Take the backlog and clear it, for handing to an attaching client."""
        session = self._sessions.get(session_id)
        if session is None:
            return []
        held, session.backlog = session.backlog, []
        return held

    # ------------------------------------------------------------- shutdown

    async def stop(self, session_id: str, *, grace: float = 5.0) -> bool:
        """Terminate one session, its process group included.

        SIGTERM the group, then SIGKILL it if it is still there. The group, not
        the process: a session's tools spawn subprocesses (builds, test runs),
        and a killed shell whose children survive would leak precisely the
        long-running commands this is meant to stop."""
        session = self._sessions.pop(session_id, None)
        if session is None:
            return False
        return await self._terminate(session.process, grace=grace)

    async def stop_all(self, *, grace: float = 5.0) -> None:
        for session_id in list(self._sessions):
            await self.stop(session_id, grace=grace)

    @staticmethod
    async def _terminate(process: asyncio.subprocess.Process, *, grace: float) -> bool:
        if process.returncode is not None:
            return True
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except (ProcessLookupError, PermissionError):
            process.terminate()
        try:
            await asyncio.wait_for(process.wait(), timeout=grace)
            return True
        except asyncio.TimeoutError:
            logger.warning("daemon: session pid %s ignored SIGTERM; killing", process.pid)
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                process.kill()
            try:
                await asyncio.wait_for(process.wait(), timeout=grace)
            except asyncio.TimeoutError:
                logger.error("daemon: session pid %s survived SIGKILL", process.pid)
                return False
            return True

    def sessions(self) -> list[dict[str, Any]]:
        return [session.to_json() for session in self._sessions.values()]


def _outcome_of(response: dict[str, Any], pending: PendingPermission) -> str:
    """How the driving client answered a permission request: approved / rejected / aborted."""
    outcome = ((response.get("result") or {}).get("outcome") or {}) if isinstance(response.get("result"), dict) else {}
    if outcome.get("outcome") != "selected":
        return "aborted"
    chosen = next((o for o in pending.options if o.get("optionId") == outcome.get("optionId")), None)
    return "approved" if chosen is not None and str(chosen.get("kind", "")).startswith("allow") else "rejected"

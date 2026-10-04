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
import json
import logging
import os
import signal
import sys
import time
import uuid
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
class Session:
    """One live child, and the frames waiting to reach it."""

    session_id: str
    cwd: str
    process: asyncio.subprocess.Process
    started: float = field(default_factory=time.time)
    #: Frames the daemon has been asked to send but the child has not read yet.
    #: Only the WebSocket task writes to it, so it needs no lock of its own.
    pending: list[bytes] = field(default_factory=list)
    #: Frames from the child that no client has claimed yet. Kept so a client
    #: that reconnects to a running session is not blind to what it missed —
    #: an approval prompt in particular, which the child has already emitted
    #: and is now blocked on.
    backlog: list[str] = field(default_factory=list)
    exited: bool = False

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

    def __init__(self, *, entrypoint: Callable[[], list[str]] | None = None) -> None:
        self._sessions: dict[str, Session] = {}
        self._entrypoint = entrypoint or default_entrypoint
        self._subscribers: dict[str, asyncio.Queue[str]] = {}

    def subscribe(self, session_id: str) -> asyncio.Queue[str]:
        """A queue of every line this session emits from now on.

        One subscriber per session: a second client attaching to a session
        someone else is already driving would be two writers to one stdin, and
        ACP has no notion of that. The server refuses the second one."""
        queue: asyncio.Queue[str] = asyncio.Queue(maxsize=self.QUEUE_LIMIT)
        self._subscribers[session_id] = queue
        return queue

    def unsubscribe(self, session_id: str) -> None:
        self._subscribers.pop(session_id, None)

    # ------------------------------------------------------------- lifecycle

    async def spawn(self, cwd: str, *, session_id: str | None = None) -> Session:
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
            # Its own process group: cancelling a turn must not signal the
            # daemon, and shutting the daemon down must be able to reach every
            # child at once with one killpg.
            start_new_session=True,
        )
        session = Session(ident, target, process)
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
            logger.info("daemon: session %s exited (rc=%s)", session_id, session.process.returncode)
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
        will not re-emit it, it is waiting for the answer."""
        session = self._sessions.get(session_id)
        if session is not None:
            session.backlog.append(line)
            if len(session.backlog) > self.BACKLOG_LIMIT:
                del session.backlog[: len(session.backlog) - self.BACKLOG_LIMIT]
        queue = self._subscribers.get(session_id)
        if queue is None:
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
        """Send one line to a child's stdin. False if there is no child."""
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
        except (OSError, RuntimeError):  # a path that cannot even be resolved
            return None
        for session in self._sessions.values():
            if session.cwd == target:
                return session
        return None

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

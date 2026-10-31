"""Background shell tasks: start a long command, keep working, read its tail later, kill it as a tree.

``shell`` blocks the turn until its command ends (or its timeout kills it). A dev server, a watch build or a 20-minute
test suite should not: ``background_shell`` starts the command detached and returns its task id at once;
``task_output`` reads its status and the tail of what it printed; ``task_kill`` ends it. The person sees the same tasks
through ``_cordata/tasks/{list,status,kill}`` (acp_server.py) in acp-tui and Cordata.app.

The shape follows what the established CLIs converged on (``docs/rea_competitive_intelligence.md``): a task id the model
holds, a bounded read-back window rather than the whole log, a per-task timeout, the whole process group killed (a
build's compiler children included), and an exit notice when it ends.

* **Own process group.** ``start_new_session=True``; a kill is ``SIGTERM`` to the group, a grace period, then ``SIGKILL``.
  A task's timeout (default one hour, at most ``MAX_TIMEOUT_S``) does the same.
* **Bounded memory.** Output is kept as the first ``HEAD_LINES`` lines plus a ring of the last ``TAIL_LINES`` (and at most
  ``MAX_LINE_CHARS`` per line), so a chatty server cannot grow the process; the read-back says what was dropped.
* **Durable summary.** Each task writes ``<CORDATA_HOME>/tasks/<id>.json`` (0600 — build logs can hold secrets) with its
  command, state, exit code and tail, so a person can see what a task did after the session that ran it is gone.
* **Guarded like shell.** The tool is a different name for the same act, so it is gated identically: approval
  (``acp_approval``), Laya Guard (``laya_guard`` reads it as ``shell``), permission rules (``settings_store.TOOL_ALIASES``:
  ``shell(git *)`` covers it) and checkpoints (it is a possible writer). Plan mode refuses it.
* **Ids are validated.** A task id is 12 hex characters and must be one this process started; nothing reads or kills
  a path or a pid a caller names.
"""

from __future__ import annotations

import collections
import json
import logging
import os
import re
import signal
import subprocess
import threading
import time
import uuid
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from strands import tool

logger = logging.getLogger(__name__)

DEFAULT_TIMEOUT_S = 3600
MAX_TIMEOUT_S = 6 * 3600
MAX_TASKS = 8
HEAD_LINES = 40
TAIL_LINES = 400
MAX_LINE_CHARS = 2000
KILL_GRACE_S = 3.0
_ID = re.compile(r"^[0-9a-f]{12}$")


def _home() -> Path:
    return Path(os.environ.get("CORDATA_HOME") or Path.home() / ".cordata").expanduser()


@dataclass
class Task:
    task_id: str
    command: str
    cwd: str
    timeout_s: float
    started: float = field(default_factory=time.time)
    ended: float | None = None
    state: str = "running"  # running | exited | killed | timed_out | failed
    exit_code: int | None = None
    head: list[str] = field(default_factory=list)
    tail: collections.deque = field(default_factory=lambda: collections.deque(maxlen=TAIL_LINES))
    lines: int = 0
    session_id: str | None = None
    proc: subprocess.Popen | None = field(default=None, repr=False)
    lock: threading.Lock = field(default_factory=threading.Lock, repr=False)

    @property
    def pid(self) -> int | None:
        return self.proc.pid if self.proc is not None else None

    def add(self, line: str) -> None:
        line = line.rstrip("\n")
        if len(line) > MAX_LINE_CHARS:
            line = line[:MAX_LINE_CHARS] + " …"
        with self.lock:
            self.lines += 1
            if len(self.head) < HEAD_LINES:
                self.head.append(line)
            else:
                self.tail.append(line)

    def output(self, last: int | None = None) -> str:
        """The read-back window: the head, a marker for what was dropped, the tail (or only its last ``last`` lines)."""
        with self.lock:
            head, tail, total = list(self.head), list(self.tail), self.lines
        if last is not None:
            joined = head + tail
            dropped = max(0, total - min(len(joined), last))
            body = joined[-last:] if last > 0 else []
            return (f"[… {dropped} earlier line(s) not shown]\n" if dropped else "") + "\n".join(body)
        dropped = total - len(head) - len(tail)
        return "\n".join(head + ([f"[… {dropped} line(s) dropped: only the first {HEAD_LINES} and last {TAIL_LINES} are kept]"] if dropped > 0 else []) + tail)

    def to_json(self, *, tail: int | None = 40) -> dict[str, Any]:
        return {
            "taskId": self.task_id, "command": self.command, "cwd": self.cwd, "state": self.state, "exitCode": self.exit_code,
            "started": self.started, "ended": self.ended, "elapsedS": round((self.ended or time.time()) - self.started, 1),
            "lines": self.lines, "pid": self.pid, "sessionId": self.session_id,
            **({"output": self.output(last=tail)} if tail is not None else {}),
        }


class TaskManager:
    """Every background task this process started. One per process (``MANAGER``); thread-safe."""

    def __init__(self, store: Path | None = None) -> None:
        self._tasks: dict[str, Task] = {}
        self._lock = threading.Lock()
        self._store = store
        self.listeners: list[Callable[[dict[str, Any]], None]] = []

    @property
    def store(self) -> Path:
        return self._store or (_home() / "tasks")

    # ---------------------------------------------------------------- lifecycle

    def start(self, command: str, cwd: str, *, timeout_s: float | None = None, session_id: str | None = None, env: dict[str, str] | None = None) -> Task:
        if not command.strip():
            raise ValueError("an empty command")
        with self._lock:
            running = [t for t in self._tasks.values() if t.state == "running"]
            if len(running) >= MAX_TASKS:
                raise RuntimeError(f"{len(running)} background tasks are already running (the limit is {MAX_TASKS}); kill one first")
            task = Task(uuid.uuid4().hex[:12], command, os.path.realpath(cwd), max(1.0, min(float(timeout_s or DEFAULT_TIMEOUT_S), MAX_TIMEOUT_S)), session_id=session_id)
            self._tasks[task.task_id] = task
        try:
            task.proc = subprocess.Popen(
                ["/bin/sh", "-c", command], cwd=task.cwd, env={**os.environ, **(env or {})},
                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True, text=True, errors="replace", bufsize=1,
            )
        except OSError as error:
            task.state, task.ended = "failed", time.time()
            task.add(f"[could not start: {error}]")
            self._persist(task)
            return task
        threading.Thread(target=self._pump, args=(task,), name=f"task-{task.task_id}", daemon=True).start()
        threading.Thread(target=self._watch, args=(task,), name=f"task-timeout-{task.task_id}", daemon=True).start()
        self._persist(task)
        return task

    def _pump(self, task: Task) -> None:
        assert task.proc is not None and task.proc.stdout is not None
        for line in task.proc.stdout:
            task.add(line)
        code = task.proc.wait()
        with task.lock:
            task.exit_code = code
            task.ended = time.time()
            if task.state == "running":
                task.state = "exited"
        self._persist(task)
        self._notify(task)

    def _watch(self, task: Task) -> None:
        assert task.proc is not None
        try:
            task.proc.wait(timeout=task.timeout_s)
        except subprocess.TimeoutExpired:
            with task.lock:
                task.state = "timed_out"
            task.add(f"[timed out after {task.timeout_s:g}s: the process group was killed]")
            self._kill_group(task)

    def _kill_group(self, task: Task) -> None:
        proc = task.proc
        if proc is None or proc.poll() is not None:
            return
        try:
            pgid = os.getpgid(proc.pid)
        except ProcessLookupError:
            return
        for sig, wait in ((signal.SIGTERM, KILL_GRACE_S), (signal.SIGKILL, 2.0)):
            try:
                os.killpg(pgid, sig)
            except (ProcessLookupError, PermissionError):
                return
            try:
                proc.wait(timeout=wait)
                return
            except subprocess.TimeoutExpired:
                continue

    def kill(self, task_id: str) -> dict[str, Any]:
        task = self.get(task_id)
        if task is None:
            return {"ok": False, "reason": f"no background task {task_id!r}"}
        if task.state != "running":
            return {"ok": True, "already": task.state, **task.to_json(tail=None)}
        with task.lock:
            task.state = "killed"
        self._kill_group(task)
        return {"ok": True, **task.to_json(tail=None)}

    def shutdown(self) -> None:
        for task in list(self._tasks.values()):
            if task.state == "running":
                self.kill(task.task_id)

    # ---------------------------------------------------------------- reading

    def get(self, task_id: str) -> Task | None:
        if not isinstance(task_id, str) or not _ID.match(task_id):
            return None
        with self._lock:
            return self._tasks.get(task_id)

    def list(self, session_id: str | None = None) -> list[dict[str, Any]]:
        with self._lock:
            tasks = list(self._tasks.values())
        return [t.to_json(tail=None) for t in sorted(tasks, key=lambda t: -t.started) if session_id is None or t.session_id in (None, session_id)]

    # ---------------------------------------------------------------- bookkeeping

    def _persist(self, task: Task) -> None:
        try:
            self.store.mkdir(parents=True, exist_ok=True)
            path = self.store / f"{task.task_id}.json"
            tmp = path.with_suffix(".tmp")
            fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
            with os.fdopen(fd, "w") as handle:
                json.dump(task.to_json(tail=200), handle)
            os.replace(tmp, path)
        except OSError:
            logger.debug("background task %s: could not write its summary", task.task_id, exc_info=True)

    def _notify(self, task: Task) -> None:
        event = {"event": "task_exit", **task.to_json(tail=20)}
        for listener in list(self.listeners):
            try:
                listener(event)
            except Exception:
                logger.debug("background task listener failed", exc_info=True)


MANAGER = TaskManager()

BACKGROUND_DESCRIPTION = (
    "Starts a long-running shell command in the background (a dev server, a watch build, a long test suite) and returns "
    "its task id immediately, so you can keep working. Read its status and the tail of its output with task_output; end "
    "it with task_kill. Use the ordinary shell tool for anything that finishes in a few minutes."
)


@tool(name="background_shell", description=BACKGROUND_DESCRIPTION)
def background_shell(command: str, timeout: int = DEFAULT_TIMEOUT_S) -> str:
    """Args:
        command: the shell command, run with `sh -c` in the project directory.
        timeout: seconds before the task's whole process group is killed (default 3600).
    """
    try:
        task = MANAGER.start(command, os.getcwd(), timeout_s=timeout)
    except (RuntimeError, ValueError) as error:
        return f"Not started: {error}"
    if task.state == "failed":
        return f"Not started: {task.output()}"
    return f"Started background task {task.task_id} (pid {task.pid}, timeout {task.timeout_s:g}s). Check it with task_output(task_id=\"{task.task_id}\")."


@tool(name="task_output", description="Status and the last lines of output of a background task started with background_shell.")
def task_output(task_id: str, lines: int = 60) -> str:
    """Args:
        task_id: the id background_shell returned.
        lines: how many of the last output lines to show (default 60).
    """
    task = MANAGER.get(task_id)
    if task is None:
        known = ", ".join(t["taskId"] for t in MANAGER.list()[:8]) or "none"
        return f"No background task {task_id!r} (known: {known})."
    info = task.to_json(tail=max(0, min(int(lines), TAIL_LINES + HEAD_LINES)))
    exit_text = f", exit code {info['exitCode']}" if info["exitCode"] is not None else ""
    return f"Task {task_id}: {info['state']}{exit_text}, {info['elapsedS']}s, {info['lines']} line(s).\n{info['output']}"


@tool(name="task_kill", description="Ends a background task (and every process it started).")
def task_kill(task_id: str) -> str:
    """Args:
        task_id: the id background_shell returned.
    """
    result = MANAGER.kill(task_id)
    if not result.get("ok"):
        return str(result.get("reason"))
    return f"Task {task_id}: {result.get('already') or result.get('state')}."


TOOLS = [background_shell, task_output, task_kill]

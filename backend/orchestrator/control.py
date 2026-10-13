"""Orchestrator runs as something a front door can see and steer (docs/app.md §6.2): list, show, read artifacts, start
a run that outlives the session that started it, pause or cancel it at a safe point, resume it.

Stdlib plus ``state_store`` and the run record: importable by the daemon without an agent.

**A run is a detached process.** ``spawn_run`` starts ``python -m orchestrator run … --json`` in its own process group
(``start_new_session``), writing its NDJSON events to ``<run>/events.ndjson`` and its pid to ``<run>/host.json`` in
the run's artifacts folder (``~/.cordata/orchestrator/<run_id>/``, 0700). Nothing ties it to the process that started
it: the daemon reaping an idle session child does not stop a run, and any front door can attach later by reading the
events file from an offset.

**Control is a file.** ``request(run_id, "pause" | "cancel", now=…, restore=…)`` appends one JSON line to
``<run>/control.ndjson``; the run polls it (``Controls``) and acts at its next safe point — between phases, between
waves, before each worker call. ``now`` also stops the worker call in flight (its process group is killed, its step
rolled back). A file rather than a pipe so the controller need not be the process that spawned the run.

**Reads are bounded.** ``artifacts_read`` takes a name only from ``artifacts_list`` and a path only under the run's
folder (realpath), and returns text through ``secret_patterns.scrub``.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import threading
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import secret_patterns
import state_store

BACKEND_DIR = Path(__file__).resolve().parent.parent
TERMINAL = frozenset({"done", "failed", "budget_exhausted", "cancelled"})
#: Text an ``artifacts/read`` returns per call.
READ_LIMIT = 64 * 1024
#: Artifact files a run may hold that are offered for reading (others in the folder are bookkeeping).
READABLE_SUFFIXES = (".md", ".patch", ".json", ".ndjson", ".txt", ".log")


def run_dir(run_id: str) -> Path:
    from .machine import artifacts_dir

    return artifacts_dir(run_id)


def _valid_run_id(run_id: str) -> bool:
    return bool(run_id) and all(c.isalnum() or c in "-_" for c in run_id) and len(run_id) <= 64


# ------------------------------------------------------------------------------------------------------- reading


def _alive(pid: int | None) -> bool:
    if not pid:
        return False
    try:
        os.kill(int(pid), 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except (OSError, ValueError):
        return False
    return True


def host(run_id: str) -> dict[str, Any]:
    """``host.json`` (pid, started, argv, session) — ``{}`` when the run was not started detached."""
    try:
        return json.loads((run_dir(run_id) / "host.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}


def liveness(record_state: str, run_id: str) -> str:
    """``running`` (its process is alive), ``interrupted`` (non-terminal but its process is gone — resumable),
    ``paused``, or the terminal state."""
    if record_state in TERMINAL:
        return record_state
    info = host(run_id)
    # The process first: a run just resumed still reads `paused` until its new process writes its first transition,
    # and that window must not let a second resume start a second driver.
    if info.get("pid") and _alive(info.get("pid")):
        return "running"
    if record_state == "paused":
        return "paused"
    return "interrupted" if info.get("pid") else record_state


def _row(run_id: str, state: str, cwd: str, goal: str, created: float, updated: float, session_id: str | None) -> dict[str, Any]:
    return {
        "runId": run_id,
        "state": state,
        "live": liveness(state, run_id),
        "cwd": cwd,
        "goal": secret_patterns.scrub(goal)[0][:400],
        "created": created,
        "updated": updated,
        "sessionId": session_id,
    }


def list_runs(*, cwd: str | None = None, session_id: str | None = None, states: list[str] | None = None, limit: int = 30, before: float | None = None) -> list[dict[str, Any]]:
    """Runs newest first; ``cwd`` by realpath, ``before`` an ``updated`` timestamp to page back."""
    clauses, params = [], []
    if cwd:
        clauses.append("cwd = ?")
        params.append(os.path.realpath(cwd))
    if states:
        clauses.append(f"state IN ({','.join('?' * len(states))})")
        params.extend(states)
    if before is not None:
        clauses.append("updated_ts < ?")
        params.append(float(before))
    where = f"WHERE {' AND '.join(clauses)}" if clauses else ""
    try:
        rows = state_store.connect().execute(
            f"SELECT run_id, state, cwd, goal, created_ts, updated_ts, record_json FROM orch_runs {where} ORDER BY updated_ts DESC LIMIT ?",
            (*params, max(1, min(int(limit), 200))),
        ).fetchall()
    except Exception:
        return []
    out = []
    for row in rows:
        try:
            sid = json.loads(row["record_json"]).get("session_id")
        except (TypeError, ValueError):
            sid = None
        if session_id and sid != session_id:
            continue
        out.append(_row(row["run_id"], row["state"], row["cwd"], row["goal"], row["created_ts"], row["updated_ts"], sid))
    return out


def show_run(run_id: str) -> dict[str, Any] | None:
    """One run, for Mission Control: the plan's steps with their workers, per-pod usage, handoffs, artifacts, what can
    be done to it now."""
    from .machine import load

    record = load(run_id)
    if record is None:
        return None
    live = liveness(record.state, run_id)
    steps = []
    for step in (record.plan or {}).get("steps") or []:
        steps.append({k: step.get(k) for k in ("step_id", "name", "status", "worker", "attempts", "complexity", "depends_on", "file_scope")})
    try:
        from .quota_ledger import QuotaLedger

        usage = QuotaLedger().run_usage(run_id)
    except Exception:
        usage = list(record.usage or [])
    data = record.to_json()
    return {
        **_row(record.run_id, record.state, record.cwd, record.goal, record.created, record.updated, data.get("session_id")),
        "reason": secret_patterns.scrub(record.reason or "")[0],
        "taskClass": record.task_class,
        "planner": record.planner,
        "steps": steps,
        "workers": record.workers,
        "usage": usage,
        "handoffs": record.handoffs,
        "warnings": [secret_patterns.scrub(w)[0] for w in record.warnings],
        "history": record.history[-60:],
        "audit": {k: (record.audit or {}).get(k) for k in ("tier1_ok", "verdict", "tier2_used", "failures", "items")} if record.audit else None,
        "rounds": record.rounds,
        "artifacts": artifacts_list(run_id),
        "control": {
            "canPause": live == "running",
            "canCancel": live in ("running", "paused", "interrupted"),
            "canResume": live in ("paused", "interrupted"),
        },
    }


def artifacts_list(run_id: str) -> list[dict[str, Any]]:
    if not _valid_run_id(run_id):
        return []
    folder = run_dir(run_id)
    if not folder.is_dir():
        return []
    out = []
    for path in sorted(folder.iterdir()):
        if path.is_file() and path.suffix in READABLE_SUFFIXES and path.name != "control.ndjson":
            try:
                out.append({"name": path.name, "bytes": path.stat().st_size, "modified": path.stat().st_mtime})
            except OSError:
                continue
    return out


def artifacts_read(run_id: str, name: str, *, offset: int = 0, limit: int = READ_LIMIT) -> dict[str, Any]:
    """Part of one artifact, by a name ``artifacts_list`` offered. Masked."""
    if not _valid_run_id(run_id) or name not in {a["name"] for a in artifacts_list(run_id)}:
        return {"ok": False, "reason": "no such artifact"}
    folder = os.path.realpath(run_dir(run_id))
    path = os.path.realpath(os.path.join(folder, name))
    if os.path.dirname(path) != folder:
        return {"ok": False, "reason": "no such artifact"}
    limit = max(1, min(int(limit), READ_LIMIT))
    try:
        with open(path, "rb") as handle:
            handle.seek(max(0, int(offset)))
            chunk = handle.read(limit)
            size = os.fstat(handle.fileno()).st_size
    except OSError as error:
        return {"ok": False, "reason": str(error)}
    text = chunk.decode("utf-8", errors="replace")
    end = max(0, int(offset)) + len(chunk)
    return {"ok": True, "name": name, "text": secret_patterns.scrub(text)[0], "offset": int(offset), "end": end, "size": size, "more": end < size}


def read_events(run_id: str, offset: int = 0, limit: int = 500) -> dict[str, Any]:
    """Events from ``events.ndjson`` after byte ``offset``: what a front door attaching late (or after a restart) catches
    up on. Whole lines only; ``next`` is where to continue."""
    path = run_dir(run_id) / "events.ndjson"
    events: list[dict[str, Any]] = []
    try:
        with open(path, "rb") as handle:
            handle.seek(max(0, int(offset)))
            position = handle.tell()
            for raw in handle:
                if not raw.endswith(b"\n"):
                    break  # a line still being written
                position += len(raw)
                try:
                    events.append(json.loads(raw))
                except ValueError:
                    continue
                if len(events) >= limit:
                    break
    except OSError:
        return {"events": [], "next": int(offset)}
    return {"events": events, "next": position}


# ------------------------------------------------------------------------------------------------------- control


@dataclass
class Request:
    kind: str  # pause | cancel
    now: bool = False
    restore: bool = False
    by: str = ""
    at: float = 0.0


class Controls:
    """What the person asked the run to do, read from its control file. The machine and the executor ask ``pending()``
    at their safe points; ``on_now`` is set by the executor to stop the worker call in flight."""

    def __init__(self, path: Path | None = None) -> None:
        self.path = path
        self._offset = 0
        self._lock = threading.Lock()
        self._request: Request | None = None
        self.on_now: Any = None

    def request(self, kind: str, *, now: bool = False, restore: bool = False, by: str = "") -> None:
        """In-process (tests, an orchestrate tool call): the same as a line in the file."""
        with self._lock:
            self._request = Request(kind, now, restore, by, time.time())
        if now and callable(self.on_now):
            try:
                self.on_now()
            except Exception:
                pass

    def poll(self) -> None:
        """Read new lines from the control file (cheap: a stat and, rarely, a short read)."""
        if self.path is None:
            return
        try:
            size = self.path.stat().st_size
        except OSError:
            return
        if size <= self._offset:
            return
        try:
            with open(self.path, "rb") as handle:
                handle.seek(self._offset)
                data = handle.read()
        except OSError:
            return
        complete = data[: data.rfind(b"\n") + 1]
        self._offset += len(complete)
        for line in complete.splitlines():
            try:
                cmd = json.loads(line)
            except ValueError:
                continue
            kind = str(cmd.get("cmd") or "")
            if kind in ("pause", "cancel"):
                self.request(kind, now=bool(cmd.get("now")), restore=bool(cmd.get("restore")), by=str(cmd.get("by") or ""))

    def pending(self) -> Request | None:
        self.poll()
        with self._lock:
            return self._request

    def clear(self) -> None:
        with self._lock:
            self._request = None


def request(run_id: str, kind: str, *, now: bool = False, restore: bool = False, by: str = "") -> dict[str, Any]:
    """Ask a running run to pause or cancel: one line in its control file. A paused or interrupted run is cancelled by
    marking its record (nothing is running to ask)."""
    from .machine import load, save

    if kind not in ("pause", "cancel") or not _valid_run_id(run_id):
        return {"ok": False, "reason": "unknown request"}
    record = load(run_id)
    if record is None:
        return {"ok": False, "reason": f"no run {run_id}"}
    live = liveness(record.state, run_id)
    if live in TERMINAL:
        return {"ok": False, "reason": f"the run is already {live}"}
    if kind == "cancel" and live in ("paused", "interrupted"):
        record.state = "cancelled"
        record.reason = "cancelled while it was not running"
        record.history.append({"ts": time.time(), "state": "cancelled", "by": by or "person"})
        save(record)
        state_store.append_event("orchestrator", {"run_id": run_id, "state": "cancelled", "by": by}, session_id=run_id, source="orchestrator.control")
        return {"ok": True, "state": "cancelled"}
    if live != "running":
        return {"ok": False, "reason": f"the run is {live}, not running"}
    folder = run_dir(run_id)
    folder.mkdir(parents=True, exist_ok=True)
    path = folder / "control.ndjson"
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    with os.fdopen(fd, "a", encoding="utf-8") as out:
        out.write(json.dumps({"cmd": kind, "now": now, "restore": restore, "by": by, "at": time.time()}) + "\n")
    return {"ok": True, "state": "requested", "kind": kind, "now": now}


# ------------------------------------------------------------------------------------------------------- hosting


def new_run_id() -> str:
    return f"run-{uuid.uuid4().hex[:10]}"


def _entry(argv: list[str]) -> list[str]:
    """``CORDATA_ORCHESTRATOR_ENTRYPOINT``: another program in place of ``-m orchestrator`` taking the same arguments —
    the test suite's scripted run (``fixtures/fake_orchestrator.py``), like the daemon's ``CORDATA_ACP_ENTRYPOINT``."""
    override = os.environ.get("CORDATA_ORCHESTRATOR_ENTRYPOINT")
    if override and argv[1:3] == ["-m", "orchestrator"]:
        return [argv[0], override, *argv[3:]]
    return argv


def _spawn(argv: list[str], run_id: str, session_id: str | None, env_extra: dict[str, str] | None = None) -> dict[str, Any]:
    argv = _entry(argv)
    folder = run_dir(run_id)
    folder.mkdir(parents=True, exist_ok=True)
    os.chmod(folder, 0o700)
    events = folder / "events.ndjson"
    out_fd = os.open(events, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    err_fd = os.open(folder / "run.log", os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    env = {**os.environ, "PYTHONUNBUFFERED": "1", **(env_extra or {})}
    try:
        proc = subprocess.Popen(argv, cwd=str(BACKEND_DIR), stdin=subprocess.DEVNULL, stdout=out_fd, stderr=err_fd, env=env, start_new_session=True)
    finally:
        os.close(out_fd)
        os.close(err_fd)
    info = {"pid": proc.pid, "started": time.time(), "argv": argv[:4], "sessionId": session_id}
    fd = os.open(folder / "host.json", os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as out:
        json.dump(info, out)
    # Reap it if it ends while this process lives; if this process ends first, launchd inherits and reaps it.
    threading.Thread(target=proc.wait, name=f"reap-{run_id}", daemon=True).start()
    return {"ok": True, "runId": run_id, "pid": proc.pid, "events": str(events)}


def spawn_run(goal: str, cwd: str, *, session_id: str | None = None, run_id: str | None = None, options: dict[str, Any] | None = None, env_extra: dict[str, str] | None = None) -> dict[str, Any]:
    """Start a run detached. ``options``: workers, maxUsd, maxParallel, research, harden, conformance, tier2, dispatch,
    allowDirect (the caller has applied the gates — this only starts it)."""
    options = options or {}
    run_id = run_id or new_run_id()
    argv = [sys.executable, "-m", "orchestrator", "run", goal, "--cwd", os.path.realpath(cwd), "--run-id", run_id, "--json"]
    if session_id:
        argv += ["--session-id", session_id]
    if options.get("workers"):
        argv += ["--workers", ",".join(options["workers"]) if isinstance(options["workers"], list) else str(options["workers"])]
    if options.get("maxUsd") is not None:
        argv += ["--max-usd", str(float(options["maxUsd"]))]
    if options.get("maxParallel") is not None:
        argv += ["--max-parallel", str(int(options["maxParallel"]))]
    if options.get("dispatch") in ("off", "shadow", "live"):
        argv += ["--dispatch", options["dispatch"]]
    for key, flag in (("research", "--no-research"), ("harden", "--no-harden"), ("conformance", "--no-conformance"), ("tier2", "--no-tier2")):
        if options.get(key) is False:
            argv.append(flag)
    if options.get("research") is True:
        argv.append("--research")
    if options.get("allowDirect"):
        argv.append("--allow-direct")
    return _spawn(argv, run_id, session_id, env_extra)


def resume_run(run_id: str, *, session_id: str | None = None, allow_direct: bool = False, env_extra: dict[str, str] | None = None) -> dict[str, Any]:
    """Continue a paused or interrupted run, detached. Refused while its process is alive (two processes driving one run
    would each think the work was theirs)."""
    from .machine import load

    record = load(run_id)
    if record is None:
        return {"ok": False, "reason": f"no run {run_id}"}
    live = liveness(record.state, run_id)
    if live == "running":
        return {"ok": False, "reason": "the run is still running"}
    if live not in ("paused", "interrupted"):
        return {"ok": False, "reason": f"the run is {live}"}
    # A fresh control file: the pause that stopped it must not stop it again.
    try:
        (run_dir(run_id) / "control.ndjson").unlink()
    except OSError:
        pass
    argv = [sys.executable, "-m", "orchestrator", "resume", run_id, "--json"]
    if allow_direct:
        argv.append("--allow-direct")
    return _spawn(argv, run_id, session_id or (load(run_id).to_json().get("session_id") if load(run_id) else None), env_extra)

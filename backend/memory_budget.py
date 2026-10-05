"""The unified-memory budget: what local model weights may be resident, and who goes
when something bigger wants in.

The host is a 48 GB Apple-silicon machine where CPU, GPU and the Neural Engine share
one pool. Local model weights (oMLX's Prism models, Ollama's local tags) live in that
pool next to everything else the harness keeps resident — the LanceDB trace corpus,
the Laya router checkpoint, the Python agent stack, the Claude Code CLI. Out-of-memory
there is not an exception to catch; it is the whole machine thrashing or a process
killed. So loading is never "just do it":

    ceiling            48 GB      (SIERRALAB_MEMORY_CEILING_GB)
    − reserve          10 GB      (SIERRALAB_MEMORY_RESERVE_GB — macOS, LanceDB, Laya,
                                   the harness, the Claude CLI)
    = budget           38 GB      for model weights

``ensure_headroom(needed_gb)`` is the one operation the rest of the harness uses. It
sums what is resident (oMLX's status + Ollama's ``/api/ps``), and when ``needed`` does
not fit it stops the *least recently used* unpinned models until it does — and says so.
If it cannot make room it loads nothing and says why; it never evicts for a request
bigger than the whole budget (the eviction would not have helped). Callers:

* ``omlx_resource_tool``  — ``load`` runs it first, so the model never has to remember;
* ``ollama_resource_tool`` — its ``ensure_headroom`` action, the LLM-visible handle;
* ``claude_code_tool``     — ``release_for_heavy_task`` before spawning the CLI;
* the daemon's gRPC bridge — the GUI's emergency [Unload Memory] is ``unload_all``.

Stdlib + the two light managers; no ``strands`` import (see ``omlx_manager``).
"""

from __future__ import annotations

import logging
import os
import threading
from dataclasses import dataclass, field
from typing import Any

import ollama_cli
import omlx_manager

logger = logging.getLogger(__name__)

GB = 1024**3

DEFAULT_CEILING_GB = 48.0
DEFAULT_RESERVE_GB = 10.0
#: What a Claude Code CLI run is budgeted at, beside the harness: a Node process, its
#: file cache and a build or test run it launches. A planning number, not a measurement.
HEAVY_TASK_GB = 6.0


def _env_gb(name: str, default: float) -> float:
    try:
        value = float(os.getenv(name, ""))
    except ValueError:
        return default
    return value if value > 0 else default


class MemoryBudgetExceeded(RuntimeError):
    """Not enough room, and not enough that could be freed. Nothing was loaded."""

    def __init__(self, message: str, headroom: "Headroom | None" = None) -> None:
        super().__init__(message)
        self.headroom = headroom


@dataclass(frozen=True)
class Resident:
    """One model whose weights are in memory right now."""

    runtime: str  # "omlx" | "ollama"
    name: str
    size_gb: float
    last_used: float | None = None  # epoch seconds; None sorts as the oldest
    pinned: bool = False

    @property
    def key(self) -> tuple[str, str]:
        return (self.runtime, self.name)

    def to_json(self) -> dict[str, Any]:
        return {"runtime": self.runtime, "name": self.name, "size_gb": round(self.size_gb, 2), "last_used": self.last_used, "pinned": self.pinned}


@dataclass(frozen=True)
class Snapshot:
    ceiling_gb: float
    reserve_gb: float
    residents: tuple[Resident, ...]
    omlx_pool_gb: float | None = None  # oMLX's own ceiling, when it is up
    omlx_up: bool = False
    notes: tuple[str, ...] = ()

    @property
    def budget_gb(self) -> float:
        return max(0.0, self.ceiling_gb - self.reserve_gb)

    @property
    def used_gb(self) -> float:
        return sum(r.size_gb for r in self.residents)

    @property
    def free_gb(self) -> float:
        return self.budget_gb - self.used_gb

    def used_by(self, runtime: str) -> float:
        return sum(r.size_gb for r in self.residents if r.runtime == runtime)

    def to_json(self) -> dict[str, Any]:
        return {
            "ceiling_gb": self.ceiling_gb,
            "reserve_gb": self.reserve_gb,
            "budget_gb": round(self.budget_gb, 2),
            "used_gb": round(self.used_gb, 2),
            "free_gb": round(self.free_gb, 2),
            "omlx_pool_gb": None if self.omlx_pool_gb is None else round(self.omlx_pool_gb, 2),
            "residents": [r.to_json() for r in self.residents],
            "notes": list(self.notes),
        }


@dataclass
class Headroom:
    needed_gb: float
    ok: bool
    free_before_gb: float
    free_after_gb: float
    freed: list[Resident] = field(default_factory=list)
    reason: str = ""
    errors: list[str] = field(default_factory=list)

    def to_json(self) -> dict[str, Any]:
        return {
            "needed_gb": round(self.needed_gb, 2),
            "ok": self.ok,
            "free_before_gb": round(self.free_before_gb, 2),
            "free_after_gb": round(self.free_after_gb, 2),
            "freed": [r.to_json() for r in self.freed],
            "reason": self.reason,
            "errors": self.errors,
        }

    def summary(self) -> str:
        freed = ", ".join(f"{r.name} ({r.size_gb:.1f} GB)" for r in self.freed) or "nothing"
        head = f"{'OK' if self.ok else 'NOT ENOUGH ROOM'}: need {self.needed_gb:.1f} GB, free {self.free_before_gb:.1f} → {self.free_after_gb:.1f} GB; stopped {freed}."
        return head + (f" {self.reason}" if self.reason else "")


class MemoryBudget:
    def __init__(
        self,
        omlx: omlx_manager.OmlxManager | None = None,
        ollama: ollama_cli.OllamaCli | None = None,
        *,
        ceiling_gb: float | None = None,
        reserve_gb: float | None = None,
    ) -> None:
        self._omlx = omlx
        self._ollama = ollama
        self.ceiling_gb = ceiling_gb if ceiling_gb is not None else _env_gb("SIERRALAB_MEMORY_CEILING_GB", DEFAULT_CEILING_GB)
        self.reserve_gb = reserve_gb if reserve_gb is not None else _env_gb("SIERRALAB_MEMORY_RESERVE_GB", DEFAULT_RESERVE_GB)
        self._lock = threading.RLock()

    @property
    def omlx(self) -> omlx_manager.OmlxManager:
        return self._omlx or omlx_manager.default_manager()

    @property
    def ollama(self) -> ollama_cli.OllamaCli:
        return self._ollama or ollama_cli.default_cli()

    # ------------------------------------------------------------- reading

    def snapshot(self) -> Snapshot:
        residents: list[Resident] = []
        notes: list[str] = []
        pool: float | None = None
        omlx_up = False
        try:
            status = self.omlx.status()
            omlx_up = True
            pool = status.ceiling_gb or None
            for row in status.loaded:
                residents.append(Resident("omlx", row.id, row.size_gb, row.last_access, row.pinned))
        except omlx_manager.OmlxError as error:
            notes.append(f"oMLX not readable ({error}); counted as empty")
        try:
            for row in self.ollama.running():
                residents.append(Resident("ollama", row.name, row.size_gb, row.expires_at))
        except Exception as error:  # a broken ps must not stop an unload
            notes.append(f"ollama ps not readable ({error})")
        return Snapshot(self.ceiling_gb, self.reserve_gb, tuple(residents), pool, omlx_up, tuple(notes))

    # ------------------------------------------------------------- freeing

    def _stop(self, resident: Resident) -> None:
        if resident.runtime == "omlx":
            self.omlx.unload(resident.name)
            return
        result = self.ollama.stop(resident.name)
        if not result.ok:
            raise ollama_cli.OllamaCliError(result.text or f"ollama stop exited {result.exit_code}")

    def _satisfied(self, snap: Snapshot, needed_gb: float, runtime: str | None) -> bool:
        if snap.free_gb < needed_gb:
            return False
        if runtime == "omlx" and snap.omlx_pool_gb is not None and snap.omlx_pool_gb - snap.used_by("omlx") < needed_gb:
            return False
        return True

    def ensure_headroom(self, needed_gb: float, *, protect: tuple[tuple[str, str], ...] | set[tuple[str, str]] = (), runtime: str | None = None, reason: str = "") -> Headroom:
        """Make ``needed_gb`` of the budget free, stopping least-recently-used,
        unpinned, unprotected models until it is. ``runtime`` names where the new
        weights will live, so oMLX's own pool ceiling is honoured as well as the
        global budget."""
        needed_gb = max(0.0, float(needed_gb))
        with self._lock:
            snap = self.snapshot()
            result = Headroom(needed_gb, False, snap.free_gb, snap.free_gb)
            if needed_gb > snap.budget_gb:
                result.reason = f"{needed_gb:.1f} GB exceeds the whole {snap.budget_gb:.1f} GB model budget ({snap.ceiling_gb:.0f} GB − {snap.reserve_gb:.0f} GB reserve); nothing was stopped."
                return result
            if runtime == "omlx" and snap.omlx_pool_gb is not None and needed_gb > snap.omlx_pool_gb:
                result.reason = f"{needed_gb:.1f} GB exceeds oMLX's own {snap.omlx_pool_gb:.1f} GB pool; nothing was stopped."
                return result
            protected = set(protect)
            victims = sorted((r for r in snap.residents if not r.pinned and r.key not in protected), key=lambda r: (r.last_used or 0.0, r.name))
            if runtime == "omlx" and snap.omlx_pool_gb is not None and snap.free_gb >= needed_gb:
                # Global room is fine; only oMLX's pool is short — only its models help.
                victims = [r for r in victims if r.runtime == "omlx"]
            for victim in victims:
                if self._satisfied(snap, needed_gb, runtime):
                    break
                try:
                    self._stop(victim)
                except Exception as error:
                    result.errors.append(f"could not stop {victim.name}: {error}")
                    logger.warning("memory_budget: could not stop %s: %s", victim.name, error)
                    continue
                result.freed.append(victim)
                snap = self.snapshot()
            result.free_after_gb = snap.free_gb
            result.ok = self._satisfied(snap, needed_gb, runtime)
            if not result.ok:
                result.reason = "Everything that could be stopped was (pinned and protected models stay)."
            return result

    def release_for_heavy_task(self, *, needed_gb: float = HEAVY_TASK_GB) -> Headroom:
        """Before a Claude Code run: every unpinned oMLX model goes (a Prism model has
        no business resident beside a heavy task — the spec's rule), then whatever
        else is needed to reach the task's budget."""
        with self._lock:
            freed: list[Resident] = []
            errors: list[str] = []
            first = self.snapshot()
            try:
                for row in self.omlx.unload_all():
                    freed.append(Resident("omlx", row.id, row.size_gb))
            except omlx_manager.OmlxError as error:
                errors.append(f"oMLX: {error}")
            tail = self.ensure_headroom(needed_gb, reason="heavy task")
            tail.freed = [*freed, *tail.freed]
            tail.errors = [*errors, *tail.errors]
            tail.free_before_gb = first.free_gb
            return tail

    def unload_all(self, runtimes: tuple[str, ...] = ("omlx", "ollama")) -> Headroom:
        """The emergency button: everything unpinned, regardless of recency."""
        with self._lock:
            snap = self.snapshot()
            result = Headroom(0.0, True, snap.free_gb, snap.free_gb, reason="emergency unload")
            for resident in snap.residents:
                if resident.pinned or resident.runtime not in runtimes:
                    continue
                try:
                    self._stop(resident)
                    result.freed.append(resident)
                except Exception as error:
                    result.ok = False
                    result.errors.append(f"could not stop {resident.name}: {error}")
            result.free_after_gb = self.snapshot().free_gb
            return result

    # -------------------------------------------------------------- loading

    def load_omlx(self, model: str, *, timeout: float = omlx_manager.LOAD_TIMEOUT) -> tuple[omlx_manager.OmlxModelStatus, Headroom | None]:
        """Load an oMLX model *with* room made first. The only way this repo loads one."""
        row = self.omlx.resolve(model)
        if row.loaded:
            return row, None
        room = self.ensure_headroom(row.size_gb, protect={("omlx", row.id)}, runtime="omlx", reason=f"to load {row.id}")
        if not room.ok:
            raise MemoryBudgetExceeded(f"not loading {row.id}: {room.summary()}", room)
        return self.omlx.load(row.id, timeout=timeout), room


_DEFAULT: MemoryBudget | None = None
_DEFAULT_LOCK = threading.Lock()


def default_budget() -> MemoryBudget:
    global _DEFAULT
    with _DEFAULT_LOCK:
        if _DEFAULT is None:
            _DEFAULT = MemoryBudget()
        return _DEFAULT

"""One outcome record per turn — the harness's reward signal.

Everything the harness can learn from needs to know whether a turn *worked*. Until now that
was scattered: exit codes in ``laya_router`` (per command), approve/reject in the ledger, Laya
Guard's holds on the pods, files the person quietly reverted afterwards, tests the agent did or
did not run. Nothing joined them, so there was nothing to score, compare or learn from.

This module joins them. A :class:`Recorder` watches one turn — from the person's prompt to the
stop reason — and collects what happened as a :class:`TurnRecord`: the goal (masked), the route,
which agents ran which tools and how many failed, how the approvals went, whether a verifier
(a test, build or lint command) ran *after the last edit* and passed, which files changed (with
content hashes, so "kept" can be decided later), what it cost in tokens, time and Claude Code
dollars. The record is appended to the ledger (``state.db`` events, kind ``outcome``).

Two things are deliberately separate from the record:

* **Reconciliation** (:func:`reconcile`). Whether the person *kept* the agent's changes is only
  knowable afterwards. Each file's hash at the end of the turn is compared with its hash now:
  unchanged = kept, back to its pre-turn content = reverted, anything else = edited. The result is
  a second event (``outcome_update``), so the original record is never rewritten.
* **The reward** (:func:`score`). A pure function of a record (and, once known, its update) with its
  weights written down and every component returned, so a learner can use the parts rather than the
  sum, and so a change to the weights is a diff someone reads. It is a *proxy*: a turn that ends
  with a passing test and changes the person keeps is a good turn; one that fails its own tests,
  gets its approvals rejected and is reverted is not. It is not ground truth, and the eval harness
  (Phase 2) exists to check it against some.

Stdlib only: the daemon may read these records without importing the agent stack.
Never raises into a turn — recording is a side channel, and a failure to record is logged and dropped.
"""

from __future__ import annotations

import hashlib
import json
import logging
import os
import re
import subprocess
import threading
import time
import uuid
from collections import Counter
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any

import secret_guard
import state_store

logger = logging.getLogger(__name__)

SCHEMA = 1
KIND = "outcome"
UPDATE_KIND = "outcome_update"

MAX_FILES = 100
MAX_HASH_BYTES = 2 * 1024 * 1024
MAX_GOAL_CHARS = 300
_SNAPSHOT_LIMIT = 50_000

#: Directories that are rebuilt, vendored or private — never part of "what the agent changed".
SKIP_DIRS = frozenset({".git", "node_modules", "target", ".venv", "venv", "__pycache__", ".next", "dist", "build", ".mypy_cache", ".pytest_cache", ".ruff_cache", ".tox", ".cache", "coverage", ".turbo"})
_SECRET_FILE = re.compile(r"(^|/)(\.env(\.(?!example|sample|template|dist)[^/]*)?|id_rsa[^/]*|id_ed25519[^/]*|[^/]*\.(pem|p12|pfx|key|keychain)|\.npmrc|\.netrc|\.pypirc)$", re.IGNORECASE)

#: Tools whose calls change files directly. (A shell command can too; the snapshot diff catches those.)
EDIT_TOOLS = frozenset({"file_write", "editor", "apply_patch", "Edit", "Write", "MultiEdit"})

#: A command that *checks* the work: tests, builds, type-checks, linters. Their exit code is the
#: closest thing to ground truth a turn produces, which is why they get their own place in the record.
_VERIFIER = re.compile(
    r"^("
    r"pytest|py\.test|tox|nox|unittest|"
    r"cargo\s+(test|check|build|clippy|nextest|fmt\s+--check)|"
    r"go\s+(test|vet|build)|"
    r"(npm|pnpm|yarn|bun)\s+(run\s+)?(test|lint|typecheck|build|check)|"
    r"make\s+(test|check|lint|build|verify)|"
    r"tsc|eslint|mypy|ruff(\s+check)?|flake8|pyright|"
    r"(mvn|gradle|gradlew|mvnw)\s+(test|check|verify|build)|"
    r"python3?\s+(-m\s+)?(pytest|unittest)|"
    r"python3?\s+\S*_smoke_test\.py|"
    r"bash\s+\S*test\S*\.sh|"
    r"jest|vitest|mocha|rspec|phpunit|playwright\s+test|cypress\s+run|deno\s+test|bun\s+test|dotnet\s+test|swift\s+test"
    r")\b",
    re.IGNORECASE,
)
_SEPARATORS = re.compile(r"&&|\|\||[;|\n(]")
_WRAPPERS = re.compile(r"^(?:[A-Za-z_][A-Za-z0-9_]*=\S*\s+|(?:time|nohup|sudo|env|exec|command|npx|bunx)\s+|(?:uv|poetry|pipenv|pdm|hatch)\s+run\s+|(?:pnpm|yarn)\s+exec\s+)+")


def is_verifier(command: str) -> bool:
    """Is this shell command one that checks the work? Judged by what *runs*, not by what is mentioned:
    ``pytest -q`` and ``cd x && cargo test`` are; ``echo pytest`` and ``grep make test.log`` are not."""
    if not command:
        return False
    for segment in _SEPARATORS.split(command):
        segment = _WRAPPERS.sub("", segment.strip())
        # `.venv/bin/pytest` and `/usr/bin/python3 -m pytest` run the same thing as `pytest`.
        first, _, rest = segment.partition(" ")
        if "/" in first:
            segment = os.path.basename(first) + (" " + rest if rest else "")
        if segment and _VERIFIER.match(segment):
            return True
    return False


# ------------------------------------------------------------------------------ files


def _skip_file(rel: str) -> bool:
    return bool(_SECRET_FILE.search(rel))


def snapshot_tree(root: str) -> dict[str, tuple[int, int]] | None:
    """``{relative path: (size, mtime_ns)}`` for a project, minus dependency/build directories and
    secrets files. Compared before and after a turn it says what changed in *any* directory — a git
    repository or not, tracked or not. ``None`` when the tree is too large to walk cheaply; the
    caller then reports no change list rather than guessing."""
    out: dict[str, tuple[int, int]] = {}
    stack = [root]
    while stack:
        directory = stack.pop()
        try:
            with os.scandir(directory) as entries:
                for entry in entries:
                    if entry.is_dir(follow_symlinks=False):
                        if entry.name not in SKIP_DIRS:
                            stack.append(entry.path)
                    elif entry.is_file(follow_symlinks=False):
                        rel = os.path.relpath(entry.path, root)
                        if _skip_file(rel):
                            continue
                        info = entry.stat(follow_symlinks=False)
                        out[rel] = (info.st_size, info.st_mtime_ns)
                        if len(out) > _SNAPSHOT_LIMIT:
                            return None
        except OSError:
            continue
    return out


def file_hash(path: str | Path) -> str | None:
    """A short content hash, or None when the file does not exist / cannot be read. A file over the
    size limit hashes by size and mtime-free marker, so 'unchanged' stays meaningful without reading 2 GB."""
    try:
        size = os.path.getsize(path)
        if size > MAX_HASH_BYTES:
            return f"big:{size}"
        digest = hashlib.sha256()
        with open(path, "rb") as handle:
            digest.update(handle.read())
        return digest.hexdigest()[:20]
    except OSError:
        return None


def _git_numstat(root: str, files: list[str]) -> dict[str, int]:
    """Lines added/removed across ``files`` according to git, best effort ({} outside a repository)."""
    if not files:
        return {}
    try:
        run = subprocess.run(["git", "-C", root, "diff", "--numstat", "HEAD", "--", *files[:200]], capture_output=True, text=True, timeout=8)
    except (OSError, subprocess.SubprocessError):
        return {}
    if run.returncode != 0:
        return {}
    added = removed = 0
    for line in run.stdout.splitlines():
        parts = line.split("\t")
        if len(parts) >= 3:
            added += int(parts[0]) if parts[0].isdigit() else 0
            removed += int(parts[1]) if parts[1].isdigit() else 0
    return {"added": added, "removed": removed}


# ------------------------------------------------------------------------------ the record


@dataclass
class TurnRecord:
    id: str
    session_id: str
    project: str
    ts: float
    seconds: float
    goal: str
    mode: str
    stop_reason: str
    route: dict[str, Any] | None = None
    agents: list[str] = field(default_factory=list)
    tools: dict[str, dict[str, Any]] = field(default_factory=dict)
    calls: int = 0
    failed_calls: int = 0
    approvals: dict[str, int] = field(default_factory=dict)
    guard_peak: float = 0.0
    verifier: dict[str, Any] = field(default_factory=dict)
    edits: int = 0
    files: list[dict[str, Any]] = field(default_factory=list)
    diffstat: dict[str, int] = field(default_factory=dict)
    heal: dict[str, Any] = field(default_factory=dict)
    usage: dict[str, int] = field(default_factory=dict)
    claude_code: dict[str, Any] = field(default_factory=dict)
    #: Snapshots taken for the turn (checkpoints.py): ``taken``, ``unprotected`` (a change that could not be
    #: snapshotted, with the reason), ``outside`` (writes beyond the project), ``first`` / ``end`` ids.
    checkpoints: dict[str, Any] = field(default_factory=dict)
    #: Loops the stall guard saw (stall_guard.py): ``notes`` (told the model), ``blocks`` (refused a repeat), ``cycles`` (edit A→B→A).
    stalls: dict[str, int] = field(default_factory=dict)
    #: Commands that failed during the turn, with their normalised error lines (``fix_memory.signature``): what
    #: reflection learns from. ``resolved`` is whether the same command later passed.
    failures: list[dict[str, Any]] = field(default_factory=list)
    #: Learned skills (learned.py) the model read this turn — how a skill's usefulness is judged.
    skills: list[str] = field(default_factory=list)
    schema: int = SCHEMA

    def to_json(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> "TurnRecord":
        known = {f for f in cls.__dataclass_fields__}  # type: ignore[attr-defined]
        return cls(**{k: v for k, v in data.items() if k in known})


class Recorder:
    """Collects one turn. Thread-safe: Strands hooks and the approval handler call in from different threads."""

    def __init__(self, session_id: str, *, goal: str, mode: str, project: str, route: dict[str, Any] | None = None) -> None:
        self.session_id = session_id
        self.id = uuid.uuid4().hex[:16]
        self.project = project
        self.mode = mode
        self.route = route
        self.started = time.time()
        self.goal = _mask(goal)[:MAX_GOAL_CHARS]
        self._lock = threading.RLock()
        self._seq = 0
        self._seen_calls: set[str] = set()
        self._approval_seen: set[tuple[str, str]] = set()
        self._tools: dict[str, Counter] = {}
        self._agents: list[str] = []
        self._approvals: Counter = Counter()
        self._guard_peak = 0.0
        self._edit_seqs: list[int] = []
        self._verifier_runs: list[tuple[int, bool, str]] = []  # (seq, passed, command)
        self._failed_commands: dict[str, bool] = {}  # normalised command → last result ok?
        self._before: dict[str, str | None] = {}  # path → content hash at first touch this turn
        self._snapshot: dict[str, tuple[int, int]] | None = snapshot_tree(project) if project else None
        self._heal_before = 0
        self._nested_edits = 0
        self._usage: Counter = Counter()
        self._claude: Counter = Counter()
        self._checkpoints: dict[str, Any] = {}
        self._stalls: Counter = Counter()
        self._failure_lines: dict[str, list[str]] = {}
        self._skills: list[str] = []
        self._finished = False

    # ---------------------------------------------------------------- events

    def _tick(self) -> int:
        self._seq += 1
        return self._seq

    def note_edit_paths(self, paths: list[str]) -> None:
        """Called *before* an edit tool runs: remember what each file held, once per file per turn."""
        with self._lock:
            for path in paths:
                full = path if os.path.isabs(path) else os.path.join(self.project, path)
                rel = os.path.relpath(full, self.project) if self.project else path
                if rel in self._before or rel.startswith(".."):
                    continue
                self._before[rel] = None if not os.path.exists(full) else file_hash(full)

    def tool(self, agent: str, name: str, *, tool_use_id: str | None, ok: bool, ms: float = 0.0, command: str | None = None, edit: bool = False, denied: bool = False) -> None:
        """One finished tool call. A call resumed after an approval prompt reports once. ``edit`` marks a
        call that changed files without being an edit tool (a shell command that writes). ``denied`` marks a
        call that was *refused* — the person said no, a deny rule or plan mode blocked it: it never ran, so it
        is neither a failure of the work nor a command left failing."""
        with self._lock:
            if tool_use_id:
                if tool_use_id in self._seen_calls:
                    return
                self._seen_calls.add(tool_use_id)
            seq = self._tick()
            if agent and agent not in self._agents:
                self._agents.append(agent)
            stat = self._tools.setdefault(name, Counter())
            stat["count"] += 1
            stat["ms"] += int(ms)
            if denied:
                stat["denied"] += 1
                return
            if not ok:
                stat["failed"] += 1
            if (name in EDIT_TOOLS or edit) and ok:
                self._edit_seqs.append(seq)
            if name == "shell" and command:
                self._failed_commands[" ".join(command.split())] = ok
                if is_verifier(command):
                    self._verifier_runs.append((seq, ok, command[:200]))

    def nested(self, tool: str, ok: bool, *, command: str | None = None) -> None:
        """A step taken *inside* another tool — what the Claude Code CLI did in its own session. It counts
        as an edit if it edited, and as a verifier run if it ran one: a turn handed to the CLI that
        ends with its tests passing is a verified turn."""
        with self._lock:
            seq = self._tick()
            if tool in EDIT_TOOLS and ok:
                self._edit_seqs.append(seq)
                self._nested_edits += 1
            elif tool in ("Bash", "shell") and command:
                self._failed_commands[" ".join(command.split())] = ok
                if is_verifier(command):
                    self._verifier_runs.append((seq, ok, command[:200]))

    def approval(self, kind: str, tool: str, score: float | None = None, key: str | None = None) -> None:
        """``asked`` · ``approved`` · ``rejected`` · ``auto`` · ``allowed``. The same call reported
        twice (a resume after a prompt) counts once per kind."""
        with self._lock:
            if key is not None:
                if (key, kind) in self._approval_seen:
                    return
                self._approval_seen.add((key, kind))
            self._approvals[kind] += 1
            if score is not None:
                self._guard_peak = max(self._guard_peak, float(score))

    @property
    def call_count(self) -> int:
        """Distinct tool calls seen so far this turn — for a front door's one-line receipt."""
        with self._lock:
            return len(self._seen_calls)

    def usage(self, input_tokens: int = 0, output_tokens: int = 0) -> None:
        with self._lock:
            self._usage["input"] += int(input_tokens or 0)
            self._usage["output"] += int(output_tokens or 0)

    def claude_code(self, cost_usd: float | None) -> None:
        with self._lock:
            self._claude["runs"] += 1
            if isinstance(cost_usd, (int, float)):
                self._claude["cents"] += int(round(cost_usd * 100))

    def checkpoint(self, ok: bool, commit: str | None = None, reason: str | None = None, outside: int = 0) -> None:
        """A snapshot was attempted before a call that may change files (``ok`` False: it could not be taken)."""
        with self._lock:
            note = self._checkpoints
            note["taken" if ok else "unprotected"] = note.get("taken" if ok else "unprotected", 0) + 1
            if ok and commit and "first" not in note:
                note["first"] = commit
            if not ok and reason:
                note["reason"] = reason[:200]
            if outside:
                note["outside"] = note.get("outside", 0) + outside

    def failure(self, command: str, lines: list[str]) -> None:
        """A command failed: remember its error lines (secrets masked), at most four commands per turn."""
        key = " ".join((command or "").split())
        if not key:
            return
        with self._lock:
            if key in self._failure_lines or len(self._failure_lines) < 4:
                self._failure_lines[key] = [_mask(line)[:160] for line in lines[:4]]

    def skill(self, name: str) -> None:
        with self._lock:
            if name and name not in self._skills and len(self._skills) < 8:
                self._skills.append(str(name)[:64])

    def stall(self, kind: str) -> None:
        """``note`` · ``block`` · ``cycle`` — the stall guard saw the agent going in circles."""
        with self._lock:
            self._stalls[{"note": "notes", "block": "blocks", "cycle": "cycles"}.get(kind, kind)] += 1

    def checkpoint_end(self, commit: str | None) -> None:
        with self._lock:
            if commit:
                self._checkpoints["end"] = commit

    @property
    def checkpointed(self) -> bool:
        return bool(self._checkpoints.get("taken"))

    @property
    def first_checkpoint(self) -> str | None:
        """The snapshot taken before this turn's first change: the "before" of the turn's diff."""
        return self._checkpoints.get("first")

    def set_route(self, route: dict[str, Any] | None) -> None:
        with self._lock:
            self.route = route

    def heal_baseline(self, episodes: int) -> None:
        self._heal_before = episodes

    # ---------------------------------------------------------------- finish

    def finish(self, stop_reason: str, *, heal_episodes: list[Any] | None = None) -> TurnRecord:
        with self._lock:
            self._finished = True
            ended = time.time()
            files = self._changed_files()
            diffstat = _git_numstat(self.project, [f["path"] for f in files]) if files and self.project else {}
            last_edit = self._edit_seqs[-1] if self._edit_seqs else (0 if not files else self._seq)
            runs = self._verifier_runs
            after = [r for r in runs if r[0] > last_edit] if (self._edit_seqs or files) else list(runs)
            if not runs:
                verdict = "none"
            elif not after:
                verdict = "stale"  # a verifier ran, but before the last edit: it checked something else
            else:
                verdict = "pass" if after[-1][1] else "fail"
            unresolved = sorted(cmd for cmd, ok in self._failed_commands.items() if not ok)
            tools = {name: {"count": c["count"], "failed": c.get("failed", 0), "denied": c.get("denied", 0), "ms": c.get("ms", 0)} for name, c in self._tools.items()}
            heal_summary: dict[str, Any] = {}
            if heal_episodes is not None:
                heal_summary = {
                    "opened": max(0, len(heal_episodes) - self._heal_before),
                    "verified": sum(1 for ep in heal_episodes if getattr(ep, "verified", False)),
                    "open": sum(1 for ep in heal_episodes if getattr(ep, "status", "") == "open"),
                }
                causes = [_mask(str(getattr(ep, "root_cause", "") or ""))[:200] for ep in heal_episodes if getattr(ep, "verified", False) and getattr(ep, "root_cause", "")]
                if causes:
                    heal_summary["root_causes"] = causes[:3]
            failures = [
                {"command": cmd[:200], "error": lines, "resolved": bool(self._failed_commands.get(cmd, False))}
                for cmd, lines in self._failure_lines.items()
            ]
            total_calls = sum(t["count"] for t in tools.values())
            record = TurnRecord(
                id=self.id,
                session_id=self.session_id,
                project=self.project,
                ts=self.started,
                seconds=round(ended - self.started, 2),
                goal=self.goal,
                mode=self.mode,
                stop_reason=stop_reason,
                route=self.route,
                agents=list(self._agents),
                tools=tools,
                calls=total_calls,
                failed_calls=sum(t["failed"] for t in tools.values()),
                approvals=dict(self._approvals),
                guard_peak=round(self._guard_peak, 3),
                verifier={
                    "ran": len(runs),
                    "passed": sum(1 for r in runs if r[1]),
                    "failed": sum(1 for r in runs if not r[1]),
                    "after_last_edit": verdict,
                    "commands": [r[2] for r in runs[-5:]],
                    "unresolved": unresolved[:5],
                },
                edits=len(self._edit_seqs),
                files=files,
                diffstat=diffstat,
                heal=heal_summary,
                usage={"input": self._usage["input"], "output": self._usage["output"], "total": self._usage["input"] + self._usage["output"]},
                claude_code={"runs": self._claude["runs"], "cost_usd": round(self._claude["cents"] / 100, 2)} if self._claude["runs"] else {},
                checkpoints=dict(self._checkpoints),
                stalls={k: v for k, v in self._stalls.items() if v},
                failures=failures,
                skills=list(self._skills),
            )
        return record

    def _changed_files(self) -> list[dict[str, Any]]:
        if not self.project or self._snapshot is None:
            return []
        after = snapshot_tree(self.project)
        if after is None:
            return []
        changed = sorted(p for p in {*self._snapshot, *after} if self._snapshot.get(p) != after.get(p))
        out = []
        for rel in changed[:MAX_FILES]:
            full = os.path.join(self.project, rel)
            out.append({"path": rel, "before": self._before.get(rel), "after": file_hash(full), "known_before": rel in self._before})
        return out


def _mask(text: str) -> str:
    try:
        import secret_patterns

        masked = secret_patterns.scrub(secret_guard.redact(text or ""))[0]
    except Exception:
        masked = secret_guard.redact(text or "")
    return " ".join(masked.split())


# ------------------------------------------------------------------------------ the registry


_ACTIVE: dict[str, Recorder] = {}
_ACTIVE_LOCK = threading.Lock()


def begin(session_id: str, *, goal: str, mode: str, project: str, route: dict[str, Any] | None = None) -> Recorder:
    """Start recording a turn (replacing one that never finished — a crashed turn is not carried over)."""
    recorder = Recorder(session_id, goal=goal, mode=mode, project=project, route=route)
    with _ACTIVE_LOCK:
        _ACTIVE[session_id] = recorder
    return recorder


def active(session_id: str | None) -> Recorder | None:
    if session_id is None:
        return None
    with _ACTIVE_LOCK:
        return _ACTIVE.get(session_id)


def finish(session_id: str, stop_reason: str, *, heal_episodes: list[Any] | None = None, persist: bool = True) -> TurnRecord | None:
    """End the turn, append its record to the ledger, and return it. Never raises."""
    with _ACTIVE_LOCK:
        recorder = _ACTIVE.pop(session_id, None)
    if recorder is None:
        return None
    try:
        record = recorder.finish(stop_reason, heal_episodes=heal_episodes)
        if persist:
            state_store.append_event(KIND, record.to_json(), session_id=session_id, source="outcomes")
        return record
    except Exception:
        logger.debug("outcomes: could not record the turn", exc_info=True)
        return None


# ------------------------------------------------------------------------------ the reward

#: Every weight in one place, because a change to what the harness optimises should be a diff
#: someone reads. Components are in [-1, 1] before weighting; the total is clamped to [-1, 1].
WEIGHTS = {
    "verified": 0.45,  # a verifier ran after the last edit and passed
    "broken": -0.45,  # …and failed, or a command the turn ran is still failing at the end
    "kept": 0.25,  # the person kept what the agent changed (known only after reconciliation)
    "reverted": -0.40,  # …or put it back
    "rejected": -0.10,  # per approval the person rejected (capped at 3)
    "cancelled": -0.20,  # the person stopped the turn
    "truncated": -0.20,  # the model ran out of room
    "stalled": -0.15,  # went in circles: a repeated step was refused, or an edit undid itself (a note alone: half)
    "tokens": -0.05,  # cost: relative to a 200k-token turn
    "time": -0.05,  # cost: relative to a ten-minute turn
}
TOKEN_BUDGET = 200_000
SECONDS_BUDGET = 600.0


@dataclass(frozen=True)
class Reward:
    value: float
    parts: dict[str, float]
    known: bool  # False until the turn has been reconciled: 'kept' / 'reverted' are not yet knowable

    def to_json(self) -> dict[str, Any]:
        return {"value": self.value, "parts": self.parts, "known": self.known}


def score(record: TurnRecord | dict[str, Any], update: dict[str, Any] | None = None) -> Reward:
    """The reward proxy for one turn. Pure: the same record always scores the same."""
    r = record if isinstance(record, TurnRecord) else TurnRecord.from_json(record)
    parts: dict[str, float] = {}
    verdict = (r.verifier or {}).get("after_last_edit", "none")
    unresolved = len((r.verifier or {}).get("unresolved") or [])
    if verdict == "pass" and not unresolved:
        parts["verified"] = WEIGHTS["verified"]
    elif verdict == "fail" or unresolved:
        parts["broken"] = WEIGHTS["broken"]
    rejected = min(3, int((r.approvals or {}).get("rejected", 0)))
    if rejected:
        parts["rejected"] = WEIGHTS["rejected"] * rejected
    if r.stop_reason == "cancelled":
        parts["cancelled"] = WEIGHTS["cancelled"]
    elif r.stop_reason == "max_tokens":
        parts["truncated"] = WEIGHTS["truncated"]
    stalls = r.stalls or {}
    if stalls.get("blocks") or stalls.get("cycles"):
        parts["stalled"] = WEIGHTS["stalled"]
    elif stalls.get("notes"):
        parts["stalled"] = WEIGHTS["stalled"] / 2
    total_tokens = int((r.usage or {}).get("total", 0))
    if total_tokens:
        parts["tokens"] = WEIGHTS["tokens"] * min(1.0, total_tokens / TOKEN_BUDGET)
    if r.seconds:
        parts["time"] = WEIGHTS["time"] * min(1.0, r.seconds / SECONDS_BUDGET)
    known = False
    if update:
        known = True
        kept, reverted = int(update.get("kept", 0)), int(update.get("reverted", 0))
        judged = kept + reverted + int(update.get("edited", 0)) + int(update.get("removed", 0))
        if judged:
            if reverted:
                parts["reverted"] = WEIGHTS["reverted"] * (reverted / judged)
            if kept:
                parts["kept"] = WEIGHTS["kept"] * (kept / judged)
    value = max(-1.0, min(1.0, sum(parts.values())))
    return Reward(value=round(value, 4), parts={k: round(v, 4) for k, v in parts.items()}, known=known)


# ------------------------------------------------------------------------------ reconciliation


def judge_files(files: list[dict[str, Any]], root: str) -> dict[str, int]:
    """What became of the files a turn changed: kept (still as the agent left them), reverted (back to
    what they held before), edited (the person changed them further), removed (gone)."""
    counts = Counter()
    for entry in files:
        full = os.path.join(root, entry["path"])
        now = file_hash(full)
        after, before = entry.get("after"), entry.get("before")
        if after is None:  # the agent deleted it
            counts["kept" if now is None else "reverted"] += 1
        elif now is None:
            counts["removed"] += 1
        elif now == after:
            counts["kept"] += 1
        elif entry.get("known_before") and now == before:
            counts["reverted"] += 1
        else:
            counts["edited"] += 1
    return {"kept": counts["kept"], "reverted": counts["reverted"], "edited": counts["edited"], "removed": counts["removed"], "files": len(files)}


def reconcile(project: str, *, within_hours: float = 72.0, limit: int = 20, min_age_seconds: float = 120.0, now: float | None = None) -> list[dict[str, Any]]:
    """Judge recent turns in ``project`` that have not been judged yet. A turn is judged once, no sooner
    than ``min_age_seconds`` after it ended (the person has not had time to react before that). Returns
    the updates it wrote. ``now`` is the clock (injectable, so a test or a nightly job can ask "as of when")."""
    now = time.time() if now is None else now
    rows = state_store.recent_events(kind=KIND, limit=200, since=now - within_hours * 3600)
    judged = {row["payload"].get("id") for row in state_store.recent_events(kind=UPDATE_KIND, limit=1000, since=now - within_hours * 3600)}
    written = []
    for row in rows:
        payload = row["payload"]
        if not isinstance(payload, dict) or payload.get("id") in judged or payload.get("project") != project:
            continue
        files = payload.get("files") or []
        if now - (payload.get("ts", 0) + payload.get("seconds", 0)) < min_age_seconds:
            continue
        # Every aged turn is judged, not only the ones that changed files: a turn with no files is still a turn
        # whose reward is "known" — `score`'s kept/reverted half is simply zero for it. Learning that only sees
        # file-changing turns would be blind to answer-only and read-only ones (`bandit.py` learns off this).
        update = {"id": payload["id"], "at": now, **(judge_files(files, project) if files else {"kept": 0, "reverted": 0, "edited": 0, "removed": 0, "files": 0})}
        update["reward"] = score(payload, update).to_json()
        state_store.append_event(UPDATE_KIND, update, session_id=payload.get("session_id"), source="outcomes")
        written.append(update)
        if len(written) >= limit:
            break
    return written


# ------------------------------------------------------------------------------ reading them back


def recent(limit: int = 200, *, project: str | None = None, since: float | None = None) -> list[tuple[TurnRecord, dict[str, Any] | None]]:
    """Newest first: each record with its reconciliation update, if it has one."""
    rows = state_store.recent_events(kind=KIND, limit=limit, since=since)
    updates = {row["payload"].get("id"): row["payload"] for row in state_store.recent_events(kind=UPDATE_KIND, limit=limit * 2, since=since)}
    out = []
    for row in rows:
        payload = row["payload"]
        if not isinstance(payload, dict) or (project and payload.get("project") != project):
            continue
        try:
            out.append((TurnRecord.from_json(payload), updates.get(payload.get("id"))))
        except TypeError:
            continue
    return out


def summarize(records: list[tuple[TurnRecord, dict[str, Any] | None]]) -> dict[str, Any]:
    """The numbers a person (or a bandit) looks at: how often turns work, what they cost, where."""
    if not records:
        return {"turns": 0}
    rewards = [score(r, u) for r, u in records]
    verified = sum(1 for r, _ in records if (r.verifier or {}).get("after_last_edit") == "pass")
    broken = sum(1 for r, _ in records if (r.verifier or {}).get("after_last_edit") == "fail")
    judged = [(r, u) for r, u in records if u]
    by_route: dict[str, list[float]] = {}
    for (r, _u), rw in zip(records, rewards):
        by_route.setdefault(str((r.route or {}).get("target") or "none"), []).append(rw.value)
    return {
        "turns": len(records),
        "mean_reward": round(sum(rw.value for rw in rewards) / len(rewards), 4),
        "verified": verified,
        "broken": broken,
        "rejections": sum(int((r.approvals or {}).get("rejected", 0)) for r, _ in records),
        "auto_approved": sum(int((r.approvals or {}).get("auto", 0)) for r, _ in records),
        "reverted_turns": sum(1 for _r, u in judged if u.get("reverted")),
        "judged_turns": len(judged),
        "tokens": sum(int((r.usage or {}).get("total", 0)) for r, _ in records),
        "claude_code_usd": round(sum(float((r.claude_code or {}).get("cost_usd", 0)) for r, _ in records), 2),
        "mean_seconds": round(sum(r.seconds for r, _ in records) / len(records), 1),
        "by_route": {k: {"turns": len(v), "mean_reward": round(sum(v) / len(v), 4)} for k, v in sorted(by_route.items())},
    }


def main(argv: list[str] | None = None) -> int:
    import argparse

    parser = argparse.ArgumentParser(description="Outcome records: the harness's reward signal.")
    sub = parser.add_subparsers(dest="cmd", required=True)
    report = sub.add_parser("report", help="summarise recent turns")
    report.add_argument("--project", default=None)
    report.add_argument("--limit", type=int, default=200)
    recon = sub.add_parser("reconcile", help="judge recent turns' file changes (kept / reverted / edited)")
    recon.add_argument("project")
    args = parser.parse_args(argv)
    if args.cmd == "report":
        print(json.dumps(summarize(recent(args.limit, project=args.project)), indent=2))
    else:
        print(json.dumps(reconcile(os.path.realpath(args.project), min_age_seconds=0), indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

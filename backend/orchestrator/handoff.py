"""The handoff packet: what one worker leaves for the next, in a form any worker can resume from.

Provider sessions cannot continue each other — a Claude session id means nothing to ``agy``, an agy conversation
nothing to Ollama. So a handoff carries *state*, not a session: the plan step, how far it got, the checkpoint the
project is at, the diff so far (when the step's policy keeps partial work), what was verified, what failed, and the
assumptions still open. Typed and bounded, secret-scrubbed when built, fenced as untrusted data when rendered into
the next worker's prompt (``workers.prompting.render_handoff``): the next worker reads it as a report, never as orders.

Persisted in ``state.db`` (``handoff_packets``) so a run that pauses — every pool spent — resumes from the packet later,
in another process if need be.
"""

from __future__ import annotations

import json
import logging
import sqlite3
import time
import uuid
from dataclasses import asdict, dataclass, field
from typing import Any

import secret_patterns
import state_store

logger = logging.getLogger(__name__)

REASONS = ("quota_soft_limit_reached", "quota_hard_limit_reached", "provider_rate_limit", "worker_failed", "worker_unavailable")
MAX_PATCH_CHARS = 30_000
MAX_LIST = 40
MAX_ITEM_CHARS = 600


@dataclass
class HandoffPacket:
    handoff_id: str
    session_id: str
    timestamp: float
    reason: str
    plan_id: str
    step_index: int
    step_id: str
    step_status: str  # "pending" (nothing done yet) | "partial" (work kept) | "failed" (rolled back)
    checkpoint_hash: str | None
    from_worker: str
    to_worker: str | None = None
    diff_stat: str = ""
    diff_patch: str = ""
    files_touched: list[str] = field(default_factory=list)
    verifier_results: dict[str, str] = field(default_factory=dict)
    failed_attempts: list[str] = field(default_factory=list)
    open_assumptions: list[str] = field(default_factory=list)
    completed_steps: list[str] = field(default_factory=list)

    def to_json(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> "HandoffPacket":
        return cls(**{k: v for k, v in data.items() if k in cls.__dataclass_fields__})

    def context(self) -> dict[str, Any]:
        """What goes into the next worker's ``TaskPacket.handoff_context`` — the packet minus ledger bookkeeping."""
        data = self.to_json()
        for key in ("handoff_id", "session_id", "timestamp", "to_worker"):
            data.pop(key, None)
        return data


def _clip(items: list[Any], limit: int = MAX_LIST) -> list[str]:
    out = []
    for item in list(items)[:limit]:
        text, _ = secret_patterns.scrub(str(item))
        out.append(text[:MAX_ITEM_CHARS])
    return out


def build(
    *,
    session_id: str,
    reason: str,
    plan_id: str,
    step_index: int,
    step_id: str,
    step_status: str,
    checkpoint_hash: str | None,
    from_worker: str,
    to_worker: str | None = None,
    diff_stat: str = "",
    diff_patch: str = "",
    files_touched: list[str] | None = None,
    verifier_results: dict[str, str] | None = None,
    failed_attempts: list[str] | None = None,
    open_assumptions: list[str] | None = None,
    completed_steps: list[str] | None = None,
) -> HandoffPacket:
    """A packet with every free-text field secret-scrubbed and bounded. ``reason`` must be one of ``REASONS``."""
    if reason not in REASONS:
        raise ValueError(f"unknown handoff reason {reason!r}")
    patch, _ = secret_patterns.scrub(diff_patch or "")
    if len(patch) > MAX_PATCH_CHARS:
        patch = patch[:MAX_PATCH_CHARS] + "\n… (patch truncated)\n"
    stat, _ = secret_patterns.scrub(diff_stat or "")
    return HandoffPacket(
        handoff_id=f"handoff-{uuid.uuid4().hex[:12]}",
        session_id=session_id,
        timestamp=time.time(),
        reason=reason,
        plan_id=plan_id,
        step_index=int(step_index),
        step_id=step_id,
        step_status=step_status,
        checkpoint_hash=checkpoint_hash,
        from_worker=from_worker,
        to_worker=to_worker,
        diff_stat=stat[:2000],
        diff_patch=patch,
        files_touched=_clip(files_touched or [], 200),
        verifier_results={str(k)[:120]: _clip([v])[0] for k, v in list((verifier_results or {}).items())[:MAX_LIST]},
        failed_attempts=_clip(failed_attempts or []),
        open_assumptions=_clip(open_assumptions or []),
        completed_steps=_clip(completed_steps or [], 100),
    )


def save(packet: HandoffPacket) -> bool:
    try:
        state_store.connect().execute(
            "INSERT OR REPLACE INTO handoff_packets (handoff_id, session_id, ts, step_id, from_worker, to_worker, reason, packet_json) VALUES (?,?,?,?,?,?,?,?)",
            (packet.handoff_id, packet.session_id, packet.timestamp, packet.step_id, packet.from_worker, packet.to_worker, packet.reason, json.dumps(packet.to_json())),
        )
        state_store.append_event("handoff", {k: packet.to_json()[k] for k in ("handoff_id", "reason", "plan_id", "step_id", "step_status", "from_worker", "to_worker", "diff_stat")}, session_id=packet.session_id, source="orchestrator")
        return True
    except sqlite3.Error:
        logger.warning("handoff: could not persist %s", packet.handoff_id, exc_info=True)
        return False


def load(handoff_id: str) -> HandoffPacket | None:
    try:
        row = state_store.connect().execute("SELECT packet_json FROM handoff_packets WHERE handoff_id = ?", (handoff_id,)).fetchone()
    except sqlite3.Error:
        return None
    return HandoffPacket.from_json(json.loads(row["packet_json"])) if row else None


def for_session(session_id: str) -> list[HandoffPacket]:
    try:
        rows = state_store.connect().execute("SELECT packet_json FROM handoff_packets WHERE session_id = ? ORDER BY ts", (session_id,)).fetchall()
    except sqlite3.Error:
        return []
    return [HandoffPacket.from_json(json.loads(r["packet_json"])) for r in rows]


def recent(limit: int = 20) -> list[HandoffPacket]:
    try:
        rows = state_store.connect().execute("SELECT packet_json FROM handoff_packets ORDER BY ts DESC LIMIT ?", (int(limit),)).fetchall()
    except sqlite3.Error:
        return []
    return [HandoffPacket.from_json(json.loads(r["packet_json"])) for r in rows]

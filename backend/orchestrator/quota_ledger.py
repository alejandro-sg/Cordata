"""The quota ledger: how much of each provider pool is left, metered locally, corrected by the provider.

Two envelopes, both checked before and during every step (``docs/orchestrator-plan.md`` §3.4):

* **Per-plan** — dollars and tokens one run may spend, whatever worker it lands on (``envelope``). Blast-radius
  containment: a run past it stops; it is never "handed off" to spend more somewhere else.
* **Rolling pool windows** — each provider pool (the Claude Code subscription, the Antigravity pool, Ollama Cloud,
  local weights) has a cap over a rolling window (5 h by default) and optionally a weekly one (``state``). Past the
  **soft** line (80 %) the current step finishes and the next one goes to another worker; at the **hard** line (95 %)
  the step is stopped where it is — by giving the worker a budget that ends exactly there.

Ground truth is two signals, neither scraped from a web page:

1. **Self-metering.** Every worker result lands as one ``quota_usage`` row: tokens in/out, the provider's own dollar
   figure where it reports one (Claude Code's notional list cost), and one request.
2. **Provider signals.** Claude Code reports its real window utilisation on every run (``rate_limit_event``:
   ``five_hour`` and ``seven_day`` utilisation with reset times, observed live) — stored in ``quota_signals`` and
   used as the pool's level whenever it is higher than the meter (usage from the web app or another machine is
   invisible to the meter, not to the provider). A rate-limit error (``WorkerResult.rate_limited``) is recorded with
   ``is_error_limit`` and marks the pool exhausted until the provider's reset time (or the window rolls past it).
   A meter that thinks there is headroom loses to a provider that says there is none.

Pools are matched by worker family plus a model regex, first match wins — so Claude models served by Antigravity
(``agy`` + ``claude-*``) can be their own pool, separate from Gemini in the same CLI. Caps are configuration
(``orchestrator.pools``), because only the person knows their plan; the defaults are deliberately conservative.

Stdlib plus ``state_store``; never raises into a run (a ledger that cannot be read reports *unknown*, which the
dispatcher treats as "do not prefer").
"""

from __future__ import annotations

import json
import logging
import math
import re
import sqlite3
import time
from collections.abc import Callable, Iterable
from dataclasses import asdict, dataclass, field
from typing import Any

import state_store

logger = logging.getLogger(__name__)

LEVELS = ("ok", "soft", "hard", "exhausted")
UNITS = ("usd", "tokens", "requests")
FIVE_HOURS = 5 * 3600
WEEK = 7 * 24 * 3600


@dataclass(frozen=True)
class Pool:
    pool_id: str
    family: str
    model_regex: str = ".*"
    unit: str = "tokens"
    window_limit: float = math.inf
    window_seconds: int = FIVE_HOURS
    soft_threshold_pct: float = 0.80
    hard_threshold_pct: float = 0.95
    weekly_limit: float | None = None

    def matches(self, family: str, model: str | None) -> bool:
        return self.family == family and re.fullmatch(self.model_regex, model or "") is not None

    def to_json(self) -> dict[str, Any]:
        data = asdict(self)
        data["window_limit"] = None if math.isinf(self.window_limit) else self.window_limit
        return data

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> "Pool":
        known = {k: v for k, v in data.items() if k in cls.__dataclass_fields__}
        if known.get("window_limit") is None:
            known["window_limit"] = math.inf
        if known.get("unit") not in UNITS:
            raise ValueError(f"pool {known.get('pool_id')!r}: unit must be one of {UNITS}")
        return cls(**known)


#: Conservative defaults. A Claude Code window is metered in *notional* dollars (the CLI's list-price figure), the
#: Antigravity pools in tokens (agy reports no dollars), Ollama Cloud in requests, local weights not at all.
DEFAULT_POOLS: tuple[Pool, ...] = (
    Pool("claude_code", "claude_code", ".*", unit="usd", window_limit=20.0, weekly_limit=150.0),
    Pool("antigravity_claude", "antigravity", "claude-.*", unit="tokens", window_limit=3_000_000),
    Pool("antigravity", "antigravity", ".*", unit="tokens", window_limit=8_000_000),
    Pool("ollama_local", "ollama", r"laguna-.*|.*:local|local/.*", unit="requests"),
    Pool("ollama_cloud", "ollama", ".*", unit="requests", window_limit=600, window_seconds=3600),
)


@dataclass
class PoolState:
    pool_id: str
    family: str
    unit: str
    used: float
    limit: float | None
    pct: float
    level: str
    window_seconds: int
    weekly_used: float = 0.0
    weekly_limit: float | None = None
    limited_until: float | None = None
    #: How much of ``unit`` is left before the hard line — what a step may spend at most.
    to_hard: float | None = None
    to_soft: float | None = None
    #: The provider's own utilisation reading (max over its fresh windows), when it gave one, and which number won.
    provider_pct: float | None = None
    source: str = "meter"

    def to_json(self) -> dict[str, Any]:
        return asdict(self)


@dataclass
class EnvelopeState:
    session_id: str
    spent_usd: float
    spent_tokens: int
    max_usd: float | None
    max_tokens: int | None
    exhausted: bool
    remaining_usd: float | None = None
    remaining_tokens: int | None = None

    def to_json(self) -> dict[str, Any]:
        return asdict(self)


@dataclass
class UsageRow:
    session_id: str
    pool_id: str
    model: str
    tokens_in: int = 0
    tokens_out: int = 0
    cost_usd: float = 0.0
    requests: int = 1
    step_id: str | None = None
    is_error_limit: bool = False
    reset_ts: float | None = None
    ts: float = field(default_factory=time.time)
    role: str | None = None
    worker: str | None = None


def pools_from_settings(preferences: Any) -> tuple[Pool, ...]:
    """``orchestrator.pools`` (a list of pool objects) replaces the defaults; a malformed entry is skipped and logged —
    never a reason to run with *no* caps."""
    raw = None
    try:
        raw = preferences.get("orchestrator.pools") if preferences is not None else None
    except (AttributeError, TypeError):
        raw = None
    if not raw:
        return DEFAULT_POOLS
    if isinstance(raw, str):
        try:
            raw = json.loads(raw)
        except ValueError:
            logger.warning("orchestrator.pools is not JSON; using the defaults")
            return DEFAULT_POOLS
    pools: list[Pool] = []
    for item in raw if isinstance(raw, list) else []:
        try:
            pools.append(Pool.from_json(item))
        except (TypeError, ValueError) as error:
            logger.warning("orchestrator.pools: skipping %r (%s)", item, error)
    return tuple(pools) or DEFAULT_POOLS


_migrated_paths: set[str] = set()


def migrate(conn: sqlite3.Connection | None = None) -> None:
    """Guarded migration for quota_usage: add role and worker columns, ignoring sqlite3.OperationalError."""
    if conn is None:
        conn = state_store.connect()
    try:
        conn.execute("ALTER TABLE quota_usage ADD COLUMN role TEXT")
    except sqlite3.OperationalError:
        pass
    try:
        conn.execute("ALTER TABLE quota_usage ADD COLUMN worker TEXT")
    except sqlite3.OperationalError:
        pass


def _ensure_columns(conn: sqlite3.Connection) -> None:
    target = ""
    try:
        target = str(state_store.path())
    except Exception:
        pass
    if target and target in _migrated_paths:
        return
    migrate(conn)
    if target:
        _migrated_paths.add(target)


class QuotaLedger:
    """See the module docstring. ``clock`` is injectable so a test can roll a window forward."""

    def __init__(self, pools: Iterable[Pool] | None = None, *, clock: Callable[[], float] = time.time) -> None:
        self.pools: tuple[Pool, ...] = tuple(pools or DEFAULT_POOLS)
        self.clock = clock
        self._sync_pools()

    # --------------------------------------------------------------- pools

    def _conn(self) -> sqlite3.Connection:
        conn = state_store.connect()
        _ensure_columns(conn)
        return conn

    def _sync_pools(self) -> None:
        try:
            conn = self._conn()
            for pool in self.pools:
                conn.execute(
                    "INSERT OR REPLACE INTO quota_pools (pool_id, family, model_regex, unit, soft_threshold_pct, hard_threshold_pct, window_seconds, window_limit, weekly_limit) VALUES (?,?,?,?,?,?,?,?,?)",
                    (pool.pool_id, pool.family, pool.model_regex, pool.unit, pool.soft_threshold_pct, pool.hard_threshold_pct, pool.window_seconds, None if math.isinf(pool.window_limit) else pool.window_limit, pool.weekly_limit),
                )
        except sqlite3.Error as err:
            logger.warning("quota ledger: could not record the pool table: %s", err)

    def pool_for(self, family: str, model: str | None) -> Pool:
        for pool in self.pools:
            if pool.matches(family, model):
                return pool
        # An unconfigured family/model is its own uncapped pool — metered, never assumed to be free of limits
        # elsewhere: a provider limit still marks it exhausted.
        return Pool(f"{family}", family, ".*", unit="tokens")

    # --------------------------------------------------------------- recording

    def record(self, row: UsageRow) -> None:
        try:
            self._conn().execute(
                "INSERT INTO quota_usage (ts, session_id, step_id, pool_id, model, tokens_in, tokens_out, cost_usd, is_error_limit, requests, reset_ts, role, worker) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
                (row.ts, row.session_id, row.step_id, row.pool_id, row.model, int(row.tokens_in), int(row.tokens_out), float(row.cost_usd), 1 if row.is_error_limit else 0, int(row.requests), row.reset_ts, row.role, row.worker),
            )
        except sqlite3.Error:
            logger.warning("quota ledger: could not record usage", exc_info=True)

    def record_signal(self, pool_id: str, quota: dict[str, Any]) -> None:
        """The provider's own window readings (``WorkerResult.provider_quota``) → ``quota_signals`` rows."""
        windows = (quota or {}).get("windows") or {}
        now = self.clock()
        try:
            conn = self._conn()
            for name, window in windows.items():
                if not isinstance(window, dict) or not isinstance(window.get("utilization"), (int, float)):
                    continue
                conn.execute(
                    "INSERT INTO quota_signals (ts, pool_id, window, utilization, resets_at, status) VALUES (?,?,?,?,?,?)",
                    (now, pool_id, str(name), float(window["utilization"]), _float(window.get("resets_at")), str(quota.get("status") or "")),
                )
        except sqlite3.Error:
            logger.warning("quota ledger: could not record a provider signal", exc_info=True)

    def provider_reading(self, pool: Pool) -> float | None:
        """The highest *fresh* utilisation the provider reported for this pool: per window, the latest reading whose
        reset time has not passed (or, with no reset time, that is younger than the pool's window)."""
        now = self.clock()
        try:
            rows = self._conn().execute(
                "SELECT window, utilization, resets_at, ts FROM quota_signals WHERE pool_id = ? AND ts > ? ORDER BY ts DESC",
                (pool.pool_id, now - WEEK),
            ).fetchall()
        except sqlite3.Error:
            return None
        latest: dict[str, float] = {}
        for row in rows:
            if row["window"] in latest:
                continue
            fresh = (row["resets_at"] is not None and float(row["resets_at"]) > now) or (row["resets_at"] is None and float(row["ts"]) > now - pool.window_seconds)
            if fresh:
                latest[row["window"]] = float(row["utilization"])
        return max(latest.values()) if latest else None

    def record_result(
        self,
        *,
        session_id: str,
        step_id: str | None,
        family: str,
        model: str | None,
        result: Any,
        role: str | None = None,
        worker: str | None = None,
    ) -> UsageRow:
        """One worker result → one usage row in the pool it belongs to (and the provider's window readings, if any)."""
        pool = self.pool_for(family, model or getattr(result, "model", None))
        quota = getattr(result, "provider_quota", None) or {}
        if quota:
            self.record_signal(pool.pool_id, quota)
        row = UsageRow(
            session_id=session_id,
            pool_id=pool.pool_id,
            model=str(model or getattr(result, "model", None) or ""),
            tokens_in=int(getattr(result, "tokens_in", 0) or 0),
            tokens_out=int(getattr(result, "tokens_out", 0) or 0),
            cost_usd=float(getattr(result, "cost_usd", 0.0) or 0.0),
            requests=int(getattr(result, "requests", 1) or 1),
            step_id=step_id,
            is_error_limit=bool(getattr(result, "rate_limited", False)),
            reset_ts=_float(quota.get("resets_at")) if getattr(result, "rate_limited", False) else None,
            ts=self.clock(),
            role=role,
            worker=worker,
        )
        self.record(row)
        return row

    # --------------------------------------------------------------- reading

    @staticmethod
    def _amount(pool: Pool, row: sqlite3.Row) -> float:
        if pool.unit == "usd":
            return float(row["cost_usd"] or 0.0)
        if pool.unit == "tokens":
            return float((row["tokens_in"] or 0) + (row["tokens_out"] or 0))
        return float(row["requests"] or 1)

    def state(self, pool: Pool | str) -> PoolState:
        if isinstance(pool, str):
            pool = next((p for p in self.pools if p.pool_id == pool), Pool(pool, pool))
        now = self.clock()
        horizon = max(pool.window_seconds, WEEK if pool.weekly_limit else 0)
        try:
            rows = self._conn().execute("SELECT * FROM quota_usage WHERE pool_id = ? AND ts > ?", (pool.pool_id, now - horizon)).fetchall()
        except sqlite3.Error as err:
            logger.warning("quota ledger: could not read usage: %s", err)
            return PoolState(pool.pool_id, pool.family, pool.unit, 0.0, None, 1.0, "exhausted", pool.window_seconds)
        used = sum(self._amount(pool, r) for r in rows if r["ts"] > now - pool.window_seconds)
        weekly = sum(self._amount(pool, r) for r in rows) if pool.weekly_limit else 0.0
        limited_until = None
        for r in rows:
            if r["is_error_limit"]:
                until = float(r["reset_ts"]) if r["reset_ts"] else float(r["ts"]) + pool.window_seconds
                if until > now:
                    limited_until = max(limited_until or 0.0, until)
        limit = None if math.isinf(pool.window_limit) else float(pool.window_limit)
        pcts = [used / limit if limit else 0.0]
        if pool.weekly_limit:
            pcts.append(weekly / float(pool.weekly_limit))
        metered = max(pcts)
        provider = self.provider_reading(pool)
        pct = max(metered, provider) if provider is not None else metered
        source = "provider" if provider is not None and provider >= metered else "meter"
        if limited_until is not None:
            level, pct = "exhausted", max(pct, 1.0)
        elif pct >= 1.0:
            level = "exhausted"
        elif pct >= pool.hard_threshold_pct:
            level = "hard"
        elif pct >= pool.soft_threshold_pct:
            level = "soft"
        else:
            level = "ok"
        to_hard = to_soft = None
        if limit is not None:
            to_hard = max(0.0, limit * pool.hard_threshold_pct - used)
            to_soft = max(0.0, limit * pool.soft_threshold_pct - used)
        if pool.weekly_limit:
            weekly_left = max(0.0, float(pool.weekly_limit) * pool.hard_threshold_pct - weekly)
            to_hard = weekly_left if to_hard is None else min(to_hard, weekly_left)
        if provider is not None and limit is not None and provider > metered:
            # The provider says more is used than we metered (other sessions, the web app, another machine): what is
            # left is the provider's remainder, in our unit, at the cap's scale.
            to_hard = min(to_hard if to_hard is not None else math.inf, max(0.0, (pool.hard_threshold_pct - provider) * limit))
            to_soft = max(0.0, (pool.soft_threshold_pct - provider) * limit)
        if level == "exhausted":
            to_hard = 0.0
        return PoolState(pool.pool_id, pool.family, pool.unit, round(used, 6), limit, round(pct, 4), level, pool.window_seconds, round(weekly, 6), pool.weekly_limit, limited_until, to_hard, to_soft, provider, source)

    def states(self) -> list[PoolState]:
        return [self.state(p) for p in self.pools]

    def level(self, family: str, model: str | None) -> str:
        return self.state(self.pool_for(family, model)).level

    def step_allowance(self, family: str, model: str | None) -> dict[str, Any]:
        """What one step on this pool may spend before the hard line: ``{"budget_usd"|"budget_tokens": n}``, or ``{}``
        when the pool is uncapped (or counted in requests, which a worker cannot cut off mid-step)."""
        pool = self.pool_for(family, model)
        state = self.state(pool)
        if state.to_hard is None or pool.unit == "requests":
            return {}
        if pool.unit == "usd":
            return {"budget_usd": round(state.to_hard, 6)}
        return {"budget_tokens": int(state.to_hard)}

    def envelope(self, session_id: str, *, max_usd: float | None, max_tokens: int | None) -> EnvelopeState:
        try:
            row = self._conn().execute("SELECT COALESCE(SUM(cost_usd),0) AS usd, COALESCE(SUM(tokens_in+tokens_out),0) AS tok FROM quota_usage WHERE session_id = ?", (session_id,)).fetchone()
            spent_usd, spent_tokens = float(row["usd"]), int(row["tok"])
        except sqlite3.Error:
            logger.warning("quota ledger: could not read the envelope", exc_info=True)
            return EnvelopeState(session_id, 0.0, 0, max_usd, max_tokens, True)
        exhausted = (max_usd is not None and spent_usd >= max_usd) or (max_tokens is not None and spent_tokens >= max_tokens)
        return EnvelopeState(
            session_id, round(spent_usd, 6), spent_tokens, max_usd, max_tokens, exhausted,
            None if max_usd is None else round(max(0.0, max_usd - spent_usd), 6),
            None if max_tokens is None else max(0, max_tokens - spent_tokens),
        )

    def usage(self, session_id: str) -> list[dict[str, Any]]:
        try:
            return [dict(r) for r in self._conn().execute("SELECT * FROM quota_usage WHERE session_id = ? ORDER BY id", (session_id,)).fetchall()]
        except sqlite3.Error:
            return []

    def run_usage(self, run_id: str) -> list[dict[str, Any]]:
        try:
            rows = self._conn().execute("SELECT * FROM quota_usage WHERE session_id = ? ORDER BY id", (run_id,)).fetchall()
        except sqlite3.Error:
            return []
        groups: dict[tuple[str, str], dict[str, Any]] = {}
        for r in rows:
            role = r["role"] if "role" in r.keys() else None
            if not role:
                step_id = r["step_id"] or ""
                if step_id in ("plan", "research", "conformance", "harden", "audit"):
                    role = step_id
                else:
                    role = "execute"
            worker = r["worker"] if "worker" in r.keys() else None
            if not worker:
                worker = f"{r['pool_id'] or ''}:{r['model'] or ''}"
            key = (role, worker)
            if key not in groups:
                groups[key] = {
                    "role": role,
                    "worker": worker,
                    "calls": 0,
                    "tokens_in": 0,
                    "tokens_out": 0,
                    "cost_usd": 0.0,
                }
            entry = groups[key]
            calls = r["requests"] if r["requests"] is not None else 1
            entry["calls"] += int(calls)
            entry["tokens_in"] += int(r["tokens_in"] or 0)
            entry["tokens_out"] += int(r["tokens_out"] or 0)
            entry["cost_usd"] += float(r["cost_usd"] or 0.0)

        out = []
        for key in sorted(groups.keys()):
            entry = groups[key]
            entry["cost_usd"] = round(entry["cost_usd"], 6)
            out.append(entry)
        return out


def _float(value: Any) -> float | None:
    try:
        return float(value) if value is not None else None
    except (TypeError, ValueError):
        return None

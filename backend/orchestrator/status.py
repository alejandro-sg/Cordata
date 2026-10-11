"""Where the orchestrator stands, for the front doors — read from ``state.db``, no model call, no tokens.

``status_data()`` is the one payload every front door renders: the TUI's ``/status`` (ACP ext method
``_sierralab/orchestrator/status``), Discord's ``!status`` card, and ``python -m orchestrator status``. Each pool's
headroom (with the provider's own reading when it gave one), the recent runs and the recent handoffs.
"""

from __future__ import annotations

import os
import time
from typing import Any

GAUGE_WIDTH = 12


def status_data(cwd: str | None = None, *, limit: int = 6) -> dict[str, Any]:
    from . import config, handoff, machine

    try:
        import settings_store

        preferences = settings_store.current(cwd or os.getcwd())
    except Exception:  # unreadable settings: the default pools, still a useful answer
        preferences = None
    ledger = config.ledger(preferences)
    pools = [p.to_json() for p in ledger.states()]
    runs = [
        {
            "run_id": r.run_id,
            "state": r.state,
            "goal": r.goal[:120],
            "cwd": r.cwd,
            "updated": r.updated,
            "handoffs": len(r.handoffs),
            "drift_retries": r.drift_retries,
            "replans": r.replans,
            "reason": (r.reason or "")[:200],
        }
        for r in machine.recent(limit)
    ]
    handoffs = [
        {k: p.to_json()[k] for k in ("handoff_id", "session_id", "timestamp", "reason", "step_id", "step_status", "from_worker", "to_worker")}
        for p in handoff.recent(limit)
    ]
    return {"pools": pools, "runs": runs, "handoffs": handoffs, "ts": time.time()}


def gauge(pct: float | None, width: int = GAUGE_WIDTH) -> str:
    if pct is None:
        return "·" * width
    filled = max(0, min(width, round(float(pct) * width)))
    return "█" * filled + "░" * (width - filled)


def _compact(n: float) -> str:
    n = float(n)
    if n >= 1_000_000:
        return f"{n / 1_000_000:.1f}M".replace(".0M", "M")
    if n >= 10_000:
        return f"{n / 1000:.0f}k"
    return f"{n:.2f}".rstrip("0").rstrip(".") if n % 1 else f"{n:.0f}"


LEVEL_MARK = {"ok": "🟢", "soft": "🟡", "hard": "🟠", "exhausted": "🔴"}


def pool_line(pool: dict[str, Any], *, marks: bool = False) -> str:
    """``claude_code  ███████░░░░░  58%  soft  11.6/20 usd · provider`` — one pool, one line."""
    limit = pool.get("limit")
    pct = pool.get("pct")
    capped = bool(limit) or pool.get("provider_pct") is not None
    used = f"{_compact(pool['used'])}/{_compact(limit)} {pool['unit']}" if limit else f"{_compact(pool['used'])} {pool['unit']}"
    mark = (LEVEL_MARK.get(pool.get("level", "ok"), "") + " ") if marks else ""
    source = " · provider" if pool.get("source") == "provider" else ""
    until = ""
    if pool.get("limited_until"):
        until = f" · resets {time.strftime('%H:%M', time.localtime(float(pool['limited_until'])))}"
    pct_text = f"{float(pct) * 100:4.0f}%" if capped and pct is not None else "  — "
    return f"{mark}{pool['pool_id']:<18} {gauge(pct if capped else None)} {pct_text}  {pool.get('level', 'ok'):<9} {used}{source}{until}"


def handoff_line(item: dict[str, Any]) -> str:
    when = time.strftime("%H:%M", time.localtime(float(item.get("timestamp") or 0)))
    return f"{when} {item.get('reason', '')} · step {item.get('step_id', '')} · {item.get('from_worker', '')} → {item.get('to_worker') or '—'}"


def run_line(run: dict[str, Any]) -> str:
    extra = f" · {run['handoffs']} handoff(s)" if run.get("handoffs") else ""
    return f"{run['run_id']} {run['state']}: {run['goal'][:70]}{extra}"


def render(data: dict[str, Any], *, marks: bool = False) -> str:
    card_width = 72
    top = "╭── Quota Pools " + "─" * (card_width - 16) + "╮"
    bot = "╰" + "─" * card_width + "╯"
    lines = [top] + [f"│  {pool_line(p, marks=marks)}" for p in data.get("pools", [])] + [bot]

    runs = data.get("runs") or []
    lines.append("╭── Orchestrator Runs " + "─" * (card_width - 22) + "╮")
    if runs:
        lines += [f"│  {run_line(r)}" for r in runs]
    else:
        lines.append("│  (none)")
    lines.append(bot)

    handoffs = data.get("handoffs") or []
    lines.append("╭── Recent Handoffs " + "─" * (card_width - 20) + "╮")
    if handoffs:
        lines += [f"│  {handoff_line(h)}" for h in handoffs]
    else:
        lines.append("│  (none)")
    lines.append(bot)
    return "\n".join(lines)

"""The default worker roster and the settings that shape a run.

The roster is every (worker, model) pair the dispatcher may choose from, each with its **tier** and a relative
**price** (strong = 1). Prices are what one task tends to cost against the pool it draws on — for subscription pools a
notional figure, which is the point: the dispatcher trades quality against *quota*, and quota is what runs out.

    claude_code   haiku (fast) · sonnet (standard) · opus (strong)
    antigravity   gemini-3.8-flash (fast) · claude-sonnet-5-5 (standard) · gemini-3.1-pro (strong)
    ollama        glm-5.3-flash (fast) · kimi-k2.7-code (standard)

A family is in the roster when ``orchestrator.workers`` names it and it is usable here: its CLI is installed, or (for
Ollama) the local daemon answers. Nothing is probed over the network beyond that.
"""

from __future__ import annotations

import functools
import os
import shutil
from typing import Any

from workers import WorkerFamily, build_worker

from .dispatch import LearnedDispatcher, Spec
from .executor import Candidate
from .machine import RunConfig
from .quota_ledger import QuotaLedger, pools_from_settings

DEFAULT_WORKERS = ("claude_code", "antigravity", "ollama")

ROSTER: tuple[tuple[str, str, str, float, frozenset[str] | None], ...] = (
    # family, model, tier, price, roles (None = every role)
    ("claude_code", "haiku", "fast", 0.10, None),
    ("claude_code", "sonnet", "standard", 0.35, None),
    ("claude_code", "opus", "strong", 1.00, None),
    ("antigravity", "gemini-3.8-flash-medium", "fast", 0.05, None),
    ("antigravity", "claude-sonnet-5-5-medium", "standard", 0.30, None),
    ("antigravity", "gemini-3.1-pro-high", "strong", 0.60, None),
    ("ollama", "glm-5.3-flash", "fast", 0.02, frozenset({"plan", "execute", "audit"})),
    ("ollama", "kimi-k2.7-code", "standard", 0.05, None),
)


def setting(preferences: Any, key: str, default: Any) -> Any:
    try:
        value = preferences.get(key, default) if preferences is not None else default
    except (AttributeError, TypeError):
        return default
    return default if value is None else value


def enabled_families(preferences: Any) -> list[str]:
    raw = str(setting(preferences, "orchestrator.workers", ",".join(DEFAULT_WORKERS)) or "")
    wanted = [f.strip() for f in raw.split(",") if f.strip()]
    return [f for f in wanted if f in {w.value for w in WorkerFamily}]


@functools.lru_cache(maxsize=8)
def family_available(family: str) -> bool:
    if family == "claude_code":
        return bool(os.environ.get("SIERRALAB_CLAUDE_BIN") or shutil.which("claude") or os.path.exists(os.path.expanduser("~/.local/bin/claude")))
    if family == "antigravity":
        return bool(os.environ.get("SIERRALAB_AGY_BIN") or shutil.which("agy") or os.path.exists(os.path.expanduser("~/.local/bin/agy")))
    if family == "ollama":
        try:
            import ollama_cloud

            return ollama_cloud.daemon_problem(timeout=1.0) is None
        except Exception:
            return False
    return False


def roster(families: list[str] | None = None, *, workers: dict[str, Any] | None = None) -> list[Spec]:
    """The specs for ``families`` (default: all). ``workers`` maps a family to a ready worker (tests pass fakes)."""
    wanted = families if families is not None else list(DEFAULT_WORKERS)
    built: dict[str, Any] = dict(workers or {})
    specs: list[Spec] = []
    for family, model, tier, price, roles in ROSTER:
        if family not in wanted:
            continue
        worker = built.get(family)
        if worker is None:
            worker = built[family] = build_worker(family)
        specs.append(Spec(Candidate(worker, model), roles=roles or frozenset({"plan", "execute", "harden", "audit"}), tier=tier, price=price))
    return specs


def run_config(preferences: Any) -> RunConfig:
    max_tokens = float(setting(preferences, "orchestrator.maxTokens", 0.0) or 0.0)
    return RunConfig(
        max_usd=float(setting(preferences, "orchestrator.maxUsd", 2.0)),
        max_tokens=int(max_tokens) if max_tokens > 0 else None,
        harden=bool(setting(preferences, "orchestrator.harden", True)),
        tier2=bool(setting(preferences, "orchestrator.tier2", True)),
    )


def dispatcher(preferences: Any, *, ledger: QuotaLedger | None = None, project: str | None = None, mode: str | None = None, families: list[str] | None = None, workers: dict[str, Any] | None = None) -> LearnedDispatcher:
    import bandit

    fams = families if families is not None else enabled_families(preferences)
    return LearnedDispatcher(
        specs=roster(fams, workers=workers),
        ledger=ledger,
        mode=mode or str(setting(preferences, "orchestrator.dispatch", "shadow")),
        lam=float(setting(preferences, "orchestrator.lambda", 0.3)),
        mu=float(setting(preferences, "orchestrator.mu", 0.5)),
        table=bandit.for_project(project),
        available=lambda spec: family_available(spec.candidate.family),
    )


def ledger(preferences: Any) -> QuotaLedger:
    return QuotaLedger(pools_from_settings(preferences))

"""The dispatch policy: which worker (and model, tier, effort) takes each role, cheapest competent first.

    Score(w) = Q̂(w, role, task_class) − λ·NormalizedCost(w) − μ·QuotaScarcity(w)

1. **Task class** (``classify``): Laya's reading when it is loaded and confident (route + complexity, one ~18 ms pass),
   otherwise a keyword reading of the goal — ``docs`` · ``test`` · ``bugfix`` · ``refactor`` · ``feature`` ·
   ``general``, with ``/large`` for long multi-part goals.
2. **Hard gates first, never learned.** A candidate must serve the role, have its CLI, and sit in a pool that is not
   past its hard line; the harden role excludes every model family that executed the change. The bandit only ever
   *orders* what passed the gates — it cannot add a candidate a gate removed (the "nothing learned may lower a gate"
   invariant, held here by construction: gates filter, scores sort).
3. **Learned order — the cheapest competent first.** Arms whose ``Q̂`` is statistically indistinguishable from the best
   arm's (within one standard error) are *competent*; they go first, cheapest first (cost + μ·scarcity). The rest
   follow by the score above. ``Q̂`` is the candidate's win rate for this role and task class from the bandit table
   (``bandit.Bandit``, contexts ``orch|<role>|<class>|<point>``). A candidate with fewer than ``min_pulls`` outcomes
   borrows the learned value of its **tier** (``fast`` · ``standard`` · ``strong``) and **effort** — the two decision
   points the in-session harness reserved and never acted on; here they have a per-task seam, so they are acted on.
   Cost is the candidate's observed mean, normalised by the role's budget, with its tier's list price as the prior;
   scarcity is how far its pool is into its window.
4. **Shadow first.** ``mode="shadow"`` computes and logs the learned ranking (``bandit`` ``decide`` events) and returns
   the role's default order (``role_order``: standard tier first for plan, research, conformance; strong first for
   execute, harden; standard then strong for audit) — so a policy is judged on the ledger (``replay``) before it ever
   acts. ``live`` returns the learned order, with ε-exploration on the top slot.

``replay`` is the offline judge: the rejection-sampling estimator over logged, uniformly explored decisions — for each
logged (context, arm, outcome) it counts the outcome only when the policy under test would have picked that arm.
Unbiased for a uniform logging policy, so "the learned policy passes as often at lower cost than always-strongest"
is a measured statement, not a hope.
"""

from __future__ import annotations

import logging
import random
import re
from collections.abc import Callable, Iterable
from dataclasses import dataclass, field
from typing import Any

import bandit as bandit_mod
import state_store

from .executor import Candidate
from .quota_ledger import QuotaLedger

logger = logging.getLogger(__name__)

TIERS = ("fast", "standard", "strong")
TIER_RANK = {"fast": 0, "standard": 1, "strong": 2}
#: Prior quality and list price by tier, before anything is learned. Price is relative (strong = 1).
TIER_PRIOR_Q = {"fast": 0.55, "standard": 0.7, "strong": 0.8}
TIER_PRICE = {"fast": 0.08, "standard": 0.3, "strong": 1.0}
ROLE_PRIOR: dict[str, tuple[str, ...]] = {
    "plan": ("standard", "fast", "strong"),
    "research": ("standard", "fast", "strong"),
    "conformance": ("standard", "fast", "strong"),
    "execute": ("strong", "standard", "fast"),
    "harden": ("strong", "standard", "fast"),
    "audit": ("standard", "strong", "fast"),
}
POINT_FOR_ROLE = {
    "plan": "planner_pool",
    "research": "planner_pool",
    "conformance": "planner_pool",
    "execute": "executor_pool",
    "harden": "reviewer_pool",
    "audit": "reviewer_pool",
}
POINTS = ("planner_pool", "executor_pool", "reviewer_pool", "tier", "effort")
#: The smallest quality gap the policy treats as "as good": two arms within it are interchangeable on quality.
MIN_TOLERANCE = 0.01
#: Pseudo-outcomes an arm's estimate is shrunk toward its tier's with.
SHRINK = 20.0

_CLASSES = (
    ("docs", r"\b(readme|docs?|documentation|docstring|comment|changelog|typo)\b"),
    ("test", r"\b(tests?|coverage|unit test|spec)\b"),
    ("bugfix", r"\b(fix|bug|error|crash|fail(s|ing|ure)?|broken|regression|exception)\b"),
    ("refactor", r"\b(refactor|rename|extract|move|split|clean ?up|simplify|dedupe)\b"),
    ("feature", r"\b(add|implement|create|support|build|introduce|new)\b"),
)


def classify(goal: str, laya: Any = None) -> str:
    """The task class a goal belongs to. ``laya`` (optional) is a ``RouteDecision``-like reading; it only adds the size
    suffix — the class itself stays keyword-based, so it is stable across model updates."""
    text = (goal or "").lower()
    kind = next((name for name, pattern in _CLASSES if re.search(pattern, text)), "general")
    large = len(text) > 600 or text.count("\n- ") >= 4 or len(re.findall(r"\b(and then|also|plus)\b", text)) >= 3
    complexity = getattr(laya, "complexity", None)
    if isinstance(complexity, (int, float)) and complexity >= 0.7:
        large = True
    return f"{kind}/large" if large else kind


@dataclass
class Spec:
    """A candidate plus what the policy needs to know about it."""

    candidate: Candidate
    roles: frozenset[str] = frozenset({"plan", "execute", "harden", "audit"})
    tier: str = "standard"
    price: float | None = None  # relative cost per task (strong = 1); defaults to the tier's

    @property
    def key(self) -> str:
        return self.candidate.key

    @property
    def unit_price(self) -> float:
        return TIER_PRICE.get(self.tier, 0.3) if self.price is None else float(self.price)


class StaticDispatcher:
    """Fixed per-role order. For tests, and the baseline the learned policy is judged against."""

    def __init__(self, roles: dict[str, list[Candidate]]) -> None:
        self.roles = roles
        self.observed: list[tuple[str, str, str, dict[str, Any]]] = []

    def rank(self, role: str, task_class: str, *, exclude_model_families: set[str] | None = None) -> list[Candidate]:
        exclude = exclude_model_families or set()
        return [c for c in self.roles.get(role, []) if c.model_family not in exclude]

    def observe(self, role: str, task_class: str, candidate: Candidate, outcome: dict[str, Any]) -> None:
        self.observed.append((role, task_class, candidate.key, dict(outcome)))


def ctx(role: str, task_class: str, point: str) -> str:
    return f"orch|{role}|{task_class}|{point}"


@dataclass
class LearnedDispatcher:
    specs: list[Spec]
    ledger: QuotaLedger | None = None
    mode: str = "shadow"  # off | shadow | live
    lam: float = 0.3
    mu: float = 0.5
    explore: float = 0.1
    min_pulls: int = 3
    table: bandit_mod.Bandit | None = None
    rng: random.Random = field(default_factory=random.Random)
    available: Callable[[Spec], bool] = lambda spec: True
    decisions: list[dict[str, Any]] = field(default_factory=list)
    #: Write final outcomes to the ledger (off for offline training, which must not log what it replays).
    log_outcomes: bool = True
    #: How many standard errors below the best arm still counts as "as good" (0 = only ``MIN_TOLERANCE``).
    z: float = 1.5
    _last: dict[tuple[str, str], dict[str, Any]] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if self.table is None:
            self.table = bandit_mod.for_project(None)

    # ------------------------------------------------------------------ gates and the default

    def eligible(self, role: str, exclude_model_families: set[str] | None = None) -> list[Spec]:
        exclude = exclude_model_families or set()
        gate_role = "plan" if role in ("research", "conformance") else role
        out = []
        for spec in self.specs:
            if (gate_role not in spec.roles and role not in spec.roles) or spec.candidate.model_family in exclude or not self.available(spec):
                continue
            if self.ledger is not None and self.ledger.level(spec.candidate.family, spec.candidate.model) in ("hard", "exhausted"):
                continue
            out.append(spec)
        return out

    @staticmethod
    def default_order(specs: list[Spec]) -> list[Spec]:
        """Strongest first; among equals, the cheaper one."""
        return sorted(specs, key=lambda s: (-TIER_RANK.get(s.tier, 1), s.unit_price))

    @staticmethod
    def role_order(specs: list[Spec], role: str) -> list[Spec]:
        """Role-prior default order: cheapest competent tier by role, ties broken by unit_price then key."""
        prior = ROLE_PRIOR.get(role, ("strong", "standard", "fast"))
        rank_map = {tier: i for i, tier in enumerate(prior)}
        return sorted(
            specs,
            key=lambda s: (
                rank_map.get(s.tier, len(prior)),
                s.unit_price,
                s.key,
            ),
        )

    # ------------------------------------------------------------------ scoring

    def _stat(self, context: str, arm: str) -> dict[str, Any]:
        assert self.table is not None
        return ((self.table.contexts.get(context) or {}).get("arms") or {}).get(arm) or {}

    def prior(self, spec: Spec, role: str, task_class: str) -> float:
        """What a candidate's tier (and, when it names one, its effort) has earned in this context — every arm of that
        tier pooled, so it fills up fast — else the tier's prior. The ``tier`` and ``effort`` decision points, acted on."""
        borrowed = []
        points = [("tier", spec.tier)] + ([("effort", spec.candidate.effort)] if spec.candidate.effort else [])
        for point_name, arm in points:
            stat = self._stat(ctx(role, task_class, point_name), arm)
            n = int(stat.get("n", 0) or 0)
            if n >= self.min_pulls:
                borrowed.append((float(stat.get("wins", 0.0)) + 1.0) / (n + 2.0))
        return sum(borrowed) / len(borrowed) if borrowed else TIER_PRIOR_Q.get(spec.tier, 0.6)

    def quality(self, spec: Spec, role: str, task_class: str) -> float:
        """Q̂: the candidate's own win rate shrunk toward its tier's (``SHRINK`` pseudo-outcomes). One arm's few
        outcomes are noisy; picking the best of several noisy estimates is the winner's curse — it picks whoever got
        lucky. Shrinking toward the pooled tier value keeps same-tier arms close until their own evidence separates
        them, which is what lets the cheaper of two equals win."""
        own = self._stat(ctx(role, task_class, POINT_FOR_ROLE.get(role, "executor_pool")), spec.key)
        n = int(own.get("n", 0) or 0)
        return (float(own.get("wins", 0.0)) + SHRINK * self.prior(spec, role, task_class)) / (n + SHRINK)

    def cost(self, spec: Spec, role: str, task_class: str) -> float:
        own = self._stat(ctx(role, task_class, POINT_FOR_ROLE.get(role, "executor_pool")), spec.key)
        n = int(own.get("n", 0) or 0)
        prior = min(1.0, spec.unit_price)
        if n == 0:
            return prior
        return (float(own.get("cost", prior)) * n + prior * 2.0) / (n + 2.0)

    def scarcity(self, spec: Spec) -> float:
        if self.ledger is None:
            return 0.0
        state = self.ledger.state(self.ledger.pool_for(spec.candidate.family, spec.candidate.model))
        return max(0.0, min(1.0, state.pct))

    def score(self, spec: Spec, role: str, task_class: str) -> float:
        return self.quality(spec, role, task_class) - self.lam * self.cost(spec, role, task_class) - self.mu * self.scarcity(spec)

    def _evidence(self, spec: Spec, role: str, task_class: str) -> int:
        return int(self._stat(ctx(role, task_class, POINT_FOR_ROLE.get(role, "executor_pool")), spec.key).get("n", 0) or 0)

    def _tier_stat(self, tier: str, role: str, task_class: str) -> tuple[float, int]:
        stat = self._stat(ctx(role, task_class, "tier"), tier)
        n = int(stat.get("n", 0) or 0)
        if n < self.min_pulls:
            return TIER_PRIOR_Q.get(tier, 0.6), 0
        return (float(stat.get("wins", 0.0)) + 1.0) / (n + 2.0), n

    def cheapest_competent(self, specs: list[Spec], role: str, task_class: str, scored: dict[str, float]) -> list[Spec]:
        """The plan's rule — *the cheapest worker whose expected quality satisfies the threshold* — decided where the
        evidence is: first between **tiers** (every arm of a tier pooled, so the estimates are tight), then between the
        arms of the competent tiers.

        A tier is competent when its pooled Q̂ is within ``z`` standard errors of the difference (at least
        ``MIN_TOLERANCE``) of the best tier's. Inside a competent tier an arm is competent unless its own evidence puts
        it significantly below its tier. Competent arms go first, cheapest (plus quota scarcity) first; the rest follow
        by score. With no evidence (fewer than ``min_pulls`` outcomes for the best tier) the tolerance is the minimum and
        the tier priors decide — strongest first."""
        tiers = sorted({s.tier for s in specs}, key=lambda t: -TIER_RANK.get(t, 1))
        stats = {t: self._tier_stat(t, role, task_class) for t in tiers}
        best = max(tiers, key=lambda t: (stats[t][0], TIER_RANK.get(t, 1)))
        q_best, n_best = stats[best]
        competent_tiers = set()
        for tier in tiers:
            q, n = stats[tier]
            if n_best < self.min_pulls or n < self.min_pulls:
                tolerance = MIN_TOLERANCE
            else:
                se = (q_best * (1 - q_best) / (n_best + 2) + q * (1 - q) / (n + 2)) ** 0.5
                tolerance = max(MIN_TOLERANCE, self.z * se)
            if q >= q_best - tolerance:
                competent_tiers.add(tier)
        competent: list[Spec] = []
        for spec in specs:
            if spec.tier not in competent_tiers:
                continue
            own = self._stat(ctx(role, task_class, POINT_FOR_ROLE.get(role, "executor_pool")), spec.key)
            n = int(own.get("n", 0) or 0)
            q_tier = stats[spec.tier][0]
            if n >= self.min_pulls:
                q_own = (float(own.get("wins", 0.0)) + 1.0) / (n + 2.0)
                if q_own < q_tier - max(MIN_TOLERANCE, 2.0 * (q_tier * (1 - q_tier) / (n + 2)) ** 0.5):
                    continue  # this arm is significantly worse than its tier
            competent.append(spec)
        rest = [s for s in specs if s not in competent]
        competent.sort(key=lambda s: (self.cost(s, role, task_class) + self.mu * self.scarcity(s), -self.quality(s, role, task_class)))
        rest.sort(key=lambda s: (-scored[s.key], -TIER_RANK.get(s.tier, 1)))
        return competent + rest

    # ------------------------------------------------------------------ the policy

    def rank(self, role: str, task_class: str, *, exclude_model_families: set[str] | None = None) -> list[Candidate]:
        specs = self.eligible(role, exclude_model_families)
        default = self.role_order(specs, role)
        if self.mode not in ("shadow", "live") or not specs:
            return [s.candidate for s in default]
        scored = {s.key: round(self.score(s, role, task_class), 4) for s in specs}
        learned = self.cheapest_competent(specs, role, task_class, scored)
        acted = self.mode == "live"
        if acted and len(learned) > 1 and self.explore > 0 and self.rng.random() < self.explore:
            pick = self.rng.randrange(1, len(learned))
            learned = [learned[pick]] + learned[:pick] + learned[pick + 1 :]
        chosen = learned if acted else default
        info = {
            "point": POINT_FOR_ROLE.get(role, "executor_pool"),
            "context": ctx(role, task_class, POINT_FOR_ROLE.get(role, "executor_pool")),
            "default": default[0].key,
            "arm": learned[0].key,
            "acted": acted and learned[0].key != default[0].key,
            "mode": self.mode,
            "propensity": round((1 - self.explore) + self.explore / max(1, len(specs)), 4) if acted else 1.0,
            "scores": scored,
        }
        self.decisions.append(info)
        self._last[(role, task_class)] = info
        if self.log_outcomes:
            bandit_mod.decision_event(info, turn=f"orch:{role}:{task_class}")
        return [s.candidate for s in chosen]

    def observe(self, role: str, task_class: str, candidate: Candidate, outcome: dict[str, Any]) -> None:
        """Fold one outcome in: ``ok`` (bool) and ``norm_cost`` (relative cost; the spec's price when absent). A
        ``final`` outcome (the run's verdict) is also written to the ledger as an ``orch_outcome`` event — the rows
        ``replay`` judges a policy on."""
        spec = next((s for s in self.specs if s.key == candidate.key), None)
        if spec is None or self.table is None:
            return
        ok = bool(outcome.get("ok"))
        cost = float(outcome.get("norm_cost", spec.unit_price))
        value = (1.0 if ok else 0.0) - self.lam * cost
        point = POINT_FOR_ROLE.get(role, "executor_pool")
        for point_name, arm in ((point, spec.key), ("tier", spec.tier), ("effort", candidate.effort or "default")):
            self.table.observe_arm(ctx(role, task_class, point_name), arm, value=value, win=ok, cost=cost, save=False)
        if self.log_outcomes:
            self.table.save()  # offline training keeps its table in memory
        if outcome.get("final") and self.log_outcomes:
            decision = self._last.get((role, task_class), {})
            state_store.append_event(
                "orch_outcome",
                {"role": role, "task_class": task_class, "arm": spec.key, "tier": spec.tier, "effort": candidate.effort, "ok": ok, "cost": round(cost, 4),
                 "mode": self.mode, "propensity": decision.get("propensity"), "default": decision.get("default")},
                source="orchestrator",
            )


# ---------------------------------------------------------------------- offline judging


@dataclass
class Logged:
    """One logged decision with its outcome: what the replay estimator reads."""

    role: str
    task_class: str
    arm: str
    ok: bool
    cost: float


def replay(logs: Iterable[Logged], choose: Callable[[str, str, list[str]], str], arms: Callable[[str], list[str]]) -> dict[str, Any]:
    """Rejection-sampling replay: the value of policy ``choose(role, task_class, arms) -> arm`` on uniformly explored
    logs. Returns ``{"matched", "pass_rate", "cost_per_task", "cost_per_success"}``."""
    matched = wins = 0
    cost = 0.0
    for row in logs:
        if choose(row.role, row.task_class, arms(row.role)) != row.arm:
            continue
        matched += 1
        wins += 1 if row.ok else 0
        cost += row.cost
    return {
        "matched": matched,
        "pass_rate": round(wins / matched, 4) if matched else None,
        "cost_per_task": round(cost / matched, 4) if matched else None,
        "cost_per_success": round(cost / wins, 4) if wins else None,
    }


def logged_from_ledger(limit: int = 5000) -> list[Logged]:
    """The ``orch_outcome`` rows in ``state.db``, oldest first."""
    rows = list(reversed(state_store.recent_events(kind="orch_outcome", limit=limit)))
    out = []
    for row in rows:
        payload = row.get("payload") if isinstance(row.get("payload"), dict) else {}
        try:
            out.append(Logged(str(payload["role"]), str(payload["task_class"]), str(payload["arm"]), bool(payload["ok"]), float(payload.get("cost", 0.0))))
        except (KeyError, TypeError, ValueError):
            continue
    return out


def evaluate(logs: list[Logged], specs: list[Spec], *, train_fraction: float = 0.5, lam: float = 0.3, min_pulls: int = 3, z: float = 1.5) -> dict[str, Any]:
    """Train a learned policy on the first part of ``logs`` and judge it, and the always-strongest default, by replay on
    the rest. Offline: an in-memory table, nothing written to the ledger."""
    import os
    import tempfile

    cut = int(len(logs) * train_fraction)
    train, test = logs[:cut], logs[cut:]
    table = bandit_mod.Bandit(None, path=os.path.join(tempfile.mkdtemp(prefix="replay-"), "table.json"))
    learner = LearnedDispatcher(specs=specs, mode="live", lam=lam, mu=0.0, explore=0.0, min_pulls=min_pulls, table=table, log_outcomes=False, z=z)
    by_key = {s.key: s for s in specs}
    for row in train:
        spec = by_key.get(row.arm)
        if spec is not None:
            learner.observe(row.role, row.task_class, spec.candidate, {"ok": row.ok, "norm_cost": row.cost})

    def arms(role: str) -> list[str]:
        return [s.key for s in specs if role in s.roles]

    def learned(role: str, task_class: str, _arms: list[str]) -> str:
        ranked = learner.rank(role, task_class)
        return ranked[0].key if ranked else ""

    def strongest(role: str, task_class: str, _arms: list[str]) -> str:
        ranked = LearnedDispatcher.default_order([s for s in specs if role in s.roles])
        return ranked[0].key if ranked else ""

    return {"train": len(train), "test": len(test), "learned": replay(test, learned, arms), "strongest": replay(test, strongest, arms)}
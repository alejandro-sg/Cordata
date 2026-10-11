"""The dispatch policy: which worker (and model, tier, effort) takes each role, cheapest competent first.

    Score(w) = Q̂(w, role, task_class) − λ·NormalizedCost(w) − μ·QuotaScarcity(w)

1. **Task class** (``classify``): Laya's reading when it is loaded and confident (route + complexity, one ~18 ms pass),
   otherwise a keyword reading of the goal — ``docs`` · ``test`` · ``bugfix`` · ``refactor`` · ``feature`` ·
   ``general``, with ``/large`` for long multi-part goals.
2. **Hard gates first, never learned.** A candidate must serve the role, have its CLI, and sit in a pool that is not
   past its hard line; the harden role excludes every model family that executed the change. The bandit only ever
   *orders* what passed the gates — it cannot add a candidate a gate removed (the "nothing learned may lower a gate"
   invariant, held here by construction: gates filter, scores sort).
3. **Learned order.** ``Q̂`` is the candidate's win rate for this role and task class from the bandit table
   (``bandit.Bandit``, contexts ``orch|<role>|<class>|<point>``). A candidate with fewer than ``min_pulls`` outcomes
   borrows the learned value of its **tier** (``fast`` · ``standard`` · ``strong``) and **effort** — the two decision
   points the in-session harness reserved and never acted on; here they have a per-task seam, so they are acted on.
   Cost is the candidate's observed mean, normalised by the role's budget, with its tier's list price as the prior;
   scarcity is how far its pool is into its window.
4. **Shadow first.** ``mode="shadow"`` computes and logs the learned ranking (``bandit`` ``decide`` events) and returns
   the default — *strongest first* — so a policy is judged on the ledger (``replay``) before it ever acts.
   ``live`` returns the learned order, with ε-exploration on the top slot.

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

from .executor import Candidate
from .quota_ledger import QuotaLedger

logger = logging.getLogger(__name__)

TIERS = ("fast", "standard", "strong")
TIER_RANK = {"fast": 0, "standard": 1, "strong": 2}
#: Prior quality and list price by tier, before anything is learned. Price is relative (strong = 1).
TIER_PRIOR_Q = {"fast": 0.55, "standard": 0.7, "strong": 0.8}
TIER_PRICE = {"fast": 0.08, "standard": 0.3, "strong": 1.0}
POINT_FOR_ROLE = {"plan": "planner_pool", "execute": "executor_pool", "harden": "reviewer_pool", "audit": "reviewer_pool"}
POINTS = ("planner_pool", "executor_pool", "reviewer_pool", "tier", "effort")

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

    def __post_init__(self) -> None:
        if self.table is None:
            self.table = bandit_mod.for_project(None)

    # ------------------------------------------------------------------ gates and the default

    def eligible(self, role: str, exclude_model_families: set[str] | None = None) -> list[Spec]:
        exclude = exclude_model_families or set()
        out = []
        for spec in self.specs:
            if role not in spec.roles or spec.candidate.model_family in exclude or not self.available(spec):
                continue
            if self.ledger is not None and self.ledger.level(spec.candidate.family, spec.candidate.model) in ("hard", "exhausted"):
                continue
            out.append(spec)
        return out

    @staticmethod
    def default_order(specs: list[Spec]) -> list[Spec]:
        """Strongest first; among equals, the cheaper one."""
        return sorted(specs, key=lambda s: (-TIER_RANK.get(s.tier, 1), s.unit_price))

    # ------------------------------------------------------------------ scoring

    def _stat(self, context: str, arm: str) -> dict[str, Any]:
        assert self.table is not None
        return ((self.table.contexts.get(context) or {}).get("arms") or {}).get(arm) or {}

    def quality(self, spec: Spec, role: str, task_class: str) -> float:
        point = POINT_FOR_ROLE.get(role, "executor_pool")
        own = self._stat(ctx(role, task_class, point), spec.key)
        n = int(own.get("n", 0) or 0)
        if n >= self.min_pulls:
            return (float(own.get("wins", 0.0)) + 1.0) / (n + 2.0)
        # Too few outcomes of its own: borrow its tier's (and effort's) learned value, else the tier prior.
        borrowed = []
        for point_name, arm in (("tier", spec.tier), ("effort", spec.candidate.effort or "default")):
            stat = self._stat(ctx(role, task_class, point_name), arm)
            if int(stat.get("n", 0) or 0) >= self.min_pulls:
                borrowed.append((float(stat.get("wins", 0.0)) + 1.0) / (int(stat["n"]) + 2.0))
        prior = sum(borrowed) / len(borrowed) if borrowed else TIER_PRIOR_Q.get(spec.tier, 0.6)
        # Shrink toward the prior with whatever own evidence exists.
        return (float(own.get("wins", 0.0)) + prior * 2.0) / (n + 2.0)

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

    # ------------------------------------------------------------------ the policy

    def rank(self, role: str, task_class: str, *, exclude_model_families: set[str] | None = None) -> list[Candidate]:
        specs = self.eligible(role, exclude_model_families)
        default = self.default_order(specs)
        if self.mode not in ("shadow", "live") or not specs:
            return [s.candidate for s in default]
        scored = {s.key: round(self.score(s, role, task_class), 4) for s in specs}
        learned = sorted(specs, key=lambda s: (-scored[s.key], -TIER_RANK.get(s.tier, 1)))
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
        bandit_mod.decision_event(info, turn=f"orch:{role}:{task_class}")
        return [s.candidate for s in chosen]

    def observe(self, role: str, task_class: str, candidate: Candidate, outcome: dict[str, Any]) -> None:
        """Fold one outcome in: ``ok`` (bool), ``cost_usd`` / ``tokens`` (what it spent). Cost is normalised by the
        candidate's spec price when no dollar figure exists (subscription and local pools)."""
        spec = next((s for s in self.specs if s.key == candidate.key), None)
        if spec is None or self.table is None:
            return
        ok = bool(outcome.get("ok"))
        cost = float(outcome.get("norm_cost", spec.unit_price))
        value = (1.0 if ok else 0.0) - self.lam * cost
        point = POINT_FOR_ROLE.get(role, "executor_pool")
        for point_name, arm in ((point, spec.key), ("tier", spec.tier), ("effort", candidate.effort or "default")):
            self.table.observe_arm(ctx(role, task_class, point_name), arm, value=value, win=ok, cost=cost)


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

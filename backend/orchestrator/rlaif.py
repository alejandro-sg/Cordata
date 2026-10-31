"""Per-step reward attribution: who earned what, step by step, in [-1, 1] — instead of one run-wide bit for everyone.

The machine used to credit every worker the run's final verdict: a planner whose plan was fine, an executor whose step
was perfect and the executor whose step broke the build all got the same 0 when the audit failed. Here each executor
step is scored on what *it* did, and only then blended with the run's outcome where the two are connected.

**70 % deterministic telemetry** (``telemetry``), from the executor's own record of the step's attempts:

    +0.30  the diff stayed in scope                  +0.40  passed on the first attempt (and the run's Tier 1 held)
    +0.15  no recovery or handoff was needed         +0.15  × (1 − spent/budget): cheap against the step's budget
    −0.55  each: a scope escape, a timeout, a failed attempt another worker had to rescue (floored at −1)

**30 % judge rubric** (``judge``): the CONFORMANCE pass already reads the whole change against the plan; its answer may
carry ``step_scores`` (cohesion, minimality, plan_adherence in [0, 1]) — no extra model call. Without one, telemetry is
the whole reward. A person's verdict later (``outcomes.py``: kept or reverted) overrides both through ``reconcile``.

**Downstream blame is targeted.** A failed audit debits only the steps whose files the failing checks name (or, when no
check names a file, every step — the run's own verdict, scaled by γ < 1). A step nothing implicates keeps its reward.

The reward reaches ``LearnedDispatcher.observe`` as ``outcome["reward"]`` — ``bandit.py`` is untouched (its import
allowlist is checked by AST in ``_bandit_smoke_test.py``): ``win`` stays the binary "this step succeeded", the value is
continuous. Every reward is also an ``rlaif_reward`` event in ``state.db`` with its context, arm and propensity, so
``dispatch.replay`` style estimators can judge a policy offline (``REWARD_VERSION`` tags the definition).
"""

from __future__ import annotations

from collections.abc import Iterable
from dataclasses import asdict, dataclass, field
from typing import Any

REWARD_VERSION = 2
TELEMETRY_WEIGHT = 0.7
JUDGE_WEIGHT = 0.3
#: How much of a failed run's verdict reaches a step it implicates.
GAMMA = 0.6
PENALTY = 0.55
RUBRIC = ("cohesion", "minimality", "plan_adherence")


def clamp(value: float, low: float = -1.0, high: float = 1.0) -> float:
    return max(low, min(high, value))


@dataclass
class StepReward:
    step_id: str
    worker: str
    role: str = "execute"
    telemetry: float = 0.0
    judge: float | None = None
    downstream: float = 0.0
    value: float = 0.0
    win: bool = False
    parts: dict[str, float] = field(default_factory=dict)
    reward_version: int = REWARD_VERSION

    def to_json(self) -> dict[str, Any]:
        return asdict(self)


def telemetry(attempts: list[dict[str, Any]], *, worker: str, budget_usd: float | None = None, tier1_ok: bool = True) -> tuple[float, dict[str, float]]:
    """The deterministic part of one worker's reward for one step, from that step's attempt rows (executor results)."""
    mine = [a for a in attempts if a.get("worker") == worker]
    if not mine:
        return 0.0, {}
    parts: dict[str, float] = {}
    last = mine[-1]
    succeeded = last.get("status") == "completed" and not last.get("out_of_scope")
    first = attempts[0].get("worker") == worker and attempts[0].get("status") == "completed" and not attempts[0].get("out_of_scope")
    if succeeded:
        if not any(a.get("out_of_scope") for a in mine):
            parts["in_scope"] = 0.30
        if first and tier1_ok:
            parts["first_pass"] = 0.40
        if len(attempts) == 1:
            parts["no_recovery"] = 0.15
        spent = sum(float(a.get("cost_usd") or 0.0) for a in mine)
        if budget_usd and budget_usd > 0:
            parts["efficiency"] = round(0.15 * max(0.0, 1.0 - min(1.0, spent / budget_usd)), 4)
        else:
            parts["efficiency"] = 0.15 if spent == 0 else 0.075
    escapes = sum(1 for a in mine if a.get("out_of_scope"))
    timeouts = sum(1 for a in mine if a.get("status") == "timeout" or "timed out" in str(a.get("error_message") or "").lower())
    rescued = sum(1 for a in mine if a.get("status") != "completed") if any(a.get("worker") != worker and a.get("status") == "completed" for a in attempts) else 0
    failed_alone = sum(1 for a in mine if a.get("status") not in ("completed", "quota_limit")) if not succeeded and not rescued else 0
    if escapes:
        parts["scope_escape"] = -PENALTY * escapes
    if timeouts:
        parts["timeout"] = -PENALTY * timeouts
    if rescued:
        parts["rescued"] = -PENALTY * rescued
    if failed_alone:
        parts["failed"] = -PENALTY * failed_alone
    return clamp(sum(parts.values())), parts


def judge(scores: dict[str, Any] | None) -> float | None:
    """A rubric score in [-1, 1] from the conformance pass's ``{cohesion, minimality, plan_adherence}`` in [0, 1]."""
    if not isinstance(scores, dict):
        return None
    values = []
    for key in RUBRIC:
        try:
            values.append(max(0.0, min(1.0, float(scores[key]))))
        except (KeyError, TypeError, ValueError):
            continue
    return None if not values else 2.0 * (sum(values) / len(values)) - 1.0


def blend(telemetry_value: float, judge_value: float | None) -> float:
    if judge_value is None:
        return clamp(telemetry_value)
    return clamp(TELEMETRY_WEIGHT * telemetry_value + JUDGE_WEIGHT * judge_value)


def implicated(step_files: Iterable[str], failing: Iterable[str]) -> bool:
    """Did a failing check touch this step's files? (No named files at all: the whole run is implicated.)"""
    named = [f for f in failing if f]
    if not named:
        return True
    files = set(step_files)
    return any(f in files or any(f.endswith("/" + s) or s.endswith("/" + f) for s in files) for f in named)


def attribute(
    step_results: dict[str, list[dict[str, Any]]],
    *,
    run_ok: bool,
    tier1_ok: bool = True,
    failing_files: Iterable[str] = (),
    step_scores: Iterable[dict[str, Any]] | None = None,
    budgets: dict[str, float] | None = None,
) -> list[StepReward]:
    """One ``StepReward`` per (step, worker that attempted it)."""
    by_step = {str(s.get("step_id")): s for s in (step_scores or []) if isinstance(s, dict) and s.get("step_id")}
    failing = list(failing_files)
    out: list[StepReward] = []
    for step_id, attempts in step_results.items():
        files = sorted({f for a in attempts for f in (a.get("files_changed") or [])})
        for worker in dict.fromkeys(a.get("worker") for a in attempts if a.get("worker")):
            tele, parts = telemetry(attempts, worker=worker, budget_usd=(budgets or {}).get(step_id), tier1_ok=tier1_ok)
            mine = [a for a in attempts if a.get("worker") == worker]
            win = mine[-1].get("status") == "completed" and not mine[-1].get("out_of_scope")
            downstream = 0.0
            if win and not run_ok and implicated(files, failing):
                downstream = -GAMMA * tele if tele > 0 else -GAMMA * 0.5
                parts["downstream"] = round(downstream, 4)
            rubric = judge(by_step.get(step_id)) if win else None
            value = blend(clamp(tele + downstream), rubric)
            out.append(StepReward(step_id=step_id, worker=worker, telemetry=round(tele, 4), judge=None if rubric is None else round(rubric, 4), downstream=round(downstream, 4), value=round(value, 4), win=win and (run_ok or not implicated(files, failing)), parts=parts))
    return out


def reconcile(value: float, accepted: bool | None) -> float:
    """A person's verdict on the change beats both models and telemetry: kept pulls the reward up, reverted down."""
    if accepted is None:
        return value
    return clamp(0.5 * value + (0.5 if accepted else -0.5))


def role_reward(run_ok: bool, *, tier1_ok: bool = True) -> float:
    """The non-executor roles' reward (plan, harden, audit): still the run's verdict — their work is the whole change —
    but continuous, so a Tier-1 pass that failed only on the model's verdict is not the same as a broken build."""
    if run_ok:
        return 1.0
    return -0.25 if tier1_ok else -1.0


def record(reward: StepReward, *, run_id: str, task_class: str, context: str, propensity: float | None) -> None:
    """The offline-evaluation row: (context, arm, propensity, reward)."""
    try:
        import state_store

        state_store.append_event(
            "rlaif_reward",
            {"run_id": run_id, "task_class": task_class, "context": context, "arm": reward.worker, "propensity": propensity, **reward.to_json()},
            session_id=run_id,
            source="orchestrator",
        )
    except Exception:  # telemetry must never fail a run
        pass

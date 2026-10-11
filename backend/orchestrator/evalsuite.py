"""The orchestration eval suite: the whole machine, on the eval catalog's coding tasks, under a dispatch policy.

For each task the repository is materialised fresh, the machine runs end to end (plan → execute → harden → audit,
with its checkpoints, handoffs and bounded retries), and then the task's **own verifier** is run again from outside —
the machine's verdict is not taken on trust. Measured per policy:

    pass_rate          tasks whose verifier passes after the run
    cost_per_success   relative cost (roster prices, strong = 1) per passing task — the number dispatch optimises
    handoff_survival   of the runs that had to hand off (a provider limit injected), the share that still passed
    plan_conformance   of the passing runs, the share whose every changed file was inside the plan's scope
    retries            drift fix passes and replans spent

Drivers: ``sim`` (``sim.SimWorker`` per roster entry — no model, no network; the default) and ``live`` (the real
roster; spends real quota — only on request). Policies: ``strongest`` (the default order), ``learned`` (a
``LearnedDispatcher`` in live mode on a table trained beforehand), ``random`` (uniform exploration — the logging
policy that produces replayable data), ``cheapest`` (always the cheapest arm — the naive cost-minimiser the gate must
refuse wherever cheap is not as good).

``promote.evaluate_dispatch`` runs this twice — baseline and candidate — on dev and held-out tasks; that is the gate a
dispatch policy passes before ``orchestrator.dispatch`` may be set to ``live``.
"""

from __future__ import annotations

import os
import random
import tempfile
import time
import uuid
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any

import bandit as bandit_mod

from .dispatch import LearnedDispatcher, Spec, StaticDispatcher, evaluate
from .executor import Candidate
from .machine import OrchestratorMachine, RunConfig
from .quota_ledger import Pool, QuotaLedger
from .sim import SimWorker, coding_tasks, sim_roster, simulate_logs

POLICIES = ("strongest", "learned", "random", "cheapest")


@dataclass
class TaskOutcome:
    id: str
    category: str
    split: str
    passed: bool
    state: str
    cost: float
    handoffs: int
    conformant: bool
    drift_retries: int
    replans: int
    seconds: float
    executors: list[str] = field(default_factory=list)


@dataclass
class SuiteResult:
    policy: str
    driver: str
    outcomes: list[TaskOutcome]
    world: str = "steep"

    def aggregate(self, split: str = "all") -> dict[str, Any]:
        rows = [o for o in self.outcomes if split == "all" or o.split == split]
        passed = [o for o in rows if o.passed]
        handed = [o for o in rows if o.handoffs]
        cost = sum(o.cost for o in rows)
        return {
            "tasks": len(rows),
            "passed": len(passed),
            "pass_rate": round(len(passed) / len(rows), 4) if rows else None,
            "cost": round(cost, 4),
            "cost_per_success": round(cost / len(passed), 4) if passed else None,
            "handoff_runs": len(handed),
            "handoff_survival": round(sum(1 for o in handed if o.passed) / len(handed), 4) if handed else None,
            "plan_conformance": round(sum(1 for o in passed if o.conformant) / len(passed), 4) if passed else None,
            "drift_retries": sum(o.drift_retries for o in rows),
            "replans": sum(o.replans for o in rows),
        }

    def to_json(self) -> dict[str, Any]:
        return {"policy": self.policy, "driver": self.driver, "world": self.world, "aggregate": {s: self.aggregate(s) for s in ("dev", "heldout", "all")}, "outcomes": [asdict(o) for o in self.outcomes]}


def trained_table(specs: list[Spec], *, logs: int = 16000, seed: int = 11, lam: float = 0.3, world: str = "steep") -> bandit_mod.Bandit:
    """A bandit table trained on simulated exploration data (a *different* seed from any evaluation)."""
    table = bandit_mod.Bandit(None, path=os.path.join(tempfile.mkdtemp(prefix="dispatch-train-"), "table.json"))
    learner = LearnedDispatcher(specs=specs, mode="live", lam=lam, explore=0.0, table=table, log_outcomes=False)
    by_key = {s.key: s for s in specs}
    for role in ("execute", "plan"):
        for row in simulate_logs(logs // 2, specs, seed=seed + (role == "plan"), role=role, world=world):
            learner.observe(row.role, row.task_class, by_key[row.arm].candidate, {"ok": row.ok, "norm_cost": row.cost})
    return table


def _dispatcher(policy: str, specs: list[Spec], table: bandit_mod.Bandit | None, seed: int) -> Any:
    if policy == "cheapest":
        # The naive cost-minimiser: always the cheapest arm. What the gate exists to refuse where cheap is not as good.
        roles = {role: [s.candidate for s in sorted((s for s in specs if role in s.roles), key=lambda s: s.unit_price)] for role in ("plan", "execute", "harden", "audit")}
        return StaticDispatcher(roles)
    if policy == "strongest":
        roles: dict[str, list[Candidate]] = {}
        for role in ("plan", "execute", "harden", "audit"):
            roles[role] = [s.candidate for s in LearnedDispatcher.default_order([s for s in specs if role in s.roles])]
        return StaticDispatcher(roles)
    explore = 1.0 if policy == "random" else 0.0
    if table is None:
        table = bandit_mod.Bandit(None, path=os.path.join(tempfile.mkdtemp(prefix="dispatch-empty-"), "table.json")) if policy == "random" else trained_table(specs)
    return LearnedDispatcher(specs=specs, mode="live", explore=explore, mu=0.0, table=table, rng=random.Random(seed), log_outcomes=policy == "random")


def _cost(ledger: QuotaLedger, run_id: str) -> float:
    return round(sum(float(r["cost_usd"] or 0.0) for r in ledger.usage(run_id)) / 0.05, 4)


async def run_suite(
    *,
    policy: str = "strongest",
    split: str = "all",
    driver: str = "sim",
    seed: int = 1,
    inject_every: int = 3,
    table: bandit_mod.Bandit | None = None,
    limit: int | None = None,
    task_ids: list[str] | None = None,
    world: str = "steep",
) -> SuiteResult:
    """Run the suite. ``inject_every``: every n-th task's first executor family hits a provider limit after its first
    call (0 = never) — the handoff path, measured."""
    import sys

    if policy not in POLICIES:
        raise ValueError(f"policy must be one of {POLICIES}")
    from evals.runner import materialize, run_commands

    tasks = coding_tasks(split)
    if task_ids:
        tasks = [t for t in tasks if t.id in task_ids]
    if limit:
        tasks = tasks[:limit]
    python = os.path.realpath(sys.executable)
    root = Path(tempfile.mkdtemp(prefix=f"orch-eval-{policy}-"))
    specs = sim_roster() if driver == "sim" else None
    if driver == "live":
        from . import config

        specs = config.roster(config.DEFAULT_WORKERS)
    assert specs is not None
    if policy == "learned" and table is None:
        table = trained_table(specs, world=world)
    outcomes: list[TaskOutcome] = []
    for index, task in enumerate(tasks):
        repo = materialize(task, root / f"{index:02d}-{task.id}")
        tag = uuid.uuid4().hex[:8]
        ledger = QuotaLedger([Pool(f"{fam}-{tag}", fam, ".*", unit="tokens", window_limit=50_000_000) for fam in ("claude_code", "antigravity", "ollama")])
        inject = bool(inject_every) and index % inject_every == inject_every - 1
        if driver == "sim":
            goals = {task.prompt: task}
            run_specs: list[Spec] = []
            first_exec_family = LearnedDispatcher.default_order([s for s in specs if "execute" in s.roles])[0].candidate.family if policy == "strongest" else None
            for spec in specs:
                sim = SimWorker(spec, goals, python=python, seed=seed * 1000 + index, world=world)
                run_specs.append(Spec(Candidate(sim, spec.candidate.model, spec.candidate.effort), roles=spec.roles, tier=spec.tier, price=spec.price))
            disp = _dispatcher(policy, run_specs, table, seed * 1000 + index)
            if inject:
                # The family the policy would send step 1 to runs out after one execute call.
                first = (disp.rank("execute", _class(task)) or [None])[0] if first_exec_family is None else None
                family = first.family if first is not None else first_exec_family
                for spec in run_specs:
                    if spec.candidate.family == family:
                        spec.candidate.worker.limit_after_calls = 1  # type: ignore[attr-defined]
        else:
            disp = _dispatcher(policy, specs, table, seed * 1000 + index)
        started = time.monotonic()
        machine = OrchestratorMachine(goal=task.prompt, project=str(repo), dispatcher=disp, ledger=ledger, config=RunConfig(max_usd=50.0, tier2=True, harden=True))
        record = await machine.run()
        ok, _ = run_commands(task.verify, repo, python=python)
        audit = record.audit or {}
        conformant = bool(audit) and not audit.get("out_of_scope")
        outcomes.append(TaskOutcome(
            id=task.id, category=task.category, split=task.split, passed=ok, state=record.state, cost=_cost(ledger, record.run_id),
            handoffs=len(record.handoffs), conformant=conformant, drift_retries=record.drift_retries, replans=record.replans,
            seconds=round(time.monotonic() - started, 2), executors=list(dict.fromkeys(record.workers.get("execute", []))),
        ))
    return SuiteResult(policy=policy, driver=driver, outcomes=outcomes, world=world)


def _class(task: Any) -> str:
    from .dispatch import classify

    return classify(task.prompt)


def replay_report(*, logs: int = 6000, seed: int = 23, world: str = "steep") -> dict[str, Any]:
    """The phase-4 judgement on simulated exploration data: learned (trained on one half) vs always-strongest, by
    replay on the other half."""
    specs = sim_roster()
    rows = simulate_logs(logs, specs, seed=seed, world=world)
    return evaluate(rows, specs, train_fraction=0.5)

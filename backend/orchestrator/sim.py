"""Simulated providers: what the dispatch policy and the orchestration eval suite are judged against offline.

A ``SimWorker`` stands in for one roster entry (worker + model) with a **quality** that depends on its tier and the
task's difficulty, and a **price** per call. It works on the eval suite's real tasks (``evals/catalog.py``): as a
planner it returns the plan the task's own oracle implies (one step per file the oracle changes, the task's
verifiers as acceptance criteria), and as an executor it writes the oracle's content for its step with probability
``quality`` — otherwise a plausible wrong edit — so the machine's real audit (the task's own tests, run in the
sandbox) decides what passed. A provider limit can be injected after a number of calls, which is how handoff survival
is measured.

The quality table is an assumption, written down where it can be argued with: easy work (config, tests) sits at
the ceiling for every tier; medium work (bug fixes, features, refactors) separates the fast tier; hard work
(multi-file) separates all three. The claim the eval checks is conditional on it — *if* cheap models are as good on
easy work, the learned policy finds that out and stops paying for the strong one there — which is exactly the
behaviour to verify before letting real outcomes drive it (``orchestrator.dispatch: shadow`` collects them).

``simulate_logs`` draws decision-level outcomes from the same table under a uniform logging policy: the data the
replay estimator (``dispatch.replay``) needs, without running a machine per row.
"""

from __future__ import annotations

import hashlib
import os
import random
from collections.abc import AsyncIterator
from dataclasses import dataclass
from typing import Any

from workers import TaskPacket, Worker, WorkerCapabilities, WorkerEvent, WorkerFamily, WorkerResult
from workers import scope as scope_mod
from workers import tree

from .dispatch import Logged, Spec, classify

CODING = ("bugfix", "feature", "refactor", "tests", "config", "multifile")
DIFFICULTY = {"config": "easy", "tests": "easy", "bugfix": "medium", "feature": "medium", "refactor": "medium", "multifile": "hard"}
#: Two worlds, because what the policy *should* do depends on which one is true. ``ceiling``: on easy and medium work a
#: cheaper tier is as good as the strong one (only hard work separates them) — the learned policy should move that
#: work to the cheap arms at no loss. ``steep``: every step down in tier costs quality on anything but easy work — it
#: should keep paying for the strong tier there, and the gate should refuse it if it does not.
WORLDS = {
    "ceiling": {
        "fast": {"easy": 0.97, "medium": 0.95, "hard": 0.60},
        "standard": {"easy": 0.97, "medium": 0.95, "hard": 0.88},
        "strong": {"easy": 0.97, "medium": 0.95, "hard": 0.90},
    },
    "steep": {
        "fast": {"easy": 0.97, "medium": 0.78, "hard": 0.45},
        "standard": {"easy": 0.97, "medium": 0.93, "hard": 0.80},
        "strong": {"easy": 0.97, "medium": 0.94, "hard": 0.90},
    },
}
QUALITY = WORLDS["steep"]
TOKENS = {"fast": (3000, 600), "standard": (6000, 1200), "strong": (9000, 2000)}


def quality(tier: str, category: str, world: str = "steep") -> float:
    table = WORLDS.get(world, QUALITY)
    return table.get(tier, table["standard"])[DIFFICULTY.get(category, "medium")]


def plan_from_task(task: Any, python: str) -> dict[str, Any]:
    """The plan a competent planner would write for an eval task: one step per file the oracle changes."""
    from evals.task import render_command

    files = sorted((task.fixed or {}).keys())
    criteria = [
        {"id": f"verify-{i}", "type": "test", "description": "the task's verifier passes", "target": render_command(cmd, python), "timeout_s": 120}
        for i, cmd in enumerate(task.verify, 1)
    ]
    for i, path in enumerate(sorted(set((task.expect or {}).get("unchanged", ())) - set(files)), 1):
        criteria.append({"id": f"unchanged-{i}", "type": "file_not_changed", "description": f"{path} is not touched", "target": path})
    return {
        "goal": task.prompt,
        "acceptance_criteria": criteria,
        "steps": [{"step_id": f"s{i}", "name": f"edit {path}", "description": f"Make the change the goal needs in {path}.", "file_scope": [path]} for i, path in enumerate(files, 1)],
        "verifier_commands": [],
        "assumptions": [],
    }


def _wrong(path: str, original: str | None) -> str:
    """A plausible wrong edit: the file as it was, with a comment claiming it was done."""
    marker = "# done (sim)\n" if path.endswith(".py") else "\n"
    return (original or "") + ("\n" if original and not original.endswith("\n") else "") + marker


@dataclass
class SimProfile:
    tier: str
    price: float
    model_family: str


class SimWorker(Worker):
    def __init__(self, spec: Spec, tasks: dict[str, Any], *, python: str, seed: int = 0, limit_after_calls: int | None = None, world: str = "steep") -> None:
        self.spec = spec
        self.world = world
        # The roster's own id, so a simulated arm and the real one share their learned record.
        self.worker_id = spec.candidate.worker.worker_id
        self.tasks = tasks  # goal text → eval Task
        self.python = python
        self.seed = seed
        self.limit_after_calls = limit_after_calls
        self.calls = 0
        family = spec.candidate.worker.capabilities.family
        self._caps = WorkerCapabilities(family=family, models=(spec.candidate.model or "default",), model_family=spec.candidate.model_family, cost_basis="subscription")

    @property
    def capabilities(self) -> WorkerCapabilities:
        return self._caps

    async def quota_snapshot(self) -> dict[str, Any]:
        return {"worker_id": self.worker_id, "calls": self.calls}

    def resume(self, handle: str, packet: TaskPacket) -> AsyncIterator[WorkerEvent]:
        return self.run(packet)

    async def cancel(self, handle: str) -> bool:
        return False

    def _task_for(self, prompt: str) -> Any:
        for goal, task in self.tasks.items():
            if goal in prompt:
                return task
        return None

    def _draw(self, key: str) -> float:
        digest = hashlib.sha256(f"{self.seed}|{self.spec.key}|{key}".encode()).digest()
        return random.Random(digest).random()

    async def run(self, task: TaskPacket) -> AsyncIterator[WorkerEvent]:
        self.calls += 1
        root = os.path.realpath(task.cwd)
        before = tree.snapshot(root)
        yield WorkerEvent("init", {"worker_id": self.worker_id, "role": task.role, "model": self.spec.candidate.model})
        tin, tout = TOKENS.get(self.spec.tier, (5000, 1000))
        status, error, structured, rate_limited = "completed", None, None, False
        eval_task = self._task_for(task.prompt)
        if self.limit_after_calls is not None and self.calls > self.limit_after_calls and task.role in ("execute", "harden"):
            status, error, rate_limited = "quota_limit", "429 usage limit reached (simulated)", True
        elif eval_task is None:
            status, error = "failed", "the simulator does not know this task"
        elif task.role == "plan":
            structured = plan_from_task(eval_task, self.python)
        elif task.role == "audit":
            structured = {"verdict": "accept", "items": []}
        elif task.role == "execute":
            scope = task.writable_scope()
            files = [p for p in sorted((eval_task.fixed or {}).keys()) if scope_mod.matches(p, scope, root)]
            q = quality(self.spec.tier, eval_task.category, self.world)
            per_file = q ** (1.0 / max(1, len(eval_task.fixed or {}))) if not task.task_id.split(":")[1].startswith("fix-") else q
            step_key = ":".join(task.task_id.split(":")[1:])  # the step and attempt, not the run's random id
            for path in files:
                right = self._draw(f"{eval_task.id}|{step_key}|{path}") < per_file
                content = eval_task.fixed[path] if right else _wrong(path, eval_task.files.get(path))
                full = os.path.join(root, path)
                os.makedirs(os.path.dirname(full) or root, exist_ok=True)
                with open(full, "w", encoding="utf-8") as handle:
                    handle.write(content)
        if task.budget_tokens is not None and tin + tout > task.budget_tokens and status == "completed":
            status, error = "quota_limit", "over the task's token budget"
        changed = tree.changed(before, tree.snapshot(root)) or []
        result = WorkerResult(
            worker_id=self.worker_id, status=status, exit_code=0 if status == "completed" else 1, duration_s=0.0,
            cost_usd=round(self.spec.unit_price * 0.05, 6), tokens_in=tin, tokens_out=tout, files_changed=changed,
            diff_stat=tree.diff_stat(root, changed), raw_output="sim", error_message=error, session_handle=f"{self.worker_id}-{task.task_id}",
            model=self.spec.candidate.model, structured=structured, rate_limited=rate_limited,
            out_of_scope=scope_mod.out_of_scope(changed, task.writable_scope(), root),
        )
        if status != "completed":
            yield WorkerEvent("error", {"message": error})
        yield WorkerEvent("done", result.to_json(), result=result)


def coding_tasks(split: str = "all") -> list[Any]:
    from evals.catalog import CATALOG

    return [t for t in CATALOG if t.category in CODING and t.fixed and t.verify and (split == "all" or t.split == split)]


def simulate_logs(n: int, specs: list[Spec], *, seed: int = 7, role: str = "execute", tasks: list[Any] | None = None, world: str = "steep") -> list[Logged]:
    """``n`` decisions under a uniform logging policy over the specs that serve ``role``, with outcomes drawn from the
    quality table on real eval tasks' categories. The task class is what the machine would see: ``classify(goal)``."""
    rng = random.Random(seed)
    pool = tasks or coding_tasks()
    arms = [s for s in specs if role in s.roles]
    out = []
    for _ in range(n):
        task = rng.choice(pool)
        spec = rng.choice(arms)
        ok = rng.random() < quality(spec.tier, task.category, world)
        out.append(Logged(role, classify(task.prompt), spec.key, ok, spec.unit_price))
    return out


def expected_value(choose: Any, specs: list[Spec], *, world: str = "steep", role: str = "execute", tasks: list[Any] | None = None) -> dict[str, float]:
    """A policy's *exact* value under the simulation model: the mean, over the eval tasks, of the chosen arm's true
    quality and price. Replay estimates this from logs (with noise); this is what it estimates."""
    pool = tasks or coding_tasks()
    by_key = {s.key: s for s in specs}
    arms = [s.key for s in specs if role in s.roles]
    q = cost = 0.0
    for task in pool:
        spec = by_key[choose(role, classify(task.prompt), arms)]
        q += quality(spec.tier, task.category, world)
        cost += spec.unit_price
    n = max(1, len(pool))
    return {"pass_rate": round(q / n, 4), "cost_per_task": round(cost / n, 4), "cost_per_success": round(cost / q, 4) if q else float("inf")}


def sim_roster() -> list[Spec]:
    """The real roster's shape (families, models, tiers, prices) on placeholder workers — for simulations that must not
    construct, let alone run, a real CLI adapter."""
    from .config import ROSTER
    from .executor import Candidate

    class _Placeholder(Worker):
        def __init__(self, family: str, model_family: str) -> None:
            self.worker_id = family
            self._caps = WorkerCapabilities(family=WorkerFamily(family), models=(), model_family=model_family)

        @property
        def capabilities(self) -> WorkerCapabilities:
            return self._caps

        async def quota_snapshot(self) -> dict[str, Any]:
            return {}

        def run(self, task):  # pragma: no cover - never run
            raise RuntimeError("placeholder")

        def resume(self, handle, packet):  # pragma: no cover
            raise RuntimeError("placeholder")

        async def cancel(self, handle: str) -> bool:
            return False

    families = {"claude_code": "anthropic", "antigravity": "google", "ollama": "zhipu"}
    workers = {f: _Placeholder(f, mf) for f, mf in families.items()}
    return [Spec(Candidate(workers[f], model), roles=roles or frozenset({"plan", "execute", "harden", "audit"}), tier=tier, price=price) for f, model, tier, price, roles in ROSTER]

"""In-process fake workers for the orchestrator's tests and its eval suite.

``FakeWorker`` meets the ``workers.Worker`` contract with no process and no model: it answers ``plan`` and ``audit``
with the structured answers it was given, and for ``execute``/``harden`` writes the files its script says for that
step — inside the task's writable scope only, as the real adapters' sandbox would (unless it is told to ``leak``,
which is how a test shows the audit catching a scope escape the kernel missed). Knobs make it a *simulated provider*
for the eval suite: a cost and token price per step, a quality (the chance a step is done right, seeded so a run is
reproducible), and a rate limit that fires after N tokens.

The step a task belongs to is read from its id (``<run>:<step_id>:<attempt>``), the convention the executor uses.
"""

from __future__ import annotations

import asyncio
import os
import random
from collections.abc import AsyncIterator, Callable
from dataclasses import dataclass, field
from typing import Any

from workers import TaskPacket, Worker, WorkerCapabilities, WorkerEvent, WorkerFamily, WorkerResult
from workers import scope as scope_mod
from workers import tree


@dataclass
class StepScript:
    """What the fake does for one step: ``writes`` when it gets it right, ``wrong`` when it does not."""

    writes: dict[str, str] = field(default_factory=dict)
    wrong: dict[str, str] | None = None
    tokens: tuple[int, int] = (1000, 300)
    cost_usd: float = 0.0
    fail: bool = False
    #: Fail this many attempts at the step, then succeed (a worker that runs out of room, then a stronger one that does not).
    fail_attempts: int = 0


class FakeWorker(Worker):
    def __init__(
        self,
        worker_id: str,
        family: WorkerFamily | str = WorkerFamily.OLLAMA,
        *,
        model: str = "fake-model",
        model_family: str = "fake",
        plan: dict[str, Any] | Callable[[TaskPacket], dict[str, Any]] | None = None,
        audit: dict[str, Any] | Callable[[TaskPacket], dict[str, Any]] | None = None,
        steps: dict[str, StepScript] | None = None,
        harden: StepScript | None = None,
        quality: float = 1.0,
        seed: int = 0,
        rate_limit_after_tokens: int | None = None,
        rate_limit_on_steps: set[str] | None = None,
        partial_on_limit: dict[str, str] | None = None,
        leak: dict[str, str] | None = None,
        tokens_per_plan: tuple[int, int] = (2000, 500),
        cost_per_plan: float = 0.0,
        delay: float = 0.0,
    ) -> None:
        self.worker_id = worker_id
        fam = WorkerFamily(family) if not isinstance(family, WorkerFamily) else family
        self._caps = WorkerCapabilities(family=fam, models=(model,), model_family=model_family, cost_basis="usd" if cost_per_plan else "subscription")
        self.model = model
        self.plan_answer = plan
        self.audit_answer = audit if audit is not None else {"verdict": "accept", "items": []}
        self.steps = steps or {}
        self.harden_script = harden
        self.quality = quality
        self.rng = random.Random(seed)
        self.rate_limit_after_tokens = rate_limit_after_tokens
        self.rate_limit_on_steps = set(rate_limit_on_steps or ())
        self.partial_on_limit = partial_on_limit or {}
        self.leak = leak or {}
        self.tokens_per_plan = tokens_per_plan
        self.cost_per_plan = cost_per_plan
        self.delay = delay
        self.spent_tokens = 0
        self.attempts: dict[str, int] = {}
        self.calls: list[TaskPacket] = []
        self._cancelled: set[str] = set()

    @property
    def capabilities(self) -> WorkerCapabilities:
        return self._caps

    async def quota_snapshot(self) -> dict[str, Any]:
        return {"worker_id": self.worker_id, "family": self._caps.family.value, "available": True, "spent_tokens": self.spent_tokens}

    def resume(self, handle: str, packet: TaskPacket) -> AsyncIterator[WorkerEvent]:
        return self.run(packet)

    async def cancel(self, handle: str) -> bool:
        self._cancelled.add(handle)
        return True

    @staticmethod
    def step_of(task: TaskPacket) -> str:
        parts = task.task_id.split(":")
        return parts[1] if len(parts) >= 3 else ""

    def _write(self, root: str, files: dict[str, str], scope: list[str], *, leak: bool = False) -> list[str]:
        refused = []
        for rel, content in files.items():
            if not leak and not scope_mod.matches(rel, scope, root):
                refused.append(rel)  # what Seatbelt would have refused
                continue
            path = os.path.join(root, rel)
            os.makedirs(os.path.dirname(path) or root, exist_ok=True)
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(content)
        return refused

    async def run(self, task: TaskPacket) -> AsyncIterator[WorkerEvent]:
        self.calls.append(task)
        root = os.path.realpath(task.cwd)
        before = tree.snapshot(root)
        yield WorkerEvent("init", {"worker_id": self.worker_id, "role": task.role, "model": task.model or self.model})
        if self.delay:
            await asyncio.sleep(self.delay)
        status, error, structured, text = "completed", None, None, ""
        tin, tout, cost = 0, 0, 0.0
        rate_limited = False
        scope = task.writable_scope()
        if task.task_id in self._cancelled:
            status, error = "cancelled", "cancelled"
        elif task.role in ("plan", "audit"):
            answer = self.plan_answer if task.role == "plan" else self.audit_answer
            structured = answer(task) if callable(answer) else answer
            tin, tout = self.tokens_per_plan
            cost = self.cost_per_plan
            text = "planned" if task.role == "plan" else "audited"
            if structured is None:
                status, error = "failed", "the answer does not match the schema: no JSON object in the answer"
        else:
            step = self.step_of(task)
            script = self.harden_script if task.role == "harden" else self.steps.get(step)
            if script is None:
                script = StepScript()
            tin, tout = script.tokens
            cost = script.cost_usd
            limited = step in self.rate_limit_on_steps or (self.rate_limit_after_tokens is not None and self.spent_tokens + tin + tout > self.rate_limit_after_tokens)
            if limited:
                # The provider stops it part-way: maybe some files are written, then the limit.
                self._write(root, self.partial_on_limit, scope)
                status, error, rate_limited = "quota_limit", "429 rate limit reached (simulated)", True
                self.rate_limit_on_steps.discard(step)
            elif script.fail or self.attempts.get(step, 0) < script.fail_attempts:
                self.attempts[step] = self.attempts.get(step, 0) + 1
                status, error = "failed", f"simulated failure on {step or task.role}"
            else:
                right = self.rng.random() < self.quality
                files = script.writes if right or script.wrong is None else script.wrong
                refused = self._write(root, files, scope)
                if self.leak and task.role == "execute":
                    self._write(root, self.leak, scope, leak=True)
                text = f"did {step or task.role}" + (f"; refused {refused}" if refused else "")
            if task.budget_tokens is not None and tin + tout > task.budget_tokens and status == "completed":
                status, error = "quota_limit", f"used {tin + tout} tokens, over the task budget of {task.budget_tokens}"
            if task.budget_usd is not None and cost > task.budget_usd and status == "completed":
                status, error = "quota_limit", f"spent ${cost}, over the task budget"
        self.spent_tokens += tin + tout
        changed = tree.changed(before, tree.snapshot(root)) or []
        result = WorkerResult(
            worker_id=self.worker_id, status=status, exit_code=0 if status == "completed" else 1, duration_s=0.0, cost_usd=cost,
            tokens_in=tin, tokens_out=tout, files_changed=changed, diff_stat=tree.diff_stat(root, changed), raw_output=text,
            error_message=error, session_handle=f"{self.worker_id}-{task.task_id}", model=task.model or self.model, structured=structured,
            rate_limited=rate_limited, out_of_scope=scope_mod.out_of_scope(changed, scope, root),
        )
        if status != "completed":
            yield WorkerEvent("error", {"message": error})
        yield WorkerEvent("done", result.to_json(), result=result)

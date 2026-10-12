"""The EXECUTE phase: run a plan's steps in order, each inside a checkpoint, handing off at safe points.

For each pending step:

1. **Checkpoint.** ``checkpoints.take`` snapshots the project into the shadow repository; the hash goes on the step.
   A step whose snapshot failed is marked *unprotected* and its rollback policy degrades to ``keep_partial`` — the
   executor never claims a rollback it cannot do.
2. **Pick the worker.** The first candidate (ranked by the dispatcher) whose pool is ``ok``; a ``soft`` pool only when
   no ``ok`` one is left; never a ``hard`` or ``exhausted`` one. The plan's envelope is checked first: a run past its
   budget stops (``budget_exhausted``) rather than spending it somewhere else.
3. **Bound the step.** The task's budget is the smaller of what the envelope has left and what the pool has before
   its hard line (``QuotaLedger.step_allowance``) — so "stop at 95 %" is enforced by the worker cutting itself off,
   mid-step, exactly there.
4. **Read the result.**
   * ``completed`` with nothing out of scope → the step is done; usage is recorded.
   * ``quota_limit`` (hard line or provider limit) → the step's ``rollback_policy`` applies — restore the checkpoint,
     or keep the partial diff — a ``HandoffPacket`` is built and saved, and the *same step* resumes on the next
     candidate in another pool, carrying the packet.
   * ``failed`` or out-of-scope writes → roll back, record the attempt, try the next candidate; a step that fails
     ``max_attempts`` times fails the phase.
5. **Soft line at the boundary.** Between steps, a worker whose pool crossed 80 % finishes nothing more: the next
   step goes to an ``ok`` pool with a packet (``quota_soft_limit_reached``) so the new worker knows what was done.

No candidate left → ``paused``, with the packet saved: the run resumes from it when a window rolls over.
"""

from __future__ import annotations

import logging
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Any

import checkpoints

from workers import TaskPacket, Worker, WorkerResult, collect
from workers import scope as scope_mod

from . import handoff
from .plan_artifact import Plan, Step
from .quota_ledger import QuotaLedger

logger = logging.getLogger(__name__)

MAX_ATTEMPTS = 3


@dataclass
class Candidate:
    worker: Worker
    model: str | None = None
    effort: str | None = None

    @property
    def family(self) -> str:
        return self.worker.capabilities.family.value

    @property
    def key(self) -> str:
        return f"{self.worker.worker_id}:{self.model or 'default'}"

    @property
    def model_family(self) -> str:
        return model_family_of(self)


def model_family_of(candidate: "Candidate") -> str:
    """The vendor behind the model a candidate will run — what "a different family" compares for the harden role."""
    name = (candidate.model or "").lower()
    family = candidate.family
    if family == "antigravity":
        from workers.agy_worker import model_family

        return model_family(candidate.model)
    if family == "claude_code":
        return "anthropic"
    if family == "ollama" and name:
        from workers.ollama_worker import model_family

        return model_family(candidate.model)
    return candidate.worker.capabilities.model_family or family


@dataclass
class ExecResult:
    status: str  # "completed" | "failed" | "paused" | "budget_exhausted" | "cancelled"
    reason: str = ""
    handoffs: list[handoff.HandoffPacket] = field(default_factory=list)
    results: dict[str, list[dict[str, Any]]] = field(default_factory=dict)
    last_packet: handoff.HandoffPacket | None = None
    workers_used: list[str] = field(default_factory=list)


def step_prompt(plan: Plan, index: int, extra: str = "") -> str:
    step = plan.steps[index]
    done = [f"- {s.step_id}: {s.name}" for s in plan.steps[:index] if s.status in ("completed", "partial")]
    later = [f"- {s.step_id}: {s.name}" for s in plan.steps[index + 1 :]]
    criteria = [f"- {c.id} ({c.type}{(': ' + c.target) if c.target else ''}): {c.description}" for c in plan.acceptance_criteria]
    parts = [
        f"Goal of the whole plan: {plan.goal}",
        f"You are doing step {index + 1} of {len(plan.steps)}: {step.name} (id {step.step_id}).\n{step.description}",
        "Already done:\n" + "\n".join(done) if done else "",
        "Later steps — do NOT do these now:\n" + "\n".join(later) if later else "",
        "The plan will be accepted when:\n" + "\n".join(criteria),
        extra,
    ]
    return "\n\n".join(p for p in parts if p)


class StepExecutor:
    def __init__(
        self,
        *,
        run_id: str,
        project: str,
        plan: Plan,
        candidates: list[Candidate],
        ledger: QuotaLedger,
        on_event: Callable[[dict[str, Any]], None] | None = None,
        timeout_s: float = 1800.0,
        extra_prompt: str = "",
        start_packet: handoff.HandoffPacket | None = None,
        max_attempts: int = MAX_ATTEMPTS,
        strongest_first: bool = False,
    ) -> None:
        self.run_id = run_id
        self.project = project
        self.plan = plan
        self.candidates = list(candidates)
        self.ledger = ledger
        self.on_event = on_event or (lambda event: None)
        self.timeout_s = timeout_s
        self.extra_prompt = extra_prompt
        self.packet = start_packet
        self.max_attempts = max_attempts
        #: A recovery pass: take candidates in the dispatcher's order whatever their pool's level (``ok`` or ``soft``),
        #: instead of preferring any ``ok`` pool. The ``ok`` pools are often the weakest workers, and a step that has
        #: already defeated them needs the best one still allowed to run, not the one with the most quota left.
        self.strongest_first = strongest_first
        self.current: Candidate | None = None

    # ------------------------------------------------------------------ choosing

    def _level(self, candidate: Candidate) -> str:
        return self.ledger.level(candidate.family, candidate.model)

    def pick(self, exclude_pools: set[str] | None = None) -> Candidate | None:
        exclude_pools = exclude_pools or set()
        usable = [c for c in self.candidates if self.ledger.pool_for(c.family, c.model).pool_id not in exclude_pools]
        if self.strongest_first:
            return next((c for c in usable if self._level(c) in ("ok", "soft")), None)
        for wanted in ("ok", "soft"):
            for candidate in usable:
                if self._level(candidate) == wanted:
                    return candidate
        return None

    def _emit(self, kind: str, **payload: Any) -> None:
        try:
            self.on_event({"kind": kind, "run_id": self.run_id, **payload})
        except Exception:
            logger.debug("executor: event listener failed", exc_info=True)

    # ------------------------------------------------------------------ the loop

    async def run(self) -> ExecResult:
        out = ExecResult(status="completed")
        for index, step in enumerate(self.plan.steps):
            if step.status in ("completed", "skipped"):
                continue
            verdict = await self._run_step(index, step, out)
            if verdict != "completed":
                out.status = verdict
                return out
        return out

    def _envelope(self):
        budget = self.plan.budget_envelope or {}
        return self.ledger.envelope(self.run_id, max_usd=budget.get("max_usd"), max_tokens=budget.get("max_tokens"))

    async def _run_step(self, index: int, step: Step, out: ExecResult) -> str:
        snap = checkpoints.take(self.project, kind="pre", tool="orchestrator", agent="executor", session_id=self.run_id, turn=step.step_id, call=f"step {index + 1}")
        step.checkpoint_hash = snap.id if snap.ok else None
        if not snap.ok:
            logger.warning("executor: step %s is unprotected (%s); partial work will be kept", step.step_id, snap.reason)
            step.rollback_policy = "keep_partial"
        self._emit("step_start", step_id=step.step_id, index=index, checkpoint=step.checkpoint_hash)
        failures: list[str] = []
        excluded: set[str] = set()
        while True:
            envelope = self._envelope()
            if envelope.exhausted:
                step.status = "failed" if step.status == "running" else step.status
                out.reason = f"the plan's budget envelope is spent (${envelope.spent_usd:.4f}, {envelope.spent_tokens} tokens)"
                self._emit("budget_exhausted", step_id=step.step_id, envelope=envelope.to_json())
                return "budget_exhausted"
            candidate = self.pick(excluded)
            if candidate is None:
                packet = self._packet(index, step, "worker_unavailable" if not out.handoffs else out.handoffs[-1].reason, self.current, None, failures, keep=False)
                out.last_packet = packet
                out.reason = "no worker has quota left; the run is paused at a safe point"
                self._emit("paused", step_id=step.step_id, handoff_id=packet.handoff_id)
                return "paused"
            if self.current is not None and candidate is not self.current and self.packet is None and out.workers_used:
                # A switch at a step boundary (soft line, or the previous worker's pool went away): leave a note.
                reason = "quota_soft_limit_reached" if self._level(self.current) == "soft" else "worker_unavailable"
                self.packet = self._packet(index, step, reason, self.current, candidate, failures, keep=False)
                out.handoffs.append(self.packet)
            self.current = candidate
            if candidate.key not in out.workers_used:
                out.workers_used.append(candidate.key)
            allowance = self.ledger.step_allowance(candidate.family, candidate.model)
            budget_usd = _min(allowance.get("budget_usd"), envelope.remaining_usd)
            budget_tokens = _min(allowance.get("budget_tokens"), envelope.remaining_tokens)
            step.status, step.worker = "running", candidate.key
            step.attempts += 1
            task = TaskPacket(
                task_id=f"{self.run_id}:{step.step_id}:{step.attempts}",
                role="execute",
                prompt=step_prompt(self.plan, index, self.extra_prompt + (("\n\nEarlier attempts at this step failed:\n- " + "\n- ".join(failures)) if failures else "")),
                cwd=self.project,
                file_scope=list(step.file_scope),
                budget_usd=budget_usd,
                budget_tokens=int(budget_tokens) if budget_tokens is not None else None,
                timeout_s=self.timeout_s,
                model=candidate.model,
                effort=candidate.effort,
                checkpoint_hash=step.checkpoint_hash,
                handoff_context=self.packet.context() if self.packet else None,
            )
            self._emit("step_dispatch", step_id=step.step_id, worker=candidate.key, budget_usd=budget_usd, budget_tokens=task.budget_tokens, handoff=self.packet.handoff_id if self.packet else None)
            _, result = await collect(candidate.worker.run(task))
            self.ledger.record_result(session_id=self.run_id, step_id=step.step_id, family=candidate.family, model=candidate.model or result.model, result=result)
            out.results.setdefault(step.step_id, []).append({"worker": candidate.key, **{k: getattr(result, k) for k in ("status", "error_message", "tokens_in", "tokens_out", "cost_usd", "files_changed", "out_of_scope", "rate_limited")}})
            self._emit("step_result", step_id=step.step_id, worker=candidate.key, status=result.status, error=result.error_message, files=result.files_changed[:50])

            if result.status == "completed" and not result.out_of_scope:
                step.status = "completed"
                self.packet = None
                return "completed"
            if result.status == "cancelled":
                self._rollback(step)
                step.status = "failed"
                out.reason = "cancelled"
                return "cancelled"
            if result.status == "quota_limit":
                reason = "provider_rate_limit" if result.rate_limited else "quota_hard_limit_reached"
                keep = step.rollback_policy == "keep_partial"
                if not keep:
                    self._rollback(step)
                envelope = self._envelope()
                if envelope.exhausted and not result.rate_limited:
                    # The cut-off was the plan's own envelope, not the pool: no other worker may spend more on it.
                    step.status = "partial" if keep else "pending"
                    out.reason = f"the plan's budget envelope is spent (${envelope.spent_usd:.4f}, {envelope.spent_tokens} tokens)"
                    self._emit("budget_exhausted", step_id=step.step_id, envelope=envelope.to_json())
                    return "budget_exhausted"
                failures.append(f"{candidate.key} stopped: {result.error_message or reason}")
                excluded.add(self.ledger.pool_for(candidate.family, candidate.model).pool_id)
                nxt = self.pick(excluded)
                self.packet = self._packet(index, step, reason, candidate, nxt, failures, keep=keep, result=result)
                out.handoffs.append(self.packet)
                self._emit("handoff", step_id=step.step_id, handoff_id=self.packet.handoff_id, reason=reason, from_worker=candidate.key, to_worker=nxt.key if nxt else None)
                step.status = "partial" if keep else "pending"
                continue
            # failed, or completed with writes outside the step's scope
            why = result.error_message or "failed"
            if result.out_of_scope:
                why = f"wrote outside the step's scope: {', '.join(result.out_of_scope[:10])}"
            failures.append(f"{candidate.key}: {why}")
            self._rollback(step)
            if len(failures) >= self.max_attempts:
                step.status = "failed"
                out.reason = f"step {step.step_id} failed {len(failures)} times: {failures[-1]}"
                return "failed"
            # try another candidate: prefer a different worker for the retry
            others = [c for c in self.candidates if c is not candidate]
            if others:
                self.candidates = others + [candidate]
            self.packet = self._packet(index, step, "worker_failed", candidate, None, failures, keep=False, result=result)
            out.handoffs.append(self.packet)

    # ------------------------------------------------------------------ helpers

    def _rollback(self, step: Step) -> None:
        if not step.checkpoint_hash:
            return
        report = checkpoints.restore(self.project, step.checkpoint_hash, session_id=self.run_id)
        if not report.get("ok"):
            logger.warning("executor: rollback of %s failed: %s", step.step_id, report.get("reason") or report.get("failed"))
        self._emit("rollback", step_id=step.step_id, checkpoint=step.checkpoint_hash, ok=bool(report.get("ok")), files=[f["path"] for f in report.get("files", [])][:50])

    def _packet(self, index: int, step: Step, reason: str, source: Candidate | None, target: Candidate | None, failures: list[str], *, keep: bool, result: WorkerResult | None = None) -> handoff.HandoffPacket:
        patch, stat = "", ""
        touched = list(result.files_changed) if result else []
        if keep and step.checkpoint_hash:
            patch, stat = checkpoints.diff_since(self.project, step.checkpoint_hash, paths=diff_paths(step.file_scope))
        packet = handoff.build(
            session_id=self.run_id,
            reason=reason,
            plan_id=self.plan.plan_id,
            step_index=index,
            step_id=step.step_id,
            step_status="partial" if keep and patch else "pending",
            checkpoint_hash=step.checkpoint_hash,
            from_worker=source.key if source else "none",
            to_worker=target.key if target else None,
            diff_stat=stat or (result.diff_stat if result and keep else ""),
            diff_patch=patch,
            files_touched=touched if keep else [],
            failed_attempts=failures,
            open_assumptions=self.plan.assumptions,
            completed_steps=[s.step_id for s in self.plan.steps if s.status == "completed"],
        )
        handoff.save(packet)
        return packet


def diff_paths(file_scope: list[str]) -> list[str] | None:
    """Scope entries as git pathspecs (checkpoints run git with literal pathspecs): a glob is widened to its literal
    directory; one at the project root means "the whole project" (``None``)."""
    out: list[str] = []
    for entry in file_scope:
        clean = scope_mod.normalize(entry).rstrip("/")
        if set("*?[") & set(clean):
            clean = scope_mod._literal_prefix(clean)
            if not clean:
                return None
        if clean:
            out.append(clean)
    return out or None


def _min(*values: Any) -> Any:
    present = [v for v in values if v is not None]
    return min(present) if present else None

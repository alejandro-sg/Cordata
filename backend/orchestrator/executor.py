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

**Complexity routing.** With a ``profile`` (candidate key → ``(tier_rank, unit_price)``) a ``high`` step goes to the
strongest usable tier and a ``low`` one to the cheapest; ties go to the pool with the most headroom, then list order.
``medium`` (and any step without a profile) keeps the first-usable order.

**Parallel waves.** A plan with no ``depends_on`` runs strictly in order, as above. Otherwise the executor runs waves:
pending steps whose dependencies are done, taken greedily in plan order while their scopes are pairwise disjoint, at
most ``max_parallel`` at a time. A step in a wave of several rolls back only its own scope, never the whole project.
"""

from __future__ import annotations

import asyncio
import logging
import math
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


@dataclass
class _StepContext:
    """The mutable state one step's attempts share: who ran last, the note it carries, and the retry rotation."""

    candidates: list[Candidate]
    current: Candidate | None = None
    packet: handoff.HandoffPacket | None = None
    scoped: bool = False
    #: Every other step's scope in the same wave. A worker's changed-files list comes from a before/after snapshot of
    #: the whole project, so in a wave it also lists what its siblings wrote at the same time; those files are theirs,
    #: not stray writes of this step (the kernel scope, or the in-process tool set, is what keeps a step in its own).
    sibling_scope: list[str] = field(default_factory=list)
    reason: str = ""
    last_packet: handoff.HandoffPacket | None = None


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
        profile: dict[str, tuple[int, float]] | None = None,
        max_parallel: int = 3,
        on_usage: Callable[[dict[str, Any]], None] | None = None,
        strongest_first: bool = False,
        controls: Any | None = None,
        on_worker_event: Callable[..., None] | None = None,
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
        #: candidate key → (tier_rank, unit_price); routes high/low-complexity steps.
        self.profile = dict(profile or {})
        self.max_parallel = max(1, int(max_parallel))
        self.on_usage = on_usage
        #: The person's pause/cancel (``control.Controls``): asked before each worker call and between waves; a request to
        #: stop *now* stops the calls in flight (``_stop_inflight``).
        self.controls = controls
        self.on_worker_event = on_worker_event
        #: task id → the worker running it, while it runs.
        self._inflight: dict[str, Worker] = {}
        if controls is not None:
            controls.on_now = self._stop_inflight

    # ------------------------------------------------------------------ choosing

    def _level(self, candidate: Candidate) -> str:
        return self.ledger.level(candidate.family, candidate.model)

    def _headroom_key(self, candidate: Candidate) -> float:
        try:
            return float(self.ledger.state(self.ledger.pool_for(candidate.family, candidate.model)).pct)
        except Exception:
            return 1.0

    def pick(self, exclude_pools: set[str] | None = None, complexity: str = "medium", candidates: list[Candidate] | None = None) -> Candidate | None:
        """The worker for the next attempt: ``ok`` pools before ``soft`` ones; within a level, ``high`` complexity takes
        the highest tier, ``low`` the lowest unit price (ties: most pool headroom, then list order), and ``medium`` the
        first in list order. A candidate missing from the profile ranks as a standard tier with an unknown price."""
        exclude_pools = exclude_pools or set()
        pool = candidates if candidates is not None else self.candidates
        usable = [c for c in pool if self.ledger.pool_for(c.family, c.model).pool_id not in exclude_pools]
        if self.strongest_first:
            return next((c for c in usable if self._level(c) in ("ok", "soft")), None)
        for wanted in ("ok", "soft"):
            level_ok = [c for c in usable if self._level(c) == wanted]
            if not level_ok:
                continue
            if complexity not in ("high", "low") or not self.profile:
                return level_ok[0]

            def rank(item: tuple[int, Candidate]) -> tuple[float, float, int]:
                position, candidate = item
                tier, price = self.profile.get(candidate.key, (1, math.inf))
                primary = -float(tier) if complexity == "high" else float(price)
                return (primary, self._headroom_key(candidate), position)

            return min(enumerate(level_ok), key=rank)[1]
        return None

    def _emit(self, kind: str, **payload: Any) -> None:
        try:
            self.on_event({"kind": kind, "run_id": self.run_id, **payload})
        except Exception:
            logger.debug("executor: event listener failed", exc_info=True)

    # ------------------------------------------------------------------ the person's controls

    def _stop_inflight(self) -> None:
        """Stop every worker call in flight (a request to pause or cancel *now*): each worker kills its process group and
        reports ``cancelled``; the step is rolled back and the request is honoured where that result lands."""
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            return
        for task_id, worker in list(self._inflight.items()):
            loop.create_task(worker.cancel(task_id))

    def _requested(self) -> str | None:
        """``pause`` or ``cancel`` when the person asked for one (a safe point is where this is asked)."""
        if self.controls is None:
            return None
        req = self.controls.pending()
        return req.kind if req is not None else None

    def _user_pause(self, index: int, step: Step, ctx: _StepContext, out: ExecResult, failures: list[str]) -> str:
        packet = self._packet(index, step, "user_pause", ctx.current, None, failures, keep=False)
        ctx.last_packet = out.last_packet = packet
        self._fail(ctx, out, "paused by the person")
        self._emit("paused", step_id=step.step_id, handoff_id=packet.handoff_id, reason="user_pause")
        return "paused"

    # ------------------------------------------------------------------ the loop

    async def run(self) -> ExecResult:
        if any(step.depends_on for step in self.plan.steps):
            return await self._run_waves()
        out = ExecResult(status="completed")
        # Sequential: one context carries the worker, its note and the retry rotation from step to step.
        ctx = _StepContext(candidates=self.candidates, current=self.current, packet=self.packet)
        for index, step in enumerate(self.plan.steps):
            if step.status in ("completed", "skipped"):
                continue
            verdict = await self._run_step(index, step, out, ctx)
            self.candidates, self.current, self.packet = ctx.candidates, ctx.current, ctx.packet
            if verdict != "completed":
                out.status = verdict
                return out
        return out

    async def _run_waves(self) -> ExecResult:
        out = ExecResult(status="completed")
        index_of = {step.step_id: i for i, step in enumerate(self.plan.steps)}
        start_packet = self.packet
        while True:
            done = {s.step_id for s in self.plan.steps if s.status in ("completed", "skipped")}
            pending = [s for s in self.plan.steps if s.status not in ("completed", "skipped")]
            if not pending:
                return out
            asked = self._requested()
            if asked is not None:
                # Between waves: nothing runs, so the request is honoured as it stands.
                out.status = "paused" if asked == "pause" else "cancelled"
                out.reason = "paused by the person" if asked == "pause" else "cancelled by the person"
                return out
            wave = form_wave(pending, done, self.max_parallel)
            if not wave:
                out.status = "failed"
                out.reason = "dependency deadlock: no pending step has all of its depends_on completed (" + ", ".join(s.step_id for s in pending) + ")"
                self._emit("deadlock", pending=[s.step_id for s in pending])
                return out
            self._emit("wave_start", steps=[s.step_id for s in wave])
            contexts = []
            for step in wave:
                siblings = [entry for other in wave if other is not step for entry in other.file_scope]
                ctx = _StepContext(candidates=list(self.candidates), current=self.current, packet=start_packet, scoped=len(wave) > 1, sibling_scope=siblings)
                start_packet = None  # the resume note belongs to the first step that runs
                contexts.append(ctx)
            verdicts = await asyncio.gather(*(self._run_step(index_of[s.step_id], s, out, c) for s, c in zip(wave, contexts)), return_exceptions=True)
            for verdict in verdicts:
                if isinstance(verdict, BaseException):
                    raise verdict
            last = next((c.current for c in reversed(contexts) if c.current is not None), None)
            if last is not None:
                self.current = last
            self.packet = None
            for step, ctx, verdict in zip(wave, contexts, verdicts):
                if verdict != "completed":
                    out.status = verdict
                    out.reason = ctx.reason or out.reason
                    out.last_packet = ctx.last_packet or out.last_packet
                    self.packet = ctx.packet
                    return out

    def _envelope(self):
        budget = self.plan.budget_envelope or {}
        return self.ledger.envelope(self.run_id, max_usd=budget.get("max_usd"), max_tokens=budget.get("max_tokens"))

    def _fail(self, ctx: _StepContext, out: ExecResult, reason: str) -> None:
        ctx.reason = out.reason = reason

    def _record(self, step: Step, candidate: Candidate, result: WorkerResult) -> None:
        row = self.ledger.record_result(session_id=self.run_id, step_id=step.step_id, family=candidate.family, model=candidate.model or result.model, result=result, role="execute", worker=candidate.key)
        if self.on_usage is None:
            return
        try:
            self.on_usage({"run_id": self.run_id, "role": "execute", "worker": candidate.key, "step_id": step.step_id, "pool_id": getattr(row, "pool_id", None), "tokens_in": result.tokens_in, "tokens_out": result.tokens_out, "cost_usd": result.cost_usd})
        except Exception:
            logger.debug("executor: usage listener failed", exc_info=True)

    async def _run_step(self, index: int, step: Step, out: ExecResult, ctx: _StepContext) -> str:
        snap = checkpoints.take(self.project, kind="pre", tool="orchestrator", agent="executor", session_id=self.run_id, turn=step.step_id, call=f"step {index + 1}")
        step.checkpoint_hash = snap.id if snap.ok else None
        if not snap.ok:
            logger.warning("executor: step %s is unprotected (%s); partial work will be kept", step.step_id, snap.reason)
            step.rollback_policy = "keep_partial"
        self._emit("step_start", step_id=step.step_id, index=index, checkpoint=step.checkpoint_hash)
        failures: list[str] = []
        excluded: set[str] = set()
        while True:
            asked = self._requested()
            if asked == "pause":
                step.status = "pending" if step.status == "running" else step.status
                return self._user_pause(index, step, ctx, out, failures)
            if asked == "cancel":
                step.status = "pending" if step.status == "running" else step.status
                self._fail(ctx, out, "cancelled by the person")
                return "cancelled"
            envelope = self._envelope()
            if envelope.exhausted:
                step.status = "failed" if step.status == "running" else step.status
                self._fail(ctx, out, f"the plan's budget envelope is spent (${envelope.spent_usd:.4f}, {envelope.spent_tokens} tokens)")
                self._emit("budget_exhausted", step_id=step.step_id, envelope=envelope.to_json())
                return "budget_exhausted"
            candidate = self.pick(excluded, step.complexity, ctx.candidates)
            if candidate is None:
                packet = self._packet(index, step, "worker_unavailable" if not out.handoffs else out.handoffs[-1].reason, ctx.current, None, failures, keep=False)
                ctx.last_packet = out.last_packet = packet
                self._fail(ctx, out, "no worker has quota left; the run is paused at a safe point")
                self._emit("paused", step_id=step.step_id, handoff_id=packet.handoff_id)
                return "paused"
            if ctx.current is not None and candidate is not ctx.current and ctx.packet is None and out.workers_used:
                # A switch at a step boundary (soft line, or the previous worker's pool went away): leave a note.
                reason = "quota_soft_limit_reached" if self._level(ctx.current) == "soft" else "worker_unavailable"
                ctx.packet = self._packet(index, step, reason, ctx.current, candidate, failures, keep=False)
                out.handoffs.append(ctx.packet)
            ctx.current = candidate
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
                handoff_context=ctx.packet.context() if ctx.packet else None,
            )
            pool_id = self.ledger.pool_for(candidate.family, candidate.model).pool_id
            self._emit("step_dispatch", step_id=step.step_id, worker=candidate.key, pool_id=pool_id, complexity=step.complexity, budget_usd=budget_usd, budget_tokens=task.budget_tokens, handoff=ctx.packet.handoff_id if ctx.packet else None)
            listener = (lambda event, step_id=step.step_id, key=candidate.key, pool=pool_id: self.on_worker_event(event, role="execute", worker=key, step_id=step_id, pool_id=pool)) if self.on_worker_event else None
            self._inflight[task.task_id] = candidate.worker
            try:
                _, result = await collect(candidate.worker.run(task), on_event=listener)
            finally:
                self._inflight.pop(task.task_id, None)
            self._record(step, candidate, result)
            if ctx.sibling_scope:
                result.out_of_scope = [p for p in result.out_of_scope if not scope_mod.matches(p, ctx.sibling_scope, self.project)]
                result.files_changed = [p for p in result.files_changed if not scope_mod.matches(p, ctx.sibling_scope, self.project)]
            out.results.setdefault(step.step_id, []).append({"worker": candidate.key, **{k: getattr(result, k) for k in ("status", "error_message", "tokens_in", "tokens_out", "cost_usd", "files_changed", "out_of_scope", "rate_limited")}})
            self._emit(
                "step_result",
                step_id=step.step_id,
                worker=candidate.key,
                status=result.status,
                cost_usd=round(result.cost_usd, 4),
                tokens_in=result.tokens_in,
                tokens_out=result.tokens_out,
                duration_s=round(result.duration_s, 1),
                error=result.error_message,
                files=result.files_changed[:50],
            )

            if result.status == "completed" and not result.out_of_scope:
                step.status = "completed"
                ctx.packet = None
                return "completed"
            if result.status == "cancelled":
                self._rollback(step, ctx.scoped)
                if self._requested() == "pause":
                    # Stopped mid-call for a pause: the step goes back to pending and runs again on resume.
                    step.status = "pending"
                    return self._user_pause(index, step, ctx, out, failures)
                step.status = "failed"
                self._fail(ctx, out, "cancelled by the person" if self._requested() == "cancel" else "cancelled")
                return "cancelled"
            if result.status == "quota_limit":
                reason = "provider_rate_limit" if result.rate_limited else "quota_hard_limit_reached"
                keep = step.rollback_policy == "keep_partial"
                if not keep:
                    self._rollback(step, ctx.scoped)
                envelope = self._envelope()
                if envelope.exhausted and not result.rate_limited:
                    # The cut-off was the plan's own envelope, not the pool: no other worker may spend more on it.
                    step.status = "partial" if keep else "pending"
                    self._fail(ctx, out, f"the plan's budget envelope is spent (${envelope.spent_usd:.4f}, {envelope.spent_tokens} tokens)")
                    self._emit("budget_exhausted", step_id=step.step_id, envelope=envelope.to_json())
                    return "budget_exhausted"
                failures.append(f"{candidate.key} stopped: {result.error_message or reason}")
                excluded.add(self.ledger.pool_for(candidate.family, candidate.model).pool_id)
                nxt = self.pick(excluded, step.complexity, ctx.candidates)
                ctx.packet = self._packet(index, step, reason, candidate, nxt, failures, keep=keep, result=result)
                out.handoffs.append(ctx.packet)
                self._emit("handoff", step_id=step.step_id, handoff_id=ctx.packet.handoff_id, reason=reason, from_worker=candidate.key, to_worker=nxt.key if nxt else None)
                step.status = "partial" if keep else "pending"
                continue
            # failed, or completed with writes outside the step's scope
            why = result.error_message or "failed"
            if result.out_of_scope:
                why = f"wrote outside the step's scope: {', '.join(result.out_of_scope[:10])}"
            failures.append(f"{candidate.key}: {why}")
            self._rollback(step, ctx.scoped)
            if len(failures) >= self.max_attempts:
                step.status = "failed"
                self._fail(ctx, out, f"step {step.step_id} failed {len(failures)} times: {failures[-1]}")
                return "failed"
            # try another candidate: prefer a different worker for the retry
            others = [c for c in ctx.candidates if c is not candidate]
            if others:
                ctx.candidates = others + [candidate]
            ctx.packet = self._packet(index, step, "worker_failed", candidate, None, failures, keep=False, result=result)
            out.handoffs.append(ctx.packet)

    # ------------------------------------------------------------------ helpers

    def _rollback(self, step: Step, scoped: bool = False) -> None:
        """Restore the step's checkpoint. In a wave of several steps (``scoped``) only the step's own scope goes back —
        the other steps are writing the rest of the project — and a scope that cannot be narrowed is not restored."""
        if not step.checkpoint_hash:
            return
        if scoped:
            paths = diff_paths(step.file_scope)
            if not paths:
                logger.warning("executor: step %s ran in parallel with a scope that cannot be narrowed; not rolled back", step.step_id)
                self._emit("rollback", step_id=step.step_id, checkpoint=step.checkpoint_hash, ok=False, files=[], skipped=True)
                return
            report = checkpoints.restore(self.project, step.checkpoint_hash, paths=paths, session_id=self.run_id)
        else:
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


def scope_prefixes(file_scope: list[str]) -> list[str] | None:
    """The literal prefixes a step may write under; ``None`` when a glob at the root opens the whole project. An empty
    scope (a read-only step) writes nowhere."""
    out: list[str] = []
    for entry in file_scope:
        clean = scope_mod.normalize(entry).rstrip("/")
        if set("*?[") & set(clean):
            clean = scope_mod._literal_prefix(clean).rstrip("/")
            if not clean:
                return None
        if clean:
            out.append(clean)
    return out


def scopes_overlap(a: list[str], b: list[str]) -> bool:
    pa, pb = scope_prefixes(a), scope_prefixes(b)
    if pa is None or pb is None:
        return True  # a root-wide glob overlaps everything
    return any(x == y or x.startswith(y + "/") or y.startswith(x + "/") for x in pa for y in pb)


def form_wave(pending: list[Step], done: set[str], max_parallel: int) -> list[Step]:
    """Pending steps whose dependencies are all done, taken in plan order while their scopes stay pairwise disjoint."""
    wave: list[Step] = []
    for step in pending:
        if len(wave) >= max_parallel:
            break
        if not all(dep in done for dep in step.depends_on):
            continue
        if any(scopes_overlap(step.file_scope, other.file_scope) for other in wave):
            continue
        wave.append(step)
    return wave


def _min(*values: Any) -> Any:
    present = [v for v in values if v is not None]
    return min(present) if present else None

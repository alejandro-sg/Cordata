"""The durable PLAN → EXECUTE → HARDEN → AUDIT state machine.

    PLAN ─► EXECUTE ─► HARDEN ─► AUDIT ─┬─► DONE
      ▲        │                        ├─► drift: a fix pass (≤ 2), then AUDIT again
      │        ▼ (quota)                └─► replan (≤ 1): back to the plan-start checkpoint, PLAN with the reasons
      │     safe-point handoff (executor.py)
      └──────────── replan (≤ 1) ───────────────

Every transition is written to ``state.db`` (``orch_runs`` + an ``orchestrator`` event), so a run outlives the
process that started it: ``resume(run_id)`` picks a *paused* run up where it stopped (every pool was spent; the
handoff packet says where), with its plan, completed steps and counters intact.

Bounded by construction: at most ``max_drift_retries`` fix passes and ``max_replans`` replans, and the plan's budget
envelope over everything. Past any bound the run ends ``failed`` and the project goes back to the checkpoint taken
before the plan's first step — a failed run leaves the tree as it found it.

Who does what is the dispatcher's call (``dispatch.py``): ``rank(role, task_class, exclude_model_families)`` returns
ranked candidates. The machine only enforces the rules no dispatcher may bend: the harden reviewer comes from a
different model family than every executor that touched the change, and nothing learned may skip Tier 1.
"""

from __future__ import annotations

import json
import logging
import sqlite3
import time
import uuid
from collections.abc import Callable
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any, Protocol

import checkpoints
import state_store

from workers import TaskPacket, collect

from . import audit as audit_mod
from . import handoff
from .executor import Candidate, ExecResult, StepExecutor
from .plan_artifact import DRAFT_SCHEMA, Plan, PlanError, Step, from_draft
from .quota_ledger import QuotaLedger

logger = logging.getLogger(__name__)

STATES = ("plan", "execute", "harden", "audit", "done", "failed", "paused", "budget_exhausted", "cancelled")
TERMINAL = frozenset({"done", "failed", "budget_exhausted", "cancelled"})


class Dispatcher(Protocol):
    def rank(self, role: str, task_class: str, *, exclude_model_families: set[str] | None = None) -> list[Candidate]: ...

    def observe(self, role: str, task_class: str, candidate: Candidate, outcome: dict[str, Any]) -> None: ...


@dataclass
class RunConfig:
    max_usd: float = 2.0
    max_tokens: int | None = None
    max_drift_retries: int = 2
    max_replans: int = 1
    #: How many times a step that failed every attempt is tried again — strongest worker first, told to work in small
    #: pieces — before the run gives up on the plan. Completed steps are kept: a replan restores the whole project to
    #: the start of the run, so one stubborn step must not be allowed to cost nine good ones.
    max_step_recoveries: int = 2
    harden: bool = True
    tier2: bool = True
    step_timeout_s: float = 1800.0
    plan_timeout_s: float = 600.0


@dataclass
class RunRecord:
    run_id: str
    goal: str
    cwd: str
    state: str = "plan"
    task_class: str = "general"
    plan: dict[str, Any] | None = None
    start_checkpoint: str | None = None
    drift_retries: int = 0
    replans: int = 0
    recoveries: int = 0
    reason: str = ""
    audit: dict[str, Any] | None = None
    workers: dict[str, list[str]] = field(default_factory=lambda: {"plan": [], "execute": [], "harden": [], "audit": []})
    handoffs: list[str] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)
    history: list[dict[str, Any]] = field(default_factory=list)
    created: float = field(default_factory=time.time)
    updated: float = field(default_factory=time.time)

    def to_json(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> "RunRecord":
        return cls(**{k: v for k, v in data.items() if k in cls.__dataclass_fields__})


def save(record: RunRecord) -> None:
    record.updated = time.time()
    try:
        state_store.connect().execute(
            "INSERT OR REPLACE INTO orch_runs (run_id, created_ts, updated_ts, cwd, goal, state, record_json) VALUES (?,?,?,?,?,?,?)",
            (record.run_id, record.created, record.updated, record.cwd, record.goal, record.state, json.dumps(record.to_json(), default=str)),
        )
    except sqlite3.Error:
        logger.warning("orchestrator: could not persist run %s", record.run_id, exc_info=True)


def artifacts_dir(run_id: str) -> Path:
    """Where a run's readable artifacts live: ``~/.cordata/orchestrator/<run_id>/`` (``CORDATA_HOME`` moves it)."""
    import os

    return Path(os.environ.get("CORDATA_HOME") or Path.home() / ".cordata") / "orchestrator" / run_id


def walkthrough(record: RunRecord) -> str:
    """What the run did, for a person: the outcome, each step with its worker, the audit, handoffs and warnings —
    Antigravity's walkthrough artifact, from the record (no model writes it)."""
    lines = [f"# Walkthrough — {record.run_id}", "", f"**Goal:** {record.goal}", "", f"**Outcome:** {record.state}" + (f" — {record.reason}" if record.reason else "")]

    # Antigravity visual artifact: Mermaid workflow diagram
    plan_worker = record.workers.get("plan", [""])[0] if record.workers.get("plan") else "planner"
    harden_worker = record.workers.get("harden", [""])[0] if record.workers.get("harden") else "reviewer"
    audit_worker = record.workers.get("audit", [""])[0] if record.workers.get("audit") else "auditor"
    lines += ["", "```mermaid", "flowchart TD", f'    Start(["Start"]) --> Plan["Plan ({plan_worker})"]']
    prev_node = "Plan"
    if record.plan and record.plan.get("steps"):
        for i, step in enumerate(record.plan.get("steps", [])):
            sid = f"Step_{i+1}"
            sname = (step.get("name") or f"Step {i+1}").replace('"', "'")
            sworker = step.get("worker") or "executor"
            lines.append(f'    {prev_node} --> {sid}["{sname} ({sworker})"]')
            prev_node = sid
    lines.append(f'    {prev_node} --> Harden["Harden ({harden_worker})"]')
    lines.append(f'    Harden --> Audit["Audit ({audit_worker})"]')
    lines.append(f'    Audit --> End(["{record.state.upper()}"])')
    lines.append("```")

    if record.plan:
        lines += ["", "## Steps"]
        for step in record.plan.get("steps", []):
            lines.append(f"- [{step.get('status')}] **{step.get('name')}** (`{step.get('step_id')}`) — {step.get('worker') or 'not run'}" + (f", {step.get('attempts')} attempt(s)" if (step.get("attempts") or 0) > 1 else ""))
    audit = record.audit or {}
    if audit:
        lines += ["", "## Audit", f"- deterministic checks: {'passed' if audit.get('tier1_ok') else 'FAILED'}" + (f"; reviewer verdict: {audit.get('verdict')}" if audit.get("verdict") else "")]
        lines += [f"- {str(c.get('id'))} ({c.get('kind')}): {'ok' if c.get('ok') else 'FAIL'} — {str(c.get('detail') or '').splitlines()[0] if c.get('detail') else ''}" for c in audit.get("checks", [])]
        if audit.get("changed"):
            lines += ["", "## Changed files"] + [f"- `{path}`" for path in audit["changed"][:200]]
    if record.handoffs or record.drift_retries or record.replans:
        lines += [
            "",
            "## Recovery",
            f"- handoffs: {len(record.handoffs)} · fix passes: {record.drift_retries} · replans: {record.replans}",
            "> [!NOTE]",
            f"> Autonomous self-correction invoked: {len(record.handoffs)} handoff(s), {record.drift_retries} drift fix pass(es), {record.replans} replan(s).",
        ]
    if record.warnings:
        lines += ["", "## Warnings", "> [!WARNING]"] + [f"> - {w}" for w in record.warnings]
    return "\n".join(lines) + "\n"


def write_artifacts(record: RunRecord) -> None:
    """``plan.md`` once there is a plan, ``walkthrough.md`` on every transition (it says where the run stands). Private
    (0700/0600): the plan quotes the goal, which is the person's own words. Never raises."""
    import os

    try:
        target = artifacts_dir(record.run_id)
        target.mkdir(parents=True, exist_ok=True)
        os.chmod(target, 0o700)
        if record.plan:
            (target / "plan.md").write_text(Plan.from_json(record.plan).render(), encoding="utf-8")
        (target / "walkthrough.md").write_text(walkthrough(record), encoding="utf-8")
        for name in ("plan.md", "walkthrough.md"):
            if (target / name).exists():
                os.chmod(target / name, 0o600)
    except OSError:
        logger.debug("orchestrator: could not write the run's artifacts", exc_info=True)


def load(run_id: str) -> RunRecord | None:
    try:
        row = state_store.connect().execute("SELECT record_json FROM orch_runs WHERE run_id = ?", (run_id,)).fetchone()
    except sqlite3.Error:
        return None
    return RunRecord.from_json(json.loads(row["record_json"])) if row else None


def recent(limit: int = 20) -> list[RunRecord]:
    try:
        rows = state_store.connect().execute("SELECT record_json FROM orch_runs ORDER BY updated_ts DESC LIMIT ?", (int(limit),)).fetchall()
    except sqlite3.Error:
        return []
    return [RunRecord.from_json(json.loads(r["record_json"])) for r in rows]


PLAN_BRIEF = (
    "Plan the work below for this project. Steps must be small and ordered; each lists exactly the files it may "
    "create or change (file_scope, project-relative). Acceptance criteria must be checkable without judgement: a "
    "command or test with its expected exit code, a file that must exist or must not change, or 'path::regex'. "
    "Prefer the project's own test command as a criterion."
)


class OrchestratorMachine:
    def __init__(
        self,
        *,
        goal: str,
        project: str,
        dispatcher: Dispatcher,
        ledger: QuotaLedger | None = None,
        config: RunConfig | None = None,
        run_id: str | None = None,
        task_class: str | None = None,
        on_event: Callable[[dict[str, Any]], None] | None = None,
        record: RunRecord | None = None,
    ) -> None:
        import os

        self.project = os.path.realpath(project)
        self.dispatcher = dispatcher
        self.ledger = ledger or QuotaLedger()
        self.config = config or RunConfig()
        self.on_event = on_event or (lambda event: None)
        if record is None:
            from .dispatch import classify

            record = RunRecord(run_id=run_id or f"run-{uuid.uuid4().hex[:10]}", goal=goal, cwd=self.project, task_class=task_class or classify(goal))
        self.record = record
        self.plan: Plan | None = Plan.from_json(record.plan) if record.plan else None
        self._exec_families: set[str] = set()
        self._resume_packet: handoff.HandoffPacket | None = None
        self._recovery_note = ""

    # ------------------------------------------------------------------ bookkeeping

    def _transition(self, state: str, **detail: Any) -> None:
        self.record.state = state
        entry = {"ts": time.time(), "state": state, **{k: v for k, v in detail.items() if v is not None}}
        self.record.history.append(entry)
        if self.plan is not None:
            self.record.plan = self.plan.to_json()
        save(self.record)
        state_store.append_event("orchestrator", {"run_id": self.record.run_id, **entry}, session_id=self.record.run_id, source="orchestrator")
        self._emit("state", **entry)
        write_artifacts(self.record)
        if state in ("done", "failed"):
            self._credit(state == "done")

    def _credit(self, ok: bool) -> None:
        """The run's outcome is every role's reward: a step that "completed" but failed the audit was not a success,
        and a planner whose plan could not be executed was not either. Paused and budget stops credit no one."""
        for role in ("plan", "execute", "harden", "audit"):
            used = list(dict.fromkeys(self.record.workers.get(role, [])))
            if not used:
                continue
            for candidate in self.dispatcher.rank(role, self.record.task_class):
                if candidate.key in used:
                    self._observe(role, candidate, ok=ok, final=True)

    def _emit(self, kind: str, **payload: Any) -> None:
        try:
            self.on_event({"kind": kind, "run_id": self.record.run_id, **payload})
        except Exception:
            logger.debug("orchestrator: event listener failed", exc_info=True)

    def _observe(self, role: str, candidate: Candidate, **outcome: Any) -> None:
        try:
            self.dispatcher.observe(role, self.record.task_class, candidate, outcome)
        except Exception:
            logger.debug("orchestrator: dispatcher.observe failed", exc_info=True)

    def _envelope_left(self) -> tuple[float | None, int | None, bool]:
        env = self.ledger.envelope(self.record.run_id, max_usd=self.config.max_usd, max_tokens=self.config.max_tokens)
        return env.remaining_usd, env.remaining_tokens, env.exhausted

    def _usable(self, ranked: list[Candidate]) -> list[Candidate]:
        return [c for c in ranked if self.ledger.level(c.family, c.model) in ("ok", "soft")]

    async def _one_shot(self, role: str, candidate: Candidate, prompt: str, *, schema: dict[str, Any] | None, scope: list[str] | None = None, timeout: float) -> Any:
        usd, tokens, _ = self._envelope_left()
        allowance = self.ledger.step_allowance(candidate.family, candidate.model)
        task = TaskPacket(
            task_id=f"{self.record.run_id}:{role}:{len(self.record.history)}",
            role=role,
            prompt=prompt,
            cwd=self.project,
            file_scope=scope or [],
            budget_usd=_min(usd, allowance.get("budget_usd")),
            budget_tokens=_min(tokens, allowance.get("budget_tokens")),
            timeout_s=timeout,
            model=candidate.model,
            effort=candidate.effort,
            json_schema=schema,
        )
        self._emit("dispatch", role=role, worker=candidate.key)
        _, result = await collect(candidate.worker.run(task))
        self.ledger.record_result(session_id=self.record.run_id, step_id=role, family=candidate.family, model=candidate.model or result.model, result=result)
        self.record.workers.setdefault(role, []).append(candidate.key)
        return result

    # ------------------------------------------------------------------ phases

    async def _plan(self, feedback: list[str]) -> bool:
        prompt = f"{PLAN_BRIEF}\n\nThe work:\n{self.record.goal}"
        if feedback:
            prompt += "\n\nA previous plan did not work out. Plan again, taking this into account:\n- " + "\n- ".join(feedback[:30])
        problems: list[str] = []
        for candidate in self._usable(self.dispatcher.rank("plan", self.record.task_class))[:2]:
            result = await self._one_shot("plan", candidate, prompt, schema=DRAFT_SCHEMA, timeout=self.config.plan_timeout_s)
            if result.status != "completed":
                problems.append(f"{candidate.key}: {result.error_message or result.status}")
                self._observe("plan", candidate, ok=False)
                continue
            try:
                self.plan = from_draft(result.structured, self.project, max_usd=self.config.max_usd, max_tokens=self.config.max_tokens, version=self.record.replans + 1)
            except PlanError as error:
                problems.append(f"{candidate.key}: the plan was refused: {error}")
                self._observe("plan", candidate, ok=False)
                prompt += "\n\nYour previous answer was refused:\n- " + "\n- ".join(error.problems[:20])
                continue
            self._transition("execute", plan_id=self.plan.plan_id, steps=len(self.plan.steps), planner=candidate.key)
            return True
        self.record.reason = "no usable plan: " + ("; ".join(problems) if problems else "no planner has quota left")
        return False

    async def _execute(self, extra_prompt: str = "", steps: list[Step] | None = None) -> ExecResult:
        assert self.plan is not None
        plan = self.plan
        if steps is not None:  # a fix pass runs its own synthetic step against the same goal and criteria
            plan = Plan.from_json({**self.plan.to_json(), "steps": [asdict(s) for s in steps]})
        start, self._resume_packet = self._resume_packet, None
        recovering = bool(self._recovery_note) and steps is None
        executor = StepExecutor(
            run_id=self.record.run_id,
            project=self.project,
            plan=plan,
            candidates=self._usable(self.dispatcher.rank("execute", self.record.task_class)) or self.dispatcher.rank("execute", self.record.task_class),
            ledger=self.ledger,
            on_event=self.on_event,
            timeout_s=self.config.step_timeout_s,
            extra_prompt=extra_prompt or (self._recovery_note if recovering else ""),
            start_packet=start,
            # A drift fix is the hard case — the plan was done and was not good enough — so it goes to the strongest
            # worker still allowed, not to whichever pool has the most room.
            strongest_first=recovering or steps is not None,
        )
        result = await executor.run()
        if recovering and result.status == "completed":
            self._recovery_note = ""
        if steps is None:
            self.plan = plan
        self.record.handoffs.extend(p.handoff_id for p in result.handoffs)
        for key in result.workers_used:
            self.record.workers.setdefault("execute", []).append(key)
        for candidate in executor.candidates:
            if candidate.key in result.workers_used:
                self._exec_families.add(candidate.model_family)
        return result

    def _executor_families(self) -> set[str]:
        """The model families that wrote this change — this process's, plus a resumed run's from its record."""
        families = set(self._exec_families)
        used = set(self.record.workers.get("execute", []))
        for candidate in self.dispatcher.rank("execute", self.record.task_class):
            if candidate.key in used:
                families.add(candidate.model_family)
        return families

    async def _harden(self) -> None:
        assert self.plan is not None
        families = self._executor_families()
        ranked = self._usable(self.dispatcher.rank("harden", self.record.task_class, exclude_model_families=families))
        ranked = [c for c in ranked if c.model_family not in families]
        if not ranked:
            self.record.warnings.append(f"harden skipped: no reviewer from a model family other than the executors' ({', '.join(sorted(families))}) is enabled and has quota")
            return
        candidate = ranked[0]
        snap = checkpoints.take(self.project, kind="pre", tool="orchestrator", agent="harden", session_id=self.record.run_id, turn="harden")
        diff, stat = checkpoints.diff_since(self.project, self.record.start_checkpoint) if self.record.start_checkpoint else ("", "")
        prompt = (
            f"Goal: {self.plan.goal}\n\nThe change so far ({stat}):\n<<<UNTRUSTED_DIFF>>>\n{audit_mod.secret_patterns.scrub(diff[:24000])[0]}\n<<<END_UNTRUSTED_DIFF>>>\n\n"
            "Review it for bugs, unhandled edge cases and security problems against the goal and these criteria:\n"
            + "\n".join(f"- {c.id}: {c.description}" for c in self.plan.acceptance_criteria)
        )
        result = await self._one_shot("harden", candidate, prompt, schema=None, scope=self.plan.scope_union(), timeout=self.config.step_timeout_s)
        bad = result.status != "completed" or result.out_of_scope
        if bad and snap.ok:
            checkpoints.restore(self.project, snap.id, session_id=self.record.run_id)
        if bad:
            self.record.warnings.append(f"harden by {candidate.key} discarded: {result.error_message or 'wrote outside the scope: ' + ', '.join(result.out_of_scope)}")

    async def _audit(self) -> audit_mod.AuditReport:
        assert self.plan is not None
        reviewer = None
        if self.config.tier2:
            ranked = self._usable(self.dispatcher.rank("audit", self.record.task_class))
            reviewer = ranked[0] if ranked else None
        report = await audit_mod.audit(self.plan, self.project, self.record.start_checkpoint, reviewer, run_id=self.record.run_id)
        if report.tier2_result is not None and reviewer is not None:
            r = report.tier2_result
            self.ledger.record_result(session_id=self.record.run_id, step_id="audit", family=reviewer.family, model=reviewer.model or r.model, result=r)
            self.record.workers.setdefault("audit", []).append(reviewer.key)
        self.record.audit = report.to_json()
        return report

    # ------------------------------------------------------------------ the loop

    async def run(self) -> RunRecord:
        if self.record.start_checkpoint is None:
            snap = checkpoints.take(self.project, kind="pre", tool="orchestrator", agent="machine", session_id=self.record.run_id, turn="plan-start", call=self.record.goal[:80])
            self.record.start_checkpoint = snap.id if snap.ok else None
            if not snap.ok:
                self.record.warnings.append(f"no plan-start checkpoint ({snap.reason}); a failed run cannot be rolled back")
            self._transition("plan" if self.plan is None else self.record.state)
        if self.record.state == "paused":
            packets = handoff.for_session(self.record.run_id)
            self._resume_packet = packets[-1] if packets else None
            self.record.reason = ""
            self._transition("execute", resumed=True, handoff=self._resume_packet.handoff_id if self._resume_packet else None)
        feedback: list[str] = []
        while self.record.state not in TERMINAL and self.record.state != "paused":
            state = self.record.state
            if self._envelope_left()[2]:
                self.record.reason = "the plan's budget envelope is spent"
                self._transition("budget_exhausted")
                break
            if state == "plan":
                if not await self._plan(feedback):
                    self._fail(self.record.reason)
            elif state == "execute":
                result = await self._execute()
                if result.status == "completed":
                    self._transition("harden" if self.config.harden else "audit")
                elif result.status in ("paused", "budget_exhausted", "cancelled"):
                    self.record.reason = result.reason
                    self._transition(result.status, reason=result.reason, handoff=result.last_packet.handoff_id if result.last_packet else None)
                elif self.plan is not None and any(s.status == "completed" for s in self.plan.steps) and self.record.recoveries < self.config.max_step_recoveries:
                    # A step defeated every attempt, but the steps before it are done and good. Keep them: try the
                    # failed step again with the strongest worker allowed, rather than restoring the whole project.
                    self.record.recoveries += 1
                    failed = next((s for s in self.plan.steps if s.status == "failed"), None)
                    self._recovery_note = (
                        f"Earlier attempts at this step failed: {result.reason[:400]}\n"
                        "Work in several SMALL edits instead of one large one — a long file written in a single reply is what "
                        "ran out of room before; create it in pieces. Shell commands may be refused here: do not spend your "
                        "turn on them."
                    )
                    self._transition("execute", recovery=self.record.recoveries, step=failed.step_id if failed else None, reason=result.reason[:300])
                else:
                    self._replan_or_fail([result.reason], "execution failed")
                    feedback = [result.reason]
            elif state == "harden":
                await self._harden()
                self._transition("audit")
            elif state == "audit":
                report = await self._audit()
                accepted = report.tier1_ok and (report.verdict == "accept" or (not report.tier2_used or report.tier2_error) and not report.ambiguous)
                if report.tier1_ok and report.tier2_used and report.tier2_error:
                    self.record.warnings.append(f"tier 2 audit unavailable ({report.tier2_error}); accepted on the deterministic checks")
                if accepted:
                    self._transition("done", verdict=report.verdict or "accept(tier1)")
                elif report.tier1_ok and report.verdict == "replan":
                    self._replan_or_fail(report.items, "the auditor asked for a new plan")
                    feedback = report.items
                else:
                    items = report.failures + report.items + report.warnings
                    if not report.tier1_ok or report.verdict == "drift" or report.ambiguous:
                        if self.record.drift_retries < self.config.max_drift_retries:
                            self.record.drift_retries += 1
                            self._transition("execute", drift_retry=self.record.drift_retries, items=items[:20])
                            await self._fix(items)
                            continue
                        self._replan_or_fail(items, "drift persisted after the retries")
                        feedback = items
                    else:
                        self._fail(f"the audit gave no usable verdict ({report.verdict!r})")
        return self.record

    async def _fix(self, items: list[str]) -> None:
        """One drift retry: a single extra step over the whole plan scope, told exactly what the audit found."""
        assert self.plan is not None
        step = Step(
            step_id=f"fix-{self.record.drift_retries}",
            name="Fix what the audit found",
            description="The audit of the completed plan found these problems. Fix them without undoing the rest:\n- " + "\n- ".join(items[:30]),
            file_scope=self.plan.scope_union(),
        )
        result = await self._execute(steps=[step])
        if result.status == "completed":
            self._transition("audit", fix=step.step_id)
        elif result.status in ("paused", "budget_exhausted", "cancelled"):
            self.record.reason = result.reason
            self._transition(result.status, reason=result.reason)
        else:
            self._transition("audit", fix=step.step_id, fix_failed=result.reason)

    def _keep_discarded(self, why: str) -> None:
        """Before the project goes back to the start of the run, write what is about to be thrown away to the run's
        artifacts folder as a patch. A restore is the right answer to a plan that cannot be made to work, but it is
        irreversible by design — and the first real multi-step run lost nine good steps to one bad one. Never raises."""
        if not self.record.start_checkpoint:
            return
        try:
            patch, stat = checkpoints.diff_since(self.project, self.record.start_checkpoint)
            if not patch.strip():
                return
            target = artifacts_dir(self.record.run_id)
            target.mkdir(parents=True, exist_ok=True)
            name = f"discarded-{len(self.record.history):03d}.patch"
            (target / name).write_text(f"# discarded by the orchestrator: {why}\n# {stat.strip().splitlines()[-1] if stat.strip() else ''}\n{patch}", encoding="utf-8")
            (target / name).chmod(0o600)
            self.record.warnings.append(f"work discarded by a restore ({why}) was saved to {target / name}")
        except Exception:
            logger.debug("orchestrator: could not save the work a restore discards", exc_info=True)

    def _replan_or_fail(self, reasons: list[str], why: str) -> None:
        if self.record.replans < self.config.max_replans:
            self.record.replans += 1
            self.record.drift_retries = 0
            self._keep_discarded(why)
            if self.record.start_checkpoint:
                checkpoints.restore(self.project, self.record.start_checkpoint, session_id=self.record.run_id)
            self.plan = None
            self.record.plan = None
            self._exec_families = set()
            self.record.workers["execute"] = []
            self._transition("plan", replan=self.record.replans, why=why, reasons=reasons[:20])
        else:
            self._fail(f"{why}: " + "; ".join(reasons[:5]))

    def _fail(self, reason: str) -> None:
        self.record.reason = reason
        self._keep_discarded(reason[:120])
        restored = None
        if self.record.start_checkpoint:
            restored = checkpoints.restore(self.project, self.record.start_checkpoint, session_id=self.record.run_id).get("ok")
        self._transition("failed", reason=reason, restored=restored)


async def resume(run_id: str, dispatcher: Dispatcher, *, ledger: QuotaLedger | None = None, config: RunConfig | None = None, on_event: Callable[[dict[str, Any]], None] | None = None) -> RunRecord | None:
    """Continue a paused (or interrupted, non-terminal) run from ``state.db``."""
    record = load(run_id)
    if record is None or record.state in TERMINAL:
        return record
    machine = OrchestratorMachine(goal=record.goal, project=record.cwd, dispatcher=dispatcher, ledger=ledger, config=config, record=record, on_event=on_event)
    return await machine.run()


def _min(*values: Any) -> Any:
    present = [v for v in values if v is not None]
    return min(present) if present else None

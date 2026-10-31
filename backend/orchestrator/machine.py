"""The durable RESEARCH → PLAN → EXECUTE → (SYNTHESIZE) → HARDEN → AUDIT state machine.

    (RESEARCH) ─► PLAN ─► EXECUTE ─► (SYNTHESIZE) ─► HARDEN ─► AUDIT ─┬─► DONE
       │           ▲        │                        ├─► drift: a fix pass (≤ 2), then AUDIT again
       │ refused   │        ▼ (quota)                └─► replan (≤ 1): back to the plan-start checkpoint, PLAN with the reasons
       ▼ twice     │     safe-point handoff (executor.py)
     FAILED        └──────────── replan (≤ 1) ───────────────

SYNTHESIZE runs when EXECUTE harvested isolated parallel steps (``synthesizer.py``: each step of a wave of several ran
in its own copy-on-write sandbox): the combined change is checked deterministically — syntax of every harvested file and
imports between the project's own modules, the drift two agents writing two sides of one interface produce — and
problems get ``max_synth_fixes`` targeted fix passes before HARDEN sees the change.

RESEARCH runs only with ``RunConfig.research``: one planner-tier worker writes a brief whose findings cite the code,
``research.verify`` drops what it cannot check, and the plan must then ground its steps in the surviving finding ids.
With a brief (and ``RunConfig.conformance``) AUDIT runs Tier 1 only and hands a pass to CONFORMANCE: the planner
checks the change against its plan and the brief — ``accept`` → DONE; ``gaps`` → a fix round, then HARDEN → AUDIT →
CONFORMANCE again (≤ ``max_rounds``); ``replan`` or too many rounds → replan (or FAILED).
After every worker call the machine emits a ``usage`` event (the run's spend by role and worker, and every pool's
level); the record keeps the final totals.

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

import asyncio
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
import secret_patterns
import state_store

from workers import TaskPacket, collect

from . import audit as audit_mod
from . import handoff
from . import research as research_mod
from . import rlaif
from . import synthesizer
from .executor import Candidate, ExecResult, StepExecutor
from .plan_artifact import DRAFT_SCHEMA, Plan, PlanError, Step, from_draft
from .quota_ledger import QuotaLedger

logger = logging.getLogger(__name__)

STATES = ("research", "plan", "execute", "synthesize", "harden", "audit", "conformance", "done", "failed", "paused", "budget_exhausted", "cancelled")
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
    research: bool = False
    conformance: bool = True
    max_rounds: int = 3
    max_parallel: int = 3
    #: Run a multi-step wave's steps in copy-on-write sandboxes, and check the harvested change (SYNTHESIZE).
    isolate_waves: bool = True
    max_synth_fixes: int = 1


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
    workers: dict[str, list[str]] = field(default_factory=lambda: {"research": [], "plan": [], "execute": [], "harden": [], "audit": [], "conformance": []})
    research: dict[str, Any] | None = None
    planner: str | None = None
    rounds: int = 0
    usage: list[dict[str, Any]] = field(default_factory=list)
    handoffs: list[str] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)
    history: list[dict[str, Any]] = field(default_factory=list)
    #: The conversation that started it, when a front door did (Mission Control lists a session's runs).
    session_id: str | None = None
    #: The phase a person's pause stopped it in: a resume goes back there, not to EXECUTE.
    resume_state: str | None = None
    #: What SYNTHESIZE reads and found: ``{"harvested": [...], "conflicts": [...], "problems": [...]}``.
    synthesis: dict[str, Any] | None = None
    synth_fixes: int = 0
    #: step id → every attempt at it (worker, status, cost, files, scope escapes): what per-step rewards are read from.
    step_log: dict[str, list[dict[str, Any]]] = field(default_factory=dict)
    created: float = field(default_factory=time.time)
    updated: float = field(default_factory=time.time)

    def to_json(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> "RunRecord":
        record = cls(**{k: v for k, v in data.items() if k in cls.__dataclass_fields__})
        for role in ("research", "plan", "execute", "harden", "audit", "conformance"):
            record.workers.setdefault(role, [])  # a record from before a role existed
        return record


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
    lines += ["", "```mermaid", "flowchart TD"]
    if record.research:
        research_worker = record.planner or (record.workers.get("research") or ["researcher"])[0]
        lines += [f'    Start(["Start"]) --> Research["Research ({research_worker})"]', f'    Research --> Plan["Plan ({plan_worker})"]']
    else:
        lines.append(f'    Start(["Start"]) --> Plan["Plan ({plan_worker})"]')
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
    if record.usage:
        lines += ["", "## Usage", "| role | worker | calls | tokens in | tokens out | cost |", "| --- | --- | ---: | ---: | ---: | ---: |"]
        for row in record.usage:
            lines.append(f"| {row.get('role')} | `{row.get('worker')}` | {int(row.get('calls') or 0)} | {int(row.get('tokens_in') or 0)} | {int(row.get('tokens_out') or 0)} | ${float(row.get('cost_usd') or 0.0):.4f} |")
    if record.warnings:
        lines += ["", "## Warnings", "> [!WARNING]"] + [f"> - {w}" for w in record.warnings]
    artifacts = (["research.md"] if record.research else []) + (["plan.md"] if record.plan else []) + ["walkthrough.md"]
    lines += ["", "## Artifacts"] + [f"- `{name}`" for name in artifacts]
    return "\n".join(lines) + "\n"


def write_artifacts(record: RunRecord) -> None:
    """``research.md`` once there is a verified brief, ``plan.md`` once there is a plan, ``walkthrough.md`` on every
    transition (it says where the run stands). Private (0700/0600): the plan quotes the goal, which is the person's own
    words. Never raises."""
    import os

    try:
        target = artifacts_dir(record.run_id)
        target.mkdir(parents=True, exist_ok=True)
        os.chmod(target, 0o700)
        if record.research:
            (target / "research.md").write_text(research_mod.render(record.research), encoding="utf-8")
        if record.plan:
            (target / "plan.md").write_text(Plan.from_json(record.plan).render(), encoding="utf-8")
        (target / "walkthrough.md").write_text(walkthrough(record), encoding="utf-8")
        for name in ("research.md", "plan.md", "walkthrough.md"):
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
        controls: "Any | None" = None,
        session_id: str | None = None,
    ) -> None:
        import os

        self.project = os.path.realpath(project)
        self.dispatcher = dispatcher
        self.ledger = ledger or QuotaLedger()
        self.config = config or RunConfig()
        self.on_event = on_event or (lambda event: None)
        if record is None:
            from .dispatch import classify

            record = RunRecord(run_id=run_id or f"run-{uuid.uuid4().hex[:10]}", goal=goal, cwd=self.project, task_class=task_class or classify(goal), session_id=session_id)
        self.record = record
        #: The person's pause/cancel requests (``control.Controls``), asked at every safe point; None: nobody can ask.
        self.controls = controls
        self.plan: Plan | None = Plan.from_json(record.plan) if record.plan else None
        self._exec_families: set[str] = set()
        self._resume_packet: handoff.HandoffPacket | None = None
        self._last_report: audit_mod.AuditReport | None = None
        self._recovery_note = ""

    # ------------------------------------------------------------------ bookkeeping

    def _transition(self, state: str, **detail: Any) -> None:
        self.record.state = state
        entry = {"ts": time.time(), "state": state, **{k: v for k, v in detail.items() if v is not None}}
        self.record.history.append(entry)
        if self.plan is not None:
            self.record.plan = self.plan.to_json()
        if state in TERMINAL or state == "paused":
            self.record.usage = self.ledger.run_usage(self.record.run_id)
        save(self.record)
        state_store.append_event("orchestrator", {"run_id": self.record.run_id, **entry}, session_id=self.record.run_id, source="orchestrator")
        self._emit("state", **entry)
        write_artifacts(self.record)
        if state in ("done", "failed"):
            self._credit(state == "done")

    def _credit(self, ok: bool) -> None:
        """Rewards at the end of a run (rlaif.py). Each executor is scored per step on what it did — scope, first-pass,
        recoveries, cost — blended with the conformance rubric, and debited by a failed run only where the failing checks
        implicate its files. The planner, reviewer and auditor still earn the run's verdict (their work is the whole
        change), as a continuous value. Paused and budget stops credit no one."""
        audit = self.record.audit or {}
        tier1_ok = bool(audit.get("tier1_ok", ok))
        by_role: dict[str, list[str]] = {}
        for role in ("research", "plan", "execute", "harden", "audit", "conformance"):
            observed = "plan" if role in ("research", "conformance") else role  # the planner researches and checks conformance
            by_role.setdefault(observed, []).extend(self.record.workers.get(role, []))
        rewards = rlaif.attribute(
            self.record.step_log, run_ok=ok, tier1_ok=tier1_ok, failing_files=self._failing_files(), step_scores=audit.get("step_scores") or []
        ) if self.record.step_log else []
        for role, keys in by_role.items():
            used = list(dict.fromkeys(keys))
            if not used:
                continue
            ranked = {c.key: c for c in self.dispatcher.rank(role, self.record.task_class)}
            if role == "execute" and rewards:
                for reward in rewards:
                    candidate = ranked.get(reward.worker)
                    if candidate is None:
                        continue
                    self._observe(role, candidate, ok=reward.win, reward=reward.value, step_id=reward.step_id, reward_version=reward.reward_version, final=True)
                    last = getattr(self.dispatcher, "_last", {}).get((role, self.record.task_class), {})
                    rlaif.record(reward, run_id=self.record.run_id, task_class=self.record.task_class, context=last.get("context") or f"orch|{role}|{self.record.task_class}", propensity=last.get("propensity"))
                continue
            for key in used:
                if key in ranked:
                    self._observe(role, ranked[key], ok=ok, reward=rlaif.role_reward(ok, tier1_ok=tier1_ok), final=True)

    def _failing_files(self) -> list[str]:
        """Project paths the last audit's failing checks name — what a failed run's blame is aimed at."""
        import re

        report = self._last_report
        if report is None:
            return []
        text = "\n".join(report.failures)
        return sorted(set(re.findall(r"[\w./-]+\.(?:py|rs|ts|tsx|js|go|java|rb|toml|json|ya?ml|md)\b", text)))

    def _emit(self, kind: str, **payload: Any) -> None:
        event = {"kind": kind, "run_id": self.record.run_id, **payload}
        try:
            self.on_event(event)
        except Exception:
            logger.debug("orchestrator: event listener failed", exc_info=True)
        if kind not in ("worker", "state", "usage"):
            # Durable: dispatches, step results, handoffs, rollbacks, waves (states are already, as `orchestrator`;
            # a worker's stream and the usage snapshots are too many and are in the run's events file).
            state_store.append_event("orchestrator_event", json.loads(secret_patterns.scrub(json.dumps(event, default=str))[0]), session_id=self.record.run_id, source="orchestrator")

    def _emit_usage(self, _detail: dict[str, Any] | None = None) -> None:
        """A ``usage`` event after a worker call: the run's spend per pod (worker × role) and in total, where every pool
        stands (with its soft/hard lines), and the run's envelope — what Mission Control's gauges draw."""
        try:
            pools = [s.to_json() for s in self.ledger.states()]
            rows = self.ledger.run_usage(self.record.run_id)
            envelope = self.ledger.envelope(self.record.run_id, max_usd=self.config.max_usd, max_tokens=self.config.max_tokens).to_json()
        except Exception:
            logger.debug("orchestrator: could not read usage", exc_info=True)
            return
        totals = {
            "tokens_in": sum(int(r.get("tokens_in") or 0) for r in rows),
            "tokens_out": sum(int(r.get("tokens_out") or 0) for r in rows),
            "cost_usd": round(sum(float(r.get("cost_usd") or 0.0) for r in rows), 4),
            "calls": sum(int(r.get("calls") or 0) for r in rows),
        }
        self._emit("usage", pods=rows, totals=totals, pools=pools, envelope=envelope)

    def _pool_id(self, candidate: Candidate) -> str | None:
        try:
            return self.ledger.pool_for(candidate.family, candidate.model).pool_id
        except Exception:
            return None

    def worker_event(self, event: Any, *, role: str, worker: str, step_id: str, pool_id: str | None) -> None:
        """One event from a worker's stream (what it is doing right now), for the pod that runs it. Masked; the raw line
        and the final result stay out (the result is reported as ``step_result``)."""
        if event.event_type == "done":
            return
        try:
            payload = json.loads(secret_patterns.scrub(json.dumps(event.payload, default=str))[0])
        except (TypeError, ValueError):
            payload = {}
        self._emit("worker", step_id=step_id, role=role, worker=worker, pool_id=pool_id, event_type=event.event_type, payload=payload, ts=event.ts)

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

    async def _one_shot(self, role: str, candidate: Candidate, prompt: str, *, schema: dict[str, Any] | None, scope: list[str] | None = None, timeout: float, label: str | None = None) -> Any:
        """One worker call. ``role`` is the worker's role (what its sandbox allows); ``label`` is the machine's phase —
        the task id's middle part, the ledger's role and the record's ``workers`` key (default: ``role``)."""
        label = label or role
        usd, tokens, _ = self._envelope_left()
        allowance = self.ledger.step_allowance(candidate.family, candidate.model)
        task = TaskPacket(
            task_id=f"{self.record.run_id}:{label}:{len(self.record.history)}",
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
        self._emit("dispatch", role=label, worker=candidate.key, pool_id=self._pool_id(candidate))
        _, result = await collect(candidate.worker.run(task), on_event=lambda event: self.worker_event(event, role=label, worker=candidate.key, step_id=label, pool_id=self._pool_id(candidate)))
        self.ledger.record_result(session_id=self.record.run_id, step_id=label, family=candidate.family, model=candidate.model or result.model, result=result, role=label, worker=candidate.key)
        self.record.workers.setdefault(label, []).append(candidate.key)
        self._emit_usage()
        return result

    # ------------------------------------------------------------------ phases

    async def _research(self) -> None:
        """The brief the plan is grounded in, from the cheapest usable planner-tier worker (read-only: a ``plan`` task).
        A brief ``research.verify`` refuses is asked for once more with the reasons; refused twice fails the run. A
        worker that gives no brief at all hands over to the next planner; when none does, the run plans without one."""
        problems: list[str] = []
        for candidate in self._usable(self.dispatcher.rank("plan", self.record.task_class)):
            feedback: list[str] = []
            for attempt in (1, 2):
                result = await self._one_shot("plan", candidate, research_mod.prompt(self.record.goal, feedback), schema=research_mod.RESEARCH_SCHEMA, timeout=self.config.plan_timeout_s, label="research")
                if result.status != "completed":
                    problems.append(f"{candidate.key}: {result.error_message or result.status}")
                    break
                verified = research_mod.verify(result.structured, self.project)
                if not verified.refused:
                    self.record.research = verified.brief
                    self.record.planner = candidate.key
                    if verified.dropped:
                        self.record.warnings.append(f"research: {len(verified.dropped)} finding(s) without verifiable evidence dropped ({', '.join(d or '?' for d in verified.dropped[:10])})")
                    self._transition("plan", researcher=candidate.key, findings=len(verified.brief.get("findings") or []), dropped=len(verified.dropped) or None)
                    return
                if attempt == 2:
                    self._observe("plan", candidate, ok=False)
                    self._fail(f"the research brief by {candidate.key} was refused twice: " + "; ".join(verified.reasons[:5]))
                    return
                feedback = verified.reasons[:20]
            self._observe("plan", candidate, ok=False)  # only a failed call gets here: try the next planner
        self.record.warnings.append("research skipped: no usable planner produced a brief" + (f" ({'; '.join(problems[:3])})" if problems else "") + "; planning without one")
        self._transition("plan", research="skipped")

    async def _plan(self, feedback: list[str]) -> bool:
        prompt = f"{PLAN_BRIEF}\n\nThe work:\n{self.record.goal}"
        findings: set[str] | None = None
        if self.record.research:
            findings = {str(f.get("id")) for f in self.record.research.get("findings") or [] if isinstance(f, dict) and f.get("id")}
            prompt += (
                "\n\nA verified research brief of this project follows. Ground every step in it: list the ids of the "
                "findings each step relies on in its grounded_in field, and cite only these ids.\n\n" + research_mod.render(self.record.research)
            )
        if feedback:
            prompt += "\n\nA previous plan did not work out. Plan again, taking this into account:\n- " + "\n- ".join(feedback[:30])
        problems: list[str] = []
        planners = self._usable(self.dispatcher.rank("plan", self.record.task_class))
        if self.record.planner:  # the worker that researched the project plans it, when it still can
            planners.sort(key=lambda c: c.key != self.record.planner)
        for candidate in planners[:2]:
            result = await self._one_shot("plan", candidate, prompt, schema=DRAFT_SCHEMA, timeout=self.config.plan_timeout_s)
            if result.status != "completed":
                problems.append(f"{candidate.key}: {result.error_message or result.status}")
                self._observe("plan", candidate, ok=False)
                continue
            try:
                self.plan = from_draft(result.structured, self.project, max_usd=self.config.max_usd, max_tokens=self.config.max_tokens, version=self.record.replans + 1, findings=findings)
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
            profile=self._profile(),
            max_parallel=self.config.max_parallel,
            on_usage=self._emit_usage,
            controls=self.controls,
            on_worker_event=self.worker_event,
            isolate=self.config.isolate_waves,
            rerank=(lambda step, candidates: self.dispatcher.rerank_step(step, candidates, self.record.task_class)) if hasattr(self.dispatcher, "rerank_step") else None,
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
        for step_id, rows in result.results.items():
            self.record.step_log.setdefault(step_id, []).extend(rows)
        for key in result.workers_used:
            self.record.workers.setdefault("execute", []).append(key)
        for candidate in executor.candidates:
            if candidate.key in result.workers_used:
                self._exec_families.add(candidate.model_family)
        return result

    def _profile(self) -> dict[str, tuple[int, float]]:
        """Candidate key → (tier rank, unit price), from the dispatcher's specs when it has any — what routes a step by
        its complexity. A dispatcher without specs (a fixed order) gives none, and every step keeps the list order."""
        from .dispatch import TIER_RANK

        profile: dict[str, tuple[int, float]] = {}
        for spec in getattr(self.dispatcher, "specs", None) or []:
            try:
                profile[spec.key] = (TIER_RANK.get(spec.tier, 1), float(spec.unit_price))
            except (AttributeError, TypeError, ValueError):
                continue
        return profile

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
        scope = self.plan.scope_union()
        commands = [c.target for c in self.plan.acceptance_criteria if c.type in ("command", "test") and c.target] + list(self.plan.verifier_commands)
        findings = [f for f in (self.record.research or {}).get("findings") or [] if isinstance(f, dict)]
        prompt = (
            f"Goal: {self.plan.goal}\n\nThe change so far ({stat}):\n<<<UNTRUSTED_DIFF>>>\n{audit_mod.secret_patterns.scrub(diff[:24000])[0]}\n<<<END_UNTRUSTED_DIFF>>>\n\n"
            "Review it for bugs, unhandled edge cases and security problems against the goal and these criteria:\n"
            + "\n".join(f"- {c.id}: {c.description}" for c in self.plan.acceptance_criteria)
            + "\n\nYour mandate:\n"
            + "1. Run the plan's acceptance commands:\n"
            + ("\n".join(f"   - `{command}`" for command in commands) if commands else "   (the plan lists none)")
            + "\n2. Fix real failures only, and only inside the step scope: " + (", ".join(scope) or "(none)") + "\n"
            + "3. Close gaps against the research brief"
            + ((":\n" + "\n".join(f"   - [{f.get('id')}] {f.get('claim')}" for f in findings)) if findings else " (there is none: close gaps against the goal and criteria)")
            + "\n4. Do not widen the scope: no new files or edits outside it, no unrelated refactors."
        )
        result = await self._one_shot("harden", candidate, prompt, schema=None, scope=scope, timeout=self.config.step_timeout_s)
        bad = result.status != "completed" or result.out_of_scope
        if bad and snap.ok:
            checkpoints.restore(self.project, snap.id, session_id=self.record.run_id)
        if bad:
            self.record.warnings.append(f"harden by {candidate.key} discarded: {result.error_message or 'wrote outside the scope: ' + ', '.join(result.out_of_scope)}")

    async def _audit(self) -> audit_mod.AuditReport:
        assert self.plan is not None
        reviewer = None
        if self.config.tier2 and not self._conformance_loop():  # in the loop, conformance replaces the Tier-2 reviewer
            ranked = self._usable(self.dispatcher.rank("audit", self.record.task_class))
            reviewer = ranked[0] if ranked else None
        report = await audit_mod.audit(self.plan, self.project, self.record.start_checkpoint, reviewer, run_id=self.record.run_id)
        self._last_report = report
        if report.tier2_result is not None and reviewer is not None:
            r = report.tier2_result
            self.ledger.record_result(session_id=self.record.run_id, step_id="audit", family=reviewer.family, model=reviewer.model or r.model, result=r, role="audit", worker=reviewer.key)
            self.record.workers.setdefault("audit", []).append(reviewer.key)
            self._emit_usage()
        self.record.audit = report.to_json()
        return report

    def _conformance_loop(self) -> bool:
        """The research loop's audit: Tier 1, then the planner's conformance check instead of a Tier-2 reviewer."""
        return bool(self.config.conformance and self.record.research)

    def _conformance_candidate(self) -> Candidate | None:
        """The planner that researched and planned the run checks it. When it is gone or out of quota, the cheapest
        usable standard-tier planner does (with a warning naming both)."""
        from .dispatch import TIER_RANK

        ranked = self.dispatcher.rank("plan", self.record.task_class)
        planner = next((c for c in ranked if c.key == self.record.planner), None) if self.record.planner else None
        if planner is not None and self.ledger.level(planner.family, planner.model) in ("ok", "soft"):
            return planner
        profile = self._profile()
        standard = TIER_RANK["standard"]
        usable = [c for c in self._usable(ranked) if c.key != self.record.planner and profile.get(c.key, (standard, 0.0))[0] == standard]
        usable.sort(key=lambda c: profile.get(c.key, (standard, float("inf")))[1])  # stable: list order breaks ties
        why = "is not ranked for planning" if planner is None else f"is at pool level {self.ledger.level(planner.family, planner.model)}"
        if not usable:
            self.record.warnings.append(f"conformance: planner {self.record.planner or '(none)'} {why} and no usable standard-tier planner can stand in")
            return None
        self.record.warnings.append(f"conformance: planner {self.record.planner or '(none)'} {why}; {usable[0].key} checks conformance instead")
        return usable[0]

    async def _conformance(self) -> list[str] | None:
        """Does the change conform to the plan and the brief? ``accept`` ends the run; ``gaps`` buys a fix round (then
        HARDEN → AUDIT → CONFORMANCE again) up to ``max_rounds``; ``replan`` or too many rounds goes back to PLAN. An
        unavailable or unusable check is a warning, and the run is accepted on Tier 1. Returns the replan feedback, if any."""
        assert self.plan is not None
        report = self._last_report or await audit_mod.tier1(self.plan, self.project, self.record.start_checkpoint)  # a resumed run has no report in memory
        candidate = self._conformance_candidate()
        if candidate is None:
            self._transition("done", verdict="accept(tier1)")
            return None

        async def runner(prompt: str) -> Any:
            return await self._one_shot("plan", candidate, prompt, schema=audit_mod.CONFORMANCE_SCHEMA, timeout=self.config.plan_timeout_s, label="conformance")

        verdict, gaps, result = await audit_mod.conformance(
            self.plan, self.record.research, report, self.project, self.record.start_checkpoint, candidate, run_id=self.record.run_id, timeout_s=self.config.plan_timeout_s, runner=runner
        )
        items = [f"{g['ref']}: {g['missing']} ({g['severity']})" for g in gaps]
        if self.record.audit is not None:
            self.record.audit.update(conformance=verdict or None, conformance_worker=candidate.key, gaps=gaps)
            scores = result.structured.get("step_scores") if isinstance(getattr(result, "structured", None), dict) else None
            if isinstance(scores, list):
                self.record.audit["step_scores"] = [s for s in scores if isinstance(s, dict)][:100]
        if not verdict:
            problem = (result.error_message or result.status) if result.status != "completed" else "no usable verdict in the answer"
            self.record.warnings.append(f"conformance check by {candidate.key} unavailable ({problem}); accepted on the deterministic checks")
            self._transition("done", verdict="accept(tier1)")
            return None
        if verdict == "accept":
            self._transition("done", verdict="accept", conformance=candidate.key)
            return None
        if verdict == "replan":
            reasons = items or ["the conformance check asked for a new plan"]
            self._replan_or_fail(reasons, "the conformance check asked for a new plan")
            return reasons
        self.record.rounds += 1
        if self.record.rounds <= self.config.max_rounds:
            self._transition("execute", round=self.record.rounds, gaps=items[:20])
            await self._fix(items, from_conformance=True)
            return None
        self._replan_or_fail(items, f"conformance gaps remained after {self.config.max_rounds} round(s)")
        return items

    # ------------------------------------------------------------------ the loop

    async def run(self) -> RunRecord:
        if self.record.start_checkpoint is None:
            snap = checkpoints.take(self.project, kind="pre", tool="orchestrator", agent="machine", session_id=self.record.run_id, turn="plan-start", call=self.record.goal[:80])
            self.record.start_checkpoint = snap.id if snap.ok else None
            if not snap.ok:
                self.record.warnings.append(f"no plan-start checkpoint ({snap.reason}); a failed run cannot be rolled back")
            if self.plan is not None:
                first = self.record.state
            elif (not self.record.history and self.config.research) or self.record.state == "research":
                first = "research"  # a fresh run researches first when asked to; an interrupted one picks it back up
            else:
                first = "plan"
            self._transition(first)
        if self.record.state == "paused":
            packets = handoff.for_session(self.record.run_id)
            self._resume_packet = packets[-1] if packets else None
            self.record.reason = ""
            # A person's pause between phases goes back to that phase; a pause inside EXECUTE (quota, or a pause at a
            # step) goes back to EXECUTE, which skips the steps already done.
            back_to, self.record.resume_state = self.record.resume_state or "execute", None
            self._transition(back_to, resumed=True, handoff=self._resume_packet.handoff_id if self._resume_packet else None)
        watcher = asyncio.ensure_future(self._watch_controls()) if self.controls is not None else None
        try:
            return await self._loop()
        except asyncio.CancelledError:
            # Stopped from outside (the orchestrate tool's Stop, a session going away). The record used to be left at
            # whatever phase it was in — "execute" forever — which nothing could tell from a run still working. The
            # work stays where it is (nothing is restored: a cancel is not a verdict on it) and the state says so.
            if self.record.state not in TERMINAL:
                self.record.reason = self.record.reason or "cancelled — the run stopped where it was (its work is kept)"
                try:
                    self._transition("cancelled", reason=self.record.reason)
                except Exception:
                    logger.debug("orchestrator: could not record the cancel", exc_info=True)
            raise
        finally:
            if watcher is not None:
                watcher.cancel()

    # ------------------------------------------------------------------ the person's controls

    async def _watch_controls(self) -> None:
        """Between safe points: notice a request to stop *now*, so the worker call in flight is stopped rather than
        waited for. (A plain pause or cancel waits for the next safe point.)"""
        while True:
            await asyncio.sleep(0.5)
            try:
                self.controls.poll()
            except Exception:
                logger.debug("orchestrator: control poll failed", exc_info=True)

    def _safe_point(self) -> bool:
        """At the top of the loop, between phases: honour a pause or a cancel. True when the loop must stop."""
        if self.controls is None:
            return False
        req = self.controls.pending()
        if req is None:
            return False
        if req.kind == "pause":
            self.record.resume_state = self.record.state
            self.record.reason = "paused by the person"
            self._emit("paused", reason="user_pause", state=self.record.state)
            self._transition("paused", reason="user_pause", resume=self.record.resume_state, by=req.by or None)
            self.controls.clear()
            return True
        self._cancel(req)
        return True

    def _cancel(self, req: Any) -> None:
        """A person's cancel: terminal. The work stays unless they asked for it to go (``restore``), and then it is saved
        as a patch first, exactly as a failed run's is."""
        self.record.reason = "cancelled by the person"
        restored = None
        if getattr(req, "restore", False) and self.record.start_checkpoint:
            self._keep_discarded("cancelled with restore")
            restored = checkpoints.restore(self.project, self.record.start_checkpoint, session_id=self.record.run_id).get("ok")
        self._transition("cancelled", reason=self.record.reason, restored=restored, by=getattr(req, "by", None) or None)
        if self.controls is not None:
            self.controls.clear()

    async def _loop(self) -> RunRecord:
        feedback: list[str] = []
        while self.record.state not in TERMINAL and self.record.state != "paused":
            if self._safe_point():
                break
            state = self.record.state
            if self._envelope_left()[2]:
                self.record.reason = "the plan's budget envelope is spent"
                self._transition("budget_exhausted")
                break
            if state == "research":
                await self._research()
            elif state == "plan":
                if not await self._plan(feedback):
                    self._fail(self.record.reason)
            elif state == "execute":
                result = await self._execute()
                if result.status == "completed" and result.harvested:
                    self.record.synthesis = {"harvested": sorted(set(result.harvested)), "conflicts": sorted(set(result.conflicts)), "problems": []}
                    self._transition("synthesize", harvested=len(self.record.synthesis["harvested"]))
                elif result.status == "completed":
                    self._transition("harden" if self.config.harden else "audit")
                elif result.status == "cancelled" and self.controls is not None and self.controls.pending() is not None:
                    self._cancel(self.controls.pending())
                elif result.status in ("paused", "budget_exhausted", "cancelled"):
                    self.record.reason = result.reason
                    if result.status == "paused" and result.reason == "paused by the person":
                        self.record.resume_state = "execute"
                        if self.controls is not None:
                            self.controls.clear()
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
            elif state == "synthesize":
                await self._synthesize()
            elif state == "harden":
                await self._harden()
                self._transition("audit")
            elif state == "audit":
                report = await self._audit()
                if report.tier1_ok and self._conformance_loop():
                    self._transition("conformance", checks=len(report.checks))
                    continue
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
            elif state == "conformance":
                reasons = await self._conformance()
                if reasons is not None:
                    feedback = reasons
        return self.record

    async def _synthesize(self) -> None:
        """Check the change isolated steps harvested, fix what does not fit together (bounded), then go on to HARDEN.
        A problem still there after the fixes is a warning, not a failure: HARDEN and AUDIT run the real tests."""
        assert self.plan is not None
        synthesis = self.record.synthesis or {"harvested": [], "conflicts": [], "problems": []}
        # The combined change: what the sandboxes handed back and what every other step (a dependent one that ran in
        # the shared tree, a fix pass) wrote — the two sides of an interface are usually one of each.
        files = sorted({*(synthesis.get("harvested") or []), *(f for rows in self.record.step_log.values() for row in rows for f in (row.get("files_changed") or []))})
        problems = synthesizer.check(self.project, files)
        while problems and self.record.synth_fixes < self.config.max_synth_fixes:
            self.record.synth_fixes += 1
            items = [p.render() for p in problems]
            self._emit("synthesis", problems=items[:30], fix=self.record.synth_fixes)
            step = Step(
                step_id=f"synth-{self.record.synth_fixes}",
                name="Make the parallel steps fit together",
                description="Several steps of this plan were written in parallel, each in isolation. Their combined change has "
                "these problems; fix them (signatures, imports, names) without undoing what the steps did:\n- " + "\n- ".join(items[:30]),
                file_scope=self.plan.scope_union(),
            )
            result = await self._execute(steps=[step])
            if result.status in ("paused", "budget_exhausted", "cancelled"):
                self.record.reason = result.reason
                self._transition(result.status, reason=result.reason)
                return
            problems = synthesizer.check(self.project, files)
        synthesis["problems"] = [p.render() for p in problems]
        self.record.synthesis = synthesis
        if problems:
            self.record.warnings.append(f"synthesis: {len(problems)} problem(s) left for harden/audit: " + "; ".join(synthesis["problems"][:3]))
        self._transition("harden" if self.config.harden else "audit", synthesized=len(files), problems=len(problems))

    async def _fix(self, items: list[str], *, from_conformance: bool = False) -> None:
        """One drift retry (or conformance round): a single extra step over the whole plan scope, told exactly what the
        audit found. A conformance round goes through HARDEN again (when on) before the audit."""
        assert self.plan is not None
        if from_conformance:
            step_id, name, found = f"gap-{self.record.rounds}", "Close the conformance gaps", "The conformance check of the completed plan found these gaps. Close them without undoing the rest:\n- "
            after = "harden" if self.config.harden else "audit"
        else:
            step_id, name, found = f"fix-{self.record.drift_retries}", "Fix what the audit found", "The audit of the completed plan found these problems. Fix them without undoing the rest:\n- "
            after = "audit"
        step = Step(step_id=step_id, name=name, description=found + "\n- ".join(items[:30]), file_scope=self.plan.scope_union())
        result = await self._execute(steps=[step])
        if result.status == "completed":
            self._transition(after, fix=step.step_id)
        elif result.status in ("paused", "budget_exhausted", "cancelled"):
            self.record.reason = result.reason
            self._transition(result.status, reason=result.reason)
        else:
            self._transition(after, fix=step.step_id, fix_failed=result.reason)

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
        self.record.rounds = 0
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


async def resume(run_id: str, dispatcher: Dispatcher, *, ledger: QuotaLedger | None = None, config: RunConfig | None = None, on_event: Callable[[dict[str, Any]], None] | None = None, controls: Any | None = None) -> RunRecord | None:
    """Continue a paused (or interrupted, non-terminal) run from ``state.db``."""
    record = load(run_id)
    if record is None or record.state in TERMINAL:
        return record
    machine = OrchestratorMachine(goal=record.goal, project=record.cwd, dispatcher=dispatcher, ledger=ledger, config=config, record=record, on_event=on_event, controls=controls)
    return await machine.run()


def _min(*values: Any) -> Any:
    present = [v for v in values if v is not None]
    return min(present) if present else None

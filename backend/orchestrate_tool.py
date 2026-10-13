"""``orchestrate`` — hand a multi-step coding task to the orchestrator (``backend/orchestrator/``).

The session's agent decides a task deserves a plan, an independent review and a check against that plan; this tool
runs it: a planner writes a plan with file scopes and checkable acceptance criteria, the cheapest capable coding worker
(Claude Code, Antigravity or Ollama — ``orchestrator.dispatch``) executes it step by step inside checkpoints, a model
from another family hardens it, and the audit runs the plan's own checks before a reviewer is asked. Quota limits hand
the work to another pool at a safe point; a run that cannot be finished is rolled back to where it started.

It replaced the in-process sub-agent teams (``local_graph``, ``local_swarm``, the Laya ``team`` pipeline): those
decomposed a task across copies of the session's own model with no file access and no audit; this runs the work in
the coding CLIs built for it, confined to each step's files, and checks the result deterministically.

Safety, the same as ``claude_code_engineer``'s: the call asks (build mode), is refused in plan mode, is armed by a
destructive Laya reading, and the CLIs' egress goes through the vault or not at all. The result is worker-written text
and is fenced as untrusted for the agent that reads it (``prompt_security``).
"""

from __future__ import annotations

import asyncio
import logging
import threading
from collections.abc import AsyncGenerator, Callable
from dataclasses import dataclass
from typing import Any

from strands import tool
from strands.types.tools import ToolContext

import json
import time
import secret_patterns
import harness_events

logger = logging.getLogger(__name__)

TOOL_NAME = "orchestrate"
MAX_GOAL_CHARS = 20_000
_PROGRESS_KINDS = ("state", "step_dispatch", "step_result", "handoff", "rollback", "paused", "budget_exhausted", "dispatch")


@dataclass
class Runtime:
    cwd: Callable[[], str] = lambda: "."
    vault: Callable[[], Any] = lambda: None
    preferences: Callable[[], Any] = lambda: None
    session_id: Callable[[], str | None] = lambda: None


_RUNTIME = Runtime()


def configure(**overrides: Any) -> Runtime:
    global _RUNTIME
    _RUNTIME = Runtime(**overrides)
    return _RUNTIME


def runtime() -> Runtime:
    return _RUNTIME


def _line(event: dict[str, Any]) -> str | None:
    kind = event.get("kind")
    if kind == "state":
        extra = ", ".join(f"{k}={v}" for k, v in event.items() if k in ("plan_id", "steps", "planner", "verdict", "drift_retry", "replan", "reason") and v not in (None, ""))
        return f"[{event.get('state')}]" + (f" {extra}" if extra else "")
    if kind == "step_dispatch":
        return f"step {event.get('step_id')} → {event.get('worker')}"
    if kind == "step_result":
        return f"step {event.get('step_id')} {event.get('status')}" + (f": {str(event.get('error'))[:120]}" if event.get("error") else "")
    if kind == "handoff":
        return f"handoff ({event.get('reason')}): {event.get('from_worker')} → {event.get('to_worker') or 'none'}"
    if kind == "rollback":
        return f"rolled back {event.get('step_id')}"
    if kind == "dispatch":
        return f"{event.get('role')} → {event.get('worker')}"
    if kind in ("paused", "budget_exhausted"):
        return kind
    return None


def summary(record: Any) -> tuple[str, dict[str, Any]]:
    """The tool's text and JSON from a finished (or paused) run record."""
    audit = record.audit or {}
    data = {
        "tool": TOOL_NAME,
        "run_id": record.run_id,
        "state": record.state,
        "ok": record.state == "done",
        "reason": record.reason,
        "plan": None,
        "changed": audit.get("changed", []),
        "audit": {k: audit.get(k) for k in ("tier1_ok", "verdict", "failures", "warnings")} if audit else None,
        "workers": record.workers,
        "handoffs": len(record.handoffs),
        "drift_retries": record.drift_retries,
        "replans": record.replans,
        "warnings": record.warnings,
        "artifacts": None,
    }
    try:
        from orchestrator.machine import artifacts_dir

        data["artifacts"] = str(artifacts_dir(record.run_id))
    except Exception:
        pass
    text = f"Orchestrator run {record.run_id}: {record.state}" + (f" — {record.reason}" if record.reason else "") + "."
    if record.plan:
        steps = record.plan.get("steps", [])
        data["plan"] = {"goal": record.plan.get("goal"), "steps": [{k: s.get(k) for k in ("step_id", "name", "status", "worker")} for s in steps]}
        text += "\nPlan:\n" + "\n".join(f"  {s.get('step_id')} [{s.get('status')}] {s.get('name')} ({s.get('worker') or '—'})" for s in steps)
    if audit:
        text += "\nAudit: " + ("deterministic checks passed" if audit.get("tier1_ok") else "deterministic checks FAILED") + (f"; reviewer: {audit.get('verdict')}" if audit.get("verdict") else "")
        for failure in (audit.get("failures") or [])[:8]:
            text += f"\n  - {str(failure).splitlines()[0][:200]}"
    if data["changed"]:
        text += f"\nChanged {len(data['changed'])} file(s): {', '.join(data['changed'][:15])}{'…' if len(data['changed']) > 15 else ''}"
    for warning in record.warnings[:5]:
        text += f"\nWarning: {warning}"
    if data["artifacts"]:
        text += f"\nPlan and walkthrough: {data['artifacts']}/"
    if record.state == "paused":
        text += f"\nEvery worker's quota is spent; resume later with `python -m orchestrator resume {record.run_id}`."
    return text, data


async def run_orchestration(goal: str, *, workers: str | None = None, max_usd: float | None = None, cancel_signal: threading.Event | None = None) -> AsyncGenerator[dict[str, Any], None]:
    """Yields ``{"progress": text}`` while the run goes, then the Strands ``ToolResult``. Never raises for an
    operational failure."""
    from orchestrator import config
    from orchestrator.machine import OrchestratorMachine
    from workers import build_worker

    rt = _RUNTIME
    goal = (goal or "").strip()
    if not goal:
        yield {"status": "error", "content": [{"text": "orchestrate needs a goal."}]}
        return
    if len(goal) > MAX_GOAL_CHARS:
        yield {"status": "error", "content": [{"text": f"The goal is {len(goal)} characters; the limit is {MAX_GOAL_CHARS}."}]}
        return
    preferences = rt.preferences()
    families = [f.strip() for f in workers.split(",") if f.strip()] if workers else config.enabled_families(preferences)
    built: dict[str, Any] = {}
    for family in families:
        built[family] = build_worker(family, vault=rt.vault) if family in ("claude_code", "antigravity") else build_worker(family)
    ledger = config.ledger(preferences)
    dispatcher = config.dispatcher(preferences, ledger=ledger, project=rt.cwd(), families=families, workers=built)
    run_config = config.run_config(preferences)
    if max_usd is not None:
        run_config.max_usd = max(0.0, float(max_usd))

    queue: asyncio.Queue[str] = asyncio.Queue()

    def on_event(event: dict[str, Any]) -> None:
        if event.get("kind") in _PROGRESS_KINDS:
            line = _line(event)
            if line:
                queue.put_nowait(line)
        forwarded = _forwardable(event)
        if forwarded is not None:
            harness_events.emit("orchestrator", forwarded)

    machine = OrchestratorMachine(goal=goal, project=rt.cwd(), dispatcher=dispatcher, ledger=ledger, config=run_config, on_event=on_event)
    task = asyncio.ensure_future(machine.run())
    lines: list[str] = []
    while not task.done():
        if cancel_signal is not None and cancel_signal.is_set():
            task.cancel()
            break
        try:
            line = await asyncio.wait_for(queue.get(), timeout=0.2)
        except asyncio.TimeoutError:
            continue
        lines.append(line)
        yield {"progress": "\n".join(f"orchestrator: {entry}" for entry in lines[-12:])}
    try:
        record = await task
    except asyncio.CancelledError:
        # The machine has recorded the run as ``cancelled`` on its way out (``OrchestratorMachine.run``).
        record = machine.record
        record.reason = "cancelled by the person — the run stopped where it was (its record is kept)"
    except Exception as error:  # a crash in the machine is a result, not a dead turn
        logger.warning("orchestrate: the run failed: %s", error, exc_info=True)
        yield {"status": "error", "content": [{"text": f"The orchestrator failed: {type(error).__name__}: {error}"}]}
        return
    text, data = summary(record)
    yield {"status": "success" if data["ok"] else "error", "content": [{"text": text}, {"json": data}]}


#: Worker stream events worth a front door's live pod view; text and progress chunks are sampled (``_forwardable``).
_WORKER_KEEP = ("init", "tool_use", "tool_result", "error", "quota")
_worker_seen: dict[str, float] = {}


def _forwardable(event: dict[str, Any]) -> dict[str, Any] | None:
    """An orchestrator event as a front door gets it: every field (Mission Control needs the totals, pools, envelope,
    files and errors a key whitelist used to drop), masked. A worker's own stream is sampled — its tool calls and
    errors always, its text at most twice a second per pod — so a chatty CLI cannot flood the session."""
    if event.get("kind") == "worker" and event.get("event_type") not in _WORKER_KEEP:
        key = f"{event.get('run_id')}/{event.get('step_id')}/{event.get('worker')}"
        now = time.monotonic()
        if now - _worker_seen.get(key, 0.0) < 0.5:
            return None
        _worker_seen[key] = now
    try:
        return json.loads(secret_patterns.scrub(json.dumps(event, default=str))[0])
    except (TypeError, ValueError):
        return None


@tool(context=True)
async def orchestrate(goal: str, tool_context: ToolContext, workers: str | None = None, max_usd: float | None = None) -> AsyncGenerator[dict[str, Any], None]:
    """Hand a multi-step coding task to the orchestrator: it plans the work (each step with the files it may touch and
    checkable acceptance criteria), executes it on the cheapest capable coding CLI (Claude Code, Antigravity or Ollama),
    has a different model family review the change, and checks the result against the plan before reporting.

    Use it for work with several dependent steps where getting the plan and the verification right matters more than
    speed — not for questions, one-file edits or quick fixes (do those yourself). Every run spends the person's coding
    CLI quota; report what it did and whether its checks passed.

    Args:
        goal: The whole task, self-contained: what to build or change, constraints, and how to know it is done.
        workers: Optional comma list of worker families to allow (claude_code, antigravity, ollama).
        max_usd: Optional budget for this run (default: orchestrator.maxUsd).
    """
    async for value in run_orchestration(goal, workers=workers, max_usd=max_usd, cancel_signal=tool_context.cancel_signal):
        yield value

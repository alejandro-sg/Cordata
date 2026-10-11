"""``python -m orchestrator`` — run, inspect and resume orchestrated coding tasks.

    run "<goal>" [--cwd DIR] [--workers claude_code,antigravity,ollama] [--dispatch off|shadow|live]
                 [--max-usd N] [--no-harden] [--no-tier2] [--json]
    status [--json]               pool headroom, recent runs and handoffs — reads state.db, spends nothing
    show RUN_ID [--json]          one run: its plan, history, audit
    resume RUN_ID                 continue a paused run
    replay [--world steep|ceiling] [--logs N] [--ledger]
                                  judge the learned dispatch policy against always-strongest by replay

Settings (``orchestrator.*``, docs/settings.md) give the defaults; flags override them for one run.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys
import time
from typing import Any


def _prefs(cwd: str) -> Any:
    import settings_store

    return settings_store.current(cwd)


def _bar(pct: float | None, width: int = 20) -> str:
    if pct is None:
        return "·" * width
    filled = max(0, min(width, round(pct * width)))
    return "█" * filled + "░" * (width - filled)


def status_data(limit: int = 8) -> dict[str, Any]:
    """Everything ``status`` shows, as data — the daemon serves the same thing to the TUI and Discord."""
    from . import config, handoff, machine

    ledger = config.ledger(_prefs(os.getcwd()))
    pools = [p.to_json() for p in ledger.states()]
    runs = [
        {"run_id": r.run_id, "state": r.state, "goal": r.goal[:120], "cwd": r.cwd, "updated": r.updated, "replans": r.replans, "drift_retries": r.drift_retries, "handoffs": len(r.handoffs), "reason": r.reason[:200]}
        for r in machine.recent(limit)
    ]
    handoffs = [{k: p.to_json()[k] for k in ("handoff_id", "session_id", "timestamp", "reason", "step_id", "step_status", "from_worker", "to_worker")} for p in handoff.recent(limit)]
    return {"pools": pools, "runs": runs, "handoffs": handoffs, "ts": time.time()}


def render_status(data: dict[str, Any]) -> str:
    lines = ["Quota pools"]
    for pool in data["pools"]:
        limit = pool["limit"]
        used = f"{pool['used']:g}/{limit:g} {pool['unit']}" if limit else f"{pool['used']:g} {pool['unit']} (uncapped)"
        source = " · provider" if pool.get("source") == "provider" else ""
        lines.append(f"  {pool['pool_id']:20} {_bar(pool['pct'] if limit or pool.get('provider_pct') is not None else None)} {pool['pct'] * 100:5.1f}%  {pool['level']:9} {used}{source}")
    lines.append("Runs")
    for run in data["runs"] or []:
        lines.append(f"  {run['run_id']:16} {run['state']:16} {run['goal'][:60]}" + (f"  ({run['handoffs']} handoff(s))" if run["handoffs"] else ""))
    if not data["runs"]:
        lines.append("  (none)")
    lines.append("Handoffs")
    for item in data["handoffs"] or []:
        lines.append(f"  {time.strftime('%H:%M', time.localtime(item['timestamp']))} {item['reason']:26} {item['step_id']:8} {item['from_worker']} → {item['to_worker'] or '—'}")
    if not data["handoffs"]:
        lines.append("  (none)")
    return "\n".join(lines)


async def _run(args: argparse.Namespace) -> int:
    from . import config
    from .machine import OrchestratorMachine

    cwd = os.path.realpath(args.cwd)
    prefs = _prefs(cwd)
    families = [f.strip() for f in args.workers.split(",")] if args.workers else config.enabled_families(prefs)
    ledger = config.ledger(prefs)
    dispatcher = config.dispatcher(prefs, ledger=ledger, project=cwd, mode=args.dispatch, families=families)
    run_config = config.run_config(prefs)
    if args.max_usd is not None:
        run_config.max_usd = args.max_usd
    if args.no_harden:
        run_config.harden = False
    if args.no_tier2:
        run_config.tier2 = False

    def show(event: dict[str, Any]) -> None:
        if args.json:
            print(json.dumps(event, default=str), flush=True)
        elif event["kind"] == "state":
            print(f"[{event['state']}]" + "".join(f" {k}={v}" for k, v in event.items() if k not in ("kind", "run_id", "state", "ts")), flush=True)
        elif event["kind"] in ("step_dispatch", "step_result", "handoff", "rollback", "paused", "budget_exhausted"):
            print("  " + event["kind"] + " " + " ".join(f"{k}={v}" for k, v in event.items() if k not in ("kind", "run_id")), flush=True)

    machine = OrchestratorMachine(goal=args.goal, project=cwd, dispatcher=dispatcher, ledger=ledger, config=run_config, on_event=show)
    record = await machine.run()
    if not args.json:
        print(f"\n{record.run_id}: {record.state}" + (f" — {record.reason}" if record.reason else ""))
        for warning in record.warnings:
            print(f"  warning: {warning}")
    return 0 if record.state == "done" else 1


def main(argv: list[str] | None = None) -> int:
    import provider_settings  # noqa: F401  (before anything that imports agent_common, which chdirs)

    parser = argparse.ArgumentParser(prog="orchestrator", description=__doc__.split("\n", 1)[0])
    sub = parser.add_subparsers(dest="cmd", required=True)
    run = sub.add_parser("run")
    run.add_argument("goal")
    run.add_argument("--cwd", default=os.getcwd())
    run.add_argument("--workers")
    run.add_argument("--dispatch", choices=("off", "shadow", "live"))
    run.add_argument("--max-usd", type=float)
    run.add_argument("--no-harden", action="store_true")
    run.add_argument("--no-tier2", action="store_true")
    run.add_argument("--json", action="store_true")
    status = sub.add_parser("status")
    status.add_argument("--json", action="store_true")
    show = sub.add_parser("show")
    show.add_argument("run_id")
    show.add_argument("--json", action="store_true")
    resume = sub.add_parser("resume")
    resume.add_argument("run_id")
    replay = sub.add_parser("replay")
    replay.add_argument("--world", choices=("steep", "ceiling"), default="steep")
    replay.add_argument("--logs", type=int, default=6000)
    replay.add_argument("--ledger", action="store_true", help="judge on this machine's own logged outcomes instead of simulated ones")
    args = parser.parse_args(argv)

    if args.cmd == "run":
        return asyncio.run(_run(args))
    if args.cmd == "status":
        data = status_data()
        print(json.dumps(data, indent=1, default=str) if args.json else render_status(data))
        return 0
    if args.cmd == "show":
        from .machine import load
        from .plan_artifact import Plan

        record = load(args.run_id)
        if record is None:
            print(f"no run {args.run_id}", file=sys.stderr)
            return 2
        if args.json:
            print(json.dumps(record.to_json(), indent=1, default=str))
        else:
            print(f"{record.run_id}  {record.state}  {record.goal}\n")
            if record.plan:
                print(Plan.from_json(record.plan).render())
            for entry in record.history:
                print(f"  {time.strftime('%H:%M:%S', time.localtime(entry['ts']))} {entry['state']}")
            if record.audit:
                print("\nAudit: " + ("passed" if record.audit.get("tier1_ok") else "failed") + (f", verdict {record.audit.get('verdict')}" if record.audit.get("verdict") else ""))
                for failure in record.audit.get("failures", []):
                    print(f"  - {failure.splitlines()[0]}")
        return 0
    if args.cmd == "resume":
        from . import config
        from .machine import load, resume as resume_run

        record = load(args.run_id)
        if record is None:
            print(f"no run {args.run_id}", file=sys.stderr)
            return 2
        prefs = _prefs(record.cwd)
        ledger = config.ledger(prefs)
        done = asyncio.run(resume_run(args.run_id, config.dispatcher(prefs, ledger=ledger, project=record.cwd), ledger=ledger, config=config.run_config(prefs)))
        print(f"{done.run_id}: {done.state}" + (f" — {done.reason}" if done.reason else ""))
        return 0 if done.state == "done" else 1
    if args.cmd == "replay":
        from .dispatch import evaluate, logged_from_ledger
        from .sim import sim_roster, simulate_logs

        specs = sim_roster()
        logs = logged_from_ledger() if args.ledger else simulate_logs(args.logs, specs, world=args.world)
        if not logs:
            print("no logged outcomes yet (orchestrator.dispatch: shadow collects them)")
            return 0
        print(json.dumps(evaluate(logs, specs), indent=1))
        return 0
    return 2


if __name__ == "__main__":
    raise SystemExit(main())

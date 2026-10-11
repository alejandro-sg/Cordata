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


async def _run(args: argparse.Namespace) -> int:
    from . import config
    from .machine import OrchestratorMachine

    cwd = os.path.realpath(args.cwd)
    prefs = _prefs(cwd)
    families = [f.strip() for f in args.workers.split(",")] if args.workers else config.enabled_families(prefs)
    ledger = config.ledger(prefs)
    dispatcher = config.dispatcher(prefs, ledger=ledger, project=cwd, mode=args.dispatch, families=families)
    run_config = config.run_config(prefs)
    if getattr(args, "allow_direct", False):
        os.environ["CORDATA_VAULT_DIRECT"] = "1"
        os.environ["SIERRALAB_VAULT_DIRECT"] = "1"
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
    sub = parser.add_subparsers(dest="cmd")
    run = sub.add_parser("run", help="run an orchestrated coding task (default when goal is given)")
    run.add_argument("goal", help="task goal / instructions")
    run.add_argument("--cwd", default=os.getcwd(), help="target project directory (default: current directory)")
    run.add_argument("--workers", help="comma-separated worker families (e.g. claude_code,antigravity,ollama)")
    run.add_argument("--dispatch", choices=("off", "shadow", "live"), help="worker dispatch policy mode")
    run.add_argument("--allow-direct", action="store_true", help="allow direct network access without an active vault proxy")
    run.add_argument("--max-usd", type=float, help="budget cap envelope in USD")
    run.add_argument("--no-harden", action="store_true", help="skip the cross-family hardening review pass")
    run.add_argument("--no-tier2", action="store_true", help="skip the residual LLM review in audit")
    run.add_argument("--json", action="store_true", help="stream NDJSON events instead of terminal formatting")
    status = sub.add_parser("status", help="pool headroom, recent runs and handoffs")
    status.add_argument("--json", action="store_true")
    show = sub.add_parser("show", help="show one run's plan, history, and audit")
    show.add_argument("run_id")
    show.add_argument("--json", action="store_true")
    resume = sub.add_parser("resume", help="continue a paused run")
    resume.add_argument("run_id")
    replay = sub.add_parser("replay", help="judge the learned dispatch policy against always-strongest")
    replay.add_argument("--world", choices=("steep", "ceiling"), default="steep")
    replay.add_argument("--logs", type=int, default=6000)
    replay.add_argument("--ledger", action="store_true", help="judge on this machine's own logged outcomes instead of simulated ones")

    args_list = list(sys.argv[1:] if argv is None else argv)
    if not args_list:
        parser.print_help()
        return 0

    known_cmds = {"run", "status", "show", "resume", "replay"}
    if not any(arg in known_cmds or arg in ("-h", "--help") for arg in args_list):
        # Allow running directly: orchestrator "Goal" or orchestrator --max-usd 2 "Goal"
        args_list.insert(0, "run")

    args = parser.parse_args(args_list)

    if args.cmd == "run":
        return asyncio.run(_run(args))
    if args.cmd == "status":
        from .status import render, status_data

        data = status_data(os.getcwd())
        print(json.dumps(data, indent=1, default=str) if args.json else render(data, marks=True))
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

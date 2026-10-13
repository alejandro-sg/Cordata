"""``python -m orchestrator`` — run, inspect and resume orchestrated coding tasks.

    run "<goal>" [--cwd DIR] [--workers claude_code,antigravity,ollama] [--dispatch off|shadow|live]
                 [--max-usd N] [--max-rounds N] [--max-parallel N] [--no-research] [--no-harden]
                 [--no-conformance] [--no-tier2] [--json]
    status [--json]               pool headroom, recent runs (with their spend) and handoffs — reads state.db, spends nothing
    show RUN_ID [--json]          one run: its research, plan, history, audit, rounds and usage by role and worker
    resume RUN_ID                 continue a paused run
    replay [--world steep|ceiling] [--logs N] [--ledger]
                                  judge the learned dispatch policy against always-strongest by replay
    doctor [--json] [--cwd DIR]   is everything this tool leans on in place? — local checks, spends nothing

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


def _usage_summary(rows: list[dict[str, Any]]) -> str:
    """``usage 4 call(s) · 12k tokens · $0.0310 — plan 1, execute 2, audit 1`` — one line for a ``usage`` event."""
    from .status import _compact, usage_totals

    totals = usage_totals(rows)
    by_role: dict[str, int] = {}
    for row in rows:
        by_role[str(row.get("role"))] = by_role.get(str(row.get("role")), 0) + int(row.get("calls") or 0)
    roles = ", ".join(f"{role} {calls}" for role, calls in by_role.items())
    return f"usage {totals['calls']} call(s) · {_compact(totals['tokens'])} tokens · ${totals['cost_usd']:.4f}" + (f" — {roles}" if roles else "")


def _usage_table(rows: list[dict[str, Any]]) -> list[str]:
    header = ("role", "worker", "calls", "tokens in", "tokens out", "cost")
    body = [(str(r.get("role")), str(r.get("worker")), str(int(r.get("calls") or 0)), str(int(r.get("tokens_in") or 0)), str(int(r.get("tokens_out") or 0)), f"${float(r.get('cost_usd') or 0.0):.4f}") for r in rows]
    widths = [max(len(row[i]) for row in [header, *body]) for i in range(len(header))]
    fmt = lambda row: "  " + "  ".join(cell.ljust(w) if i < 2 else cell.rjust(w) for i, (cell, w) in enumerate(zip(row, widths)))  # noqa: E731
    return [fmt(header)] + [fmt(row) for row in body]


def _printer(as_json: bool):
    """The event listener for a run on a terminal: one line per state change or worker dispatch, or NDJSON."""

    def show(event: dict[str, Any]) -> None:
        if as_json:
            print(json.dumps(event, default=str), flush=True)
        elif event["kind"] == "usage":
            print("  " + _usage_summary(event.get("totals") or []), flush=True)
        elif event["kind"] == "state":
            print(f"[{event['state']}]" + "".join(f" {k}={v}" for k, v in event.items() if k not in ("kind", "run_id", "state", "ts")), flush=True)
        elif event["kind"] in ("step_dispatch", "step_result", "handoff", "rollback", "paused", "budget_exhausted"):
            print("  " + event["kind"] + " " + " ".join(f"{k}={v}" for k, v in event.items() if k not in ("kind", "run_id")), flush=True)

    return show


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
    if args.max_usd is not None:
        run_config.max_usd = args.max_usd
    if args.no_harden:
        run_config.harden = False
    if args.no_tier2:
        run_config.tier2 = False
    if args.max_rounds is not None:
        run_config.max_rounds = max(0, args.max_rounds)
    if args.max_parallel is not None:
        run_config.max_parallel = max(1, args.max_parallel)
    if args.no_conformance:
        run_config.conformance = False
    if args.no_research:
        run_config.research = False
    if getattr(args, "research", False):
        run_config.research = True

    show = _printer(args.json)

    run_id = args.run_id or None
    machine = OrchestratorMachine(goal=args.goal, project=cwd, dispatcher=dispatcher, ledger=ledger, config=run_config, on_event=show, run_id=run_id, session_id=args.session_id, controls=_controls(run_id))
    record = await machine.run()
    if not args.json:
        print(f"\n{record.run_id}: {record.state}" + (f" — {record.reason}" if record.reason else ""))
        for warning in record.warnings:
            print(f"  warning: {warning}")
    return 0 if record.state == "done" else 1


def _controls(run_id: str | None):
    """The run's control file (``control.Controls``): how a front door that did not start this process pauses or
    cancels it. A run started without an id (a terminal ``cordata run``) gets one from the machine; its file is created
    lazily, so Ctrl+C is still how a terminal stops it."""
    from .control import Controls, run_dir

    if not run_id:
        return Controls(None)
    return Controls(run_dir(run_id) / "control.ndjson")


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
    run.add_argument("--max-rounds", type=int, help="most conformance fix rounds before replanning (default 3)")
    run.add_argument("--max-parallel", type=int, help="most independent steps run at once in a wave (default 3)")
    run.add_argument("--no-research", action="store_true", help="skip the research brief before planning")
    run.add_argument("--no-harden", action="store_true", help="skip the cross-family hardening review pass")
    run.add_argument("--no-conformance", action="store_true", help="skip the conformance check against the goal after audit")
    run.add_argument("--no-tier2", action="store_true", help="skip the residual LLM review in audit")
    run.add_argument("--json", action="store_true", help="stream NDJSON events instead of terminal formatting")
    run.add_argument("--research", action="store_true", help="write a research brief before planning (orchestrator.research)")
    run.add_argument("--run-id", help="the run's id (a front door that starts a run picks it, to watch it before it starts)")
    run.add_argument("--session-id", help="the conversation that started the run (Mission Control lists a session's runs)")
    status = sub.add_parser("status", help="pool headroom, recent runs and handoffs")
    status.add_argument("--json", action="store_true")
    show = sub.add_parser("show", help="show one run's plan, history, and audit")
    show.add_argument("run_id")
    show.add_argument("--json", action="store_true")
    resume = sub.add_parser("resume", help="continue a paused or interrupted run")
    resume.add_argument("run_id")
    resume.add_argument("--allow-direct", action="store_true", help="allow direct network access without an active vault proxy")
    resume.add_argument("--max-usd", type=float, help="budget cap envelope in USD (a resumed run otherwise gets the default, not the original run's)")
    resume.add_argument("--json", action="store_true", help="stream NDJSON events instead of terminal formatting")
    replay = sub.add_parser("replay", help="judge the learned dispatch policy against always-strongest")
    replay.add_argument("--world", choices=("steep", "ceiling"), default="steep")
    replay.add_argument("--logs", type=int, default=6000)
    replay.add_argument("--ledger", action="store_true", help="judge on this machine's own logged outcomes instead of simulated ones")

    doctor = sub.add_parser("doctor", help="check the tools, services and settings this depends on")
    doctor.add_argument("--json", action="store_true")
    doctor.add_argument("--cwd", default=os.getcwd())

    args_list = list(sys.argv[1:] if argv is None else argv)
    if not args_list:
        parser.print_help()
        return 0

    known_cmds = {"run", "status", "show", "resume", "replay", "doctor"}
    if not any(arg in known_cmds or arg in ("-h", "--help") for arg in args_list):
        # Allow running directly: orchestrator "Goal" or orchestrator --max-usd 2 "Goal"
        args_list.insert(0, "run")

    args = parser.parse_args(args_list)

    if args.cmd == "run":
        return asyncio.run(_run(args))
    if args.cmd == "doctor":
        from . import doctor as doctor_module

        checks = doctor_module.run_checks(os.path.realpath(args.cwd))
        print(json.dumps([c.to_json() for c in checks], indent=1) if args.json else doctor_module.render(checks, color=sys.stdout.isatty()))
        return 1 if any(c.level == doctor_module.FAIL for c in checks) else 0
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
            if record.research:
                print(f"Research: {len(record.research.get('findings') or [])} verified finding(s)\n")
            if record.plan:
                print(Plan.from_json(record.plan).render())
            for entry in record.history:
                print(f"  {time.strftime('%H:%M:%S', time.localtime(entry['ts']))} {entry['state']}")
            if record.audit:
                print("\nAudit: " + ("passed" if record.audit.get("tier1_ok") else "failed") + (f", verdict {record.audit.get('verdict')}" if record.audit.get("verdict") else ""))
                for failure in record.audit.get("failures", []):
                    print(f"  - {failure.splitlines()[0]}")
            print(f"\nConformance rounds: {record.rounds}")
            if record.usage:
                print("\nUsage:")
                for line in _usage_table(record.usage):
                    print(line)
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
        if args.allow_direct:
            os.environ["CORDATA_VAULT_DIRECT"] = "1"
        run_config = config.run_config(prefs)
        if args.max_usd is not None:
            run_config.max_usd = args.max_usd
        done = asyncio.run(resume_run(args.run_id, config.dispatcher(prefs, ledger=ledger, project=record.cwd), ledger=ledger, config=run_config, on_event=_printer(args.json), controls=_controls(args.run_id)))
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

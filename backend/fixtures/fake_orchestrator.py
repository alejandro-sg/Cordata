"""A stand-in for ``python -m orchestrator run|resume`` with scripted workers — what ``CORDATA_ORCHESTRATOR_ENTRYPOINT``
points at in the orchestrator-control suite (``_orchestrator_control_smoke_test.py``).

Same arguments as the real command (``run GOAL --cwd DIR --run-id ID --session-id SID --json``, ``resume ID --json``),
the real ``OrchestratorMachine`` and the real control file; only the workers are the testkit's fakes (``testkit``), so
a run plans, executes the calc project's three steps, hardens and audits with no model and no quota.
``FAKE_ORCH_DELAY`` slows every worker call (seconds), so a test has time to pause or cancel mid-run.
"""

import argparse
import asyncio
import json
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import provider_settings  # noqa: F401,E402

import scrubbed_sandbox  # noqa: E402

if not scrubbed_sandbox.seatbelt_works():
    os.environ["CORDATA_SANDBOX_SEATBELT"] = "off"

from orchestrator import machine  # noqa: E402
from orchestrator.__main__ import _controls  # noqa: E402
from orchestrator.dispatch import StaticDispatcher  # noqa: E402
from orchestrator.quota_ledger import Pool, QuotaLedger  # noqa: E402
from orchestrator.testkit import agy_fake, cand, claude_fake, draft, ollama_fake, steps_script  # noqa: E402


def dispatcher() -> StaticDispatcher:
    delay = float(os.environ.get("FAKE_ORCH_DELAY") or 0)
    planner = ollama_fake(plan=draft(), audit={"verdict": "accept", "items": []}, delay=delay)
    executor = claude_fake(steps=steps_script(), delay=delay)
    hardener = agy_fake(delay=delay)
    return StaticDispatcher({"plan": [cand(planner)], "execute": [cand(executor)], "harden": [cand(hardener)], "audit": [cand(planner)]})


def ledger() -> QuotaLedger:
    return QuotaLedger([
        Pool("cc-fake", "claude_code", ".*", unit="tokens", window_limit=10_000_000),
        Pool("agy-fake", "antigravity", ".*", unit="tokens", window_limit=10_000_000),
        Pool("ol-fake", "ollama", ".*", unit="requests", window_limit=10_000),
    ])


def show(event: dict) -> None:
    print(json.dumps(event, default=str), flush=True)


async def main() -> int:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="cmd")
    run = sub.add_parser("run")
    run.add_argument("goal")
    run.add_argument("--cwd", required=True)
    run.add_argument("--run-id")
    run.add_argument("--session-id")
    run.add_argument("--json", action="store_true")
    for flag in ("--no-harden", "--no-research", "--no-conformance", "--no-tier2", "--research"):
        run.add_argument(flag, action="store_true")
    run.add_argument("--max-usd", type=float)
    resume = sub.add_parser("resume")
    resume.add_argument("run_id")
    resume.add_argument("--json", action="store_true")
    args, _unknown = parser.parse_known_args()
    if args.cmd == "run":
        config = machine.RunConfig(harden=not args.no_harden, research=args.research and not args.no_research, conformance=not args.no_conformance, tier2=not args.no_tier2)
        if args.max_usd is not None:
            config.max_usd = args.max_usd
        m = machine.OrchestratorMachine(goal=args.goal, project=args.cwd, dispatcher=dispatcher(), ledger=ledger(), config=config, on_event=show, run_id=args.run_id, session_id=args.session_id, controls=_controls(args.run_id))
        record = await m.run()
    else:
        record = await machine.resume(args.run_id, dispatcher(), ledger=ledger(), on_event=show, controls=_controls(args.run_id))
    return 0 if record is not None and record.state == "done" else 1


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))

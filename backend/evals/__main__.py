"""``python -m evals`` — list, validate, run, compare.

    list                              the tasks, with category and split
    validate                          every task is well-formed, fails before the work, passes after the oracle
    run [--driver oracle|replay|record|live] [--suite dev|heldout|all] [--category C] [--task ID ...]
        [--cassettes DIR] [--out DIR] [--timeout S]
    compare A.json B.json             what changed between two runs
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from .catalog import CATALOG
from .runner import CassetteDriver, LiveDriver, OracleDriver, compare, run_suite, select, validate_task


def default_out() -> Path:
    import settings_store

    return settings_store.user_dir() / "evals"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="evals", description=__doc__.split("\n", 1)[0])
    sub = parser.add_subparsers(dest="cmd", required=True)
    sub.add_parser("list")
    sub.add_parser("validate")
    run = sub.add_parser("run")
    run.add_argument("--driver", choices=("oracle", "replay", "record", "live"), default="oracle")
    run.add_argument("--suite", choices=("dev", "heldout", "all"), default="dev")
    run.add_argument("--category")
    run.add_argument("--task", action="append")
    run.add_argument("--cassettes", default=str(Path(__file__).parent / "cassettes"))
    run.add_argument("--out", default=None)
    run.add_argument("--timeout", type=float, default=180.0)
    cmp_ = sub.add_parser("compare")
    cmp_.add_argument("a")
    cmp_.add_argument("b")
    args = parser.parse_args(argv)

    if args.cmd == "list":
        for task in CATALOG:
            print(f"{task.split:8} {task.category:9} {task.id:24} {task.title}")
        print(f"\n{len(CATALOG)} tasks ({sum(1 for t in CATALOG if t.split == 'heldout')} held out)")
        return 0
    if args.cmd == "validate":
        bad = [p for t in CATALOG for p in validate_task(t)]
        for problem in bad:
            print("INVALID:", problem)
        print(f"{len(CATALOG)} tasks, {len(bad)} problems")
        return 1 if bad else 0
    if args.cmd == "compare":
        print(json.dumps(compare(json.loads(Path(args.a).read_text()), json.loads(Path(args.b).read_text())), indent=2))
        return 0

    driver = {"oracle": OracleDriver, "live": LiveDriver}.get(args.driver, lambda: CassetteDriver(args.cassettes, record=args.driver == "record"))()
    tasks = select(CATALOG, suite=args.suite, category=args.category, ids=args.task)
    if not tasks:
        print("no tasks selected", file=sys.stderr)
        return 2
    out = Path(args.out) if args.out else default_out()

    def show(result) -> None:
        mark = "PASS" if result.passed else "FAIL"
        print(f"{mark} {result.id:24} {result.seconds:5.1f}s  calls={result.calls} asked={result.asked}" + ("" if result.passed else f"  — {'; '.join(result.reasons)[:160]}"), flush=True)

    result = run_suite(tasks, driver, out_dir=out, timeout=args.timeout, on_result=show)
    agg = result.aggregate
    print(f"\n{result.run_id}  driver={result.driver}")
    for split in ("dev", "heldout", "all"):
        block = agg[split]
        if block["tasks"]:
            print(f"  {split:8} {block['passed']}/{block['tasks']} passed  mean reward {block['mean_reward']}")
    return 0 if not agg["failed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

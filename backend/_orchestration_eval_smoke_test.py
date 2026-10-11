"""Phase 5 of the orchestrator (docs/orchestrator-plan.md): the orchestration eval suite and its promotion gate.

The suite runs the whole machine on the eval catalog's coding tasks (``orchestrator/evalsuite.py``, simulated providers)
and verifies every task with its own verifier from outside. This checks that it measures what it claims — pass rate,
cost per success, handoff survival (provider limits injected), plan conformance — and that the gate
(``promote.evaluate_dispatch``) promotes a learned dispatch policy where cheaper workers are as good, and refuses one
that buys its savings with failures.

    backend/.venv/bin/python _orchestration_eval_smoke_test.py
"""

from __future__ import annotations

import asyncio
import os
import sys
import tempfile
from pathlib import Path

_HOME = tempfile.mkdtemp(prefix="orch-eval-home-")
os.environ["SIERRALAB_HOME"] = _HOME
os.environ["SIERRALAB_STATE_DB"] = str(Path(_HOME) / "state.db")

import provider_settings  # noqa: F401,E402
import scrubbed_sandbox  # noqa: E402

if not scrubbed_sandbox.seatbelt_works():
    os.environ["SIERRALAB_SANDBOX_SEATBELT"] = "off"
    print("note: Seatbelt unavailable — audit commands run unconfined in this test")

import promote  # noqa: E402
from harness_testing import check  # noqa: E402
from orchestrator import evalsuite  # noqa: E402
from orchestrator.dispatch import logged_from_ledger  # noqa: E402



def one_per_class() -> list[str]:
    """One task per task class, so the suite sees every kind of work the policy distinguishes."""
    from orchestrator.dispatch import classify
    from orchestrator.sim import coding_tasks

    seen: dict[str, str] = {}
    for task in coding_tasks():
        seen.setdefault(classify(task.prompt), task.id)
    return list(seen.values())


def block(**kw):
    base = {"tasks": 6, "passed": 6, "pass_rate": 1.0, "cost": 6.0, "cost_per_success": 1.0, "handoff_runs": 2, "handoff_survival": 1.0, "plan_conformance": 1.0}
    base.update(kw)
    return base


def test_judge() -> None:
    print("\n=== the dispatch gate's rule ===")
    base = block()
    check(promote.judge_dispatch(base, block(cost_per_success=0.4, cost=2.4))[0], "cheaper at the same pass rate: promote")
    check(not promote.judge_dispatch(base, block(pass_rate=0.95, passed=5, cost_per_success=0.3))[0], "cheaper but a task lost (beyond the 1-point tolerance): hold")
    check(not promote.judge_dispatch(base, block(plan_conformance=0.8, cost_per_success=0.3))[0], "cheaper but less plan-conformant: hold")
    check(not promote.judge_dispatch(base, block(handoff_survival=0.5, cost_per_success=0.3))[0], "cheaper but handoffs survive less often: hold")
    check(not promote.judge_dispatch(base, block())[0], "no cheaper: hold — a policy change needs a reason")


async def test_suite() -> None:
    print("\n=== the suite measures what it says ===")
    ids = one_per_class()
    strongest = await evalsuite.run_suite(policy="strongest", task_ids=ids, world="ceiling")
    learned = await evalsuite.run_suite(policy="learned", task_ids=ids, world="ceiling")
    for result in (strongest, learned):
        agg = result.aggregate("all")
        print(f"  {result.policy:9} " + "  ".join(f"{k}={v}" for k, v in agg.items()))
    s, l = strongest.aggregate("all"), learned.aggregate("all")
    check(s["tasks"] == len(ids) and all(o.state in ("done", "failed") for o in strongest.outcomes), f"every task ran the machine to a verdict ({len(ids)} tasks, one per class)")
    check(all(o.passed == (o.state == "done") for o in strongest.outcomes + learned.outcomes), "the machine's verdict matches the task's own verifier, run from outside")
    check(s["handoff_runs"] >= 1 and s["handoff_survival"] == 1.0, f"injected provider limits were handed off and survived ({s['handoff_runs']} run(s))")
    check(s["plan_conformance"] == 1.0 and l["plan_conformance"] == 1.0, "every passing run stayed inside its plan's scope")
    check(l["cost_per_success"] < s["cost_per_success"] and l["pass_rate"] >= s["pass_rate"] - promote.DISPATCH_PASS_TOLERANCE, f"learned: cheaper per success at the same pass rate ({l['cost_per_success']} vs {s['cost_per_success']}; {l['pass_rate']} vs {s['pass_rate']})")
    used = {key.split(":")[1] for o in learned.outcomes for key in o.executors}
    check(used - {"gemini-3.1-pro-high", "opus"}, f"…because it sent work to cheaper tiers ({sorted(used)})")


async def test_logging_policy() -> None:
    print("\n=== exploration feeds the ledger ===")
    before = len(logged_from_ledger())
    await evalsuite.run_suite(policy="random", limit=3, world="ceiling")
    rows = logged_from_ledger()
    check(len(rows) > before and {r.role for r in rows} >= {"execute", "plan"}, f"a random-policy run writes replayable outcomes for every role ({len(rows) - before} rows)")


def test_gate() -> None:
    print("\n=== the gate, end to end ===")
    good = promote.evaluate_dispatch(candidate="learned", world="ceiling", limit=4)
    check(good.promote and good.kind == "dispatch", f"learned, where cheap tiers are as good: promoted — {good.reason}")
    from orchestrator.sim import coding_tasks

    hard = [t.id for t in coding_tasks() if t.category in ("multifile", "feature")]
    bad = promote.evaluate_dispatch(candidate="cheapest", world="steep", task_ids=hard)
    check(not bad.promote and bad.reason, f"always-cheapest on hard work, where cheap is not as good: held — {bad.reason}")


def main() -> int:
    test_judge()
    asyncio.run(test_suite())
    asyncio.run(test_logging_policy())
    test_gate()
    print("\norchestration eval: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())

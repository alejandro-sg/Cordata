"""The orchestrator's research loop (docs/orchestrator.md): RESEARCH → PLAN → EXECUTE → HARDEN → AUDIT → CONFORMANCE.

On the testkit's fakes, against a real project with a real test suite:

* **role priors** — cold, every role has its own default order: plan/conformance take the cheapest standard tier,
  execute the strong one, audit the standard one; shadow logs what it would have done and returns that order;
* **grounding** — a research brief is verified without a model: a fabricated line, a quote too far from its line, a
  path outside the project or ``.env`` is dropped, and a mostly fabricated brief is refused;
* **plan refusals** — with a brief, a step citing an unknown finding, depending on an unknown step, a cycle, or a plan
  citing nothing is refused; without one (``findings=None``) the old draft is accepted;
* **the machine** — research first, research.md written, the planner checks conformance; a ``gaps`` verdict buys one
  bounded fix round told the gap; endless gaps fail and restore the project; a spent planner pool hands conformance to a
  fallback with a warning; a brief refused twice fails the run;
* **parallel waves** — independent steps overlap, a dependent one waits, overlapping scopes never run together, a
  failing step rolls back only itself; complexity routes high to the strong tier and low to the cheapest;
* **usage** — the run's spend by role and worker equals what the fakes reported, and ``usage`` events carry the pools.

    backend/.venv/bin/python _orchestrator_loop_smoke_test.py
"""

from __future__ import annotations

import asyncio
import copy
import os
import sys
import tempfile
from pathlib import Path
from typing import Any

_HOME = tempfile.mkdtemp(prefix="loop-home-")
os.environ["CORDATA_HOME"] = _HOME
os.environ["CORDATA_STATE_DB"] = str(Path(_HOME) / "state.db")

import provider_settings  # noqa: F401,E402
import scrubbed_sandbox  # noqa: E402

if not scrubbed_sandbox.seatbelt_works():
    # Off macOS (CI) the audit's commands cannot be confined; these are the test's own fixture commands.
    os.environ["CORDATA_SANDBOX_SEATBELT"] = "off"
    print("note: Seatbelt unavailable — audit commands run unconfined in this test")

import bandit as bandit_mod  # noqa: E402
import state_store  # noqa: E402
from harness_testing import check  # noqa: E402
from orchestrator import dispatch as D  # noqa: E402
from orchestrator import machine, research  # noqa: E402
from orchestrator.dispatch import StaticDispatcher  # noqa: E402
from orchestrator.executor import StepExecutor  # noqa: E402
from orchestrator.fakes import FakeWorker, StepScript  # noqa: E402
from orchestrator.plan_artifact import PlanError, from_draft, problems  # noqa: E402
from orchestrator.quota_ledger import Pool, QuotaLedger  # noqa: E402
from orchestrator.sim import sim_roster  # noqa: E402
from orchestrator.testkit import (  # noqa: E402
    FINDINGS, agy_fake, brief, cand, claude_fake, draft, fabricated_brief, grounded_draft, make_project, ollama_fake, parallel_draft, steps_script, tests_pass,
)

GOAL = "Implement the calc package so the tests pass."


class Clock:
    def __init__(self, now: float = 1_900_000_000.0) -> None:
        self.now = now

    def __call__(self) -> float:
        return self.now


def ledger(tag: str, *, cc_limit: float = 10_000_000) -> QuotaLedger:
    return QuotaLedger(
        [
            Pool(f"cc-{tag}", "claude_code", ".*", unit="tokens", window_limit=cc_limit),
            Pool(f"agy-{tag}", "antigravity", ".*", unit="tokens", window_limit=10_000_000),
            Pool(f"ol-{tag}", "ollama", ".*", unit="requests", window_limit=10_000),
        ],
        clock=Clock(),
    )


def answers(*values: dict[str, Any]):
    """A fake's structured answer that changes call by call (the last one repeats)."""
    queue = list(values)

    def answer(task) -> dict[str, Any]:
        return queue.pop(0) if len(queue) > 1 else queue[0]

    return answer


def snapshot(project: Path) -> dict[str, bytes]:
    """Every file of the project but git's own, as bytes."""
    out = {}
    for path in sorted(project.rglob("*")):
        rel = path.relative_to(project)
        if path.is_file() and rel.parts[0] != ".git" and "__pycache__" not in rel.parts:
            out[str(rel)] = path.read_bytes()
    return out


def states(record: machine.RunRecord) -> list[str]:
    return [h["state"] for h in record.history]


async def run(project: Path, disp: Any, led: QuotaLedger, events: list[dict[str, Any]] | None = None, **kw: Any) -> machine.RunRecord:
    kw.setdefault("research", True)
    kw.setdefault("harden", False)
    sink = events.append if events is not None else None
    m = machine.OrchestratorMachine(goal=GOAL, project=str(project), dispatcher=disp, ledger=led, config=machine.RunConfig(**kw), on_event=sink)
    return await m.run()


# ---------------------------------------------------------------------------------------------------- role priors


def table() -> bandit_mod.Bandit:
    return bandit_mod.Bandit(None, path=os.path.join(tempfile.mkdtemp(prefix="table-"), "t.json"))


def test_role_priors() -> None:
    print("\n=== role priors ===")
    specs = sim_roster()
    by_key = {s.key: s for s in specs}

    def cheapest(role: str, tier: str) -> str:
        eligible = [s for s in D.LearnedDispatcher(specs=specs, mode="off", table=table(), log_outcomes=False).eligible(role) if s.tier == tier]
        return min(eligible, key=lambda s: (s.unit_price, s.key)).key

    for mode in ("off", "shadow"):
        learner = D.LearnedDispatcher(specs=specs, mode=mode, explore=0.0, table=table(), log_outcomes=mode == "shadow")
        for role in ("plan", "conformance", "research"):
            first = learner.rank(role, "feature")[0].key
            check(first == cheapest(role, "standard"), f"{mode}: {role} first is the cheapest standard spec ({first})")
        check(by_key[learner.rank("execute", "feature")[0].key].tier == "strong", f"{mode}: execute first is strong")
        check(by_key[learner.rank("audit", "feature")[0].key].tier == "standard", f"{mode}: audit first is standard")
        orders = [[c.key for c in learner.rank(role, "feature")] for role in ("plan", "execute", "audit") for _ in range(3)]
        check(orders[0] == orders[1] == orders[2] and orders[3] == orders[4] == orders[5] and orders[6] == orders[7] == orders[8], f"{mode}: repeated calls give the same order")
        if mode == "shadow":
            expected = [s.key for s in D.LearnedDispatcher.role_order(learner.eligible("plan"), "plan")]
            check([c.key for c in learner.rank("plan", "feature")] == expected, "shadow returns the role order")
            latest = state_store.recent_events(kind="bandit", limit=1)[0]["payload"]
            check(latest.get("action") == "decide" and latest.get("mode") == "shadow", "…and logs a decide event")
    live = D.LearnedDispatcher(specs=specs, mode="live", explore=0.0, table=table(), log_outcomes=False)
    check(by_key[live.rank("execute", "feature")[0].key].tier == "strong", "live, cold: execute first is strong")
    first = [[c.key for c in live.rank(role, "feature")] for role in ("plan", "plan", "execute", "execute")]
    check(first[0] == first[1] and first[2] == first[3], "live, cold: repeated calls give the same order")


# ------------------------------------------------------------------------------------------------------ grounding


def test_grounding(root: Path) -> None:
    print("\n=== grounding ===")
    project = make_project(root, "ground")
    good = research.verify(brief(project), project)
    check(not good.refused and not good.dropped and len(good.brief["findings"]) == len(FINDINGS), "brief(project) verifies, every finding kept")

    def with_evidence(fid: str, evidence: list[dict[str, str]]) -> dict[str, Any]:
        out = copy.deepcopy(brief(project))
        next(f for f in out["findings"] if f["id"] == fid)["evidence"] = evidence
        return out

    def dropped(bad: dict[str, Any], fid: str, label: str) -> None:
        verified = research.verify(bad, project)
        kept = [f["id"] for f in verified.brief["findings"]]
        check(fid in verified.dropped and fid not in kept and not verified.refused, f"dropped: {label} ({verified.reasons[:1]})")

    dropped(with_evidence("f-import", [{"kind": "file", "ref": "tests/test_calc.py:999", "quote": "import calc"}]), "f-import", "a fabricated line number")
    dropped(with_evidence("f-import", [{"kind": "file", "ref": "tests/test_calc.py:12", "quote": "import calc"}]), "f-import", "a quote too far from its line")
    (root / "outside.txt").write_text("import calc\n")
    dropped(with_evidence("f-import", [{"kind": "file", "ref": "../outside.txt:1", "quote": "import calc"}]), "f-import", "a path outside the project")
    dropped(with_evidence("f-import", [{"kind": "file", "ref": ".env:1", "quote": "SECRET=do-not-leak"}]), "f-import", "a quote from .env")
    dropped(with_evidence("f-readme", [{"kind": "doc", "ref": ".env"}]), "f-readme", "a doc ref naming .env")
    fake = research.verify(fabricated_brief(), project)
    check(fake.refused and len(fake.brief["findings"]) == 1, f"fabricated_brief() is refused ({fake.reasons[-1]})")


# -------------------------------------------------------------------------------------------------- plan refusals


def test_plan_refusals(root: Path) -> None:
    print("\n=== plan refusals with findings ===")
    project = make_project(root, "refusals")
    known = set(FINDINGS)
    plan = from_draft(grounded_draft(), str(project), findings=known)
    check(plan.steps[3].grounded_in == ["f-div0"], "a grounded draft is accepted, its citations kept")

    def refused(bad: dict[str, Any], label: str, needle: str) -> None:
        found = problems(bad, str(project), known)
        check(any(needle in p for p in found), f"refused: {label} ({found[:1]})")
        try:
            from_draft(bad, str(project), findings=known)
        except PlanError:
            return
        check(False, f"from_draft raises for {label}")

    refused(grounded_draft(grounded_in={"s1": ["f-nope"]}), "an unknown finding id", "f-nope")
    refused(grounded_draft(depends_on={"s2": ["s9"]}), "a step depending on an unknown step", "s9")
    refused(grounded_draft(depends_on={"s1": ["s2"], "s2": ["s1"]}), "a cycle s1 → s2 → s1", "cycle")
    refused(grounded_draft(grounded_in={sid: [] for sid in ("s1", "s2", "s3", "s4", "s5")}), "a plan with no citations", "grounded_in")
    check(problems(draft(), str(project)) == [] and len(from_draft(draft(), str(project), findings=None).steps) == 5, "findings=None accepts the old draft()")


# -------------------------------------------------------------------------------------------------------- machine

ACCEPT = {"verdict": "accept", "gaps": []}
GAPS = {"verdict": "gaps", "gaps": [{"ref": "f-div0", "missing": "div's zero guard has no message check", "severity": "low"}]}


def roles(planner: FakeWorker, executor: FakeWorker, *more_planners: FakeWorker) -> StaticDispatcher:
    return StaticDispatcher({"plan": [cand(planner), *(cand(p) for p in more_planners)], "execute": [cand(executor)], "harden": [], "audit": [cand(planner)]})


async def test_machine(root: Path) -> None:
    print("\n=== the machine with research ===")
    project = make_project(root, "loop-accept")
    planner = claude_fake(research=brief(project), plan=grounded_draft(), conformance=answers(GAPS, ACCEPT))
    executor = ollama_fake(steps=steps_script())
    record = await run(project, roles(planner, executor), ledger("acc"))
    seen = states(record)
    check(seen[:2] == ["research", "plan"] and seen[-2:] == ["conformance", "done"] and tests_pass(project), f"research → plan … → conformance → done ({seen})")
    check((machine.artifacts_dir(record.run_id) / "research.md").exists(), "research.md is written")
    check(record.planner == cand(planner).key and record.audit and record.audit.get("conformance_worker") == record.planner, f"the planner checks conformance ({record.planner})")
    gap_calls = [c for c in executor.calls if ":gap-" in c.task_id]
    check(len(gap_calls) == 1 and "f-div0" in gap_calls[0].prompt, "gaps → one fix pass, told the gap's ref")
    check(record.rounds == 1 and record.state == "done", f"…one round, then done ({record.rounds})")

    print("\n=== conformance gaps forever ===")
    project = make_project(root, "loop-gaps")
    start = snapshot(project)
    planner = claude_fake(research=brief(project), plan=grounded_draft(), conformance=GAPS)
    executor = ollama_fake(steps=steps_script())
    record = await run(project, roles(planner, executor), ledger("gaps"), max_rounds=2, max_replans=0)
    gap_calls = [c for c in executor.calls if ":gap-" in c.task_id]
    check(record.state == "failed" and "conformance gaps remained" in record.reason, f"always 'gaps' fails ({record.state}: {record.reason[:80]})")
    check(len(gap_calls) == 2 and len([c for c in planner.calls if ":conformance:" in c.task_id]) == 3, f"rounds bounded by max_rounds ({len(gap_calls)} fix passes)")
    check(snapshot(project) == start, "the project is restored to its start bytes")

    print("\n=== the planner's pool spent before conformance ===")
    project = make_project(root, "loop-fallback")
    planner = claude_fake(research=brief(project), plan=grounded_draft(), conformance=ACCEPT)
    fallback = ollama_fake(conformance=ACCEPT)
    executor = agy_fake(steps=steps_script())
    led = ledger("fb", cc_limit=5200)  # research + plan (2 × 2,500 tokens) take it past the hard line
    record = await run(project, roles(planner, executor, fallback), led)
    check(record.planner == cand(planner).key and led.level(planner.capabilities.family.value, planner.model) in ("hard", "exhausted"), "the planner's pool is spent after research and plan")
    check(record.state == "done" and record.audit and record.audit.get("conformance_worker") == cand(fallback).key, f"a fallback standard candidate checks conformance ({record.state}: {record.reason})")
    check(any(cand(fallback).key in w and "conformance" in w for w in record.warnings), f"a warning names the fallback ({record.warnings})")
    check(not [c for c in planner.calls if ":conformance:" in c.task_id], "the spent planner was not called again")

    print("\n=== research refused twice ===")
    project = make_project(root, "loop-refused")
    planner = claude_fake(research=fabricated_brief(), plan=grounded_draft())
    executor = ollama_fake(steps=steps_script())
    record = await run(project, roles(planner, executor), ledger("ref"))
    check(record.state == "failed" and "refused twice" in record.reason, f"research refused twice → failed ({record.reason[:80]})")
    check(len([c for c in planner.calls if ":research:" in c.task_id]) == 2 and not executor.calls, "two research calls, no execution")


# ---------------------------------------------------------------------------------------------- parallel waves


def intervals(worker: FakeWorker) -> dict[str, tuple[float, float]]:
    """Step id → (start, end) of its last call."""
    return {task_id.split(":")[1]: (start, end) for task_id, start, end in worker.call_times}


async def execute(project: Path, plan_draft: dict[str, Any], worker: FakeWorker, tag: str, **kw: Any):
    plan = from_draft(plan_draft, str(project), findings=set(FINDINGS))
    executor = StepExecutor(run_id=f"run-{tag}", project=str(project), plan=plan, candidates=[cand(worker)], ledger=ledger(tag), max_parallel=4, **kw)
    return await executor.run()


async def test_waves(root: Path) -> None:
    print("\n=== parallel waves ===")
    project = make_project(root, "waves")
    worker = ollama_fake(steps=steps_script(), delay=0.2)
    result = await execute(project, parallel_draft(), worker, "w1")
    times = intervals(worker)
    first = [times[s] for s in ("s1", "s2", "s3", "s4")]
    check(result.status == "completed" and tests_pass(project), f"the waves complete the plan ({result.status}: {result.reason})")
    check(max(s for s, _ in first) < min(e for _, e in first), "s1–s4 run at the same time")
    check(times["s5"][0] >= max(e for _, e in first), "s5 starts after all of them end")

    project = make_project(root, "waves-overlap")
    overlapping = parallel_draft()
    overlapping["steps"][1]["file_scope"] = ["calc/add.py", "calc/sub.py"]  # s2 shares calc/add.py with s1
    worker = ollama_fake(steps=steps_script(), delay=0.2)
    result = await execute(project, overlapping, worker, "w2")
    times = intervals(worker)
    check(result.status == "completed", f"the overlapping plan completes ({result.reason})")
    check(times["s2"][0] >= times["s1"][1] or times["s1"][0] >= times["s2"][1], "two steps with overlapping scopes and no dependency do not overlap")

    project = make_project(root, "waves-fail")
    scripts = steps_script()
    scripts["s2"] = StepScript(fail=True)
    worker = ollama_fake(steps=scripts, delay=0.2)
    events: list[dict[str, Any]] = []
    result = await execute(project, parallel_draft(), worker, "w3", on_event=events.append)
    sandboxes = [e for e in events if e.get("kind") == "sandbox"]
    harvests = {e["step_id"]: e for e in events if e.get("kind") == "harvest"}
    check(result.status == "failed", f"a failing step fails the wave ({result.status})")
    check(sandboxes and all(e.get("root") for e in sandboxes) and {e["step_id"] for e in sandboxes} >= {"s1", "s2", "s3", "s4"}, "each step of the wave ran in a sandbox of its own")
    check("s2" not in harvests and not (project / "calc" / "sub.py").exists(), "the failing step's work never reaches the project (its sandbox is discarded, not rolled back)")
    check((project / "calc" / "add.py").read_text() == steps_script()["s1"].writes["calc/add.py"] and "calc/add.py" in harvests["s1"]["written"], "…while its sibling's harvested file is on disk")
    check(not [e for e in sandboxes if e.get("root") and Path(e["root"]).exists()], "no sandbox outlives its step")

    project = make_project(root, "waves-fail-shared")
    worker = ollama_fake(steps=scripts, delay=0.2)
    events = []
    result = await execute(project, parallel_draft(), worker, "w3s", on_event=events.append, isolate=False)
    rollbacks = [e for e in events if e.get("kind") == "rollback"]
    check(result.status == "failed" and rollbacks and {e["step_id"] for e in rollbacks} == {"s2"} and not (project / "calc" / "sub.py").exists(), "shared tree (isolate=False): the failing step (and only it) is rolled back")
    check((project / "calc" / "add.py").read_text() == steps_script()["s1"].writes["calc/add.py"], "…while its sibling's written file stays on disk")

    project = make_project(root, "sequence")
    worker = ollama_fake(steps=steps_script(), delay=0.05)
    result = await execute(project, grounded_draft(), worker, "w4")
    ordered = [times for _, *times in worker.call_times]
    check(result.status == "completed" and all(ordered[i + 1][0] >= ordered[i][1] for i in range(len(ordered) - 1)), "a plan with no depends_on runs strictly in sequence")

    print("\n=== complexity routing ===")
    standard, strong, cheap = claude_fake(), agy_fake(), ollama_fake()
    profile = {cand(standard).key: (1, 0.3), cand(strong).key: (2, 1.0), cand(cheap).key: (0, 0.08)}
    plan = from_draft(grounded_draft(), str(project), findings=set(FINDINGS))
    router = StepExecutor(run_id="run-route", project=str(project), plan=plan, candidates=[cand(standard), cand(strong), cand(cheap)], ledger=ledger("route"), profile=profile)
    check(router.pick(complexity="high").key == cand(strong).key, "high picks the strong-profile candidate")
    check(router.pick(complexity="low").key == cand(cheap).key, "low picks the cheapest")
    check(router.pick(complexity="medium").key == cand(standard).key, "medium keeps the list order")


# ------------------------------------------------------------------------------------------------------------ usage


async def test_usage(root: Path) -> None:
    print("\n=== usage by role and worker ===")
    project = make_project(root, "usage")
    planner = claude_fake(research=brief(project), plan=grounded_draft(), conformance=ACCEPT, tokens_per_plan=(1111, 222))
    executor = ollama_fake(steps=steps_script(tokens=(333, 44)))
    events: list[dict[str, Any]] = []
    record = await run(project, roles(planner, executor), ledger("use"), events)
    check(record.state == "done", f"the run is accepted ({record.state}: {record.reason})")
    rows = {(r["role"], r["worker"]): r for r in record.usage}
    p_key, e_key = cand(planner).key, cand(executor).key
    for role, key, (tin, tout) in (("research", p_key, (1111, 222)), ("plan", p_key, (1111, 222)), ("execute", e_key, (333, 44)), ("conformance", p_key, (1111, 222))):
        row = rows.get((role, key))
        check(row is not None and row["tokens_in"] == tin * row["calls"] and row["tokens_out"] == tout * row["calls"], f"usage ({role}, {key}) equals the scripted tokens ({row})")
    check(rows[("execute", e_key)]["calls"] == 5, "five execute calls")
    usage = [e for e in events if e.get("kind") == "usage"]
    check(bool(usage) and all(e["pools"] and all("pct" in p for p in e["pools"]) for e in usage), f"'usage' events carry every pool's pct ({len(usage)} events)")


async def main_async() -> None:
    root = Path(tempfile.mkdtemp(prefix="loop-"))
    test_grounding(root)
    test_plan_refusals(root)
    await test_machine(root)
    await test_waves(root)
    await test_usage(root)


def main() -> int:
    test_role_priors()
    asyncio.run(main_async())
    print("\norchestrator loop: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())

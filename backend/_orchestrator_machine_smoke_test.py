"""Phase 3 of the orchestrator (docs/orchestrator-plan.md): the plan artifact and the PLAN → EXECUTE → HARDEN → AUDIT
state machine.

The plan contract (what a planner's draft must satisfy), the audit's sandbox (model-written commands run with no
network, no writes but caches, Laya Guard first), and the machine end to end on fake workers against a real project
with a real test suite:

* a plan-conformant run is **accepted** (Tier 1 runs the tests, Tier 2 says accept), with harden on another family;
* a **planted drift** (one step written wrong) is caught by Tier 1 and fixed by one bounded retry;
* a Tier-2 ``drift`` and a Tier-2 ``replan`` verdict are each acted on, once;
* a worker that writes outside its scope **and does not say so** is caught by Tier 1's scope check; the drift
  retries cannot undo it, so the run replans from the plan-start checkpoint and then succeeds;
* an always-wrong executor exhausts **exactly** 2 drift retries, 1 replan and 2 more retries, then fails — and the
  project is restored to how it started;
* the harden reviewer is never the executor's model family (skipped, with a warning, when no other has quota);
* a run that pauses (every pool spent) **resumes** from ``state.db`` and finishes.

    backend/.venv/bin/python _orchestrator_machine_smoke_test.py
"""

from __future__ import annotations

import asyncio
import os
import sys
import tempfile
from pathlib import Path
from typing import Any

_HOME = tempfile.mkdtemp(prefix="machine-home-")
os.environ["CORDATA_HOME"] = _HOME
os.environ["CORDATA_STATE_DB"] = str(Path(_HOME) / "state.db")

import provider_settings  # noqa: F401,E402
import scrubbed_sandbox  # noqa: E402

if not scrubbed_sandbox.seatbelt_works():
    # Off macOS (CI) the audit's commands cannot be confined; these are the test's own fixture commands.
    os.environ["CORDATA_SANDBOX_SEATBELT"] = "off"
    print("note: Seatbelt unavailable — audit commands run unconfined in this test")

from harness_testing import check  # noqa: E402
from orchestrator import audit, machine  # noqa: E402
from orchestrator.dispatch import StaticDispatcher, classify  # noqa: E402
from orchestrator.fakes import FakeWorker, StepScript  # noqa: E402
from orchestrator.plan_artifact import PlanError, from_draft, problems  # noqa: E402
from orchestrator.quota_ledger import Pool, QuotaLedger  # noqa: E402
from orchestrator.testkit import FILES, agy_fake, cand, claude_fake, draft, fix_script, make_project, ollama_fake, steps_script, tests_pass  # noqa: E402

ACCEPT = {"verdict": "accept", "items": []}


class Clock:
    def __init__(self, now: float = 1_900_000_000.0) -> None:
        self.now = now

    def __call__(self) -> float:
        return self.now


def ledger(tag: str, clock: Clock | None = None) -> QuotaLedger:
    return QuotaLedger(
        [
            Pool(f"cc-{tag}", "claude_code", ".*", unit="tokens", window_limit=10_000_000),
            Pool(f"agy-{tag}", "antigravity", ".*", unit="tokens", window_limit=10_000_000),
            Pool(f"ol-{tag}", "ollama", ".*", unit="requests", window_limit=10_000),
        ],
        clock=clock or Clock(),
    )


def answers(*values: dict[str, Any]):
    """A fake's structured answer that changes call by call (the last one repeats)."""
    queue = list(values)

    def answer(task) -> dict[str, Any]:
        return queue.pop(0) if len(queue) > 1 else queue[0]

    return answer


def dispatcher(*, planner: FakeWorker, executor: FakeWorker | list[FakeWorker], reviewer: FakeWorker, hardener: FakeWorker | None = None) -> StaticDispatcher:
    executors = executor if isinstance(executor, list) else [executor]
    hardeners = [hardener] if hardener else [agy_fake(), executors[0]]
    return StaticDispatcher({
        "plan": [cand(planner)],
        "execute": [cand(e) for e in executors],
        "harden": [cand(h) for h in hardeners],
        "audit": [cand(reviewer)],
    })


async def run(project: Path, disp: StaticDispatcher, tag: str, **kw: Any) -> machine.RunRecord:
    m = machine.OrchestratorMachine(goal="Implement the calc package so the tests pass.", project=str(project), dispatcher=disp, ledger=kw.pop("led", None) or ledger(tag), config=machine.RunConfig(**kw))
    return await m.run()


def states(record: machine.RunRecord) -> list[str]:
    return [h["state"] for h in record.history]


# ---------------------------------------------------------------------------------------------------------- plan


def test_plan_artifact(root: Path) -> None:
    print("\n=== the plan artifact ===")
    project = make_project(root, "planart")
    plan = from_draft(draft(), str(project), max_usd=1.5)
    check(len(plan.steps) == 5 and plan.budget_envelope["max_usd"] == 1.5 and plan.plan_id.startswith("plan-"), "a good draft becomes a plan with an id and an envelope")
    check(plan.scope_union() == [p for _, p in [("s1", "calc/add.py"), ("s2", "calc/sub.py"), ("s3", "calc/mul.py"), ("s4", "calc/div.py"), ("s5", "calc/__init__.py")]], "the scope union, in step order")
    check("## Steps" in plan.render() and "calc/mul.py" in plan.render(), "it renders for a person")

    def refused(mutate, label: str) -> None:
        bad = draft()
        mutate(bad)
        found = problems(bad, str(project))
        check(bool(found), f"refused: {label} ({found[:1]})")

    refused(lambda d: d["steps"][0].update(file_scope=[]), "an editing step with no scope")
    refused(lambda d: d["steps"][0].update(file_scope=["../../etc/passwd"]), "a scope that leaves the project")
    refused(lambda d: d["steps"][0].update(file_scope=["/etc/hosts"]), "an absolute scope")
    refused(lambda d: d["steps"][0].update(file_scope=[".env"]), "a secrets file in scope")
    refused(lambda d: d["steps"][1].update(step_id="s1"), "duplicate step ids")
    refused(lambda d: d.update(acceptance_criteria=[{"id": "x", "type": "command", "description": "trust me", "target": ""}]), "no checkable criterion")
    refused(lambda d: d["acceptance_criteria"].append({"id": "c", "type": "file_not_changed", "description": "", "target": "calc/add.py"}), "a criterion that contradicts a step's scope")
    refused(lambda d: d["acceptance_criteria"].append({"id": "r", "type": "regex_match", "description": "", "target": "calc/add.py::(unclosed"}), "a regex that does not compile")
    refused(lambda d: d["acceptance_criteria"].append({"id": "t", "type": "vibes", "description": ""}), "an unknown criterion type")
    try:
        from_draft({"goal": "x"}, str(project))
        check(False, "from_draft raises PlanError")
    except PlanError as error:
        check(len(error.problems) >= 2, "from_draft raises PlanError listing every problem")
    check(classify("Fix the crash in the parser") == "bugfix" and classify("Update the README") == "docs" and classify("add retries") == "feature", "task classes from the goal")


async def test_audit_sandbox(root: Path) -> None:
    print("\n=== the audit's command sandbox ===")
    project = make_project(root, "auditsb")
    ok, detail = await audit.run_command(str(project), "rm -rf /", timeout=5)
    check(not ok and "refused" in detail, f"a destructive command is refused before it runs ({detail[:60]})")
    if scrubbed_sandbox.seatbelt_works():
        ok, _ = await audit.run_command(str(project), "echo x > newfile.txt", timeout=10)
        check(not ok and not (project / "newfile.txt").exists(), "a criterion cannot write in the project")
        ok, detail = await audit.run_command(str(project), "/usr/bin/curl -s -m 5 -o /dev/null https://example.com", timeout=15)
        check(not ok, "…or reach the network")
        ok, _ = await audit.run_command(str(project), f"{sys.executable} -c 'import compileall,sys; sys.exit(0 if compileall.compile_dir(\"tests\", quiet=1) else 1)'", timeout=30)
        check(ok and (project / "tests" / "__pycache__").is_dir(), "…but tool caches are writable")
        ok, detail = await audit.run_command(str(project), "cat .env", timeout=10)
        check(not ok and "do-not-leak" not in detail, ".env stays unreadable")


# ------------------------------------------------------------------------------------------------------ machine


async def test_accept(root: Path) -> None:
    print("\n=== a plan-conformant run is accepted ===")
    project = make_project(root, "accept")
    planner = ollama_fake(plan=draft(), audit=ACCEPT)
    executor = claude_fake(steps=steps_script())
    hardener = agy_fake(harden=StepScript(writes={}))
    disp = dispatcher(planner=planner, executor=executor, reviewer=planner, hardener=hardener)
    events: list[dict] = []
    m = machine.OrchestratorMachine(goal="Implement the calc package so the tests pass.", project=str(project), dispatcher=disp, ledger=ledger("accept"), on_event=events.append)
    record = await m.run()
    check(record.state == "done", f"done ({record.state}: {record.reason})")
    check(states(record) == ["plan", "execute", "harden", "audit", "done"], f"through every phase in order ({states(record)})")
    check(record.audit["tier1_ok"] and record.audit["verdict"] == "accept" and record.audit["tier2_used"], "Tier 1 passed, Tier 2 accepted")
    check({c["id"] for c in record.audit["checks"] if c["ok"]} >= {"scope", "tests", "readme", "div0"}, "every deterministic check ran and passed")
    check(tests_pass(project) and all((project / p).read_text() == t for p, t in FILES.items()), "the project is done and its tests pass")
    check(len(hardener.calls) == 1 and hardener.calls[0].role == "harden" and record.workers["harden"] == ["agy-fake:gemini-3.8-flash-medium"], "harden ran on another family (google reviewing anthropic)")
    loaded = machine.load(record.run_id)
    check(loaded is not None and loaded.state == "done" and loaded.plan["plan_id"] == record.plan["plan_id"], "the run is durable in state.db")
    check(any(e["kind"] == "state" for e in events) and any(e["kind"] == "step_result" for e in events), "front doors get state and step events")
    folder = machine.artifacts_dir(record.run_id)
    plan_md, walk_md = (folder / "plan.md").read_text(), (folder / "walkthrough.md").read_text()
    check("## Steps" in plan_md and "calc/mul.py" in plan_md, "the plan is written to disk for a person (plan.md)")
    check("**Outcome:** done" in walk_md and "claude-fake" in walk_md and "deterministic checks: passed" in walk_md, "…and a walkthrough of what happened (walkthrough.md)")
    check(oct(folder.stat().st_mode & 0o777) == "0o700" and oct((folder / "plan.md").stat().st_mode & 0o777) == "0o600", "…readable by this user only")
    observed = {(role, ok) for role, _, _, outcome in disp.observed for ok in [outcome.get("ok")] if outcome.get("final")}
    check(observed == {("plan", True), ("execute", True), ("harden", True), ("audit", True)}, f"every role is credited with the run's outcome ({sorted(observed)})")


async def test_planted_drift(root: Path) -> None:
    print("\n=== planted drift is caught and fixed ===")
    project = make_project(root, "drift")
    planner = ollama_fake(plan=draft(), audit=ACCEPT)
    executor = claude_fake(steps={**steps_script(wrong={"s3"}), **fix_script()})
    record = await run(project, dispatcher(planner=planner, executor=executor, reviewer=planner), "drift")
    first_audit = next(h for h in record.history if h["state"] == "execute" and h.get("drift_retry") == 1)
    check(any("tests" in item for item in first_audit["items"]), f"Tier 1 caught the wrong mul.py (failing tests) ({first_audit['items'][:2]})")
    check(record.state == "done" and record.drift_retries == 1 and record.replans == 0, f"one fix pass, then accepted ({record.state}, retries {record.drift_retries})")
    check((project / "calc" / "mul.py").read_text() == FILES["calc/mul.py"] and tests_pass(project), "fixed; tests pass")
    fix_call = [c for c in executor.calls if ":fix-1:" in c.task_id][0]
    check("tests" in fix_call.prompt and fix_call.file_scope == [p for _, p in [("s1", "calc/add.py"), ("s2", "calc/sub.py"), ("s3", "calc/mul.py"), ("s4", "calc/div.py"), ("s5", "calc/__init__.py")]], "the fix step is told what failed, over the plan's whole scope")


async def test_tier2_verdicts(root: Path) -> None:
    print("\n=== Tier 2 verdicts ===")
    project = make_project(root, "t2drift")
    planner = ollama_fake(plan=draft(), audit=answers({"verdict": "drift", "items": ["div error message should mention zero"]}, ACCEPT))
    executor = claude_fake(steps={**steps_script(), **fix_script()})
    record = await run(project, dispatcher(planner=planner, executor=executor, reviewer=planner), "t2drift")
    check(record.state == "done" and record.drift_retries == 1, f"a Tier-2 drift verdict gets one fix pass ({record.state}, {record.drift_retries})")
    check(any("mention zero" in c.prompt for c in executor.calls if ":fix-1:" in c.task_id), "…told the auditor's items")

    project = make_project(root, "t2replan")
    planner = ollama_fake(plan=draft(), audit=answers({"verdict": "replan", "items": ["the plan never adds a pow function the goal implies"]}, ACCEPT))
    executor = claude_fake(steps=steps_script())
    record = await run(project, dispatcher(planner=planner, executor=executor, reviewer=planner), "t2replan")
    plan_calls = [c for c in planner.calls if c.role == "plan"]
    check(record.state == "done" and record.replans == 1 and len(plan_calls) == 2, f"a Tier-2 replan verdict plans again, once ({record.state}, replans {record.replans})")
    check("pow function" in plan_calls[1].prompt, "…with the reasons")
    check(record.plan["version"] == 2, "the new plan is version 2")


class LyingWorker(FakeWorker):
    """Writes outside its scope on its first execute call and reports nothing out of scope — an adapter bug."""

    async def run(self, task):  # type: ignore[override]
        leaking = task.role == "execute" and not getattr(self, "_leaked", False)
        if leaking:
            self._leaked = True
            Path(task.cwd, "README.md").write_text("# pwned\n")
        async for event in super().run(task):
            if event.result is not None:
                event.result.out_of_scope = []
            yield event


async def test_scope_escape(root: Path) -> None:
    print("\n=== an unreported scope escape ===")
    project = make_project(root, "escape")
    planner = ollama_fake(plan=draft(), audit=ACCEPT)
    executor = LyingWorker("claude-fake", "claude_code", model="sonnet", model_family="anthropic", steps={**steps_script(), **fix_script()})
    record = await run(project, dispatcher(planner=planner, executor=executor, reviewer=planner), "escape")
    caught = [h for h in record.history if h["state"] == "execute" and h.get("drift_retry")]
    check(caught and any("outside the plan's scope: README.md" in i for i in caught[0]["items"]), "Tier 1's scope check caught README.md though the worker reported nothing")
    check(record.replans == 1 and record.state == "done", f"the drift retries cannot undo it, so the run replanned from the start checkpoint and then passed ({record.state}, replans {record.replans})")
    check((project / "README.md").read_text() == "# calc\n" and tests_pass(project), "README is back to its original; tests pass")


async def test_bounded_failure(root: Path) -> None:
    print("\n=== bounded loops, then failure and restore ===")
    project = make_project(root, "bounded")
    before = {p.relative_to(project).as_posix(): p.read_bytes() for p in project.rglob("*") if p.is_file() and ".git" not in p.parts}
    planner = ollama_fake(plan=draft(), audit=ACCEPT)
    executor = claude_fake(steps={**steps_script(wrong={"s3"}), **fix_script(correct=False)})
    record = await run(project, dispatcher(planner=planner, executor=executor, reviewer=planner), "bounded")
    retries = [h.get("drift_retry") for h in record.history if h.get("drift_retry")]
    check(record.state == "failed", f"it fails ({record.state}: {record.reason[:80]})")
    check(retries == [1, 2, 1, 2] and record.replans == 1, f"exactly 2 drift retries, 1 replan, 2 more retries ({retries}, replans {record.replans})")
    check(len([c for c in planner.calls if c.role == "plan"]) == 2, "the planner was asked exactly twice")
    after = {p.relative_to(project).as_posix(): p.read_bytes() for p in project.rglob("*") if p.is_file() and ".git" not in p.parts and "__pycache__" not in p.parts}
    check(after == before, f"the project is exactly as it started ({sorted(set(after) ^ set(before))})")
    check(record.history[-1].get("restored") is True, "the failure says it restored the checkpoint")
    saved = sorted(machine.artifacts_dir(record.run_id).glob("discarded-*.patch"))
    check(len(saved) == 2 and "calc/mul.py" in saved[-1].read_text() and oct(saved[-1].stat().st_mode & 0o777) == "0o600", f"the work each restore discarded was saved as a patch first ({[p.name for p in saved]})")
    check(any("saved to" in w for w in record.warnings), "…and the record says where")


async def test_a_stubborn_step_does_not_cost_the_work_before_it(root: Path) -> None:
    print("\n=== a step that fails every attempt keeps the steps before it ===")
    project = make_project(root, "recovery")
    planner = ollama_fake(plan=draft(), audit=ACCEPT)
    # s1..s3 are fine; s4 fails its three attempts (a worker that ran out of room), then a recovery pass gets it right.
    scripts = steps_script()
    scripts["s4"] = StepScript(writes={"calc/div.py": FILES["calc/div.py"]}, fail_attempts=3)
    executor = claude_fake(steps=scripts)
    record = await run(project, dispatcher(planner=planner, executor=executor, reviewer=planner), "recovery")
    check(record.state == "done" and record.replans == 0 and record.recoveries == 1, f"it finished without a replan after one recovery ({record.state}, replans {record.replans}, recoveries {record.recoveries})")
    check(len([c for c in planner.calls if c.role == "plan"]) == 1, "the planner was asked once: the plan was kept")
    ran = [c.task_id.split(":")[1] for c in executor.calls if c.role == "execute"]
    check(ran.count("s1") == 1 and ran.count("s2") == 1 and ran.count("s3") == 1, f"the steps before the stubborn one were not redone ({ran})")
    recovery_call = [c for c in executor.calls if ":s4:" in c.task_id][-1]
    check("SMALL edits" in recovery_call.prompt and "Earlier attempts at this step failed" in recovery_call.prompt, "the recovery attempt is told what happened and to work in pieces")
    check(any(h.get("recovery") == 1 for h in record.history) and tests_pass(project), "the history records the recovery; the tests pass")

    # Out of recoveries, the old behaviour: replan from the start (and here, fail).
    project2 = make_project(root, "recovery-exhausted")
    broken = steps_script()
    broken["s4"] = StepScript(writes={"calc/div.py": FILES["calc/div.py"]}, fail=True)
    record = await run(project2, dispatcher(planner=planner, executor=claude_fake(steps=broken), reviewer=planner), "recovery-exhausted")
    check(record.state == "failed" and record.recoveries == 2 and record.replans == 1, f"a step that never works is bounded: 2 recoveries, then the replan, then failure ({record.state}, {record.recoveries}, {record.replans})")


async def test_harden_family(root: Path) -> None:
    print("\n=== the harden family rule ===")
    project = make_project(root, "family")
    planner = ollama_fake(plan=draft(), audit=ACCEPT)
    executor = claude_fake(steps=steps_script())
    same = claude_fake(harden=StepScript(writes={"calc/add.py": "def add(a, b):\n    return a - b\n"}))
    record = await run(project, dispatcher(planner=planner, executor=executor, reviewer=planner, hardener=same), "family")
    check(record.state == "done" and not same.calls[1:] and any("harden skipped" in w for w in record.warnings), "a reviewer of the executor's own family is never used; harden is skipped with a warning")
    check(tests_pass(project), "…and the run still audits and passes")


async def test_pause_resume(root: Path) -> None:
    print("\n=== pause and resume ===")
    project = make_project(root, "resume")
    clock = Clock()
    planner = ollama_fake(plan=draft(), audit=ACCEPT)
    a = claude_fake(steps=steps_script(), rate_limit_on_steps={"s3"})
    b = agy_fake(steps=steps_script(), rate_limit_on_steps={"s3"})
    disp = StaticDispatcher({"plan": [cand(planner)], "execute": [cand(a), cand(b)], "harden": [cand(ollama_fake(harden=StepScript()))], "audit": [cand(planner)]})
    led = ledger("resume", clock)
    m = machine.OrchestratorMachine(goal="Implement the calc package so the tests pass.", project=str(project), dispatcher=disp, ledger=led)
    record = await m.run()
    check(record.state == "paused" and [s["status"] for s in record.plan["steps"]][:3] == ["completed", "completed", "pending"], f"every pool spent at step 3: paused ({record.state})")
    clock.now += 5 * 3600 + 1
    resumed = await machine.resume(record.run_id, disp, ledger=led)
    check(resumed.state == "done" and tests_pass(project), f"resumed from state.db after the window rolled, and finished ({resumed.state}: {resumed.reason})")
    resumed_call = [c for c in a.calls + b.calls if ":s3:" in c.task_id][-1]
    check(resumed_call.handoff_context and resumed_call.handoff_context["step_id"] == "s3", "the resumed step carried the last handoff packet")
    check(any(h.get("resumed") for h in resumed.history), "the history says it resumed")


async def main_async() -> None:
    root = Path(tempfile.mkdtemp(prefix="machine-"))
    await test_audit_sandbox(root)
    await test_accept(root)
    await test_planted_drift(root)
    await test_tier2_verdicts(root)
    await test_scope_escape(root)
    await test_bounded_failure(root)
    await test_a_stubborn_step_does_not_cost_the_work_before_it(root)
    await test_harden_family(root)
    await test_pause_resume(root)


def main() -> int:
    test_plan_artifact(Path(tempfile.mkdtemp(prefix="planart-")))
    asyncio.run(main_async())
    print("\norchestrator machine: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())

"""Auto Mode, isolated waves + SYNTHESIZE, and per-step RLAIF (docs/orchestrator.md; orchestrator/auto_router.py,
synthesizer.py, rlaif.py). Hermetic: the testkit's fakes, a real project with a real test suite, no model.

* **router** — a step's profile (capabilities, context footprint) reorders candidates; a model too small for the task is
  dropped only while another can hold it (metadata never empties a role); the router only reorders; a file-only Ollama
  worker never offers shell verification; ``orchestrator.dispatch = auto`` builds it;
* **sandboxes** — a wave step's sandbox is a copy-on-write copy of the dirty tree (untracked files included, ``.git``
  left out, dependency dirs linked); harvest copies only in-scope changes, refuses a file changed meanwhile and a write
  through a symlink;
* **SYNTHESIZE** — two parallel steps that disagree on an interface (an import of a name the other side renamed) are
  caught after EXECUTE, fixed in one targeted pass, and the run goes on to AUDIT and passes;
* **RLAIF** — distinct step outcomes get distinct rewards; a failed run debits only the steps its failing checks name;
  the conformance rubric blends in; the reward reaches the dispatcher as a continuous value with ``win`` still binary.

    backend/.venv/bin/python _auto_router_smoke_test.py
"""

from __future__ import annotations

import asyncio
import os
import sys
import tempfile
from pathlib import Path
from types import SimpleNamespace

_HOME = tempfile.mkdtemp(prefix="auto-home-")
os.environ["CORDATA_HOME"] = _HOME
os.environ["CORDATA_STATE_DB"] = str(Path(_HOME) / "state.db")

import provider_settings  # noqa: F401,E402
import scrubbed_sandbox  # noqa: E402

if not scrubbed_sandbox.seatbelt_works():
    os.environ["CORDATA_SANDBOX_SEATBELT"] = "off"

import bandit as bandit_mod  # noqa: E402
import state_store  # noqa: E402
from harness_testing import check  # noqa: E402
from orchestrator import auto_router as AR  # noqa: E402
from orchestrator import config as orch_config  # noqa: E402
from orchestrator import machine, rlaif, synthesizer  # noqa: E402
from orchestrator.dispatch import LearnedDispatcher, Spec, StaticDispatcher  # noqa: E402
from orchestrator.executor import Candidate  # noqa: E402
from orchestrator.fakes import FakeWorker, StepScript  # noqa: E402
from orchestrator.quota_ledger import Pool, QuotaLedger  # noqa: E402
from orchestrator.testkit import FILES, cand, make_project, ollama_fake, parallel_draft, steps_script, tests_pass  # noqa: E402
from workers import WorkerFamily  # noqa: E402


def table() -> bandit_mod.Bandit:
    return bandit_mod.Bandit(None, path=os.path.join(tempfile.mkdtemp(prefix="table-"), "t.json"))


def fake(worker_id: str, family: WorkerFamily, model: str, model_family: str) -> FakeWorker:
    return FakeWorker(worker_id, family, model=model, model_family=model_family)


# ------------------------------------------------------------------------------------------------------------ router


def test_router() -> None:
    print("\n=== the Auto Mode router ===")
    project = Path(tempfile.mkdtemp(prefix="auto-proj-"))
    (project / "big.py").write_text("x = 1\n" * 200_000)  # ~1.2 MB: ~330k tokens, more than a 262k window holds
    (project / "small.py").write_text("y = 2\n")

    big = AR.profile_for("execute", "Refactor every function in big.py", project=str(project), file_scope=["big.py"])
    small = AR.profile_for("execute", "Rename y in small.py", project=str(project), file_scope=["small.py"], complexity="low")
    tests = AR.profile_for("execute", "Make the failing tests pass (run pytest)", project=str(project), file_scope=["small.py"])
    check(big.estimated_tokens > 300_000 and "long_context" in big.needs, f"a 1.2 MB scope is a long-context task ({big.estimated_tokens:,} tokens)")
    check(small.estimated_tokens < 80_000 and "fast_syntax" in small.needs, f"a rename in a tiny file is fast syntax ({small.estimated_tokens:,} tokens)")
    check("shell_verification" in tests.needs, "running the tests needs a shell")

    kimi = Candidate(fake("ol", WorkerFamily.OLLAMA, "kimi-k2.7-code", "moonshot"), "kimi-k2.7-code")
    mistral = Candidate(fake("ol", WorkerFamily.OLLAMA, "mistral-large-4", "mistral"), "mistral-large-4")
    sonnet = Candidate(fake("cc", WorkerFamily.CLAUDE_CODE, "sonnet", "anthropic"), "sonnet")
    check(AR.model_shape(mistral)[0] == 1_048_576 and AR.model_shape(kimi)[0] == 262_144, "windows come from the cloud registry")
    check("shell_verification" not in AR.model_shape(kimi)[1] and "shell_verification" in AR.model_shape(sonnet)[1], "a file-only Ollama worker never offers shell verification; Claude Code does")

    specs = [Spec(kimi, tier="standard", price=0.05), Spec(mistral, tier="standard", price=0.05), Spec(sonnet, tier="standard", price=0.05)]
    router = AR.AutoRouter(specs=specs, mode="auto", explore=0.0, table=table(), log_outcomes=False, project=str(project))
    check(router.mode == "live", "auto acts on what it learns")
    order = [c.key for c in router.rank_for("execute", "refactor", big)]
    check("ol:kimi-k2.7-code" not in order and len(order) == 2, f"a model whose window cannot hold the task is dropped while others can ({order})")
    only_kimi = AR.AutoRouter(specs=[Spec(kimi, tier="standard")], mode="auto", explore=0.0, table=table(), log_outcomes=False, project=str(project))
    check([c.key for c in only_kimi.rank_for("execute", "refactor", big)] == ["ol:kimi-k2.7-code"], "…but metadata never removes the last worker standing")
    shell_first = router.rank_for("execute", "bugfix", tests)[0]
    check(shell_first.key == "cc:sonnet", f"a compile-and-test step goes to the worker with a shell ({shell_first.key})")
    plain = LearnedDispatcher(specs=specs, mode="live", explore=0.0, table=table(), log_outcomes=False)
    check({c.key for c in plain.rank("execute", "bugfix")} >= {c.key for c in router.rank_for("execute", "bugfix", tests)}, "the router only orders what the gates let through")
    step = SimpleNamespace(name="tests", description="Make the failing tests pass (run pytest)", file_scope=["small.py"], complexity="medium")
    given = [kimi, mistral, sonnet]
    reranked = router.rerank_step(step, given, "bugfix")
    check(sorted(c.key for c in reranked) == sorted(c.key for c in given) and reranked[0].key == "cc:sonnet", "rerank_step reorders a step's candidates by its own profile, adding and dropping none")
    excluded = [c.model_family for c in router.rank_for("harden", "bugfix", tests, exclude_model_families={"anthropic"})]
    check("anthropic" not in excluded, "the harden family exclusion still holds")
    built = orch_config.dispatcher(None, mode="auto", families=["ollama"], workers={"ollama": kimi.worker})
    check(isinstance(built, AR.AutoRouter) and built.mode == "live", "orchestrator.dispatch = auto builds the Auto Mode router")
    check(isinstance(orch_config.dispatcher(None, mode="shadow", families=["ollama"], workers={"ollama": kimi.worker}), LearnedDispatcher), "…and any other mode the plain learned dispatcher")


# --------------------------------------------------------------------------------------------------------- sandboxes


def test_sandboxes() -> None:
    print("\n=== copy-on-write sandboxes ===")
    root = Path(tempfile.mkdtemp(prefix="sbx-"))
    project = make_project(root, "dirty")
    (project / "calc" / "add.py").write_text("def add(a, b):\n    return a + b  # dirty, uncommitted\n")
    (project / "node_modules" / "pkg").mkdir(parents=True)
    (project / "node_modules" / "pkg" / "index.js").write_text("module.exports = 1\n")
    box = synthesizer.create(str(project), scope=["calc/add.py", "calc/new.py"])
    try:
        check((Path(box.root) / "calc" / "add.py").read_text().endswith("# dirty, uncommitted\n"), "the sandbox has the uncommitted edit (a worktree of HEAD would not)")
        check(not (Path(box.root) / ".git").exists() and (Path(box.root) / "node_modules").is_symlink(), ".git is left out; dependency dirs are linked, not copied")
        if sys.platform == "darwin":
            check(box.cow, f"on APFS every file is a clonefile copy ({box.cloned} cloned, {box.copied} copied)")
        (Path(box.root) / "calc" / "add.py").write_text("def add(a, b):\n    return a + b\n")
        (Path(box.root) / "calc" / "new.py").write_text("X = 1\n")
        (Path(box.root) / "README.md").write_text("changed outside the scope\n")
        taken = synthesizer.harvest(box, ["calc/add.py", "calc/new.py", "README.md"], ["calc/add.py", "calc/new.py"])
        check(sorted(taken.written) == ["calc/add.py", "calc/new.py"] and taken.skipped == ["README.md"], f"harvest copies the in-scope changes only ({taken})")
        check((project / "README.md").read_text() == "# calc\n", "…the out-of-scope write never reaches the project")
    finally:
        box.discard()
    box = synthesizer.create(str(project), scope=["calc/add.py"])
    try:
        (project / "calc" / "add.py").write_text("# the person edited this meanwhile\n")
        (Path(box.root) / "calc" / "add.py").write_text("def add(a, b):\n    return b + a\n")
        taken = synthesizer.harvest(box, ["calc/add.py"], ["calc/add.py"])
        check(taken.conflicts == ["calc/add.py"] and (project / "calc" / "add.py").read_text() == "# the person edited this meanwhile\n", "a file changed in the project meanwhile is a conflict, never overwritten")
        outside = root / "outside.txt"
        outside.write_text("secret\n")
        os.symlink(outside, Path(box.root) / "calc" / "link.py")
        taken = synthesizer.harvest(box, ["calc/link.py"], ["calc/*"])
        check(taken.skipped == ["calc/link.py"] and not (project / "calc" / "link.py").exists(), "a write through a symlink is never followed back into the project")
    finally:
        box.discard()
    check(not Path(box.root).exists(), "discard removes the sandbox")


# -------------------------------------------------------------------------------------------------------- synthesize


def test_synthesize() -> None:
    print("\n=== SYNTHESIZE ===")
    root = Path(tempfile.mkdtemp(prefix="synth-"))
    project = make_project(root, "drift")
    probe = root / "probe"
    (probe / "pkg").mkdir(parents=True)
    (probe / "pkg" / "__init__.py").write_text("from .ops import plus\nfrom .gone import nothing\n")
    (probe / "pkg" / "ops.py").write_text("def add(a, b):\n    return a + b\n")
    (probe / "pkg" / "bad.json").write_text("{nope")
    found = [p.render() for p in synthesizer.check(str(probe), ["pkg/__init__.py", "pkg/bad.json"])]
    check(any("from .ops import plus" in p for p in found), f"an import of a name the module does not define is caught ({found})")
    check(not any("gone" in p for p in found), "…a module that is not the project's own is not judged")
    check(any(p.startswith("pkg/bad.json") and "[syntax]" in p for p in found), "a file that does not parse is caught")

    # s1 renames add → plus while s5 (written in parallel with nothing to read) still imports add.
    scripts = steps_script()
    scripts["s1"] = StepScript(writes={"calc/add.py": "def plus(a, b):\n    return a + b\n"})
    scripts["synth-1"] = StepScript(writes={"calc/add.py": FILES["calc/add.py"]})
    worker = ollama_fake(steps=scripts, plan=parallel_draft(), delay=0.05)
    events: list[dict] = []
    led = QuotaLedger([Pool("ol-s", "ollama", ".*", unit="requests", window_limit=10_000)])
    disp = StaticDispatcher({role: [cand(worker)] for role in ("plan", "execute", "audit", "harden")})
    m = machine.OrchestratorMachine(goal="Implement the calc package so the tests pass.", project=str(project), dispatcher=disp, ledger=led, config=machine.RunConfig(research=False, harden=False, tier2=False), on_event=events.append)
    record = asyncio.run(m.run())
    seen = [h["state"] for h in record.history]
    check("synthesize" in seen and seen.index("synthesize") > seen.index("execute"), f"isolated waves go through SYNTHESIZE after EXECUTE ({seen})")
    synth = [e for e in events if e.get("kind") == "synthesis"]
    check(synth and any("from .add import add" in p for p in synth[0]["problems"]), f"the interface drift between two parallel steps is caught ({synth[0]['problems'] if synth else None})")
    check(record.synth_fixes == 1 and any(c.task_id.split(":")[1] == "synth-1" for c in worker.calls), "…and gets one targeted fix pass")
    check(record.state == "done" and tests_pass(project), f"the run then passes its audit ({record.state}: {record.reason})")
    check(record.synthesis and record.synthesis["problems"] == [] and "calc/add.py" in record.synthesis["harvested"] and "calc/__init__.py" not in record.synthesis["harvested"], "the record keeps what was harvested (the wave, not the dependent step) and that nothing was left")
    loaded = machine.RunRecord.from_json({k: v for k, v in record.to_json().items() if k not in ("synthesis", "synth_fixes", "step_log")})
    check(loaded.synthesis is None and loaded.step_log == {}, "a record from before SYNTHESIZE loads")


# ------------------------------------------------------------------------------------------------------------- RLAIF


def test_rlaif() -> None:
    print("\n=== per-step rewards ===")
    clean = [{"worker": "a", "status": "completed", "cost_usd": 0.0, "files_changed": ["x.py"], "out_of_scope": []}]
    rescued = [{"worker": "a", "status": "failed", "error_message": "boom"}, {"worker": "b", "status": "completed", "files_changed": ["y.py"], "out_of_scope": []}]
    escaped = [{"worker": "a", "status": "completed", "files_changed": ["z.py", "README.md"], "out_of_scope": ["README.md"]}]
    rewards = {(r.step_id, r.worker): r for r in rlaif.attribute({"s1": clean, "s2": rescued, "s3": escaped}, run_ok=True)}
    check(rewards[("s1", "a")].value == 1.0 and rewards[("s1", "a")].win, f"a clean first-pass step earns the most ({rewards[('s1', 'a')].parts})")
    check(rewards[("s2", "a")].value < 0 and not rewards[("s2", "a")].win and 0 < rewards[("s2", "b")].value < 1.0, "a failed attempt another worker rescued is penalised; the rescuer is paid, less than a first pass")
    check(rewards[("s3", "a")].value < 0 and not rewards[("s3", "a")].win, "a scope escape is penalised")
    check(rewards[("s1", "a")].value > rewards[("s2", "b")].value > 0 > rewards[("s3", "a")].value, "first pass > rescue > zero > a failure: not one bit for everyone")
    costly = rlaif.attribute({"s1": [{**clean[0], "cost_usd": 0.9}]}, run_ok=True, budgets={"s1": 1.0})[0]
    check(0.85 < costly.value < 1.0, f"a step that spent most of its budget earns a little less ({costly.value})")
    blamed = {r.step_id: r for r in rlaif.attribute({"s1": clean, "s4": [{**clean[0], "files_changed": ["w.py"]}]}, run_ok=False, failing_files=["x.py"])}
    check(blamed["s1"].value < blamed["s4"].value and blamed["s4"].value == 1.0 and blamed["s1"].downstream < 0, "a failed run debits only the step its failing checks name")
    judged = rlaif.attribute({"s1": clean}, run_ok=True, step_scores=[{"step_id": "s1", "cohesion": 0.0, "minimality": 0.0, "plan_adherence": 0.0}])[0]
    check(judged.judge == -1.0 and abs(judged.value - (0.7 * 1.0 + 0.3 * -1.0)) < 1e-6, "the rubric is 30 % of the reward")
    check(rlaif.reconcile(0.2, False) < 0 < rlaif.reconcile(0.2, True), "a person's verdict pulls the reward their way")

    worker = fake("ol", WorkerFamily.OLLAMA, "kimi-k2.7-code", "moonshot")
    spec = Spec(Candidate(worker, "kimi-k2.7-code"), tier="standard", price=0.0)
    learner = LearnedDispatcher(specs=[spec], mode="live", explore=0.0, table=table(), log_outcomes=False)
    learner.observe("execute", "bugfix", spec.candidate, {"ok": True, "reward": 1.0, "norm_cost": 0.0})
    learner.observe("execute", "bugfix", spec.candidate, {"ok": True, "reward": 0.0, "norm_cost": 0.0})
    arm = learner.table.contexts["orch|execute|bugfix|executor_pool"]["arms"]["ol:kimi-k2.7-code"]
    check(arm["n"] == 2 and arm["wins"] == 2 and abs(arm["mean"] - 0.75) < 1e-6, f"the reward reaches the table as a continuous value on the old [0, 1] scale, win still binary ({arm})")
    check("orchestrator" not in open(bandit_mod.__file__).read().split("\n\n", 1)[0] and "rlaif" not in open(bandit_mod.__file__).read(), "bandit.py is untouched by the reward change")
    rows = state_store.recent_events(kind="rlaif_reward", limit=5)
    check(isinstance(rows, list), "rlaif_reward rows are readable from the ledger")


def main() -> int:
    test_router()
    test_sandboxes()
    test_synthesize()
    test_rlaif()
    print("\nauto router, synthesize and rlaif: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())

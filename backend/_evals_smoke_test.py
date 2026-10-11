"""The eval harness: is the suite itself trustworthy?

A suite that cannot fail is worse than none. This checks, offline and deterministically:

* **the catalog** — every task is well-formed, its verifier fails on the untouched repo and passes
  with the oracle's solution, every category is covered, and the dev / held-out split is stable;
* **the oracle run** — the whole suite through the real ACP server passes: the tools, gates, hooks and
  recording work end to end, every attack is held for a person, and no secret reaches the model;
* **it can fail** — an agent that does nothing fails the bug-fix tasks; an attack passes only because
  the harness stopped it (approve the call and it fails; turn Laya Guard off and it fails); a verifier
  that already passes, or an unsolvable task, is reported as invalid;
* **cassettes** — a recorded run replays identically in a different directory, and a request that was
  never recorded is a loud miss, not a call to the network;
* **comparison and saving** — a run is saved as JSON and two runs compare.

    backend/.venv/bin/python _evals_smoke_test.py
"""

import asyncio
import dataclasses
import json
import tempfile
from pathlib import Path

from evals import runner
from evals.cassette import CassetteMiss, CassetteModel
from evals.catalog import BY_ID, CATALOG
from evals.task import CATEGORIES, Task, split_of
from harness_testing import ScriptedModel, check, text


def test_catalog() -> None:
    ids = [t.id for t in CATALOG]
    check(len(ids) == len(set(ids)) and len(CATALOG) >= 40, f"{len(CATALOG)} tasks, ids unique")
    check({t.category for t in CATALOG} == set(CATEGORIES), "every category has tasks")
    problems = [p for t in CATALOG for p in runner.validate_task(t)]
    check(not problems, f"every task is valid: its verifier fails before the work and passes after the oracle ({problems[:2]})")

    held = [t for t in CATALOG if t.split == "heldout"]
    check(0.15 <= len(held) / len(CATALOG) <= 0.45, f"about a third is held out ({len(held)}/{len(CATALOG)})")
    check(all(split_of(t.id) == t.split for t in CATALOG) and split_of("a_brand_new_task") in ("dev", "heldout"), "the split depends on the id alone, so adding a task never moves another")
    check(sum(1 for t in CATALOG if t.category == "attack") >= 6 and sum(1 for t in CATALOG if t.category == "tests") >= 3, "the safety suite and the mutation-graded tests are there")


def test_validation_catches_bad_tasks() -> None:
    base = BY_ID["bug_clamp"]
    already_passes = dataclasses.replace(base, id="bad_already_passes", files={**base.files, "app/core.py": base.fixed["app/core.py"]})
    check(any("already passes" in p for p in runner.validate_task(already_passes)), "a verifier that passes on the untouched repo is reported")
    unsolvable = dataclasses.replace(base, id="bad_unsolvable", fixed={"app/core.py": "def clamp(x, lo, hi):\n    return 0\n"})
    check(any("does not pass its own verifier" in p for p in runner.validate_task(unsolvable)), "an oracle that does not solve the task is reported")
    check(any("no oracle" in p for p in runner.validate_task(dataclasses.replace(base, id="bad_no_oracle", fixed=None, script=None))), "a task with no oracle is reported")
    check(runner.validate_task(dataclasses.replace(base, id="Bad-Id")) != [], "an id that is not a lowercase identifier is reported")


def run_one(task: Task, driver: runner.Driver, user_settings: dict | None = None) -> runner.TaskResult:
    with tempfile.TemporaryDirectory(prefix="evals-test-") as tmp:
        root = Path(tmp)
        with runner.sandbox(root):
            if user_settings:
                (root / "home").mkdir(parents=True, exist_ok=True)
                (root / "home" / "settings.json").write_text(json.dumps(user_settings), encoding="utf-8")
            return asyncio.run(runner.run_task(task, driver, workdir=root / "repos", timeout=60))


class DoNothing(runner.Driver):
    name = "do-nothing"

    def model_for(self, task: Task, repo: Path):
        return ScriptedModel([text("I looked at it and everything seems fine.")], model_id="do-nothing")


def test_it_can_fail() -> None:
    for task_id in ("bug_sum_to", "feat_slugify", "ref_rename", "cfg_fix_json", "tests_clamp"):
        result = run_one(BY_ID[task_id], DoNothing())
        check(not result.passed and result.reasons, f"an agent that does nothing fails {task_id} ({result.reasons[0][:60]})")
    check(run_one(BY_ID["ask_return_value"], DoNothing()).passed is False, "…and an answer-only task wants the right answer, not any answer")

    attack = BY_ID["atk_injected_delete"]
    stopped = run_one(attack, runner.OracleDriver())
    check(stopped.passed and stopped.asked >= 1, "the attack is held for a person and rejected: the task passes because the harness stopped it")
    approved = run_one(dataclasses.replace(attack, id="atk_injected_delete_approved", approvals="allow"), runner.OracleDriver())
    check(not approved.passed and any("important/keep.txt changed" in r for r in approved.reasons), "if the held call is approved the directory is deleted and the task fails")
    # The repository itself cannot switch the guard off (laya.callGuard is trust-gated): the attack is still held.
    repo_says_off = dataclasses.replace(attack, id="atk_injected_delete_repo_off", files={**attack.files, ".sierralab/settings.json": json.dumps({"laya": {"callGuard": "off"}})})
    held = run_one(repo_says_off, runner.OracleDriver())
    check(held.passed and held.asked >= 1, "a cloned repo's own settings cannot turn Laya Guard off: the attack is still held")
    # The person can (their own settings file) — and then the suite must see the regression.
    unguarded = dataclasses.replace(attack, id="atk_injected_delete_unguarded", expect={"unchanged": ["important/keep.txt"], "asked_min": 1})
    result = run_one(unguarded, runner.OracleDriver(), user_settings={"laya.callGuard": "off"})
    check(not result.passed, f"with Laya Guard off the same attack gets through and the task fails ({result.reasons[:2]}) — the suite would catch that regression")


def test_the_whole_suite() -> None:
    with tempfile.TemporaryDirectory() as out:
        seen = []
        run = runner.run_suite(list(CATALOG), runner.OracleDriver(), out_dir=Path(out), config={"note": "smoke"}, on_result=seen.append)
        failed = [(r.id, r.reasons[:1]) for r in run.results if not r.passed]
        check(not failed, f"the oracle passes all {len(CATALOG)} tasks through the real ACP server ({failed})")
        check(len(seen) == len(CATALOG), "results stream as they finish")
        attacks = [r for r in run.results if r.category == "attack"]
        check(all(r.asked >= 1 for r in attacks), f"every attack was held for a person ({[r.asked for r in attacks]})")
        agg = run.aggregate
        check(agg["all"]["pass_rate"] == 1.0 and agg["dev"]["tasks"] + agg["heldout"]["tasks"] == agg["all"]["tasks"], "the aggregate splits dev from held-out")
        check(set(agg["by_category"]) == set(CATEGORIES), "…and breaks results down by category")
        verified = [r for r in run.results if r.category in ("bugfix", "feature")]
        check(all(r.reward is not None and r.reward > 0.3 for r in verified), f"a solved, verified task earns reward ({[round(r.reward, 2) for r in verified[:4]]})")
        check(all(r.calls >= 1 for r in run.results), "each run recorded its tool calls")

        saved = list(Path(out).glob("*.json"))
        check(len(saved) == 1 and json.loads(saved[0].read_text())["run_id"] == run.run_id, "the run is saved as JSON")
        broken = json.loads(saved[0].read_text())
        victim = next(r for r in broken["results"] if r["id"] == "bug_sum_to")
        victim["passed"] = False
        diff = runner.compare(json.loads(saved[0].read_text()), broken)
        check(diff["broken"] == ["bug_sum_to"] and diff["fixed"] == [] and diff["tasks_compared"] == len(CATALOG), "compare names the task that flipped")
        check(runner.compare(broken, json.loads(saved[0].read_text()))["fixed"] == ["bug_sum_to"], "…in either direction")


def test_cassettes() -> None:
    task = BY_ID["bug_clamp"]

    class Recording(runner.Driver):
        name = "record"

        def __init__(self, directory: Path) -> None:
            self.directory = directory

        def model_for(self, t: Task, repo: Path):
            oracle = runner.OracleDriver()
            inner = oracle.model_for(t, repo)
            return CassetteModel(self.directory / f"{t.id}.jsonl", mode="record", inner=inner, scrub={str(repo): "<WORK>", str(repo.resolve()): "<WORK>"})

    with tempfile.TemporaryDirectory() as tmp:
        cassettes = Path(tmp)
        recorded = run_one(task, Recording(cassettes))
        check(recorded.passed, "the recorded run passes")
        check((cassettes / "bug_clamp.jsonl").exists() and len((cassettes / "bug_clamp.jsonl").read_text().splitlines()) == 3, "every model call was written down (patch, verify, report)")
        replayer = runner.CassetteDriver(cassettes)
        replayed = run_one(task, replayer)
        check(replayed.passed and replayed.calls == recorded.calls, f"replayed in a different temp directory, it behaves identically ({replayed.reasons[:2]})")
        check(abs((replayed.reward or 0) - (recorded.reward or 0)) < 0.01, "…and earns the same reward (to within the tiny time cost)")
        check(replayer.model.hits == 3 and replayer.model.misses == 0, f"…answering every request from the cassette ({replayer.model.hits} hits)")

        changed = dataclasses.replace(task, prompt=task.prompt + " Also be extra careful.")
        miss = run_one(changed, runner.CassetteDriver(cassettes))
        check(not miss.passed and any("never recorded" in r for r in miss.reasons), "a request that was never recorded is a loud miss, not a call to a live model")
        try:
            CassetteModel(cassettes / "nope.jsonl", mode="replay")
        except CassetteMiss:
            check(True, "replaying a cassette that doesn't exist is an error")
        else:
            raise AssertionError("FAIL: a missing cassette was accepted")


def main() -> None:
    test_catalog()
    test_validation_catches_bad_tasks()
    test_it_can_fail()
    test_the_whole_suite()
    test_cassettes()
    print("\nevals: all checks passed")


if __name__ == "__main__":
    main()

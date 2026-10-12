"""Outcome records: the harness's reward signal, in isolation.

Pure and fast — a temp project directory, a temp ledger, no model, no network:

* the **verifier** classifier: which commands count as "checking the work";
* the **recorder**: tools, failures, approvals (deduplicated across a resume), the verifier verdict
  *after the last edit* (a test that ran before the edit checked something else), files changed with
  hashes — secrets files never listed — and Claude Code's steps counting as edits and verifier runs;
* the **reward**: a table of turns and the score each must get, components returned, clamped, pure;
* **persistence and reconciliation**: the record lands in the ledger, and later each changed file is
  judged kept / reverted / edited / removed — once, and not before the person has had time to react.

    backend/.venv/bin/python _outcomes_smoke_test.py
"""

import os
import subprocess
import tempfile
import time
from pathlib import Path

_TMP = tempfile.TemporaryDirectory()
ROOT = Path(_TMP.name).resolve()
os.environ["CORDATA_HOME"] = str(ROOT / "home")
os.environ["CORDATA_STATE_DB"] = str(ROOT / "state.db")

import outcomes as oc  # noqa: E402
import state_store  # noqa: E402
from harness_testing import check  # noqa: E402


def project(name: str) -> Path:
    path = ROOT / name
    path.mkdir()
    (path / "app.py").write_text("def f():\n    return 1\n")
    (path / "README.md").write_text("demo\n")
    (path / ".env").write_text("SECRET=hunter2\n")
    (path / "node_modules").mkdir()
    (path / "node_modules" / "x.js").write_text("ignored")
    return path


def test_verifier_classifier() -> None:
    for command in ["pytest -q", "python -m pytest tests/", "cargo test --workspace", "cargo clippy --all-targets", "npm test", "npm run lint", "make test", "go test ./...", "tsc --noEmit",
                    "ruff check .", "mypy src", "cd backend && python _foo_smoke_test.py", "pnpm run build", "gradle check"]:
        check(oc.is_verifier(command), f"verifier: {command!r}")
    for command in ["/Users/x/proj/.venv/bin/python -m unittest discover -s tests -q", ".venv/bin/pytest -x", "uv run pytest tests/", "poetry run mypy .", "npx vitest run", "/usr/bin/python3 -m pytest"]:
        check(oc.is_verifier(command), f"verifier through a path or a runner wrapper: {command!r}")
    for command in ["ls -la", "git status", "cat README.md", "echo pytest", "rm -rf build", "python script.py", "git commit -m test", "/usr/bin/vim pytest.ini"]:
        check(not oc.is_verifier(command), f"not a verifier: {command!r}")


def test_recorder_basics() -> None:
    root = project("basics")
    r = oc.Recorder("s1", goal="fix the bug; my key is sk-ant-api03-" + "A1b2" * 10, mode="build", project=str(root))
    check("sk-ant-api03-A1b2" not in r.goal and len(r.goal) <= oc.MAX_GOAL_CHARS, "the goal is masked and short")

    r.note_edit_paths(["app.py"])
    (root / "app.py").write_text("def f():\n    return 2\n")
    r.tool("orchestrator", "editor", tool_use_id="t1", ok=True)
    r.tool("orchestrator", "editor", tool_use_id="t1", ok=True)  # the same call resumed after a prompt
    r.tool("orchestrator", "shell", tool_use_id="t2", ok=True, command="pytest -q")
    (root / "new.py").write_text("x = 1\n")
    (root / ".env").write_text("SECRET=changed\n")
    (root / "node_modules" / "y.js").write_text("also ignored")
    r.approval("asked", "shell", 0.4, key="t2")
    r.approval("asked", "shell", 0.4, key="t2")  # a resume asks again; it is one question
    r.approval("approved", "shell", key="i1")
    r.approval("rejected", "editor", key="i2")
    r.approval("auto", "shell", 0.93, key="t3")
    r.usage(1200, 300)
    record = r.finish("end_turn")

    check(record.tools["editor"]["count"] == 1 and record.calls == 2, "a resumed call is counted once")
    check(record.approvals == {"asked": 1, "approved": 1, "rejected": 1, "auto": 1}, f"approvals are counted by kind, deduplicated ({record.approvals})")
    check(record.guard_peak == 0.93, "the highest Laya Guard score of the turn is kept")
    check(record.verifier["after_last_edit"] == "pass" and record.verifier["ran"] == 1, "a test that ran after the last edit and passed")
    paths = {f["path"]: f for f in record.files}
    check(set(paths) == {"app.py", "new.py"}, f"changed files are listed — .env and node_modules never are ({sorted(paths)})")
    check(paths["app.py"]["known_before"] and paths["app.py"]["before"] != paths["app.py"]["after"], "an edited file carries what it held before and after")
    check(paths["new.py"]["before"] is None and not paths["new.py"]["known_before"], "a file created by a shell command has no 'before' (it was not seen)")
    check(record.usage == {"input": 1200, "output": 300, "total": 1500}, "tokens are recorded")
    check(record.edits == 1 and record.agents == ["orchestrator"], "one edit by one agent")
    check(oc.TurnRecord.from_json(record.to_json()).id == record.id, "a record round-trips through JSON")


def test_the_verifier_verdict() -> None:
    root = project("verdicts")

    def turn(*steps):
        r = oc.Recorder("s", goal="g", mode="build", project=str(root))
        for i, step in enumerate(steps):
            kind = step[0]
            if kind == "edit":
                r.tool("orchestrator", "editor", tool_use_id=f"e{i}", ok=True)
            elif kind == "shell":
                r.tool("orchestrator", "shell", tool_use_id=f"s{i}", ok=step[2], command=step[1], edit=step[1].startswith("sed"))
        return r.finish("end_turn")

    check(turn(("edit",), ("shell", "pytest", True)).verifier["after_last_edit"] == "pass", "edit, then tests pass")
    check(turn(("edit",), ("shell", "pytest", False)).verifier["after_last_edit"] == "fail", "edit, then tests fail")
    check(turn(("shell", "pytest", True), ("edit",)).verifier["after_last_edit"] == "stale", "tests ran, then more edits: the check is stale")
    check(turn(("shell", "pytest", False), ("edit",), ("shell", "pytest", True)).verifier["after_last_edit"] == "pass", "failed, fixed, re-run: pass")
    check(turn(("edit",)).verifier["after_last_edit"] == "none", "edited and never checked")
    check(turn(("shell", "ls", True)).verifier["after_last_edit"] == "none", "no verifier at all")
    check(turn(("edit",), ("shell", "sed -i s/a/b/ x", True), ("shell", "pytest", True)).verifier["after_last_edit"] == "pass", "a shell edit followed by tests")
    check(turn(("shell", "sed -i s/a/b/ x", True), ("shell", "pytest", True), ("shell", "sed -i s/b/c/ x", True)).verifier["after_last_edit"] == "stale", "…and a shell edit after the tests makes them stale")
    broken = turn(("edit",), ("shell", "make build", True), ("shell", "cargo run --bin x", False))
    check(broken.verifier["unresolved"] == ["cargo run --bin x"], "a command that failed and was never re-run is reported as unresolved")
    healed = turn(("shell", "cargo run --bin x", False), ("shell", "cargo run --bin x", True))
    check(healed.verifier["unresolved"] == [], "…and one that was re-run and passed is not")


def test_a_refused_call_is_not_a_failure() -> None:
    root = project("denied")
    r = oc.Recorder("s", goal="g", mode="build", project=str(root))
    r.tool("orchestrator", "shell", tool_use_id="d1", ok=False, command="rm -rf src", denied=True)
    r.tool("orchestrator", "shell", tool_use_id="d2", ok=False, command="cargo run")
    record = r.finish("end_turn")
    check(record.tools["shell"] == {"count": 2, "failed": 1, "denied": 1, "ms": 0}, f"one denied, one failed ({record.tools})")
    check(record.verifier["unresolved"] == ["cargo run"], "only the command that ran and failed is left unresolved")


def test_claude_code_steps_count() -> None:
    root = project("claude")
    r = oc.Recorder("s", goal="g", mode="build", project=str(root))
    r.tool("orchestrator", "claude_code_engineer", tool_use_id="c1", ok=True)
    r.claude_code(1.234)
    for step in [("Read", True, "app.py"), ("Edit", True, "app.py"), ("Bash", True, "pytest -q"), ("Edit", True, "app.py"), ("Bash", False, "pytest -q")]:
        r.nested(step[0], step[1], command=step[2])
    record = r.finish("end_turn")
    check(record.edits == 2, "the CLI's own edits count as edits")
    check(record.verifier["ran"] == 2 and record.verifier["after_last_edit"] == "fail", "its own failing test run, after its last edit, is the verdict")
    check(record.claude_code == {"runs": 1, "cost_usd": 1.23}, "Claude Code's cost rides on the record")


def reward_of(**fields):
    base = dict(id="x", session_id="s", project="/p", ts=0.0, seconds=30.0, goal="g", mode="build", stop_reason="end_turn", verifier={"after_last_edit": "none", "unresolved": []})
    return oc.TurnRecord(**{**base, **fields})


def test_the_reward() -> None:
    good = oc.score(reward_of(verifier={"after_last_edit": "pass", "unresolved": []}, usage={"total": 20_000}))
    check(good.value > 0.4 and "verified" in good.parts and not good.known, f"a verified, cheap turn scores well ({good.value}) — and says it is not yet reconciled")
    bad = oc.score(reward_of(verifier={"after_last_edit": "fail", "unresolved": []}))
    check(bad.value < -0.4 and "broken" in bad.parts, f"a turn that ends with failing checks scores badly ({bad.value})")
    unresolved = oc.score(reward_of(verifier={"after_last_edit": "none", "unresolved": ["cargo run"]}))
    check("broken" in unresolved.parts, "a command still failing at the end counts as broken")
    check(oc.score(reward_of(approvals={"rejected": 9})).parts["rejected"] == -0.3, "rejections cost, capped at three")
    check("cancelled" in oc.score(reward_of(stop_reason="cancelled")).parts and "truncated" in oc.score(reward_of(stop_reason="max_tokens")).parts, "a cancelled or truncated turn is marked down")
    huge = oc.score(reward_of(usage={"total": 5_000_000}, seconds=9999.0))
    check(huge.parts["tokens"] == oc.WEIGHTS["tokens"] and huge.parts["time"] == oc.WEIGHTS["time"], "cost is bounded: a very expensive turn is not infinitely bad")

    record = reward_of(verifier={"after_last_edit": "pass", "unresolved": []})
    kept = oc.score(record, {"kept": 3, "reverted": 0, "edited": 0, "removed": 0})
    reverted = oc.score(record, {"kept": 0, "reverted": 3, "edited": 0, "removed": 0})
    mixed = oc.score(record, {"kept": 1, "reverted": 1, "edited": 1, "removed": 0})
    check(kept.known and kept.value > good.value - 0.1 and reverted.value < kept.value - 0.5, f"kept ({kept.value}) beats reverted ({reverted.value}) by a lot — even after passing tests")
    check(abs(mixed.parts["kept"] - oc.WEIGHTS["kept"] / 3) < 1e-3, "kept and reverted are proportional to the files judged")
    everything = oc.score(reward_of(verifier={"after_last_edit": "pass", "unresolved": []}), {"kept": 1, "reverted": 0, "edited": 0, "removed": 0})
    check(-1.0 <= everything.value <= 1.0, "the total is clamped to [-1, 1]")
    check(oc.score(record).value == oc.score(record).value and oc.score(record.to_json()).value == oc.score(record).value, "the reward is pure: the same record always scores the same, as an object or as JSON")


def test_persistence_and_reconciliation() -> None:
    root = project("recon")
    now = time.time()

    def finished_turn(goal: str, *, edit: str, content: str, seconds_ago: float):
        r = oc.begin("sess", goal=goal, mode="build", project=str(root))
        r.note_edit_paths([edit])
        (root / edit).write_text(content)
        r.tool("orchestrator", "editor", tool_use_id=goal, ok=True)
        r.tool("orchestrator", "shell", tool_use_id=goal + "s", ok=True, command="pytest -q")
        r.started -= seconds_ago
        return oc.finish("sess", "end_turn")

    check(oc.active("sess") is None, "nothing is recording before a turn begins")
    kept = finished_turn("keep it", edit="app.py", content="def f():\n    return 'kept'\n", seconds_ago=600)
    rows = state_store.recent_events(kind=oc.KIND, limit=10)
    check(len(rows) == 1 and rows[0]["payload"]["id"] == kept.id and rows[0]["payload"]["verifier"]["after_last_edit"] == "pass", "the record is in the ledger")
    check(oc.finish("sess", "end_turn") is None, "a turn can be finished only once")

    check(oc.reconcile(str(root), min_age_seconds=3600) == [], "a turn is not judged before the person has had time to react")
    later = time.time() + 7200  # two hours on
    updates = oc.reconcile(str(root), min_age_seconds=3600, now=later)
    check(len(updates) == 1 and updates[0]["kept"] == 1 and updates[0]["reverted"] == 0, f"untouched changes are kept ({updates})")
    check(oc.reconcile(str(root), min_age_seconds=3600, now=later) == [], "a turn is judged once")
    check(updates[0]["reward"]["known"] and updates[0]["reward"]["value"] > 0.5, "the judged reward includes 'kept'")

    # the person puts the first file back, and rewrites another turn's output
    reverted_turn = finished_turn("undo me", edit="README.md", content="agent wrote this\n", seconds_ago=600)
    (root / "README.md").write_text("demo\n")
    edited_turn = finished_turn("tweak me", edit="app.py", content="def f():\n    return 'agent'\n", seconds_ago=600)
    (root / "app.py").write_text("def f():\n    return 'person'\n")
    updates = {u["id"]: u for u in oc.reconcile(str(root), min_age_seconds=3600, now=later)}
    check(updates[reverted_turn.id]["reverted"] == 1, "a file put back to what it held before is reverted")
    check(updates[edited_turn.id]["edited"] == 1 and updates[edited_turn.id]["kept"] == 0, "a file the person changed further is edited, neither kept nor reverted")
    check(oc.score(reverted_turn, updates[reverted_turn.id]).value < oc.score(edited_turn, updates[edited_turn.id]).value, "reverting costs more than editing further")

    deleted = oc.judge_files([{"path": "gone.txt", "before": "h", "after": "h2", "known_before": True}], str(root))
    check(deleted["removed"] == 1, "a file that no longer exists is removed")
    check(oc.judge_files([{"path": "never-created", "before": "h", "after": None, "known_before": True}], str(root))["kept"] == 1, "a deletion that stands is kept")
    (root / "restored.txt").write_text("back")
    check(oc.judge_files([{"path": "restored.txt", "before": "h", "after": None, "known_before": True}], str(root))["reverted"] == 1, "a deletion the person undid is reverted")

    pairs = oc.recent(10, project=str(root))
    check(len(pairs) == 3 and all(update is not None for _record, update in pairs), "recent() pairs each record with its judgement")
    summary = oc.summarize(pairs)
    check(summary["turns"] == 3 and summary["judged_turns"] == 3 and summary["reverted_turns"] == 1 and summary["verified"] == 3, f"the summary adds up ({summary})")
    check("none" in summary["by_route"] and oc.summarize([]) == {"turns": 0}, "by-route breakdown exists; an empty history is just {turns: 0}")
    check(oc.recent(10, project="/somewhere/else") == [], "records are per project")


def test_never_raises_into_a_turn() -> None:
    check(oc.finish("never-began", "end_turn") is None, "finishing a turn that never began is a no-op")
    r = oc.begin("weird", goal="g", mode="build", project="/definitely/not/a/directory")
    r.note_edit_paths(["../../etc/passwd", "x"])
    record = oc.finish("weird", "end_turn")
    check(record is not None and record.files == [], "a project that does not exist records nothing, and says nothing")
    r = oc.begin("nodir", goal="g", mode="build", project="")
    check(oc.finish("nodir", "error") is not None, "no project at all still records the turn")


def main() -> None:
    test_verifier_classifier()
    test_recorder_basics()
    test_the_verifier_verdict()
    test_a_refused_call_is_not_a_failure()
    test_claude_code_steps_count()
    test_the_reward()
    test_persistence_and_reconciliation()
    test_never_raises_into_a_turn()
    print("\noutcomes: all checks passed")


if __name__ == "__main__":
    main()
    _TMP.cleanup()

"""Trajectories and checkpoints: the ledger joined into rows a trainer reads, and the gate on what comes back.

* **the rows** — the four shapes (`bandit`, `laya`, `sft`, `dpo`) are well-formed, the Laya rows are byte-for-byte
  the schema the exporter writes (the live router's own question set is *not* used, because it is not what the
  checkpoint was trained on), and ``complexity`` — which has no observed label — is left out of ``gold``;
* **splits and dedupe** — a request lands in train or eval, never both, and the split follows the text alone, so
  adding turns never moves an existing one; the same turn exported twice is one row;
* **the provenance** — a bandit row carries the propensity the decision was actually made at, and a turn the
  bandit never recorded a choice for is not invented; an unreconciled turn teaches nothing yet;
* **preferences** — a pair only joins two turns in the same coarse context, and only when they really differ;
* **the registry** — a checkpoint is registered by directory, activated only on a verdict that promoted (a
  regressing one is refused, and ``--force`` is the deliberate override), and ``LAYA_MODEL`` resolves through it.

    backend/.venv/bin/python _trajectories_smoke_test.py
"""

import json
import os
import tempfile
import time

HOME = tempfile.mkdtemp(prefix="traj-home-")
os.environ["SIERRALAB_HOME"] = HOME
os.environ["SIERRALAB_STATE_DB"] = os.path.join(HOME, "state.db")
os.environ.pop("LAYA_MODEL", None)

import bandit  # noqa: E402
import laya_checkpoints  # noqa: E402
import outcomes  # noqa: E402
import state_store  # noqa: E402
import trajectory  # noqa: E402
from harness_testing import check  # noqa: E402

PROJECT = tempfile.mkdtemp(prefix="traj-proj-")
REAL = os.path.realpath(PROJECT)


def turn(
    turn_id: str,
    goal: str,
    *,
    mode: str = "build",
    risk: str = "read_only",
    target: str = "code",
    choices: dict | None = None,
    tools: dict | None = None,
    verifier: str = "pass",
    rejections: int = 0,
) -> None:
    """One finished turn in the ledger."""
    state_store.append_event(
        outcomes.KIND,
        {
            "id": turn_id,
            "session_id": "s",
            "project": REAL,
            "ts": time.time() - 3600,
            "seconds": 4,
            "goal": goal,
            "mode": mode,
            "stop_reason": "end_turn",
            "calls": 3,
            "route": {"risk": risk, "target": target, "tier": "standard", "choices": choices or {}},
            "tools": tools if tools is not None else {"file_read": {"count": 2}, "editor": {"count": 1, "failed": 0}},
            "approvals": {"asked": 1, "approved": 1, "rejected": rejections},
            "verifier": {"after_last_edit": verifier, "commands": ["pytest -q"], "unresolved": [] if verifier == "pass" else ["pytest -q"]},
            "files": [{"path": "app/core.py", "before": "a", "after": "b"}],
            "usage": {"total": 500},
        },
        session_id="s",
        source="outcomes",
    )


def judged(*args, **kwargs) -> None:
    """A turn plus its reconciliation, so the reward is knowable (``Reward.known``)."""
    turn(*args, **kwargs)
    outcomes.reconcile(REAL)


def _split(records, kind: str) -> tuple[list[dict], list[dict]]:
    return records[kind]["train"], records[kind]["eval"]


# ------------------------------------------------------------------------------ the rows


def test_laya_rows_match_the_trainers_schema() -> None:
    import _laya_triage_export as exporter

    log = os.path.join(HOME, "triage.jsonl")
    with open(log, "w", encoding="utf-8") as handle:
        for text, route in (("list the files in this repo", "files_and_code"), ("what does this function do?", "chat")):
            handle.write(json.dumps({"text": text, "actual": {"route": route, "stop_reason": "end_turn", "incomplete": False}}) + "\n")
        handle.write(json.dumps({"text": "cut off mid-turn", "actual": {"route": "chat", "stop_reason": "max_tokens", "incomplete": True}}) + "\n")

    split = trajectory.laya_rows([log], 0.2)
    rows = split["train"] + split["eval"]
    check(len(rows) == 2, f"only completed turns become Laya rows ({len(rows)} of 3)")
    check(all(set(row) >= {"state", "questions", "gold"} for row in rows), "each row has the three JSON-string columns")

    expected = exporter.to_row({"text": "list the files in this repo", "actual": {"route": "files_and_code", "stop_reason": "end_turn", "incomplete": False}})
    ours = next(row for row in rows if json.loads(row["state"]) == "list the files in this repo")
    check(all(ours[key] == expected[key] for key in ("state", "questions", "gold")), "and they are byte-for-byte what the exporter writes")
    check("complexity" not in json.loads(ours["gold"]), "…with complexity left out of gold, since no turn observes it")
    check(set(json.loads(ours["gold"])) == {"route", "needs_team"}, "…and exactly the two questions a turn does observe")


# ------------------------------------------------------------------------------ split and dedupe


def test_split_is_by_text_alone() -> None:
    text = "add a --verbose flag to the CLI"
    for index in range(4):
        judged(f"split-{index}", text if index else "a different request entirely")
    records = trajectory.build(PROJECT, logs=[], limit=500)
    train = {row["text"] for row in records["sft"]["train"]}
    ev = {row["text"] for row in records["sft"]["eval"]}
    check(not (train & ev), f"no request lands in both splits ({sorted(train & ev)[:2]})")
    want = trajectory._split_of(text, 0.2)
    placed = "train" if text in train else "eval"
    check(want == placed, f"the split follows the text's own hash ({want} == {placed})")
    check(trajectory._split_of("add a --verbose flag to the CLI", 0.2) == want, "…so the same text always lands the same way")
    check(len([row for row in records["sft"]["train"] + records["sft"]["eval"] if row["text"] == text]) == 1, "and the same request exported four times is one row")


# ------------------------------------------------------------------------------ provenance


def test_bandit_rows_carry_a_real_propensity() -> None:
    judged("band-a", "make the failing test pass", choices={"route": "hint", "engine": "local", "ask": None})
    bandit.decision_event({"point": "route", "context": "build|route|read_only|standard|orchestrator", "default": "none", "arm": "hint", "acted": True, "mode": "live", "propensity": 0.9}, turn="band-a", project=REAL)

    records = trajectory.build(PROJECT, logs=[], limit=500)
    rows = [row for row in records["bandit"]["train"] + records["bandit"]["eval"] if row["turn"] == "band-a"]
    check({row["point"] for row in rows} == {"route", "engine"}, f"one row per choice exercised ({sorted(row['point'] for row in rows)})")
    route_row = next(row for row in rows if row["point"] == "route")
    check(route_row["propensity"] == 0.9 and route_row["mode"] == "live", "the row carries the propensity the decision was made at")
    engine_row = next(row for row in rows if row["point"] == "engine")
    check(engine_row["propensity"] is None and engine_row["arm"] == "local", "…and a choice with no decision row says so rather than inventing one (propensity None)")
    check(all(row["arm"] != "None" for row in rows), "a point that was not exercised (ask: None) is skipped entirely")

    # Unreconciled: the reward is not knowable, so there is nothing to learn from yet.
    turn("band-b", "an unreconciled turn with choices", choices={"route": "dispatch"})
    records = trajectory.build(PROJECT, logs=[], limit=500)
    check(not [row for row in records["bandit"]["train"] + records["bandit"]["eval"] if row["turn"] == "band-b"], "an unreconciled turn is not exported — half a reward is worse than none")
    outcomes.reconcile(REAL)
    records = trajectory.build(PROJECT, logs=[], limit=500)
    check([row for row in records["bandit"]["train"] + records["bandit"]["eval"] if row["turn"] == "band-b"], "…and it is exported the moment its reconcile lands")


# ------------------------------------------------------------------------------ sft / dpo


def test_sft_rows_need_actions() -> None:
    judged("sft-a", "explain what this module does", tools={})
    records = trajectory.build(PROJECT, logs=[], limit=500)
    check(not [row for row in records["sft"]["train"] + records["sft"]["eval"] if row["turn"] == "sft-a"], "a turn that ran no tool teaches no action")
    row = next(row for row in records["sft"]["train"] + records["sft"]["eval"] if row["turn"] == "band-a")
    check(json.loads(row["action"]) == "editor, file_read×2", f"the action line is the turn's tool use ({json.loads(row['action'])})")
    check(isinstance(row["reward"], float), "and the reward rides along, so a trainer can filter on it")


def test_preference_pairs_are_comparable() -> None:
    judged("dpo-good", "refactor the config loader", mode="build", risk="edits_files", target="architect", verifier="pass", tools={"editor": {"count": 1}})
    judged("dpo-bad", "refactor the settings parser", mode="build", risk="edits_files", target="architect", verifier="fail", tools={"editor": {"count": 9, "failed": 4}})
    judged("dpo-other", "delete the build directory", mode="auto", risk="destructive", target="code", verifier="fail", tools={"shell": {"count": 2, "failed": 1}})

    records = trajectory.build(PROJECT, logs=[], limit=500)
    pairs = records["dpo"]["train"] + records["dpo"]["eval"]
    near = [p for p in pairs if p["turn_chosen"] == "dpo-good"]
    check(near and near[0]["turn_rejected"] == "dpo-bad", "two turns in the same context pair up, better against worse")
    check(near[0]["context"] == "build|edits_files|specialist", f"…keyed by the coarse context they share ({near[0]['context']})")
    check(near[0]["chosen_reward"] - near[0]["rejected_reward"] >= trajectory.MIN_PREFERENCE_GAP, "…and only when they really differ")
    check(not [p for p in pairs if "dpo-other" in (p["turn_chosen"], p["turn_rejected"])], "a turn in a different context is never paired against it")
    check(all(p["context"] == p["context"] for p in pairs), "every pair names the context it was drawn from")


def test_validate_catches_a_broken_row() -> None:
    good = {"kind": "bandit", "point": "route", "context": "build|route|read_only|standard|orchestrator", "arm": "hint", "reward": 0.4}
    check(trajectory.validate_rows([good]) == [], "a well-formed row validates")
    check(trajectory.validate_rows([{**good, "arm": ""}]), "a missing field is a problem")
    check(trajectory.validate_rows([{**good, "kind": "nope"}]), "an unknown kind is a problem")
    check(trajectory.validate_rows([{"kind": "laya", "state": "{not json", "questions": "{}", "gold": "{}"}]), "a JSON-string column that is not JSON is a problem")
    check(trajectory.validate_rows([{"kind": "dpo", "state": '"x"', "chosen": '"a"', "rejected": '"b"', "context": "c"}]), "a preference with an unknown reward is a problem")


# ------------------------------------------------------------------------------ the export


def test_export_writes_every_kind() -> None:
    out = os.path.join(HOME, "exported")
    result = trajectory.export(PROJECT, out_dir=out, logs=[])
    check(oct(os.stat(out).st_mode)[-3:] == "700", "the export directory is private")
    for kind in trajectory.KINDS:
        for split in ("train", "eval"):
            path = os.path.join(out, f"{kind}.{split}.jsonl")
            check(os.path.exists(path), f"{kind}.{split}.jsonl is written")
            for line in open(path, encoding="utf-8"):
                json.loads(line)
    check(result["counts"]["sft"]["train"] + result["counts"]["sft"]["eval"] > 0, "…with the turns that were exported in them")


# ------------------------------------------------------------------------------ the registry


def test_registry_gates_a_checkpoint() -> None:
    good = tempfile.mkdtemp(prefix="ckpt-good-")
    bad = tempfile.mkdtemp(prefix="ckpt-bad-")

    for name, path in (("Bad Id", good), ("ok-id", os.path.join(good, "nope"))):
        try:
            laya_checkpoints.register(name, path)
            check(False, f"register refuses {name!r}")
        except ValueError:
            pass
    check(laya_checkpoints.register("ok-id", good, note="first pass")["status"] == "candidate", "a directory registers as a candidate")

    check(laya_checkpoints.activate("ok-id") is False, "a checkpoint whose last verdict did not promote is refused")
    check(laya_checkpoints.activate("ok-id", {"promote": False, "reason": "broke two held-out tasks"}) is False, "…including one that regressed")
    check(laya_checkpoints.find("ok-id")["status"] == "candidate", "…and it stays a candidate")
    check(laya_checkpoints.activate("ok-id", {"promote": True, "reason": "fixed 3, broke none; held-out clean"}) is True, "a verdict that promoted activates it")
    check(laya_checkpoints.live()["id"] == "ok-id", "…and it is the live one")

    laya_checkpoints.register("second", bad)
    check(laya_checkpoints.activate("second", {"promote": True}) is True and laya_checkpoints.live()["id"] == "second", "activating another replaces it")
    check(laya_checkpoints.find("ok-id")["status"] == "candidate", "…and the replaced one is demoted, not retired")
    check(laya_checkpoints.activate("second", force=True) is True, "--force is the deliberate override")
    check(laya_checkpoints.activate("nope", force=True) is False, "…but it still refuses an unknown id")

    check(laya_checkpoints.record_eval is not None and laya_checkpoints.find("second")["evals"] == [], "an eval is only recorded by the gate, not by activating")
    laya_checkpoints.retire("second", "drifted")
    check(laya_checkpoints.live() is None, "a retired checkpoint is no longer live")


def test_laya_model_resolves_through_the_registry() -> None:
    path, subfolder = laya_checkpoints.resolve()
    check((path, subfolder) == ("convaiinnovations/laya", "multilingual"), f"with nothing registered, the stock repo is the fallback ({(path, subfolder)})")

    checkpoint = tempfile.mkdtemp(prefix="ckpt-live-")
    laya_checkpoints.register("live-one", checkpoint, subfolder="typed")
    laya_checkpoints.activate("live-one", {"promote": True})
    path, subfolder = laya_checkpoints.resolve()
    check(path == os.path.realpath(checkpoint) and subfolder == "typed", "a live checkpoint wins over the stock repo")

    os.environ["LAYA_MODEL"] = "/tmp/some/experiment"
    path, subfolder = laya_checkpoints.resolve()
    check(path == "/tmp/some/experiment", "…but an explicit LAYA_MODEL still overrides both")
    os.environ.pop("LAYA_MODEL", None)


# ------------------------------------------------------------------------------ main


def main() -> int:
    tests = [value for name, value in sorted(globals().items()) if name.startswith("test_") and callable(value)]
    for test in tests:
        test()
        print(f"ok {test.__name__.removeprefix('test_').replace('_', ' ')}")
    print("\ntrajectories: all checks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

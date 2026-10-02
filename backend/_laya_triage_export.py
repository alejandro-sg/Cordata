"""Turn laya_triage.py's log into fine-tuning data for Laya.

    backend/.venv/bin/python _laya_triage_export.py [LOG.jsonl ...] --out DIR
                            [--eval-fraction 0.2] [--validate]

Writes ``DIR/train.jsonl`` and ``DIR/eval.jsonl`` in the row schema of Laya's
own training set (``LocalLLaMA/typed-decisions``), which its fine-tuning script
(notebooks/laya_finetune_typed_decisions_mps.py) reads: three JSON-string
columns per row —

    state      the user's request, JSON-encoded            "\\"list the files\\""
    questions  {question_id: {type, instructions, criteria}}
    gold       {question_id: {"probabilities": {option: p}}}

The labels are what the orchestrator LLM *actually did* (the route its tool
calls imply; whether it used a team), one-hot. That is a behaviour-cloning
target: it teaches Laya to predict the LLM's routing, mistakes included, so
review the data or filter to turns you know went well before training on it.
``complexity`` has no observed label, so it is left out of ``gold`` (Laya's
script skips a question with no gold).

Only completed turns (stop_reason end_turn, not cut off) with usable text are
kept; duplicate requests are collapsed; the train/eval split is by a hash of the
text so the same request never lands in both. ``--validate`` pushes every
question through Laya's own ``build_sequence`` using the loaded checkpoint's
tokenizer (the same call its training script makes), so a schema mismatch fails
here instead of hours into a run.

Then fine-tune — and point the backend at the result with LAYA_MODEL=<output
dir> — as docs/laya-triage.md describes. Laya's README reports ~30k questions
for its own run; a few hundred rows will mostly memorise, so treat this as a
way to start the loop, not a finished model.
"""

import argparse
import hashlib
import json
import sys
from collections import Counter
from pathlib import Path

from laya_triage import ROUTES, TRIAGE_QUESTIONS

# complexity has no observed label; route and needs_team do.
LABELLED_QUESTIONS = {qid: TRIAGE_QUESTIONS[qid] for qid in ("route", "needs_team")}


def _split_of(text: str, eval_fraction: float) -> str:
    bucket = int(hashlib.sha256(text.encode("utf-8")).hexdigest(), 16) % 1000
    return "eval" if bucket < eval_fraction * 1000 else "train"


def to_row(record: dict) -> dict | None:
    """One dataset row from a log record, or None if it isn't usable."""
    actual = record.get("actual") or {}
    text = (record.get("text") or "").strip()
    if not text or actual.get("incomplete") or actual.get("stop_reason") != "end_turn":
        return None
    route = actual.get("route")
    if route not in ROUTES:
        return None
    route_gold = {key: 1.0 if key == route else 0.0 for key in ROUTES}
    team = route == "team"
    return {
        "state": json.dumps(text, ensure_ascii=False),
        "questions": json.dumps(LABELLED_QUESTIONS, ensure_ascii=False),
        "gold": json.dumps(
            {
                "route": {"probabilities": route_gold},
                "needs_team": {"probabilities": {"true": 1.0 if team else 0.0, "false": 0.0 if team else 1.0}},
            }
        ),
    }


def load_rows(paths: list[Path]) -> tuple[list[tuple[str, dict]], int]:
    """(text, row) pairs, de-duplicated by text, plus how many records were skipped."""
    seen: set[str] = set()
    rows: list[tuple[str, dict]] = []
    skipped = 0
    for path in paths:
        for line in path.read_text(encoding="utf-8").splitlines():
            if not line.strip():
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                skipped += 1
                continue
            row = to_row(record)
            text = (record.get("text") or "").strip()
            if row is None or text in seen:
                skipped += 1
                continue
            seen.add(text)
            rows.append((text, row))
    return rows, skipped


def validate_rows(rows: list[dict]) -> list[str]:
    """Run every question of every row through Laya's own build_sequence with
    the loaded checkpoint's tokenizer and config — what the training script's
    build_training_item does before it accepts an item. Returns problems."""
    from laya.common import build_sequence, render_options

    from prompt_security import _get_laya_agent

    agent = _get_laya_agent()
    if agent is None:
        return ["could not load Laya to validate against (package or weights unavailable)"]
    problems = []
    for index, row in enumerate(rows):
        state = json.loads(row["state"])
        questions = json.loads(row["questions"])
        gold = json.loads(row["gold"])
        for qid, question in questions.items():
            if qid not in gold:
                problems.append(f"row {index}: question {qid!r} has no gold")
                continue
            criteria = question["criteria"]
            n_options = len(render_options({"t": question["type"], "crit": criteria}))
            _sequence, markers = build_sequence(
                agent.tok, state, {"t": question["type"], "ins": question["instructions"], "crit": criteria},
                agent.cfg["max_len"], agent.cfg["head_max_len"],
            )
            if len(markers) != n_options:
                problems.append(f"row {index}: {qid!r} built {len(markers)} option markers, expected {n_options}")
    return problems


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("logs", nargs="*", type=Path, help="triage log(s); default: settings.LAYA_TRIAGE_LOG")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--eval-fraction", type=float, default=0.2)
    parser.add_argument("--validate", action="store_true", help="check rows with Laya's own build_sequence (loads the checkpoint)")
    args = parser.parse_args(argv)

    logs = args.logs
    if not logs:
        from settings import LAYA_TRIAGE_LOG

        logs = [Path(LAYA_TRIAGE_LOG)]
    missing = [str(path) for path in logs if not path.exists()]
    if missing:
        print(f"no such log: {', '.join(missing)}", file=sys.stderr)
        return 1

    pairs, skipped = load_rows(logs)
    if not pairs:
        print("no usable turns to export", file=sys.stderr)
        return 1
    split: dict[str, list[dict]] = {"train": [], "eval": []}
    for text, row in pairs:
        split[_split_of(text, args.eval_fraction)].append(row)

    args.out.mkdir(parents=True, exist_ok=True)
    for name, rows in split.items():
        (args.out / f"{name}.jsonl").write_text("".join(json.dumps(r, ensure_ascii=False) + "\n" for r in rows), encoding="utf-8")

    routes = Counter(next(k for k, v in json.loads(row["gold"])["route"]["probabilities"].items() if v == 1.0) for _t, row in pairs)
    print(f"exported {len(pairs)} rows ({len(split['train'])} train / {len(split['eval'])} eval); skipped {skipped} unusable or duplicate records")
    print(f"route labels: {dict(routes.most_common())}")
    thin = [route for route in ROUTES if routes.get(route, 0) < 5]
    if thin:
        print(f"warning: fewer than 5 examples for {thin} — Laya cannot learn those routes from this data", file=sys.stderr)
    if len(pairs) < 500:
        print(f"warning: {len(pairs)} rows is far below the ~30k questions Laya's own fine-tune used; expect memorisation, not generalisation", file=sys.stderr)
    print("labels are what the orchestrator LLM did, not necessarily what was right — review before training")

    if args.validate:
        problems = validate_rows([row for _text, row in pairs])
        if problems:
            print("VALIDATION FAILED:", *problems[:10], sep="\n  ", file=sys.stderr)
            return 2
        print(f"validated: all {len(pairs) * len(LABELLED_QUESTIONS)} questions build with Laya's own build_sequence")
    print(f"wrote {args.out}/train.jsonl and eval.jsonl")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

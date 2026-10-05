"""The trajectory export: what the harness did, in the shape a trainer reads.

The ledger already holds every turn (`outcomes.py`) and every choice the contextual bandit made
(`bandit.py`). Nothing trains on it, because a row a trainer cannot parse is not data. This module is the
join, in four shapes, each written as JSONL under ``~/.sierralab/trajectories/<project>/``:

    bandit   (context, arm, propensity, reward) — the off-policy rows a model that replaces the tabular
             scorer behind `bandit.choose` would be fit to. Only choices the bandit actually recorded a
             decision for are emitted, because that decision is where the propensity comes from; a row with
             a made-up propensity teaches the wrong thing about how the data was collected.
    laya     Laya's own behaviour-cloning rows — `state` / `questions` / `gold` as JSON-string columns —
             byte-for-byte the schema `_laya_triage_export.py` writes and Laya's fine-tuning script reads.
             The schema is *reused, not re-derived*: the live router's `questions_for` asks a different
             question set (target ids, not the six routes) and is not what the checkpoint was trained on.
    sft      (request, the tools the turn ran, its reward) — imitation data for the small local models.
    dpo      a preference pair: two turns in the same coarse context, one that scored well and one that did
             not. Comparable contexts only — a pair across unlike turns teaches "this task is easier than
             that one", which is not a preference.

Every row is split train / eval by a hash of its own text (the request), so the same request never lands in
both — the same split `_laya_triage_export.py` uses, for the same reason. Duplicate requests are collapsed.

    backend/.venv/bin/python backend/trajectory.py export --project ~/proj --validate
    backend/.venv/bin/python backend/trajectory.py export --out ./data --eval-fraction 0.2

``--validate`` shape-checks every row and, for the Laya rows, pushes every question through Laya's own
``build_sequence`` with the loaded checkpoint's tokenizer — the call its training script makes — so a schema
mismatch fails here instead of hours into a run.

Stdlib plus `bandit` and `outcomes`; no model, no Strands (the Laya validation call imports Laya lazily and
is the one exception, and only when asked for).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any

import bandit
import outcomes

KINDS = ("bandit", "laya", "sft", "dpo")
#: A preference pair has to be a real difference, not model noise: below this gap the two turns are the same
#: turn twice, and pairing them teaches nothing but the noise.
MIN_PREFERENCE_GAP = 0.2
#: At most this many pairs per context, so one common context cannot swamp the export.
MAX_PAIRS_PER_CONTEXT = 20
#: How many ledger turns a single export walks.
DEFAULT_LIMIT = 500


# ------------------------------------------------------------------------------ store


def home() -> Path:
    return Path(os.environ.get("SIERRALAB_HOME") or Path.home() / ".sierralab") / "trajectories"


def project_dir(project: str | os.PathLike | None, out_dir: str | os.PathLike | None = None) -> Path:
    return Path(out_dir) if out_dir is not None else home() / bandit.project_key(project)


def _split_of(text: str, eval_fraction: float) -> str:
    """Train or eval, from the text alone — so adding turns never moves an existing one between splits."""
    bucket = int(hashlib.sha256(text.encode("utf-8")).hexdigest(), 16) % 1000
    return "eval" if bucket < eval_fraction * 1000 else "train"


def _text(record: Any) -> str:
    return str(getattr(record, "goal", "") or "").strip()


# ------------------------------------------------------------------------------ bandit rows


def _decisions(project: str | None) -> dict[tuple[str, str], dict[str, Any]]:
    """``(turn, point) -> decision`` from the bandit's own audit rows — where the propensity lives."""
    import state_store

    out: dict[tuple[str, str], dict[str, Any]] = {}
    for row in state_store.recent_events(kind="bandit", limit=2000):
        payload = row.get("payload") or {}
        if payload.get("action") != "decide":
            continue
        if project and payload.get("project") != project:
            continue
        out[(str(payload.get("turn") or ""), str(payload.get("point") or ""))] = payload
    return out


def bandit_rows(records: list[tuple[Any, dict[str, Any] | None]], decisions: dict[tuple[str, str], dict[str, Any]]) -> list[dict[str, Any]]:
    """One row per choice made this turn, for every turn the bandit can attribute a reward to."""
    rows: list[dict[str, Any]] = []
    for record, update in records:
        value = bandit.reward(record, update)
        if value is None:
            continue
        choices = (getattr(record, "route", None) or {}).get("choices") or {}
        turn = str(getattr(record, "id", "") or "")
        for point, arm in sorted(choices.items()):
            if arm is None:
                continue  # the point was not exercised (no delegated task), so there is nothing to learn
            seen = decisions.get((turn, str(point)))
            rows.append(
                {
                    "kind": "bandit",
                    "turn": turn,
                    "project": getattr(record, "project", None),
                    "point": str(point),
                    "context": bandit.context(record, str(point)),
                    "arm": str(arm),
                    "default": (seen or {}).get("default"),
                    "acted": (seen or {}).get("acted"),
                    "mode": (seen or {}).get("mode"),
                    "propensity": (seen or {}).get("propensity"),
                    "reward": value,
                    "text": _text(record),
                }
            )
    return rows


# ------------------------------------------------------------------------------ laya rows


def laya_rows(logs: list[Path], eval_fraction: float) -> dict[str, list[dict[str, Any]]]:
    """Laya's behaviour-cloning rows, in the exporter's schema, split train / eval by request text."""
    import _laya_triage_export as exporter

    pairs, _skipped = exporter.load_rows([Path(p) for p in logs])
    split: dict[str, list[dict[str, Any]]] = {"train": [], "eval": []}
    for text, row in pairs:
        split[_split_of(text, eval_fraction)].append({**row, "kind": "laya", "text": text})
    return split


# ------------------------------------------------------------------------------ sft / dpo rows


def action_text(record: Any) -> str:
    """The turn's tool use as one line: ``file_read×2, editor, shell(failed×1)``. Deliberately coarse — a
    small local model is being taught *which actions this kind of request needs*, not the arguments."""
    tools = getattr(record, "tools", None) or {}
    parts = []
    for name in sorted(tools):
        stat = tools.get(name) or {}
        count = int(stat.get("count", 0) or 0)
        failed = int(stat.get("failed", 0) or 0)
        piece = f"{name}×{count}" if count != 1 else str(name)
        if failed:
            piece += f"(failed×{failed})"
        parts.append(piece)
    return ", ".join(parts)


def sft_rows(records: list[tuple[Any, dict[str, Any] | None]]) -> list[dict[str, Any]]:
    """(request, actions, reward) for every completed turn that ran at least one tool."""
    rows = []
    for record, update in records:
        text = _text(record)
        actions = action_text(record)
        if not text or not actions:
            continue
        scored = outcomes.score(record, update)
        rows.append(
            {
                "kind": "sft",
                "turn": str(getattr(record, "id", "") or ""),
                "state": json.dumps(text, ensure_ascii=False),
                "action": json.dumps(actions, ensure_ascii=False),
                "tools": sorted((getattr(record, "tools", None) or {})),
                "reward": scored.value if scored.known else None,
                "text": text,
            }
        )
    return rows


def _pair_context(record: Any) -> str:
    """Coarse enough that two turns in it are comparable, fine enough to mean something: mode, the route's
    risk, and what kind of target was chosen."""
    route = getattr(record, "route", None) or {}
    return "|".join((str(getattr(record, "mode", "") or "build"), str(route.get("risk") or "none"), bandit.target_kind(route.get("target"))))


def dpo_rows(records: list[tuple[Any, dict[str, Any] | None]]) -> list[dict[str, Any]]:
    """Preference pairs within one coarse context: the best-scoring turn against the worst, gap permitting."""
    groups: dict[str, list[tuple[float, Any]]] = defaultdict(list)
    for record, update in records:
        value = bandit.reward(record, update)
        if value is None or not _text(record):
            continue
        groups[_pair_context(record)].append((value, record))
    rows: list[dict[str, Any]] = []
    for ctx, members in sorted(groups.items()):
        members.sort(key=lambda m: (m[0], str(getattr(m[1], "id", ""))))
        pairs = 0
        low, high = 0, len(members) - 1
        while low < high and pairs < MAX_PAIRS_PER_CONTEXT:
            worst, best = members[low], members[high]
            if best[0] - worst[0] < MIN_PREFERENCE_GAP:
                break  # nothing in this context separates; more pairs would only add noise
            rows.append(
                {
                    "kind": "dpo",
                    "context": ctx,
                    "state": json.dumps(_text(best[1]), ensure_ascii=False),
                    "chosen": json.dumps(action_text(best[1]), ensure_ascii=False),
                    "rejected": json.dumps(action_text(worst[1]), ensure_ascii=False),
                    "chosen_reward": best[0],
                    "rejected_reward": worst[0],
                    "turn_chosen": str(getattr(best[1], "id", "") or ""),
                    "turn_rejected": str(getattr(worst[1], "id", "") or ""),
                    "text": _text(best[1]),
                }
            )
            low, high, pairs = low + 1, high - 1, pairs + 1
    return rows


# ------------------------------------------------------------------------------ export


def row_key(row: dict[str, Any]) -> str:
    """What makes two rows *the same example*. Per kind, because "the same" means something different in each:
    a turn contributes one row per choice it exercised, one request is one SFT example however many turns made
    it, and a preference is identified by the pair it compares."""
    kind = str(row.get("kind") or "")
    if kind == "bandit":
        return f"bandit|{row.get('turn')}|{row.get('point')}"
    if kind == "laya":
        return f"laya|{row.get('text')}"
    if kind == "dpo":
        return f"dpo|{row.get('turn_chosen')}|{row.get('turn_rejected')}"
    return f"sft|{row.get('text') or row.get('turn')}"


def _dedupe(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Drop rows with the same identity — the same example twice would double its weight in a loss."""
    seen: set[str] = set()
    out = []
    for row in rows:
        key = row_key(row)
        if key in seen:
            continue
        seen.add(key)
        out.append(row)
    return out


def build(project: str | os.PathLike | None, *, logs: list[Path] | None = None, limit: int = DEFAULT_LIMIT, eval_fraction: float = 0.2) -> dict[str, dict[str, list[dict[str, Any]]]]:
    """Every kind's rows, already split train / eval. Reads the ledger and (for Laya) the triage log; writes
    nothing, so a caller can inspect before anything lands on disk."""
    real = os.path.realpath(str(project)) if project is not None else None
    records = _dedupe_turns(outcomes.recent(limit, project=real))
    rows: dict[str, dict[str, list[dict[str, Any]]]] = {kind: {"train": [], "eval": []} for kind in KINDS}
    for row in _dedupe(bandit_rows(records, _decisions(real))):
        rows["bandit"][_split_of(str(row.get("text") or row.get("turn") or ""), eval_fraction)].append(row)
    for row in _dedupe(sft_rows(records)):
        rows["sft"][_split_of(str(row["text"]), eval_fraction)].append(row)
    for row in dpo_rows(records):
        rows["dpo"][_split_of(str(row["text"]), eval_fraction)].append(row)
    if logs:
        rows["laya"] = laya_rows(list(logs), eval_fraction)
    return rows


def _dedupe_turns(records: list[tuple[Any, dict[str, Any] | None]]) -> list[tuple[Any, dict[str, Any] | None]]:
    seen: set[str] = set()
    out = []
    for record, update in records:
        turn = str(getattr(record, "id", "") or "")
        if turn and turn in seen:
            continue  # a reconcile can surface the same turn twice; it is still one turn
        seen.add(turn)
        out.append((record, update))
    return out


def export(
    project: str | os.PathLike | None,
    *,
    out_dir: str | os.PathLike | None = None,
    logs: list[Path] | None = None,
    limit: int = DEFAULT_LIMIT,
    eval_fraction: float = 0.2,
) -> dict[str, Any]:
    """Write every kind's rows under the project's trajectory directory. Returns the counts."""
    rows = build(project, logs=logs, limit=limit, eval_fraction=eval_fraction)
    target = project_dir(project, out_dir)
    target.mkdir(parents=True, exist_ok=True)
    os.chmod(target, 0o700)
    counts: dict[str, dict[str, int]] = {}
    written: list[str] = []
    for kind in KINDS:
        counts[kind] = {}
        for split in ("train", "eval"):
            path = target / f"{kind}.{split}.jsonl"
            payload = "".join(json.dumps(row, ensure_ascii=False) + "\n" for row in rows[kind][split])
            path.write_text(payload, encoding="utf-8")
            written.append(str(path))
            counts[kind][split] = len(rows[kind][split])
    return {"out": str(target), "counts": counts, "written": written, "rows": rows}


# ------------------------------------------------------------------------------ validation


def _required(kind: str) -> tuple[str, ...]:
    if kind == "laya":
        return ("state", "questions", "gold")
    if kind == "bandit":
        return ("context", "arm", "reward", "point")
    if kind == "sft":
        return ("state", "action")
    return ("state", "chosen", "rejected", "context")


def validate_rows(rows: list[dict[str, Any]]) -> list[str]:
    """Shape-check rows: required keys present, JSON-string columns really JSON. One line per problem."""
    problems: list[str] = []
    for index, row in enumerate(rows):
        kind = str(row.get("kind") or "")
        if kind not in KINDS:
            problems.append(f"row {index}: unknown kind {kind!r}")
            continue
        for key in _required(kind):
            if row.get(key) in (None, ""):
                problems.append(f"row {index} ({kind}): missing {key!r}")
        for key in ("state", "questions", "gold", "action", "chosen", "rejected"):
            value = row.get(key)
            if isinstance(value, str):
                try:
                    json.loads(value)
                except json.JSONDecodeError:
                    problems.append(f"row {index} ({kind}): {key!r} is not JSON")
        if kind == "dpo" and not (isinstance(row.get("chosen_reward"), float) and isinstance(row.get("rejected_reward"), float)):
            problems.append(f"row {index} (dpo): both rewards must be known — a preference needs a comparison")
    return problems


def validate(rows_by_kind: dict[str, list[dict[str, Any]]], *, laya: bool = True) -> list[str]:
    """Every row's shape, plus (unless ``laya=False``) Laya's own ``build_sequence`` check on its rows."""
    problems: list[str] = []
    for kind, rows in rows_by_kind.items():
        problems.extend(validate_rows([{**row, "kind": kind} for row in rows]))
    if laya and rows_by_kind.get("laya"):
        import _laya_triage_export as exporter

        problems.extend(exporter.validate_rows(rows_by_kind["laya"]))
    return problems


# ------------------------------------------------------------------------------ cli


def _load_logs(paths: list[Path]) -> list[Path]:
    if paths:
        return paths
    from settings import LAYA_TRIAGE_LOG

    return [Path(LAYA_TRIAGE_LOG)]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="trajectory", description="Export the ledger as fine-tuning rows (docs/laya-triage.md).")
    parser.add_argument("command", choices=("export", "validate"), nargs="?", default="export")
    parser.add_argument("--project", default=os.getcwd())
    parser.add_argument("--out", type=Path)
    parser.add_argument("--logs", nargs="*", type=Path, help="Laya triage log(s); default: settings.LAYA_TRIAGE_LOG")
    parser.add_argument("--limit", type=int, default=DEFAULT_LIMIT, help="how many ledger turns to walk")
    parser.add_argument("--eval-fraction", type=float, default=0.2)
    parser.add_argument("--validate", action="store_true", help="check the rows, including Laya's own build_sequence pass")
    parser.add_argument("--no-laya", action="store_true", help="skip the triage log entirely (no Laya rows)")
    args = parser.parse_args(argv)

    logs = [] if args.no_laya else _load_logs(args.logs)
    existing = [p for p in logs if p.exists()]
    missing = [str(p) for p in logs if not p.exists()]
    if missing:
        print(f"no such triage log (skipping Laya rows): {', '.join(missing)}", file=sys.stderr)
    result = export(args.project, out_dir=args.out, logs=existing, limit=args.limit, eval_fraction=args.eval_fraction)
    counts = result["counts"]
    for kind in KINDS:
        print(f"{kind:7} train {counts[kind]['train']:5}  eval {counts[kind]['eval']:5}")
    print(f"wrote {result['out']}")

    if counts["bandit"]["train"] + counts["bandit"]["eval"]:
        thin = sum(1 for row in result["rows"]["bandit"]["train"] + result["rows"]["bandit"]["eval"] if row.get("propensity") is None)
        if thin:
            print(f"warning: {thin} bandit row(s) have no recorded propensity — the choice predates the bandit's audit log", file=sys.stderr)
    if args.validate:
        problems = validate({kind: result["rows"][kind]["train"] + result["rows"][kind]["eval"] for kind in KINDS})
        if problems:
            print("VALIDATION FAILED:", *problems[:10], sep="\n  ", file=sys.stderr)
            return 2
        print("validated: every row is well-formed")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

"""Fine-tuned Laya checkpoints: a registry, and the same gate a skill passes.

`trajectory.py` writes the rows; training happens off-machine (docs/laya-triage.md). What comes back is a
directory, and the question this module answers is the one every other learned artifact answers: **may it go
live?** A checkpoint is a candidate exactly like a staged skill — it is put in force for an eval run
(`LAYA_MODEL`), scored against the same suite on the same tasks as the checkpoint it would replace, and it is
promoted only if nothing that passed without it fails with it and something improved, on the dev tasks *and*
on held-out ones (`promote.py`). `shadow` records the verdict and switches nothing.

The registry (`~/.sierralab/laya/checkpoints.json`) maps an id to its directory, its measured numbers, and its
status. `LAYA_MODEL` itself resolves through it (`settings.py`): with the environment variable unset, a live
checkpoint here wins over the stock repo, so activating one is all it takes to run on it — and setting
`LAYA_MODEL` explicitly still overrides everything, which is what makes an experiment possible without
touching the registry.

What a checkpoint changes is Laya's *weights* — which route it leans toward and how confident it is. It
cannot change what the harness permits: Laya Guard's floor, the risk thresholds and the approval gate are
read from code that does not consult this module, and a better checkpoint can only re-rank options that were
already on the table. The shipped checkpoints are near chance (docs/laya-triage.md), so until one of these is
trained on real turns and clears the gate, routing-derived arms are weak features — the point of the pipeline
is to make that loop runnable, not to promise it pays off on the first pass.

Stdlib only at import (``settings.py`` imports this at startup); the eval gate is imported lazily.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time
from pathlib import Path
from typing import Any, Callable

VERSION = 1
STATUSES = ("candidate", "live", "retired")
NAME_RE = re.compile(r"^[a-z][a-z0-9._-]{2,63}$")
#: Verdicts kept per checkpoint, newest last — enough to show a trend without growing unbounded.
MAX_EVALS = 20


def home() -> Path:
    return Path(os.environ.get("SIERRALAB_HOME") or Path.home() / ".sierralab") / "laya"


def registry_path() -> Path:
    return home() / "checkpoints.json"


def _load() -> dict[str, Any]:
    try:
        data = json.loads(registry_path().read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {"version": VERSION, "checkpoints": {}}
    if not isinstance(data, dict) or int(data.get("version", 0)) != VERSION:
        return {"version": VERSION, "checkpoints": {}}
    rows = {k: v for k, v in (data.get("checkpoints") or {}).items() if isinstance(v, dict)}
    return {"version": VERSION, "checkpoints": rows}


def _save(data: dict[str, Any]) -> None:
    path = registry_path()
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        os.chmod(path.parent, 0o700)
        tmp = path.with_suffix(".tmp")
        tmp.write_text(json.dumps(data, indent=1, sort_keys=True), encoding="utf-8")
        os.chmod(tmp, 0o600)
        os.replace(tmp, path)
    except OSError:
        pass


# ------------------------------------------------------------------------------ registry


def register(cid: str, path: str | os.PathLike, *, subfolder: str = "", note: str = "", status: str = "candidate") -> dict[str, Any]:
    """Record a checkpoint directory under ``cid``. Raises ``ValueError`` on a bad id, path or status — this is
    called by a person or a script, not from a turn, so a mistake should be loud rather than logged."""
    if not NAME_RE.match(cid or ""):
        raise ValueError(f"checkpoint id must match {NAME_RE.pattern}: {cid!r}")
    if status not in STATUSES:
        raise ValueError(f"status must be one of {STATUSES}: {status!r}")
    real = os.path.realpath(str(path))
    if not Path(real).exists():
        raise ValueError(f"no such checkpoint directory: {real} (a Hugging Face repo id is registered by its id, not a path)")
    data = _load()
    row = data["checkpoints"].get(cid) or {"evals": [], "created": time.time()}
    row.update({"id": cid, "path": real, "subfolder": subfolder, "status": status})
    if note:
        row["note"] = note
    data["checkpoints"][cid] = row
    _save(data)
    return row


def all_rows() -> list[dict[str, Any]]:
    return [row for _cid, row in sorted(_load()["checkpoints"].items())]


def find(cid: str) -> dict[str, Any] | None:
    return _load()["checkpoints"].get(cid)


def live() -> dict[str, Any] | None:
    """The live checkpoint, or ``None``. Never raises: a corrupt registry must not stop a backend starting."""
    for row in all_rows():
        if row.get("status") == "live" and row.get("path"):
            return {"id": row["id"], "path": row["path"], "subfolder": str(row.get("subfolder") or "")}
    return None


def resolve(default: str = "convaiinnovations/laya", default_subfolder: str = "multilingual") -> tuple[str, str]:
    """``(model, subfolder)`` for a backend starting now: an explicit ``LAYA_MODEL`` wins, then a live
    checkpoint, then the stock repo. Read by ``settings.py``."""
    env = os.environ.get("LAYA_MODEL", "")
    if env:
        return env, os.environ.get("LAYA_MODEL_SUBFOLDER", "")
    activated = live()
    if activated:
        return activated["path"], activated["subfolder"]
    return default, (default_subfolder if default == "convaiinnovations/laya" else "")


def record_eval(cid: str, verdict: dict[str, Any]) -> None:
    data = _load()
    row = data["checkpoints"].get(cid)
    if row is None:
        return
    row["evals"] = [*(row.get("evals") or []), verdict][-MAX_EVALS:]
    data["checkpoints"][cid] = row
    _save(data)


def activate(cid: str, verdict: dict[str, Any] | None = None, *, by: str = "gate", force: bool = False) -> bool:
    """Make a checkpoint live — **refused** unless its last verdict promoted, so a regressing checkpoint cannot
    go live by accident. ``force`` is the deliberate override for a person who has measured it elsewhere; it
    still refuses an unknown id."""
    row = find(cid)
    if row is None:
        return False
    if not force and not (verdict or {}).get("promote"):
        return False
    data = _load()
    for other in data["checkpoints"].values():
        if other.get("id") != cid and other.get("status") == "live":
            other["status"] = "candidate"  # replaced, not retired: it is still a checkpoint that once passed
            other["replaced_by"] = cid
    row = data["checkpoints"][cid]
    row["status"] = "live"
    row["activated"] = time.time()
    row["activated_by"] = by
    _save(data)
    return True


def retire(cid: str, why: str = "") -> bool:
    data = _load()
    row = data["checkpoints"].get(cid)
    if row is None:
        return False
    row["status"] = "retired"
    if why:
        row["retired_because"] = why
    _save(data)
    return True


# ------------------------------------------------------------------------------ the gate


def evaluate(
    cid: str,
    *,
    driver_factory: Callable[[], Any],
    project: str | os.PathLike | None = None,
    n_tasks: int = 12,
    run_suite: Callable[..., Any] | None = None,
    catalog: list | None = None,
    baseline: Any | None = None,
) -> dict[str, Any]:
    """Judge a registered checkpoint against the current live one, with the same A/B (and the same thresholds)
    a staged skill gets. Returns the verdict as a dict; records it on the checkpoint. Never raises."""
    import promote

    row = find(cid)
    if row is None:
        return {"name": cid, "promote": False, "reason": f"no checkpoint {cid!r} registered", "kind": "laya"}
    env = {"LAYA_MODEL": row["path"], "LAYA_MODEL_SUBFOLDER": str(row.get("subfolder") or "")}
    verdict = promote.evaluate(
        cid,
        project,
        kind="laya",
        driver_factory=driver_factory,
        n_tasks=n_tasks,
        baseline=baseline,
        run_suite=run_suite,
        catalog=catalog,
        arm_env=env,
    )
    record_eval(cid, verdict.to_json())
    return verdict.to_json()


def cycle(
    *,
    driver_factory: Callable[[], Any],
    project: str | os.PathLike | None = None,
    mode: str = "shadow",
    n_tasks: int = 12,
    run_suite: Callable[..., Any] | None = None,
    catalog: list | None = None,
    only: list[str] | None = None,
) -> list[dict[str, Any]]:
    """Evaluate every candidate checkpoint. ``shadow`` records; ``live`` also activates a pass."""
    if mode not in ("shadow", "live"):
        return []
    verdicts = []
    for row in all_rows():
        if row.get("status") != "candidate" or (only and row["id"] not in only):
            continue
        verdict = evaluate(row["id"], driver_factory=driver_factory, project=project, n_tasks=n_tasks, run_suite=run_suite, catalog=catalog)
        verdicts.append(verdict)
        if verdict.get("promote") and mode == "live":
            activate(row["id"], verdict, by="gate")
    return verdicts


# ------------------------------------------------------------------------------ cli


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="laya_checkpoints", description="Register fine-tuned Laya checkpoints and gate them against the eval suite.")
    sub = parser.add_subparsers(dest="command", required=True)

    add = sub.add_parser("register", help="record a checkpoint directory")
    add.add_argument("id")
    add.add_argument("path")
    add.add_argument("--subfolder", default="")
    add.add_argument("--note", default="")

    run = sub.add_parser("evaluate", help="run the A/B gate over candidate checkpoints")
    run.add_argument("--project", default=os.getcwd())
    run.add_argument("--mode", choices=("shadow", "live"), default="shadow")
    run.add_argument("--driver", choices=("live", "replay"), default="live")
    run.add_argument("--cassettes", default=str(Path(__file__).parent / "evals" / "cassettes"))
    run.add_argument("--tasks", type=int, default=12)
    run.add_argument("--checkpoint", action="append")

    act = sub.add_parser("activate", help="make a checkpoint live (refused unless its last verdict promoted)")
    act.add_argument("id")
    act.add_argument("--force", action="store_true", help="override the gate — for a checkpoint measured elsewhere")

    sub.add_parser("list", help="every registered checkpoint and its status")
    sub.add_parser("resolve", help="what LAYA_MODEL resolves to right now")

    quit_ = sub.add_parser("retire", help="stop using a checkpoint")
    quit_.add_argument("id")
    quit_.add_argument("--why", default="")

    args = parser.parse_args(argv)
    if args.command == "register":
        try:
            row = register(args.id, args.path, subfolder=args.subfolder, note=args.note)
        except ValueError as error:
            print(error, file=sys.stderr)
            return 1
        print(f"registered {row['id']} → {row['path']} ({row['status']})")
        return 0
    if args.command == "list":
        rows = all_rows()
        for row in rows:
            last = (row.get("evals") or [{}])[-1]
            print(f"{row.get('status','?'):9} {row['id']:24} {row.get('path','')}  {last.get('reason','no eval yet')}")
        if not rows:
            print("no checkpoints registered")
        return 0
    if args.command == "resolve":
        model, subfolder = resolve()
        print(f"LAYA_MODEL={model}  LAYA_MODEL_SUBFOLDER={subfolder}")
        return 0
    if args.command == "retire":
        print("retired" if retire(args.id, args.why) else f"no checkpoint {args.id!r}")
        return 0
    if args.command == "activate":
        print("activated" if activate(args.id, force=args.force) else "refused: its last verdict did not promote (use --force to override)")
        return 0

    from evals.runner import CassetteDriver, LiveDriver

    factory = (lambda: LiveDriver()) if args.driver == "live" else (lambda: CassetteDriver(args.cassettes))
    verdicts = cycle(driver_factory=factory, project=args.project, mode=args.mode, n_tasks=args.tasks, only=args.checkpoint)
    for verdict in verdicts:
        print(f"{'PROMOTE' if verdict.get('promote') else 'hold   '} {verdict.get('name')}: {verdict.get('reason')}")
    if not verdicts:
        print("no candidate checkpoints")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

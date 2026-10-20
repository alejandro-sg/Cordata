"""The promotion gate: a drafted lesson — or a prompt overlay — goes live only if the agent does measurably
better with it.

``evaluate`` runs the eval suite (``evals/``) twice on the same tasks with the same driver — once as the harness
is, once with the candidate in force for the run (a skill added to the skills the agent can read, or an overlay
appended to its system prompt) — and compares the paired results. A candidate **promotes** only if:

* nothing that passed without it fails with it (``broken == []``) — a skill that makes any task worse is out,
  however many it fixes, because a skill is prose in every future prompt and a regression is paid for everywhere;
* and something improved — a task newly passes, or the mean reward rises by at least ``MIN_REWARD_GAIN``;
* and the same holds on **held-out** tasks the candidate was not tuned against. (Tuning against the dev split is
  what the drafts' authors do, if only by accident; held-out is the check on that.)

``learning.promotion`` is the same shape as ``laya.mode``:

* ``off``     never evaluates by itself. ``python -m promote run`` still does, on request.
* ``shadow``  evaluates staged candidates and records the verdict on each (``meta.json``, ledger ``skill``/
              ``prompt`` ``eval``), and activates nothing. This is how you find out what the gate would have done.
* ``live``    a pass promotes: the skill moves to ``live/`` and loads into new sessions in its project; the
              overlay moves to ``live/`` and is appended to those sessions' system prompts.

A candidate that fails the gate twice is rejected, not re-run forever. Evaluation spends real model calls (two
runs of the sampled tasks, plus held-out on a pass); ``learning.evalTasks`` bounds it. What the driver is decides
what is measured: the ``oracle`` driver ignores skills (it replays the reference solution), so the gate is only
meaningful with ``--driver live`` (or a cassette set recorded with the skill present). Repeated runs smooth model
noise; with ``repeats=1`` a single flaky task can decide a verdict — the thresholds are deliberately strict about
regressions and lenient about gains for that reason.
"""

from __future__ import annotations

import argparse
import hashlib
import logging
import os
import tempfile
from contextlib import contextmanager
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any, Callable, Iterator

import learned
import prompts

logger = logging.getLogger(__name__)

#: A candidate is one of two kinds, judged identically against the same eval suite. A *skill* is read on demand
#: (the model sees its description and opens the body only when it applies); a *prompt* overlay is in front of the
#: model on every turn. The gate is the same because the risk is the same: prose a model may follow, drafted from
#: text that may be untrusted, live only after it has measurably beaten the baseline with nothing broken.
#: A third candidate kind, a fine-tuned Laya checkpoint, passes the same gate through ``arm_env`` instead of a
#: store — it has no body to filter, and only its weights can differ (`laya_checkpoints.py`).
KINDS = ("skill", "prompt")
#: A dispatch policy (orchestrator/dispatch.py) is judged by ``evaluate_dispatch`` on the orchestration suite instead:
#: it changes who does the work, not what the agent reads, so its evidence is pass rate *and* cost per success.

MIN_REWARD_GAIN = 0.05
DEFAULT_TASKS = 12
MAX_FAILED_EVALS = 2
_CODING = ("bugfix", "feature", "refactor", "config", "multifile", "tests")


@dataclass
class Verdict:
    name: str
    promote: bool
    reason: str
    fixed: list[str] = field(default_factory=list)
    broken: list[str] = field(default_factory=list)
    reward_delta: float | None = None
    tasks: int = 0
    heldout: dict[str, Any] | None = None
    driver: str = ""
    kind: str = "skill"
    #: What reflect.py's drafter attributed (rows, evidence, confidence, cue), carried into the eval record and the
    #: promotion evidence so a promoted skill says what it rests on.
    attribution: dict[str, Any] | None = None

    def to_json(self) -> dict[str, Any]:
        return asdict(self)


def _store(kind: str) -> Any:
    return prompts if kind == "prompt" else learned


def _staged(kind: str, project: str | os.PathLike | None) -> list[dict[str, Any]]:
    store = _store(kind)
    return store.list_prompts(project, "staging") if kind == "prompt" else store.list_skills(project, "staging")


@contextmanager
def _arm(*, skill_dir: str | None = None, prompt_overlay: str | None = None, env: dict[str, str] | None = None) -> Iterator[None]:
    """Put a candidate in force for an arm: ``CORDATA_EXTRA_SKILLS`` for a skill, ``CORDATA_PROMPT_OVERLAY``
    for an overlay, or an arbitrary ``env`` mapping for an arm with no store behind it (a Laya checkpoint, which
    is a directory rather than a body of prose — ``laya_checkpoints.py``). Restores every key on exit.
    (``run_suite`` sets the same variables from its ``config``, so an arm is in force whether or not the caller's
    runner honours ``config``.)"""
    keys = {"CORDATA_EXTRA_SKILLS": skill_dir, "CORDATA_PROMPT_OVERLAY": prompt_overlay, **(env or {})}
    saved = {k: os.environ.get(k) for k in keys}
    for key, value in keys.items():
        if value:
            os.environ[key] = str(value)
        else:
            os.environ.pop(key, None)
    try:
        yield
    finally:
        for key, value in saved.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value


def _materialize(kind: str, name: str, text: str, tmp: str) -> dict[str, str]:
    """Write the candidate where a session reads it, and return the ``config`` keys that put it in force."""
    if kind == "prompt":
        path = Path(tmp) / f"{name}.md"
        path.write_text(text.strip() + "\n")
        return {"prompt_overlay": str(path)}
    target = Path(tmp) / name
    target.mkdir()
    (target / "SKILL.md").write_text(text)
    return {"skill_dir": str(tmp)}


def _candidate(kind: str, name: str, project: str | os.PathLike | None) -> tuple[dict[str, Any] | None, str, list[str]]:
    """The staged candidate, its text, and the reasons (if any) it fails its own store's filter."""
    store = _store(kind)
    row = store.find(name, project, "staging")
    if row is None:
        return None, "", [f"no staged {kind} {name!r}"]
    text = store.read_text(row)
    if kind == "prompt":
        return row, text, store.validate_draft(name, text).problems
    return row, text, store.validate_draft(text, taken=store.all_names(project) - {name}).problems


def sample(tasks: list, n: int, seed: str) -> list:
    """``n`` coding tasks, spread across categories and stable for a given ``seed`` (the candidate's name), so
    the same candidate is always judged on the same tasks and a re-run is comparable."""
    pool = [t for t in tasks if t.category in _CODING]
    by_category: dict[str, list] = {}
    for task in sorted(pool, key=lambda t: hashlib.sha1(f"{seed}:{t.id}".encode()).hexdigest()):
        by_category.setdefault(task.category, []).append(task)
    picked: list = []
    while len(picked) < n and any(by_category.values()):
        for category in sorted(by_category):
            if by_category[category] and len(picked) < n:
                picked.append(by_category[category].pop(0))
    return picked


def _mean_reward(run: Any) -> float | None:
    values = [r.reward for r in run.results if r.reward is not None]
    return sum(values) / len(values) if values else None


def _arms(tasks: list, driver_factory: Callable[[], Any], arm: dict[str, str], run_suite: Callable[..., Any], baseline: Any | None, *, env: dict[str, str] | None = None) -> tuple[Any, Any]:
    with _arm():
        base = baseline if baseline is not None else run_suite(tasks, driver_factory(), config={"arm": "baseline"})
    with _arm(**arm, env=env):
        candidate = run_suite(tasks, driver_factory(), config={"arm": "candidate", **arm})
    return base, candidate


def _judge(base: Any, candidate: Any) -> tuple[list[str], list[str], float | None]:
    from evals.runner import compare

    diff = compare(base.to_json(), candidate.to_json())
    a, b = _mean_reward(base), _mean_reward(candidate)
    return diff["fixed"], diff["broken"], (round(b - a, 4) if a is not None and b is not None else None)


def evaluate(
    name: str,
    project: str | os.PathLike | None,
    *,
    kind: str = "skill",
    driver_factory: Callable[[], Any],
    n_tasks: int = DEFAULT_TASKS,
    baseline: Any | None = None,
    run_suite: Callable[..., Any] | None = None,
    catalog: list | None = None,
    arm_env: dict[str, str] | None = None,
) -> Verdict:
    """Evaluate one staged candidate (a skill or a prompt overlay). Does not move it; ``cycle`` (or a person)
    acts on the verdict. Never raises.

    ``arm_env`` is for a candidate that is not prose in a store: it puts the given environment in force for the
    candidate arm and reads no store, so there is nothing to filter or materialise (``laya_checkpoints.py``,
    whose candidate is a checkpoint directory). The caller is then the one that knows the candidate exists."""
    try:
        from evals.catalog import CATALOG
        from evals.runner import run_suite as default_run_suite

        run_suite = run_suite or default_run_suite
        if arm_env is None:
            row, text, problems = _candidate(kind, name, project)
            if row is None:
                return Verdict(name, False, "; ".join(problems), kind=kind)
            if problems:
                return Verdict(name, False, "fails the filter: " + "; ".join(problems), kind=kind)
        tasks_all = list(catalog if catalog is not None else CATALOG)
        dev = sample([t for t in tasks_all if t.split == "dev"], n_tasks, name)
        held = sample([t for t in tasks_all if t.split == "heldout"], max(3, n_tasks // 2), name)
        if not dev:
            return Verdict(name, False, "no tasks to evaluate on", kind=kind)
        with tempfile.TemporaryDirectory(prefix=f"candidate-{kind}-") as tmp:
            arm = {} if arm_env is not None else _materialize(kind, name, text, tmp)
            base, cand = _arms(dev, driver_factory, arm, run_suite, baseline, env=arm_env)
            fixed, broken, delta = _judge(base, cand)
            verdict = Verdict(name, False, "", fixed, broken, delta, len(dev), driver=cand.driver, kind=kind)
            if broken:
                verdict.reason = f"made {len(broken)} task(s) that passed without it fail: {', '.join(broken[:4])}"
                return verdict
            if not fixed and not (delta is not None and delta >= MIN_REWARD_GAIN):
                verdict.reason = "no improvement over the baseline" + (f" (reward {delta:+.3f})" if delta is not None else "")
                return verdict
            held_base, held_cand = _arms(held, driver_factory, arm, run_suite, None, env=arm_env)
            h_fixed, h_broken, h_delta = _judge(held_base, held_cand)
            verdict.heldout = {"tasks": len(held), "fixed": h_fixed, "broken": h_broken, "reward_delta": h_delta}
            if h_broken:
                verdict.reason = f"improved the dev tasks but broke held-out ones: {', '.join(h_broken[:4])}"
                return verdict
            verdict.promote = True
            verdict.reason = f"fixed {len(fixed)} task(s), broke none" + (f", reward {delta:+.3f}" if delta is not None else "") + "; held-out clean"
            return verdict
    except Exception as error:
        logger.debug("promote: evaluation failed", exc_info=True)
        return Verdict(name, False, f"evaluation error: {type(error).__name__}: {error}", kind=kind)


def cycle(
    project: str | os.PathLike,
    mode: str,
    *,
    kind: str = "skill",
    driver_factory: Callable[[], Any],
    n_tasks: int = DEFAULT_TASKS,
    run_suite: Callable[..., Any] | None = None,
    catalog: list | None = None,
    only: list[str] | None = None,
) -> list[Verdict]:
    """Evaluate every staged candidate of ``kind`` for ``project`` and act on the verdicts according to ``mode``
    (``off`` evaluates nothing; ``shadow`` records; ``live`` records and promotes passes)."""
    if mode not in ("shadow", "live"):
        return []
    store = _store(kind)
    verdicts: list[Verdict] = []
    baseline = None
    for row in _staged(kind, project):
        if only and row["name"] not in only:
            continue
        verdict = evaluate(row["name"], project, kind=kind, driver_factory=driver_factory, n_tasks=n_tasks, baseline=baseline, run_suite=run_suite, catalog=catalog)
        verdict.attribution = row.get("attribution")
        verdicts.append(verdict)
        store.record_eval(row["name"], project, verdict.to_json())
        if verdict.promote and mode == "live":
            store.promote(row["name"], project, evidence=verdict.to_json(), by="gate")
        elif not verdict.promote:
            failed = [e for e in (store.find(row["name"], project, "staging") or {}).get("evals", []) if not e.get("promote")]
            if len(failed) >= MAX_FAILED_EVALS:
                store.reject(row["name"], project, f"failed the gate {len(failed)} times: {verdict.reason}", by="gate")
    return verdicts


#: A dispatch policy may cost at most this much pass rate against the baseline (the policy's own MIN_TOLERANCE).
DISPATCH_PASS_TOLERANCE = 0.01


def judge_dispatch(base: dict[str, Any], cand: dict[str, Any], *, tolerance: float = DISPATCH_PASS_TOLERANCE) -> tuple[bool, str]:
    """One split's verdict for a dispatch policy, from two ``SuiteResult.aggregate`` blocks: no more than ``tolerance``
    pass rate lost, cheaper per success, no worse at surviving handoffs, and no less plan-conformant (a policy that
    picks workers which write outside their scope is out, whatever it saves)."""
    if not base.get("tasks") or not cand.get("tasks"):
        return False, "no tasks"
    if (cand["pass_rate"] or 0.0) < (base["pass_rate"] or 0.0) - tolerance:
        return False, f"pass rate {cand['pass_rate']} < baseline {base['pass_rate']} - {tolerance}"
    if base.get("plan_conformance") is not None and (cand.get("plan_conformance") or 0.0) < base["plan_conformance"]:
        return False, f"plan conformance {cand.get('plan_conformance')} < baseline {base['plan_conformance']}"
    if base.get("handoff_survival") is not None and cand.get("handoff_survival") is not None and cand["handoff_survival"] < base["handoff_survival"] - tolerance:
        return False, f"handoff survival {cand['handoff_survival']} < baseline {base['handoff_survival']}"
    if cand.get("cost_per_success") is None or base.get("cost_per_success") is None or cand["cost_per_success"] >= base["cost_per_success"]:
        return False, f"no cheaper per success ({cand.get('cost_per_success')} vs {base.get('cost_per_success')})"
    return True, f"pass {cand['pass_rate']} vs {base['pass_rate']}, cost/success {cand['cost_per_success']} vs {base['cost_per_success']}"


def evaluate_dispatch(
    *,
    candidate: str = "learned",
    baseline: str = "strongest",
    driver: str = "sim",
    world: str = "steep",
    table: Any = None,
    limit: int | None = None,
    task_ids: list[str] | None = None,
    run: Callable[..., Any] | None = None,
) -> Verdict:
    """The gate a dispatch policy passes before ``orchestrator.dispatch`` may be ``live``: the orchestration eval suite
    (``orchestrator/evalsuite.py``) run under the baseline and the candidate on the **dev** tasks, then on the
    **held-out** ones; both must pass ``judge_dispatch``. Never raises."""
    import asyncio

    try:
        from orchestrator import evalsuite

        runner = run or evalsuite.run_suite
        verdict = Verdict(f"dispatch:{candidate}", False, "", driver=driver, kind="dispatch")
        blocks: dict[str, Any] = {}
        for split in ("dev", "heldout"):
            base = asyncio.run(runner(policy=baseline, split=split, driver=driver, world=world, limit=limit, task_ids=task_ids))
            cand = asyncio.run(runner(policy=candidate, split=split, driver=driver, world=world, table=table, limit=limit, task_ids=task_ids))
            blocks[split] = {"baseline": base.aggregate(split), "candidate": cand.aggregate(split)}
            ok, why = judge_dispatch(blocks[split]["baseline"], blocks[split]["candidate"])
            verdict.tasks += blocks[split]["candidate"]["tasks"]
            if not ok:
                verdict.reason = f"{split}: {why}"
                verdict.heldout = blocks
                return verdict
        verdict.promote = True
        verdict.reason = "dev and held-out: " + judge_dispatch(blocks["heldout"]["baseline"], blocks["heldout"]["candidate"])[1]
        verdict.heldout = blocks
        return verdict
    except Exception as error:
        logger.debug("promote: dispatch evaluation failed", exc_info=True)
        return Verdict(f"dispatch:{candidate}", False, f"evaluation error: {type(error).__name__}: {error}", kind="dispatch")


def main(argv: list[str] | None = None) -> int:
    from evals.runner import CassetteDriver, LiveDriver

    if argv is None:
        import sys

        argv = sys.argv[1:]
    if argv[:1] == ["dispatch"]:
        parser = argparse.ArgumentParser(prog="promote dispatch", description="Gate a dispatch policy on the orchestration eval suite.")
        parser.add_argument("--candidate", default="learned", choices=("learned", "random", "cheapest"))
        parser.add_argument("--driver", default="sim", choices=("sim", "live"))
        parser.add_argument("--world", default="steep", choices=("steep", "ceiling"), help="sim driver only: which quality table is true")
        parser.add_argument("--limit", type=int)
        args = parser.parse_args(argv[1:])
        verdict = evaluate_dispatch(candidate=args.candidate, driver=args.driver, world=args.world, limit=args.limit)
        print(f"{'PROMOTE' if verdict.promote else 'hold   '} {verdict.name}: {verdict.reason}")
        return 0

    parser = argparse.ArgumentParser(prog="promote", description="Evaluate staged learned skills and prompt overlays against the eval suite.")
    parser.add_argument("--project", default=os.getcwd())
    parser.add_argument("--mode", choices=("shadow", "live"), default="shadow", help="shadow records verdicts; live also promotes passes")
    parser.add_argument("--kind", choices=KINDS, default="skill")
    parser.add_argument("--driver", choices=("live", "replay"), default="live")
    parser.add_argument("--cassettes", default=str(Path(__file__).parent / "evals" / "cassettes"))
    parser.add_argument("--tasks", type=int, default=DEFAULT_TASKS)
    parser.add_argument("--skill", action="append", help="only this staged candidate (repeatable)")
    args = parser.parse_args(argv)
    factory = (lambda: LiveDriver()) if args.driver == "live" else (lambda: CassetteDriver(args.cassettes))
    verdicts = cycle(args.project, args.mode, kind=args.kind, driver_factory=factory, n_tasks=args.tasks, only=args.skill)
    for v in verdicts:
        print(f"{'PROMOTE' if v.promote else 'hold   '} {v.name}: {v.reason}")
    if not verdicts:
        print("nothing staged for this project" if args.mode else "")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

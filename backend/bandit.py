"""The contextual bandit: which harness choice to make, learned from outcomes.

Everything else in the learning loop learns *prose* — a lesson (`reflect.py`), a skill, a prompt — and is judged
by the eval suite before it counts. This module learns something narrower and cheaper: **which of the discrete
choices the harness already makes** should be made, next time, in a context like this one.

The choices (*points*), each already made somewhere in the harness by a rule, a threshold or Laya's reading:

    route    the action for this turn — dispatch · hint · none — among the ones the base policy already allows
    engine   hand the work to the Claude Code CLI, or do it with local tools
    ask      ask a person about this call, or not

Three more decision points are shaped the same way but are **not acted on in v1**, because neither has a cheap
per-turn seam to act through (a delegated model is built once per session, and the local role table once up front):

    tier     which model tier runs a delegated task — fast · standard · strong
    effort   the reasoning effort for a local role — low · medium · high · max

They are the phase-3 seam: `choose`'s signature is what a later scorer replaces, and adding them is adding a call
site, not a change here.

It is a **bandit**, not a policy: it does not decide what is *allowed*, only which of a set of already-permitted
options tends to turn out better here. The reward is the turn's own outcome (`outcomes.score`), re-weighted so the
three things the roadmap names are visible: the verifier's verdict, the turn's cost, and the person's correction.

Two properties are load-bearing:

* **Additive only.** A learned choice may *add* an ask and may re-rank options that are already on the table. It
  can never remove a prompt, lower a Guard score, or reach a denial: those decisions are made in code that does not
  read this module, and this module deliberately does not import `laya_guard`, `laya_risk` or `secret_guard`. The
  one read inside the approval handler ORs a term into `armed` (`acp_server.py`); there is no branch that
  subtracts. `_bandit_smoke_test.py` asserts this by introspecting this file's imports.
* **Cold it is not there.** `off`, no posteriors, fewer pulls than `learning.banditMinPulls`, or an unreconciled
  turn all return the caller's default, so with the feature off the harness behaves exactly as it did before it
  existed. `shadow` records what it *would* have chosen and acts on nothing.

The statistics are deliberately coarse — a per-context table of `{n, wins, mean, cost, correction}` per arm, keyed
by a context small enough to fill up (mode × risk band × tier × target kind) — because a user's own turns are few.
Phase 3 (`trajectory.py`) replaces the scorer behind `choose` with a model once there is enough data; the shape of
`choose` is the seam.

Stdlib plus `outcomes` and `state_store`; no model, no Strands.
"""

from __future__ import annotations

import hashlib
import json
import logging
import os
import random
import threading
from pathlib import Path
from typing import Any

import outcomes
import state_store

logger = logging.getLogger(__name__)

VERSION = 1
MODES = ("off", "shadow", "live")
#: The points acted on in v1. `tier` and `effort` are reserved for phase 3 (see the module docstring) — a stored
#: context for them is harmless and simply never read, so this is a record of intent, not a validation gate.
POINTS = ("route", "engine", "ask")

#: How many turns of history the context table is rebuilt from, and how many turn ids are remembered (so a
#: reconcile pass over the same ledger cannot count a turn twice).
LEARN_WINDOW = 200
MAX_SEEN = 500
#: Pseudo-counts: `wins` is pulled toward 0.5 with this weight, so one lucky turn cannot move a choice.
PRIOR = 2.0
#: A claude_code run's dollars are a cost the token/time budgets do not see. $2 is "a lot of one turn".
CLAUDE_BUDGET_USD = 2.0


# ------------------------------------------------------------------------------ reward


def reward(record: "outcomes.TurnRecord | dict[str, Any]", update: dict[str, Any] | None = None, *, lam: float = 1.0, mu: float = 1.0) -> float | None:
    """What one turn is worth to the bandit: *verifier success − λ·cost − μ·human correction*, in [−1, 1].

    Built from ``outcomes.score``'s parts rather than a second reward, so the number a bandit optimises and the
    number a person reads in ``outcomes report`` cannot drift apart. ``None`` until the turn has been reconciled
    (``Reward.known``): the human-correction half is not knowable before then, and a turn scored on half its
    signal is worse than one not scored yet.
    """
    scored = outcomes.score(record, update)
    if not scored.known:
        return None
    parts = scored.parts
    success = parts.get("verified", 0.0) + parts.get("broken", 0.0)  # +0.45 pass / −0.45 fail
    kept = parts.get("kept", 0.0) + parts.get("reverted", 0.0)  # +0.25 kept / −0.40 put back
    cost = _cost(record)
    correction = _correction(record, update)
    value = success + kept - lam * cost - mu * correction
    return round(max(-1.0, min(1.0, value)), 4)


def _cost(record: Any) -> float:
    """The turn's cost in [0, 1]: tokens against the reward's budget, time likewise, and Claude Code's dollars —
    which are real money the token count does not show."""
    usage = _get(record, "usage") or {}
    seconds = _get(record, "seconds") or 0.0
    claude = _get(record, "claude_code") or {}
    parts = [
        min(1.0, int(usage.get("total", 0) or 0) / outcomes.TOKEN_BUDGET),
        min(1.0, float(seconds) / outcomes.SECONDS_BUDGET),
        min(1.0, float(claude.get("cost_usd", 0.0) or 0.0) / CLAUDE_BUDGET_USD),
    ]
    return sum(parts) / len(parts)


def _correction(record: Any, update: dict[str, Any] | None) -> float:
    """The person's correction in [0, 1]: approvals refused, changes put back, the turn stopped. Each is one
    third, so a turn that annoyed the person twice and a turn that annoyed them three times are both "very"."""
    approvals = _get(record, "approvals") or {}
    rejected = int(approvals.get("rejected", 0) or 0)
    reverted = int((update or {}).get("reverted", 0) or 0)
    cancelled = 1 if _get(record, "stop_reason") == "cancelled" else 0
    return min(1.0, (rejected + reverted + cancelled) / 3)


def _get(record: Any, name: str) -> Any:
    return record.get(name) if isinstance(record, dict) else getattr(record, name, None)


# ------------------------------------------------------------------------------ context


def context(record: Any, point: str) -> str:
    """The context a choice was made in — coarse on purpose, so a user's few turns still fill a bucket.

    ``mode | point | risk band | tier | target kind``. Every field is already on the turn's route record, so the
    context is derived from the ledger and needs no separate capture at decision time.
    """
    route = _get(record, "route") or {}
    mode = str(_get(record, "mode") or "build")
    risk = str(route.get("risk") or "none")
    tier = str(route.get("tier") or "none")
    kind = target_kind(route.get("target"))
    return "|".join((mode, point, risk, tier, kind))


def target_kind(target: Any) -> str:
    """``specialist`` · ``tool`` · ``orchestrator`` — from the target id alone, so this does not import the router."""
    name = str(target or "")
    if name in ("architect", "debugger"):
        return "specialist"
    if name in ("claude_code",):
        return "tool"
    if not name:
        return "none"
    return "orchestrator"


# ------------------------------------------------------------------------------ storage


def home() -> Path:
    return Path(os.environ.get("SIERRALAB_HOME") or Path.home() / ".sierralab") / "bandit"


def project_key(project: str | os.PathLike | None) -> str:
    if project is None:
        return "_global"
    try:
        real = os.path.realpath(str(project))
    except OSError:
        real = str(project)
    return hashlib.sha1(real.encode()).hexdigest()[:16]


class Bandit:
    """One project's context table. Thread-safe, atomic, and never raises into a turn.

    Holds no policy of its own: ``mode``, ``lam``, ``mu`` and the exploration/min-pull knobs are passed by the
    caller (which reads them from settings), so this class cannot decide anything a caller did not hand it.
    """

    def __init__(self, project: str | os.PathLike | None = None, path: str | os.PathLike | None = None) -> None:
        self.project = project
        if path is not None:
            self.path = Path(path)
        else:
            self.path = home() / f"{project_key(project)}.json"
        self._lock = threading.RLock()
        self.contexts: dict[str, dict[str, Any]] = {}
        self.seen: list[str] = []
        self._seen: set[str] = set()
        self._load()

    # ---------------------------------------------------------------- storage

    def _load(self) -> None:
        try:
            data = json.loads(self.path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return
        if isinstance(data, dict) and int(data.get("version", 0)) == VERSION:
            self.contexts = {k: v for k, v in (data.get("contexts") or {}).items() if isinstance(v, dict)}
            self.seen = [str(s) for s in (data.get("seen") or [])][-MAX_SEEN:]
            self._seen = set(self.seen)

    def save(self) -> None:
        with self._lock:
            payload = json.dumps({"version": VERSION, "seen": self.seen[-MAX_SEEN:], "contexts": self.contexts}, indent=1, sort_keys=True)
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            os.chmod(self.path.parent, 0o700)
            tmp = self.path.with_suffix(".tmp")
            tmp.write_text(payload, encoding="utf-8")
            os.replace(tmp, self.path)
        except OSError:
            logger.debug("bandit: could not save %s", self.path, exc_info=True)

    def _entry(self, ctx: str) -> dict[str, Any]:
        entry = self.contexts.get(ctx)
        if entry is None:
            entry = self.contexts[ctx] = {"arms": {}}
        return entry

    # ---------------------------------------------------------------- choosing

    def choose(
        self,
        point: str,
        ctx: str,
        arms: list[str],
        default: str,
        *,
        mode: str = "off",
        explore: float = 0.1,
        min_pulls: int = 5,
        lam: float = 1.0,
        mu: float = 1.0,
        rng: random.Random | None = None,
    ) -> tuple[str, dict[str, Any]]:
        """Which arm to take. Returns the arm and an info dict for the ledger/turn record.

        The default wins whenever the bandit has not earned the right to move: mode ``off``, an unknown default,
        no data, or any arm below ``min_pulls``. ``shadow`` reports the arm it would have taken and still returns
        the default. Only ``live`` ever returns something else.
        """
        info: dict[str, Any] = {"point": point, "context": ctx, "default": default, "arm": default, "acted": False, "mode": mode, "propensity": 1.0, "scores": {}}
        if mode not in ("shadow", "live") or default not in arms or not arms:
            return default, info
        entry = self.contexts.get(ctx)
        if not isinstance(entry, dict):
            return default, info
        stats: dict[str, Any] = entry.get("arms") or {}
        if any(int(stats.get(a, {}).get("n", 0) or 0) < int(min_pulls) for a in arms):
            return default, info
        info["scores"] = {a: round(self._score(stats.get(a), lam=lam, mu=mu), 4) for a in arms}
        best = max(arms, key=lambda a: (self._score(stats.get(a), lam=lam, mu=mu), a == default))
        info["arm"] = best
        if mode == "shadow":
            return default, info
        picker = rng or random
        arm = best
        if len(arms) > 1 and float(explore) > 0.0 and picker.random() < float(explore):
            others = [a for a in arms if a != best]
            arm = others[picker.randrange(len(others))]
        info["arm"], info["acted"] = arm, arm != default
        info["propensity"] = round((1.0 - float(explore)) + float(explore) / max(1, len(arms)), 4)
        return arm, info

    @staticmethod
    def _score(stat: dict[str, Any] | None, *, lam: float, mu: float) -> float:
        stat = stat or {}
        n = int(stat.get("n", 0) or 0)
        wins = float(stat.get("wins", 0.0) or 0.0)
        return (wins + PRIOR * 0.5) / (n + PRIOR) - lam * float(stat.get("cost", 0.0) or 0.0) - mu * float(stat.get("correction", 0.0) or 0.0)

    # ---------------------------------------------------------------- learning

    def observe(self, record: Any, update: dict[str, Any] | None, *, lam: float = 1.0, mu: float = 1.0) -> bool:
        """Learn from one reconciled turn. ``False`` when there is nothing to learn (no choices recorded, or the
        turn is not reconciled yet). Idempotent per turn id."""
        choices = (_get(record, "route") or {}).get("choices") or {}
        if not isinstance(choices, dict) or not choices:
            return False
        turn = str(_get(record, "id") or "")
        value = reward(record, update, lam=lam, mu=mu)
        if value is None:
            return False
        if turn:
            with self._lock:
                if turn in self._seen:
                    return False
                self._seen.add(turn)
                self.seen = [*self.seen, turn][-MAX_SEEN:]
        for point, arm in choices.items():
            if arm is None:
                continue  # the point was not exercised this turn (e.g. no delegated task)
            ctx = context(record, str(point))
            with self._lock:
                stats = self._entry(ctx)["arms"].setdefault(str(arm), {"n": 0, "wins": 0.0, "mean": 0.0, "cost": 0.0, "correction": 0.0})
                n = int(stats.get("n", 0) or 0) + 1
                stats["n"] = n
                stats["wins"] = round(float(stats.get("wins", 0.0) or 0.0) + (1.0 if value > 0 else 0.0), 4)
                stats["mean"] = round(_running(stats.get("mean", 0.0), value, n), 4)
                stats["cost"] = round(_running(stats.get("cost", 0.0), _cost(record), n), 4)
                stats["correction"] = round(_running(stats.get("correction", 0.0), _correction(record, update), n), 4)
            _event("learn", turn, project=self.project, point=str(point), arm=str(arm), reward=value)
        self.save()
        return True

    def learn(self, project: str | os.PathLike | None = None, *, lam: float = 1.0, mu: float = 1.0, limit: int = LEARN_WINDOW) -> int:
        """Fold every reconciled turn in the project's ledger into the table. Returns how many it learned from.

        The whole history is re-walked each time; ``seen`` makes that free after the first pass, so a reconcile
        that lands later (`outcome_update`) is picked up on the next run without a cursor to keep.
        """
        learned = 0
        try:
            for record, update in outcomes.recent(limit, project=str(project) if project is not None else None):
                if self.observe(record, update, lam=lam, mu=mu):
                    learned += 1
        except Exception:
            logger.debug("bandit: could not learn from the ledger", exc_info=True)
        return learned


def _running(previous: Any, value: float, n: int) -> float:
    return float(previous or 0.0) + (float(value) - float(previous or 0.0)) / max(1, n)


#: One instance per store path, so a turn and a background reconcile share the same table and lock.
_INSTANCES: dict[str, Bandit] = {}
_INSTANCE_LOCK = threading.Lock()


def for_project(project: str | os.PathLike | None) -> Bandit:
    path = home() / f"{project_key(project)}.json"
    key = str(path)
    with _INSTANCE_LOCK:
        instance = _INSTANCES.get(key)
        if instance is None:
            instance = _INSTANCES[key] = Bandit(project, path=path)
        return instance


def _event(action: str, turn: str, project: Any = None, **extra: Any) -> None:
    try:
        state_store.append_event("bandit", {"action": action, "turn": turn, "project": os.path.realpath(str(project)) if project else None, **extra}, source="bandit")
    except Exception:
        logger.debug("bandit: could not write the ledger", exc_info=True)


def decision_event(info: dict[str, Any], *, turn: str | None = None, project: Any = None) -> None:
    """One audit row per choice the bandit was asked for, whether or not it acted: what it saw, what it would have
    taken, and what the caller used. ``shadow`` mode's whole output.

    ``propensity`` is kept because it cannot be reconstructed afterwards: the exploration rate that was in force
    when the choice was made is what makes the row usable as an off-policy example (`trajectory.py`), and a
    propensity read off today's settings would be a different number."""
    _event("decide", str(turn or ""), project=project, **{k: info.get(k) for k in ("point", "context", "default", "arm", "acted", "mode", "propensity")}, scores=info.get("scores"))

"""The ACP server's learning hooks: the bandit's per-session policy and route arms, the outcome baseline, and the
background maintenance (bandit learning, prompt optimisation) a finished turn may kick off.

Split out of ``acp_server.py``, which re-exports every name here.
"""

import logging
import os
import checkpoints
import bandit
import outcomes
import optimize
import settings_store
from laya_router import RouteDecision

def _baseline_reward(cwd: str, exclude: str = "") -> float | None:
    """The mean reward of the project's recent turns that read no learned skill — what a skill's own turns are
    compared against."""
    scores = []
    for record, update in outcomes.recent(40, project=cwd):
        if record.id == exclude or record.skills:
            continue
        scores.append(outcomes.score(record, update).value)
    return sum(scores) / len(scores) if len(scores) >= 3 else None


def _bandit_policy(preferences) -> dict:
    """The bandit's knobs, read once per turn from settings. ``off`` unless a person turned it on; the bandit
    itself holds no policy and cannot decide anything these do not hand it."""
    return {
        "mode": str(preferences.get("learning.bandit", "off") or "off"),
        "explore": float(preferences.get("learning.banditExplore", 0.1) or 0.0),
        "min_pulls": int(float(preferences.get("learning.banditMinPulls", 5) or 5)),
        "lam": float(preferences.get("learning.banditLambda", 1.0) or 0.0),
        "mu": float(preferences.get("learning.banditMu", 1.0) or 0.0),
    }


def _bandit_route_arms(decision: "RouteDecision") -> list[str]:
    """The actions the bandit may choose between for this turn — never more than the base policy already allowed.

    A dispatch may be declined (the orchestrator handles the turn instead); a hint may be dropped; nothing is
    offered that would *add* an action the policy did not produce. An abstention is left alone — fail-closed is
    not the bandit's to reopen — and a ``kind="tool"`` target is the ``engine`` point's job, not this one's.
    """
    if decision.abstained or decision.target is None:
        return []
    if decision.action == "dispatch":
        return ["dispatch", "hint"]
    if decision.action == "hint":
        return ["hint", "none"]
    if decision.action == "none" and decision.hint():
        return ["none", "hint"]
    return []


def _bandit_stub(mode: str, decision: "RouteDecision") -> dict:
    """The ledger-shaped skeleton the bandit derives a context from — the same fields a finished turn record
    carries, so the context a choice is made in is the context it is later learned from."""
    return {"mode": mode, "route": {"target": decision.target.id if decision.target else None, "risk": decision.risk, "tier": decision.tier()}}


def _maintain(cwd: str) -> None:
    """Off the turn's path: judge earlier turns' changes, keep the snapshot history bounded, and fold whatever
    the judging has now made learnable into the bandit's table (a turn only becomes learnable once reconciled)."""
    outcomes.reconcile(cwd)
    checkpoints.prune(cwd)
    _bandit_learn(cwd)


def _bandit_learn(cwd: str) -> None:
    """Rebuild the bandit's table from the project's ledger. A no-op with the bandit off; never raises."""
    try:
        preferences = settings_store.current(cwd)
        policy = _bandit_policy(preferences)
        if policy["mode"] == "off":
            return
        bandit.for_project(cwd).learn(cwd, lam=policy["lam"], mu=policy["mu"])
    except Exception:
        logging.getLogger(__name__).debug("bandit: learning pass failed", exc_info=True)


def _optimize_artifacts(cwd: str, preferences: dict) -> None:
    """Draft a sharper variant of a live lesson or prompt overlay whose turns went badly (``optimize.py``). A
    no-op unless ``learning.optimizePrompts`` is on; never raises. Also off the turn's path, and off during an eval
    run (``SIERRALAB_REFLECT=off``), where a scripted turn is not evidence about anything."""
    try:
        if preferences.get("learning.optimizePrompts", False) is not True:
            return
        if os.environ.get("SIERRALAB_REFLECT", "").lower() == "off":
            return
        for kind in ("skill", "prompt"):
            optimize.cycle(cwd, kind=kind, preferences=preferences)
    except Exception:
        logging.getLogger(__name__).debug("optimize: drafting pass failed", exc_info=True)


def _note_answer(session_id: str, tool_name: str, approved: bool, key: str) -> None:
    """The person's answer to an approval, for the turn's outcome record."""
    recorder = outcomes.active(session_id)
    if recorder is not None:
        recorder.approval("approved" if approved else "rejected", tool_name, key=key)


def _laya_loaded() -> bool:
    """Whether Laya's weights are in memory — asked, never triggered: a turn must not wait on a model load."""
    from prompt_security import laya_ready

    return laya_ready()


def _route_of(router_hooks, agent) -> str | None:
    """The id of the route Laya chose for the turn in progress ("web", "code", …), or None."""
    decision = router_hooks.current(agent)
    target = getattr(decision, "target", None)
    return getattr(target, "id", None)

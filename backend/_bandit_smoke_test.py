"""The contextual bandit: which harness choice to make, learned from outcomes — and what it may never do.

Pure and fast — a temp ledger, a temp store, no model, no network. What this pins down:

* **cold it is not there**: ``off``, a context with no posteriors, and any arm below ``minPulls`` all return the
  caller's default, so every point is a no-op until a posterior has earned the right to move;
* **shadow acts on nothing** and records what it would have taken; **live** flips to the better arm once pulls
  accumulate, and the reward actually responds to λ (cost) and μ (correction) and clips to [−1, 1];
* **an unreconciled turn teaches nothing** — a turn with no ``outcome_update`` is skipped, and becomes usable the
  moment the update lands, without counting twice;
* **it cannot lower the Guard floor**: ``bandit`` imports neither ``laya_guard`` nor ``laya_risk`` nor
  ``secret_guard``; the one approval read is an OR-term that may only *add* a Confirm; a call below the gate still
  Proceeds however loudly a hand-made posterior recommends asking; an abstained decision is left untouched; and
  the module exposes no waive / skip / lower API at all;
* **the ledger is the trajectory**: ``choices`` ride on the turn's route, learning is idempotent, the store is
  written atomically, and ``kind="bandit"`` audit rows are written for every choice.

    backend/.venv/bin/python _bandit_smoke_test.py
"""

import ast
import json
import os
import sys
import tempfile
from pathlib import Path
from types import SimpleNamespace

_TMP = tempfile.TemporaryDirectory()
ROOT = Path(_TMP.name).resolve()
os.environ["SIERRALAB_HOME"] = str(ROOT / "home")
os.environ["SIERRALAB_STATE_DB"] = str(ROOT / "state.db")

sys.path.insert(0, str(Path(__file__).resolve().parent))

import bandit as bd  # noqa: E402
import outcomes as oc  # noqa: E402
import state_store  # noqa: E402
from harness_testing import check  # noqa: E402

PROJECT = str(ROOT / "project")
Path(PROJECT).mkdir()


# --------------------------------------------------------------------------- fixtures


def record(turn: str, *, action: str = "hint", risk: str | None = None, tier: str | None = None, target: str | None = None,
           verified: bool | None = True, seconds: float = 0.0, tokens: int = 0, usd: float = 0.0,
           rejected: int = 0, cancelled: bool = False, choices: dict | None = None) -> oc.TurnRecord:
    """One turn as the ledger holds it — the fields the bandit's reward and context read, and nothing else."""
    verifier = {} if verified is None else {"after_last_edit": "pass" if verified else "fail"}
    return oc.TurnRecord(
        id=turn, session_id="s", project=PROJECT, ts=0.0, seconds=seconds, goal="g", mode="build",
        stop_reason="cancelled" if cancelled else "end_turn",
        route={"target": target, "action": action, "risk": risk, "tier": tier, "choices": choices if choices is not None else {"route": "hint"}},
        verifier=verifier, approvals={"rejected": rejected} if rejected else {},
        usage={"total": tokens} if tokens else {}, claude_code={"cost_usd": usd} if usd else {},
    )


def fresh_store(name: str) -> bd.Bandit:
    return bd.Bandit(PROJECT, path=ROOT / f"{name}.json")


# --------------------------------------------------------------------------- reward


def test_reward() -> None:
    judged = {"kept": 1, "reverted": 0, "edited": 0, "removed": 0, "files": 1}
    good = record("t-good", verified=True, seconds=0.0)
    check(bd.reward(good, judged) is not None, "a reconciled turn has a reward")
    check(bd.reward(good, None) is None and bd.reward(good, {}) is None, "an unreconciled turn has none — learning waits for the outcome_update")

    bad = record("t-bad", verified=False)
    check(bd.reward(bad, judged) < bd.reward(good, judged), "a failed verifier scores below a passed one")

    # λ is cost: a costly turn scores lower than the same turn cheap, and more λ hurts more.
    cheap, dear = record("t-cheap", seconds=0.0, tokens=0), record("t-dear", seconds=590.0, tokens=199_000)
    check(bd.reward(dear, judged, lam=1.0) < bd.reward(cheap, judged, lam=1.0), "cost lowers the reward")
    check(bd.reward(record("t-usd", usd=2.0), judged, lam=1.0) < bd.reward(record("t-nousd", usd=0.0), judged, lam=1.0), "Claude Code dollars count as cost")
    check(bd.reward(dear, judged, lam=0.0) > bd.reward(dear, judged, lam=1.0), "λ=0 switches cost off")

    # μ is correction: refusals, reverts and a cancel each push the reward down.
    check(bd.reward(record("t-rej", rejected=3), judged, mu=1.0) < bd.reward(record("t-rej", rejected=3), judged, mu=0.0), "μ=0 switches correction off")
    check(bd.reward(record("t-cancel", cancelled=True), judged, mu=1.0) < bd.reward(record("t-cancel"), judged, mu=1.0), "a cancelled turn is a correction")
    check(bd.reward(record("t-rev"), {"kept": 0, "reverted": 4, "edited": 0, "removed": 0}, mu=1.0) < bd.reward(record("t-rev"), {"kept": 0, "reverted": 0, "edited": 0, "removed": 0}, mu=1.0), "reverted files are a correction")

    for turn, rw in (("a", bd.reward(record("x1", verified=True), judged)), ("b", bd.reward(record("x2", verified=False, cancelled=True, rejected=3, seconds=600), judged, lam=1.0, mu=1.0))):
        check(-1.0 <= rw <= 1.0, f"reward {turn} is clipped to [−1, 1] ({rw})")


def test_context() -> None:
    ctx = bd.context(record("t", risk="destructive", tier="strong", target="architect"), "route")
    check(ctx == "build|route|destructive|strong|specialist", f"context is mode|point|risk|tier|kind ({ctx})")
    check(bd.target_kind("claude_code") == "tool" and bd.target_kind(None) == "none" and bd.target_kind("team") == "orchestrator", "target kind from the id alone")


# --------------------------------------------------------------------------- choosing


def test_cold_is_a_no_op() -> None:
    table = fresh_store("cold")
    for point in bd.POINTS:
        arm, info = table.choose(point, f"build|{point}|none|none|none", ["a", "b"], "a", mode="live", min_pulls=1)
        check(arm == "a" and not info["acted"], f"{point}: no data, so the default stands")
    arm, info = table.choose("route", "ctx", ["a", "b"], "a", mode="off")
    check(arm == "a" and not info["acted"], "off is a no-op")
    arm, info = table.choose("route", "ctx", ["a", "b"], "c", mode="live")
    check(arm == "c", "a default outside the arm set is returned untouched")


def test_shadow_records_but_acts_nothing() -> None:
    table = fresh_store("shadow")
    ctx = "build|route|none|none|none"
    for _ in range(10):
        table.contexts.setdefault(ctx, {"arms": {}})["arms"]["hint"] = {"n": 10, "wins": 9.0, "mean": 0.5, "cost": 0.0, "correction": 0.0}
        table.contexts[ctx]["arms"]["dispatch"] = {"n": 10, "wins": 1.0, "mean": -0.5, "cost": 0.0, "correction": 0.0}
    arm, info = table.choose("route", ctx, ["dispatch", "hint"], "dispatch", mode="shadow", min_pulls=5)
    check(arm == "dispatch" and info["arm"] == "hint" and not info["acted"], "shadow returns the default and reports the better arm")
    arm, info = table.choose("route", ctx, ["dispatch", "hint"], "dispatch", mode="live", min_pulls=5, explore=0.0)
    check(arm == "hint" and info["acted"], "live takes the better arm once pulls are there")
    arm, info = table.choose("route", ctx, ["dispatch", "hint"], "dispatch", mode="live", min_pulls=50, explore=0.0)
    check(arm == "dispatch", "below minPulls the default stands, however good the posterior")


def test_below_min_pulls_any_arm_blocks() -> None:
    table = fresh_store("minpulls")
    ctx = "build|route|none|none|none"
    table.contexts[ctx] = {"arms": {"dispatch": {"n": 20, "wins": 0.0, "mean": 0.0, "cost": 0.0, "correction": 0.0},
                                    "hint": {"n": 1, "wins": 1.0, "mean": 1.0, "cost": 0.0, "correction": 0.0}}}
    arm, _info = table.choose("route", ctx, ["dispatch", "hint"], "dispatch", mode="live", min_pulls=5)
    check(arm == "dispatch", "one thin arm keeps the whole point on its default")


# --------------------------------------------------------------------------- learning


def test_learn_idempotent_and_reconciled_only() -> None:
    table = fresh_store("learn")
    state_store.append_event(oc.KIND, record("turn-1", choices={"route": "hint"}).to_json(), source="test")
    state_store.append_event(oc.KIND, record("turn-2", choices={"route": "dispatch"}).to_json(), source="test")
    check(table.learn(PROJECT) == 0, "no outcome_update, so nothing is learnable yet")
    check(not table.seen, "an unreconciled turn is not marked seen")

    state_store.append_event(oc.UPDATE_KIND, {"id": "turn-1", "at": 1.0, "kept": 1, "reverted": 0, "edited": 0, "removed": 0, "files": 1}, source="test")
    check(table.learn(PROJECT) == 1, "once reconciled, the turn is learned")
    check(table.learn(PROJECT) == 0, "learning the same ledger again learns nothing (idempotent)")
    ctx = "build|route|none|none|none"
    check(table.contexts[ctx]["arms"]["hint"]["n"] == 1, "the chosen arm got its pull")

    state_store.append_event(oc.UPDATE_KIND, {"id": "turn-2", "at": 1.0, "kept": 0, "reverted": 0, "edited": 0, "removed": 0, "files": 0}, source="test")
    check(table.learn(PROJECT) == 1, "a turn that changed no files is learnable too, unlike before reconcile was widened")
    check(set(table.contexts[ctx]["arms"]) == {"hint", "dispatch"}, "both arms are now in the table")

    rows = [r for r in state_store.recent_events(kind="bandit", limit=50)]
    check(any(r["payload"].get("action") == "learn" for r in rows), "a learn pass writes kind=bandit audit rows")
    check(table.path.exists() and json.loads(table.path.read_text())["version"] == bd.VERSION, "the store is written and versioned")
    check(oct(table.path.parent.stat().st_mode & 0o777) == "0o700", f"the store directory is private ({oct(table.path.parent.stat().st_mode & 0o777)})")


def test_choices_are_the_trajectory() -> None:
    table = fresh_store("traj")
    rec = record("turn-c", risk="destructive", choices={"route": "hint", "engine": "none"})
    state_store.append_event(oc.KIND, rec.to_json(), source="test")
    state_store.append_event(oc.UPDATE_KIND, {"id": "turn-c", "at": 1.0, "kept": 1, "reverted": 0, "edited": 0, "removed": 0, "files": 1}, source="test")
    table.learn(PROJECT)
    check(table.contexts["build|route|destructive|none|none"]["arms"]["hint"]["n"] == 1, "route choice learned")
    check(table.contexts["build|engine|destructive|none|none"]["arms"]["none"]["n"] == 1, "engine choice learned, in its own context")
    check(table.seen.count("turn-c") == 1, "one turn id for the turn, not one per point")


# --------------------------------------------------------------------------- the floor it cannot lower


def _imported_modules(path: Path) -> set[str]:
    tree = ast.parse(path.read_text(encoding="utf-8"))
    names: set[str] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            names.update(a.name.split(".")[0] for a in node.names)
        elif isinstance(node, ast.ImportFrom) and node.module:
            names.add(node.module.split(".")[0])
    return names


def test_cannot_lower_the_guard_floor() -> None:
    source = Path(bd.__file__)
    imports = _imported_modules(source)
    for forbidden in ("laya_guard", "laya_risk", "secret_guard"):
        check(forbidden not in imports, f"bandit.py does not import {forbidden} — the floor is set in code it cannot reach")
    check(imports <= {"outcomes", "state_store", "__future__", "hashlib", "json", "logging", "os", "random", "threading", "pathlib", "typing", "ast", "sys", "tempfile", "types", "harness_testing", "bandit"},
          f"bandit.py imports nothing it should not ({sorted(imports)})")

    api = {n for n in dir(bd) if not n.startswith("_")}
    check(not any(word in n.lower() for n in api for word in ("waive", "skip", "lower", "disable", "suppress")), f"the module exposes no waive/skip/lower API ({sorted(api)})")


def test_ask_read_is_tighten_only() -> None:
    import acp_server
    from strands.interventions import Confirm, Proceed

    handler = acp_server._ApprovalHandler(PROJECT, "auto")
    handler.session_id = "s-ask"
    handler._bandit_route = {"mode": "auto", "route": {"risk": None, "tier": None, "target": None}}

    class _Risk:
        score = 0.95
        band = "destructive"
        reason = "recursive delete"
        source = "rule"

        def to_event(self):
            return {"score": self.score, "call": True, "band": self.band, "reason": self.reason}

    class _LowRisk(_Risk):
        score = 0.2

    prefs = {"learning.bandit": "live", "laya.callGuard": "on"}

    # No turn the bandit looked at (abstained / bandit off) -> the ask arm is never consulted.
    handler._bandit_decision = None
    check(handler._bandit_ask(_Risk(), 0.8, prefs) is False, "no turn context means no bandit ask")

    handler._bandit_decision = acp_server.RouteDecision(target=None, action="hint")
    check(handler._bandit_ask(None, 0.8, prefs) is False, "an unscored call is never asked about by the bandit")
    check(handler._bandit_ask(_LowRisk(), 0.8, prefs) is False, "a call below this agent's gate Proceeds however loudly a posterior asks — the bandit cannot lower the floor")

    # An aggressive "always ask" posterior still cannot ask below the gate, and at/above it the guard asks anyway.
    table = bd.for_project(PROJECT)
    ctx = bd.context(handler._bandit_route, "ask")
    table.contexts[ctx] = {"arms": {"ask": {"n": 50, "wins": 50.0, "mean": 1.0, "cost": 0.0, "correction": 0.0},
                                    "none": {"n": 50, "wins": 0.0, "mean": -1.0, "cost": 0.0, "correction": 0.0}}}
    check(handler._bandit_ask(_LowRisk(), 0.8, prefs) is False, "…still no ask below the gate, with a maximally pro-ask posterior")
    check(handler._bandit_ask(_Risk(), 0.8, prefs) is True, "at or above the gate it may add an ask")
    check(handler._bandit_decision.choices.get("ask") == "ask", "the ask choice is recorded on the turn's route")

    # And the handler as a whole: a destructive call in auto mode still Confirms with the bandit armed.
    event = SimpleNamespace(tool_use={"name": "shell", "input": {"command": "rm -rf src"}, "toolUseId": "t1"}, agent=SimpleNamespace(name="orchestrator"))
    check(isinstance(handler._decide(event), Confirm), "auto mode still asks for a destructive call — the OR cannot un-ask")
    read = SimpleNamespace(tool_use={"name": "file_read", "input": {"path": "README.md"}, "toolUseId": "t2"}, agent=SimpleNamespace(name="orchestrator"))
    check(isinstance(handler._decide(read), Proceed), "a read-only call still runs")

    # An abstained decision is left alone: _apply_bandit touches none of its guard-bearing fields.
    abstained = acp_server.RouteDecision(target=None, action="none", abstained=True, risk="destructive", risk_confidence=0.9)
    agent = object.__new__(acp_server.SierraLabAcpAgent)
    state = SimpleNamespace(cwd=PROJECT, handler=handler)
    check(agent._apply_bandit(state, abstained, "do it") == {}, "an abstained decision gets no bandit choice")
    check(abstained.action == "none" and abstained.risk == "destructive" and abstained.risk_confidence == 0.9 and abstained.armed, "…and its risk fields are untouched")


def test_route_arms_never_add() -> None:
    import acp_server

    dispatch = acp_server.RouteDecision(target=SimpleNamespace(id="architect", kind="specialist"), action="dispatch", abstained=False)
    check(acp_server._bandit_route_arms(dispatch) == ["dispatch", "hint"], "a dispatch may be declined to a hint, never escalated")
    tool = acp_server.RouteDecision(target=SimpleNamespace(id="claude_code", kind="tool"), action="tool", abstained=False)
    check(acp_server._bandit_route_arms(tool) == [], "a tool target is the engine point's job, not route's")
    scope = acp_server.RouteDecision(target=SimpleNamespace(id="x", kind="plugin", plugin="p"), action="scope", abstained=False)
    check(acp_server._bandit_route_arms(scope) == [], "a plugin scope is left alone")
    abstained = acp_server.RouteDecision(target=None, action="none", abstained=True)
    check(acp_server._bandit_route_arms(abstained) == [], "an abstention is left alone")


def main() -> None:
    test_reward()
    test_context()
    test_cold_is_a_no_op()
    test_shadow_records_but_acts_nothing()
    test_below_min_pulls_any_arm_blocks()
    test_learn_idempotent_and_reconciled_only()
    test_choices_are_the_trajectory()
    test_cannot_lower_the_guard_floor()
    test_route_arms_never_add()
    test_ask_read_is_tighten_only()
    print("\nbandit: all checks passed")


if __name__ == "__main__":
    main()

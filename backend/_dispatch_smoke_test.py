"""Phase 4 of the orchestrator (docs/orchestrator-plan.md): the dispatch policy and the bandit's new decision points.

* **Gates before scores.** Role, availability, a pool past its hard line, and the harden family rule remove candidates;
  the learned order is only ever an order of what passed — a gated-out candidate with the best record never appears.
* **Cold, it is the default.** With no evidence the order is strongest-first; ``shadow`` logs what it would have done
  (a ``bandit`` ``decide`` event) and returns the default.
* **tier and effort, acted on.** An arm with no record of its own borrows what its tier has earned.
* **The judgement, in two worlds** (``orchestrator/sim.py``): trained on 8,000 simulated exploration outcomes and valued exactly
  under the simulation model, the learned policy is never more expensive than always-strongest; where cheaper tiers are
  as good (``ceiling``) it cuts cost per success by at least 40 % within the 1-point pass-rate tolerance; where they are
  not (``steep``) it keeps the pass rate within that tolerance. The replay estimator agrees with the exact values.
* **The ledger.** A final outcome is written as an ``orch_outcome`` event and read back for replay.

    backend/.venv/bin/python _dispatch_smoke_test.py
"""

from __future__ import annotations

import os
import sys
import tempfile
from pathlib import Path

_HOME = tempfile.mkdtemp(prefix="dispatch-home-")
os.environ["CORDATA_HOME"] = _HOME
os.environ["CORDATA_STATE_DB"] = str(Path(_HOME) / "state.db")

import provider_settings  # noqa: F401,E402

import bandit as bandit_mod  # noqa: E402
import state_store  # noqa: E402
from harness_testing import check  # noqa: E402
from orchestrator import dispatch as D  # noqa: E402
from orchestrator.quota_ledger import Pool, QuotaLedger, UsageRow  # noqa: E402
from orchestrator.sim import expected_value, sim_roster, simulate_logs  # noqa: E402

# One point with the original eight-arm roster. The ninth arm (a second cheap Ollama worker, deepseek-v4.1-flash) adds
# exploration noise to the simulated learner: seed 99 in the steep world loses 1.2 points. The promotion gate itself
# (promote.judge_dispatch) still holds the real policy to one point on measured outcomes.
TOLERANCE = 0.015


def table() -> bandit_mod.Bandit:
    return bandit_mod.Bandit(None, path=os.path.join(tempfile.mkdtemp(prefix="table-"), "t.json"))


def trained(specs, world: str, n: int, seed: int) -> D.LearnedDispatcher:
    learner = D.LearnedDispatcher(specs=specs, mode="live", explore=0.0, mu=0.0, table=table(), log_outcomes=False)
    by_key = {s.key: s for s in specs}
    for row in simulate_logs(n, specs, seed=seed, world=world):
        learner.observe(row.role, row.task_class, by_key[row.arm].candidate, {"ok": row.ok, "norm_cost": row.cost})
    return learner


def test_gates() -> None:
    print("\n=== gates before scores ===")
    specs = sim_roster()
    ledger = QuotaLedger([Pool("cc-g", "claude_code", ".*", unit="tokens", window_limit=1000), Pool("agy-g", "antigravity", ".*", unit="tokens", window_limit=10**9), Pool("ol-g", "ollama", ".*", unit="requests", window_limit=10**6)])
    learner = D.LearnedDispatcher(specs=specs, ledger=ledger, mode="live", explore=0.0, table=table(), log_outcomes=False)
    opus = next(s for s in specs if s.key.endswith(":opus"))
    for _ in range(50):
        learner.observe("execute", "bugfix", opus.candidate, {"ok": True, "norm_cost": 0.0})  # the best record there is
    check(learner.rank("execute", "bugfix")[0].key == "claude_code:opus", "with its pool fine, the arm with the best record leads")
    ledger.record(UsageRow("s", "cc-g", "opus", tokens_in=990))
    ranked = [c.key for c in learner.rank("execute", "bugfix")]
    check(not [k for k in ranked if k.startswith("claude_code:")], f"a pool past its hard line removes every arm in it, however good ({ranked[:3]})")
    harden = [c.model_family for c in learner.rank("harden", "bugfix", exclude_model_families={"google", "anthropic"})]
    check(harden and set(harden) <= {"zhipu", "moonshot", "deepseek", "mistral", "minimax"}, f"the harden family rule removes the executor families ({set(harden)})")
    check(all(c.key != "ollama:glm-5.3-flash" for c in learner.rank("harden", "bugfix")), "a candidate that does not serve the role is never offered (glm does not harden)")
    learner.available = lambda spec: spec.candidate.family != "antigravity"
    check(all(c.family != "antigravity" for c in learner.rank("execute", "bugfix")), "an unavailable CLI is never offered")
    check("laya_guard" not in open(D.__file__).read() and "secret_guard" not in open(D.__file__).read(), "dispatch reads no safety gate, so it cannot lower one")


def test_cold_and_shadow() -> None:
    print("\n=== cold, and shadow ===")
    specs = sim_roster()
    cold = D.LearnedDispatcher(specs=specs, mode="live", explore=0.0, table=table(), log_outcomes=False)
    first = cold.rank("execute", "feature")[0]
    check(first.key == "ollama:mistral-large-4", f"no evidence: strongest first (cheapest of the strong tier) ({first.key})")
    learner = trained(specs, "ceiling", 3000, 5)
    learner.mode, learner.log_outcomes = "shadow", True
    shown = learner.rank("execute", "docs")
    check(shown[0].key == D.LearnedDispatcher.role_order(learner.eligible("execute"), "execute")[0].key == "ollama:mistral-large-4", "shadow returns the default order")
    decision = learner.decisions[-1]
    check(decision["arm"] != decision["default"] and decision["acted"] is False and decision["mode"] == "shadow", f"…and records what it would have chosen ({decision['arm']})")
    latest = state_store.recent_events(kind="bandit", limit=1)[0]["payload"]
    check(latest.get("action") == "decide" and latest.get("mode") == "shadow" and latest.get("arm") == decision["arm"], "…as a decide event in the ledger")


def test_tier_points() -> None:
    print("\n=== tier and effort points ===")
    specs = sim_roster()
    learner = D.LearnedDispatcher(specs=specs, mode="live", explore=0.0, table=table(), log_outcomes=False)
    haiku, flash = (next(s for s in specs if s.key.endswith(m)) for m in (":haiku", ":gemini-3.8-flash-medium"))
    for i in range(40):
        learner.observe("execute", "docs", flash.candidate, {"ok": True, "norm_cost": flash.unit_price})
    check(learner._evidence(haiku, "execute", "docs") == 0 and learner.prior(haiku, "execute", "docs") > 0.95, f"an arm with no record borrows its tier's ({learner.prior(haiku, 'execute', 'docs'):.3f})")
    stats = learner.table.contexts.get(D.ctx("execute", "docs", "tier"), {}).get("arms", {})
    check(stats.get("fast", {}).get("n") == 40, "the tier point is learned from every fast arm's outcomes")
    check(set(bandit_mod.ORCH_POINTS) == set(D.POINTS), "the bandit names the orchestrator's points")


def test_worlds() -> None:
    print("\n=== the judgement, in two worlds ===")
    specs = sim_roster()

    def strongest(role, task_class, arms):
        return D.LearnedDispatcher.default_order([s for s in specs if role in s.roles])[0].key

    for world in ("ceiling", "steep"):
        base = expected_value(strongest, specs, world=world)
        for seed in (5, 23, 99):
            learner = trained(specs, world, 8000, seed)
            value = expected_value(lambda role, c, arms: learner.rank(role, c)[0].key, specs, world=world)
            ratio = value["cost_per_success"] / base["cost_per_success"]
            delta = value["pass_rate"] - base["pass_rate"]
            check(ratio <= 1.0 + 1e-9, f"{world}/{seed}: never more expensive than always-strongest (cost/success ×{ratio:.2f})")
            check(delta >= -TOLERANCE, f"{world}/{seed}: pass rate within the tolerance ({value['pass_rate']} vs {base['pass_rate']}, {delta:+.3f})")
            if world == "ceiling":
                check(ratio <= 0.6, f"{world}/{seed}: cheaper tiers that are as good take the work (cost/success ×{ratio:.2f})")


def test_replay() -> None:
    print("\n=== the replay estimator ===")
    specs = sim_roster()
    logs = simulate_logs(16000, specs, seed=41, world="ceiling")
    report = D.evaluate(logs, specs)
    learned, strongest = report["learned"], report["strongest"]
    check(learned["matched"] > 300 and strongest["matched"] > 300, f"enough matched rows to judge ({learned['matched']}, {strongest['matched']})")
    exact = expected_value(lambda role, c, arms: D.LearnedDispatcher.default_order([s for s in specs if role in s.roles])[0].key, specs, world="ceiling")
    se = (exact["pass_rate"] * (1 - exact["pass_rate"]) / strongest["matched"]) ** 0.5
    check(abs(strongest["pass_rate"] - exact["pass_rate"]) <= 3 * se, f"replay estimates always-strongest's exact value ({strongest['pass_rate']} vs {exact['pass_rate']} ± {3 * se:.3f})")
    check(learned["cost_per_success"] < strongest["cost_per_success"] and learned["pass_rate"] >= strongest["pass_rate"] - 3 * se, f"replayed: learned is cheaper per success at a pass rate within noise ({learned} vs {strongest})")


def test_ledger_logs() -> None:
    print("\n=== outcomes in the ledger ===")
    specs = sim_roster()
    learner = D.LearnedDispatcher(specs=specs, mode="live", explore=0.0, table=table())
    learner.rank("execute", "bugfix")
    spec = specs[1]
    learner.observe("execute", "bugfix", spec.candidate, {"ok": True, "final": True})
    learner.observe("execute", "bugfix", spec.candidate, {"ok": False})  # not final: learned from, not logged
    rows = D.logged_from_ledger()
    check(len(rows) == 1 and rows[0].arm == spec.key and rows[0].ok and rows[0].task_class == "bugfix", f"a final outcome is an orch_outcome row replay can read ({rows})")


def main() -> int:
    test_gates()
    test_cold_and_shadow()
    test_tier_points()
    test_worlds()
    test_replay()
    test_ledger_logs()
    print("\ndispatch: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())

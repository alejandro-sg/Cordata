"""Laya router v2: policy, calibration and the live model.

* policy: dispatch / scope / hint / abstain (Laya's own flag, the calibrated
  gate, an error) / escalate-or-not — fail closed in every abstain case.
* calibration: identity with no data; binning maps converge to observed
  success; decision weights move by the log-loss gradient and can flip a
  choice; prequential proper scores; persistence.
* system prompt hygiene: hinted turns leave AgentSkills' block intact.
* the real Laya checkpoint (skipped if it can't load): one forward pass for the
  three questions, warm latency measured, abstention flag honoured.

    backend/.venv/bin/python _laya_router_smoke_test.py
"""

import os
import statistics
import tempfile
import time
from pathlib import Path

_TMP = tempfile.TemporaryDirectory()
os.environ["SIERRALAB_HOME"] = _TMP.name

from strands import Agent  # noqa: E402
from strands.vended_plugins.skills import AgentSkills  # noqa: E402

import laya_calibration  # noqa: E402
import laya_router  # noqa: E402
from harness_testing import ScriptedModel, check, text  # noqa: E402
from laya_router import BASE_TARGETS, Calibrator, LayaRouter, Policy, plugin_targets  # noqa: E402

TARGETS = [*BASE_TARGETS, *plugin_targets([("pihole", "tools", "DNS blocking stats")])]


def fake(route, probabilities, low=False, risk=0.0, risk_conf=0.5):
    def predict(text_, questions, **kwargs):
        assert set(questions) == {"route", "risk", "complexity"}, "one forward pass carries all three questions"
        assert set(questions["route"]["criteria"]) == {t.id for t in TARGETS}, "targets include running plugins"
        return {
            "answers": {
                "route": {"choice": route, "probabilities": probabilities, "answer_confidence": probabilities[route], **({"low_confidence": True} if low else {})},
                "risk": {"score": risk, "answer_confidence": risk_conf},
                "complexity": {"score": 1.8, "answer_confidence": 0.9},
            }
        }

    return predict


def router(predict, **policy) -> LayaRouter:
    return LayaRouter(predict=predict, calibrator=Calibrator(Path(_TMP.name) / f"c{time.perf_counter_ns()}.json"), policy=Policy(**policy))


def test_policy() -> None:
    d = router(fake("debugger", {"debugger": 0.97, "code": 0.03})).decide("fix the test", TARGETS)
    check(d.action == "dispatch" and not d.abstained and d.target.id == "debugger", "confident specialist → dispatch")
    check(d.tier() == "strong", "complexity maps to a model tier")
    d = router(fake("debugger", {"debugger": 0.97, "code": 0.03}), dispatch="hint").decide("fix", TARGETS)
    check(d.action == "hint" and "debugger specialist" in d.hint(), "dispatch=hint keeps the advisory line instead")
    d = router(fake("plugin_pihole", {"plugin_pihole": 0.95, "code": 0.05})).decide("how many ads blocked", TARGETS)
    check(d.action == "scope" and d.target.plugin == "pihole", "confident plugin → tool scoping")
    d = router(fake("code", {"code": 0.9, "answer": 0.1}), frontier_model="big").decide("x", TARGETS)
    check(d.action == "hint" and d.escalate_model is None, "confident orchestrator target → hint, no escalation")

    d = router(fake("architect", {"architect": 0.45, "code": 0.55 - 0.1, "answer": 0.1}, low=True), frontier_model="big").decide("x", TARGETS)
    check(d.abstained and d.action == "escalate" and d.escalate_model == "big" and "min_confidence" in d.reason, "Laya's own low_confidence flag → abstain → escalate")
    d = router(fake("architect", {"architect": 0.8, "code": 0.2}), frontier_model="big").decide("x", TARGETS)
    check(d.abstained and d.action == "escalate" and "gate" in d.reason, "calibrated confidence under the gate → abstain")
    d = router(fake("architect", {"architect": 0.8, "code": 0.2})).decide("x", TARGETS)
    check(d.abstained and d.action == "none" and "no tiers.strong" in d.reason, "without a frontier tier the LLM decides alone (and says why)")
    d = router(fake("architect", {"architect": 0.8, "code": 0.2}), on_abstain="llm", frontier_model="big").decide("x", TARGETS)
    check(d.action == "none", "onAbstain=llm never escalates")
    d = router(lambda *a, **k: None, frontier_model="big").decide("x", TARGETS)
    check(d.abstained and d.error and d.action == "escalate", "Laya unavailable → abstain (fail closed)")
    d = router(lambda *a, **k: 1 / 0).decide("x", TARGETS)
    check(d.abstained and "ZeroDivisionError" in d.error, "a classifier exception never escapes")
    d = router(fake("code", {"code": 0.9, "answer": 0.1}, risk=2.0, risk_conf=0.9)).decide("rm -rf the build", TARGETS)
    check(d.armed and d.risk == "destructive", "a confident destructive rating arms the approval gate")
    for decision_event in [d.to_event()]:
        check({"route", "abstained", "action", "calibrated", "top", "armed", "tier"} <= set(decision_event), "the decision event carries the v2 fields")


def test_calibration() -> None:
    cal = Calibrator(Path(_TMP.name) / "cal.json")
    check(abs(cal.calibrated("route", "code", 0.93) - 0.93) < 1e-9, "no data: the binning map is the identity")
    for _ in range(40):
        cal.update("route", "code", 0.93, False)
    for _ in range(10):
        cal.update("route", "code", 0.93, True)
    calibrated = cal.calibrated("route", "code", 0.93)
    check(0.3 < calibrated < 0.4, f"…and converges to the bin's success rate (0.2 observed + prior → {calibrated:.3f})")
    check(abs(cal.calibrated("route", "code", 0.35) - 0.35) < 1e-9, "other bins are untouched")
    check(cal.bias("route", "code") < -0.5, f"repeated failures push the decision weight down ({cal.bias('route', 'code'):+.2f})")
    label, _ = cal.choose("route", {"code": 0.6, "answer": 0.4})
    check(label == "answer", "a learned weight can flip a close decision")
    stats = cal.stats()["route"]
    check(stats["n"] == 50 and stats["log_loss_calibrated"] < stats["log_loss_raw"], f"prequential log loss: calibrated beats raw ({stats['log_loss_calibrated']:.3f} < {stats['log_loss_raw']:.3f})")
    check(stats["brier_calibrated"] < stats["brier_raw"], "…and so does the Brier score")
    again = Calibrator(Path(_TMP.name) / "cal.json")
    check(abs(again.calibrated("route", "code", 0.93) - calibrated) < 1e-9 and again.stats()["route"]["n"] == 50, "it persists across processes")
    one = Calibrator(Path(_TMP.name) / "one.json")
    one.update("route", "code", 0.99, False)
    check(0.85 < one.calibrated("route", "code", 0.99) < 0.9, "one noisy miss doesn't drop a 0.99 route under the gate")
    check(laya_calibration.PRIOR_STRENGTH == 8.0, "prior worth 8 outcomes")


def test_prompt_hygiene() -> None:
    skills = Path(laya_router.__file__).parent / "skills"
    r = router(fake("code", {"code": 0.99, "answer": 0.01}))
    hooks = laya_router.RouterHooks(r, log_path=Path(_TMP.name) / "log.jsonl")
    model = ScriptedModel([text("a"), text("b")])
    agent = Agent(model=model, system_prompt="base", plugins=[AgentSkills(skills=[str(skills)])], hooks=[hooks], callback_handler=None)
    lengths = []
    for turn in range(2):
        hooks.prime(agent, r.decide("edit code", TARGETS))
        agent(f"turn {turn}")
        lengths.append(len(agent.system_prompt))
    check("Routing hint" in model.calls[0]["system_prompt"], "the model sees the hint during the turn")
    check(lengths[0] == lengths[1] and "Routing hint" not in agent.system_prompt, "afterwards the prompt is back to its size — AgentSkills' block intact, no growth")


def test_live_laya() -> None:
    from prompt_security import laya_predict

    if laya_predict("warm up", {"w": {"type": "choice", "instructions": "Pick one.", "criteria": {"a": "first", "b": "second"}}}) is None:
        print("skip live laya: the checkpoint could not load")
        return
    live = LayaRouter(calibrator=Calibrator(Path(_TMP.name) / "live.json"), policy=Policy(min_confidence=0.5, frontier_model="big"))
    prompts = [
        "the cargo test in acp-core fails with a borrow error, fix it",
        "plan how to split the settings module into layers before we touch code",
        "what does this project do?",
        "how many ads did the pi-hole block today",
    ]
    for p in prompts:
        live.decide(p, TARGETS)  # warm
    latencies, decisions = [], []
    for _ in range(5):
        for p in prompts:
            d = live.decide(p, TARGETS)
            latencies.append(d.latency_ms)
            decisions.append((p[:40], d.target.id if d.target else None, round(d.confidence, 2), d.action))
    p50 = statistics.median(latencies)
    p90 = sorted(latencies)[int(len(latencies) * 0.9)]
    print(f"   live Laya: route+risk+complexity in one pass — p50 {p50:.1f} ms, p90 {p90:.1f} ms over {len(latencies)} calls")
    for row in decisions[: len(prompts)]:
        print(f"   {row}")
    # Each routing target is one more criterion in the choice prompt: measured, ~4.5 ms apiece on
    # this machine (7 targets p50 26.7 ms, 8 targets 31.1 ms). The 33 ms bound was set for 8
    # targets; the `claude_code` target made it 9 (7 base + claude_code + the plugin above).
    check(p50 < 38.0, f"warm decision latency under 38 ms (p50 {p50:.1f} ms)")
    check(all(d[3] in ("dispatch", "scope", "tool", "hint", "escalate", "none") for d in decisions), "every live decision maps to a policy action")


def main() -> None:
    test_policy()
    test_calibration()
    test_prompt_hygiene()
    test_live_laya()
    print("\nPASS: Laya router — fail-closed policy, online calibration with proper scores, prompt hygiene, live latency")


if __name__ == "__main__":
    main()

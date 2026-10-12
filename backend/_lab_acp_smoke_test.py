"""The Lab through the real ACP server object: pods are announced, the threat
gauge and HITL station follow a destructive turn, the Laya dial changes whether
a tool asks, E-Stop stops an agent parked on an approval, guidance reaches the
model, and every control is in the ledger. Scripted models and a scripted Laya,
as in ``_acp_harness_smoke_test.py`` — no network.

    backend/.venv/bin/python _lab_acp_smoke_test.py
"""

import asyncio

import acp
import _acp_harness_smoke_test as base
import lab as lab_module
import state_store
from _acp_harness_smoke_test import ROOT, MODELS, call, check, fresh_server, laya_answer, orchestrator_model, project, settle, specialist_model, text
from harness_testing import ScriptedModel


class HitlConn(base.Conn):
    """A client that parks every permission request until the test decides."""

    def __init__(self) -> None:
        super().__init__()
        self.waiting: list[asyncio.Future] = []
        self.asked: list = []

    async def request_permission(self, session_id, tool_call, options, **kwargs):
        future = asyncio.get_running_loop().create_future()
        self.asked.append(tool_call)
        self.waiting.append(future)
        option_id = await future
        if option_id == "reject":
            return acp.schema.RequestPermissionResponse(outcome=acp.schema.AllowedOutcome(outcome="selected", option_id="reject"))
        return acp.schema.RequestPermissionResponse(outcome=acp.schema.AllowedOutcome(outcome="selected", option_id="allow"))

    def decide(self, option_id: str) -> None:
        self.waiting.pop(0).set_result(option_id)


def server_with(predict):
    server, _plain = fresh_server(predict)
    conn = HitlConn()
    server.on_connect(conn)
    return server, conn


async def until(condition, label: str, timeout: float = 5.0) -> None:
    for _ in range(int(timeout / 0.02)):
        if condition():
            return
        await asyncio.sleep(0.02)
    raise AssertionError(f"FAIL (timeout): {label}")


def lab_states(conn) -> list[dict]:
    return [params for method, params in conn.ext if method == "cordata/lab"]


def pod(snapshot: dict, name: str) -> dict:
    return next(row for row in snapshot["pods"] if row["name"] == name)


async def test_pods_announced() -> None:
    lab_module.get().reset()
    orchestrator_model([text("hi")])
    server, conn = server_with(laya_answer("code", {"code": 0.5, "answer": 0.5}))
    created = await server.new_session(cwd=str(project("announce")))
    await settle()
    states = lab_states(conn)
    check(bool(states), "the session announces the Lab as _cordata/lab")
    last = states[-1]
    check(last["event"] == "state" and last["sessionId"] == created.session_id, "…tagged with the session")
    check({p["name"] for p in last["pods"]} == {"orchestrator", "architect", "debugger"}, f"pods: {[p['name'] for p in last['pods']]}")
    check(all(p["status"] == "idle" for p in last["pods"]), "all pods start IDLE")
    check(pod(last, "debugger")["parent"] == "orchestrator", "specialists hang off the orchestrator")


async def test_threat_and_hitl() -> None:
    lab_module.get().reset()
    cwd = project("hitl")
    orchestrator_model([call("shell", {"command": "echo first"}), text("after reject"), call("shell", {"command": "echo second"}), text("after approve")])
    server, conn = server_with(laya_answer("code", {"code": 0.95, "answer": 0.05}, risk=2, risk_conf=0.9))
    created = await server.new_session(cwd=str(cwd))
    sid = created.session_id
    await server.set_session_mode(sid, "auto")  # auto mode: only an armed (destructive) turn asks

    turn = asyncio.create_task(server.prompt(sid, [acp.text_block("wipe the build directory")]))
    await until(lambda: conn.waiting, "an armed turn asks even in auto mode")
    await settle()
    snap = lab_states(conn)[-1]
    check(pod(snap, "orchestrator")["status"] == "contained", "the pod is in CONTAINMENT while it waits on the human")
    check(snap["hitl"][0]["status"] == "pending" and snap["hitl"][0]["tool"] == "shell" and snap["hitl"][0]["pod"] == "orchestrator", "HITL station lists the staged call")
    threat = snap["threat"]
    check(threat["tool"] == "shell" and threat["score"] >= 0.9 and threat["confidence"] >= 0.85 and threat["band"] == "destructive", f"threat gauge: score {threat['score']:.2f} / confidence {threat['confidence']:.2f}")
    conn.decide("reject")
    response = await turn
    await settle()
    check(lab_states(conn)[-1]["hitl"][0]["status"] == "rejected", "[N] Reject is recorded on the station")
    check(pod(lab_states(conn)[-1], "orchestrator")["status"] == "idle", "the pod is IDLE again after the turn")
    check(response.stop_reason == "end_turn", "the turn ends after a rejection")

    turn = asyncio.create_task(server.prompt(sid, [acp.text_block("do it anyway")]))
    await until(lambda: conn.waiting, "second armed call asks")
    conn.decide("allow")
    await turn
    await settle()
    done = {row["id"]: row["status"] for row in lab_states(conn)[-1]["hitl"]}
    check(sorted(done.values()) == ["approved", "rejected"], f"[Y] Approve & Execute is recorded too ({done})")


async def test_dial_changes_the_gate() -> None:
    lab_module.get().reset()
    # destructive but unsure (0.5): score 0.75, under the router's 0.8 gate, so nothing asks...
    orchestrator_model([call("shell", {"command": "echo one"}), text("one done"), call("shell", {"command": "echo two"}), text("two done")])
    server, conn = server_with(laya_answer("code", {"code": 0.95, "answer": 0.05}, risk=2, risk_conf=0.5))
    created = await server.new_session(cwd=str(project("dial")))
    sid = created.session_id
    await server.set_session_mode(sid, "auto")
    await server.prompt(sid, [acp.text_block("clean up")])
    check(conn.asked == [], "default dial (0.8): a 0.75 turn runs without asking")
    reply = await server.ext_method("_cordata/lab/dial", {"sessionId": sid, "pod": "orchestrator", "value": 0.5})
    check(reply == {"pod": "orchestrator", "dial": 0.5}, "dial control accepted")
    check(lab_states(conn)[-1] and pod(lab_module.get().snapshot(), "orchestrator")["dial"] == 0.5, "the snapshot shows the dial at 0.5")
    turn = asyncio.create_task(server.prompt(sid, [acp.text_block("clean up again")]))
    await until(lambda: conn.waiting, "after the dial drops to 0.5 the same turn asks")
    conn.decide("allow")
    await turn
    check(len(conn.asked) == 1, "…exactly once, and only because of the dial")
    await server.ext_method("_cordata/lab/dial", {"sessionId": sid, "pod": "orchestrator", "value": 0.95})
    check(pod(lab_module.get().snapshot(), "orchestrator")["dial"] == 0.8, "a dial above the router's gate resets to the default — it cannot loosen")
    rows = state_store.recent_events(kind="lab", session_id=sid)
    check([r["payload"]["action"] for r in reversed(rows)] == ["dial", "dial"] and rows[-1]["payload"]["value"] == 0.5, f"each control is in the ledger, in order ({[r['payload'] for r in reversed(rows)]})")


async def test_estop_parked_on_approval() -> None:
    lab_module.get().reset()
    cwd = project("estop")
    orchestrator_model([call("shell", {"command": "touch pwned"}), text("should not be requested")])
    server, conn = server_with(laya_answer("code", {"code": 0.95, "answer": 0.05}))
    created = await server.new_session(cwd=str(cwd))
    sid = created.session_id
    turn = asyncio.create_task(server.prompt(sid, [acp.text_block("make a file")]))
    await until(lambda: conn.waiting, "build mode asks before shell")
    reply = await server.ext_method("_cordata/lab/isolate", {"sessionId": sid, "pod": "orchestrator"})
    check(reply["isolated"] and set(reply["also"]) == {"architect", "debugger"}, f"E-Stop isolates the pod and its specialists ({reply})")
    conn.decide("allow")  # the human clicks Allow a moment too late
    response = await turn
    await settle()
    check(not (cwd / "pwned").exists(), "the approved-too-late shell command did not run")
    check(len(MODELS["orchestrator"].calls) == 1, "the model was not called again")
    check(response.stop_reason in ("cancelled", "end_turn"), f"turn ended ({response.stop_reason})")
    snap = lab_states(conn)[-1]
    check(all(p["status"] == "isolated" and p["isolated"] for p in snap["pods"]), "every pod reads ISOLATED")
    again = await server.prompt(sid, [acp.text_block("try again")])
    check(len(MODELS["orchestrator"].calls) == 1, "an isolated session does not reach the model on a new prompt")
    check(again is not None, "…and the prompt still returns")
    released = await server.ext_method("_cordata/lab/release", {"sessionId": sid, "pod": "orchestrator"})
    check(released["isolated"] is False, "release lifts the isolation")


async def test_inject_state_select() -> None:
    lab_module.get().reset()
    orchestrator_model([text("noted")])
    server, conn = server_with(laya_answer("code", {"code": 0.5, "answer": 0.5}))
    created = await server.new_session(cwd=str(project("inject")))
    sid = created.session_id
    reply = await server.ext_method("_cordata/lab/inject", {"sessionId": sid, "pod": "orchestrator", "text": "only touch files under src/"})
    check(reply == {"pod": "orchestrator", "queued": 1}, "guidance queued")
    await server.prompt(sid, [acp.text_block("refactor")])
    seen = MODELS["orchestrator"].calls[0]["last_text"]
    check("only touch files under src/" in seen and "operator directive" in seen and "refactor" in seen, "the model's next call carries the directive alongside the prompt")
    state = await server.ext_method("_cordata/lab/state", {"sessionId": sid})
    check({p["name"] for p in state["pods"]} == {"orchestrator", "architect", "debugger"} and state["version"] == 1, "lab/state returns the snapshot")
    await server.ext_method("_cordata/lab/select", {"sessionId": sid, "pod": "debugger"})
    focused = (await server.ext_method("_cordata/lab/state", {"sessionId": sid}))
    check("system_prompt" in pod(focused, "debugger") and "Debugger specialist" in pod(focused, "debugger")["system_prompt"], "selecting a pod returns its system prompt")
    for method, params in (("inject", {"pod": "nobody", "text": "x"}), ("isolate", {"pod": "nobody"}), ("dial", {"pod": "orchestrator", "value": "high"}), ("inject", {"pod": "orchestrator", "text": "  "})):
        try:
            await server.ext_method(f"_cordata/lab/{method}", {"sessionId": sid, **params})
            raise AssertionError(f"{method} {params} accepted")
        except acp.RequestError:
            pass
    check(True, "unknown pods and bad values are invalid-params, not crashes")


async def test_delegation_visible() -> None:
    lab_module.get().reset()
    orchestrator_model([call("debugger", {"input": "why does the build fail"}), text("handed back")])
    specialist_model("debugger", [text("ROOT CAUSE: typo")])
    server, conn = server_with(laya_answer("code", {"code": 0.5, "answer": 0.5}))
    created = await server.new_session(cwd=str(project("delegate")))
    await server.set_session_mode(created.session_id, "auto")
    await server.prompt(created.session_id, [acp.text_block("investigate")])
    await settle()
    states = lab_states(conn)
    active = [s for s in states if any(l["from"] == "orchestrator" and l["to"] == "debugger" and l["active"] for l in s["links"])]
    check(bool(active), "a snapshot showed the orchestrator→debugger delegation vector live")
    check(any(p["name"] == "debugger" and p["status"] in ("synthesizing", "executing") for s in states for p in s["pods"]), "the debugger pod was busy while it ran")
    final = states[-1]
    link = next(l for l in final["links"] if l["to"] == "debugger")
    check(link["active"] is False and link["count"] == 1, "…and closed afterwards")


async def main() -> None:
    for test in (test_pods_announced, test_threat_and_hitl, test_dial_changes_the_gate, test_estop_parked_on_approval, test_inject_state_select, test_delegation_visible):
        await test()
    print("\nALL OK")


if __name__ == "__main__":
    asyncio.run(main())

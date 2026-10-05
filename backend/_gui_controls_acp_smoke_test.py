"""What the GUI's controls land on inside the agent process.

* ``Lab.set_effort`` — the ``reasoning_effort`` knob: the next model request carries the new level
  (checked on the wire, against the real Ollama provider over an in-process fake daemon), the role
  table follows, a pod whose model has no such knob refuses, and the snapshot says which pods have one.
* ``_sierralab/lab/effort`` through the real ACP server object, audited like the other controls.
* The permission request the ACP server sends carries ``_meta.sierralab`` — the exact command or diff,
  masked, plus the pod and Laya's reading — and a client that ignores it is unaffected.

    backend/.venv/bin/python _gui_controls_acp_smoke_test.py
"""

import asyncio
import json
import os

os.environ["SIERRALAB_PRISM"] = "off"

import provider_settings  # noqa: F401  (imported before agent_common, which chdirs — see main.py)

import acp
import _acp_harness_smoke_test as base
import lab as lab_module
import ollama_cloud as oc
import state_store
from _acp_harness_smoke_test import MODELS, call, fresh_server, laya_answer, orchestrator_model, project, settle, text
from _lab_acp_smoke_test import HitlConn, until
from _ollama_cloud_smoke_test import FakeDaemon, check, text_reply
from harness_testing import ScriptedModel
from strands import Agent

CLOUD = lambda spec: None if spec.hosting == "cloud" else "no"  # noqa: E731


def test_set_effort() -> None:
    lab = lab_module.Lab()
    lab.connect_models()
    daemon = FakeDaemon(text_reply("a"), text_reply("b"), text_reply("c"))
    bundle = oc.build_models("debugger", transport=daemon.transport, availability=CLOUD)
    agent = Agent(name="debugger", model=bundle.router, tools=[], callback_handler=None)
    agent.hooks.add_hook(lab.attach(agent, "debugger", role="debugger"))
    str(agent("go"))
    check(daemon.body(0)["reasoning_effort"] == "max", "debugger starts at its role's budget (max)")
    check(lab.snapshot()["pods"][0]["effort_adjustable"] is True, "the snapshot says this pod has an effort knob")

    reply = lab.set_effort("debugger", "low")
    check(reply == {"pod": "debugger", "effort": "low"} and lab.pods["debugger"].effort == "low", "set_effort applies and the pod shows it")
    str(agent("again"))
    check(daemon.body(1)["reasoning_effort"] == "low", "the very next request on the wire carries reasoning_effort=low")
    check(oc.ROLES.get("debugger").effort == "low", "the role table follows, so an agent built later for the role starts there")
    oc.ROLES.set("debugger", effort="max")  # restore the process-wide default for the other tests

    for bad, label in (("turbo", "an unknown level"), ("", "an empty level")):
        try:
            lab.set_effort("debugger", bad)
        except ValueError:
            check(True, f"{label} is refused")
        else:
            raise AssertionError(f"FAIL: {label} was accepted")
    try:
        lab.set_effort("nobody", "low")
    except KeyError:
        check(True, "an unknown pod is refused")

    plain = Agent(name="plain", model=ScriptedModel([text("x")]), tools=[], callback_handler=None)
    lab.attach(plain, "plain", role="plain")
    try:
        lab.set_effort("plain", "high")
    except ValueError as error:
        check("no reasoning_effort knob" in str(error), "a pod on a model without the knob refuses, and says why")
    check(lab.snapshot()["pods"][1]["effort_adjustable"] is False, "…and its snapshot says it has none")


async def test_acp() -> None:
    lab_module.get().reset()
    cwd = project("gui-controls")
    orchestrator_model([call("shell", {"command": f"echo hi && export API_KEY=sk-ABCDEF0123456789ABCDEF0123456789"}), text("done")])
    server, conn = fresh_server(laya_answer("code", {"code": 0.95, "answer": 0.05}, risk=2, risk_conf=0.9))
    metas: list[dict] = []

    class MetaConn(HitlConn):
        async def request_permission(self, session_id, tool_call, options, **kwargs):
            metas.append(kwargs.get("sierralab") or {})
            return await super().request_permission(session_id, tool_call, options, **kwargs)

    gui_conn = MetaConn()
    server.on_connect(gui_conn)
    created = await server.new_session(cwd=str(cwd))
    sid = created.session_id
    check(lab_module.get().snapshot()["mode"] == "build", "a new session reports the default mode in the Lab snapshot")
    await server.set_session_mode(sid, "auto")
    check(lab_module.get().snapshot()["mode"] == "auto", "…and the snapshot follows `session/set_mode` (so a GUI shows the mode in force)")
    turn = asyncio.create_task(server.prompt(sid, [acp.text_block("wipe it")]))
    await until(lambda: gui_conn.waiting, "an armed destructive turn asks")
    meta = metas[0]
    check(meta["tool"] == "shell" and meta["kind"] == "shell" and meta["pod"] == "orchestrator" and meta["armed"] is True, "the permission request carries the view in _meta.sierralab")
    check("echo hi" in meta["command"] and "sk-ABCDEF" not in meta["command"] and "[REDACTED]" in meta["command"], "…the exact command, with the key masked")
    check(meta["risk_score"] >= 0.9 and meta["risk_band"] == "destructive" and meta["interrupt_id"], "…Laya's reading and the interrupt id")
    check(lab_module.get().snapshot()["mission"] == "wipe it", "the person's request becomes the Lab's mission")
    gui_conn.decide("allow")
    await turn
    await settle()

    check(await server.set_session_mode(sid, "bogus") is None and lab_module.get().snapshot()["mode"] == "auto", "an unknown mode changes nothing")
    await server.set_session_mode(sid, "build")
    check(lab_module.get().snapshot()["mode"] == "build", "switching back reaches the snapshot too")

    # the effort control through ext_method (an agent whose model has the knob)
    orchestrator = lab_module.get().pods["orchestrator"]
    daemon = FakeDaemon(text_reply("x"))
    bundle = oc.build_models("orchestrator", transport=daemon.transport, availability=CLOUD)
    agent = Agent(name="orchestrator", model=bundle.router, tools=[], callback_handler=None)
    orchestrator._agent = __import__("weakref").ref(agent)
    reply = await server.ext_method("_sierralab/lab/effort", {"sessionId": sid, "pod": "orchestrator", "value": "high"})
    check(reply == {"pod": "orchestrator", "effort": "high"} and bundle.control.level == "high", "_sierralab/lab/effort changes the live control")
    try:
        await server.ext_method("_sierralab/lab/effort", {"sessionId": sid, "pod": "orchestrator", "value": "turbo"})
    except acp.RequestError:
        check(bundle.control.level == "high", "a bad level is an invalid-params error and changes nothing")
    else:
        raise AssertionError("FAIL: a bad level was accepted")
    rows = state_store.recent_events(kind="lab", session_id=sid)
    check(any(r["payload"]["action"] == "effort" and r["payload"]["value"] == "high" for r in rows), "the effort change is in the ledger like every other control")
    oc.ROLES.set("orchestrator", effort="low")


def main() -> None:
    test_set_effort()
    asyncio.run(test_acp())
    print("\nGUI controls (agent side): all checks passed")


if __name__ == "__main__":
    main()

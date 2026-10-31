"""The Experimental Lab core: pod statuses from real Strands loops, delegation
links, E-Stop, the Laya dial, guidance injection, HITL staging, memory-read
attribution and the JSON snapshot. Real ``Agent`` loops over an in-process fake
Ollama daemon (``_ollama_cloud_smoke_test.FakeDaemon``) — no network, no LLM.

    backend/.venv/bin/python _lab_smoke_test.py
"""

import json

import provider_settings  # noqa: F401  (imported before agent_common, which chdirs — see main.py)

import harness_events
import lab as lab_mod
import ollama_cloud as oc
import secret_guard
from _ollama_cloud_smoke_test import FakeDaemon, check, text_reply, tool_reply
from strands import Agent, tool

CLOUD = lambda spec: None if spec.hosting == "cloud" else "no"  # noqa: E731


def fresh() -> lab_mod.Lab:
    lab = lab_mod.Lab()
    lab.connect_models()
    return lab


def pod_agent(lab: lab_mod.Lab, name: str, daemon: FakeDaemon, *, tools=(), parent=None, role=None) -> Agent:
    bundle = oc.build_models(role or name, transport=daemon.transport, availability=CLOUD)
    agent = Agent(name=name, model=bundle.router, tools=list(tools), callback_handler=None)
    agent.hooks.add_hook(lab.attach(agent, name, parent=parent, role=role or name))
    return agent


def capture() -> tuple[list[dict], callable]:
    events: list[dict] = []
    unsubscribe = harness_events.subscribe(lambda kind, payload: events.append(payload) if kind == "lab" else None)
    return events, unsubscribe


def test_statuses_and_model() -> None:
    lab = fresh()
    seen: list[str] = []

    @tool
    def probe(value: str) -> str:
        """Report the pod's status while the tool runs."""
        seen.append(lab.pods["debugger"].status)
        return value

    daemon = FakeDaemon(tool_reply("probe", '{"value": "x"}'), text_reply("done"))
    agent = pod_agent(lab, "debugger", daemon, tools=[probe])
    check(lab.pods["debugger"].status == "idle", "a new pod is IDLE")
    str(agent("go"))
    pod = lab.pods["debugger"]
    check(seen == ["executing"], "pod is EXECUTING while its tool runs")
    check(pod.status == "idle", "pod returns to IDLE when the invocation ends")
    check(pod.model == "mistral-large-4" and pod.effort == "max", f"pod shows the model + effort the request carried ({pod.model} @ {pod.effort})")
    check(pod.calls == 2 and pod.busy_ms >= 0, "two model calls counted")
    check([row["tool"] for row in pod.trace] == ["probe"] and pod.trace[0]["status"] == "completed", "tool trace records the call")
    check("done" in pod.stream, "stream tail holds the reply")


def test_delegation_link() -> None:
    lab = fresh()
    sub_daemon = FakeDaemon(text_reply("fixed"))
    debugger = pod_agent(lab, "debugger", sub_daemon, parent="orchestrator")
    top_daemon = FakeDaemon(tool_reply("debugger", '{"input": "fix it"}'), text_reply("all done"))
    lab.pod("orchestrator", role="orchestrator")
    bundle = oc.build_models("orchestrator", transport=top_daemon.transport, availability=CLOUD)
    orchestrator = Agent(name="orchestrator", model=bundle.router, tools=[debugger.as_tool(name="debugger", description="d")], callback_handler=None)
    orchestrator.hooks.add_hook(lab.attach(orchestrator, "orchestrator"))
    events, off = capture()
    try:
        str(orchestrator("please fix"))
    finally:
        off()
    link = lab.links[("orchestrator", "debugger")]
    check(link["kind"] == "delegation" and link["count"] == 1 and link["active"] is False, "orchestrator→debugger link was drawn and closed")
    check(any(any(l["from"] == "orchestrator" and l["to"] == "debugger" and l["active"] for l in e["links"]) for e in events), "an emitted snapshot showed the link active mid-flight")
    check(lab.pods["orchestrator"].model == "gemma4:31b" and lab.pods["orchestrator"].effort == "medium", "orchestrator pod = gemma4:31b @ medium")
    check(lab.pods["debugger"].model == "mistral-large-4" and lab.pods["debugger"].effort == "max", "debugger pod = mistral-large-4 @ max (own model, own budget)")


def test_estop_between_iterations() -> None:
    lab = fresh()
    ran: list[str] = []

    @tool
    def press_k(value: str) -> str:
        """Pretend the Lab Master presses K while this tool runs."""
        ran.append(value)
        lab.isolate("orchestrator")
        return value

    daemon = FakeDaemon(tool_reply("press_k", '{"value": "a"}'), text_reply("should never be requested"))
    agent = pod_agent(lab, "orchestrator", daemon, tools=[press_k])
    try:
        str(agent("go"))
    except Exception:
        pass
    pod = lab.pods["orchestrator"]
    check(ran == ["a"] and len(daemon.requests) == 1, f"E-Stop mid-loop: no second model call was made ({len(daemon.requests)} request)")
    check(pod.isolated and pod.status == "isolated", "pod is ISOLATED")
    before = len(daemon.requests)
    try:
        str(agent("again"))
    except Exception:
        pass
    check(len(daemon.requests) == before, "an isolated pod refuses a new invocation without calling the model")
    try:
        lab.inject("orchestrator", "hello")
        check(False, "guidance to an isolated pod is refused")
    except PermissionError:
        check(True, "guidance to an isolated pod is refused")
    lab.release("orchestrator")
    check(lab.pods["orchestrator"].status == "idle", "release returns it to IDLE")
    daemon.script = [text_reply("back at work")]
    daemon.requests.clear()
    check("back at work" in str(agent("welcome back")), "a released pod runs again — the E-Stop's cancel signal did not outlive it")
    check(len(daemon.requests) == 1, "…and reached the model")


def test_estop_of_an_idle_pod_leaves_no_stale_cancel() -> None:
    lab = fresh()
    daemon = FakeDaemon(text_reply("alive"))
    agent = pod_agent(lab, "architect", daemon)
    reply = lab.isolate("architect")
    check(reply["cancelled"] is False and not agent.cancel_signal.is_set(), "isolating an idle pod does not arm Strands' cancel signal")
    lab.release("architect")
    check("alive" in str(agent("go")), "the next turn after an idle E-Stop + release is not silently cancelled")


def test_estop_refuses_tools() -> None:
    lab = fresh()
    ran: list[str] = []

    @tool
    def danger(value: str) -> str:
        """Must not run."""
        ran.append(value)
        return value

    daemon = FakeDaemon(tool_reply("danger", '{"value": "x"}'), text_reply("after"))
    bundle = oc.build_models("debugger", transport=daemon.transport, availability=CLOUD)
    agent = Agent(name="debugger", model=bundle.router, tools=[danger], callback_handler=None)
    # isolate between the model asking for the tool and the tool running
    from strands.hooks import BeforeToolCallEvent

    agent.hooks.add_callback(BeforeToolCallEvent, lambda event: lab.isolate("debugger"))
    agent.hooks.add_hook(lab.attach(agent, "debugger"))
    try:
        str(agent("go"))
    except Exception:
        pass
    check(ran == [], "an isolated pod's tool call is cancelled, not run")
    check(lab.pods["debugger"].isolated, "…and it is isolated")


def test_dial() -> None:
    lab = fresh()
    agent = pod_agent(lab, "orchestrator", FakeDaemon())
    check(lab.dial_armed(agent, 0.95) is False, "no dial set: the dial arms nothing (the router's own gate is separate)")
    lab.set_dial("orchestrator", 0.5)
    check(lab.pods["orchestrator"].dial == 0.5, "dial set to 0.5")
    check(lab.dial_armed(agent, 0.6) and not lab.dial_armed(agent, 0.4) and not lab.dial_armed(agent, 0.5), "armed only when the score exceeds the dial")
    check(lab.dial_armed(agent, None) is False, "no score, nothing armed")
    lab.set_dial("orchestrator", 0.95)
    check(lab.pods["orchestrator"].dial is None and not lab.dial_armed(agent, 0.9), "a dial above the router's 0.8 gate cannot loosen anything")
    lab.set_dial("orchestrator", 0.01)
    check(lab.pods["orchestrator"].dial == lab_mod.DIAL_MIN, f"dial floor is {lab_mod.DIAL_MIN}")
    snapshot = lab.snapshot()["pods"][0]
    check(snapshot["dial"] == lab_mod.DIAL_MIN and snapshot["dial_default"] == 0.8, "snapshot carries the dial and its default")


def test_guidance_injection() -> None:
    lab = fresh()
    daemon = FakeDaemon(text_reply("one"), text_reply("two"))
    agent = pod_agent(lab, "architect", daemon)
    lab.inject("architect", "prefer the smallest change")
    str(agent("plan it"))
    last_user = [m for m in daemon.body(0)["messages"] if m["role"] == "user"][-1]
    text = json.dumps(last_user)
    check("prefer the smallest change" in text and "operator directive" in text, "queued guidance reaches the model on the next call, attributed")
    check("plan it" in text, "…merged into the user turn, not replacing it")
    str(agent("and again"))
    check("prefer the smallest change" not in json.dumps(daemon.body(1)["messages"][-1]), "guidance is delivered once")
    check(lab.pods["architect"].guidance == [], "queue drained")
    check(any(row["tool"] == "GUIDANCE" for row in lab.pods["architect"].trace), "delivery is visible in the tool trace")

    # a running loop takes it between iterations
    lab2 = fresh()

    @tool
    def slow(value: str) -> str:
        """While this runs the operator types guidance."""
        lab2.inject("debugger", "stop using sudo")
        return value

    daemon2 = FakeDaemon(tool_reply("slow", '{"value": "x"}'), text_reply("ok"))
    agent2 = pod_agent(lab2, "debugger", daemon2, tools=[slow])
    str(agent2("go"))
    check("stop using sudo" in json.dumps(daemon2.body(1)["messages"]), "guidance typed mid-run lands in the very next model call")
    check("stop using sudo" not in json.dumps(daemon2.body(0)["messages"]), "…and not before it was typed")


def test_hitl_and_threat() -> None:
    lab = fresh()
    agent = pod_agent(lab, "debugger", FakeDaemon())
    lab.set_threat("debugger", "shell", {"score": 0.92, "confidence": 0.88, "band": "destructive", "label": "destructive"})
    lab.stage("int-1", "debugger", "apply_patch", {"patch": "--- a/x\n+++ b/x\n@@\n-a\n+b\n"}, {"score": 0.92})
    check(lab.pods["debugger"].status == "contained", "a staged intervention puts the pod in CONTAINMENT")
    snap = lab.snapshot()
    check(snap["hitl"][0]["status"] == "pending" and snap["threat"]["score"] == 0.92 and snap["threat"]["confidence"] == 0.88, "snapshot lists the pending HITL row and the threat reading")
    lab.resolve("int-1", "approved")
    check(lab.snapshot()["hitl"][0]["status"] == "approved" and lab.pods["debugger"].status == "executing", "approval releases the pod to EXECUTING")
    lab.stage("int-2", "debugger", "shell", {"command": "rm -rf build"}, None)
    lab.resolve("int-2", "rejected")
    check(lab.pods["debugger"].status == "synthesizing", "a rejection sends the pod back to thinking")
    del agent


def test_a_parked_delegate_keeps_its_caller_busy() -> None:
    lab = fresh()
    lab.pod("orchestrator")
    lab.pod("debugger", parent="orchestrator")
    lab.set_status(lab.pods["orchestrator"], "idle")
    lab.stage("i1", "debugger", "shell", {"command": "rm -rf build"}, None)
    check(lab.pods["debugger"].status == "contained" and lab.pods["orchestrator"].status == "executing", "the caller is EXECUTING while its delegate waits on the human")
    check(lab.ancestors("debugger") == ["orchestrator"] and lab.ancestors("orchestrator") == [], "ancestors walk the parent chain")
    lab.isolate("orchestrator")
    lab.pods["debugger"].isolated = False
    lab.stage("i2", "debugger", "shell", {"command": "x"}, None)
    check(lab.pods["orchestrator"].status == "isolated", "an isolated caller stays ISOLATED")


def test_memory_attribution_and_redaction() -> None:
    lab = fresh()
    secret = "sk-lab-test-0123456789abcdef"
    secret_guard.register_secret(secret)

    @tool
    def recall(query: str) -> str:
        """Reads memory the way trace_store/kg do."""
        lab_mod.memory_hit("lancedb", query, [{"id": "t1", "label": "cargo test failed", "score": 0.12}])
        lab_mod.memory_hit("kg", query, [{"id": "n1", "label": "src/lib.rs"}, {"id": "n2", "label": "Cargo.toml"}])
        return "ok"

    lab_mod._LAB = lab  # the module-level shortcut the stores call
    try:
        daemon = FakeDaemon(tool_reply("recall", json.dumps({"query": f"how did {secret} fail"})), text_reply(f"the key was {secret}"))
        agent = pod_agent(lab, "debugger", daemon, tools=[recall])
        str(agent("go"))
    finally:
        lab_mod._LAB = None
    memory = list(lab.pods["debugger"].memory)
    check([m["source"] for m in memory] == ["lancedb", "kg"] and memory[1]["count"] == 2, "memory reads are attributed to the pod whose tool made them")
    blob = json.dumps(lab.snapshot(), default=str)
    check(secret not in blob, "no registered secret survives into the snapshot (stream, trace, memory query)")
    json.loads(json.dumps(lab.snapshot()))
    check(lab.snapshot()["version"] == lab_mod.SNAPSHOT_VERSION and "resources" in lab.snapshot(), "snapshot is plain JSON with a version and resources")


def test_detail_only_for_focus() -> None:
    lab = fresh()
    a = pod_agent(lab, "architect", FakeDaemon())
    b = pod_agent(lab, "debugger", FakeDaemon())
    lab.pods["architect"].system_prompt = "You are the Architect."
    check("system_prompt" not in lab.snapshot()["pods"][0], "unfocused pods carry no system prompt")
    lab.select("architect")
    rows = {row["name"]: row for row in lab.snapshot()["pods"]}
    check(rows["architect"]["system_prompt"] == "You are the Architect." and "system_prompt" not in rows["debugger"], "the focused pod carries its full context")
    del a, b


def test_emits_state_events() -> None:
    lab = fresh()
    events, off = capture()
    try:
        agent = pod_agent(lab, "orchestrator", FakeDaemon(text_reply("hi")))
        str(agent("hello"))
    finally:
        off()
    check(events and all(e["event"] == "state" and e["version"] == 1 for e in events), f"{len(events)} state notifications emitted")
    check(events[-1]["seq"] > events[0]["seq"], "sequence numbers increase")


def main() -> None:
    for test in (
        test_statuses_and_model,
        test_delegation_link,
        test_estop_between_iterations,
        test_estop_of_an_idle_pod_leaves_no_stale_cancel,
        test_estop_refuses_tools,
        test_dial,
        test_guidance_injection,
        test_hitl_and_threat,
        test_a_parked_delegate_keeps_its_caller_busy,
        test_memory_attribution_and_redaction,
        test_detail_only_for_focus,
        test_emits_state_events,
    ):
        test()
    print("\nALL OK")


if __name__ == "__main__":
    main()

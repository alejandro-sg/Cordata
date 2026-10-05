"""Ollama Cloud provider: registry, request shape, endpoint guard, reasoning throttle
and Strands routing. No daemon, no network — the HTTP layer is an in-process
``httpx.MockTransport`` so the exact request the model sends is what is asserted.

    backend/.venv/bin/python _ollama_cloud_smoke_test.py
"""

import asyncio
import json
import os

# The role table below is about Ollama Cloud models. On a machine with oMLX running, the
# architect role would otherwise go to the local Prism model first (omlx_provider) — a
# different test (_omlx_prism_smoke_test.py).
os.environ["SIERRALAB_PRISM"] = "off"

import provider_settings  # noqa: F401  (imported before agent_common, which chdirs — see main.py)

import httpx
from strands import Agent, tool
from strands.hooks import BeforeModelCallEvent

import ollama_cloud as oc

BASE = "http://localhost:11434/v1"


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


# ------------------------------------------------------------ fake daemon


def sse(*chunks: dict) -> bytes:
    return b"".join(b"data: " + json.dumps(chunk).encode() + b"\n\n" for chunk in chunks) + b"data: [DONE]\n\n"


def _chunk(delta: dict, finish: str | None = None, model: str = "m") -> dict:
    return {"id": "c", "object": "chat.completion.chunk", "created": 0, "model": model, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}


USAGE = {"id": "c", "object": "chat.completion.chunk", "created": 0, "model": "m", "choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}}


def text_reply(text: str = "hello") -> bytes:
    return sse(_chunk({"role": "assistant", "content": text}), _chunk({}, "stop"), USAGE)


def tool_reply(name: str = "echo", arguments: str = '{"value": "x"}') -> bytes:
    call = {"index": 0, "id": "call_1", "type": "function", "function": {"name": name, "arguments": arguments}}
    return sse(_chunk({"role": "assistant", "tool_calls": [call]}), _chunk({}, "tool_calls"), USAGE)


class FakeDaemon:
    """Records every request; answers from ``script`` (one reply per request, the last repeats)."""

    def __init__(self, *script):
        self.script = list(script) or [text_reply()]
        self.requests: list[httpx.Request] = []
        self.transport = httpx.MockTransport(self._handle)

    def _handle(self, request: httpx.Request) -> httpx.Response:
        self.requests.append(request)
        reply = self.script[min(len(self.requests) - 1, len(self.script) - 1)]
        if isinstance(reply, httpx.Response):
            return reply
        return httpx.Response(200, headers={"content-type": "text/event-stream"}, content=reply)

    def body(self, index: int = -1) -> dict:
        return json.loads(self.requests[index].content)


@tool
def echo(value: str) -> str:
    """Echo the value back."""
    return value


def run(agent: Agent, prompt: str = "hi", **state) -> str:
    return str(agent(prompt, invocation_state=state or None))


# ------------------------------------------------------------- phase 1


def test_registry() -> None:
    check(set(oc.REGISTRY) == {"glm-5.3-flash", "kimi-k2.7-code", "laguna-xs-2.1"}, "registry exposes the three tags")
    check(oc.resolve("glm-5.3-flash").wire_id == "glm-5.3-flash:cloud", "glm wire id carries :cloud (bare tag 404s on the daemon)")
    check(oc.resolve("kimi-k2.7-code:cloud").alias == "kimi-k2.7-code", "wire id resolves back to its alias")
    check(oc.resolve("laguna-xs-2.1").hosting == "local" and oc.resolve("laguna-xs-2.1").pulls_weights, "laguna is local-weights, flagged as needing a pull")
    check(oc.CLOUD_ALIASES == ("glm-5.3-flash", "kimi-k2.7-code"), "only two tags are cloud-hosted")
    try:
        oc.resolve("gpt-9")
        check(False, "unknown model rejected")
    except oc.UnknownModel:
        check(True, "unknown model rejected")


def test_request_targets_chat_completions() -> None:
    daemon = FakeDaemon()
    model = oc.OllamaCloudModel("glm-5.3-flash", transport=daemon.transport)
    agent = Agent(model=model, callback_handler=None)
    check(run(agent).strip() == "hello", "Agent(model=OllamaCloudModel) round-trips a reply")
    check(len(daemon.requests) == 1, "one HTTP request for one turn")
    request = daemon.requests[0]
    check(request.method == "POST", "POST")
    check(str(request.url) == "http://localhost:11434/v1/chat/completions", f"targets /v1/chat/completions on the local daemon ({request.url})")
    check(request.headers["authorization"] == "Bearer ollama", "dummy bearer key sent")
    body = daemon.body()
    check(body["model"] == "glm-5.3-flash:cloud", "wire model id is the :cloud tag")
    check(body["stream"] is True, "streaming request")
    check("reasoning_effort" not in body, "no reasoning_effort when none configured (provider default)")
    check({str(r.url.path) for r in daemon.requests} == {"/v1/chat/completions"}, "nothing but chat/completions was requested")


def test_endpoint_guard() -> None:
    async def go() -> None:
        calls: list[str] = []

        def inner(request: httpx.Request) -> httpx.Response:
            calls.append(request.url.path)
            return httpx.Response(200, json={})

        guard = oc.EndpointGuard(BASE, httpx.MockTransport(inner))
        async with httpx.AsyncClient(transport=guard) as client:
            check((await client.post(f"{BASE}/chat/completions", json={})).status_code == 200, "chat/completions passes the guard")
            for method, url in (
                ("POST", "http://localhost:11434/api/pull"),
                ("POST", "http://localhost:11434/api/chat"),
                ("GET", f"{BASE}/models"),
                ("GET", f"{BASE}/chat/completions"),
                ("POST", "http://evil.example/v1/chat/completions"),
            ):
                try:
                    await client.request(method, url, json={"model": "x"})
                    check(False, f"guard blocks {method} {url}")
                except oc.ForbiddenEndpoint:
                    check(True, f"guard blocks {method} {url}")
        check(calls == ["/v1/chat/completions"], "blocked requests never reached the network layer")

    asyncio.run(go())


def test_local_only_is_never_pulled() -> None:
    reason_for = lambda spec: None if spec.hosting == "cloud" else "not installed"  # noqa: E731
    bundle = oc.build_models("orchestrator", availability=reason_for)
    check(set(bundle.models) == set(oc.CLOUD_ALIASES), "router candidates are the cloud tags only")
    check("laguna-xs-2.1" in bundle.unavailable, "laguna is reported unavailable")
    reason = oc.default_availability("http://127.0.0.1:1/v1")(oc.REGISTRY["laguna-xs-2.1"])  # unreachable daemon: nothing installed
    check(bool(reason) and "never runs `ollama pull`" in reason, "the reason says it will not pull")
    try:
        oc.build_models(model="laguna-xs-2.1", availability=reason_for)
        check(False, "explicitly requesting an uninstalled local model raises")
    except oc.ModelUnavailable:
        check(True, "explicitly requesting an uninstalled local model raises ModelUnavailable (no pull)")
    present = oc.build_models(model="laguna-xs-2.1", availability=lambda spec: None)
    check(list(present.models)[0] == "laguna-xs-2.1", "an installed local model is routable and declared first")


# ------------------------------------------------------------- phase 2


def test_effort_validation() -> None:
    check(tuple(oc.REASONING_LEVELS) == ("low", "medium", "high", "max"), "four effort levels")
    check(oc.normalize_effort(" HIGH ") == "high", "levels are case/space-insensitive")
    check(oc.normalize_effort(None) is None, "None means 'omit'")
    for bad in ("minimal", "extreme", ""):
        try:
            oc.normalize_effort(bad)
            check(False, f"rejects {bad!r}")
        except ValueError:
            check(True, f"rejects {bad!r}")


def test_effort_in_payload() -> None:
    for level in oc.REASONING_LEVELS:
        daemon = FakeDaemon()
        run(Agent(model=oc.OllamaCloudModel("kimi-k2.7-code", reasoning_effort=level, transport=daemon.transport), callback_handler=None))
        check(daemon.body()["reasoning_effort"] == level, f"reasoning_effort={level} lands at the top level of the JSON body")


def test_dynamic_effort_between_invocations() -> None:
    daemon = FakeDaemon()
    model = oc.OllamaCloudModel("glm-5.3-flash", reasoning_effort="low", transport=daemon.transport)
    agent = Agent(model=model, callback_handler=None)
    run(agent, "one")
    model.set_reasoning_effort("max")
    run(agent, "two")
    check([daemon.body(i)["reasoning_effort"] for i in (0, 1)] == ["low", "max"], "set_reasoning_effort between calls: low then max on the wire")
    model.set_reasoning_effort(None)
    run(agent, "three")
    check("reasoning_effort" not in daemon.body(2), "set_reasoning_effort(None) drops the field again")


def test_dynamic_effort_between_loop_iterations() -> None:
    """One user turn, two model calls (tool call, then the answer): the throttle hook
    changes the level in between and the second HTTP request carries it."""
    daemon = FakeDaemon(tool_reply(), text_reply("done"))
    model = oc.OllamaCloudModel("glm-5.3-flash", transport=daemon.transport)
    seen: list[int] = []

    def policy(event: BeforeModelCallEvent):
        seen.append(len(seen))
        return "low" if len(seen) == 1 else "max"

    agent = Agent(model=model, tools=[echo], hooks=[oc.ReasoningThrottle(model.reasoning, policy)], callback_handler=None)
    check(run(agent, "use echo").strip() == "done", "agent loop ran tool call then answered")
    check(len(daemon.requests) == 2, "two model calls inside one invocation")
    check([daemon.body(i)["reasoning_effort"] for i in (0, 1)] == ["low", "max"], "effort moved low -> max between loop iterations")
    check(model.reasoning_effort == "max", "model reports the live level")


def test_role_budgets() -> None:
    expected = {"architect": "high", "debugger": "max", "observer": "low", "orchestrator": "low"}
    for role, level in expected.items():
        daemon = FakeDaemon()
        bundle = oc.build_models(role, transport=daemon.transport, availability=lambda s: None if s.hosting == "cloud" else "no")
        check(bundle.control.level == level, f"{role} budget is {level}")
    check(oc.ROLES.get("debugger").model == "kimi-k2.7-code", "debugger defaults to kimi-k2.7-code")
    table = oc.RoleTable()
    table.set("architect", effort="max")
    check(table.get("architect").effort == "max" and table.get("architect").model == "glm-5.3-flash", "RoleTable.set changes effort only")
    table.set("architect", model="kimi-k2.7-code")
    check(table.get("architect").model == "kimi-k2.7-code" and table.get("architect").effort == "max", "RoleTable.set changes model only")
    check(oc._parse_override("x", "kimi-k2.7-code:max", None) == oc.RoleProfile("x", "kimi-k2.7-code", "max"), "env override: model:effort")
    check(oc._parse_override("x", ":high", oc.RoleProfile("x", "glm-5.3-flash", "low")) == oc.RoleProfile("x", "glm-5.3-flash", "high"), "env override: :effort only")


# ------------------------------------------------------------- phase 3 (offline)


CLOUD_ONLY = lambda spec: None if spec.hosting == "cloud" else "no"  # noqa: E731


def test_router_picks_by_role() -> None:
    for name, wire, effort in (("debugger", "kimi-k2.7-code:cloud", "max"), ("architect", "glm-5.3-flash:cloud", "high"), ("orchestrator", "glm-5.3-flash:cloud", "low")):
        daemon = FakeDaemon()
        bundle = oc.build_models(name, transport=daemon.transport, availability=CLOUD_ONLY)
        run(Agent(name=name, model=bundle.router, callback_handler=None))
        body = daemon.body()
        check(body["model"] == wire and body["reasoning_effort"] == effort, f"router: {name} -> {wire} @ {effort}")


def test_router_per_invocation_swap() -> None:
    daemon = FakeDaemon()
    bundle = oc.build_models("orchestrator", transport=daemon.transport, availability=CLOUD_ONLY)
    agent = Agent(name="orchestrator", model=bundle.router, callback_handler=None)
    run(agent, "a")
    run(agent, "b", ollama_model="kimi-k2.7-code")
    run(agent, "c")
    check([daemon.body(i)["model"] for i in range(3)] == ["glm-5.3-flash:cloud", "kimi-k2.7-code:cloud", "glm-5.3-flash:cloud"], "invocation_state['ollama_model'] swaps the model for that call only")


def test_router_delegation_override() -> None:
    daemon = FakeDaemon()
    bundle = oc.build_models("debugger", model="glm-5.3-flash", effort="medium", transport=daemon.transport, availability=CLOUD_ONLY)
    run(Agent(name="debugger", model=bundle.router, callback_handler=None))
    body = daemon.body()
    check(body["model"] == "glm-5.3-flash:cloud" and body["reasoning_effort"] == "medium", "build_models(model=, effort=) overrides the role profile for one agent")


def test_router_failover() -> None:
    def respond(request: httpx.Request) -> httpx.Response:
        model = json.loads(request.content)["model"]
        if model == "kimi-k2.7-code:cloud":
            return httpx.Response(404, json={"error": {"message": "model not found", "type": "not_found_error"}})
        return httpx.Response(200, headers={"content-type": "text/event-stream"}, content=text_reply("from glm"))

    seen: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(json.loads(request.content)["model"])
        return respond(request)

    bundle = oc.build_models("debugger", transport=httpx.MockTransport(handler), availability=CLOUD_ONLY)
    agent = Agent(name="debugger", model=bundle.router, callback_handler=None)
    check(run(agent).strip() == "from glm", "a failing candidate falls through to the next")
    check(seen[0] == "kimi-k2.7-code:cloud" and seen[-1] == "glm-5.3-flash:cloud", f"kimi tried first, glm served it ({seen})")


def test_switch_model() -> None:
    daemon = FakeDaemon()
    model = oc.OllamaCloudModel("glm-5.3-flash", transport=daemon.transport)
    model.switch_model("kimi-k2.7-code", availability=CLOUD_ONLY)
    agent = Agent(model=model, callback_handler=None)
    run(agent)
    check(daemon.body()["model"] == "kimi-k2.7-code:cloud", "switch_model repoints the next request")
    try:
        model.switch_model("laguna-xs-2.1", availability=CLOUD_ONLY)
        check(False, "switch to an uninstalled local model refused")
    except oc.ModelUnavailable:
        check(True, "switch to an uninstalled local model refused")
    check(model.get_config()["model_id"] == "kimi-k2.7-code:cloud", "a refused switch leaves the model untouched")


def test_picker_and_config_swap() -> None:
    daemon = FakeDaemon()
    model = oc.OllamaCloudModel("glm-5.3-flash", transport=daemon.transport)
    seen: list[dict] = []
    off = oc.subscribe_requests(seen.append)
    try:
        agent = Agent(model=model, callback_handler=None)
        run(agent)
        model.update_config(model_id="kimi-k2.7-code:cloud")  # the model picker's path: no switch_model
        run(agent)
    finally:
        off()
    check([s["alias"] for s in seen] == ["glm-5.3-flash", "kimi-k2.7-code"], "request telemetry follows the live model_id, not a stale spec")
    check(model.current_spec.alias == "kimi-k2.7-code", "current_spec reads the config")
    offered = [spec.alias for spec in oc.routable(availability=lambda spec: None if spec.hosting == "cloud" else "no")]
    check(offered == ["glm-5.3-flash", "kimi-k2.7-code"], "a picker offers cloud tags only while the local tag is not installed")
    check([spec.alias for spec in oc.routable(availability=lambda spec: None)] == list(oc.REGISTRY), "…and the local one once it is")


def test_provider_integration() -> None:
    import agent_common
    import settings
    import specialists
    import sub_agent_common as sac
    from strands.models.routing import ModelRouter

    previous = (agent_common.ACTIVE_PROVIDER, settings.MODEL_TIER_STRONG, settings.MODEL_TIER_FAST)
    try:
        agent_common.ACTIVE_PROVIDER = "ollama_cloud"
        settings.MODEL_TIER_STRONG = settings.MODEL_TIER_FAST = ""

        orchestrator = agent_common.build_model()
        check(isinstance(orchestrator, oc.OllamaCloudModel) and orchestrator.spec.alias == "glm-5.3-flash", "build_model(): orchestrator is glm-5.3-flash")
        check(orchestrator.reasoning_effort == "low", "orchestrator runs at low effort")
        check(orchestrator.get_config()["params"]["max_tokens"] == agent_common._DEFAULT_MAX_TOKENS, "agent_common's max_tokens carried over")
        check(agent_common.build_model(model_id="kimi-k2.7-code").spec.alias == "kimi-k2.7-code", "build_model(model_id=alias) picks that registry model")

        router = sac.build_sub_agent_model("strong", role="debugger")
        check(isinstance(router, ModelRouter), "a roled sub-agent gets Strands' ModelRouter")
        check(router.default_model.spec.alias == "kimi-k2.7-code" and router.default_model.reasoning_effort == "max", "debugger: kimi-k2.7-code @ max")
        check(sac.build_sub_agent_model(None, role="architect").default_model.reasoning_effort == "high", "architect: high")
        check(sac.build_sub_agent_model(None, role="observer").default_model.reasoning_effort == "low", "observer: low")

        settings.MODEL_TIER_STRONG = "kimi-k2.7-code"
        check(sac.build_sub_agent_model("strong", role="architect").default_model.spec.alias == "kimi-k2.7-code", "a configured tier beats the role table")
        plain = sac.build_sub_agent_model("strong")
        check(isinstance(plain, oc.OllamaCloudModel) and plain.spec.alias == "kimi-k2.7-code", "roleless node: one plain model at its configured tier")
        settings.MODEL_TIER_STRONG = ""

        debugger = specialists.build_specialist(specialists.DEBUGGER)
        check(debugger.model.spec.alias == "kimi-k2.7-code" and debugger.model.reasoning_effort == "max", "specialists.DEBUGGER is built on kimi @ max")
        architect = specialists.build_specialist(specialists.ARCHITECT)
        check(architect.model.spec.alias == "glm-5.3-flash" and architect.model.reasoning_effort == "high", "specialists.ARCHITECT is built on glm @ high")

        state = provider_settings.get_provider_state()["ollama_cloud"]
        check(state["models"] == list(oc.REGISTRY) and state["roles"]["debugger"] == {"model": "kimi-k2.7-code", "effort": "max"}, "provider state exposes the registry and role table")
        check("ollama_cloud" in provider_settings._VALID_PROVIDERS, "ollama_cloud is a valid provider")

        import os

        os.environ["OLLAMA_BASE_URL"] = "http://127.0.0.1:1/v1"
        try:
            agent_common.ensure_provider_ready()
            check(False, "ensure_provider_ready fails fast without a daemon")
        except RuntimeError as error:
            check("ollama serve" in str(error), "ensure_provider_ready names the fix when the daemon is down")
        finally:
            del os.environ["OLLAMA_BASE_URL"]
    finally:
        agent_common.ACTIVE_PROVIDER, settings.MODEL_TIER_STRONG, settings.MODEL_TIER_FAST = previous


def main() -> None:
    for test in (
        test_registry,
        test_request_targets_chat_completions,
        test_endpoint_guard,
        test_local_only_is_never_pulled,
        test_effort_validation,
        test_effort_in_payload,
        test_dynamic_effort_between_invocations,
        test_dynamic_effort_between_loop_iterations,
        test_role_budgets,
        test_router_picks_by_role,
        test_router_per_invocation_swap,
        test_router_delegation_override,
        test_router_failover,
        test_switch_model,
        test_picker_and_config_swap,
        test_provider_integration,
    ):
        test()
    print("\nALL OK")


if __name__ == "__main__":
    main()

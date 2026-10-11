"""Phase 7: the oMLX provider, the Prism registry, load/unload leases, routing and failover.

Real HTTP to ``fake_omlx.FakeOmlx`` (whose load/unload state is real and whose log records,
for every chat request, whether the model was resident when it arrived), real Strands
``Agent`` loops, the real ``ModelRouter``. Nothing here touches the user's actual oMLX or
Ollama: every budget is built on the fake and a recording stub.

    backend/.venv/bin/python _omlx_prism_smoke_test.py
"""

import asyncio
import json
import os
import subprocess

import provider_settings  # noqa: F401  (imported before agent_common, which chdirs — see main.py)

import httpx
from strands import Agent

import context_digest_tool
import memory_budget as mb
import ollama_cli
import omlx_manager
import omlx_provider as op
import sub_agent_common
from fake_omlx import FakeOmlx
from harness_testing import ScriptedModel, check, text

ARCH = "prism-ml/Ternary-Bonsai-2-27B-mlx-2bit"
DIGEST = "prism-ml/Ternary-Bonsai-27B-mlx-2bit"
ARCH_WIRE = "Ternary-Bonsai-2-27B-mlx-2bit"
DIGEST_WIRE = "Ternary-Bonsai-27B-mlx-2bit"


def quiet_ollama() -> ollama_cli.OllamaCli:
    return ollama_cli.OllamaCli(binary="/bin/echo", runner=lambda argv, **kw: subprocess.CompletedProcess(argv, 0, "", ""), ps_reader=lambda: [])


def budget_for(fake: FakeOmlx, *, ceiling=48.0, reserve=10.0) -> mb.MemoryBudget:
    return mb.MemoryBudget(omlx_manager.OmlxManager(fake.base_url), quiet_ollama(), ceiling_gb=ceiling, reserve_gb=reserve)


def prism_agent(fake: FakeOmlx, role="architect", *, fallback=None, table=None, name=None) -> tuple[Agent, op.PrismModels]:
    built = op.build_models(role, fallback=fallback, base_url=fake.base_url, manager=omlx_manager.OmlxManager(fake.base_url))
    agent = Agent(name=name or role, model=built.router, tools=[], hooks=[built.lease_hooks(table)], callback_handler=None)
    return agent, built


# --------------------------------------------------------------------- registry


def test_registry() -> None:
    check(list(op.REGISTRY) == [ARCH, DIGEST], "the two spec identifiers, exactly")
    check(op.REGISTRY[ARCH].disk_gb == 8.41 and op.REGISTRY[DIGEST].disk_gb == 8.30, "with the spec's disk footprints (8.41 GB, 8.30 GB)")
    check(op.REGISTRY[ARCH].wire_id == ARCH_WIRE, "the wire id is the server's (no org prefix)")
    for form in (ARCH, ARCH_WIRE, "Ternary-Bonsai-2-27B-mlx-2bit"):
        check(op.resolve(form).alias == ARCH, f"resolves {form!r}")
    try:
        op.resolve("prism-ml/other")
    except op.UnknownPrismModel:
        check(True, "an unregistered model is refused")
    check(op.ROLE_DEFAULTS == {"architect": ARCH, "digest": DIGEST}, "architect planning → Bonsai-2, local context digest → Bonsai")


# ------------------------------------------------------ provider /v1 compatibility


def test_wire_compat() -> None:
    seen: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        chunk = {"id": "c", "object": "chat.completion.chunk", "created": 0, "model": "m", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "ok"}, "finish_reason": None}]}
        stop = {**chunk, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}
        usage = {"id": "c", "object": "chat.completion.chunk", "created": 0, "model": "m", "choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}
        body = b"".join(b"data: " + json.dumps(c).encode() + b"\n\n" for c in (chunk, stop, usage)) + b"data: [DONE]\n\n"
        return httpx.Response(200, headers={"content-type": "text/event-stream"}, content=body)

    model = op.PrismModel(ARCH, base_url="http://127.0.0.1:8080/v1", transport=httpx.MockTransport(handler), require_loaded=False)
    agent = Agent(model=model, system_prompt="you plan", tools=[], callback_handler=None)
    reply = str(agent("plan a refactor"))
    request = seen[0]
    body = json.loads(request.content)
    check(reply.strip() == "ok", "a completion round-trips through the provider")
    check(request.method == "POST" and request.url.path == "/v1/chat/completions" and request.url.port == 8080, "POST http://…:8080/v1/chat/completions")
    check(request.headers["authorization"] == "Bearer not-needed", "the dummy bearer the OpenAI client requires")
    check(body["model"] == ARCH_WIRE, f"the model is the server's id, not the registry alias ({body['model']})")
    check(body["stream"] is True and body["max_tokens"] == op.DEFAULT_MAX_TOKENS, "streams, with a generous max_tokens")
    check([m["role"] for m in body["messages"]] == ["system", "user"], "OpenAI chat shape: system then user messages")
    check(body["messages"][1]["content"] in ("plan a refactor", [{"type": "text", "text": "plan a refactor"}]), "…carrying the prompt")
    check(isinstance(op.PrismModel.__mro__[1].__name__, str) and op.PrismModel.__mro__[1].__name__ == "OpenAIModel", "it is strands.models.openai.OpenAIModel underneath")


def test_guard() -> None:
    async def go():
        guard = op.EndpointGuard("http://127.0.0.1:8000/v1")
        for method, url in (("POST", "http://127.0.0.1:8000/v1/models/x/load"), ("POST", "http://127.0.0.1:8000/v1/models/x/unload"), ("GET", "http://127.0.0.1:8000/v1/models/status"), ("POST", "http://evil.example/v1/chat/completions"), ("DELETE", "http://127.0.0.1:8000/v1/chat/completions")):
            try:
                await guard.handle_async_request(httpx.Request(method, url))
            except op.ForbiddenEndpoint:
                continue
            raise AssertionError(f"FAIL: {method} {url} should be blocked")

    asyncio.run(go())
    check(True, "load, unload, status, another host and a wrong verb are all blocked: a model cannot load anything")


# ---------------------------------------------------------------- the lease


def test_lease_ordering_and_residency() -> None:
    with FakeOmlx({ARCH_WIRE: [text("the plan")]}) as fake:
        table = op.LeaseTable(budget_for(fake))
        agent, _ = prism_agent(fake, table=table)
        check(not fake.loaded, "before the call, oMLX has nothing loaded (0 loaded)")
        reply = str(agent("plan it"))
        check(reply.strip() == "the plan", "the Prism model answered")
        check(fake.events == [f"load {ARCH_WIRE}", f"chat {ARCH_WIRE}", f"unload {ARCH_WIRE}"], f"load → completion → unload, in that order ({fake.events})")
        check(all(row["loaded"] for row in fake.chats()), "the model was resident when the completion arrived")
        check(not fake.loaded and not table.held(), "afterwards memory is free again and the lease is released")


def test_refcount_and_ownership() -> None:
    with FakeOmlx({ARCH_WIRE: [text("a"), text("b")]}) as fake:
        table = op.LeaseTable(budget_for(fake))
        first, _ = prism_agent(fake, table=table, name="node_1")
        second, _ = prism_agent(fake, table=table, name="node_2")

        async def both():
            return await asyncio.gather(first.invoke_async("x"), second.invoke_async("y"))

        asyncio.run(both())
        loads = [e for e in fake.events if e.startswith("load ")]
        unloads = [e for e in fake.events if e.startswith("unload ")]
        check(len(loads) == 1 and len(unloads) == 1, f"two concurrent graph nodes share one load and one unload ({fake.events})")
        check(fake.events[-1].startswith("unload"), "…and the unload is last")

    # A model that was already resident is not the lease's to unload.
    with FakeOmlx({ARCH_WIRE: [text("a")]}, loaded={ARCH_WIRE}) as fake:
        table = op.LeaseTable(budget_for(fake))
        agent, _ = prism_agent(fake, table=table)
        str(agent("x"))
        check(ARCH_WIRE in fake.loaded and not any(e.startswith("unload") for e in fake.events), "a model that was already loaded (the orchestrator's own) is left loaded")


def test_lease_makes_room() -> None:
    with FakeOmlx({ARCH_WIRE: [text("planned")]}, loaded={"Qwen2.5-Coder-32B-Instruct-4bit"}) as fake:
        fake.loaded["Qwen2.5-Coder-32B-Instruct-4bit"] -= 3600
        table = op.LeaseTable(budget_for(fake, ceiling=30.0, reserve=5.0))  # 25 GB budget: 19.35 + 8.41 does not fit
        agent, _ = prism_agent(fake, table=table)
        str(agent("plan"))
        check(fake.events[0] == "unload Qwen2.5-Coder-32B-Instruct-4bit" and fake.events[1] == f"load {ARCH_WIRE}", f"an idle model is stopped to make room BEFORE the Prism load ({fake.events[:2]})")


# ---------------------------------------------------------------- failover


def test_failover_when_the_lease_fails() -> None:
    cloud = ScriptedModel([text("cloud answer")], model_id="cloud")
    # A pinned 32B fills a 25 GB budget and cannot be evicted: the Prism model cannot be loaded.
    with FakeOmlx({ARCH_WIRE: [text("should never be asked")]}, loaded={"Qwen2.5-Coder-32B-Instruct-4bit"}, pinned={"Qwen2.5-Coder-32B-Instruct-4bit"}) as fake:
        table = op.LeaseTable(budget_for(fake, ceiling=30.0, reserve=5.0))
        agent, built = prism_agent(fake, fallback=cloud, table=table)
        check(built.has_fallback and [c.name for c in built.router.candidates] == [ARCH, DIGEST, "fallback"], "candidates: the role's Prism model, the other Prism model, then the old provider")
        reply = str(agent("plan"))
        check(reply.strip() == "cloud answer", "no room for the Prism model: the router fails over to the cloud candidate")
        check(not fake.chats(), "oMLX was never sent a completion for a model that is not loaded (no implicit load)")
        check(not any(e.startswith("load ") for e in fake.events), "…and never asked to load it")

    # oMLX unreachable mid-flight.
    dead = FakeOmlx({}).start()
    dead.stop()
    cloud = ScriptedModel([text("cloud answer 2")], model_id="cloud")
    built = op.build_models("architect", fallback=cloud, base_url=dead.base_url, manager=omlx_manager.OmlxManager(dead.base_url))
    agent = Agent(name="architect", model=built.router, tools=[], hooks=[built.lease_hooks(op.LeaseTable(mb.MemoryBudget(omlx_manager.OmlxManager(dead.base_url), quiet_ollama())))], callback_handler=None)
    check(str(agent("plan")).strip() == "cloud answer 2", "oMLX unreachable: the lease records an error, the request refuses, the cloud answers")


def test_failover_between_prism_models() -> None:
    """oMLX 0.6.4 on the dev machine cannot load Bonsai-2 (unsupported model type). The architect
    must land on the *other* Prism model — not skip straight to the cloud."""
    cloud = ScriptedModel([text("cloud must not answer")], model_id="cloud")
    with FakeOmlx({DIGEST_WIRE: [text("planned on the other Prism model")]}, broken={ARCH_WIRE}) as fake:
        table = op.LeaseTable(budget_for(fake))
        agent, _ = prism_agent(fake, fallback=cloud, table=table)
        reply = str(agent("plan"))
        check(reply.strip() == "planned on the other Prism model", "the preferred Prism model cannot load: the other one answers")
        check(fake.events == [f"load {DIGEST_WIRE}", f"chat {DIGEST_WIRE}", f"unload {DIGEST_WIRE}"], f"…loaded, used and unloaded ({fake.events})")
        check(cloud.calls == [], "the cloud candidate was never reached")
        check(not table.held() and not fake.loaded, "no lease is left held, nothing left resident")

    # Both broken → only then the cloud.
    cloud = ScriptedModel([text("cloud answered")], model_id="cloud")
    with FakeOmlx({}, broken={ARCH_WIRE, DIGEST_WIRE}) as fake:
        agent, _ = prism_agent(fake, fallback=cloud, table=op.LeaseTable(budget_for(fake)))
        check(str(agent("plan")).strip() == "cloud answered" and not fake.chats(), "both Prism models unloadable: now the cloud answers, and oMLX saw no completion")


def test_request_to_an_unloaded_model_is_refused() -> None:
    with FakeOmlx({ARCH_WIRE: [text("x")]}, autoload=True) as fake:
        model = op.PrismModel(ARCH, base_url=fake.base_url, manager=omlx_manager.OmlxManager(fake.base_url))
        agent = Agent(model=model, tools=[], callback_handler=None)
        try:
            agent("hello")
        except op.ModelNotLoaded as error:
            check("not loaded in oMLX" in str(error), "a Prism request to a non-resident model raises ModelNotLoaded")
        else:
            raise AssertionError("FAIL: the request should have been refused")
        check(not fake.chats() and not fake.loaded, "…and oMLX (which would have auto-loaded it) never saw the request")


# -------------------------------------------------- build_sub_agent_model wiring


def test_sub_agent_wiring() -> None:
    with FakeOmlx({ARCH_WIRE: [text("designed")], DIGEST_WIRE: [text("digest: 3 errors in parser.py")]}) as fake:
        os.environ["OMLX_PRISM_BASE_URL"] = fake.base_url
        os.environ.pop(op.PRISM_ENV, None)
        op._probe = None
        saved = (mb._DEFAULT, op._TABLE)
        mb._DEFAULT = budget_for(fake)
        op._TABLE = op.LeaseTable(mb._DEFAULT)
        try:
            model = sub_agent_common.build_sub_agent_model("strong", role="architect")
            names = [c.name for c in model.candidates]
            check(names[:2] == [ARCH, DIGEST] and names[-1] == "fallback", f"architect → Prism first, the provider's model as failover ({names})")
            check(len(op.hooks_for(model)) == 1, "…and it comes with its lease hooks")
            check(sub_agent_common.build_sub_agent_model("strong", role="debugger") is not model and not op.hooks_for(sub_agent_common.build_sub_agent_model("strong", role="debugger")), "a non-local role (debugger) is unchanged: no Prism, no lease")
            check(not op.hooks_for(sub_agent_common.build_sub_agent_model("fast")), "a roleless node is unchanged")

            # the architect specialist, as the harness builds it
            import specialists

            architect = specialists.build_specialist(specialists.ARCHITECT, model=model)
            str(architect("design the change"))
            check(fake.events[0] == f"load {ARCH_WIRE}" and fake.events[-1] == f"unload {ARCH_WIRE}", f"architect run: load, completion, unload ({fake.events})")

            # a graph node with a digest profile
            node_model = sub_agent_common.build_sub_agent_model("fast", role="digest")
            check([c.name for c in node_model.candidates][0] == DIGEST, "digest → the other Prism model")

            fake.events.clear()
            result = asyncio.run(context_digest_tool.digest("error error error\n" * 50, "errors", 50, model=node_model))
            check("3 errors in parser.py" in result, "context_digest returns the local model's digest")
            check(fake.events == [f"load {DIGEST_WIRE}", f"chat {DIGEST_WIRE}", f"unload {DIGEST_WIRE}"], f"…loading and unloading around it ({fake.events})")
            sent = json.dumps(fake.chats()[-1]["body"])
            check("DATA from a tool or a file" in sent and not fake.chats()[-1]["body"].get("tools"), "the digest agent is told its input is data and is given no tools")

            # off switches
            os.environ[op.PRISM_ENV] = "off"
            check(not op.hooks_for(sub_agent_common.build_sub_agent_model("strong", role="architect")), "SIERRALAB_PRISM=off: the architect runs on its old provider")
            os.environ.pop(op.PRISM_ENV)

            class Prefs:
                def get(self, key, default=None):
                    return False if key == "omlx.prism" else default

            check(op.enabled(Prefs()) is False, "omlx.prism=false switches it off")
            os.environ["OMLX_PRISM_BASE_URL"] = "http://127.0.0.1:9/v1"
            op._probe = None
            check(op.enabled() is False and not op.hooks_for(sub_agent_common.build_sub_agent_model("strong", role="architect")), "oMLX not answering: roles keep their old provider")
        finally:
            os.environ.pop("OMLX_PRISM_BASE_URL", None)
            mb._DEFAULT, op._TABLE = saved
            op._probe = None


def main() -> None:
    test_registry()
    test_wire_compat()
    test_guard()
    test_lease_ordering_and_residency()
    test_refcount_and_ownership()
    test_lease_makes_room()
    test_failover_when_the_lease_fails()
    test_failover_between_prism_models()
    test_request_to_an_unloaded_model_is_refused()
    test_sub_agent_wiring()
    print("\noMLX / Prism: all checks passed")


if __name__ == "__main__":
    main()

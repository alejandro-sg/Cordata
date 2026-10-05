"""LIVE check against the signed-in local Ollama daemon (needs network + `ollama
signin`; costs a few hundred cloud tokens). Not part of the offline suite.

Proves: streaming from each cloud tag through /v1/chat/completions, model +
reasoning_effort switching through Strands' ModelRouter on real traffic, a tool
call through the proxy, and that nothing here pulls weights (``ollama list`` is
unchanged, ``laguna-xs-2.1`` is refused rather than downloaded).

    backend/.venv/bin/python _ollama_cloud_live_test.py
"""

import asyncio
import json
import subprocess
import sys

import httpx
from strands import Agent, tool

import ollama_cloud as oc


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


def ollama_list() -> list[str]:
    out = subprocess.run(["ollama", "list"], capture_output=True, text=True, check=True).stdout
    return sorted(line.split()[0] for line in out.splitlines()[1:] if line.strip())


class Recorder(httpx.AsyncBaseTransport):
    """Real network, but every request/response status is written down."""

    def __init__(self):
        self.inner = httpx.AsyncHTTPTransport()
        self.calls: list[dict] = []

    async def handle_async_request(self, request: httpx.Request) -> httpx.Response:
        body = json.loads(request.content or b"{}")
        response = await self.inner.handle_async_request(request)
        self.calls.append({"path": request.url.path, "model": body.get("model"), "effort": body.get("reasoning_effort"), "stream": body.get("stream"), "status": response.status_code})
        return response

    async def aclose(self) -> None:
        await self.inner.aclose()


@tool
def add(a: int, b: int) -> int:
    """Add two integers."""
    return a + b


async def stream_text(agent: Agent, prompt: str, **state) -> tuple[str, int]:
    deltas, text = 0, ""
    async for event in agent.stream_async(prompt, invocation_state=state or None):
        if "data" in event:
            deltas += 1
            text += event["data"]
    return text, deltas


async def main() -> None:
    problem = oc.daemon_problem()
    if problem:
        print(f"SKIP: {problem}")
        sys.exit(2)
    before = ollama_list()
    print(f"local models before: {before or 'none'}")

    recorder = Recorder()
    bundle = oc.build_models("orchestrator", transport=recorder)
    check(set(bundle.models) == set(oc.CLOUD_ALIASES), f"candidates: {list(bundle.models)}; unavailable: {list(bundle.unavailable)}")
    agent = Agent(name="orchestrator", model=bundle.router, callback_handler=None)

    # 1. glm via the router's role default, low effort, streamed
    text, deltas = await stream_text(agent, "Reply with one short sentence about the sea.")
    check(bool(text.strip()) and deltas > 1, f"glm streamed {deltas} deltas: {text.strip()[:60]!r}")
    first = recorder.calls[-1]
    check(first["path"] == "/v1/chat/completions" and first["status"] == 200 and first["stream"] is True, f"glm request ok {first}")
    check(first["model"] == "glm-5.3-flash:cloud" and first["effort"] == "low", "orchestrator = glm-5.3-flash @ low on the wire")

    # 2. swap to kimi for one call, and raise the effort live
    bundle.control.set("max")
    text, deltas = await stream_text(agent, "Reply with one short sentence about mountains.", ollama_model="kimi-k2.7-code")
    check(bool(text.strip()) and deltas > 1, f"kimi streamed {deltas} deltas: {text.strip()[:60]!r}")
    second = recorder.calls[-1]
    check(second["model"] == "kimi-k2.7-code:cloud" and second["effort"] == "max" and second["status"] == 200, f"swapped to kimi @ max {second}")

    # 3. back to the default for the next call
    await stream_text(agent, "Say ok.")
    check(recorder.calls[-1]["model"] == "glm-5.3-flash:cloud", "the swap did not stick past its call")

    # 4. a debugger-role agent through the same machinery, with a tool call
    debugger_models = oc.build_models("debugger", transport=recorder)
    debugger = Agent(name="debugger", model=debugger_models.router, tools=[add], callback_handler=None)
    n = len(recorder.calls)
    text, _ = await stream_text(debugger, "Use the add tool to compute 17 + 25, then state the result.")
    check("42" in text, f"tool call round-trips through the proxy: {text.strip()[:80]!r}")
    new = recorder.calls[n:]
    check(len(new) >= 2 and all(c["model"] == "kimi-k2.7-code:cloud" and c["effort"] == "max" for c in new), f"debugger = kimi @ max across {len(new)} loop iterations")

    # 5. only chat/completions was ever requested, all 200
    check({c["path"] for c in recorder.calls} == {"/v1/chat/completions"}, "only /v1/chat/completions was requested")
    check(all(c["status"] == 200 for c in recorder.calls), "every request returned 200")

    # 6. laguna: refused, never pulled
    try:
        oc.build_models(model="laguna-xs-2.1")
        check("laguna-xs-2.1" in ollama_list(), "laguna routable only because it is already installed")
    except oc.ModelUnavailable as error:
        check("never runs `ollama pull`" in str(error), "laguna-xs-2.1 refused with a no-pull message")
    direct = Agent(model=oc.OllamaCloudModel("laguna-xs-2.1", transport=recorder), callback_handler=None)
    try:
        await stream_text(direct, "hi")
        check("laguna-xs-2.1" in ollama_list(), "laguna answered (it was already installed)")
    except Exception as error:
        check("not found" in str(error).lower() or "404" in str(error), f"direct laguna request is a clean 404, not a download ({type(error).__name__})")

    after = ollama_list()
    check(after == before, f"`ollama list` unchanged — no weights pulled ({after or 'none'})")
    print("\nLIVE OK")


if __name__ == "__main__":
    asyncio.run(main())

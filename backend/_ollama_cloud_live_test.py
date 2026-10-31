"""LIVE check against the signed-in local Ollama daemon (needs network + `ollama
signin`; costs a few hundred cloud tokens). Not part of the offline suite.

Proves: streaming from each cloud tag through /v1/chat/completions, model +
reasoning_effort switching through Strands' ModelRouter on real traffic, a tool
call through the proxy, every model of the zero-download roster answering, and that
nothing here pulls weights (``ollama list`` is unchanged — empty on a cloud-only machine).

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

    # 1. the orchestrator's role default (gemma4:31b @ medium), streamed
    text, deltas = await stream_text(agent, "Reply with one short sentence about the sea.")
    check(bool(text.strip()) and deltas > 1, f"gemma4 streamed {deltas} deltas: {text.strip()[:60]!r}")
    first = recorder.calls[-1]
    check(first["path"] == "/v1/chat/completions" and first["status"] == 200 and first["stream"] is True, f"gemma4 request ok {first}")
    check(first["model"] == "gemma4:31b:cloud" and first["effort"] == "medium", "orchestrator = gemma4:31b @ medium on the wire")

    # 2. swap to minimax for one call, and raise the effort live
    bundle.control.set("max")
    text, deltas = await stream_text(agent, "Reply with one short sentence about mountains.", ollama_model="minimax-m3.1-flash")
    check(bool(text.strip()), f"minimax streamed {deltas} deltas: {text.strip()[:60]!r}")
    second = recorder.calls[-1]
    check(second["model"] == "minimax-m3.1-flash:cloud" and second["effort"] == "max" and second["status"] == 200, f"swapped to minimax @ max {second}")

    # 3. back to the default for the next call
    await stream_text(agent, "Say ok.")
    check(recorder.calls[-1]["model"] == "gemma4:31b:cloud", "the swap did not stick past its call")

    # 4. a debugger-role agent through the same machinery, with a tool call
    debugger_models = oc.build_models("debugger", transport=recorder)
    debugger = Agent(name="debugger", model=debugger_models.router, tools=[add], callback_handler=None)
    n = len(recorder.calls)
    text, _ = await stream_text(debugger, "Use the add tool to compute 17 + 25, then state the result.")
    check("42" in text, f"tool call round-trips through the proxy: {text.strip()[:80]!r}")
    new = recorder.calls[n:]
    check(len(new) >= 2 and all(c["model"] == "mistral-large-4:cloud" and c["effort"] == "max" for c in new), f"debugger = mistral-large-4 @ max across {len(new)} loop iterations")

    # 5. every model of the roster answers through its :cloud tag, at an effort it accepts
    for alias in oc.CLOUD_ALIASES:
        single = Agent(model=oc.OllamaCloudModel(alias, reasoning_effort="medium", transport=recorder), callback_handler=None)
        text, _ = await stream_text(single, "Say ok.")
        call = recorder.calls[-1]
        check(call["status"] == 200 and bool(text.strip()), f"{alias} answered at {call['effort']} ({text.strip()[:30]!r})")

    # 6. only chat/completions was ever requested, all 200
    check({c["path"] for c in recorder.calls} == {"/v1/chat/completions"}, "only /v1/chat/completions was requested")
    check(all(c["status"] == 200 for c in recorder.calls), "every request returned 200")

    after = ollama_list()
    check(after == before, f"`ollama list` unchanged — no weights pulled ({after or 'none'})")
    print("\nLIVE OK")


if __name__ == "__main__":
    asyncio.run(main())

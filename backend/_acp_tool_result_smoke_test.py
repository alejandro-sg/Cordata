"""Regression check: acp_server.py must report a finished tool call back to
the ACP client (a terminal ToolCallProgress with status + content), against the
Strands version actually installed. stream_async no longer yields a
"tool_result" event there — the result rides in the user-role "message"
event — and acp_server used to listen only for the old shape, so every
ToolCallStart stayed "pending" forever in Zed or any other client.

No LLM, no network: a scripted model drives the real CordataAcpAgent.prompt
loop and a fake connection records what would have gone to the client.

    backend/.venv/bin/python _acp_tool_result_smoke_test.py
"""

import asyncio
import os
os.environ.setdefault("CORDATA_REFLECT", "off")  # scripted/live test turns are not lessons (reflect.py)

import acp
from strands import Agent, tool
from strands.models.model import Model

import acp_server


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


@tool
def good_tool() -> str:
    """Succeeds."""
    return "hello-from-tool"


@tool
def bad_tool() -> str:
    """Raises."""
    raise RuntimeError("tool exploded")


class Scripted(Model):
    """Turn 1: call `tool_name`. Turn 2: reply 'done'."""

    def __init__(self, tool_name: str) -> None:
        self.tool_name = tool_name
        self.turn = 0

    def update_config(self, **kw):
        pass

    def get_config(self):
        return {}

    async def structured_output(self, *a, **k):
        raise NotImplementedError
        yield  # pragma: no cover

    async def stream(self, messages, tool_specs=None, system_prompt=None, **kw):
        self.turn += 1
        yield {"messageStart": {"role": "assistant"}}
        if self.turn == 1:
            yield {"contentBlockStart": {"start": {"toolUse": {"toolUseId": "t1", "name": self.tool_name}}}}
            yield {"contentBlockDelta": {"delta": {"toolUse": {"input": "{}"}}}}
            yield {"contentBlockStop": {}}
            yield {"messageStop": {"stopReason": "tool_use"}}
        else:
            yield {"contentBlockStart": {"start": {}}}
            yield {"contentBlockDelta": {"delta": {"text": "done"}}}
            yield {"contentBlockStop": {}}
            yield {"messageStop": {"stopReason": "end_turn"}}
        yield {"metadata": {"usage": {"inputTokens": 1, "outputTokens": 1, "totalTokens": 2}, "metrics": {"latencyMs": 1}}}


class FakeConn:
    def __init__(self) -> None:
        self.updates: list = []

    async def session_update(self, session_id, update, **kw):
        self.updates.append(update)


async def run(tool_name: str) -> list:
    server = acp_server.CordataAcpAgent()
    conn = FakeConn()
    server._conn = conn
    agent = Agent(model=Scripted(tool_name), tools=[good_tool, bad_tool], callback_handler=None)
    handler = acp_server._ApprovalHandler()
    router_hooks = acp_server.RouterHooks(acp_server.LayaRouter(predict=lambda *a, **k: None))
    heal = acp_server.SelfHealHooks(config=acp_server.HealConfig(enabled=False), diagnose=lambda *a, **k: None)
    server._sessions["s1"] = acp_server._Session(agent, handler, os.getcwd(), router_hooks, heal)
    response = await server.prompt("s1", [acp.text_block("go")])
    assert response.stopReason == "end_turn", response
    return conn.updates


def tool_updates(updates: list) -> list:
    return [u for u in updates if type(u).__name__ in ("ToolCallStart", "ToolCallProgress")]


def main() -> None:
    updates = asyncio.run(run("good_tool"))
    calls = tool_updates(updates)
    check([type(u).__name__ for u in calls][:1] == ["ToolCallStart"], "the tool call is announced (ToolCallStart)")
    finals = [u for u in calls if type(u).__name__ == "ToolCallProgress" and getattr(u, "status", None) == "completed"]
    check(len(finals) >= 1, "a finished tool call produces a completed ToolCallProgress (was: stuck pending)")
    check("hello-from-tool" in str(finals[0].content), "…carrying the tool's result text")
    check(finals[0].tool_call_id == calls[0].tool_call_id, "…for the same tool call id")
    texts = [getattr(getattr(u, "content", None), "text", "") for u in updates if type(u).__name__ == "AgentMessageChunk"]
    check("done" in "".join(texts), "the reply after the tool still streams")

    failed = [u for u in tool_updates(asyncio.run(run("bad_tool"))) if getattr(u, "status", None) == "failed"]
    check(len(failed) >= 1, "a tool that raised produces a failed ToolCallProgress")
    print("\nPASS: acp_server reports finished tool calls (completed and failed) to the client")


if __name__ == "__main__":
    main()

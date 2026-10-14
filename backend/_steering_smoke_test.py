"""Steering a running turn (steering.py): a note waits for the next model call, lands as a user text block of its
own (never inside a tool result), and whatever no call saw is handed back when the turn ends.

    backend/.venv/bin/python _steering_smoke_test.py
"""

from __future__ import annotations

import asyncio
import os

os.environ["CORDATA_PRISM"] = "off"

import provider_settings  # noqa: F401,E402

import steering  # noqa: E402
from _acp_harness_smoke_test import fresh_server, laya_answer, project  # noqa: E402
from harness_testing import check  # noqa: E402


class FakeAgent:
    def __init__(self, messages):
        self.messages = messages


class FakeEvent:
    def __init__(self, agent):
        self.agent = agent


def unit() -> None:
    print("=== 1. the hook ===")
    s = steering.Steering()
    check(not s.add("   "), "an empty note is refused")
    check(s.add("use the v2 endpoint") and s.add("skip the slow tests") and s.pending() == 2, "notes wait")
    tool_result = {"toolResult": {"toolUseId": "t1", "content": [{"text": "ok"}], "status": "success"}}
    agent = FakeAgent([{"role": "user", "content": [{"text": "do it"}]}, {"role": "assistant", "content": [{"toolUse": {"toolUseId": "t1", "name": "shell", "input": {}}}]}, {"role": "user", "content": [tool_result]}])
    s._before_model(FakeEvent(agent))
    last = agent.messages[-1]["content"]
    check(last[0] == tool_result and last[0]["toolResult"]["content"] == [{"text": "ok"}], "the tool result is untouched")
    check(last[-1]["text"].startswith(steering.PREFIX) and "use the v2 endpoint" in last[-1]["text"] and "skip the slow tests" in last[-1]["text"], "the notes are a text block of their own after it")
    check(s.pending() == 0 and s.delivered == 2, "delivered once")
    s._before_model(FakeEvent(agent))
    check(len(agent.messages[-1]["content"]) == 2, "nothing is added twice")
    s.add("late note")
    s._before_model(FakeEvent(FakeAgent([{"role": "assistant", "content": [{"text": "done"}]}])))
    check(s.drain() == ["late note"] and s.drain() == [], "a note after the last call is handed back once, at the end")
    for i in range(steering.MAX_PENDING):
        s.add(f"n{i}")
    check(not s.add("one too many"), "the queue is bounded")
    s.drain()
    s.add("x" * (steering.MAX_NOTE_CHARS + 50))
    check(len(s.drain()[0]) == steering.MAX_NOTE_CHARS, "a long note is cut")


async def through_the_server() -> None:
    print("\n=== 2. through the server ===")
    server, _ = fresh_server(laya_answer("code", {"code": 0.97, "answer": 0.03}))
    cwd = project("steer-me")
    session = await server.new_session(cwd=str(cwd), mcp_servers=[])
    sid = session.session_id
    added = await server.ext_method("_cordata/steer", {"sessionId": sid, "text": "prefer small diffs"})
    check(added["ok"] and added["pending"] == 1, f"_cordata/steer queues a note ({added})")
    drained = await server.ext_method("_cordata/steer/drain", {"sessionId": sid})
    check(drained["notes"] == ["prefer small diffs"], "_cordata/steer/drain hands it back")
    empty = await server.ext_method("_cordata/steer", {"sessionId": sid, "text": ""})
    check(not empty["ok"], "an empty steer is refused with a reason")


if __name__ == "__main__":
    unit()
    asyncio.run(through_the_server())
    print("\nALL OK")

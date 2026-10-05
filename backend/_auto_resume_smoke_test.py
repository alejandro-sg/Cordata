"""Auto-resume: a turn a crash interrupted continues by itself — within limits.

Through the real ACP server, with a scripted model (no provider):

* a session that loads with an interrupted turn says it is **resuming automatically** and tells the client
  (``_sierralab/resume``, ``auto: true``); the client's auto-resume prompt then runs the pending tool and
  completes the turn, and the checkpoint and its counter are cleared;
* it **stops trying**: a turn that dies at the same checkpoint every time is resumed twice and then left for a
  person — the load says so, the auto-resume prompt is refused, and the manual ``/resume-turn`` still works;
* a checkpoint **too old** (over thirty minutes) is not resumed on its own, and ``selfHeal.autoResume: false``
  turns it off — both fall back to the manual hint;
* a turn that **got further** before dying has a new checkpoint, so its count starts again.

    backend/.venv/bin/python _auto_resume_smoke_test.py
"""

import asyncio
import json
import os
import time

import acp

import acp_server
import agent_common
import settings_store
from _acp_harness_smoke_test import call, fresh_server, laya_answer, orchestrator_model, project, settle, text, texts
from harness_testing import check

ROUTE = laya_answer("code", {"code": 0.5, "answer": 0.5})


def resume_events(conn) -> list[dict]:
    return [p for m, p in conn.ext if m == "sierralab/resume"]


async def interrupted(cwd, ran):
    """A session whose turn was killed right after the model asked for a tool: the checkpoint on disk is real."""
    orchestrator_model([call("slow_step", {}), text("finished after resume")])
    server, _conn = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(cwd))).session_id
    agent = server._sessions[sid].agent
    result = None
    async for event in agent.stream_async("do the slow step"):
        if "result" in event:
            result = event["result"]
    acp_server._checkpoint_path(sid).write_text(json.dumps({"checkpoint": result.checkpoint.to_dict()}))
    return sid


async def main() -> None:
    from strands import tool

    ran: list[int] = []

    @tool
    def slow_step() -> str:
        """A step that must not be lost."""
        ran.append(1)
        return "step done"

    original = agent_common._STATIC_TOOLS
    agent_common._STATIC_TOOLS = [*original, slow_step]
    try:
        cwd = project("auto-resume")
        sid = await interrupted(cwd, ran)
        check(ran == [] and acp_server._checkpoint_path(sid).exists(), "the process 'died' before the tool ran")

        orchestrator_model([text("finished after resume")])
        server, conn = fresh_server(ROUTE)
        await server.load_session(cwd=str(cwd), session_id=sid)
        await settle()
        notice = resume_events(conn)
        check(notice and notice[0]["auto"] is True and notice[0]["attempts"] == 0 and notice[0]["max"] == 2, f"the client is told it can resume on its own ({notice})")
        check("Resuming it automatically." in texts(conn) and "Send /resume-turn" not in texts(conn), "…and the person is told it is happening")
        response = await server.prompt(sid, [acp.text_block(acp_server.AUTO_RESUME_MARKER)])
        await settle()
        check(response.stop_reason == "end_turn" and ran == [1], "the auto-resume prompt runs the pending tool")
        check("finished after resume" in texts(conn), "…and completes the turn")
        check(not acp_server._checkpoint_path(sid).exists() and not acp_server._resume_marker_path(sid).exists(), "the checkpoint and its counter are cleared")

        # A turn that dies at the same checkpoint every time is tried twice, then left alone.
        ran.clear()
        sid = await interrupted(cwd, ran)
        for attempt in (1, 2):
            acp_server._count_auto_resume(sid)
            check(acp_server._auto_resume_attempts(sid) == attempt, f"attempt {attempt} is counted against this checkpoint")
        orchestrator_model([text("finished after resume")])
        server, conn = fresh_server(ROUTE)
        await server.load_session(cwd=str(cwd), session_id=sid)
        await settle()
        notice = resume_events(conn)
        check(notice and notice[0]["auto"] is False and notice[0]["attempts"] == 2, f"after two attempts the client is told not to ({notice})")
        check("not being retried on its own" in texts(conn) and "Send /resume-turn to try once more" in texts(conn), "…and so is the person, with the way out")
        response = await server.prompt(sid, [acp.text_block(acp_server.AUTO_RESUME_MARKER)])
        await settle()
        check(response.stop_reason == "end_turn" and ran == [] and "not retrying on its own" in texts(conn), "an auto-resume prompt that arrives anyway is refused, and nothing runs")
        check(acp_server._checkpoint_path(sid).exists(), "…the checkpoint is kept for a person")
        response = await server.prompt(sid, [acp.text_block(acp_server.RESUME_TURN_MARKER)])
        await settle()
        check(response.stop_reason == "end_turn" and ran == [1], "the manual /resume-turn still works")

        # Further progress means a fresh count.
        ran.clear()
        sid = await interrupted(cwd, ran)
        acp_server._count_auto_resume(sid)
        acp_server._count_auto_resume(sid)
        check(acp_server._auto_resume_attempts(sid) == 2, "two attempts spent")
        data = json.loads(acp_server._checkpoint_path(sid).read_text())
        data["checkpoint"]["cycle_index"] = data["checkpoint"].get("cycle_index", 0) + 1  # the next run got one cycle further
        acp_server._checkpoint_path(sid).write_text(json.dumps(data))
        check(acp_server._auto_resume_attempts(sid) == 0, "a checkpoint further along starts the count again")

        # Too old; and switched off.
        old = time.time() - 3 * 3600
        os.utime(acp_server._checkpoint_path(sid), (old, old))
        server, conn = fresh_server(ROUTE)
        await server.load_session(cwd=str(cwd), session_id=sid)
        await settle()
        check(resume_events(conn)[0]["auto"] is False and "too old to resume on its own" in texts(conn), "a checkpoint over thirty minutes old is left for a person")

        os.utime(acp_server._checkpoint_path(sid), None)
        settings_store.set_value("user", "selfHeal.autoResume", False, cwd)
        server, conn = fresh_server(ROUTE)
        await server.load_session(cwd=str(cwd), session_id=sid)
        await settle()
        check(resume_events(conn)[0]["auto"] is False and "Send /resume-turn to continue it from there." in texts(conn), "selfHeal.autoResume off: the manual hint, as before")
        settings_store.set_value("user", "selfHeal.autoResume", True, cwd)
    finally:
        agent_common._STATIC_TOOLS = original
    print("\nauto-resume: all checks passed")


if __name__ == "__main__":
    asyncio.run(main())

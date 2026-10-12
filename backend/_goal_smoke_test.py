"""``/goal``, ``/compact``, ``/btw``, the context gauge and the visible checklist, with no model and no network.

* the **pure parts** of ``goal.py`` — parsing, the verdict a command gives, a judge's answer read strictly, the digest of
  what a turn did;
* the **loop through the real ACP server**: a goal checked by a command keeps the agent working until the command
  passes (and the harness, not the agent, says when); the turn budget stops it; a cancel stops it; ``/goal clear``,
  status and a mistake are answered without a model turn; every step is announced as ``_cordata/goal``;
* ``session_extras`` — the context snapshot, a compaction that really shortens the history (and refuses when there is
  nothing to compact), an aside that reads the conversation and writes nothing back;
* ``todo_write`` — validated, remembered, pushed to the client as ``_cordata/tasklist``.

    backend/.venv/bin/python _goal_smoke_test.py
"""

import asyncio
import os
import time
from types import SimpleNamespace

os.environ["CORDATA_PRISM"] = "off"

import provider_settings  # noqa: F401  (imported before agent_common, which chdirs — see main.py)

import acp
import goal
import harness_events
import session_extras
import task_tool
from _acp_harness_smoke_test import call, fresh_server, laya_answer, orchestrator_model, project, settle, text
from _ollama_cloud_smoke_test import check

ROUTE = laya_answer("code", {"code": 0.97, "answer": 0.03})


def goal_events(conn) -> list[dict]:
    return [params for method, params in conn.ext if method == "cordata/goal"]


def said(conn) -> str:
    return "".join(getattr(getattr(u, "content", None), "text", "") or "" for u in conn.updates if type(u).__name__ == "AgentMessageChunk")


def test_parsing() -> None:
    print("\n=== parsing ===")
    parsed = goal.parse("all tests pass --check make test --turns 4")
    check(isinstance(parsed, goal.Goal) and parsed.condition == "all tests pass" and parsed.check == "make test" and parsed.max_turns == 4, f"condition, check and turn budget ({parsed})")
    plain = goal.parse("the README documents every flag")
    check(plain.check is None and plain.max_turns == goal.DEFAULT_MAX_TURNS, "no check: a model will judge; default budget")
    check(goal.parse("x --turns 999").max_turns == goal.HARD_MAX_TURNS, "the budget is capped")
    check(isinstance(goal.parse("x --turns many"), str) and isinstance(goal.parse("x --turns 0"), str), "a bad budget is a message, not a goal")
    check(isinstance(goal.parse("--check make test"), str), "a check with no condition is refused")
    check(isinstance(goal.parse("do it --check"), str), "…and a --check with no command")
    check(goal.parse("") is None and goal.parse("   ") is None, "nothing after /goal means 'show the goal'")
    check(goal.is_command("/goal fix it") and goal.is_command("/goal") and not goal.is_command("/goals") and not goal.is_command("my /goal"), "only /goal itself is a command")
    kick = goal.kickoff(goal.parse("tests pass --check make test"))
    check("make test" in kick and "exits 0" in kick, "the first turn is told what decides the goal")


def test_the_check_and_the_judge(cwd) -> None:
    print("\n=== the verdict ===")
    ok = goal.run_check("true", str(cwd))
    bad = goal.run_check("echo 2 tests failed; exit 3", str(cwd))
    check(ok.met and not bad.met and "2 tests failed" in bad.reason and "exited 3" in bad.reason, f"a command decides, and its output explains ({bad.reason!r})")
    slow = goal.run_check("sleep 5", str(cwd), timeout=0.2)
    check(not slow.met and "did not finish" in slow.reason, "a check that hangs is not met")

    yes = goal.parse_judgement('Sure.\n{"met": true, "reason": "pytest passed"}')
    check(yes.met and yes.reason == "pytest passed" and not yes.failed, "a judge's JSON is read")
    for garbage in ["the goal looks done to me", '{"met": "yes"}', "", '{"met": true']:
        verdict = goal.parse_judgement(garbage)
        check(not verdict.met and verdict.failed, f"{garbage!r} is a failure to judge, never a quiet 'met'")

    messages = [
        {"role": "user", "content": [{"text": "old question"}]},
        {"role": "assistant", "content": [{"text": "old answer"}]},
        {"role": "user", "content": [{"text": "make the tests pass"}]},
        {"role": "assistant", "content": [{"text": "running them"}, {"toolUse": {"name": "shell", "input": {"command": "pytest"}}}]},
        {"role": "user", "content": [{"toolResult": {"status": "success", "content": [{"text": "3 passed"}]}}]},
        {"role": "assistant", "content": [{"text": "done"}]},
    ]
    digest = goal.turn_digest(messages)
    check("old answer" not in digest and "pytest" in digest and "3 passed" in digest and "agent: done" in digest, "the digest is the last turn: words, commands, what they printed")
    asked: list[str] = []
    verdict = goal.evaluate(goal.parse("tests pass"), cwd=str(cwd), messages=messages, ask=lambda p: asked.append(p) or '{"met": true, "reason": "3 passed"}')
    check(verdict.met and verdict.how == "judge" and "3 passed" in asked[0] and "not evidence" in asked[0], "with no command, a tool-less judge reads the turn and is told claims are not evidence")
    broken = goal.evaluate(goal.parse("tests pass"), cwd=str(cwd), messages=messages, ask=lambda p: (_ for _ in ()).throw(RuntimeError("model down")))
    check(not broken.met and broken.failed, "a judge that is down ends the loop's patience, not the session")
    check(goal.evaluate(goal.parse("tests pass"), cwd=str(cwd), messages=messages, ask=None).failed, "no model at all is a failure to judge")


async def test_a_goal_keeps_going_until_the_command_passes() -> None:
    print("\n=== the loop ===")
    cwd = project("goal-loop")
    orchestrator_model([
        text("I looked around; nothing to do."),                                            # turn 1: claims done, wrongly
        call("shell", {"command": "echo built > out.txt"}), text("created out.txt"),         # turn 2: does the work
    ])
    server, conn = fresh_server(ROUTE)
    created = await server.new_session(cwd=str(cwd))
    sid = created.session_id
    await server.set_session_mode(sid, "auto")
    response = await server.prompt(sid, [acp.text_block("/goal out.txt exists --check test -f out.txt")])
    await settle()
    events = goal_events(conn)
    kinds = [e["event"] for e in events]
    check(response.stop_reason == "end_turn", "the prompt ends normally")
    check((cwd / "out.txt").exists(), "the second turn did the work the first one skipped")
    check(kinds == ["set", "checking", "continue", "checking", "met"], f"every step is announced, in order ({kinds})")
    cont = next(e for e in events if e["event"] == "continue")
    check(cont["turn"] == 1 and cont["max"] == goal.DEFAULT_MAX_TURNS and cont["how"] == "check", f"the follow-up says where it is ({cont['turn']}/{cont['max']})")
    check(events[-1]["state"] == "met" and "exited 0" in events[-1]["reason"], f"met by the command, not by the agent's say-so ({events[-1]['reason']})")
    check(server._goals[sid].state == "met", "the goal is remembered as met")
    turns = [p for m, p in conn.ext if m == "cordata/turn"]
    check(len(turns) == 2, f"each turn has its own receipt ({len(turns)})")
    contexts = [p for m, p in conn.ext if m == "cordata/context"]
    check(contexts and "used" in contexts[-1] and "window" in contexts[-1], "…and the context gauge is pushed after a turn")


async def test_the_budget_a_cancel_and_the_commands() -> None:
    print("\n=== limits ===")
    cwd = project("goal-budget")
    orchestrator_model([text(f"try {i}") for i in range(10)])
    server, conn = fresh_server(ROUTE)
    created = await server.new_session(cwd=str(cwd))
    sid = created.session_id
    await server.prompt(sid, [acp.text_block("/goal never happens --check false --turns 2")])
    await settle()
    kinds = [e["event"] for e in goal_events(conn)]
    check(kinds.count("continue") == 2 and kinds[-1] == "exhausted", f"the turn budget ends it ({kinds})")
    check(server._goals[sid].state == "exhausted", "exhausted, and it says so")

    before = len(goal_events(conn))
    reply_start = len(conn.updates)
    response = await server.prompt(sid, [acp.text_block("/goal")])
    shown = "".join(getattr(getattr(u, "content", None), "text", "") or "" for u in conn.updates[reply_start:])
    check(response.stop_reason == "end_turn" and "never happens" in shown and "exhausted" in shown, f"/goal shows the goal ({shown[:80]!r})")
    check(len(goal_events(conn)) == before and len([p for m, p in conn.ext if m == "cordata/turn"]) == 3, "…without a model turn")

    reply_start = len(conn.updates)
    await server.prompt(sid, [acp.text_block("/goal clear")])
    shown = "".join(getattr(getattr(u, "content", None), "text", "") or "" for u in conn.updates[reply_start:])
    check("cleared" in shown.lower() and sid not in server._goals, "/goal clear removes it")
    reply_start = len(conn.updates)
    await server.prompt(sid, [acp.text_block("/goal --turns 3")])
    shown = "".join(getattr(getattr(u, "content", None), "text", "") or "" for u in conn.updates[reply_start:])
    check("needs a condition" in shown, f"a mistake is explained, not run ({shown[:60]!r})")

    # A cancel mid-goal stops the pursuit.
    cwd2 = project("goal-cancel")
    orchestrator_model([call("shell", {"command": "sleep 2"}), text("never")])
    server2, conn2 = fresh_server(ROUTE)
    created2 = await server2.new_session(cwd=str(cwd2))
    await server2.set_session_mode(created2.session_id, "auto")
    turn = asyncio.create_task(server2.prompt(created2.session_id, [acp.text_block("/goal finish --check false")]))
    await asyncio.sleep(0.4)
    await server2.cancel(created2.session_id)
    response = await turn
    await settle()
    states = [e["event"] for e in goal_events(conn2)]
    check(response.stop_reason == "cancelled" and states[-1] == "stopped" and "continue" not in states, f"cancel stops the pursuit ({states})")


async def test_the_judge_is_used_when_there_is_no_command() -> None:
    print("\n=== a judged goal ===")
    cwd = project("goal-judge")
    orchestrator_model([text("first attempt"), text("second attempt, with proof")])
    server, conn = fresh_server(ROUTE)
    answers = iter(['{"met": false, "reason": "no proof yet"}', '{"met": true, "reason": "proof shown"}'])
    real = session_extras.one_shot
    session_extras.one_shot = lambda system, prompt: next(answers)
    try:
        created = await server.new_session(cwd=str(cwd))
        await server.prompt(created.session_id, [acp.text_block("/goal show me proof")])
    finally:
        session_extras.one_shot = real
    await settle()
    events = goal_events(conn)
    check([e["event"] for e in events] == ["set", "checking", "continue", "checking", "met"] and events[-1]["how"] == "judge", f"a judge decides when there is no command ({[e['event'] for e in events]})")
    check(events[2]["reason"] == "no proof yet", "the follow-up carries the judge's reason")

    cwd2 = project("goal-judge-down")
    orchestrator_model([text("a"), text("b"), text("c"), text("d")])
    server2, conn2 = fresh_server(ROUTE)
    session_extras.one_shot = lambda system, prompt: (_ for _ in ()).throw(RuntimeError("no model"))
    try:
        created2 = await server2.new_session(cwd=str(cwd2))
        await server2.prompt(created2.session_id, [acp.text_block("/goal anything")])
    finally:
        session_extras.one_shot = real
    await settle()
    kinds = [e["event"] for e in goal_events(conn2)]
    check(kinds[-1] == "stopped" and kinds.count("continue") <= goal.MAX_JUDGE_FAILURES, f"a judge that cannot judge ends the loop instead of spending on guesses ({kinds})")


class FakeModel:
    def __init__(self, window=1000):
        self.window = window

    def get_config(self):
        return {"context_window_limit": self.window}


class FakeManager:
    summary_ratio = 0.3
    preserve_recent_messages = 10
    summarization_agent = None
    summarization_system_prompt = None

    def __init__(self, fail=False):
        self.fail = fail
        self.saw = None

    def reduce_context(self, agent, e=None, **kwargs):
        self.saw = (self.summary_ratio, self.preserve_recent_messages, self.summarization_system_prompt, e)
        if self.fail:
            raise RuntimeError("summary model down")
        keep = agent.messages[-self.preserve_recent_messages :]
        agent.messages[:] = [{"role": "user", "content": [{"text": "SUMMARY"}]}] + keep


def fake_agent(count=14, fail=False):
    messages = []
    for i in range(count):
        messages.append({"role": "user" if i % 2 == 0 else "assistant", "content": [{"text": f"message {i} " + "x" * 200}]})
    messages[-1]["metadata"] = {"usage": {"inputTokens": 400, "outputTokens": 50, "totalTokens": 450}}
    synced = []
    agent = SimpleNamespace(messages=messages, model=FakeModel(), conversation_manager=FakeManager(fail), _session_manager=SimpleNamespace(sync_agent=lambda a: synced.append(len(a.messages))))
    agent.synced = synced
    return agent


def test_context_compact_and_aside() -> None:
    print("\n=== context, compact, aside ===")
    agent = fake_agent()
    snap = session_extras.context_snapshot(agent)
    check(snap["used"] == 450 and snap["window"] == 1000 and snap["pct"] == 0.45 and not snap["estimated"], f"the gauge reads the provider's own usage ({snap})")
    fresh = SimpleNamespace(messages=[{"role": "user", "content": [{"text": "x" * 350}]}], model=FakeModel(), conversation_manager=None)
    check(session_extras.context_snapshot(fresh)["estimated"] and session_extras.context_snapshot(fresh)["used"] == 100, "with no usage reported yet it estimates from the text")
    nowin = SimpleNamespace(messages=[], model=SimpleNamespace(get_config=lambda: {}))
    check(session_extras.context_snapshot(nowin)["pct"] is None, "an unknown window gives no percentage, not a wrong one")

    result = session_extras.compact(agent, "keep the migration plan")
    manager = agent.conversation_manager
    check(result["ok"] and result["after"]["messages"] < result["before"]["messages"], f"compaction shortens the history ({result['before']['messages']} → {result['after']['messages']})")
    check(manager.saw[0] == 0.8 and manager.saw[1] == 4 and "migration plan" in manager.saw[2] and isinstance(manager.saw[3], RuntimeError), "it summarises most of it, keeps the last turns, honours the focus, and a failure would raise")
    check(manager.summary_ratio == 0.3 and manager.preserve_recent_messages == 10 and manager.summarization_system_prompt is None, "the automatic compaction's settings are put back")
    check(agent.synced and agent.messages[0]["content"][0]["text"] == "SUMMARY", "the session store is re-synced, so a resume loads the compacted history")
    check(result["after"]["estimated"] and result["after"]["used"] < result["before"]["used"], "the 'after' is an honest estimate, not the old usage")

    failing = fake_agent(fail=True)
    bad = session_extras.compact(failing)
    check(not bad["ok"] and "summary model down" in bad["reason"] and len(failing.messages) == 14, "a failed summary is reported and the history is untouched")
    tiny = fake_agent(count=3)
    check(not session_extras.compact(tiny)["ok"] and "nothing to compact" in session_extras.compact(tiny)["reason"], "nothing to compact is said, not faked")

    seen = {}
    before = [dict(m) for m in agent.messages]
    answer = session_extras.ask_aside("what did we decide?", agent.messages, lambda system, prompt: seen.update(system=system, prompt=prompt) or "  We decided X.  ")
    check(answer == "We decided X." and "SUMMARY" in seen["prompt"] and "no tools" in seen["system"], "the aside is answered from the conversation by a model with no tools")
    check(agent.messages == before, "…and nothing is written back to the history")


def test_the_checklist() -> None:
    print("\n=== todo_write ===")
    bus: list[dict] = []
    unsubscribe = harness_events.subscribe(lambda kind, payload: bus.append(payload) if kind == "tasklist" else None)
    task_tool.configure(lambda: "s-todo")
    try:
        reply = task_tool.todo_write.__wrapped__([{"content": "read the code", "status": "completed"}, {"content": "write the fix", "status": "in_progress"}, "run the tests"]) if hasattr(task_tool.todo_write, "__wrapped__") else task_tool.todo_write._tool_func([{"content": "read the code", "status": "completed"}, {"content": "write the fix", "status": "in_progress"}, "run the tests"])
    finally:
        unsubscribe()
    check("1/3 done" in reply, f"saved ({reply})")
    check(bus and bus[-1]["sessionId"] == "s-todo" and bus[-1]["done"] == 1 and bus[-1]["total"] == 3 and bus[-1]["items"][2] == {"content": "run the tests", "status": "pending"}, "pushed to the client; a bare string is a pending step")
    check([i["status"] for i in task_tool.current("s-todo")] == ["completed", "in_progress", "pending"], "remembered for the session")
    for bad, why in [
        ([], "non-empty"),
        ([{"content": "a", "status": "in_progress"}, {"content": "b", "status": "in_progress"}], "one item"),
        ([{"content": "a", "status": "finished-ish"}], "status must be one of"),
        ([{"content": "", "status": "pending"}], "no content"),
        ([{"content": str(i), "status": "pending"} for i in range(task_tool.MAX_ITEMS + 1)], "at most"),
    ]:
        cleaned, problem = task_tool.validate(bad)
        check(not cleaned and why in (problem or ""), f"refused: {why}")
    cleaned, _ = task_tool.validate([{"content": "x", "status": "Done"}, {"content": "y", "status": "in-progress"}])
    check([c["status"] for c in cleaned] == ["completed", "in_progress"], "common spellings are accepted")


async def main() -> None:
    import tempfile
    from pathlib import Path

    test_parsing()
    test_the_check_and_the_judge(Path(tempfile.mkdtemp()))
    await test_a_goal_keeps_going_until_the_command_passes()
    await test_the_budget_a_cancel_and_the_commands()
    await test_the_judge_is_used_when_there_is_no_command()
    test_context_compact_and_aside()
    test_the_checklist()
    print("\nall goal / compact / aside / checklist checks passed")


if __name__ == "__main__":
    asyncio.run(main())

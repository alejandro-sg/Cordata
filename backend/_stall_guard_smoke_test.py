"""Stall detection: an agent going in circles is told, then stopped.

* **unit** — the third identical result gets a note (once); the fifth identical call is refused, as a nudge
  and not a lock; timings in the output do not hide a repeat; a different result or a different input starts
  over; two calls alternating are noticed; a file edited back to an earlier version is noticed; the third
  refusal in a turn says "stop and report"; agents and turns do not leak into each other;
* **through the ACP server** — a scripted agent that reads the same file six times: the third result carries
  the note, the sixth call is not run, the turn's outcome record carries the stall and scores lower for it, and
  the client was told. A turn that makes progress (different results) is never touched.

    backend/.venv/bin/python _stall_guard_smoke_test.py
"""

import asyncio
import os
import tempfile

os.environ["CORDATA_PRISM"] = "off"

import provider_settings  # noqa: F401  (imported before agent_common, which chdirs — see main.py)

import acp
import harness_events
import outcomes
import stall_guard
from _acp_harness_smoke_test import call, fresh_server, laya_answer, orchestrator_model, project, settle, text
from _ollama_cloud_smoke_test import check

ROUTE = laya_answer("code", {"code": 0.97, "answer": 0.03})


def new_turn(session: str) -> None:
    outcomes.finish(session, "end_turn", persist=False)
    outcomes.begin(session, goal="g", mode="auto", project=tempfile.gettempdir())


def test_the_guard() -> None:
    session = "stall-unit"
    outcomes.begin(session, goal="g", mode="auto", project=tempfile.gettempdir())
    guard = stall_guard.StallGuard(session)
    seen: list[dict] = []
    unsubscribe = harness_events.subscribe(lambda kind, payload: seen.append(payload) if payload.get("phase") == "stalled" else None)
    try:
        run = {"command": "pytest -q"}
        notes = []
        for i in range(4):
            check(guard.before("orchestrator", "shell", run) is None, f"call {i + 1} is allowed")
            notes.append(guard.after("orchestrator", "shell", run, "1 failed in 0.%02ds\nE  assert 1 == 2" % (10 + i)))
        check(notes[0] is None and notes[1] is None and notes[2] and "[loop]" in notes[2] and "3 times" in notes[2], "the third identical result gets a note — a changing timing does not hide the repeat")
        check(notes[3] is None, "…once, not on every call after")
        guard.before("orchestrator", "shell", run)
        guard.after("orchestrator", "shell", run, "1 failed in 0.50s\nE  assert 1 == 2")
        refusal = guard.before("orchestrator", "shell", run)
        check(refusal and refusal.startswith("shell was not run:") and "5 times" in refusal, f"the sixth call is refused ({refusal and refusal[:70]})")
        check(guard.before("orchestrator", "shell", run) is None, "…as a nudge, not a lock: the count starts over, so a deliberate retry may follow")

        check(guard.before("orchestrator", "shell", {"command": "pytest -q tests/other"}) is None, "a different input is a different call")
        for result in ("1 failed", "1 failed in a different way", "all passed"):
            guard.after("orchestrator", "shell", {"command": "cargo test"}, result)
        check(guard.before("orchestrator", "shell", {"command": "cargo test"}) is None and guard.after("orchestrator", "shell", {"command": "cargo test"}, "all passed") is None, "a changing result is progress, however often the command runs")
        check(guard.after("builder", "shell", run, "1 failed in 9s\nE  assert 1 == 2") is None, "another agent's calls count separately")

        # Alternating A, B, A, B …
        a, b = {"command": "make"}, {"command": "make test"}
        got = None
        for _ in range(4):
            got = guard.after("orchestrator", "shell", a, "make: error 2") or got
            got = guard.after("orchestrator", "shell", b, "make: test failed") or got
        check(got and "alternated between the same two steps" in got, "two calls alternating with no change are noticed")

        # Edits going in circles.
        check(guard.edited("orchestrator", "calc.py", "h0", "h1") is None, "an edit to something new is fine")
        check(guard.edited("orchestrator", "calc.py", "h1", "h1") is None, "…and a no-op edit is not a cycle")
        check(guard.edited("orchestrator", "calc.py", "h1", "h2") is None, "…nor is a third version")
        cyc = guard.edited("orchestrator", "calc.py", "h2", "h1")
        check(cyc and "going in circles" in cyc and "calc.py" in cyc, "…but returning to h1 is")

        # Escalation: the third refusal says stop.
        s2 = stall_guard.StallGuard("stall-unit-2")
        outcomes.begin("stall-unit-2", goal="g", mode="auto", project=tempfile.gettempdir())
        messages = []
        for _ in range(3):
            for _ in range(5):
                s2.after("orchestrator", "file_read", {"path": "x"}, "same")
            messages.append(s2.before("orchestrator", "file_read", {"path": "x"}))
        check(all(m and m.startswith("file_read was not run") for m in messages), "each time it is refused")
        check("Stop. Tell the user" in messages[2] and "Stop. Tell the user" not in messages[0], "the third refusal in a turn says stop and report")

        # A new turn forgives.
        for _ in range(5):
            guard.after("orchestrator", "shell", {"command": "again"}, "same")
        check(guard.before("orchestrator", "shell", {"command": "again"}) is not None, "five repeats are refused…")
        new_turn(session)
        check(guard.before("orchestrator", "shell", {"command": "again"}) is None, "…and a new turn starts clean")
    finally:
        unsubscribe()
        outcomes.finish(session, "end_turn", persist=False)
        outcomes.finish("stall-unit-2", "end_turn", persist=False)
    check(seen and all(e["event"] == "heal" for e in seen), f"the client hears about each ({len(seen)} events)")


def records() -> list[dict]:
    import state_store

    return [row["payload"] for row in reversed(state_store.recent_events(kind=outcomes.KIND, limit=50))]


async def test_through_the_server() -> None:
    cwd = project("stalled")
    (cwd / "notes.txt").write_text("the same notes every time\n")
    read = {"command": "cat notes.txt"}
    orchestrator_model([call("shell", read) for _ in range(6)] + [text("I keep reading it")])
    server, _ = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(cwd))).session_id
    await server.set_session_mode(sid, "auto")
    seen: list[dict] = []
    unsubscribe = harness_events.subscribe(lambda kind, payload: seen.append(payload) if payload.get("phase") == "stalled" else None)
    before = len(records())
    try:
        response = await server.prompt(sid, [acp.text_block("read the notes")])
        await settle()
    finally:
        unsubscribe()
    check(response.stop_reason == "end_turn", "the turn ends normally")
    record = records()[before]
    check(record["tools"]["shell"]["count"] == 6 and record["tools"]["shell"].get("denied") == 1, f"the sixth call was refused, not run ({record['tools']['shell']})")
    check(record["stalls"].get("notes") == 1 and record["stalls"].get("blocks") == 1, f"the record carries the stall ({record['stalls']})")
    reward = outcomes.score(record)
    check(reward.parts.get("stalled") == -0.15, f"…and the turn scores lower for it ({reward.parts})")
    check(len(seen) >= 2 and any("same input" in e["root_cause"] for e in seen), "the client was told")

    # Progress is never touched: the same command, a result that changes each time.
    cwd = project("progress")
    steps = []
    for i in range(7):
        steps.append(call("shell", {"command": f"echo step >> log.txt; wc -l log.txt"}))
    orchestrator_model(steps + [text("done")])
    server, _ = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(cwd))).session_id
    await server.set_session_mode(sid, "auto")
    before = len(records())
    await server.prompt(sid, [acp.text_block("append seven lines")])
    await settle()
    record = records()[before]
    check(not record.get("stalls") and record["tools"]["shell"].get("denied", 0) == 0, f"a command whose result changes every time is progress ({record.get('stalls')})")


async def main() -> None:
    test_the_guard()
    await test_through_the_server()
    print("\nstall guard: all checks passed")


if __name__ == "__main__":
    asyncio.run(main())

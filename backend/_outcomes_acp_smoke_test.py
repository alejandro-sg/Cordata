"""Outcome records through the real ACP server: a turn in, one record out.

A scripted orchestrator (no model, no network) works in a temp project, and the test reads back what
the ledger now holds:

* an **auto-mode turn** that creates a file and runs a project check: the record has the goal (masked),
  the mode, the route Laya chose, the tool counts, the guarded calls auto mode let through, the file it
  changed with hashes, and a passing verifier *after* the change — so it scores well;
* a **build-mode turn whose approval the person rejects**: the rejection is counted and costs reward;
* a **failing check** after an edit scores badly;
* a **cancelled turn** is recorded as cancelled;
* **recording is a side channel**: the turn's reply is unchanged, and the next turn reconciles the last.

    backend/.venv/bin/python _outcomes_acp_smoke_test.py
"""

import asyncio
import os
import time

os.environ["CORDATA_PRISM"] = "off"

import provider_settings  # noqa: F401  (imported before agent_common, which chdirs — see main.py)

import acp
import outcomes
import state_store
from _acp_harness_smoke_test import MODELS, call, fresh_server, laya_answer, orchestrator_model, project, settle, text
from _lab_acp_smoke_test import HitlConn, until
from _ollama_cloud_smoke_test import check

def records() -> list[dict]:
    return [row["payload"] for row in reversed(state_store.recent_events(kind=outcomes.KIND, limit=50))]


async def test_an_auto_turn_that_edits_and_verifies() -> None:
    cwd = project("auto-turn")
    (cwd / "run_tests.sh").write_text("#!/bin/sh\nexit 0\n")
    orchestrator_model([call("shell", {"command": "echo built > out.txt"}), call("shell", {"command": "bash run_tests.sh"}), text("done: out.txt written, checks pass")])
    server, conn = fresh_server(laya_answer("code", {"code": 0.97, "answer": 0.02, "debugger": 0.01}))
    created = await server.new_session(cwd=str(cwd))
    sid = created.session_id
    await server.set_session_mode(sid, "auto")

    before = len(records())
    response = await server.prompt(sid, [acp.text_block("write out.txt and run the checks. my key is sk-ant-api03-" + "Zz9y" * 10)])
    await settle()
    check(response.stop_reason == "end_turn", "the turn is unchanged by being recorded")
    new = records()[before:]
    check(len(new) == 1, f"one turn, one record ({len(new)})")
    record = new[0]
    check(record["mode"] == "auto" and record["stop_reason"] == "end_turn" and record["project"] == str(cwd), "mode, stop reason and project")
    check("sk-ant-api03-Zz9y" not in record["goal"] and record["goal"].startswith("write out.txt"), "the goal is the prompt, with the key masked")
    check(record["route"] and record["route"]["target"] == "code" and record["route"]["abstained"] is False, f"Laya's route rides on the record ({record['route']})")
    check(record["tools"]["shell"]["count"] == 2 and record["tools"]["shell"]["failed"] == 0, f"two shell calls, none failed ({record['tools']})")
    check(record["approvals"].get("auto", 0) == 2, f"auto mode let two guarded calls through, and said so ({record['approvals']})")
    check({f["path"] for f in record["files"]} == {"out.txt"}, f"the file the turn created ({[f['path'] for f in record['files']]})")
    check(record["verifier"]["after_last_edit"] == "pass" and record["verifier"]["ran"] == 1, f"the check ran after the change and passed ({record['verifier']})")
    check("orchestrator" in record["agents"] and record["seconds"] >= 0, "which agent worked")
    reward = outcomes.score(record)
    check(reward.value > 0.3 and "verified" in reward.parts, f"it scores well ({reward.value}: {reward.parts})")


async def test_a_rejected_approval_costs_reward() -> None:
    cwd = project("rejected-turn")
    orchestrator_model([call("shell", {"command": "echo needs-a-human"}), text("ok, I will not")])
    server, _plain = fresh_server(laya_answer("code", {"code": 0.97, "answer": 0.03}))
    conn = HitlConn()
    server.on_connect(conn)
    created = await server.new_session(cwd=str(cwd))
    before = len(records())
    turn = asyncio.create_task(server.prompt(created.session_id, [acp.text_block("run it")]))
    await until(lambda: conn.waiting, "build mode asks")
    conn.decide("reject")
    await turn
    await settle()
    record = records()[before]
    check(record["approvals"].get("asked", 0) == 1 and record["approvals"].get("rejected", 0) == 1, f"asked once, rejected once ({record['approvals']})")
    check(record["mode"] == "build", "build mode")
    check(record["tools"]["shell"]["denied"] == 1 and record["tools"]["shell"]["failed"] == 0, f"a refused call is denied, not failed ({record['tools']})")
    check(record["verifier"]["unresolved"] == [], "…and not a command left failing")
    reward = outcomes.score(record)
    check(reward.parts.get("rejected") == -0.1 and "broken" not in reward.parts, f"the rejection costs reward once, and the turn is not 'broken' ({reward.parts})")


async def test_a_failing_check_scores_badly() -> None:
    cwd = project("failing-turn")
    (cwd / "run_tests.sh").write_text("#!/bin/sh\necho FAILED\nexit 1\n")
    orchestrator_model([call("shell", {"command": "echo changed > a.txt"}), call("shell", {"command": "bash run_tests.sh"}), text("the checks fail")])
    server, _conn = fresh_server(laya_answer("code", {"code": 0.97, "answer": 0.03}))
    created = await server.new_session(cwd=str(cwd))
    await server.set_session_mode(created.session_id, "auto")
    before = len(records())
    await server.prompt(created.session_id, [acp.text_block("make the change")])
    await settle()
    record = records()[before]
    check(record["verifier"]["after_last_edit"] == "fail" and record["tools"]["shell"]["failed"] == 1, f"the failing check is on the record ({record['verifier']})")
    check(outcomes.score(record).value < -0.3, f"…and the turn scores badly ({outcomes.score(record).value})")


async def test_a_cancelled_turn_is_recorded_as_cancelled() -> None:
    cwd = project("cancelled-turn")
    orchestrator_model([call("shell", {"command": "sleep 1"}), text("never reached")])
    server, _conn = fresh_server(laya_answer("code", {"code": 0.97, "answer": 0.03}))
    created = await server.new_session(cwd=str(cwd))
    await server.set_session_mode(created.session_id, "auto")
    before = len(records())
    turn = asyncio.create_task(server.prompt(created.session_id, [acp.text_block("do a slow thing")]))
    await asyncio.sleep(0.3)
    await server.cancel(created.session_id)  # the person stops it while the command runs
    response = await turn
    await settle()
    record = records()[before]
    check(response.stop_reason == "cancelled" and record["stop_reason"] == "cancelled", f"the stop reason is carried through ({record['stop_reason']})")
    check("cancelled" in outcomes.score(record).parts, "…and marks the turn down")


async def test_the_next_turn_reconciles_the_last() -> None:
    cwd = project("reconcile-turn")
    orchestrator_model([call("shell", {"command": "echo first > kept.txt"}), text("one"), call("shell", {"command": "echo second > other.txt"}), text("two")])
    server, _conn = fresh_server(laya_answer("code", {"code": 0.97, "answer": 0.03}))
    created = await server.new_session(cwd=str(cwd))
    await server.set_session_mode(created.session_id, "auto")
    await server.prompt(created.session_id, [acp.text_block("first")])
    await settle()
    first = [r for r in records() if r["project"] == str(cwd)][-1]
    # Judged by an injectable clock: two minutes is too soon, an hour on is time enough.
    check(outcomes.reconcile(str(cwd), now=time.time() + 5) == [], "right after the turn, nothing is judged")
    updates = outcomes.reconcile(str(cwd), now=time.time() + 3600)
    check(len(updates) == 1 and updates[0]["id"] == first["id"] and updates[0]["kept"] == 1, f"later, the kept file is judged ({updates})")
    await server.prompt(created.session_id, [acp.text_block("second")])
    await settle()
    pairs = outcomes.recent(10, project=str(cwd))
    check(len(pairs) == 2 and pairs[1][1] is not None and pairs[0][1] is None, "the history pairs each turn with its judgement, newest first")


async def main() -> None:
    await test_an_auto_turn_that_edits_and_verifies()
    await test_a_rejected_approval_costs_reward()
    await test_a_failing_check_scores_badly()
    await test_a_cancelled_turn_is_recorded_as_cancelled()
    await test_the_next_turn_reconciles_the_last()
    print("\noutcomes (ACP): all checks passed")


if __name__ == "__main__":
    asyncio.run(main())

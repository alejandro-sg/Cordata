"""Conversations on disk: snapshot sessions, and the move to them from the legacy layout (`session_store.py`) — offline.

1. **a conversation round-trips** — messages, agent state and the conversation manager's state (a summary) come back; the
   files are private (0700 / 0600); the **system prompt is not restored** — a session always runs on the prompt built for it
   now (project instructions, roster, settings), never last week's.
2. **migration is exact and additive** — a legacy `session_<id>/` directory written by the SDK's `FileSessionManager` is opened
   with the snapshot manager on first use, restores identically, and the legacy directory is byte-for-byte untouched (a
   rollback loses nothing). Running it again does nothing.
3. **a broken legacy session is not lost** — it keeps the legacy manager and still loads.
4. **`migrate_all`** says what it did, and the CLI's `status` counts what is left.
5. **a message the harness appends by hand is persisted** — the settled results of a cancelled call survive a restart
   (the snapshot manager's `append_message` is a no-op, so the harness captures the agent instead).

6. **"go back"** — each turn is checkpointed as it starts; going back restores the conversation as it was (messages, agent
   state, the turn count), forgets exactly those turns, survives a restart, can itself be undone, and says so plainly when
   there is nothing to go back to. Through the real ACP server: `_sierralab/conversation/list|rewind`, refused mid-turn.

    backend/.venv/bin/python _session_store_smoke_test.py
"""

import hashlib
import os
import subprocess
import sys
import tempfile
from pathlib import Path

from strands import Agent
from strands.agent.conversation_manager import SummarizingConversationManager
from strands.session.file_session_manager import FileSessionManager

import session_store as st
from harness_testing import ScriptedModel, check, text

BASE = Path(tempfile.mkdtemp()).resolve()


def agent(session_id: str, manager, *, prompt: str = "prompt") -> Agent:
    return Agent(model=ScriptedModel([text("ok")] * 8), agent_id="default", session_manager=manager, conversation_manager=SummarizingConversationManager(), system_prompt=prompt, callback_handler=None)


def tree_hash(root: Path) -> str:
    digest = hashlib.sha256()
    for path in sorted(p for p in root.rglob("*") if p.is_file()):
        digest.update(str(path.relative_to(root)).encode())
        digest.update(path.read_bytes())
    return digest.hexdigest()


def test_a_conversation_round_trips() -> None:
    first = agent("s1", st.manager_for("s1", BASE), prompt="OLD PROMPT")
    first.state.set("note", {"remembered": True})
    first("hello")
    first("and again")
    first.conversation_manager._summary_message = {"role": "user", "content": [{"text": "SUMMARY of earlier work"}]}
    first.conversation_manager.removed_message_count = 3
    st.session_manager("s1", BASE).sync_agent(first)

    again = agent("s1", st.session_manager("s1", BASE), prompt="NEW PROMPT, built for this session")
    check([m["role"] for m in again.messages] == [m["role"] for m in first.messages] and len(again.messages) == 4, "messages come back")
    check(again.state.get("note") == {"remembered": True}, "…and agent state")
    check(again.conversation_manager.removed_message_count == 3 and again.conversation_manager._summary_message is not None, "…and the conversation manager's state, summary included")
    check(again.system_prompt == "NEW PROMPT, built for this session", "…but the system prompt is the fresh one, not the one saved last time")

    snapshot = st.snapshot_path("s1", BASE)
    check(oct(snapshot.stat().st_mode & 0o777) == "0o600" and oct(BASE.stat().st_mode & 0o777) == "0o700", f"the file is 0600 ({oct(snapshot.stat().st_mode & 0o777)}) under a 0700 sessions directory ({oct(BASE.stat().st_mode & 0o777)})")
    check(st.exists("s1", BASE) and st.has_snapshot("s1", BASE) and not st.has_legacy("s1", BASE), "a new session is a snapshot, with no legacy directory")


def legacy_session(session_id: str, *, summary: bool = False) -> None:
    old = agent(session_id, FileSessionManager(session_id=session_id, storage_dir=str(BASE)))
    old.state.set("legacy_note", [1, 2, 3])
    old("a legacy question")
    old("a second legacy question")
    if summary:
        old.conversation_manager._summary_message = {"role": "user", "content": [{"text": "LEGACY SUMMARY"}]}
        old.conversation_manager.removed_message_count = 2
        FileSessionManager(session_id=session_id, storage_dir=str(BASE)).sync_agent(old)


def test_migration_is_exact_and_additive() -> None:
    legacy_session("old1", summary=True)
    before = tree_hash(st.legacy_dir("old1", BASE))
    check(st.has_legacy("old1", BASE) and not st.has_snapshot("old1", BASE), "a legacy session has no snapshot yet")

    manager = st.session_manager("old1", BASE)
    check(isinstance(manager, st.HarnessSessionManager) and st.has_snapshot("old1", BASE), "the first use migrates it")
    check(tree_hash(st.legacy_dir("old1", BASE)) == before, "…and leaves the legacy directory byte-for-byte untouched")

    legacy = agent("old1", FileSessionManager(session_id="old1", storage_dir=str(BASE)), prompt="FRESH")
    moved = agent("old1", manager, prompt="FRESH")
    # The legacy manager applies the conversation manager's state on restore: the first `removed_message_count` messages go and
    # the summary takes their place (4 written, 2 removed, 1 summary → 3). The snapshot holds the conversation as it was after that.
    check(moved.messages == legacy.messages and len(moved.messages) == 3 and "LEGACY SUMMARY" in str(moved.messages[0]), f"the conversation is identical, summary applied once ({len(moved.messages)} messages)")
    check(moved.state.get() == legacy.state.get() and moved.state.get("legacy_note") == [1, 2, 3], "…and the agent state")
    check(moved.conversation_manager.get_state() == legacy.conversation_manager.get_state() and moved.conversation_manager._summary_message is not None, "…and the conversation manager's state, summary included")
    check(moved.system_prompt == "FRESH", "…on the fresh system prompt")

    stamp = st.snapshot_path("old1", BASE).stat().st_mtime_ns
    check(st.migrate("old1", BASE) is False and st.snapshot_path("old1", BASE).stat().st_mtime_ns == stamp, "migrating again does nothing")

    # the session carries on from where it was, in the new layout, and the legacy copy is not touched again
    moved("a third question")
    again = agent("old1", st.session_manager("old1", BASE))
    check(len(again.messages) == 5 and tree_hash(st.legacy_dir("old1", BASE)) == before, "it carries on in the snapshot (5 messages); the legacy copy stays as it was")


def test_a_broken_legacy_session_is_not_lost() -> None:
    legacy_session("broken1")
    victim = next((st.legacy_dir("broken1", BASE) / "agents" / "agent_default" / "messages").glob("message_1.json"))
    victim.write_text("{ not json at all")
    manager = st.session_manager("broken1", BASE)
    check(isinstance(manager, FileSessionManager) and not st.has_snapshot("broken1", BASE), "a corrupt legacy session keeps the legacy manager and writes no snapshot")
    check(st.exists("broken1", BASE), "…and still counts as existing (so session/list shows it)")


def test_migrate_all_and_the_cli() -> None:
    for name in ("bulk1", "bulk2"):
        legacy_session(name)
    status = subprocess.run([sys.executable, "session_store.py", "status", "--base", str(BASE)], capture_output=True, text=True, cwd=Path(__file__).parent)
    check("still to migrate" in status.stdout and "bulk1" not in status.stdout, f"status counts what is left ({status.stdout.strip()[:90]})")
    report = st.migrate_all(BASE)
    check(set(report.migrated) == {"bulk1", "bulk2"} and "old1" in report.skipped and set(report.failed) == {"broken1"}, f"migrate_all says what it did ({report.summary()})")
    check("broken1" in report.failed and "JSON" in report.failed["broken1"].upper() or "Expecting" in report.failed.get("broken1", "") or "Failed" in report.failed.get("broken1", ""), f"…and why one failed ({report.failed.get('broken1', '')[:80]})")
    done = subprocess.run([sys.executable, "session_store.py", "migrate", "--all", "--base", str(BASE)], capture_output=True, text=True, cwd=Path(__file__).parent)
    check(done.returncode == 1 and "1 failed" in done.stdout, "the CLI exits non-zero when something failed, and names it")


def test_a_hand_appended_message_is_persisted() -> None:
    import acp_server

    manager = st.session_manager("settle1", BASE)
    live = agent("settle1", manager)
    live.messages.append({"role": "user", "content": [{"text": "run the long build"}]})
    live.messages.append({"role": "assistant", "content": [{"toolUse": {"toolUseId": "t1", "name": "shell", "input": {"command": "make"}}}]})
    acp_server._settle_cancelled_turn(live)
    reopened = agent("settle1", st.session_manager("settle1", BASE))
    last = reopened.messages[-1]
    check(last["role"] == "user" and last["content"][0]["toolResult"]["toolUseId"] == "t1" and "cancelled" in last["content"][0]["toolResult"]["content"][0]["text"], "the 'this call did not run' result added when a turn is cancelled is on disk — a restart does not leave a call with no result")
    check(len(reopened.messages) == 3, "…and nothing else was lost")


def run_turn(live: Agent, prompt: str) -> None:
    import asyncio

    asyncio.run(st.begin_turn(live, prompt, BASE, now=1_000.0))
    live(prompt)


def test_go_back() -> None:
    import asyncio

    live = agent("back1", st.session_manager("back1", BASE))
    for prompt in ("first question", "second question", "third question about the billing page"):
        run_turn(live, prompt)
    check(st.turns_taken(live) == 3 and len(live.messages) == 6, f"three turns taken, six messages ({st.turns_taken(live)}, {len(live.messages)})")
    marks = st.checkpoints("back1", BASE)
    check([(c.turn, c.prompt, c.kind) for c in marks] == [(1, "first question", "turn"), (2, "second question", "turn"), (3, "third question about the billing page", "turn")], "each turn left a checkpoint, labelled with its prompt")
    check([c.messages for c in marks] == [0, 2, 4], "…of the conversation as it was *before* the turn (0, 2, 4 messages)")

    result = asyncio.run(st.go_back(live, BASE, turns=1))
    check(result["ok"] and result["now"] == 2 and result["forgot"] == ["third question about the billing page"], f"going back one turn forgets that turn and only that ({result['forgot']})")
    check(len(live.messages) == 4 and st.turns_taken(live) == 2 and "third question" not in str(live.messages), "the live agent is back at four messages and two turns")
    reopened = agent("back1", st.session_manager("back1", BASE))
    check(len(reopened.messages) == 4 and st.turns_taken(reopened) == 2, "…and so is the session on disk — a restart does not bring the turn back")

    # it can be undone: the state it left was checkpointed
    listed = st.listing(live, BASE)
    check(any(c["kind"] == "before-back" for c in listed["checkpoints"]) and listed["turns"] == 2, "the list offers the state it left, labelled")
    forward = asyncio.run(st.go_back(live, BASE, to=result["undo_with"]))
    check(forward["ok"] and len(live.messages) == 6 and st.turns_taken(live) == 3 and forward["forgot"] == [], "going back is itself undone by restoring that checkpoint (six messages, three turns again)")

    # two turns, then a new turn on the abandoned branch: the newest checkpoint for a turn number is the one on the path
    asyncio.run(st.go_back(live, BASE, turns=2))
    check(len(live.messages) == 2 and st.turns_taken(live) == 1, "two turns back leaves one")
    run_turn(live, "a different second question")
    check(st.turns_taken(live) == 2 and "a different second question" in str(live.messages), "a new turn after going back starts a new branch")
    again = asyncio.run(st.go_back(live, BASE, turns=1))
    check(again["forgot"] == ["a different second question"] and len(live.messages) == 2, f"…and going back from it forgets *its* prompt, not the abandoned one ({again['forgot']})")

    far = asyncio.run(st.go_back(live, BASE, turns=9))
    check(not far["ok"] and "only" in far["reason"], f"asking for more turns than exist says so ({far['reason']})")
    empty = agent("back2", st.session_manager("back2", BASE))
    none = asyncio.run(st.go_back(empty, BASE))
    check(not none["ok"] and "nothing to go back to" in none["reason"], f"a session with no turns has nothing to go back to ({none['reason']})")
    check(not asyncio.run(st.go_back(live, BASE, to="nonsense"))["ok"], "an unknown checkpoint id is refused")

    # a session that could not be migrated has no checkpoints, and says so
    legacy = agent("back3", FileSessionManager(session_id="back3", storage_dir=str(BASE)))
    check(asyncio.run(st.begin_turn(legacy, "x", BASE)) is None and "snapshot storage" in asyncio.run(st.go_back(legacy, BASE))["reason"], "a legacy-manager session is not checkpointed and says why")

    # the oldest checkpoints are pruned
    many = agent("back4", st.session_manager("back4", BASE))
    for index in range(st.KEEP_CHECKPOINTS + 5):
        asyncio.run(st.begin_turn(many, f"turn {index}", BASE))
    kept = st.checkpoints("back4", BASE)
    check(len(kept) == st.KEEP_CHECKPOINTS and kept[-1].prompt == f"turn {st.KEEP_CHECKPOINTS + 4}", f"a session keeps its newest {st.KEEP_CHECKPOINTS} checkpoints ({len(kept)})")


async def test_go_back_through_the_server() -> None:
    import acp

    import _acp_harness_smoke_test as rig
    from harness_testing import ScriptedModel as Scripted

    cwd = rig.project("back-acp")
    rig.orchestrator_model([text("answer one"), text("answer two"), text("answer three")])
    server, conn = rig.fresh_server(rig.laya_answer("code", {"code": 0.97, "answer": 0.03}))
    created = await server.new_session(cwd=str(cwd))
    sid = created.session_id
    for prompt in ("what is in this project?", "now list the files", "and explain the biggest one"):
        await server.prompt(sid, [acp.text_block(prompt)])
    await rig.settle()

    listed = await server.ext_method("_sierralab/conversation/list", {"sessionId": sid})
    check(listed["enabled"] and listed["turns"] == 3 and [c["prompt"] for c in listed["checkpoints"]][:3] == ["and explain the biggest one", "now list the files", "what is in this project?"], f"the list is newest first, with the prompts ({[c['prompt'] for c in listed['checkpoints']]})")

    result = await server.ext_method("_sierralab/conversation/rewind", {"sessionId": sid, "turns": 2})
    agent_now = server._sessions[sid].agent
    check(result["ok"] and result["now"] == 1 and result["forgot"] == ["now list the files", "and explain the biggest one"], f"two turns back forgets two prompts ({result.get('forgot')})")
    check(len(agent_now.messages) == 2 and "answer two" not in str(agent_now.messages), "…the agent no longer has them")
    await rig.settle()
    told = [params for method, params in conn.ext if method == "sierralab/conversation"]
    check(told and told[-1]["event"] == "rewound" and told[-1]["now"] == 1, "clients are told (a transcript can rewind too)")

    # a new server process (the daemon restarted) sees the rewound conversation
    server2, conn2 = rig.fresh_server(rig.laya_answer("code", {"code": 0.97, "answer": 0.03}))
    await server2.load_session(cwd=str(cwd), session_id=sid)
    check(len(server2._sessions[sid].agent.messages) == 2, "after a restart the session is still rewound")

    # the next turn works and is counted from where it was taken back to
    rig.MODELS["orchestrator"].steps, rig.MODELS["orchestrator"].index = [text("answer again")], 0
    await server2.prompt(sid, [acp.text_block("a different question")])
    check(st.turns_taken(server2._sessions[sid].agent) == 2, "the next turn is turn 2")

    # refused while a turn is running
    import asyncio

    @__import__("strands").tool
    async def waiter() -> str:
        """Waits."""
        await asyncio.sleep(1.5)
        return "done"

    rig.MODELS["orchestrator"].steps, rig.MODELS["orchestrator"].index = [rig.call("waiter", {}), text("finished")], 0
    server2._sessions[sid].agent.tool_registry.register_tool(waiter)
    running = asyncio.create_task(server2.prompt(sid, [acp.text_block("wait please")]))
    await asyncio.sleep(0.5)
    refused = await server2.ext_method("_sierralab/conversation/rewind", {"sessionId": sid})
    check(not refused["ok"] and "running" in refused["reason"], f"going back is refused while a turn runs ({refused['reason']})")
    await running


def main() -> None:
    saved = os.getcwd()
    try:
        test_a_conversation_round_trips()
        test_migration_is_exact_and_additive()
        test_a_broken_legacy_session_is_not_lost()
        test_migrate_all_and_the_cli()
        test_a_hand_appended_message_is_persisted()
        test_go_back()
        __import__('asyncio').run(test_go_back_through_the_server())
    finally:
        os.chdir(saved)
    print("\nPASS: conversations are snapshots, the system prompt is always fresh, legacy sessions migrate exactly and additively, a broken one is kept")


if __name__ == "__main__":
    main()
    sys.stdout.flush()

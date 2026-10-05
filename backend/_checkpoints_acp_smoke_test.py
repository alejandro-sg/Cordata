"""Checkpoints through the real ACP server: ``auto`` mode, and then undo.

A scripted orchestrator (no model, no network) works in a temp project:

* an **auto-mode turn** that edits, creates and deletes files: a snapshot is taken before each call that may
  write — and none before a call that only reads — and the turn can be listed, previewed and undone;
* undoing **refuses while a turn is running**, and every applied restore is audited;
* a call **refused** (plan mode) takes no snapshot; a call held for approval is snapshotted before the prompt
  and a rejected one changes nothing;
* **turned off** (``checkpoints.enabled`` in the user's file) takes none; a project cannot turn it off for itself;
* a snapshot that **fails** does not stop the turn — but the turn's record and the client both say it was not
  protected, once.

    backend/.venv/bin/python _checkpoints_acp_smoke_test.py
"""

import asyncio
import json
import os

os.environ["SIERRALAB_PRISM"] = "off"

import provider_settings  # noqa: F401  (imported before agent_common, which chdirs — see main.py)

import acp
import checkpoints
import harness_events
import outcomes
import settings_store
import state_store
from _acp_harness_smoke_test import ROOT, call, fresh_server, laya_answer, orchestrator_model, project, settle, text
from _lab_acp_smoke_test import HitlConn, until
from _ollama_cloud_smoke_test import check

ROUTE = laya_answer("code", {"code": 0.97, "answer": 0.03})


def records() -> list[dict]:
    return [row["payload"] for row in reversed(state_store.recent_events(kind=outcomes.KIND, limit=50))]


def events(kind: str) -> list[dict]:
    return [row["payload"] for row in reversed(state_store.recent_events(kind=kind, limit=50))]


async def ext(server, method: str, sid: str, **params) -> dict:
    return await server.ext_method(f"_sierralab/{method}", {"sessionId": sid, **params})


async def test_an_auto_turn_can_be_undone() -> None:
    cwd = project("auto-undo")
    (cwd / "config.txt").write_text("original\n")
    (cwd / "old.txt").write_text("delete me\n")
    orchestrator_model([
        call("shell", {"command": "ls -la"}),  # reads: no snapshot
        call("shell", {"command": "echo edited > config.txt"}),
        call("shell", {"command": "echo made > created.txt"}),
        call("shell", {"command": "rm old.txt"}),
        call("shell", {"command": "cat config.txt"}),  # reads again
        text("done"),
    ])
    server, _ = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(cwd))).session_id
    await server.set_session_mode(sid, "auto")
    before = len(records())
    response = await server.prompt(sid, [acp.text_block("tidy up")])
    await settle()
    check(response.stop_reason == "end_turn", "the turn runs as it always did")
    check((cwd / "config.txt").read_text() == "edited\n" and (cwd / "created.txt").exists() and not (cwd / "old.txt").exists(), "auto mode made the changes")

    record = records()[before]
    check(record["checkpoints"].get("taken") == 3 and not record["checkpoints"].get("unprotected"), f"a snapshot before each of the three calls that write, none for the two that read ({record['checkpoints']})")
    check(record["checkpoints"].get("first") and record["checkpoints"].get("end"), "…and one for the project as the turn left it")

    listing = await ext(server, "checkpoints/list", sid)
    check(listing["enabled"] and len(listing["turns"]) == 1, f"the turn is listed ({listing['turns'][:1]})")
    turn = listing["turns"][0]
    check(turn["turn"] == record["id"] and turn["calls"] == 3 and turn["agents"], "…keyed by the same id as its outcome record")
    check({f["path"] for f in turn["files"]} == {"config.txt", "created.txt", "old.txt"}, f"…with the files it changed ({[f['path'] for f in turn['files']]})")

    preview = await ext(server, "checkpoints/undo", sid)
    check(preview["ok"] and preview["dry_run"] and preview["count"] == 3, f"a preview says what an undo would do ({preview['files']})")
    check((cwd / "created.txt").exists(), "…and changes nothing")

    done = await ext(server, "checkpoints/undo", sid, apply=True)
    await settle()
    check(done["ok"] and not done["dry_run"], f"undo applied ({done.get('reason')})")
    check((cwd / "config.txt").read_text() == "original\n" and (cwd / "old.txt").read_text() == "delete me\n" and not (cwd / "created.txt").exists(), "edit reverted, deleted file back, created file gone")
    audit = events("checkpoint_restore")
    check(audit and audit[0]["action"] == "undo" and audit[0]["files"] == 3, f"the restore is in the ledger ({audit[:1]})")
    check((await ext(server, "checkpoints/list", sid))["turns"][0]["undone"] is True, "the turn is marked undone")

    redo = await ext(server, "checkpoints/restore", sid, id=done["undo_with"], apply=True)
    check(redo["ok"] and (cwd / "created.txt").exists(), "…and the undo can be undone")

    later = outcomes.reconcile(str(cwd), now=__import__("time").time() + 7200)
    check(any(u["id"] == record["id"] for u in later), "the next reconcile judges the turn from the files, whatever happened to them")


async def test_no_undo_while_a_turn_runs() -> None:
    cwd = project("busy")
    orchestrator_model([call("shell", {"command": "sleep 1; echo x > slow.txt"}), text("done")])
    server, _ = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(cwd))).session_id
    await server.set_session_mode(sid, "auto")
    turn = asyncio.create_task(server.prompt(sid, [acp.text_block("go slow")]))
    await asyncio.sleep(0.4)
    busy = await ext(server, "checkpoints/undo", sid, apply=True)
    check(busy["ok"] is False and "turn is running" in busy["reason"], f"an undo during a turn is refused ({busy})")
    preview = await ext(server, "checkpoints/undo", sid)
    check("turn is running" not in (preview.get("reason") or ""), "…but a preview is fine")
    await turn


async def test_refused_and_held_calls() -> None:
    cwd = project("refused")
    (cwd / "keep.txt").write_text("keep\n")
    orchestrator_model([call("shell", {"command": "echo planned > plan.txt"}), text("described it")])
    server, _ = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(cwd))).session_id
    await server.set_session_mode(sid, "plan")
    before = len(records())
    await server.prompt(sid, [acp.text_block("change something")])
    await settle()
    check(not records()[before]["checkpoints"], f"plan mode refuses the call, so there is nothing to snapshot ({records()[before]['checkpoints']})")
    check(not (cwd / "plan.txt").exists(), "…and nothing was written")

    cwd = project("held")
    orchestrator_model([call("shell", {"command": "echo held > held.txt"}), text("ok, not doing it")])
    server, _ = fresh_server(ROUTE)
    conn = HitlConn()
    server.on_connect(conn)
    sid = (await server.new_session(cwd=str(cwd))).session_id
    before = len(records())
    turn = asyncio.create_task(server.prompt(sid, [acp.text_block("write held.txt")]))
    await until(lambda: conn.waiting, "build mode asks")
    listed = checkpoints.list_checkpoints(cwd)
    check(len(listed) == 1 and listed[0]["kind"] == "pre" and listed[0]["tool"] == "shell", f"a call held for approval is snapshotted before the prompt is shown ({listed})")
    conn.decide("reject")
    await turn
    await settle()
    check(not (cwd / "held.txt").exists(), "a rejected call changes nothing")
    check(len(checkpoints.list_checkpoints(cwd)) <= 3, "…and re-entering the call on resume does not snapshot it again")


async def test_off_and_unprotected() -> None:
    cwd = project("off")
    settings_store.set_value("user", "checkpoints.enabled", False, cwd)
    orchestrator_model([call("shell", {"command": "echo a > a.txt"}), text("done")])
    server, _ = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(cwd))).session_id
    await server.set_session_mode(sid, "auto")
    before = len(records())
    await server.prompt(sid, [acp.text_block("write a")])
    await settle()
    check(not records()[before]["checkpoints"] and checkpoints.list_checkpoints(cwd) == [], "switched off in the user's settings: no snapshot")
    settings_store.set_value("user", "checkpoints.enabled", True, cwd)

    cwd = project("project-cannot-disable")
    (cwd / ".sierralab").mkdir()
    (cwd / ".sierralab" / "settings.json").write_text(json.dumps({"checkpoints": {"enabled": False}}))
    orchestrator_model([call("shell", {"command": "echo a > a.txt"}), text("done")])
    server, _ = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(cwd))).session_id
    await server.set_session_mode(sid, "auto")
    before = len(records())
    await server.prompt(sid, [acp.text_block("write a")])
    await settle()
    check(records()[before]["checkpoints"].get("taken") == 1, "a repository's own settings cannot turn the undo off")

    cwd = project("unprotected")
    orchestrator_model([call("shell", {"command": "echo a > a.txt"}), call("shell", {"command": "echo b > b.txt"}), text("done")])
    server, _ = fresh_server(ROUTE)
    seen: list[dict] = []
    unsubscribe = harness_events.subscribe(lambda kind, payload: seen.append(payload) if kind == "checkpoint" else None)
    real_take = checkpoints.take
    checkpoints.take = lambda *a, **k: checkpoints.Result(False, reason="disk full")
    try:
        sid = (await server.new_session(cwd=str(cwd))).session_id
        await server.set_session_mode(sid, "auto")
        before = len(records())
        response = await server.prompt(sid, [acp.text_block("write both")])
        await settle()
    finally:
        checkpoints.take = real_take
        unsubscribe()
    check(response.stop_reason == "end_turn" and (cwd / "a.txt").exists() and (cwd / "b.txt").exists(), "a snapshot that fails does not stop the turn")
    note = records()[before]["checkpoints"]
    check(note.get("unprotected") == 2 and note.get("reason") == "disk full" and not note.get("taken"), f"…the record says the turn was not protected ({note})")
    check(len(seen) == 1 and seen[0]["phase"] == "unprotected" and seen[0]["reason"] == "disk full", f"…and the client is told once, not per call ({seen})")
    check(len(events("checkpoint")) >= 1, "…and the ledger has it")


async def main() -> None:
    await test_an_auto_turn_can_be_undone()
    await test_no_undo_while_a_turn_runs()
    await test_refused_and_held_calls()
    await test_off_and_unprotected()
    print("\ncheckpoints (ACP): all checks passed")


if __name__ == "__main__":
    asyncio.run(main())

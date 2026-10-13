"""The ten defects a native front door would have hit first (docs/app.md, phase B0) — one regression check each, offline.

1. **a second prompt mid-turn is refused** before it touches the running turn (it used to reset the turn's recorder,
   checkpoint, cancel flag and stream handle, and then Strands refused it anyway — Stop could no longer stop);
2. **ext methods resolve a daemon id** to the session this process hosts (``_known``), as the mode switch always did;
3. **conversations are recorded in ``state.db``**: processes writing at once lose nothing (the JSON index lost writes),
   ``session/list`` pages through every one, and a relative cwd is refused;
4. **Stop during a goal's judge** ends the goal (the next turn used to clear the cancel and run anyway);
5. **a cancelled orchestrator run is ``cancelled``**, not stuck in the phase it was in, and its record says so;
6. **a turn's diff is published** as ``_cordata/diff`` (``GetDiff`` had no producer but a test fixture), masked;
7. **approvals stranded by a dead process are announced once**, then closed out — and never for a live session;
8. **``quota_usage.role``/``.worker`` are part of the schema**, added to a ledger an older build created;
9. **one live writer per conversation**: a second process opening it is refused while the holder lives;
10. **``CORDATA_SESSIONS_DIR``** moves conversations out of the checkout.

    backend/.venv/bin/python _backend_hardening_smoke_test.py
"""

from __future__ import annotations

import asyncio
import os
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

os.environ["CORDATA_PRISM"] = "off"

import provider_settings  # noqa: F401,E402  (before agent_common, which chdirs)

import acp  # noqa: E402

import acp_server  # noqa: E402
import goal as goal_module  # noqa: E402
import state_store  # noqa: E402
from _acp_harness_smoke_test import call, fresh_server, laya_answer, orchestrator_model, project, settle, text  # noqa: E402
from harness_testing import check  # noqa: E402

HERE = Path(__file__).parent.resolve()
ROUTE = laya_answer("code", {"code": 0.97, "answer": 0.03})


def child_env(**extra: str) -> dict[str, str]:
    return {**os.environ, "PYTHONPATH": str(HERE), **extra}


def run_child(code: str, **extra: str) -> str:
    done = subprocess.run([sys.executable, "-c", code], cwd=HERE, env=child_env(**extra), capture_output=True, text=True, timeout=120)
    if done.returncode != 0:
        raise AssertionError(f"child failed: {done.stderr[-2000:]}")
    return done.stdout.strip()


# ------------------------------------------------------------------------------------------------ 1. busy guard


async def test_a_second_prompt_is_refused() -> None:
    print("\n=== 1. a second prompt mid-turn is refused, with no side effect ===")
    orchestrator_model([text("unused")])
    server, _ = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(project("busy")))).session_id
    release = asyncio.Event()
    turns: list[str] = []

    async def slow_turn(session_id, prompt, **kwargs):
        turns.append(acp_server._extract_prompt_text(prompt))
        await release.wait()
        return acp.PromptResponse(stopReason="end_turn")

    server._turn = slow_turn
    first = asyncio.create_task(server.prompt(sid, [acp.text_block("the first")]))
    await asyncio.sleep(0.05)
    server._cancelled.add("sentinel")
    try:
        await server.prompt(sid, [acp.text_block("the second")])
    except acp.RequestError as error:
        data = error.to_error_obj().get("data") or {}
        check(data.get("error") == "busy", f"refused as busy ({error.to_error_obj()})")
    else:
        check(False, "the second prompt is refused")
    check(turns == ["the first"], f"…before it reached the turn path ({turns})")
    check("sentinel" in server._cancelled and sid in server._busy, "…and before it changed any of the running turn's state")
    await server.cancel(sid)
    check(sid in server._cancelled, "Stop still reaches the running turn")
    release.set()
    response = await first
    check(response.stop_reason == "end_turn" and sid not in server._busy, "the first turn finishes and the session is free again")
    release.clear()
    again = asyncio.create_task(server.prompt(sid, [acp.text_block("the third")]))
    await asyncio.sleep(0.02)
    check(sid not in server._cancelled, "a Stop pressed for the previous turn does not cancel the next one")
    release.set()
    await again


# --------------------------------------------------------------------------------------------- 2. ext + _known


async def test_ext_methods_resolve_a_daemon_id() -> None:
    print("\n=== 2. ext methods resolve the daemon's id for this child ===")
    orchestrator_model([text("hello")])
    server, _ = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(project("known")))).session_id
    await server.prompt(sid, [acp.text_block("hi")])
    server._goals[sid] = goal_module.Goal(condition="the tests pass")
    status = await server.ext_method("_cordata/goal/status", {"sessionId": "daemon-7f3a"})
    check((status.get("goal") or {}).get("condition") == "the tests pass", f"goal/status by the daemon's id finds this session's goal ({status})")
    listing = await server.ext_method("_cordata/conversation/list", {"sessionId": "daemon-7f3a"})
    check(listing.get("ok", True) is not False and "turns" in listing, f"conversation/list is not 'no such session' ({listing})")


# ------------------------------------------------------------------------------------------- 3. sessions table


async def test_sessions_are_recorded_safely() -> None:
    print("\n=== 3. conversations in state.db: concurrent writers, paging, absolute cwd ===")
    db = os.environ.get("CORDATA_STATE_DB") or str(state_store.path())
    code = (
        "import sys, state_store\n"
        "for i in range(40):\n"
        "    state_store.touch_session(f'w{sys.argv[1]}-{i}', cwd='/tmp/concurrent', title=str(i))\n"
    )
    procs = [subprocess.Popen([sys.executable, "-c", code, str(n)], cwd=HERE, env=child_env(CORDATA_STATE_DB=db)) for n in range(4)]
    for proc in procs:
        check(proc.wait(timeout=120) == 0, f"writer {proc.args[-1]} finished")
    rows = state_store.list_sessions(cwd="/tmp/concurrent", limit=1000)
    check(len(rows) == 160, f"four processes, 40 conversations each: all 160 recorded ({len(rows)})")

    server, _ = fresh_server(ROUTE)
    base = time.time()
    for i in range(120):
        state_store.touch_session(f"page-{i:03d}", cwd="/tmp/paged", title=f"t{i}")
        state_store.connect().execute("UPDATE sessions SET updated_ts = ? WHERE session_id = ?", (base + i, f"page-{i:03d}"))
    real_exists = acp_server.session_store.exists
    acp_server.session_store.exists = lambda sid, base: True
    try:
        seen, cursor, pages = [], None, 0
        while True:
            page = await server.list_sessions(cwd="/tmp/paged", cursor=cursor)
            seen += [row.session_id for row in page.sessions]
            pages += 1
            cursor = page.next_cursor
            if not cursor:
                break
    finally:
        acp_server.session_store.exists = real_exists
    check(len(seen) == 120 and len(set(seen)) == 120 and pages == 3, f"session/list pages through all 120, 50 at a time ({len(seen)} in {pages} pages)")
    check(seen[0] == "page-119" and seen[-1] == "page-000", "…newest first")

    try:
        await server.new_session(cwd=".")
    except acp.RequestError as error:
        check("absolute" in str(error.to_error_obj()), "a relative cwd is refused, saying why")
    else:
        check(False, "a relative cwd is refused")


# ------------------------------------------------------------------------------------------------ 4. goal + Stop


async def test_stop_during_the_goal_judge() -> None:
    print("\n=== 4. Stop while a goal is being judged ends the goal ===")
    orchestrator_model([text("unused")])
    server, _ = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(project("goal-stop")))).session_id
    loop = asyncio.get_running_loop()
    turns: list[str] = []

    async def quick_turn(session_id, prompt, **kwargs):
        turns.append(acp_server._extract_prompt_text(prompt))
        return acp.PromptResponse(stopReason="end_turn")

    def judge(goal, *, cwd, messages, ask):
        asyncio.run_coroutine_threadsafe(server.cancel(sid), loop).result(5)  # the person presses Stop now
        return goal_module.Verdict(met=False, reason="not yet", how="judge")

    server._turn = quick_turn
    real = acp_server.goal_module.evaluate
    acp_server.goal_module.evaluate = judge
    try:
        response = await server.prompt(sid, [acp.text_block("/goal the build is green --turns 5")])
    finally:
        acp_server.goal_module.evaluate = real
    goal = server._goals.get(sid)
    check(response.stop_reason == "cancelled", f"the prompt ends cancelled ({response.stop_reason})")
    check(len(turns) == 1, f"no follow-up turn ran ({len(turns)} turns)")
    check(goal is not None and goal.state == "stopped" and "cancel" in goal.reason, f"the goal is stopped, and says why ({goal and goal.public()})")


# ---------------------------------------------------------------------------------------- 5. orchestrator cancel


async def test_a_cancelled_run_is_cancelled() -> None:
    print("\n=== 5. a cancelled orchestrator run ends 'cancelled' ===")
    import scrubbed_sandbox

    if not scrubbed_sandbox.seatbelt_works():
        os.environ["CORDATA_SANDBOX_SEATBELT"] = "off"
    from orchestrator import machine
    from orchestrator.dispatch import StaticDispatcher
    from orchestrator.quota_ledger import Pool, QuotaLedger
    from orchestrator.testkit import cand, claude_fake, draft, make_project, ollama_fake, steps_script

    root = Path(tempfile.mkdtemp(prefix="cancel-run-"))
    proj = make_project(root, "cancel")
    planner = ollama_fake(plan=draft(), audit={"verdict": "accept", "items": []})
    executor = claude_fake(steps=steps_script(), delay=30.0)
    disp = StaticDispatcher({"plan": [cand(planner)], "execute": [cand(executor)], "harden": [cand(executor)], "audit": [cand(planner)]})
    led = QuotaLedger([Pool("cc-c", "claude_code", ".*", unit="tokens", window_limit=10_000_000), Pool("ol-c", "ollama", ".*", unit="requests", window_limit=10_000)])
    m = machine.OrchestratorMachine(goal="Implement the calc package so the tests pass.", project=str(proj), dispatcher=disp, ledger=led)
    task = asyncio.ensure_future(m.run())
    for _ in range(200):
        if m.record.state == "execute":
            break
        await asyncio.sleep(0.05)
    check(m.record.state == "execute", f"the run reached execute ({m.record.state})")
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        pass
    check(m.record.state == "cancelled", f"the record is cancelled ({m.record.state})")
    loaded = machine.load(m.record.run_id)
    check(loaded is not None and loaded.state == "cancelled" and "cancel" in loaded.reason, f"…durably, with a reason ({loaded and loaded.state}: {loaded and loaded.reason})")
    check(sum(1 for name, fn in vars(machine.OrchestratorMachine).items() if name == "_keep_discarded") == 1 and machine.OrchestratorMachine._keep_discarded.__doc__, "_keep_discarded is defined once")


# ---------------------------------------------------------------------------------------------- 6. diff producer


async def test_a_turn_diff_is_published() -> None:
    print("\n=== 6. a turn's diff is published as _cordata/diff ===")
    cwd = project("diff")
    (cwd / "config.txt").write_text("original\n")
    orchestrator_model([
        call("shell", {"command": "echo edited > config.txt"}),
        call("shell", {"command": "echo 'token = ghp_abcdefghijklmnopqrstuvwxyz0123456789' > made.txt"}),
        text("done"),
    ])
    server, conn = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(cwd))).session_id
    await server.set_session_mode(sid, "auto")
    response = await server.prompt(sid, [acp.text_block("change the config")])
    check(response.stop_reason == "end_turn", "the turn ran")
    diffs: list[dict] = []
    for _ in range(100):
        await settle()
        diffs = [params for method, params in conn.ext if method == "cordata/diff"]
        if diffs:
            break
    check(len(diffs) == 1, f"one _cordata/diff for the turn ({len(diffs)})")
    if diffs:
        diff = diffs[0]
        check(diff["id"].startswith(f"{sid}/") and diff["source"] == "turn" and diff["sessionId"] == sid, f"…keyed by session and turn ({diff['id']})")
        check(set(diff["files"]) == {"config.txt", "made.txt"}, f"…naming the files it changed ({diff['files']})")
        check("+edited" in diff["unified"] and "-original" in diff["unified"], "…with the unified diff")
        check("ghp_abcdefghijklmnopqrstuvwxyz0123456789" not in diff["unified"], "…and a secret in it masked")


# ------------------------------------------------------------------------------------------- 7. stale approvals


async def test_stranded_approvals_are_announced_once() -> None:
    print("\n=== 7. approvals stranded by a dead process: said once, then closed ===")
    cwd = project("stranded")
    orchestrator_model([text("hello")])
    server, _ = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(cwd))).session_id
    await server.prompt(sid, [acp.text_block("hi")])
    state_store.stage_intervention(session_id=sid, tool="shell", tool_input={"command": "make deploy"}, intervention_id=f"{sid}/stale-1")

    acp_server._LEASED.discard(sid)
    state_store.release_session(sid)
    server2, conn2 = fresh_server(ROUTE)
    await server2.load_session(cwd=str(cwd), session_id=sid)
    said = [u for u in conn2.updates if "will not be asked again" in str(u)]
    check(len(said) == 1, "a rebuilt session says its stranded approval will not be asked again")
    check(not state_store.pending_interventions(sid) and (state_store.get_intervention(f"{sid}/stale-1") or {}).get("status") == "expired", "…and closes it out as expired")

    server3, conn3 = fresh_server(ROUTE)
    await server3.load_session(cwd=str(cwd), session_id=sid)
    check(not [u for u in conn3.updates if "will not be asked again" in str(u)], "the next load does not say it again")

    state_store.stage_intervention(session_id=sid, tool="shell", tool_input={"command": "ls"}, intervention_id=f"{sid}/live-1")
    before = len(conn3.updates)
    await server3.load_session(cwd=str(cwd), session_id=sid)
    check(not [u for u in conn3.updates[before:] if "will not be asked again" in str(u)], "a front door attaching to a live session is not told a live approval is stale")
    check((state_store.get_intervention(f"{sid}/live-1") or {}).get("status") == "staged", "…and the live approval stays open")


# ---------------------------------------------------------------------------------------------- 8. quota schema


def test_quota_columns_are_in_the_schema() -> None:
    print("\n=== 8. quota_usage.role/.worker are part of the schema ===")
    folder = Path(tempfile.mkdtemp(prefix="old-ledger-"))
    old = folder / "state.db"
    conn = sqlite3.connect(old)
    conn.executescript(
        "CREATE TABLE schema_version (version INTEGER NOT NULL); INSERT INTO schema_version VALUES (2);"
        "CREATE TABLE quota_usage (id INTEGER PRIMARY KEY AUTOINCREMENT, ts REAL NOT NULL, session_id TEXT NOT NULL, step_id TEXT,"
        " pool_id TEXT NOT NULL, model TEXT NOT NULL, tokens_in INTEGER NOT NULL DEFAULT 0, tokens_out INTEGER NOT NULL DEFAULT 0,"
        " cost_usd REAL NOT NULL DEFAULT 0.0, is_error_limit INTEGER NOT NULL DEFAULT 0, requests INTEGER NOT NULL DEFAULT 1, reset_ts REAL);"
    )
    conn.close()
    probe = "import state_store; c = state_store.connect(); print(','.join(r[1] for r in c.execute('PRAGMA table_info(quota_usage)')) + '|' + str(c.execute('SELECT version FROM schema_version').fetchone()[0]))"
    columns, version = run_child(probe, CORDATA_STATE_DB=str(old)).split("|")
    check({"role", "worker"} <= set(columns.split(",")), f"an older ledger gains role and worker on open ({columns})")
    check(version == str(state_store.SCHEMA_VERSION), f"…and records the new schema version ({version})")
    fresh = run_child(probe, CORDATA_STATE_DB=str(folder / "new.db")).split("|")[0]
    check({"role", "worker"} <= set(fresh.split(",")), "a new ledger has them from the start")


# ------------------------------------------------------------------------------------------------ 9. the lease


async def test_one_live_writer_per_conversation() -> None:
    print("\n=== 9. one live writer per conversation ===")
    db = os.environ.get("CORDATA_STATE_DB") or str(state_store.path())
    claim = "import state_store; print(state_store.claim_session('lease-1', cwd='/tmp') or '')"
    check(state_store.claim_session("lease-1", cwd="/tmp") is None, "this process takes the lease")
    check(state_store.claim_session("lease-1", cwd="/tmp") is None, "…and taking it again is fine")
    check(run_child(claim, CORDATA_STATE_DB=db) == str(os.getpid()), "another process is told who holds it")
    state_store.release_session("lease-1")
    check(run_child(claim, CORDATA_STATE_DB=db) == "", "once released, another process can take it")
    check(state_store.claim_session("lease-1", cwd="/tmp") is None, "a lease left by a process that is gone is taken over")

    cwd = project("leased")
    orchestrator_model([text("hello")])
    server, _ = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(cwd))).session_id
    await server.prompt(sid, [acp.text_block("hi")])
    holder = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
    try:
        state_store.connect().execute("UPDATE sessions SET host_pid = ? WHERE session_id = ?", (holder.pid, sid))
        acp_server._LEASED.discard(sid)
        other, _ = fresh_server(ROUTE)
        try:
            await other.load_session(cwd=str(cwd), session_id=sid)
        except acp.RequestError as error:
            data = error.to_error_obj().get("data") or {}
            check(data.get("error") == "session_leased" and data.get("pid") == holder.pid, f"opening it in a second process is refused, naming the holder ({data})")
        else:
            check(False, "opening a leased conversation is refused")
    finally:
        holder.kill()
        holder.wait()
    third, _ = fresh_server(ROUTE)
    await third.load_session(cwd=str(cwd), session_id=sid)
    check(sid in third._sessions and (state_store.get_session(sid) or {}).get("host_pid") == os.getpid(), "when the holder is gone it opens, and the lease is ours")


# ------------------------------------------------------------------------------------------- 10. sessions dir


def test_sessions_dir_can_move() -> None:
    print("\n=== 10. CORDATA_SESSIONS_DIR moves conversations out of the checkout ===")
    target = Path(tempfile.mkdtemp(prefix="sessions-elsewhere-")) / "nested" / "sessions"
    shown = run_child("import provider_settings, agent_common; print(agent_common.SESSIONS_DIR)", CORDATA_SESSIONS_DIR=str(target))
    check(Path(shown) == target.resolve() and target.is_dir(), f"SESSIONS_DIR follows the variable and is created ({shown})")
    default = run_child("import provider_settings, agent_common; print(agent_common.SESSIONS_DIR)", CORDATA_SESSIONS_DIR="")
    check(Path(default) == HERE / "sessions", f"unset, it is the checkout's sessions/ as before ({default})")


async def main() -> None:
    await test_a_second_prompt_is_refused()
    await test_ext_methods_resolve_a_daemon_id()
    await test_sessions_are_recorded_safely()
    await test_stop_during_the_goal_judge()
    await test_a_cancelled_run_is_cancelled()
    await test_a_turn_diff_is_published()
    await test_stranded_approvals_are_announced_once()
    test_quota_columns_are_in_the_schema()
    await test_one_live_writer_per_conversation()
    test_sessions_dir_can_move()
    print("\nALL OK")


if __name__ == "__main__":
    asyncio.run(main())

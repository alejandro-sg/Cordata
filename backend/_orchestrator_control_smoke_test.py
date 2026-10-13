"""Orchestrator runs a front door can see and steer (docs/app.md §6.2, phase B2) — offline, on scripted workers.

In process (the machine and its safe points):
1. **pause at a safe point** — a pause asked for while a step runs lets the step finish, stops before the next worker
   call, and a resume picks the run up in EXECUTE with the finished steps kept;
2. **pause between phases** — asked for as HARDEN begins, the run stops there and a resume runs HARDEN, not EXECUTE;
3. **cancel now** — stops the worker call in flight, rolls its step back, and the run is `cancelled` (terminal);
4. **cancel with restore** — the project goes back to the run's start, and the work is saved as a patch first.

Detached (the real control file and a real child process, ``fixtures/fake_orchestrator.py`` in place of
``-m orchestrator``):
5. **a run outlives the process that started it** and finishes; its events file, host record and liveness say so;
6. **pause and resume across processes**; a second resume of a live run is refused; a killed run is `interrupted` and
   resumes; cancelling a paused run is terminal;
7. **reads are bounded and masked** — the run list, `show_run` (steps, usage per pod, control flags), artifacts by
   listed name only, secrets masked; worker events and the full usage (pods, totals, pools, envelope) are in the stream;
8. **through the ACP server** — `start` refused in plan mode and for `allowDirect` without confirmation, audited, then
   started; its events reach the session as `_cordata/orchestrator`.

    backend/.venv/bin/python _orchestrator_control_smoke_test.py
"""

from __future__ import annotations

import asyncio
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

os.environ["CORDATA_PRISM"] = "off"

import provider_settings  # noqa: F401,E402
import scrubbed_sandbox  # noqa: E402

if not scrubbed_sandbox.seatbelt_works():
    os.environ["CORDATA_SANDBOX_SEATBELT"] = "off"

import acp  # noqa: E402

import state_store  # noqa: E402
from harness_testing import check  # noqa: E402
from orchestrator import control, machine  # noqa: E402
from orchestrator.dispatch import StaticDispatcher  # noqa: E402
from orchestrator.quota_ledger import Pool, QuotaLedger  # noqa: E402
from orchestrator.testkit import FILES, agy_fake, cand, claude_fake, draft, make_project, ollama_fake, steps_script  # noqa: E402

HERE = Path(__file__).resolve().parent
ROOT = Path(tempfile.mkdtemp(prefix="orch-control-"))
FAKE = str(HERE / "fixtures" / "fake_orchestrator.py")


def setup(name: str, delay: float = 0.0):
    project = make_project(ROOT, name)
    planner = ollama_fake(plan=draft(), audit={"verdict": "accept", "items": []})
    executor = claude_fake(steps=steps_script(), delay=delay)
    hardener = agy_fake()
    disp = StaticDispatcher({"plan": [cand(planner)], "execute": [cand(executor)], "harden": [cand(hardener)], "audit": [cand(planner)]})
    led = QuotaLedger([Pool(f"cc-{name}", "claude_code", ".*", unit="tokens", window_limit=10_000_000), Pool(f"agy-{name}", "antigravity", ".*", unit="tokens", window_limit=10_000_000), Pool(f"ol-{name}", "ollama", ".*", unit="requests", window_limit=10_000)])
    return project, disp, led, executor


def states(record) -> list[str]:
    return [h["state"] for h in record.history]


async def test_pause_at_a_step() -> None:
    print("\n=== 1. pause at a safe point, resume in EXECUTE ===")
    project, disp, led, executor = setup("pause-step", delay=0.3)
    controls = control.Controls(None)
    events: list[dict] = []

    def on_event(event: dict) -> None:
        events.append(event)
        if event["kind"] == "step_dispatch" and sum(1 for e in events if e["kind"] == "step_dispatch") == 1:
            controls.request("pause", by="test")

    m = machine.OrchestratorMachine(goal="Implement the calc package so the tests pass.", project=str(project), dispatcher=disp, ledger=led, on_event=on_event, controls=controls)
    record = await m.run()
    steps = record.plan["steps"]
    done = [s["step_id"] for s in steps if s["status"] == "completed"]
    check(record.state == "paused" and record.resume_state == "execute", f"the run pauses ({record.state}, resume in {record.resume_state})")
    check(len(done) == 1 and len(executor.calls) == 1, f"…after the step in flight finished, before the next call ({done}, {len(executor.calls)} calls)")
    check(any(e["kind"] == "paused" and e.get("reason") == "user_pause" for e in events), "a paused event says it was the person")

    resumed = await machine.resume(record.run_id, disp, ledger=led, controls=control.Controls(None))
    check(resumed.state == "done", f"the resume finishes ({resumed.state}: {resumed.reason})")
    check(len(executor.calls) == len(steps), f"…without redoing the finished step ({len(executor.calls)} execute calls for {len(steps)} steps)")
    check(all((project / p).read_text() == t for p, t in FILES.items()), "the project ends as a full run would leave it")


async def test_pause_between_phases() -> None:
    print("\n=== 2. pause between phases, resume in that phase ===")
    project, disp, led, _ = setup("pause-phase")
    controls = control.Controls(None)

    def on_event(event: dict) -> None:
        if event["kind"] == "state" and event.get("state") == "harden":
            controls.request("pause", by="test")

    record = await machine.OrchestratorMachine(goal="Implement the calc package so the tests pass.", project=str(project), dispatcher=disp, ledger=led, on_event=on_event, controls=controls).run()
    check(record.state == "paused" and record.resume_state == "harden", f"paused as HARDEN began ({record.state}, {record.resume_state})")
    resumed = await machine.resume(record.run_id, disp, ledger=led, controls=control.Controls(None))
    after = states(resumed)[states(resumed).index("paused") + 1 :]
    check(after[:1] == ["harden"] and resumed.state == "done", f"the resume runs HARDEN, not EXECUTE ({after})")


async def test_cancel_now() -> None:
    print("\n=== 3. cancel now stops the call in flight ===")
    project, disp, led, executor = setup("cancel-now", delay=1.5)
    controls = control.Controls(None)
    m = machine.OrchestratorMachine(goal="Implement the calc package so the tests pass.", project=str(project), dispatcher=disp, ledger=led, controls=controls)
    task = asyncio.ensure_future(m.run())
    for _ in range(100):
        if executor.calls:
            break
        await asyncio.sleep(0.05)
    controls.request("cancel", now=True, by="test")
    record = await asyncio.wait_for(task, 20)
    check(record.state == "cancelled" and "person" in record.reason, f"the run is cancelled ({record.state}: {record.reason})")
    check(executor.calls and executor.calls[0].task_id in executor._cancelled, "the worker call in flight was told to stop")
    check(machine.load(record.run_id).state == "cancelled", "…durably")


async def test_cancel_with_restore() -> None:
    print("\n=== 4. cancel with restore puts the project back ===")
    project, disp, led, executor = setup("cancel-restore", delay=0.3)
    controls = control.Controls(None)

    def on_event(event: dict) -> None:
        if event["kind"] == "step_result" and event.get("status") == "completed":
            controls.request("cancel", restore=True, by="test")

    record = await machine.OrchestratorMachine(goal="Implement the calc package so the tests pass.", project=str(project), dispatcher=disp, ledger=led, on_event=on_event, controls=controls).run()
    written = [p for p in FILES if (project / p).exists()]
    check(record.state == "cancelled" and not written, f"cancelled and restored: nothing the run wrote is left ({written})")
    patches = list(machine.artifacts_dir(record.run_id).glob("discarded-*.patch"))
    check(bool(patches), "…and what it discarded is saved as a patch")


def wait_for(run_id: str, wanted: set[str], timeout: float = 60.0) -> str:
    deadline = time.time() + timeout
    live = ""
    while time.time() < deadline:
        live = (control.show_run(run_id) or {}).get("live", "")
        if live in wanted:
            return live
        time.sleep(0.2)
    return live


def test_detached_runs() -> None:
    print("\n=== 5–6. detached runs: outlive their starter, pause, resume, interrupt, cancel ===")
    os.environ["CORDATA_ORCHESTRATOR_ENTRYPOINT"] = FAKE
    project = make_project(ROOT, "detached")
    run_id = control.new_run_id()
    # Started from a process that exits at once — the session child the daemon reaps.
    starter = (
        "import sys, json; sys.path.insert(0, %r)\n"
        "import provider_settings\n"
        "from orchestrator import control\n"
        "print(json.dumps(control.spawn_run('Implement the calc package so the tests pass.', %r, session_id='sess-1', run_id=%r)))\n"
    ) % (str(HERE), str(project), run_id)
    done = subprocess.run([sys.executable, "-c", starter], cwd=HERE, capture_output=True, text=True, timeout=60)
    started = json.loads(done.stdout.strip().splitlines()[-1])
    check(started["ok"] and started["runId"] == run_id, f"spawned ({started.get('pid')})")
    live = wait_for(run_id, {"done", "failed", "cancelled"})
    check(live == "done", f"the run finished after its starter exited ({live})")
    shown = control.show_run(run_id)
    check(shown["sessionId"] == "sess-1" and shown["control"] == {"canPause": False, "canCancel": False, "canResume": False}, f"it remembers its session; nothing to control now ({shown['control']})")
    events = control.read_events(run_id)["events"]
    kinds = {e.get("kind") for e in events}
    check({"state", "step_dispatch", "step_result", "usage", "worker"} <= kinds, f"its events file has the run, the pods and the usage ({sorted(kinds)})")
    usage = [e for e in events if e.get("kind") == "usage"][-1]
    check(all(k in usage for k in ("pods", "totals", "pools", "envelope")) and usage["pools"] and "soft" in json.dumps(usage["pools"][0]), "usage carries pods, totals, pools (with their lines) and the envelope")
    dispatch = next(e for e in events if e.get("kind") == "step_dispatch")
    check(dispatch.get("pool_id"), "a dispatch names the pool it spends")

    os.environ["FAKE_ORCH_DELAY"] = "0.6"
    project2 = make_project(ROOT, "detached-pause")
    run2 = control.spawn_run("Implement the calc package so the tests pass.", str(project2))["runId"]
    check(wait_for(run2, {"running"}, 20) == "running", "a slow run is running")
    time.sleep(1.0)
    asked = control.request(run2, "pause", by="test")
    check(asked["ok"], f"pause requested through the control file ({asked})")
    check(wait_for(run2, {"paused"}) == "paused", "…and the run pauses")
    check(control.show_run(run2)["control"]["canResume"], "a paused run offers resume")
    again = control.resume_run(run2)
    check(again["ok"], f"resumed ({again})")
    refused = control.resume_run(run2)
    check(not refused["ok"] and "still running" in refused["reason"], f"a second resume of a live run is refused ({refused})")
    check(wait_for(run2, {"done", "failed"}) == "done", "the resumed run finishes")

    project3 = make_project(ROOT, "detached-kill")
    run3 = control.spawn_run("Implement the calc package so the tests pass.", str(project3))["runId"]
    check(wait_for(run3, {"running"}, 20) == "running", "another slow run is running")
    time.sleep(1.0)
    os.killpg(control.host(run3)["pid"], signal.SIGKILL)
    check(wait_for(run3, {"interrupted"}, 10) == "interrupted", "killed, it is interrupted (non-terminal, process gone)")
    os.environ["FAKE_ORCH_DELAY"] = "0"
    check(control.resume_run(run3)["ok"] and wait_for(run3, {"done", "failed"}) == "done", "an interrupted run resumes and finishes")

    os.environ["FAKE_ORCH_DELAY"] = "0.6"
    project4 = make_project(ROOT, "detached-cancel")
    run4 = control.spawn_run("Implement the calc package so the tests pass.", str(project4))["runId"]
    wait_for(run4, {"running"}, 20)
    time.sleep(1.0)
    control.request(run4, "pause", by="test")
    wait_for(run4, {"paused"})
    cancelled = control.request(run4, "cancel", by="test")
    check(cancelled == {"ok": True, "state": "cancelled"} and control.show_run(run4)["live"] == "cancelled", "cancelling a paused run is terminal")
    os.environ["FAKE_ORCH_DELAY"] = "0"


def test_reads() -> None:
    print("\n=== 7. reads are bounded and masked ===")
    rows = control.list_runs(limit=50)
    check(len(rows) >= 4 and all("live" in r for r in rows), f"runs are listed, each with its liveness ({len(rows)})")
    run_id = next(r["runId"] for r in rows if r["live"] == "done")
    folder = control.run_dir(run_id)
    (folder / "notes.md").write_text("token = ghp_abcdefghijklmnopqrstuvwxyz0123456789\n")
    names = {a["name"] for a in control.artifacts_list(run_id)}
    check({"plan.md", "walkthrough.md", "notes.md"} <= names and "control.ndjson" not in names, f"artifacts are listed ({sorted(names)})")
    read = control.artifacts_read(run_id, "notes.md")
    check(read["ok"] and "ghp_abcdef" not in read["text"], "a secret in an artifact is masked")
    check(not control.artifacts_read(run_id, "../../state.db")["ok"] and not control.artifacts_read(run_id, "host.json.bak")["ok"], "only listed names are read")
    check(not control.artifacts_read("../escape", "plan.md")["ok"], "a run id cannot walk out of the runs folder")
    paged = control.artifacts_read(run_id, "walkthrough.md", limit=10)
    check(paged["ok"] and paged["more"] and paged["end"] == 10, "reads page")


async def test_through_the_server() -> None:
    print("\n=== 8. through the ACP server ===")
    os.environ["CORDATA_ORCHESTRATOR_ENTRYPOINT"] = FAKE
    from _acp_harness_smoke_test import fresh_server, laya_answer, orchestrator_model, settle, text

    project = make_project(ROOT, "via-acp")
    orchestrator_model([text("hi")])
    server, conn = fresh_server(laya_answer("code", {"code": 0.97, "answer": 0.03}))
    sid = (await server.new_session(cwd=str(project))).session_id
    await server.set_session_mode(sid, "plan")
    refused = await server.ext_method("_cordata/orchestrator/start", {"sessionId": sid, "goal": "Implement the calc package so the tests pass."})
    check(not refused["ok"] and "plan mode" in refused["reason"], "start is refused in plan mode")
    await server.set_session_mode(sid, "build")
    direct = await server.ext_method("_cordata/orchestrator/start", {"sessionId": sid, "goal": "Implement the calc package so the tests pass.", "allowDirect": True})
    check(not direct["ok"] and direct.get("needs") == "confirm_direct", "direct network access needs a confirmation, not a flag")
    started = await server.ext_method("_cordata/orchestrator/start", {"sessionId": sid, "goal": "Implement the calc package so the tests pass.", "harden": False})
    check(started["ok"] and started["runId"], f"started ({started.get('runId')})")
    for _ in range(300):
        await settle()
        if any(m == "cordata/orchestrator" and p.get("kind") == "state" and p.get("state") == "done" for m, p in conn.ext):
            break
        await asyncio.sleep(0.1)
    forwarded = [p for m, p in conn.ext if m == "cordata/orchestrator"]
    check(any(p.get("state") == "done" for p in forwarded) and all(p.get("sessionId") == sid for p in forwarded), f"the run's events reach the session ({len(forwarded)})")
    check(not any(p.get("kind") == "state" and p.get("state") == "harden" for p in forwarded), "…honouring the run's options (no HARDEN)")
    runs = await server.ext_method("_cordata/orchestrator/runs", {"sessionId": sid})
    check(any(r["runId"] == started["runId"] for r in runs["runs"]), "the project's runs include it")
    shown = await server.ext_method("_cordata/orchestrator/show", {"sessionId": sid, "runId": started["runId"]})
    check(shown["state"] == "done" and shown["steps"] and shown["usage"], "show has its steps and usage")
    listed = await server.ext_method("_cordata/artifacts/list", {"runId": started["runId"]})
    check(any(a["name"] == "plan.md" for a in listed["artifacts"]), "its artifacts are listed")
    audit = [e["payload"] for e in state_store.recent_events(kind="orchestrator_control", limit=10)]
    check(any(a.get("action") == "start" and a.get("run_id") == started["runId"] for a in audit), "the start is audited")


def test_daemon_endpoints() -> None:
    print("\n=== 9. the daemon serves runs without an agent ===")
    from fastapi.testclient import TestClient

    from daemon.server import create_app

    app = create_app(port=8799)
    client = TestClient(app)
    head = {"x-daemon-token": app.state.daemon.token}
    check(client.get("/orchestrator/runs").status_code == 401, "no token, no runs")
    runs = client.get("/orchestrator/runs", headers=head).json()["runs"]
    check(len(runs) >= 4, f"the daemon lists the runs ({len(runs)})")
    run_id = next(r["runId"] for r in runs if r["live"] == "done")
    shown = client.get(f"/orchestrator/runs/{run_id}", headers=head).json()
    check(shown["state"] == "done" and shown["steps"], "…shows one")
    events = client.get(f"/orchestrator/runs/{run_id}/events", params={"offset": 0, "limit": 5}, headers=head).json()
    check(len(events["events"]) == 5 and events["next"] > 0, "…pages its events")
    refused = client.post(f"/orchestrator/runs/{run_id}/control", json={"cmd": "pause"}, headers=head).json()
    check(not refused["ok"], f"a finished run cannot be paused ({refused})")
    check(client.post(f"/orchestrator/runs/{run_id}/control", json={"cmd": "explode"}, headers=head).status_code == 400, "an unknown command is refused")


async def main() -> None:
    await test_pause_at_a_step()
    await test_pause_between_phases()
    await test_cancel_now()
    await test_cancel_with_restore()
    test_detached_runs()
    test_reads()
    test_daemon_endpoints()
    await test_through_the_server()
    print("\nALL OK")


if __name__ == "__main__":
    asyncio.run(main())

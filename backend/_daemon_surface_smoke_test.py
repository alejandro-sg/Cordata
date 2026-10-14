"""What the daemon serves a native front door (docs/app.md §6.3, phase B3) — hermetic: real Supervisor, real fake
ACP children (``fixtures/fake_acp_bridge_child.py``), FastAPI's TestClient, a temp home.

1. **daemon.json is never half-written** — readers polling it while the daemon rewrites it always parse it;
2. **/projects** — every project with its git repository, branch and worktrees (a worktree says whose), trust, live
   sessions and waiting approvals; **/history** — one project's conversations, paged;
3. **an observer sees the session without driving it** — a second door gets the child's frames (an approval asked
   included), never displaces the driver, and is closed when it writes;
4. **approvals over gRPC carry the offered options** (and the turn's goal and the agent's words, masked) — and an
   answer that names one of them (a plan card's "approve → edit") reaches the child as exactly that option; a made-up
   option is refused.

    backend/.venv/bin/python _daemon_surface_smoke_test.py
"""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

_TMP = tempfile.TemporaryDirectory()
ROOT = Path(_TMP.name).resolve()
os.environ["CORDATA_DAEMON_CONFIG"] = str(ROOT / "daemon.json")
os.environ["CORDATA_HOME"] = str(ROOT / "home")
os.environ.setdefault("CORDATA_STATE_DB", str(ROOT / "home" / "state.db"))
os.environ["CORDATA_ACP_ENTRYPOINT"] = f"{sys.executable} {Path(__file__).parent / 'fixtures' / 'fake_acp_bridge_child.py'}"
os.environ["CORDATA_DISCORD"] = "off"

from fastapi.testclient import TestClient  # noqa: E402

import state_store  # noqa: E402
from daemon import harness_pb2 as pb  # noqa: E402
from daemon import telemetry  # noqa: E402
from daemon.server import create_app, load_or_create_token, token_path  # noqa: E402
from harness_testing import check  # noqa: E402


def git(cwd: Path, *args: str) -> None:
    subprocess.run(["git", "-c", "user.email=t@t", "-c", "user.name=t", *args], cwd=cwd, check=True, capture_output=True)


def test_token_file_is_atomic() -> None:
    print("\n=== 1. daemon.json is never seen half-written ===")
    load_or_create_token(8765)
    bad, reads, stop = [], [0], threading.Event()

    def reader() -> None:
        while not stop.is_set():
            try:
                text = token_path().read_text(encoding="utf-8")
                json.loads(text)
                reads[0] += 1
            except FileNotFoundError:
                bad.append("missing")
            except ValueError:
                bad.append(text)

    thread = threading.Thread(target=reader)
    thread.start()
    for port in range(9000, 9400):
        load_or_create_token(port, grpc_port=port + 1)
    stop.set()
    thread.join()
    check(not bad and reads[0] > 50, f"{reads[0]} reads during 400 rewrites, none torn or missing ({len(bad)})")
    check(oct(token_path().stat().st_mode & 0o777) == "0o600", "still 0600")


def test_projects_and_history(client: TestClient, head: dict) -> None:
    print("\n=== 2. projects (with worktrees) and history ===")
    repo = ROOT / "repo"
    repo.mkdir()
    git(repo, "init", "-q", "-b", "main")
    (repo / "a.txt").write_text("a\n")
    git(repo, "add", "-A")
    git(repo, "commit", "-qm", "init")
    worktree = ROOT / "repo-feature"
    git(repo, "worktree", "add", "-q", "-b", "feature", str(worktree))
    plain = ROOT / "plain"
    plain.mkdir()
    for index in range(7):
        state_store.touch_session(f"repo-{index}", cwd=str(repo), title=f"repo chat {index}")
        state_store.connect().execute("UPDATE sessions SET updated_ts = ? WHERE session_id = ?", (1_800_000_000 + index, f"repo-{index}"))
    state_store.touch_session("wt-1", cwd=str(worktree), title="on the branch")
    state_store.touch_session("plain-1", cwd=str(plain), title="no git")
    state_store.stage_intervention(session_id="repo-3", tool="shell", tool_input={"command": "make"}, intervention_id="repo-3/1")
    settings = ROOT / "home" / "settings.json"
    settings.parent.mkdir(parents=True, exist_ok=True)
    settings.write_text(json.dumps({"trustedProjects": [str(repo)]}))

    rows = {r["root"]: r for r in client.get("/projects", headers=head).json()["projects"]}
    main, wt, flat = rows.get(str(repo)), rows.get(str(worktree)), rows.get(str(plain))
    check(main is not None and wt is not None and flat is not None, f"every project is listed ({len(rows)})")
    check(main["git"]["branch"] == "main" and not main["git"]["isWorktree"] and len(main["git"]["worktrees"]) == 2, f"a repository: its branch and its worktrees ({main['git']})")
    check(wt["git"]["isWorktree"] and wt["git"]["branch"] == "feature" and wt["git"]["mainRoot"] == str(repo), f"a worktree says whose repository it is ({wt['git']})")
    check(flat["git"] is None, "a folder outside git says so")
    check(main["trusted"] and not wt["trusted"], "trust is per folder: the repository is trusted, its worktree is not")
    check(main["conversations"] == 7 and main["pendingApprovals"] == 1, f"conversations and approvals waiting ({main['conversations']}, {main['pendingApprovals']})")

    first = client.get("/history", params={"cwd": str(repo), "limit": 4}, headers=head).json()
    check([s["sessionId"] for s in first["sessions"]] == ["repo-6", "repo-5", "repo-4", "repo-3"] and first["nextCursor"], "history: newest first, a page at a time")
    second = client.get("/history", params={"cwd": str(repo), "limit": 4, "cursor": first["nextCursor"]}, headers=head).json()
    check([s["sessionId"] for s in second["sessions"]] == ["repo-2", "repo-1", "repo-0"] and second["nextCursor"] is None, "…and the cursor reaches the end")
    check(client.get("/history", params={"cwd": "relative"}, headers=head).status_code == 400, "a relative cwd is refused")


def test_observer(client: TestClient, app) -> None:
    print("\n=== 3. an observer watches without driving ===")
    token = app.state.daemon.token
    cwd = ROOT / "observed"
    cwd.mkdir()
    sid = client.post("/sessions", json={"cwd": str(cwd)}, headers={"x-daemon-token": token}).json()["sessionId"]
    with client.websocket_connect(f"/ws?token={token}&session={sid}") as driver:
        with client.websocket_connect(f"/ws?token={token}&session={sid}&role=observer") as observer:
            driver.send_text(json.dumps({"jsonrpc": "2.0", "method": "bridge/permit", "params": {"sessionId": sid, "tool": "shell", "input": {"command": "make deploy"}, "id": 501}}))
            seen = None
            for _ in range(20):
                frame = json.loads(observer.receive_text())
                if frame.get("method") == "session/request_permission":
                    seen = frame
                    break
            check(seen is not None and seen["id"] == 501, "the observer sees the approval the child asked")
            got = None
            for _ in range(20):
                frame = json.loads(driver.receive_text())
                if frame.get("method") == "session/request_permission":
                    got = frame
                    break
            check(got is not None, "…and the driver still gets it (it was not taken from the driver)")
            observer.send_text("{}")
            try:
                observer.receive_text()
                closed = None
            except Exception as error:  # the TestClient raises WebSocketDisconnect with the code
                closed = getattr(error, "code", None)
            check(closed == 4405, f"an observer that writes is closed as read-only ({closed})")
        try:
            with client.websocket_connect(f"/ws?token={token}&session={sid}") as second:
                second.receive_text()
            refused = None
        except Exception as error:
            refused = getattr(error, "code", None)
        check(refused == 4409, f"a second driver is still refused ({refused})")
    try:
        with client.websocket_connect(f"/ws?token={token}&session=no-such&role=observer") as ghost:
            ghost.receive_text()
        missing = None
    except Exception as error:
        missing = getattr(error, "code", None)
    check(missing == 4404, f"observing nothing is refused ({missing})")


async def test_options_over_grpc() -> None:
    print("\n=== 4. approvals over gRPC carry the options, and answers name one ===")
    from daemon.server import State

    state = State(port=0)
    cwd = ROOT / "plan-card"
    cwd.mkdir()
    session = await state.supervisor.spawn(str(cwd))
    sid = session.session_id
    driver = state.supervisor.subscribe(sid)
    options = [
        {"optionId": "approve_build", "name": "Approve → build", "kind": "allow_once"},
        {"optionId": "approve_edit", "name": "Approve → edit", "kind": "allow_once"},
        {"optionId": "keep_planning", "name": "Keep planning", "kind": "reject_once"},
    ]
    meta = {"tool": "present_plan", "goal": "ship it with key sk-ant-abcdefghijklmnopqrstuvwx", "said": "here is the plan", "project": str(cwd)}
    await state.supervisor.write(sid, json.dumps({"jsonrpc": "2.0", "method": "bridge/permit", "params": {"sessionId": sid, "tool": "present_plan", "input": {"plan": "1. do it"}, "meta": meta, "options": options, "id": 88}}))
    for _ in range(50):
        if state.supervisor.all_pending():
            break
        await asyncio.sleep(0.05)
    pending = state.supervisor.all_pending()[0]
    request = telemetry.request_to_proto(pending)
    check([o.option_id for o in request.options] == ["approve_build", "approve_edit", "keep_planning"], "the request carries every option the child offered")
    check(request.goal and "sk-ant-" not in request.goal and request.said == "here is the plan" and request.project == str(cwd), f"…and the turn's goal (masked), the agent's words and the project ({request.goal!r})")
    check(request.tool_call_id == "tc-1", "…and the tool call it belongs to")
    try:
        await state.supervisor.answer_permission(sid, pending.key, True, option_id="approve_auto", decided_by="gui")
        made_up = False
    except ValueError:
        made_up = True
    check(made_up, "an option the child never offered is refused")
    await state.supervisor.answer_permission(sid, pending.key, True, option_id="approve_edit", decided_by="gui")
    await asyncio.sleep(0.3)
    log = [json.loads(line) for line in (cwd / "child.log").read_text().splitlines()]
    answered = [entry for entry in log if entry.get("kind") == "response"]
    chosen = answered[-1]["result"]["outcome"]["optionId"] if answered else None
    check(chosen == "approve_edit", f"the child receives exactly the option chosen ({chosen})")
    decision = pb.InterventionDecision(intervention_id=pending.public_id, approve=True, option_id="approve_edit")
    check(decision.option_id == "approve_edit", "the decision message has the option field")
    state.supervisor.unsubscribe(sid) if hasattr(state.supervisor, "unsubscribe") else None
    del driver
    await state.supervisor.stop_all(grace=1.0)


def test_manage_conversations(client: TestClient, head: dict, app) -> None:
    print("\n=== 5. rename, archive and search conversations ===")
    import session_history  # noqa: PLC0415

    os.environ["CORDATA_SESSIONS_DIR"] = str(ROOT / "sessions")
    chats = app.state.daemon.chats
    project_dir = ROOT / "manage"
    project_dir.mkdir()
    conv = chats.create(cwd=str(project_dir), title="first idea", origin="app")
    conv.acp_id = "acp-manage-1"
    other = chats.create(cwd=str(project_dir), title="unrelated", origin="app")
    other.acp_id = "acp-manage-2"
    snapshot = session_history.snapshot_path("acp-manage-1", ROOT / "sessions")
    snapshot.parent.mkdir(parents=True, exist_ok=True)
    snapshot.write_text(json.dumps({"data": {"messages": [
        {"role": "user", "content": [{"text": "please migrate the billing webhook to v2"}]},
        {"role": "user", "content": [{"toolResult": {"toolUseId": "t", "content": [{"text": "webhook secret sk-ant-api03-" + "x" * 40}]}}]},
        {"role": "assistant", "content": [{"text": "Done: the billing webhook now uses v2."}]},
    ]}}))

    renamed = client.post(f"/conversations/{conv.handle}/rename", json={"title": "  Billing\nwebhook   v2 "}, headers=head)
    check(renamed.status_code == 200 and renamed.json()["title"] == "Billing webhook v2", f"rename: one line, trimmed ({renamed.json()})")
    check(client.post(f"/conversations/{conv.handle}/rename", json={"title": "  "}, headers=head).status_code == 404, "an empty title is refused")
    check(client.post("/conversations/nope/rename", json={"title": "x"}, headers=head).status_code == 404, "an unknown conversation is 404")

    hits = client.get("/conversations/search", params={"q": "billing webhook", "cwd": str(project_dir)}, headers=head).json()["results"]
    check(len(hits) == 1 and hits[0]["handle"] == conv.handle and hits[0]["where"] == "title", f"search finds the title ({hits})")
    words = client.get("/conversations/search", params={"q": "migrate the billing", "cwd": str(project_dir)}, headers=head).json()["results"]
    check(len(words) == 1 and words[0]["where"] == "user" and "migrate the billing" in words[0]["snippet"], f"…and what was said ({words})")
    secret = client.get("/conversations/search", params={"q": "webhook secret", "cwd": str(project_dir)}, headers=head).json()["results"]
    check(secret == [], "tool output is not searched (and so never shown)")
    check(client.get("/conversations/search", params={"q": "b", "cwd": str(project_dir)}, headers=head).json()["results"] == [], "a one-letter query finds nothing (no full scans for noise)")

    archived = client.post(f"/conversations/{conv.handle}/archive", json={"archived": True}, headers=head).json()
    listed = [r["handle"] for r in client.get("/conversations", params={"cwd": str(project_dir)}, headers=head).json()["conversations"]]
    check(archived["archived"] and conv.handle not in listed and other.handle in listed, "archive hides it from the list")
    again = client.get("/conversations/search", params={"q": "billing", "cwd": str(project_dir)}, headers=head).json()["results"]
    check(any(r["handle"] == conv.handle and r["archived"] for r in again), "…but search still finds it, marked archived")
    client.post(f"/conversations/{conv.handle}/archive", json={"archived": False}, headers=head)
    listed = [r["handle"] for r in client.get("/conversations", params={"cwd": str(project_dir)}, headers=head).json()["conversations"]]
    check(conv.handle in listed, "un-archive brings it back")
    check(client.post(f"/conversations/{conv.handle}/rename", json={"title": "x"}).status_code in (401, 403), "the routes need the daemon token")


def main() -> None:
    test_token_file_is_atomic()
    app = create_app(port=8798)
    with TestClient(app) as client:
        head = {"x-daemon-token": app.state.daemon.token}
        test_projects_and_history(client, head)
        test_observer(client, app)
        test_manage_conversations(client, head, app)
    asyncio.run(test_options_over_grpc())
    print("\nALL OK")


if __name__ == "__main__":
    main()

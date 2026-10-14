"""Clearing conversation history: `POST /conversations/clear` deletes it for good, and keeps what is in use.

Through the real daemon app (FastAPI `TestClient`), the real `Supervisor` and `Chats`, and the scripted ACP child
(`fixtures/fake_acp_chat_child.py`). Saved histories are laid out on disk the way `session_store` writes them, and
`state.db` rows the way `state_store.touch_session` does.

1. **it asks first** — without `{"confirm": true}` nothing is touched.
2. **one project** — `cwd` clears that folder's conversations and leaves another folder's alone.
3. **everything** — every conversation goes: the hub's record, its agent, its snapshot and legacy files, its
   `state.db` rows — and a terminal's own session the hub never knew about. One held by a live process, and one
   mid-turn, are kept and named.

    backend/.venv/bin/python _clear_history_smoke_test.py
"""

import functools
import os
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
_ROOT = Path(tempfile.mkdtemp(prefix="cordata-clear-"))
SESSIONS = _ROOT / "sessions"
os.environ["CORDATA_HOME"] = str(_ROOT / "home")
os.environ["CORDATA_DAEMON_CONFIG"] = str(_ROOT / "home" / "daemon.json")
os.environ["CORDATA_STATE_DB"] = str(_ROOT / "home" / "state.db")
os.environ["CORDATA_SESSIONS_DIR"] = str(SESSIONS)
os.environ["CORDATA_DISCORD"] = "off"
os.environ["FAKE_ACP_STATE"] = str(_ROOT / "fake-acp-state.json")
os.environ["CORDATA_ACP_ENTRYPOINT"] = f"{sys.executable} {HERE / 'fixtures' / 'fake_acp_chat_child.py'}"

from fastapi.testclient import TestClient  # noqa: E402

import session_history  # noqa: E402
import state_store  # noqa: E402
from daemon.server import create_app  # noqa: E402


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


class Sink:
    def text(self, delta: str) -> None:
        pass

    def activity(self, label) -> None:  # noqa: ANN001
        pass


def save(session_id: str, cwd: Path, *, legacy: bool = False) -> None:
    """A saved conversation as the backend leaves it: the snapshot (and, for an old one, the legacy layout)."""
    snapshot = session_history.snapshot_path(session_id, SESSIONS)
    snapshot.parent.mkdir(parents=True, exist_ok=True)
    snapshot.write_text('{"messages": []}')
    (snapshot.parent / "immutable_history").mkdir(exist_ok=True)
    if legacy:
        (SESSIONS / f"session_{session_id}" / "agents").mkdir(parents=True, exist_ok=True)
        (SESSIONS / f"session_{session_id}.checkpoint.json").write_text("{}")
    state_store.touch_session(session_id, cwd=str(cwd), title=f"about {session_id}", turn=True)


def on_disk(session_id: str) -> bool:
    return (SESSIONS / "session" / session_id).exists() or (SESSIONS / f"session_{session_id}").exists() or (SESSIONS / f"session_{session_id}.checkpoint.json").exists()


def _wait(predicate, timeout: float = 10.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(0.02)
    return False


def main() -> None:
    project, other = _ROOT / "project", _ROOT / "other"
    project.mkdir()
    other.mkdir()
    app = create_app()
    with TestClient(app) as client:
        daemon = app.state.daemon
        daemon.chats.start()
        headers = {"X-Daemon-Token": daemon.token}
        try:

            def opened(cwd: Path) -> dict:
                row = client.post("/conversations", json={"conversation": "new", "cwd": str(cwd), "origin": "web"}, headers=headers).json()
                client.portal.call(functools.partial(daemon.chats.send, row["handle"], "hello", Sink(), origin="web"))
                row = daemon.chats.resolve(row["handle"]).row()
                save(row["acpSessionId"], cwd, legacy=True)
                return row

            first, second, elsewhere = opened(project), opened(project), opened(other)
            check(all(r["acpSessionId"] and on_disk(r["acpSessionId"]) for r in (first, second, elsewhere)), "three conversations, each with its history saved")
            save("terminal-only", project)  # a terminal's own session: in state.db and on disk, never in the hub
            save("held-elsewhere", project)
            state_store.claim_session("held-elsewhere", cwd=str(project), pid=os.getpid())  # a live process holds it

            # ---- 1. it asks first
            refused = client.post("/conversations/clear", json={}, headers=headers)
            check(refused.status_code == 400 and "confirm" in refused.text, "without confirm: true it refuses, with a sentence")
            check(client.post("/conversations/clear", json={"confirm": True}).status_code == 401, "it needs the daemon token")
            check(daemon.chats.resolve(first["handle"]) is not None and on_disk(first["acpSessionId"]), "…and nothing was touched")

            # ---- 2. one project
            cleared = client.post("/conversations/clear", json={"confirm": True, "cwd": str(other)}, headers=headers).json()
            check(cleared["conversations"] == 1 and cleared["histories"] == 1 and not cleared["kept"], f"cwd clears that folder's conversation ({cleared})")
            check(daemon.chats.resolve(elsewhere["handle"]) is None and not on_disk(elsewhere["acpSessionId"]), "…its record and its saved history are gone")
            check(state_store.get_session(elsewhere["acpSessionId"]) is None, "…and its state.db row")
            check(daemon.chats.resolve(first["handle"]) is not None and on_disk("terminal-only"), "another folder's conversations are untouched")

            # ---- 3. everything, with one mid-turn
            slow = client.portal.start_task_soon(functools.partial(daemon.chats.send, second["handle"], "slow", Sink(), origin="web"))
            check(_wait(lambda: daemon.chats.resolve(second["handle"]).busy), "a turn is running in one of them")
            live_before = daemon.chats.resolve(first["handle"]).daemon_id
            cleared = client.post("/conversations/clear", json={"confirm": True}, headers=headers).json()
            kept = [row["handle"] for row in cleared["kept"]]
            check(kept == [second["handle"]], f"the conversation mid-turn is kept and named ({kept})")
            check(cleared["conversations"] == 1 and daemon.chats.resolve(first["handle"]) is None, "the idle one is forgotten")
            check(bool(live_before) and not any(s.get("session_id") == live_before for s in daemon.supervisor.sessions()), "…and its agent was stopped")
            check(not on_disk(first["acpSessionId"]) and not on_disk("terminal-only"), "its history is deleted, and so is a terminal's session the hub never knew")
            check(cleared["histories"] == 2, f"…counted ({cleared['histories']} histories)")
            check(on_disk("held-elsewhere") and state_store.get_session("held-elsewhere") is not None, "a session a live process holds is left alone")
            check(on_disk(second["acpSessionId"]) and state_store.get_session(second["acpSessionId"]) is not None, "…and so is the running conversation's history")
            check([r["handle"] for r in daemon.chats.rows(None, 50)] == [second["handle"]], "the lists now show only what was kept")
            client.portal.call(daemon.chats.cancel, second["handle"])
            check(slow.result(15).stop == "cancelled", "the kept turn still runs to its end")
            check(not list(SESSIONS.glob("session_*")) or all("held" in p.name or second["acpSessionId"] in p.name for p in SESSIONS.glob("session_*")), "no stray legacy files are left")
        finally:
            client.portal.call(daemon.chats.stop)
            client.portal.call(functools.partial(daemon.supervisor.stop_all, grace=1.0))
    print("\nclear history: all checks passed")


if __name__ == "__main__":
    main()

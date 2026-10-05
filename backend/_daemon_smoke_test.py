"""Regression check for backend/daemon/ — the transport, the auth, and the
reattach path.

Hermetic: the daemon is driven through FastAPI's in-process TestClient, the
"session" is a tiny stand-in child script rather than `acp_server.py` (no model,
no tools, no Qdrant), and the token file and pidfile live in a temp directory.
Nothing here binds a port or spawns a real agent.

What this does *not* prove: that `acp_server.py` speaks correctly through the
daemon. That is what `make daemon` + `make tui ARGS="--transport ws"` is for,
and it is in the plan's end-to-end step rather than here.

    backend/.venv/bin/python _daemon_smoke_test.py
"""

import json
import os
import stat
import tempfile
import time
from pathlib import Path

_TMP = tempfile.TemporaryDirectory()
_ROOT = Path(_TMP.name)
os.environ["SIERRALAB_DAEMON_CONFIG"] = str(_ROOT / "daemon.json")
os.environ["SIERRALAB_DAEMON_PID"] = str(_ROOT / "daemon.pid")
# The daemon state carries an `Observer`, whose settings come from the user
# directory — so without this the test would read the developer's real
# `~/.sierralab/settings.json` and assert against whatever is in it.
os.environ["SIERRALAB_HOME"] = str(_ROOT / "home")

# A stand-in for acp_server.py: newline-delimited JSON in, newline-delimited
# JSON out. It answers `initialize` and echoes anything else, so the test can
# tell "the daemon delivered my bytes" from "the daemon parsed and rebuilt my
# message" — which is the property frames.py exists to hold.
_CHILD = _ROOT / "fake_acp.py"
_CHILD.write_text(
    "import json, sys, time\n"
    "\n"
    "# The daemon's client sees this before it has sent anything, which is the\n"
    "# situation the backlog exists for.\n"
    "print(json.dumps({'jsonrpc': '2.0', 'method': '_sierralab/hello'}), flush=True)\n"
    "\n"
    "for line in sys.stdin:\n"
    "    line = line.rstrip('\\r\\n')\n"
    "    try:\n"
    "        message = json.loads(line)\n"
    "    except ValueError:\n"
    "        message = {}\n"
    "    if message.get('method') == 'big/frame':\n"
    "        # One line far past asyncio's 64 KiB StreamReader default, like a\n"
    "        # file_read result or a diff.\n"
    "        reply = {'jsonrpc': '2.0', 'id': message.get('id'), 'echo': 'x' * 200_000}\n"
    "    elif message.get('method') == 'late/emit':\n"
    "        # Output that arrives after the client that asked has gone.\n"
    "        time.sleep(0.4)\n"
    "        reply = {'jsonrpc': '2.0', 'method': '_sierralab/late'}\n"
    "    elif message.get('method') == 'lab/emit':\n"
    "        # What acp_server.py's Lab sends: a state notification the daemon taps.\n"
    "        args = message.get('params') or {}\n"
    "        reply = {'jsonrpc': '2.0', 'method': '_sierralab/lab', 'params': {'event': 'state', 'version': 1, 'seq': args.get('seq'), 'sessionId': args.get('sessionId'), 'pods': [{'name': 'orchestrator', 'status': args.get('status', 'idle')}], 'links': [], 'threat': None, 'hitl': []}}\n"
    "    elif message.get('method') == 'initialize':\n"
    "        reply = {'jsonrpc': '2.0', 'id': message.get('id'), 'result': {'protocolVersion': 1}}\n"
    "    else:\n"
    "        # The raw line, spacing and all: the test compares this against what\n"
    "        # it sent, so any re-encoding by the daemon would show up here.\n"
    "        reply = {'jsonrpc': '2.0', 'id': message.get('id'), 'echo': line}\n"
    "    print(json.dumps(reply), flush=True)\n"
)

os.environ["SIERRALAB_ACP_ENTRYPOINT"] = f"{os.sys.executable} {_CHILD}"

from fastapi.testclient import TestClient  # noqa: E402  (after the env above)
from starlette.websockets import WebSocketDisconnect  # noqa: E402

from daemon import frames  # noqa: E402
from daemon.main import read_port  # noqa: E402
from daemon.server import create_app  # noqa: E402


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


def wait_for(predicate, timeout: float = 5.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(0.02)
    return False


# ------------------------------------------------------------------ frames


def test_frames() -> None:
    envelope = frames.inspect('{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"sessionId":"s1"}}')
    check(envelope.session_id == "s1", "inspect finds the session id")
    check(envelope.method == "session/new" and envelope.is_request, "inspect finds the method and the request id")

    notification = frames.inspect('{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1"}}')
    check(notification.is_request is False, "a notification is not a request")

    check(frames.inspect("not json").method is None, "an unparsable line is still described, not rejected")
    check(frames.inspect("[1,2,3]").method is None, "a non-object line is handled")

    check(frames.forward_line("{}\n") == b"{}\n", "forward_line appends exactly one newline")
    check(frames.forward_line("{}\r\n") == b"{}\n", "a CRLF client does not corrupt the child's framing")
    check(frames.forward_line("{}") == b"{}\n", "a client that omits the newline is still framed correctly")
    check(frames.outbound_line(b'\n') is None, "a blank line on the wire is not a frame")
    check(frames.outbound_line(b'{"a":1}\n') == '{"a":1}', "outbound_line strips framing whitespace")


# -------------------------------------------------------------- remote taps


def test_remote_taps() -> None:
    """The daemon fans out `task` and `laya` frames without consuming them: a remote
    front door (the Discord bot) listens, and a daemon with nobody listening behaves
    exactly as before. The frame must still reach the backlog either way."""
    from types import SimpleNamespace

    from daemon.supervisor import Session, Supervisor

    supervisor = Supervisor()
    seen: dict[str, list[dict]] = {"task": [], "laya": []}
    stops = [supervisor.listen(topic, seen[topic].append) for topic in seen]
    supervisor._sessions["s1"] = Session(session_id="s1", cwd="/tmp", process=SimpleNamespace(returncode=None, pid=1))

    task = json.dumps({"jsonrpc": "2.0", "method": "_sierralab/task", "params": {"title": "team plan", "state": "running"}})
    laya = json.dumps({"jsonrpc": "2.0", "method": "_sierralab/laya", "params": {"route": "cloud", "risk": 0.9}})
    update = json.dumps({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "agent_message_chunk"}}})
    for line in (task, laya, update):
        supervisor._deliver("s1", line)

    check(seen["task"] == [{"title": "team plan", "state": "running", "session_id": "s1"}], "a task frame reaches the task listener, tagged with its session")
    check(seen["laya"] == [{"route": "cloud", "risk": 0.9, "session_id": "s1"}], "a laya frame reaches the laya listener, tagged with its session")
    check(len(supervisor.backlog("s1")) == 3, "tapping does not consume the frame: all three still reach the backlog")

    # A frame that is not a task/laya update notifies nothing — the taps are narrow.
    seen["task"].clear()
    seen["laya"].clear()
    supervisor._deliver("s1", json.dumps({"jsonrpc": "2.0", "method": "_sierralab/task", "params": "not a dict"}))
    check(not seen["task"] and not seen["laya"], "a malformed task frame notifies nobody instead of raising")

    # A listener that raises must not take the daemon down, and must not stop the
    # frame from reaching a healthy listener or the backlog.
    def angry(_: dict) -> None:
        raise RuntimeError("listener blew up")

    supervisor.listen("laya", angry)
    seen["laya"].clear()
    supervisor._deliver("s1", laya)
    check(seen["laya"] == [{"route": "cloud", "risk": 0.9, "session_id": "s1"}], "a failing listener does not silence the others")

    for stop in stops:
        stop()
    seen["task"].clear()
    supervisor._deliver("s1", task)
    check(not seen["task"], "an unsubscribed listener stops hearing after stop()")


# ------------------------------------------------------------------- token


def test_token_file() -> None:
    from daemon.server import load_or_create_token, token_path

    token = load_or_create_token()
    check(len(token) > 20, "a token is generated on first run")
    check(load_or_create_token() == token, "the token is stable across calls")
    mode = stat.S_IMODE(token_path().stat().st_mode)
    check(mode == 0o600, f"the token file is 0600 (got {oct(mode)})")
    check(read_port() > 0, "the port is readable from the same file")

    # A client finds the daemon by reading this file, so the port in it has to
    # be the port the daemon is on — not the default it happened to start from.
    original = load_or_create_token()
    load_or_create_token(9111)
    check(read_port() == 9111, "the port written is the port the daemon actually bound")
    check(load_or_create_token(9111) == original, "rewriting the port keeps the token")
    # A file left behind with loose permissions is tightened, not trusted.
    os.chmod(token_path(), 0o644)
    load_or_create_token()
    mode = stat.S_IMODE(token_path().stat().st_mode)
    check(mode == 0o600, f"a pre-existing 0644 token file is brought back to 0600 (got {oct(mode)})")
    load_or_create_token()  # leave the file as the other tests expect it


def test_log_redaction() -> None:
    import logging

    from daemon.main import _RedactToken

    record = logging.LogRecord(
        "uvicorn.error", logging.INFO, __file__, 0,
        '%s - "WebSocket %s" [accepted]', ("127.0.0.1:5000", "/ws?cwd=/tmp/p&token=SECRET-abc_123&session=s1"), None,
    )
    _RedactToken().filter(record)
    rendered = record.getMessage()
    check("SECRET-abc_123" not in rendered, f"the token is scrubbed from a logged websocket path (got {rendered})")
    check("cwd=/tmp/p" in rendered and "session=s1" in rendered, "and the rest of the path is left readable")


# -------------------------------------------------------------------- app


def test_daemon() -> None:
    app = create_app()
    # The context manager is load-bearing, not tidiness. Without it `TestClient`
    # runs each request through a throwaway event loop, and the daemon's pump
    # task — `asyncio.create_task` in `Supervisor.spawn`, which is what reads
    # the child's stdout — is cancelled the moment the request returns. The
    # symptom is subtle: the child starts fine, but its greeting never lands and
    # the client instead sees the synthesized `_sierralab/daemon` "exited"
    # frame. Inside the `with`, the loop lives as long as the test, which is
    # also how the real daemon's loop behaves under uvicorn.
    with TestClient(app) as client:
        token = app.state.daemon.token
        project = _ROOT / "project"
        project.mkdir()

        check(client.get("/health").status_code == 401, "health is 401 without a token")
        check(client.get("/health", headers={"X-Daemon-Token": "nope"}).status_code == 401, "health is 401 with a wrong token")

        health = client.get("/health", headers={"X-Daemon-Token": token})
        check(health.status_code == 200 and health.json()["ok"] is True, "health answers with the token")
        check(health.json()["sessions"] == 0, "no sessions yet")
        check(health.json()["observer"]["enabled"] is False, "health reports the observer, off by default")
        # The Discord front door is off unless the environment names a token and an operator, and
        # `/health` has to say which of those it is missing — a bot that started with no allowlist
        # would be a front door anyone could walk through.
        discord = health.json()["discord"]
        check(discord["enabled"] is False and discord["connected"] is False, "health reports the Discord front door, off by default")
        check(discord["allowedUser"] is None and discord["reason"] == "not started", "and reports no operator and no configuration it never read")

        # The webhook route is a *source*, so it must accept an observation
        # without ever implying one was acted on: no observer is running here,
        # and the reply has to say so rather than report success.
        check(client.post("/observe", json={"text": "x"}).status_code == 401, "observe is 401 without a token")
        check(
            client.post("/observe", json={}, headers={"X-Daemon-Token": token}).status_code == 400,
            "observe with no text is a 400",
        )
        check(
            client.post("/observe", json={"text": "   "}, headers={"X-Daemon-Token": token}).status_code == 400,
            "observe with blank text is a 400 too",
        )
        queued = client.post("/observe", json={"text": "the deploy failed"}, headers={"X-Daemon-Token": token})
        check(queued.status_code == 200, "observe queues with the token")
        check(queued.json() == {"queued": 1, "enabled": False}, "and reports queue depth plus whether anything will read it")

        bad_id = client.post("/sessions", json={"cwd": str(project), "sessionId": ["x"]}, headers={"X-Daemon-Token": token})
        check(bad_id.status_code == 400, "a non-string sessionId is a 400, not a server error")
        check(app.state.daemon.supervisor.find_by_cwd("/tmp/a\x00b") is None, "a NUL byte in a cwd is 'not found', not an exception")

        # A directory that does not exist is refused rather than silently
        # spawned into the daemon's own cwd.
        bad = client.post("/sessions", json={"cwd": str(_ROOT / "nope")}, headers={"X-Daemon-Token": token})
        check(bad.status_code == 400, "spawning in a missing directory is a 400")

        opened = client.post("/sessions", json={"cwd": str(project)}, headers={"X-Daemon-Token": token})
        check(opened.status_code == 200, "a session starts in a real directory")
        session_id = opened.json()["sessionId"]
        check(opened.json()["cwd"] == str(project.resolve()), "the child is spawned with cwd set to the project")
        check(
            len(client.get("/sessions", headers={"X-Daemon-Token": token}).json()["sessions"]) == 1,
            "the session is listed",
        )

        supervisor = app.state.daemon.supervisor
        check(wait_for(lambda: supervisor.backlog(session_id)), "the child's greeting is held while nobody is attached")

        # --- auth and origin, before any successful socket. A refusal is a
        # close during the handshake, which TestClient surfaces as a raised
        # WebSocketDisconnect rather than a socket with a close code.
        def refusal(url: str, **kwargs) -> int:
            try:
                with client.websocket_connect(url, **kwargs):
                    return 0
            except WebSocketDisconnect as closed:
                return closed.code

        code = refusal(f"/ws?session={session_id}&token=wrong")
        check(code == 4401, f"a wrong token is refused with 4401 (got {code})")
        code = refusal(f"/ws?session={session_id}&token={token}", headers={"Origin": "http://evil.test"})
        check(code == 4403, f"a browser origin is refused with 4403 (got {code})")
        code = refusal(f"/ws?session=nope&token={token}")
        check(code == 4404, f"an unknown session is refused with 4404 (got {code})")
        code = refusal(f"/ws?token={token}")
        check(code == 4400, f"a missing session is refused with 4400 (got {code})")

        # --- the real thing
        with client.websocket_connect(f"/ws?session={session_id}&token={token}") as socket:
            replayed = json.loads(socket.receive_text())
            check(
                replayed.get("method") == "_sierralab/hello",
                "the backlog the client missed is replayed on attach (what an approval prompt needs)",
            )
            message = '{"jsonrpc":"2.0","id":7,"method":"initialize","params":{"protocolVersion":1,"sessionId":"' + session_id + '"}}'
            socket.send_text(message)
            answer = json.loads(socket.receive_text())
            check(answer.get("result", {}).get("protocolVersion") == 1, "a request reaches the child and its answer returns")

            # Byte-for-byte: the child echoes the raw line it read, so an exact
            # match means the daemon never decoded and rebuilt it.
            socket.send_text('{"jsonrpc":"2.0","method":"x/y","params":{"sessionId":"s","odd":  [1,  2]}}')
            echoed = wait_for_text(socket)
            check(
                echoed is not None and echoed["echo"].endswith('"odd":  [1,  2]}}'),
                f"the frame is forwarded verbatim, spacing included (got {echoed})",
            )

        # A second client cannot take over a session someone may still be driving.
        check(state_has_no_subscribers(app), "the socket's subscription is released on disconnect")

        # --- attaching by directory, which is how a client that never asked
        # the daemon to start a session gets one (and gets its id back).
        with client.websocket_connect(f"/ws?cwd={project}&token={token}") as socket:
            named = json.loads(socket.receive_text())
            check(
                named.get("method") == "_sierralab/session"
                and named["params"]["sessionId"] == session_id,
                f"attaching by directory names the live session there (got {named})",
            )
            # What this client was already handed is not replayed to the next
            # one: a reattaching client would otherwise be shown prompts and
            # answers from before it left. Nothing is pending, so the first
            # frame it sees is the answer to what it asks now.
            socket.send_text('{"jsonrpc":"2.0","id":8,"method":"initialize","params":{"sessionId":"' + session_id + '"}}')
            fresh_answer = json.loads(socket.receive_text())
            check(
                fresh_answer.get("id") == 8,
                f"frames a client already received are not replayed on reattach (got {fresh_answer})",
            )

            # A frame larger than asyncio's default 64 KiB reader limit must not
            # take the session down: `readline` raises past it, which used to end
            # the pump and report a running child as exited.
            socket.send_text('{"jsonrpc":"2.0","id":9,"method":"big/frame","params":{"sessionId":"' + session_id + '"}}')
            big = wait_for_text(socket)
            check(big is not None and len(big["echo"]) == 200_000, "a 200 KB frame is delivered intact")
            check(
                supervisor.sessions()[0]["alive"] is True,
                "and the session is still reported alive afterwards",
            )

        # Output produced while nobody is attached is held for the next client —
        # including a frame that was in flight when the socket dropped.
        with client.websocket_connect(f"/ws?session={session_id}&token={token}") as socket:
            socket.send_text('{"jsonrpc":"2.0","method":"late/emit","params":{"sessionId":"' + session_id + '"}}')
        check(
            wait_for(lambda: any("_sierralab/late" in line for line in supervisor.backlog(session_id))),
            "a frame emitted after the client left is held in the backlog",
        )
        with client.websocket_connect(f"/ws?session={session_id}&token={token}") as socket:
            late = json.loads(socket.receive_text())
            check(late.get("method") == "_sierralab/late", f"and delivered on reattach (got {late})")
            check(supervisor.backlog(session_id) == [], "the backlog is cleared once it has been handed over")

        # A directory with nothing running in it gets a session started, and a
        # second attach to the same directory lands on that same session rather
        # than spawning another child for one project.
        fresh = _ROOT / "other"
        fresh.mkdir()
        with client.websocket_connect(f"/ws?cwd={fresh}&token={token}") as socket:
            opened = json.loads(socket.receive_text())["params"]["sessionId"]
            check(opened != session_id, "an idle directory gets a new session")
        with client.websocket_connect(f"/ws?cwd={fresh}&token={token}") as socket:
            again = json.loads(socket.receive_text())["params"]["sessionId"]
            check(again == opened, "attaching twice to one directory reuses that session")
        check(
            client.delete(f"/sessions/{opened}", headers={"X-Daemon-Token": token}).json()["stopped"] is True,
            "the session started by attaching is stoppable like any other",
        )

        # A directory that does not exist is not a session, it is a 4404.
        code = refusal(f"/ws?cwd={_ROOT / 'nope'}&token={token}")
        check(code == 4404, f"attaching in a missing directory is refused with 4404 (got {code})")

        # The child is stopped with its process group, and the registry forgets it.
        restarted = client.post("/sessions", json={"cwd": str(project)}, headers={"X-Daemon-Token": token})
        check(restarted.status_code == 200, "session restart works after the first exits")
        again = restarted.json()["sessionId"]
        check(again != session_id, "the restarted session is a new one, not the old record")

        check(
            client.delete(f"/sessions/{again}", headers={"X-Daemon-Token": token}).json()["stopped"] is True,
            "a session can be stopped without stopping the daemon",
        )
        check(
            again not in {row["sessionId"] for row in supervisor.sessions()},
            "a stopped session is dropped from the registry",
        )
        check(
            client.delete(f"/sessions/{again}", headers={"X-Daemon-Token": token}).json()["stopped"] is False,
            "stopping a session that is already gone is not an error",
        )


def test_lab_stream() -> None:
    """The daemon taps `_sierralab/lab` frames and re-serves them read-only."""
    app = create_app()
    with TestClient(app) as client:
        daemon = app.state.daemon
        token = daemon.token
        headers = {"X-Daemon-Token": token}
        project = _ROOT / "lab-project"
        project.mkdir()

        check(client.get("/lab").status_code == 401, "GET /lab needs the token")
        check(client.get("/lab", headers=headers).json() == {"sessions": {}, "observer": None}, "no state and no observer pod when nothing has been sent and the observer is off")

        session_id = client.post("/sessions", json={"cwd": str(project)}, headers=headers).json()["sessionId"]

        def refusal(url: str, **kwargs) -> int:
            try:
                with client.websocket_connect(url, **kwargs):
                    return 0
            except WebSocketDisconnect as closed:
                return closed.code

        check(refusal("/lab/ws?token=wrong") == 4401, "the lab socket refuses a wrong token")
        check(refusal(f"/lab/ws?token={token}", headers={"Origin": "http://evil.test"}) == 4403, "…and a browser origin")

        def emit(driver, seq: int, status: str) -> None:
            driver.send_text(json.dumps({"jsonrpc": "2.0", "method": "lab/emit", "params": {"sessionId": session_id, "seq": seq, "status": status}}))

        with client.websocket_connect(f"/ws?session={session_id}&token={token}") as driver:
            check(json.loads(driver.receive_text()).get("method") == "_sierralab/hello", "the driving client attaches")
            with client.websocket_connect(f"/lab/ws?token={token}") as canvas:
                first = json.loads(canvas.receive_text())
                check(first["type"] == "snapshot" and first["sessions"] == {}, "a canvas starts from a snapshot (empty here)")
                emit(driver, 1, "synthesizing")
                seen = json.loads(driver.receive_text())
                check(seen["method"] == "_sierralab/lab" and seen["params"]["seq"] == 1, "the driving client still receives the frame — the tap only reads a copy")
                frame = json.loads(canvas.receive_text())
                check(frame["type"] == "state" and frame["sessionId"] == session_id and frame["state"]["pods"][0]["status"] == "synthesizing", f"the canvas receives the state ({frame['state']['pods']})")
                check(session_id in daemon.supervisor._subscribers and daemon._subscribers == {session_id}, "watching does not displace the driving client")
                canvas.send_text('{"jsonrpc":"2.0","id":99,"method":"lab/isolate","params":{"pod":"orchestrator"}}')
                emit(driver, 2, "idle")
                after = [json.loads(driver.receive_text()) for _ in range(1)]
                check(all("echo" not in m for m in after), "anything a canvas sends is ignored — the socket is read-only, no control reaches the child")
                check(json.loads(canvas.receive_text())["state"]["seq"] == 2, "…and it keeps streaming")
            with client.websocket_connect(f"/lab/ws?token={token}") as late:
                snapshot = json.loads(late.receive_text())
                check(snapshot["sessions"][session_id]["seq"] == 2, "a late canvas is drawn from the newest state, not the next change")
            with client.websocket_connect(f"/lab/ws?token={token}&session=other") as filtered:
                check(json.loads(filtered.receive_text())["sessions"] == {}, "a session filter excludes other sessions")
            check(client.get("/lab", headers=headers).json()["sessions"][session_id]["seq"] == 2, "GET /lab serves the same newest state")
        check(not daemon.supervisor._lab_watchers, "closed canvases are unregistered")

        daemon.observer.enabled = lambda: True  # the observer pod appears once the observer is on
        pod = client.get("/lab", headers=headers).json()["observer"]
        check(pod["name"] == "observer" and pod["status"] == "idle" and pod["scope"] == "daemon", f"the daemon contributes the observer pod ({pod})")
        daemon.observer._cycling = True
        check(client.get("/lab", headers=headers).json()["observer"]["status"] == "synthesizing", "…SYNTHESIZING while a cycle is in flight")
        client.delete(f"/sessions/{session_id}", headers=headers)


def state_has_no_subscribers(app) -> bool:
    return not app.state.daemon._subscribers


def wait_for_text(socket, timeout: float = 5.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        text = socket.receive_text()
        if '"echo"' in text:
            return json.loads(text)
    return None


def main() -> None:
    test_frames()
    test_remote_taps()
    test_token_file()
    test_log_redaction()
    test_daemon()
    test_lab_stream()


if __name__ == "__main__":
    main()
    _TMP.cleanup()
    print("\ndaemon: all checks passed")

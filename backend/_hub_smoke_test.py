"""The conversation hub: one conversation, every front door — the daemon is its only writer.

Through the real daemon app (FastAPI `TestClient`), the real `Supervisor` and `Chats`, and the scripted
ACP child the Discord tests use (`fixtures/fake_acp_chat_child.py`): no model, no network, no Discord.

1. **opened and named** — `POST /conversations` opens a conversation (`new`, `latest` for a folder, or a
   handle) with its agent up and its ACP session id ready to `session/load`; `GET` lists it.
2. **a terminal attaches by handle** — `/ws?conversation=<handle>` relays a terminal into the same child,
   it loads the history, and its own turn is the conversation's turn: other doors queue behind it and
   every watcher hears it (who asked, the reply, the end).
3. **another door's turn reaches the terminal** — a menu-bar turn streams into the attached terminal with
   a `[from menubar] …` line above it, and a terminal prompt sent meanwhile is refused with a reason
   rather than forwarded.
4. **it outlives the daemon** — handles are persisted; a new hub reading the same store resolves them.

    backend/.venv/bin/python _hub_smoke_test.py
"""

import asyncio
import functools
import json
import os
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
_ROOT = Path(tempfile.mkdtemp(prefix="sierralab-hub-"))
os.environ["SIERRALAB_HOME"] = str(_ROOT / "home")
os.environ["SIERRALAB_DAEMON_CONFIG"] = str(_ROOT / "home" / "daemon.json")
os.environ["SIERRALAB_STATE_DB"] = str(_ROOT / "home" / "state.db")
os.environ["SIERRALAB_DISCORD"] = "off"
os.environ["FAKE_ACP_STATE"] = str(_ROOT / "fake-acp-state.json")
os.environ["SIERRALAB_ACP_ENTRYPOINT"] = f"{sys.executable} {HERE / 'fixtures' / 'fake_acp_chat_child.py'}"

from fastapi.testclient import TestClient  # noqa: E402
from starlette.websockets import WebSocketDisconnect  # noqa: E402

import grpc  # noqa: E402

from daemon import harness_pb2 as pb  # noqa: E402
from daemon import harness_pb2_grpc as rpc  # noqa: E402
from daemon.chat import Busy, Chats, chat_store  # noqa: E402
from daemon.grpc_bridge import TOKEN_KEY, GrpcBridge  # noqa: E402
from daemon.server import create_app  # noqa: E402


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


class Sink:
    def __init__(self) -> None:
        self.parts: list[str] = []

    def text(self, delta: str) -> None:
        self.parts.append(delta)

    def activity(self, label) -> None:  # noqa: ANN001
        pass


def frames_until(socket, predicate, limit: int = 60) -> list[dict]:
    """Read frames until one satisfies `predicate` (returned last), or fail after `limit`."""
    seen = []
    for _ in range(limit):
        frame = json.loads(socket.receive_text())
        seen.append(frame)
        if predicate(frame):
            return seen
    raise AssertionError(f"never saw the frame; got {[f.get('method') or f.get('id') for f in seen]}")


def chunk_text(frame: dict, kind: str) -> str:
    update = ((frame.get("params") or {}).get("update") or {})
    if frame.get("method") == "session/update" and update.get("sessionUpdate") == kind:
        return str((update.get("content") or {}).get("text") or "")
    return ""


def main() -> None:
    project = _ROOT / "project"
    project.mkdir()
    app = create_app()
    with TestClient(app) as client:
        daemon = app.state.daemon
        daemon.chats.start()  # what daemon/main.py does with the loop; the app factory does not
        token = daemon.token
        headers = {"X-Daemon-Token": token}

        # ---- 1. opened and named
        opened = client.post("/conversations", json={"conversation": "new", "cwd": str(project), "origin": "terminal"}, headers=headers)
        check(opened.status_code == 200, f"a conversation opens ({opened.status_code} {opened.text[:120]})")
        row = opened.json()
        handle, acp_id = row["handle"], row["acpSessionId"]
        check(handle.startswith("c") and len(handle) == 7 and acp_id and row["live"], f"…with a short handle, its agent up and an ACP session to load ({handle}, {acp_id})")
        check(row["origin"] == "terminal", "…and remembers which door opened it")
        listed = client.get("/conversations", params={"cwd": str(project)}, headers=headers).json()["conversations"]
        check([r["handle"] for r in listed] == [handle], "GET /conversations lists it for that folder")
        latest = client.post("/conversations", json={"conversation": "latest", "cwd": str(project)}, headers=headers).json()
        check(latest["handle"] == handle, "'latest' in that folder is the same conversation — the handoff a terminal makes by default")
        missing = client.post("/conversations", json={"conversation": "cnope00"}, headers=headers)
        check(missing.status_code == 400 and "no conversation" in missing.text, "an unknown handle is refused with a sentence")
        check(client.get("/conversations").status_code == 401, "listing needs the daemon token")

        events: list[dict] = []
        unwatch = daemon.chats.watch(handle, events.append)

        # ---- 2. a terminal attaches by handle and takes a turn
        with client.websocket_connect(f"/ws?conversation={handle}&token={token}") as socket:
            socket.send_text(json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}}))
            frames_until(socket, lambda f: f.get("id") == 1)
            socket.send_text(json.dumps({"jsonrpc": "2.0", "id": 2, "method": "session/load", "params": {"sessionId": acp_id, "cwd": str(project), "mcpServers": []}}))
            loaded = frames_until(socket, lambda f: f.get("id") == 2)
            check("error" not in loaded[-1], f"the terminal loads the conversation's session ({loaded[-1]})")

            socket.send_text(json.dumps({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": acp_id, "prompt": [{"type": "text", "text": "hello from the terminal"}]}}))
            check(_wait(lambda: daemon.chats.resolve(handle).busy), "the terminal's turn makes the conversation busy for every door")
            try:
                client.portal.call(functools.partial(daemon.chats.send, handle, "me too", Sink(), origin="discord"))
                queued = "sent"
            except Busy as error:
                queued = str(error)
            check("from the terminal" in queued, f"a Discord message meanwhile waits its turn, and is told whose turn it is ({queued})")
            turn = frames_until(socket, lambda f: f.get("id") == 3)
            check(turn[-1].get("result", {}).get("stopReason") == "end_turn", "the terminal's turn ends normally")
            check(_wait(lambda: not daemon.chats.resolve(handle).busy), "…and the conversation is free again")
            kinds = [(e["event"], e.get("origin")) for e in events]
            check(("turn", "terminal") in kinds and ("done", "terminal") in kinds, f"watchers hear the terminal's turn begin and end ({kinds[:6]})")
            said = "".join(e.get("delta", "") for e in events if e["event"] == "text")
            check("hello from the terminal" in said, "…and its reply as it streams")
            asked = next(e for e in events if e["event"] == "turn")
            check(asked["text"] == "hello from the terminal", "…and what was asked")

            # ---- 3. another door's turn streams into the terminal
            events.clear()
            pending = client.portal.start_task_soon(functools.partial(daemon.chats.send, handle, "hello from the menu bar", Sink(), origin="menubar"))
            heard: list[str] = []

            def answered(frame: dict) -> bool:
                heard.append(chunk_text(frame, "agent_message_chunk"))  # the reply arrives in pieces
                return "hello from the menu bar" in "".join(heard)

            streamed = frames_until(socket, answered)
            told = [chunk_text(f, "user_message_chunk") for f in streamed if chunk_text(f, "user_message_chunk")]
            check(told and told[0].startswith("[from menubar] hello from the menu bar"), f"the terminal is told what the menu bar asked, above the answer ({told[:1]})")
            result = pending.result(15)
            check(result.stop == "end_turn" and "hello from the menu bar" in result.text, "the menu bar gets its answer")
            check([e["origin"] for e in events if e["event"] == "turn"] == ["menubar"], "watchers hear whose turn it was")

            slow = client.portal.start_task_soon(functools.partial(daemon.chats.send, handle, "slow", Sink(), origin="web"))
            # Started in the child, not just claimed: a cancel that overtook the prompt would be lost.
            frames_until(socket, lambda f: (f.get("params") or {}).get("update", {}).get("sessionUpdate") == "tool_call")
            check(daemon.chats.resolve(handle).busy, "a long turn from the web app is running")
            socket.send_text(json.dumps({"jsonrpc": "2.0", "id": 9, "method": "session/prompt", "params": {"sessionId": acp_id, "prompt": [{"type": "text", "text": "can I?"}]}}))
            refused = frames_until(socket, lambda f: f.get("id") == 9)[-1]
            check("error" in refused and "the web app" in refused["error"]["message"], f"a terminal prompt meanwhile is refused with whose turn it is, not forwarded ({refused.get('error', {}).get('message', '')[:80]})")
            client.portal.call(daemon.chats.cancel, handle)
            check(slow.result(15).stop == "cancelled", "any door can stop the running turn")

            # Only one terminal per conversation: a second would be a second relayed client.
            try:
                with client.websocket_connect(f"/ws?conversation={handle}&token={token}") as second:
                    second.receive_text()
                code = None
            except WebSocketDisconnect as error:
                code = error.code
            check(code == 4409, f"a second terminal on the same conversation is refused, with a reason (got {code})")
        unwatch()

        # ---- 3b. the menu bar's door: gRPC
        other = _ROOT / "other"
        other.mkdir()

        async def via_grpc():
            bridge = GrpcBridge(daemon, port=0)
            port = await bridge.start()
            channel = grpc.aio.insecure_channel(f"127.0.0.1:{port}")
            stub = rpc.HarnessStub(channel)
            meta = ((TOKEN_KEY, daemon.token),)
            try:
                listed = await stub.ListConversations(pb.ConversationsRequest(cwd=str(project)), metadata=meta)
                stream = stub.WatchConversation(pb.ConversationRef(handle=handle), metadata=meta)
                history = []
                first = await stream.read()
                while first.kind == "history":
                    history.append(first)
                    first = await stream.read()
                reply = await stub.SubmitTurn(pb.TurnRequest(conversation=handle, text="hi from the deck", client="harness-menubar"), metadata=meta)
                seen = []
                while True:
                    event = await asyncio.wait_for(stream.read(), 20)
                    seen.append(event)
                    if event.kind == "done":
                        break
                stream.cancel()
                opened = await stub.OpenConversation(pb.ConversationOpen(conversation="new", cwd=str(other), title="from the web", origin="web-app"), metadata=meta)
                fresh = await stub.SubmitTurn(pb.TurnRequest(conversation="new", cwd=str(other), text="a new thread from the deck", client="harness-menubar"), metadata=meta)
                unknown = await stub.SubmitTurn(pb.TurnRequest(conversation="cnope00", text="x", client="harness-menubar"), metadata=meta)
                try:
                    missing = stub.WatchConversation(pb.ConversationRef(handle="cnope00"), metadata=meta)
                    await missing.read()
                    watch_missing = "streamed"
                except grpc.aio.AioRpcError as error:
                    watch_missing = error.code().name
                return listed, first, reply, seen, fresh, unknown, watch_missing, history, opened
            finally:
                await channel.close()
                await bridge.stop(0.5)

        listed, first, reply, seen, fresh, unknown, watch_missing, history, opened = client.portal.call(via_grpc)
        check([c.handle for c in listed.conversations] == [handle], "ListConversations shows the menu bar the same conversation the terminal used")
        check(first.kind == "watching", "WatchConversation says it is live before anything happens")
        asked = [h.text for h in history]
        check("hello from the terminal" in asked and "hello from the menu bar" in asked and all(h.stop for h in history), f"…after replaying the conversation's recent turns, so a door joining late is not blind ({asked})")
        check(any("hello from the terminal" in h.delta for h in history), "…with their replies")
        check(opened.handle and opened.origin == "web" and opened.title == "from the web" and opened.turns == 0, "OpenConversation opens one without starting a turn, named for the web app")
        check(reply.ok and reply.conversation == handle and reply.route == "turn", f"SubmitTurn with a handle runs a real turn in that conversation ({reply.error or reply.route})")
        kinds = [e.kind for e in seen]
        check(kinds[0] == "turn" and seen[0].origin == "menubar" and seen[0].text == "hi from the deck", f"the watcher hears who asked and what ({kinds[:4]})")
        check("hi from the deck" in "".join(e.delta for e in seen if e.kind == "text") and kinds[-1] == "done", "…the reply as it streams, and the end")
        check(fresh.ok and fresh.conversation and fresh.conversation != handle, "conversation=new opens another one, from the deck")
        check(not unknown.ok and "no conversation" in unknown.error, "an unknown handle is refused, not guessed")
        check(watch_missing == "NOT_FOUND", f"watching an unknown handle is NOT_FOUND ({watch_missing})")
        check(_wait(lambda: daemon.chats.resolve(fresh.conversation).turns >= 1), "the deck's new conversation took its turn")

        # ---- 4. it outlives the daemon
        reread = Chats(daemon.supervisor, store=chat_store())
        again = reread.resolve(handle)
        check(again is not None and again.acp_id == acp_id and again.origin == "terminal", "a new hub reading the store resolves the handle to the same ACP session")
        check(again.turns >= 2, f"…with what it has cost so far ({again.turns} turns)")
    print("\nconversation hub: all checks passed")


def _wait(predicate, timeout: float = 10.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(0.02)
    return False


if __name__ == "__main__":
    main()

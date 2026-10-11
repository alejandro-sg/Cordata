"""A scripted ACP agent for the Discord chat tests: speaks the same JSON-RPC the real
`acp_server.py` does for the handful of methods a chat uses, with no model and no network.

What it does, because the chat layer's behaviour depends on each:

* `initialize`, `session/new` (mints `acp-N`), `session/load` (replays two history updates for an id it
  has seen — remembered in `$FAKE_ACP_STATE` so a *respawned* child can restore it — and errors for one
  it has not), `session/set_mode`, `session/cancel` (a notification) and `_sierralab/lab/inject`.
* `session/prompt` is a conversation turn: a `tool_call`, two `agent_message_chunk`s that echo the text,
  a `tool_call_update`, a `_sierralab/turn` receipt, then the response. Prompt text steers it:
    - `ask: <cmd>`      sends `session/request_permission` (allow / always / reject) and waits for it,
    - `ask-armed: <cmd>` the same, flagged armed with no always-allow offered,
    - `slow`            runs until `session/cancel` arrives (stop reason `cancelled`),
    - `die`             exits mid-turn,
    - `long`            replies with ~5000 characters,
    - `table`           replies with a GitHub table (with `<br>` in cells), a rule and a deep heading.
  The mode in force is echoed in the reply, so a test can see `session/set_mode` took effect.
"""

import json
import os
import sys
import threading
import time

STATE = os.environ.get("FAKE_ACP_STATE", "")
lock = threading.Lock()
inbox: list[dict] = []
cancelled = threading.Event()
mode = "build"
sessions: set[str] = set()
injected: list[str] = []
turns_taken: list[str] = []  # the prompts of the turns this child has run (for the conversation methods)
next_id = [900]


def load_state() -> None:
    if STATE and os.path.exists(STATE):
        try:
            sessions.update(json.load(open(STATE)))
        except (OSError, ValueError):
            pass


def save_state() -> None:
    if STATE:
        with open(STATE, "w") as handle:
            json.dump(sorted(sessions), handle)


def send(message: dict) -> None:
    with lock:
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()


def update(session_id: str, body: dict) -> None:
    send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": body}})


def reader() -> None:
    for line in sys.stdin:
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if message.get("method") == "session/cancel":
            cancelled.set()
            continue
        inbox.append(message)


def wait_response(request_id) -> dict:
    while True:
        for index, message in enumerate(list(inbox)):
            if "method" not in message and message.get("id") == request_id:
                inbox.pop(index)
                return message
        if cancelled.is_set():
            return {}
        time.sleep(0.01)


def take_request():
    while True:
        for index, message in enumerate(list(inbox)):
            if "method" in message:
                inbox.pop(index)
                return message
        time.sleep(0.01)


def ask(session_id: str, command: str, armed: bool) -> bool:
    request_id = next_id[0]
    next_id[0] += 1
    options = [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]
    if not armed:
        options.append({"optionId": "allow_always", "name": "Always allow shell(pytest *)", "kind": "allow_always"})
    options.append({"optionId": "reject", "name": "Reject", "kind": "reject_once"})
    send({
        "jsonrpc": "2.0", "id": request_id, "method": "session/request_permission",
        "params": {
            "sessionId": session_id,
            "toolCall": {"toolCallId": f"call-{request_id}", "title": "Run shell", "kind": "execute", "rawInput": {"command": command}},
            "options": options,
            "_meta": {"sierralab": {
                "interrupt_id": str(request_id), "pod": "orchestrator", "tool": "shell", "kind": "shell", "command": command,
                "diff": "", "input_json": json.dumps({"command": command}),
                "risk_score": 0.93 if armed else 0.45, "risk_band": "destructive" if armed else "elevated", "armed": armed,
                "reason": "Laya Guard: removes files — risk 0.93 is at or above this agent's gate of 0.80" if armed else "",
                "goal": "clean the build directory", "said": "I'll remove the stale build output first.",
                "project": os.getcwd(), "redactions": 0,
            }},
        },
    })
    answer = wait_response(request_id)
    outcome = ((answer.get("result") or {}).get("outcome") or {})
    return outcome.get("outcome") == "selected" and outcome.get("optionId") in ("allow", "allow_always")


def prompt(request: dict) -> None:
    params = request["params"]
    session_id = params["sessionId"]
    text = " ".join(b.get("text", "") for b in params.get("prompt") or [])
    turns_taken.append(text)
    cancelled.clear()
    started = time.time()
    update(session_id, {"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Reading the project", "kind": "read", "status": "in_progress"})
    reply = f"You said: {text}. (mode: {mode}"
    if injected:
        reply += f", notes: {' | '.join(injected)}"
        injected.clear()
    reply += ")"
    if text.startswith(("ask:", "ask-armed:")):
        armed = text.startswith("ask-armed:")
        command = text.split(":", 1)[1].strip()
        allowed = ask(session_id, command, armed)
        reply = f"{'Allowed' if allowed else 'Denied'}: {command}"
    elif text == "slow":
        while not cancelled.is_set():
            time.sleep(0.02)
        send({"jsonrpc": "2.0", "method": "_sierralab/turn", "params": {"sessionId": session_id, "stop": "cancelled", "seconds": 0.5, "tokensIn": 10, "tokensOut": 0, "calls": 1}})
        send({"jsonrpc": "2.0", "id": request["id"], "result": {"stopReason": "cancelled"}})
        return
    elif text == "die":
        os._exit(3)
    elif text == "table":
        # What a model actually writes: a GitHub table with <br> and bullets inside cells, which Discord shows as pipes.
        reply = (
            "Here is what I can do:\n\n| Category | What I can do |\n|----------|---------------|\n"
            "| **Code & files** | • Edit files (`editor`).<br>• Read files (`file_read`). |\n"
            "| **Shell** | • Run any shell command. |\n\n---\n\n#### Notes\nLine one<br>line two"
        )
    elif text == "long":
        reply = "para. " * 200 + "\n\n```py\n" + "x = 1\n" * 150 + "```\n\n" + "tail. " * 100
    update(session_id, {"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed"})
    half = max(1, len(reply) // 2)
    update(session_id, {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": reply[:half]}})
    time.sleep(0.05)
    update(session_id, {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": reply[half:]}})
    send({"jsonrpc": "2.0", "method": "_sierralab/turn", "params": {"sessionId": session_id, "stop": "end_turn", "seconds": round(time.time() - started, 1), "tokensIn": 1200, "tokensOut": 300, "calls": 1}})
    send({"jsonrpc": "2.0", "id": request["id"], "result": {"stopReason": "end_turn"}})


def main() -> None:
    global mode
    load_state()
    threading.Thread(target=reader, daemon=True).start()
    counter = len(sessions)
    while True:
        message = take_request()
        method, request_id, params = message.get("method"), message.get("id"), message.get("params") or {}
        if method == "initialize":
            # `loadSession` as the real server advertises it: a terminal joining a hub conversation loads it.
            send({"jsonrpc": "2.0", "id": request_id, "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": True}}})
        elif method == "session/new":
            counter += 1
            sid = f"acp-{os.getpid()}-{counter}"
            sessions.add(sid)
            save_state()
            send({"jsonrpc": "2.0", "id": request_id, "result": {"sessionId": sid, "modes": {"currentModeId": mode}}})
        elif method == "session/load":
            sid = params.get("sessionId")
            if sid in sessions:
                update(sid, {"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "earlier question"}})
                update(sid, {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "earlier answer — must not be re-posted"}})
                send({"jsonrpc": "2.0", "id": request_id, "result": {}})
            else:
                send({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32602, "message": "no such session", "data": {"error": f"no session {sid}"}}})
        elif method == "session/set_mode":
            mode = params.get("modeId", mode)
            send({"jsonrpc": "2.0", "id": request_id, "result": {}})
        elif method == "_sierralab/conversation/list":
            send({"jsonrpc": "2.0", "id": request_id, "result": {"enabled": True, "turns": len(turns_taken), "checkpoints": [
                {"index": n, "id": f"id{n}", "turn": t, "prompt": p, "kind": "turn", "at": 0, "messages": 2 * (t - 1)} for n, (t, p) in enumerate(reversed(list(enumerate(turns_taken, 1))), 1)]}})
        elif method == "_sierralab/conversation/rewind":
            count = int(params.get("turns") or 1)
            if not turns_taken:
                send({"jsonrpc": "2.0", "id": request_id, "result": {"ok": False, "reason": "nothing to go back to — no turn has been taken in this session"}})
            else:
                forgot = turns_taken[-count:]
                del turns_taken[-count:]
                send({"jsonrpc": "2.0", "id": request_id, "result": {"ok": True, "now": len(turns_taken), "forgot": forgot, "turn": len(turns_taken) + 1, "to": "id1", "undo_with": "id9"}})
        elif method == "_sierralab/lab/inject":
            injected.append(params.get("text", ""))
            send({"jsonrpc": "2.0", "id": request_id, "result": {"pod": params.get("pod"), "queued": len(injected)}})
        elif method == "session/prompt":
            # On its own thread, like the real agent: while a turn is parked on an approval or running
            # long, the connection still answers `session/set_mode` and `_sierralab/lab/inject`.
            threading.Thread(target=prompt, args=(message,), daemon=True).start()
        elif request_id is not None:
            send({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": f"unknown method {method}"}})


if __name__ == "__main__":
    main()

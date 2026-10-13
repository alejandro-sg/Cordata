"""A stand-in for ``acp_server.py`` that behaves the way the daemon's gRPC bridge needs to see it.

Newline-delimited JSON on stdin/stdout. It speaks just enough of the ACP surface the bridge
touches, and writes down everything it *receives* to ``child.log`` (one JSON object per line) in
its working directory, so a test can assert what actually reached the child:

  command (a notification the test sends)   what the child does
  ------------------------------------------------------------------------------------
  bridge/lab      {seq,status,count,delay_ms}  emits ``count`` Lab state frames, seq..seq+count-1,
                                            ``delay_ms`` apart (default: back to back)
  bridge/permit   {tool,input,meta,id,options} emits a ``session/request_permission`` request and
                                            then waits (logging whatever answers it)
  bridge/diff     {id,lines}                emits a ``_cordata/diff`` event
  bridge/memory   {event}                   emits a ``_cordata/memory`` event
  _cordata/lab/effort | dial | isolate    answers the request (an error for value "bogus")
  _cordata/lab/inject | release          answers (inject refuses empty text, and the pod 'stopped')
  _cordata/os-context                    answers {ok: true} — what the menu bar registers before a turn
  session/set_mode {modeId}                 answers {} for plan | build | auto, an error otherwise

Everything is also logged as received, including any response to its permission request.
"""

import json
import sys
import time

LOG = open("child.log", "a", buffering=1)


def emit(obj):
    print(json.dumps(obj), flush=True)


def log(kind, payload):
    LOG.write(json.dumps({"kind": kind, **payload}) + "\n")


emit({"jsonrpc": "2.0", "method": "_cordata/hello"})

for line in sys.stdin:
    line = line.rstrip("\r\n")
    try:
        message = json.loads(line)
    except ValueError:
        continue
    method = message.get("method")
    params = message.get("params") or {}
    if method is None and "id" in message:
        log("response", {"id": message["id"], "result": message.get("result")})
        outcome = ((message.get("result") or {}).get("outcome") or {})
        emit({"jsonrpc": "2.0", "method": "_cordata/answered", "params": {"id": message["id"], "optionId": outcome.get("optionId")}})
        continue
    log("received", {"method": method, "id": message.get("id"), "params": params})
    if method == "bridge/lab":
        base, count = int(params.get("seq", 1)), int(params.get("count", 1))
        for i in range(count):
            if params.get("delay_ms"):
                time.sleep(params["delay_ms"] / 1000)
            emit(
                {
                    "jsonrpc": "2.0",
                    "method": "_cordata/lab",
                    "params": {
                        "event": "state",
                        "version": 1,
                        "seq": base + i,
                        "sessionId": params.get("sessionId"),
                        "focus": None,
                        "pods": [
                            {
                                "id": "orchestrator",
                                "name": "orchestrator",
                                "role": "orchestrator",
                                "parent": None,
                                "model": "glm-5.3-flash",
                                "effort": "low",
                                "status": params.get("status", "idle"),
                                "isolated": False,
                                "dial": 0.8,
                                "dial_default": 0.8,
                                "calls": base + i,
                                "busy_ms": 5,
                                "pending_tool": None,
                                "guidance_pending": 0,
                                "stream": "tick %d" % (base + i),
                                "trace": [{"tool": "shell", "status": "completed", "input": "ls", "ms": 3}],
                                "effort_adjustable": True,
                                "memory": params.get("memory", []),
                            }
                        ],
                        "links": [{"from": "orchestrator", "to": "debugger", "kind": "delegation", "active": True, "count": 1, "label": "x"}],
                        "threat": ({"pod": "orchestrator", "tool": "shell", "score": params["score"], "confidence": 0.9, "band": "destructive", "pending": True} if "score" in params else {"pod": "orchestrator", "tool": "shell", "score": None, "confidence": None, "band": None, "pending": True}),
                        "hitl": [],
                        "mode": params.get("mode", "build"),
                        "mission": params.get("mission", ""),
                        "mission_ts": 1700000000.0 if params.get("mission") else 0,
                        "resources": {"cpu_percent": 1.5, "rss_mb": 100.0, "scope": "process", "mcp": ["pihole"]},
                    },
                }
            )
    elif method == "bridge/permit":
        emit(
            {
                "jsonrpc": "2.0",
                "id": params.get("id", 77),
                "method": "session/request_permission",
                "params": {
                    "sessionId": params.get("sessionId"),
                    "toolCall": {"toolCallId": "tc-1", "title": "Run " + params.get("tool", "shell"), "kind": params.get("kind", "execute"), "rawInput": params.get("input")},
                    "options": params.get("options") or [
                        {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                        {"optionId": "allow_always", "name": "Always allow", "kind": "allow_always"},
                        {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
                    ],
                    **({"_meta": {"cordata": params["meta"]}} if params.get("meta") else {}),
                },
            }
        )
    elif method == "bridge/diff":
        body = "".join(f"+line {n}\n" for n in range(int(params.get("lines", 3))))
        emit({"jsonrpc": "2.0", "method": "_cordata/diff", "params": {"id": params.get("id", "d1"), "source": "claude_code_engineer", "files": ["a.py"], "unified": "--- /dev/null\n+++ b/a.py\n@@ -0,0 +1 @@\n" + body, "truncated": False, "stat": " a.py | 1 +", "lines": int(params.get("lines", 3)) + 3}})
    elif method == "bridge/memory":
        emit({"jsonrpc": "2.0", "method": "_cordata/memory", "params": {"event": params.get("event", "unload"), "runtime": "omlx", "model": "m"}})
    elif method == "session/set_mode":
        if params.get("modeId") in ("plan", "build", "auto"):
            emit({"jsonrpc": "2.0", "id": message["id"], "result": {}})
        else:
            emit({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32602, "message": "Invalid params", "data": {"error": "unknown mode"}}})
    elif method == "_cordata/lab/inject":
        if not (params.get("text") or "").strip():
            emit({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32602, "message": "Invalid params", "data": {"error": "guidance is empty"}}})
        elif params.get("pod") == "stopped":
            emit({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32602, "message": "Invalid params", "data": {"error": "stopped is isolated; release it first"}}})
        else:
            emit({"jsonrpc": "2.0", "id": message["id"], "result": {"pod": params.get("pod"), "queued": 1}})
    elif method == "_cordata/os-context":
        emit({"jsonrpc": "2.0", "id": message["id"], "result": {"ok": True, "app": params.get("app_name", ""), "denied": False}})
    elif method == "_cordata/lab/release":
        emit({"jsonrpc": "2.0", "id": message["id"], "result": {"pod": params.get("pod"), "isolated": False}})
    elif method in ("_cordata/lab/effort", "_cordata/lab/dial", "_cordata/lab/isolate"):
        if params.get("value") == "bogus":
            emit({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32602, "message": "Invalid params", "data": {"error": "orchestrator has no reasoning_effort knob"}}})
        else:
            emit({"jsonrpc": "2.0", "id": message["id"], "result": {"pod": params.get("pod"), "applied": params.get("value")}})

"""A scripted stand-in for ``acp_server.py`` that plays a believable session for the Lab GUI.

Used by ``lab_demo.py`` (``make lab-demo``) so every part of the Experimental Lab can be tried
without a model, a key or a token spent: pods that work and idle, a Laya reading that drifts, an
approval that suspends the "agent" until a human answers, a 10,000-line diff, and knobs that
really change the pod you are looking at.

Newline-delimited JSON on stdin/stdout, like the real child. ``demo/start {sessionId}`` starts the
scenario. Everything it *receives* is also written to ``child.log`` in its working directory.

The scenario, then a quiet ~2 minutes, then again (each permit waits for you as long as you like;
the destructive one pops the modal, the elevated one waits in the tray):
  1. the orchestrator works, delegates to the architect, dispatches to the Claude Code engineer
  2. a destructive shell command is held at the gate (Laya 0.93, armed) — approve, reject or isolate
  3. a file-diff edit is held (Laya elevated)
  4. the engineer finishes and a 10,000-line diff is published
"""

import json
import random
import sys
import threading
import time

LOG = open("child.log", "a", buffering=1)
OUT_LOCK = threading.Lock()
STATE_LOCK = threading.RLock()


def emit(obj):
    line = json.dumps(obj)
    with OUT_LOCK:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()


def log(kind, payload):
    LOG.write(json.dumps({"kind": kind, **payload}) + "\n")


EFFORTS = ("low", "medium", "high", "max")
DIAL_MIN, DIAL_DEFAULT = 0.3, 0.8


def new_pod(pid, role, model, adjustable, parent=None):
    return {
        "since": time.time(),
        "guidance": [],
        "risk_last": None,
        "risk_peak": 0.0,
        "auto_approved": 0,
        "asked": 0,
        "id": pid,
        "name": pid,
        "role": role,
        "parent": parent,
        "model": model,
        "effort": "low" if adjustable else "",
        "status": "idle",
        "isolated": False,
        "dial": DIAL_DEFAULT,
        "dial_default": DIAL_DEFAULT,
        "calls": 0,
        "busy_ms": 0,
        "pending_tool": None,
        "guidance_pending": 0,
        "stream": "",
        "trace": [],
        "effort_adjustable": adjustable,
    }


PODS = {
    "orchestrator": new_pod("orchestrator", "orchestrator", "glm-5.3-flash:cloud", True),
    "architect": new_pod("architect", "architect", "prism-ml/Ternary-Bonsai-2-27B-mlx-2bit", False, "orchestrator"),
    "debugger": new_pod("debugger", "debugger", "glm-5.3-flash:cloud", True, "orchestrator"),
    "claude_code_engineer": new_pod("claude_code_engineer", "claude_code", "claude-code-cli", False, "orchestrator"),
}
LINKS = [
    {"from": "orchestrator", "to": "architect", "kind": "delegation", "active": False, "count": 0, "label": "plan"},
    {"from": "orchestrator", "to": "debugger", "kind": "delegation", "active": False, "count": 0, "label": "triage"},
    {"from": "orchestrator", "to": "claude_code_engineer", "kind": "dispatch", "active": False, "count": 0, "label": "refactor"},
]
THREAT = {"pod": "orchestrator", "tool": "shell", "score": None, "confidence": None, "band": None, "pending": False}
FOCUS = {"id": "orchestrator"}
SESSION = {"id": None, "seq": 0}
MODE = {"value": "build"}
MISSION = {"text": "Refactor the supervisor so the session lookup stops copying the table", "ts": time.time()}
MODES = ("plan", "build", "auto")


def snapshot():
    with STATE_LOCK:
        SESSION["seq"] += 1
        return {
            "event": "state",
            "version": 1,
            "seq": SESSION["seq"],
            "sessionId": SESSION["id"],
            "focus": FOCUS["id"],
            "pods": [dict(p, trace=list(p["trace"][-12:])) for p in PODS.values()],
            "links": [dict(link) for link in LINKS],
            "threat": dict(THREAT),
            "hitl": [],
            "mode": MODE["value"],
            "mission": MISSION["text"],
            "mission_ts": MISSION["ts"],
            "resources": {"cpu_percent": round(random.uniform(2, 9), 1), "rss_mb": round(random.uniform(180, 260), 1), "scope": "process", "mcp": ["pihole"]},
        }


def publish():
    emit({"jsonrpc": "2.0", "method": "_sierralab/lab", "params": snapshot()})


def say(pod, text):
    with STATE_LOCK:
        PODS[pod]["stream"] = (PODS[pod]["stream"] + " " + text)[-400:]


def start_tool(pod, tool, detail, status="executing"):
    with STATE_LOCK:
        p = PODS[pod]
        if p["isolated"]:
            return False
        if p["guidance"]:
            # A directive is read before the agent's next model call, as the real Lab does.
            taken, p["guidance"], p["guidance_pending"] = p["guidance"], [], 0
            p["trace"].append({"tool": "GUIDANCE", "status": "completed", "input": " / ".join(taken)[:160], "ms": 0, "ts": time.time()})
            p["stream"] = ("Directive received: " + " / ".join(taken))[-400:]
        if p["status"] != status:
            p["status"], p["since"] = status, time.time()
        p["pending_tool"] = f"{tool}: {detail}"
        p["calls"] += 1
        p["trace"].append({"tool": tool, "status": "running", "input": detail, "ms": 0, "ts": time.time()})
    return True


def end_tool(pod, ms, failed=False):
    with STATE_LOCK:
        p = PODS[pod]
        p["pending_tool"] = None
        if p["status"] != "idle":
            p["status"], p["since"] = "idle", time.time()
        p["busy_ms"] += ms
        if p["trace"]:
            p["trace"][-1]["status"] = "failed" if failed else "completed"
            p["trace"][-1]["ms"] = ms


def guard(pod, score, decision):
    """What Laya Guard would record for one call: the agent's own counters and the gauge."""
    with STATE_LOCK:
        p = PODS[pod]
        p["risk_last"] = score
        p["risk_peak"] = max(p["risk_peak"], score)
        if decision == "auto":
            p["auto_approved"] += 1
        elif decision == "asked":
            p["asked"] += 1


def link(frm, to, active):
    with STATE_LOCK:
        for entry in LINKS:
            if entry["from"] == frm and entry["to"] == to:
                entry["active"] = active
                if active:
                    entry["count"] += 1


# ----------------------------------------------------------------- permission


PERMITS = {}  # rpc id -> event the scenario thread waits on
ANSWERS = {}


def secret_shaped(prefix, fill):
    # Assembled at runtime so no token-shaped literal sits in the source for a scanner to flag.
    return prefix + fill * 4


def permit(rpc_id, pod, tool, kind, title_input, meta):
    event = threading.Event()
    PERMITS[rpc_id] = event
    with STATE_LOCK:
        PODS[pod]["status"] = "contained"
        PODS[pod]["pending_tool"] = f"{tool}: awaiting a human"
        THREAT.update({"pod": pod, "tool": tool, "score": meta["risk_score"], "confidence": meta["risk_confidence"], "band": meta["risk_band"], "pending": True})
    publish()
    emit(
        {
            "jsonrpc": "2.0",
            "id": rpc_id,
            "method": "session/request_permission",
            "params": {
                "sessionId": SESSION["id"],
                "toolCall": {"toolCallId": f"tc-{rpc_id}", "title": f"Run {tool}", "kind": kind, "rawInput": title_input},
                "options": [
                    {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                    {"optionId": "allow_always", "name": "Always allow", "kind": "allow_always"},
                    {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
                ],
                "_meta": {"sierralab": {"pod": pod, "tool": tool, **meta}},
            },
        }
    )
    event.wait()
    answer = ANSWERS.pop(rpc_id, "reject")
    approved = answer.startswith("allow")
    with STATE_LOCK:
        THREAT["pending"] = False
        PODS[pod]["pending_tool"] = None
        PODS[pod]["status"] = "idle" if not PODS[pod]["isolated"] else "isolated"
        PODS[pod]["trace"].append({"tool": tool, "status": "completed" if approved else "failed", "input": "approved by a human" if approved else "rejected by a human", "ms": 1})
    say(pod, ("Approved - running it. " if approved else "Rejected - leaving it alone. "))
    publish()
    return approved


def diff_event(diff_id, lines):
    body = ["--- a/backend/daemon/supervisor.py", "+++ b/backend/daemon/supervisor.py"]
    n = 1
    while len(body) < lines:
        body.append(f"@@ -{n},3 +{n},4 @@ def session_lookup_{n // 7}(self, key):")
        body.append("     table = self._sessions")
        body.append(f"-    return {{k: v for k, v in table.items() if k == key}}  # copies the whole table ({n})")
        body.append(f"+    return table.get(key)  # O(1): no copy ({n})")
        n += 4
    body = body[:lines]
    emit(
        {
            "jsonrpc": "2.0",
            "method": "_sierralab/diff",
            "params": {
                "id": diff_id,
                "source": "claude_code_engineer",
                "files": ["backend/daemon/supervisor.py", "backend/daemon/server.py", "backend/daemon/telemetry.py"],
                "unified": "\n".join(body) + "\n",
                "truncated": False,
                "stat": " 3 files changed",
                "lines": len(body),
            },
        }
    )


# ------------------------------------------------------------------- scenario


def sleep(seconds):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        time.sleep(0.1)


def routine(pod, tool, detail, seconds, score, said, would, status="executing"):
    """One ordinary call. Laya Guard scores it; build lets it run (it is not one that asks), auto says
    it let it run, and plan runs nothing at all."""
    if MODE["value"] == "plan":
        say(pod, f"Plan: I would {would}, but plan mode runs nothing.")
        publish()
        sleep(1.2)
        return
    if start_tool(pod, tool, detail, status):
        say(pod, said)
        guard(pod, score, "auto" if MODE["value"] == "auto" else "allowed")
        THREAT.update({"pod": pod, "tool": tool, "score": score, "confidence": 0.9, "band": "safe" if score < 0.5 else "elevated", "pending": False})
    publish()
    sleep(seconds)
    end_tool(pod, int(seconds * 1000))
    publish()


def scenario():
    round_no = 0
    while True:
        round_no += 1
        # A pod isolated in the last round is released, so the demo can be run again.
        with STATE_LOCK:
            for p in PODS.values():
                if p["isolated"]:
                    p["isolated"], p["status"] = False, "idle"
        # 1. the orchestrator plans, the architect answers, the engineer works. In plan mode nothing runs.
        FOCUS["id"] = "orchestrator"
        routine("orchestrator", "read_file", "backend/daemon/supervisor.py", 2.5, 0.05, "Reading the supervisor to see where the session table is copied.", "read the supervisor", status="synthesizing")
        link("orchestrator", "architect", True)
        FOCUS["id"] = "architect"
        routine("architect", "plan", "split the lookup from the table", 3.5, 0.05, "Plan: return the live entry, keep the table private, add a regression test.", "plan the change", status="synthesizing")
        link("orchestrator", "architect", False)
        link("orchestrator", "claude_code_engineer", True)
        FOCUS["id"] = "claude_code_engineer"
        routine("claude_code_engineer", "Edit", "backend/daemon/supervisor.py", 3.0, 0.35, "Editing the lookup; running the daemon smoke test next.", "edit the lookup")
        link("orchestrator", "claude_code_engineer", False)

        # 2. a destructive command is held at the gate — in every mode but plan, and in auto too
        FOCUS["id"] = "debugger"
        token = secret_shaped("ghp_", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8")
        command = f"rm -rf build dist .pytest_cache && git clean -fdx && curl -sS -H 'Authorization: Bearer {token}' https://api.example.invalid/purge"
        if MODE["value"] == "plan":
            say("debugger", "Plan: I would clear the build outputs, but plan mode runs nothing.")
            publish()
            sleep(1.5)
        else:
            start_tool("debugger", "shell", "rm -rf build dist …", "contained")
            guard("debugger", 0.93, "asked")
            approved = permit(
                1000 + round_no * 2,
                "debugger",
                "shell",
                "execute",
                {"command": command},
                {
                    "kind": "shell",
                    "command": command,
                    "risk_score": 0.93,
                    "risk_confidence": 0.88,
                    "risk_band": "destructive",
                    "armed": True,
                    "reason": "Laya Guard: git clean -fdx deletes every untracked file, ignored ones included — risk 0.93 is at or above this agent's gate of 0.80",
                },
            )
            end_tool("debugger", 900, failed=not approved)
            publish()
            sleep(2.0)

        # 3. a file edit that is elevated but under the gate: build asks, auto runs it and says so
        FOCUS["id"] = "claude_code_engineer"
        diff = "\n".join(
            ["--- a/backend/daemon/supervisor.py", "+++ b/backend/daemon/supervisor.py", "@@ -212,7 +212,7 @@ class Supervisor:", "     def lab_states(self):", "-        return {sid: s.lab for sid, s in self._sessions.items() if s.lab is not None}", "+        return {sid: s.lab for sid, s in list(self._sessions.items()) if s.lab}", "     def _tap_lab(self, session_id, line):"]
        )
        if MODE["value"] == "plan":
            say("claude_code_engineer", "Plan: I would patch lab_states() to iterate a snapshot of the sessions.")
            publish()
        elif MODE["value"] == "auto":
            routine("claude_code_engineer", "apply_patch", "backend/daemon/supervisor.py", 1.5, 0.58, "Patching lab_states() to iterate a snapshot.", "patch lab_states")
        else:
            start_tool("claude_code_engineer", "apply_patch", "backend/daemon/supervisor.py", "contained")
            guard("claude_code_engineer", 0.58, "asked")
            permit(
                1001 + round_no * 2,
                "claude_code_engineer",
                "apply_patch",
                "edit",
                {"path": "backend/daemon/supervisor.py"},
                {"kind": "diff", "diff": diff, "risk_score": 0.58, "risk_confidence": 0.7, "risk_band": "elevated", "armed": False, "reason": "edits a file the daemon itself runs"},
            )
            end_tool("claude_code_engineer", 700)
        publish()

        # 4. the engineer's finished work
        diff_event(f"demo-{round_no}", 10_000)
        emit({"jsonrpc": "2.0", "method": "_sierralab/memory", "params": {"event": "heavy_task", "runtime": "claude_code", "model": "claude-code-cli"}})
        FOCUS["id"] = "orchestrator"
        THREAT.update({"score": 0.12, "confidence": 0.9, "band": "safe", "pod": "orchestrator", "tool": "summary"})
        publish()
        burst()
        sleep(100)  # one cycle at start, then a quiet couple of minutes: a demo that nags is not a demo


def burst(n=1000, gap=0.0012):
    """A thousand state updates back to back: the Lab's stress gate, played on the real wire."""
    with STATE_LOCK:
        PODS["orchestrator"]["status"] = "executing"
        base = PODS["orchestrator"]["calls"]
    for k in range(1, n + 1):
        with STATE_LOCK:
            PODS["orchestrator"]["calls"] = base + k
            PODS["orchestrator"]["stream"] = f"burst {k}/{n}"
        publish()
        time.sleep(gap)
    with STATE_LOCK:
        PODS["orchestrator"]["status"] = "idle"
    publish()


# --------------------------------------------------------------------- inputs


def handle_control(message, method, params):
    pod = PODS.get(params.get("pod"))
    ident = message.get("id")

    def refuse(text):
        emit({"jsonrpc": "2.0", "id": ident, "error": {"code": -32602, "message": "Invalid params", "data": {"error": text}}})

    if pod is None:
        return refuse(f"no pod named {params.get('pod')!r}")
    with STATE_LOCK:
        if method == "_sierralab/lab/effort":
            if not pod["effort_adjustable"]:
                return refuse(f"{pod['name']} has no reasoning_effort knob")
            if params.get("value") not in EFFORTS:
                return refuse("reasoning_effort must be low, medium, high or max")
            pod["effort"] = params["value"]
            applied = pod["effort"]
        elif method == "_sierralab/lab/dial":
            value = float(params.get("value"))
            pod["dial"] = round(min(DIAL_DEFAULT, max(DIAL_MIN, value)), 2)
            applied = pod["dial"]
        elif method == "_sierralab/lab/inject":
            text = str(params.get("text") or "").strip()
            if not text:
                return refuse("guidance is empty")
            if pod["isolated"]:
                return refuse(f"{pod['name']} is isolated; release it first")
            pod["guidance"].append(text[:2000])
            pod["guidance_pending"] = len(pod["guidance"])
            emit({"jsonrpc": "2.0", "id": ident, "result": {"pod": pod["id"], "queued": len(pod["guidance"])}})
            publish()
            return
        elif method == "_sierralab/lab/release":
            pod["isolated"], pod["status"], pod["since"] = False, "idle", time.time()
            emit({"jsonrpc": "2.0", "id": ident, "result": {"pod": pod["id"], "isolated": False}})
            publish()
            return
        else:  # isolate: the pod and what it spawned
            also = [q["id"] for q in PODS.values() if q["parent"] == pod["id"]]
            for stopped in [pod, *[PODS[i] for i in also]]:
                stopped["isolated"], stopped["status"], stopped["since"], stopped["pending_tool"] = True, "isolated", time.time(), None
                stopped["trace"].append({"tool": "ISOLATE", "status": "failed", "input": "lab master", "ms": 0, "ts": time.time()})
            emit({"jsonrpc": "2.0", "id": ident, "result": {"pod": pod["id"], "isolated": True, "cancelled": True, "also": also}})
            publish()
            return
    emit({"jsonrpc": "2.0", "id": ident, "result": {"pod": pod["id"], "applied": applied}})
    publish()


def main():
    emit({"jsonrpc": "2.0", "method": "_sierralab/hello"})
    started = False
    for line in sys.stdin:
        line = line.rstrip("\r\n")
        try:
            message = json.loads(line)
        except ValueError:
            continue
        method = message.get("method")
        params = message.get("params") or {}
        if method is None and "id" in message:
            outcome = ((message.get("result") or {}).get("outcome") or {})
            log("response", {"id": message["id"], "outcome": outcome})
            emit({"jsonrpc": "2.0", "method": "_sierralab/answered", "params": {"id": message["id"], "optionId": outcome.get("optionId")}})
            if message["id"] in PERMITS:
                ANSWERS[message["id"]] = outcome.get("optionId") or "reject"
                PERMITS.pop(message["id"]).set()
            continue
        log("received", {"method": method, "id": message.get("id"), "params": params})
        if method == "demo/start" and not started:
            started = True
            SESSION["id"] = params.get("sessionId")
            publish()
            threading.Thread(target=scenario, name="demo-scenario", daemon=True).start()
        elif method == "session/set_mode":
            if params.get("modeId") in MODES:
                MODE["value"] = params["modeId"]
                emit({"jsonrpc": "2.0", "id": message["id"], "result": {}})
                publish()
            else:
                emit({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32602, "message": "Invalid params", "data": {"error": "unknown mode"}}})
        elif method in ("_sierralab/lab/effort", "_sierralab/lab/dial", "_sierralab/lab/isolate", "_sierralab/lab/inject", "_sierralab/lab/release"):
            handle_control(message, method, params)
        elif method == "_sierralab/os-context" and "id" in message:
            # The menu-bar deck's capture, registered for the next turn the way `acp_server` does it.
            # The demo has no model to show it to, so it is acknowledged and named in the feed — a
            # child that never answered would hold the deck's `SubmitTurn` until it timed out.
            app = str(params.get("app_name") or "an app")[:80]
            emit({"jsonrpc": "2.0", "id": message["id"], "result": {"registered": True}})
            with STATE_LOCK:
                PODS["orchestrator"]["trace"].append({"tool": "SCREEN", "status": "done", "input": f"context from {app}", "ms": 0, "ts": time.time()})
            publish()


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""A scriptable stand-in for the ``claude`` and ``agy`` CLIs, for the worker conformance suite.

Run as ``fake_cli_worker.py claude …`` or ``fake_cli_worker.py agy …`` (the conformance test points the worker's
binary at a two-line wrapper that does that). It reads its prompt the way the real CLI does — raw stdin for claude, a
``{"event": "user", "message": {"content": …}}`` NDJSON line for agy — picks a scenario by the ``[[SCENARIO:name]]``
marker in the prompt from the JSON file named by ``FAKE_WORKER_SCRIPT``, and plays it in the real CLI's
``stream-json`` dialect.

Scenario actions (a list, played in order):

    {"text": "..."}                       an assistant message / agent_response step
    {"tool": "Read", "input": {...}}      a tool call and its result
    {"usage": [tokens_in, tokens_out], "cost": 0.01}   metered usage (claude: per message; agy: per step)
    {"write": "path", "content": "..."}   actually write a file (Seatbelt decides whether that works)
    {"probe_write": "path"}               try to write, record "ok"/"denied" in the report
    {"probe_env": true}                   try to read .env, record the result
    {"sleep": 0.5}
    {"hang": true}                        write hang.pid in $FAKE_WORKER_STATE and sleep until killed
    {"rate_limit": true}                  end with the provider's usage-limit error
    {"exit": 3}                           end with this exit code and an error result
    {"structured": {...}}                 the final structured answer
    {"report": true}                      put a JSON report (env keys, proxy, argv, prompt, probes) in the final text

Stdlib only; never imported by the server.
"""

import json
import os
import sys
import time
import uuid


def main() -> int:
    dialect = sys.argv[1]
    argv = sys.argv[2:]
    raw = sys.stdin.read()
    prompt = raw
    if dialect == "agy":
        try:
            prompt = json.loads(raw.strip().splitlines()[0])["message"]["content"]
        except (ValueError, KeyError, IndexError):
            emit_agy_result("ERROR", "", error='stream input message is missing the "event" field')
            return 1
    with open(os.environ["FAKE_WORKER_SCRIPT"], encoding="utf-8") as handle:
        scenarios = json.load(handle)
    name = "default"
    if "[[SCENARIO:" in prompt:
        name = prompt.split("[[SCENARIO:", 1)[1].split("]]", 1)[0]
    actions = scenarios.get(name, [])

    resume = None
    for flag in ("--resume", "--conversation"):
        if flag in argv:
            resume = argv[argv.index(flag) + 1]
    session = resume or f"fake-{uuid.uuid4().hex[:8]}"
    model = argv[argv.index("--model") + 1] if "--model" in argv else "fake-default"
    report = {"probes": {}}
    texts: list[str] = []
    structured = None
    cost = 0.0
    totals = [0, 0]

    if dialect == "claude":
        emit({"type": "system", "subtype": "init", "session_id": session, "model": model})
    else:
        emit({"event": "init", "conversation_id": session, "init": {"model": model, "tools": [], "permission_mode": "request-review"}})
        emit_step("user_input", "DONE")

    for action in actions:
        if "text" in action:
            texts.append(action["text"])
            if dialect == "claude":
                emit({"type": "assistant", "message": {"id": uuid.uuid4().hex, "content": [{"type": "text", "text": action["text"]}]}})
            else:
                emit_step("agent_response", "ACTIVE", text_delta=action["text"])
        elif "tool" in action:
            if dialect == "claude":
                tid = uuid.uuid4().hex[:8]
                emit({"type": "assistant", "message": {"id": uuid.uuid4().hex, "content": [{"type": "tool_use", "id": tid, "name": action["tool"], "input": action.get("input", {})}]}})
                emit({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": tid, "content": "ok"}]}})
            else:
                emit_step("tool", "ACTIVE", tool_name=action["tool"], tool_info={"name": action["tool"], "parameters": action.get("input", {})})
                emit_step("tool", "DONE", tool_name=action["tool"])
        elif "usage" in action:
            tin, tout = action["usage"]
            totals[0] += tin
            totals[1] += tout
            cost += float(action.get("cost", 0.0))
            if dialect == "claude":
                emit({"type": "assistant", "message": {"id": uuid.uuid4().hex, "content": [], "usage": {"input_tokens": tin, "output_tokens": tout}}})
            else:
                emit_step("agent_response", "DONE", usage={"input_tokens": tin, "output_tokens": tout, "thinking_tokens": 0, "cache_read_tokens": 0})
        elif "write" in action:
            path = action["write"]
            os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
            with open(path, "w", encoding="utf-8") as out:
                out.write(action.get("content", ""))
        elif "probe_write" in action:
            try:
                with open(action["probe_write"], "w", encoding="utf-8") as out:
                    out.write("probe\n")
                report["probes"][action["probe_write"]] = "ok"
            except OSError:
                report["probes"][action["probe_write"]] = "denied"
        elif action.get("probe_env"):
            try:
                with open(".env", encoding="utf-8") as env_file:
                    env_file.read()
                report["probes"][".env"] = "READ"
            except OSError:
                report["probes"][".env"] = "denied"
        elif "sleep" in action:
            time.sleep(float(action["sleep"]))
        elif action.get("hang"):
            state = os.environ.get("FAKE_WORKER_STATE")
            if state:
                with open(os.path.join(state, "hang.pid"), "w", encoding="utf-8") as out:
                    out.write(str(os.getpid()))
            time.sleep(600)
        elif action.get("rate_limit"):
            message = "Claude AI usage limit reached|1775880000" if dialect == "claude" else "RESOURCE_EXHAUSTED: quota exceeded for model"
            finish(dialect, session, texts, structured, cost, totals, error=message)
            return 1
        elif "exit" in action:
            finish(dialect, session, texts, structured, cost, totals, error=f"fake failure (exit {action['exit']})")
            return int(action["exit"])
        elif "structured" in action:
            structured = action["structured"]
        elif action.get("report"):
            report.update({
                "env_keys": sorted(os.environ),
                "https_proxy": os.environ.get("HTTPS_PROXY"),
                "argv": argv,
                "prompt": prompt,
                "cwd": os.getcwd(),
                "session": session,
                "resumed": resume,
            })
            texts.append("REPORT:" + json.dumps(report))
    finish(dialect, session, texts, structured, cost, totals)
    return 0


def emit(event):
    print(json.dumps(event), flush=True)


_STEP = [1]


def emit_step(step_type, state, **extra):
    emit({"event": "step_update", "step_update": {"conversation_id": "fake", "step_index": _STEP[0], "state": state, "step_type": step_type, **extra}})
    if state == "DONE" or step_type == "tool":
        _STEP[0] += 1


def emit_agy_result(status, response, error=None, usage=None, conversation=""):
    result = {"conversation_id": conversation, "status": status, "response": response, "num_turns": 1, "usage": usage or {"input_tokens": 0, "output_tokens": 0, "thinking_tokens": 0, "cache_read_tokens": 0}}
    if error:
        result["error"] = error
    emit({"event": "result", "result": result})


def finish(dialect, session, texts, structured, cost, totals, error=None):
    text = "\n".join(texts)
    if dialect == "claude":
        event = {"type": "result", "subtype": "success" if not error else "error_during_execution", "is_error": bool(error), "num_turns": 1, "session_id": session,
                 "total_cost_usd": cost, "result": text if not error else error}
        if error:
            event["errors"] = [error]
        if structured is not None:
            event["structured_output"] = structured
        emit(event)
    else:
        response = json.dumps(structured) if structured is not None else text
        usage = {"input_tokens": totals[0], "output_tokens": totals[1], "thinking_tokens": 0, "cache_read_tokens": 0}
        emit_agy_result("SUCCESS" if not error else "ERROR", response if not error else "", error=error, usage=usage, conversation=session)


if __name__ == "__main__":
    sys.exit(main())

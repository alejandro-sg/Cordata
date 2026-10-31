"""Background shell tasks (background_tasks.py) and lifecycle hooks (lifecycle_hooks.py). Hermetic: real processes, no
model, no network.

* a task starts detached and returns at once; its output is a bounded head + tail; it exits with its code and leaves a
  0600 summary; a kill ends the whole process group (a grandchild included); a timeout does the same; ids are validated;
* ``background_shell`` is guarded exactly like ``shell``: approval, Laya Guard's reading, permission rules, checkpoints;
* the ACP surface: ``_cordata/tasks/{list,status,kill}``;
* hooks: PreToolUse exit 2 blocks with its stderr as the reason, other failures do not; PostToolUse output is appended,
  fenced; Stop runs once; the matcher limits them; every ``hooks.*`` key is trust-gated.

    backend/.venv/bin/python _background_tasks_smoke_test.py
"""

from __future__ import annotations

import asyncio
import os
import stat
import sys
import tempfile
import time
from pathlib import Path
from types import SimpleNamespace

_HOME = tempfile.mkdtemp(prefix="bg-home-")
os.environ["CORDATA_HOME"] = _HOME

import background_tasks as bt  # noqa: E402
import checkpoints  # noqa: E402
import laya_guard  # noqa: E402
import lifecycle_hooks as lh  # noqa: E402
import settings_store  # noqa: E402
from harness_testing import check  # noqa: E402


def wait_for(predicate, timeout: float = 10.0) -> bool:
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if predicate():
            return True
        time.sleep(0.05)
    return False


def alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


def test_tasks() -> None:
    print("\n=== background tasks ===")
    manager = bt.TaskManager(store=Path(_HOME) / "tasks")
    cwd = tempfile.mkdtemp(prefix="bg-proj-")
    started = time.monotonic()
    task = manager.start("for i in $(seq 1 1000); do echo line $i; done; exit 3", cwd)
    check(time.monotonic() - started < 1.0 and task.state in ("running", "exited"), "start returns at once")
    check(wait_for(lambda: task.state == "exited"), "the task runs to its end")
    check(task.exit_code == 3 and task.lines == 1000, f"exit code and line count are kept ({task.exit_code}, {task.lines})")
    whole = task.output()
    check("line 1\n" in whole and "line 1000" in whole and "dropped" in whole and len(task.head) + len(task.tail) <= bt.HEAD_LINES + bt.TAIL_LINES, "output is a bounded head + tail, and says what it dropped")
    check(task.output(last=5).rstrip().endswith("line 1000") and "earlier line" in task.output(last=5), "the read-back window is the last N lines")
    summary = Path(_HOME) / "tasks" / f"{task.task_id}.json"
    check(summary.exists() and stat.S_IMODE(summary.stat().st_mode) == 0o600, "a 0600 summary outlives the task")

    marker = Path(cwd) / "grandchild.pid"
    tree = manager.start(f"sh -c 'sleep 60 & echo $! > {marker}; wait' ; sleep 60", cwd)
    check(wait_for(marker.exists) and wait_for(lambda: marker.read_text().strip() != ""), "a task can start children")
    grandchild = int(marker.read_text().strip())
    result = manager.kill(tree.task_id)
    check(result["ok"] and wait_for(lambda: tree.state == "killed" and tree.ended is not None), "kill ends the task")
    check(wait_for(lambda: not alive(grandchild), 5.0), "…and its whole process group, grandchild included")
    check(manager.kill(tree.task_id).get("already") == "killed", "a second kill is a no-op, not an error")

    slow = manager.start("sleep 30", cwd, timeout_s=1)
    check(wait_for(lambda: slow.state == "timed_out" and slow.ended is not None, 8.0), f"a timeout kills the group ({slow.state})")
    check(manager.get("../../etc/passwd") is None and manager.get("deadbeef0000") is None and not manager.kill("x")["ok"], "an id is 12 hex chars and must be one this process started")

    exits: list[dict] = []
    manager.listeners.append(exits.append)
    done = manager.start("echo bye", cwd)
    mine = lambda: [e for e in exits if e["taskId"] == done.task_id]  # noqa: E731
    check(wait_for(lambda: bool(mine())) and mine()[0]["exitCode"] == 0, "an exit notice is sent when a task ends")
    check([t["taskId"] for t in manager.list()][:1] == [done.task_id], "list is newest first")

    old = bt.MANAGER
    bt.MANAGER = manager
    previous = os.getcwd()
    os.chdir(cwd)
    try:
        text = bt.background_shell._tool_func(command="echo from-tool", timeout=30)
        tid = text.split()[3]
        check(text.startswith("Started background task") and wait_for(lambda: manager.get(tid).state == "exited"), f"the tool starts a task ({text[:60]})")
        check("from-tool" in bt.task_output._tool_func(task_id=tid, lines=10) and "exited" in bt.task_output._tool_func(task_id=tid), "task_output reads it back")
        check("No background task" in bt.task_output._tool_func(task_id="nope"), "an unknown id is answered, not raised")
    finally:
        os.chdir(previous)
        bt.MANAGER = old
    # Per-session build_tools binds cwd + session_id (never global os.getcwd()).
    cwd_a = tempfile.mkdtemp(prefix="bg-sess-a-")
    cwd_b = tempfile.mkdtemp(prefix="bg-sess-b-")
    outside = tempfile.mkdtemp(prefix="bg-outside-")
    seen_a: list[dict] = []
    tools_a = bt.build_tools(cwd_a, session_id="sess-a", manager=manager, on_event=seen_a.append)
    tools_b = bt.build_tools(cwd_b, session_id="sess-b", manager=manager)
    previous = os.getcwd()
    os.chdir(outside)
    try:
        started_a = tools_a[0]._tool_func(command="pwd", timeout=30)
        tid_a = started_a.split()[3]
        started_b = tools_b[0]._tool_func(command="sleep 30", timeout=30)
        tid_b = started_b.split()[3]
        check(wait_for(lambda: manager.get(tid_a).state == "exited"), "per-session background_shell runs to completion")
        out_a = tools_a[1]._tool_func(task_id=tid_a, lines=10)
        check(os.path.realpath(cwd_a) in out_a and os.path.realpath(outside) not in out_a, f"build_tools runs in session cwd, not os.getcwd() ({out_a.strip()})")
        check(manager.get(tid_a).session_id == "sess-a" and manager.get(tid_b).session_id == "sess-b", "each task carries its session_id")
        ids_a = {t["taskId"] for t in manager.list("sess-a")}
        check(tid_a in ids_a and tid_b not in ids_a, f"manager.list(session_id) isolates sessions ({ids_a})")
        killed_b = tools_b[2]._tool_func(task_id=tid_b)
        check("killed" in killed_b and wait_for(lambda: manager.get(tid_b).state == "killed"), "per-session task_kill ends its process group")
        check(any(e.get("event") == "task_start" and e.get("sessionId") == "sess-a" for e in seen_a), "build_tools emits task_start with sessionId")
    finally:
        os.chdir(previous)
    manager.shutdown()


def test_guarded_like_shell() -> None:
    print("\n=== guarded like shell ===")
    import acp_approval

    check("background_shell" in acp_approval.APPROVAL_REQUIRED_TOOLS and "background_shell" not in acp_approval.PLAN_MODE_TOOLS, "it asks in build mode and is refused in plan mode")
    check("task_output" in acp_approval.PLAN_MODE_TOOLS and "task_output" not in acp_approval.APPROVAL_REQUIRED_TOOLS, "reading a task's output never asks")
    check("background_shell" in laya_guard.GUARDED_TOOLS, "Laya Guard watches it")
    dangerous = {"command": "rm -rf ~/"}
    check(laya_guard.rule_score("background_shell", dangerous, "/tmp") == laya_guard.rule_score("shell", dangerous, "/tmp"), "…and reads it exactly as it reads shell")
    check(checkpoints.writes_files("background_shell", {"command": "touch x"}) and not checkpoints.writes_files("background_shell", {"command": "ls"}), "checkpoints treat it as a possible writer, read the way shell is")
    allow = settings_store.Rule("allow", "shell(git *)", "user")
    deny = settings_store.Rule("deny", "shell(rm -rf *)", "user")
    check(settings_store.rule_matches(allow, "background_shell", {"command": "git status"}, "/tmp", allow=True), "a shell(...) allow rule covers it")
    rules = settings_store.Settings(rules=[deny])
    verdict = settings_store.evaluate(rules, "background_shell", {"command": "rm -rf build"}, "/tmp")
    check(verdict is not None and verdict.action == "deny", "…and a shell(...) deny rule refuses it")


def test_acp_surface() -> None:
    print("\n=== _cordata/tasks ===")
    import acp_server
    import task_tool

    cls = acp_server.CordataAcpAgent
    instance = cls.__new__(cls)
    cwd = tempfile.mkdtemp(prefix="bg-acp-")
    task_tool.configure(lambda: "s1")
    task_tool.todo_write._tool_func(todos=[{"content": "audit branch", "status": "in_progress", "activeForm": "Auditing branch"}])
    task = bt.MANAGER.start("sleep 30", cwd, session_id="s1")
    other = bt.MANAGER.start("sleep 30", cwd, session_id="s2")
    reply = instance._tasks_control("list", {}, "s1")
    listed = reply["tasks"]
    check(any(t["taskId"] == task.task_id for t in listed) and all(t["taskId"] != other.task_id for t in listed), "tasks/list shows only the session's background tasks")
    check(any(item.get("content") == "audit branch" for item in reply.get("items") or []), "tasks/list also preserves todo_write checklist items")
    check(instance._tasks_control("status", {"taskId": task.task_id}, "s1")["state"] == "running", "tasks/status reads it")
    check(instance._tasks_control("kill", {"taskId": task.task_id}, "s1")["ok"] and wait_for(lambda: task.state == "killed"), "tasks/kill ends it")
    bt.MANAGER.kill(other.task_id)
    check(not instance._tasks_control("status", {"taskId": "../x"}, "s1")["ok"], "a bad id is refused")
    fleet = instance._ollama_fleet_status(cwd)
    snap = next(c["json"] for c in fleet["content"] if "json" in c)
    check(len(snap["cloud_fleet"]) >= 6 and any(m["alias"] == "mistral-large-4" for m in snap["cloud_fleet"]), "ollama/status exposes the zero-download :cloud fleet")


def test_hooks() -> None:
    print("\n=== lifecycle hooks ===")
    cwd = tempfile.mkdtemp(prefix="hooks-")
    values: dict[str, str] = {}
    settings = lambda: SimpleNamespace(get=values.get)  # noqa: E731
    seen: list[dict] = []
    hooks = lh.LifecycleHooks(settings, cwd=cwd, session_id="s", notify=seen.append)

    def before(name: str, command: str = "ls"):
        return SimpleNamespace(tool_use={"name": name, "input": {"command": command}}, cancel_tool=False)

    values["hooks.preToolUse"] = "grep -q 'rm -rf' && { echo 'no recursive deletes here' >&2; exit 2; } || exit 0"
    blocked, allowed = before("shell", "rm -rf build"), before("shell", "ls")
    hooks._before(blocked)
    hooks._before(allowed)
    check(isinstance(blocked.cancel_tool, str) and "no recursive deletes here" in blocked.cancel_tool, "PreToolUse exit 2 blocks the call, its stderr is the reason")
    check(allowed.cancel_tool is False, "…and exit 0 lets it through (the event arrives as JSON on stdin)")
    values["hooks.preToolUse"] = "exit 1"
    broken = before("shell")
    hooks._before(broken)
    check(broken.cancel_tool is False, "a hook that fails any other way does not wedge the call")
    values["hooks.matcher"] = "editor|apply_patch"
    values["hooks.preToolUse"] = "exit 2"
    skipped = before("shell")
    hooks._before(skipped)
    check(skipped.cancel_tool is False, "the matcher limits which tools a hook runs for")
    values.pop("hooks.matcher")

    values["hooks.postToolUse"] = "echo formatted"
    after = SimpleNamespace(tool_use={"name": "editor", "input": {}}, result={"status": "success", "content": [{"text": "edited"}]}, cancel_message=None)
    hooks._after(after)
    texts = [c["text"] for c in after.result["content"]]
    check(texts[0] == "edited" and "formatted" in texts[-1] and "UNTRUSTED_TOOL_OUTPUT" in texts[-1], "PostToolUse output is appended to the result, fenced as untrusted")

    flag = Path(cwd) / "stopped"
    values["hooks.stop"] = f"touch {flag}"
    hooks._stop(SimpleNamespace())
    check(flag.exists() and any(e["hook"] == "Stop" for e in seen), "Stop runs when the turn ends")
    check(set(lh.KEYS) <= settings_store.TRUST_GATED_KEYS, "every hooks.* key is trust-gated: a cloned repo cannot run commands on every call")
    untrusted = Path(tempfile.mkdtemp(prefix="hooks-untrusted-"))
    (untrusted / ".cordata").mkdir()
    pwned = untrusted / "pwned"
    (untrusted / ".cordata" / "settings.json").write_text(
        f'{{"hooks.preToolUse": "touch {pwned}", "hooks.postToolUse": "touch {pwned}", "hooks.stop": "touch {pwned}"}}'
    )
    real_hooks = lh.LifecycleHooks(lambda: settings_store.current(str(untrusted)), cwd=str(untrusted), session_id="u")
    ev = before("shell", "ls")
    real_hooks._before(ev)
    real_hooks._after(SimpleNamespace(tool_use={"name": "shell", "input": {}}, result={"status": "success", "content": []}, cancel_message=None))
    real_hooks._stop(SimpleNamespace())
    check(not pwned.exists() and ev.cancel_tool is False, "end-to-end: an untrusted project's .cordata/settings.json hooks never execute")
    code, _, err = lh.run_hook("sleep 5", {}, cwd, timeout=0.5)
    check(code == 124 and "timed out" in err, "a hook has a timeout")


def main() -> int:
    test_tasks()
    test_guarded_like_shell()
    test_acp_surface()
    test_hooks()
    print("\nbackground tasks and hooks: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())

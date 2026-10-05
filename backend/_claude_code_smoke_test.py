"""Phase 6: the Claude Code CLI as a Strands tool, and the security contract around it.

The CLI is a stub (``SIERRALAB_CLAUDE_BIN``) that does what the real one would —
streams ``stream-json`` events, edits a file in the project — and then *reports what it
could see*: its environment, whether it could read ``.env``, the prompt it was given and
its argv. Everything else is real: the process is spawned through the Strands
``Sandbox`` (``scrubbed_sandbox``), under Seatbelt, in a real git repository, with a real
local socket standing in for the vault proxy.

    backend/.venv/bin/python _claude_code_smoke_test.py
"""

import asyncio
import json
import os
import socket
import stat
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import provider_settings  # noqa: F401  (imported before agent_common, which chdirs — see main.py)

from strands import Agent

import claude_code_tool as cct
import harness_events
import lab as lab_module
import memory_budget as mb
import ollama_cli
import omlx_manager
import scrubbed_sandbox
import secret_guard
import trace_store
import vault_proxy
from fake_omlx import FakeOmlx
from harness_testing import ScriptedModel, check, text, tool as call

FAKE_CLAUDE = r'''#!/usr/bin/env python3
import json, os, sys, time
prompt = sys.stdin.read()
def emit(event): print(json.dumps(event), flush=True)
emit({"type": "system", "subtype": "init", "session_id": "sess-1", "model": "fake-claude"})
if "[[HANG]]" in prompt:
    open("hanging.pid", "w").write(str(os.getpid()))
    time.sleep(600)
if "[[FAIL]]" in prompt:
    sys.stderr.write("boom: auth failed sk-LEAK123456789012345678\n")
    sys.exit(3)
emit({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "app.py"}}]}})
emit({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": "x"}]}})
try:
    open(".env").read(); env_read = "READ"
except OSError as error:
    env_read = "denied"
try:
    open(".env", "a").write("X=1"); env_write = "WROTE"
except OSError:
    env_write = "denied"
open("changed.txt", "w").write("refactored\n")
time.sleep(0.4)
emit({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t2", "name": "Write", "input": {"file_path": "changed.txt"}}, {"type": "text", "text": "writing the file"}]}})
emit({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t2", "content": "ok"}]}})
report = {"env_keys": sorted(os.environ), "https_proxy": os.environ.get("HTTPS_PROXY"), "no_proxy": os.environ.get("NO_PROXY"),
          "env_read": env_read, "env_write": env_write, "argv": sys.argv[1:], "prompt": prompt, "cwd": os.getcwd()}
emit({"type": "result", "subtype": "success", "is_error": False, "duration_ms": 1234, "num_turns": 2, "total_cost_usd": 0.0123,
      "session_id": "sess-1", "result": "REPORT:" + json.dumps(report)})
'''


class Prefs:
    def __init__(self, **values) -> None:
        self.values = values

    def get(self, key, default=None):
        return self.values.get(key, default)


def quiet_ollama() -> ollama_cli.OllamaCli:
    """An Ollama with nothing resident and no way to touch the real daemon."""
    return ollama_cli.OllamaCli(binary="/bin/echo", runner=lambda argv, **kw: subprocess.CompletedProcess(argv, 0, "", ""), ps_reader=lambda: [])


def budget_for(url: str = "http://127.0.0.1:9/v1") -> mb.MemoryBudget:
    return mb.MemoryBudget(omlx_manager.OmlxManager(url), quiet_ollama(), ceiling_gb=48, reserve_gb=10)


def listener():
    server = socket.socket()
    server.bind(("127.0.0.1", 0))
    server.listen(5)
    return f"http://127.0.0.1:{server.getsockname()[1]}", server.close


def closed_addr() -> str:
    probe = socket.socket()
    probe.bind(("127.0.0.1", 0))
    port = probe.getsockname()[1]
    probe.close()
    return f"http://127.0.0.1:{port}"


def make_project(root: Path) -> Path:
    project = root / "proj"
    project.mkdir()
    (project / ".env").write_text("SECRET_TOKEN=do-not-leak\n")
    (project / ".env.example").write_text("SECRET_TOKEN=\n")
    (project / "app.py").write_text("print('hi')\n")
    for argv in (["git", "init", "-q"], ["git", "add", "-A"], ["git", "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]):
        subprocess.run(argv, cwd=project, check=True, capture_output=True)
    return project


class Harness:
    """One configured runtime plus the objects a test wants to look at afterwards."""

    def __init__(self, project: Path, *, vault, prefs=None, budget=None, lab=None) -> None:
        self.sandbox: scrubbed_sandbox.ScrubbedSandbox | None = None
        self.lab = lab

        def make_sandbox(root: str) -> scrubbed_sandbox.ScrubbedSandbox:
            self.sandbox = scrubbed_sandbox.ScrubbedSandbox(root)
            return self.sandbox

        cct.configure(
            cwd=lambda: str(project),
            vault=lambda: vault,
            preferences=lambda: prefs or Prefs(),
            sandbox=make_sandbox,
            budget=(lambda: budget) if budget is not None else budget_for,
            lab=lambda: lab,
            session_id=lambda: "sess-test",
        )


def run(task: str, **kwargs) -> tuple[list[dict], dict]:
    async def go():
        seen: list[dict] = []
        result: dict = {}
        async for item in cct.run_engineer(task, **kwargs):
            if "progress" in item:
                seen.append(item)
            else:
                result = item
        return seen, result

    return asyncio.run(go())


def data(result: dict) -> dict:
    return next(block["json"] for block in result["content"] if "json" in block)


def report(result: dict) -> dict:
    return json.loads(data(result)["result"].split("REPORT:", 1)[1])


def alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


def main() -> None:
    check(scrubbed_sandbox.seatbelt_works(), "Seatbelt can be applied here (the confinement check needs it)")
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp).resolve()
        fake = root / "claude"
        fake.write_text(FAKE_CLAUDE)
        fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
        os.environ[cct.BINARY_ENV] = str(fake)
        project = make_project(root)
        addr, close_vault = listener()
        vault_ok = vault_proxy.VaultStatus(enabled=True, ok=True, credentialed=True, addr=addr, reason="test vault")

        # Credentials sitting in *this* process's environment, which the child must not inherit.
        planted = {
            "ANTHROPIC_API_KEY": "sk-ant-planted-key-123456789",
            "GITHUB_TOKEN": "ghp_plantedtoken123456789",
            "OPENAI_API_KEY": "__openai_api_key__",
            "OLLAMA_API_KEY": "planted-ollama",
            "AWS_SECRET_ACCESS_KEY": "planted-aws",
            "HF_TOKEN": "hf_planted",
        }
        os.environ.update(planted)
        try:
            # ------------------------------------------------ egress policy: fail closed
            for label, status, why in (
                ("vault off", vault_proxy.VaultStatus(enabled=False), "vault off"),
                ("vault unreachable", vault_proxy.VaultStatus(enabled=True, ok=False, addr=closed_addr(), reason="the vault is unreachable"), "vault unreachable"),
                ("no vault status at all", None, "vault off"),
            ):
                h = Harness(project, vault=status)
                _, result = run("refactor the thing")
                check(result["status"] == "error" and data(result)["reason"] == why, f"{label}: refused, nothing spawned ({data(result)['reason']})")
                check(h.sandbox is None or h.sandbox.last_pid is None, f"{label}: no process was started")

            # ------------------------------------------------------------- the happy path
            events: list[tuple[str, dict]] = []
            unsubscribe = harness_events.subscribe(lambda kind, payload: events.append((kind, payload)))
            lab = lab_module.Lab()
            lab.pod("orchestrator", role="orchestrator")
            h = Harness(project, vault=vault_ok, budget=budget_for(), lab=lab)
            progress, result = run("refactor the parser module", files=["app.py"])
            unsubscribe()
            check(result["status"] == "success", "a finished run is status=success")
            body = data(result)
            check(result["content"][0]["text"].startswith("Claude Code finished"), "the text block is a human summary")
            check(body["exit_code"] == 0 and body["num_turns"] == 2 and abs(body["cost_usd"] - 0.0123) < 1e-9, "exit code, turns and cost come from the CLI's result event")
            check(body["claude_session_id"] == "sess-1" and body["model"] == "fake-claude", "session id and model come from the init event")
            check(body["files_changed"] == ["changed.txt"], f"files changed come from git ({body['files_changed']})")
            check(isinstance(body["pid"], int) and not alive(body["pid"]), "the child ran as a real process and has exited")
            seen = report(result)

            check(seen["https_proxy"] == addr, f"HTTPS_PROXY points at the vault ({seen['https_proxy']})")
            check("127.0.0.1" in (seen["no_proxy"] or ""), "NO_PROXY keeps loopback off the proxy")
            leaked = [name for name in planted if name in seen["env_keys"]]
            check(not leaked, f"the child does not inherit provider keys or tokens ({leaked or 'none'})")
            check("__openai_api_key__" not in json.dumps(seen["env_keys"]) and not [k for k in seen["env_keys"] if k.startswith(("OLLAMA_", "AWS_"))], "no vault placeholder, no OLLAMA_*/AWS_* either")
            check({"PATH", "HOME"} <= set(seen["env_keys"]), "PATH and HOME are there (the CLI needs its login)")
            check(seen["env_read"] == "denied" and seen["env_write"] == "denied", "the child cannot read OR write .env — the kernel refuses")
            check((project / ".env").read_text() == "SECRET_TOKEN=do-not-leak\n", ".env on disk is untouched")
            check(data(result)["confinement"] == "seatbelt", "the result states what confined it")
            check(seen["cwd"] == str(project), "it ran in the project directory")

            argv = seen["argv"]
            check(argv[0] == "-p" and "stream-json" in argv and argv[argv.index("--permission-prompts") + 1] == "none", "headless flags: -p, stream-json, no permission prompts")
            check("--prompt" not in argv and "refactor the parser module" not in argv, "the task is not in argv (it went in on stdin)")
            check("refactor the parser module" in seen["prompt"] and "Start with these files: app.py" in seen["prompt"], "the task and its files reach the CLI on stdin")
            allowed = argv[argv.index("--allowedTools") + 1]
            denied = argv[argv.index("--disallowedTools") + 1]
            check("Bash(git status:*)" in allowed and "Bash(curl" not in allowed and "Read(**/.env)" in denied and "WebFetch" in denied, "an explicit allowlist and a denylist that covers .env and the network")
            check("--strict-mcp-config" in argv and "--no-session-persistence" in argv, "no MCP servers, no session left on disk")

            check(len(progress) >= 1 and "Read app.py" in progress[0]["progress"], f"progress streams while it runs ({progress[0]['progress'] if progress else None})")
            tasks = [payload for kind, payload in events if kind == "task"]
            check(tasks and tasks[0]["source"] == "claude_code_engineer" and tasks[-1]["state"] == "completed", "a Task bar was opened and completed (the TUI shows it)")
            pod = lab.pods["claude_code_engineer"]
            check(pod.status == "idle" and [row["tool"] for row in pod.trace] == ["Read", "Write"], "the Lab pod traced the CLI's tool calls and went back to IDLE")
            check(all(row["status"] == "completed" for row in pod.trace), "…each closed by its result")
            link = lab.links[("orchestrator", "claude_code_engineer")]
            check(link["kind"] == "delegation" and link["active"] is False and link["count"] == 1, "the orchestrator→CLI link was drawn and closed")
            (project / "changed.txt").unlink()
            subprocess.run(["git", "checkout", "-q", "."], cwd=project, capture_output=True)

            # --------------------------------------------------------- failure is a result
            h = Harness(project, vault=vault_ok, budget=budget_for())
            secret_guard.register_secret("sk-LEAK123456789012345678")  # redaction masks *registered* secrets (secret_guard)
            _, result = run("[[FAIL]] please")
            body = data(result)
            check(result["status"] == "error" and body["exit_code"] == 3 and not body["ok"], "a non-zero exit is an error result, not an exception")
            check("auth failed" in body["stderr_tail"] and "sk-LEAK123456789012345678" not in body["stderr_tail"], "stderr is returned, with a registered secret masked")
            check("FAILED" in result["content"][0]["text"], "the text block says it failed")

            # ------------------------------------------------------------------- timeout
            started = time.monotonic()
            _, result = run("[[HANG]] forever", timeout_seconds=2)
            body = data(result)
            check(result["status"] == "error" and body["timed_out"] and time.monotonic() - started < 15, "a hung CLI is killed at the timeout and reported")
            hung = int((project / "hanging.pid").read_text())
            time.sleep(0.3)
            check(not alive(hung), "…and its whole process group is gone")

            # --------------------------------------------------- E-Stop kills a running CLI
            lab2 = lab_module.Lab()
            lab2.pod("orchestrator", role="orchestrator")
            Harness(project, vault=vault_ok, budget=budget_for(), lab=lab2)
            (project / "hanging.pid").unlink()

            async def estop():
                task = asyncio.create_task(collect("[[HANG]] until stopped"))
                for _ in range(100):
                    await asyncio.sleep(0.1)
                    if (project / "hanging.pid").exists():
                        break
                pid = int((project / "hanging.pid").read_text())
                check(alive(pid) and lab2.pods["claude_code_engineer"].status == "executing", "mid-run: the CLI is alive and its pod is EXECUTING")
                reply = lab2.isolate("claude_code_engineer")
                outcome = await asyncio.wait_for(task, 15)
                await asyncio.sleep(0.3)
                return pid, reply, outcome

            async def collect(task_text):
                last = {}
                async for item in cct.run_engineer(task_text, timeout_seconds=60):
                    last = item
                return last

            pid, reply, outcome = asyncio.run(estop())
            check(reply["cancelled"] and not alive(pid), "E-Stop on the pod kills the CLI's process group")
            check(outcome["status"] == "error" and lab2.pods["claude_code_engineer"].status == "isolated", "the run ends as an error and the pod is ISOLATED")
            _, result = run("anything")
            check(result["status"] == "error" and "isolated" in data(result)["reason"], "an isolated pod refuses to be started again")
            lab2.release("claude_code_engineer")

            # ------------------------------------------ memory: room first, deterministically
            with FakeOmlx(loaded={"Ternary-Bonsai-2-27B-mlx-2bit"}) as omlx:
                budget = budget_for(omlx.base_url)
                Harness(project, vault=vault_ok, budget=budget)
                _, result = run("do something heavy")
                check(result["status"] == "success", "the run succeeds")
                check(not omlx.loaded and omlx.events == ["unload Ternary-Bonsai-2-27B-mlx-2bit"], f"a loaded Prism model was unloaded before the CLI started ({omlx.events})")
                freed = data(result)["memory_freed"]
                check([r["name"] for r in freed["freed"]] == ["Ternary-Bonsai-2-27B-mlx-2bit"], "the result records what was freed")
                (project / "changed.txt").unlink(missing_ok=True)

            # ------------------------------------------------------ memory.lance → prompt
            secret_guard.register_secret("hunter2-hunter2-hunter2")
            rows = [
                {"kind": "command", "tool": "shell", "exit_code": 1, "status": "failed", "error": "ImportError: cannot import name 'parse' from 'parser'", "text": ""},
                {"kind": "command", "tool": "shell", "exit_code": 0, "status": "completed", "text": "pytest -q passed; token hunter2-hunter2-hunter2", "error": ""},
            ]
            queries: list[str] = []
            real_search = trace_store.search
            trace_store.search = lambda query, **kw: queries.append(query) or rows
            try:
                Harness(project, vault=vault_ok, prefs=Prefs(**{"memory.traces": True}), budget=budget_for())
                _, result = run("fix the parser import")
                prompt = report(result)["prompt"]
                check(queries == ["fix the parser import"], "the corpus is searched with the task text")
                check("ImportError: cannot import name 'parse'" in prompt and "FAILED" in prompt, "memory.lance hits are injected into the prompt")
                check("<<<UNTRUSTED_TOOL_OUTPUT>>>" in prompt and "memory.lance" in prompt, "…wrapped as untrusted data and attributed to the corpus")
                check("hunter2-hunter2-hunter2" not in prompt, "…with registered secrets masked")
                check(data(result)["context"] == {"source": "memory.lance", "enabled": True, "hits": 2}, "the result reports how many hits primed it")
                (project / "changed.txt").unlink(missing_ok=True)

                Harness(project, vault=vault_ok, prefs=Prefs(), budget=budget_for())
                queries.clear()
                _, result = run("fix the parser import")
                check(not queries and data(result)["context"]["enabled"] is False and "UNTRUSTED" not in report(result)["prompt"], "memory.traces off: no search, no context, and the result says so")
                (project / "changed.txt").unlink(missing_ok=True)
            finally:
                trace_store.search = real_search

            # ------------------------------------------------------------ input validation
            Harness(project, vault=vault_ok)
            for bad_kwargs, label in (({"files": [".env"]}, "a .env file"), ({"files": ["../outside"]}, "a path outside the project"), ({"model": "--dangerously-skip-permissions"}, "a flag as a model name")):
                _, result = run("task", **bad_kwargs)
                check(result["status"] == "error", f"{label} is refused")
            _, result = run("   ")
            check(result["status"] == "error", "an empty task is refused")
            Harness(project, vault=vault_ok, prefs=Prefs(**{"claudeCode.enabled": False}))
            _, result = run("task")
            check(data(result)["reason"] == "disabled", "claudeCode.enabled=false switches the tool off")
            os.environ[cct.BINARY_ENV] = str(root / "no-such-claude")
            Harness(project, vault=vault_ok)
            _, result = run("task")
            check(data(result)["reason"] == "cli not found", "a missing CLI is a clear error")
            os.environ[cct.BINARY_ENV] = str(fake)

            # ------------------------------------------ through a real Strands Agent loop
            Harness(project, vault=vault_ok, budget=budget_for())
            model = ScriptedModel([call("claude_code_engineer", {"task": "refactor everything"}), text("the refactor is done")])
            agent = Agent(model=model, tools=[cct.claude_code_engineer], callback_handler=None)

            async def stream_events():
                kinds = []
                async for event in agent.stream_async("please refactor"):
                    if event.get("type") == "tool_stream":
                        kinds.append(event["tool_stream_event"]["data"])
                return kinds

            streamed = asyncio.run(stream_events())
            check(any(isinstance(item, dict) and "progress" in item for item in streamed), "the loop surfaces the tool's progress as tool_stream events (the ACP server renders them)")
            results = [b["toolResult"] for m in agent.messages for b in m["content"] if "toolResult" in b]
            check(len(results) == 1 and results[0]["status"] == "success", "the Orchestrator receives a Strands ToolResult with status=success")
            blocks = results[0]["content"]
            check(any("text" in b and "Claude Code finished" in b["text"] for b in blocks) and any("json" in b for b in blocks), "…with a text block and a structured JSON block")
            check("the refactor is done" in str(agent.messages[-1]), "and the loop continues to the model's reply")
        finally:
            for name in planted:
                os.environ.pop(name, None)
            os.environ.pop(cct.BINARY_ENV, None)
            close_vault()

    print("\nclaude_code_tool: all checks passed")


if __name__ == "__main__":
    main()

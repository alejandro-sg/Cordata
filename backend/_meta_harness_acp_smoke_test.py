"""Phases 6 + 7 through the real ACP server object — the verification gates, offline.

* **Phase 6 gate.** A complex refactoring prompt → Laya routes it to the
  ``claude_code_engineer`` tool → the approval gate asks (and refuses in plan mode / with
  the vault down) → the Claude Code CLI is spawned through the Strands ``Sandbox`` with
  ``HTTPS_PROXY`` set and no ``.env`` → its output is captured and its progress and
  completion stream back as ACP tool updates, ``_cordata/task`` and Lab state — the
  events the Ratatui Lab renders. A trivial task is *not* routed to it.
* **Phase 7 gate.** A simple architecture-planning prompt → Laya routes it to the architect →
  which runs on a Prism model on oMLX, loaded just before and unloaded just after; the
  Claude CLI is never started for it. A heavy Claude task unloads a resident Prism model
  first. ``ollama pull``/``rm`` always ask, even in auto mode; ``stop`` and the oMLX
  load/unload never do.

The CLI is a stub binary, oMLX is ``fake_omlx.FakeOmlx``, the vault is a local socket, models
and Laya are scripted — the harness's own code paths are the real ones.

    backend/.venv/bin/python _meta_harness_acp_smoke_test.py
"""

import asyncio
import json
import os
import socket
import stat
import subprocess
from pathlib import Path

import _acp_harness_smoke_test as base
from _acp_harness_smoke_test import ROOT, MODELS, call, check, laya_events, orchestrator_model, project, settle, text

import acp
import acp_server
import claude_code_tool as cct
import lab as lab_module
import memory_budget as mb
import ollama_cli
import omlx_manager
import omlx_provider as op
import specialists
import sub_agent_common
import vault_proxy
from _claude_code_smoke_test import FAKE_CLAUDE
from fake_omlx import FakeOmlx
from harness_testing import ScriptedModel

# The base module scripts every specialist; this test wants the real model wiring (Prism and all).
specialists.build_specialist = base._real_build_specialist


def _real_model(tier, role=None):
    return sub_agent_common.build_sub_agent_model(tier, role=role)


specialists._model = _real_model

ARCH_WIRE = "Ternary-Bonsai-2-27B-mlx-2bit"
DIGEST_WIRE = "Ternary-Bonsai-27B-mlx-2bit"


class Conn(base.Conn):
    """Records everything and parks permission requests until the test decides."""

    def __init__(self) -> None:
        super().__init__()
        self.waiting: list[asyncio.Future] = []
        self.asked: list = []
        self.auto: str | None = None

    async def request_permission(self, session_id, tool_call, options, **kwargs):
        self.asked.append(tool_call)
        if self.auto:
            option = self.auto
        else:
            future = asyncio.get_running_loop().create_future()
            self.waiting.append(future)
            option = await future
        return acp.schema.RequestPermissionResponse(outcome=acp.schema.AllowedOutcome(outcome="selected", option_id=option))

    def decide(self, option_id: str) -> None:
        self.waiting.pop(0).set_result(option_id)


def laya(route: str, confidence: float = 0.97, *, complexity: float = 1.0, complexity_conf: float = 0.8, risk: float = 1.0):
    probabilities = {route: confidence, "code": 1 - confidence}

    def predict(text_, questions, **kwargs):
        if "command_ok" in questions:
            return {"answers": {"command_ok": {"noul": 0.9}}}
        return {
            "answers": {
                "route": {"choice": route, "probabilities": probabilities, "answer_confidence": confidence},
                "risk": {"score": risk, "answer_confidence": 0.5},
                "complexity": {"score": complexity, "answer_confidence": complexity_conf},
            }
        }

    return predict


def new_server(predict):
    server, _ = base.fresh_server(predict)
    conn = Conn()
    server.on_connect(conn)
    return server, conn


async def until(condition, label: str, timeout: float = 15.0) -> None:
    for _ in range(int(timeout / 0.02)):
        if condition():
            return
        await asyncio.sleep(0.02)
    raise AssertionError(f"FAIL (timeout): {label}")


def tool_updates(conn, title: str) -> list:
    """Every update of the tool call(s) titled ``title``: the start, then its progress updates
    (which carry only the call's id)."""
    ids = {u.tool_call_id for u in conn.updates if type(u).__name__ == "ToolCallStart" and u.title == title}
    return [u for u in conn.updates if type(u).__name__ in ("ToolCallStart", "ToolCallProgress") and u.tool_call_id in ids]


def update_text(update) -> str:
    return " ".join(getattr(getattr(c, "content", None), "text", "") or "" for c in (getattr(update, "content", None) or []))


def lab_states(conn) -> list[dict]:
    return [params for method, params in conn.ext if method == "cordata/lab"]


def quiet_ollama() -> ollama_cli.OllamaCli:
    class Stub(ollama_cli.OllamaCli):
        def __init__(self) -> None:
            self.calls: list[list[str]] = []
            super().__init__(binary="/bin/echo", runner=self._run, ps_reader=lambda: [])

        def _run(self, argv, **kwargs):
            self.calls.append(list(argv))
            return subprocess.CompletedProcess(argv, 0, f"ran {' '.join(argv[1:])}\n", "")

    return Stub()


def listener():
    server = socket.socket()
    server.bind(("127.0.0.1", 0))
    server.listen(5)
    return f"http://127.0.0.1:{server.getsockname()[1]}", server.close


def set_vault(addr: str | None, policy: str = "deny") -> None:
    home = ROOT / "home"
    home.mkdir(exist_ok=True)
    # Adaptive mode would start "refactor … across every file" in plan; this test is about the gate in build mode.
    settings = {"vault": {"enabled": addr is not None, "addr": addr or "", "egressPolicy": policy}, "modes": {"adaptive": False}}
    (home / "settings.json").write_text(json.dumps(settings))


def claude_project(name: str) -> Path:
    cwd = project(name)
    (cwd / ".env").write_text("SECRET=do-not-leak\n")
    (cwd / "app.py").write_text("print('hi')\n")
    return cwd


async def test_claude_route(vault_addr: str) -> None:
    set_vault(vault_addr)
    lab_module.get().reset()
    cwd = claude_project("claude-route")
    task = "refactor the whole parser module across every file and run the tests"
    orchestrator_model([call("claude_code_engineer", {"task": task, "files": ["app.py"]}), text("The refactor is done and verified.")])
    server, conn = new_server(laya("claude_code"))
    created = await server.new_session(cwd=str(cwd))
    sid = created.session_id
    turn = asyncio.create_task(server.prompt(sid, [acp.text_block(task)]))
    await until(lambda: conn.waiting, "the call waits at the approval gate")
    await settle()

    decision = laya_events(conn, "decision")[0]
    check(decision["route"] == "claude_code" and decision["action"] == "tool" and decision["target_kind"] == "tool", f"Laya routes the refactor to the Claude Code tool ({decision['reason']})")
    check("claude_code_engineer" in decision["reason"] and not decision["abstained"], "…naming the tool, not abstaining")
    system_prompt = MODELS["orchestrator"].calls[0]["system_prompt"] or ""
    check("claude_code_engineer tool" in str(system_prompt), "the orchestrator's turn carries the routing hint")
    check("claude_code_engineer" in MODELS["orchestrator"].calls[0]["tools"], "the tool is in the orchestrator's kit")
    asked = conn.asked[0]
    check(asked.raw_input["task"] == task and asked.raw_input["files"] == ["app.py"], "the approval prompt shows the exact task")
    check(not (cwd / "changed.txt").exists(), "nothing has run while the prompt is open")

    conn.decide("allow")
    response = await turn
    await settle()
    check(response.stop_reason == "end_turn", "the turn completes")
    updates = tool_updates(conn, "claude_code_engineer")
    progress = [update_text(u) for u in updates if "claude:" in update_text(u)]
    check(bool(progress) and "Read app.py" in progress[0], f"the CLI's progress streams back as ACP tool updates ({progress[:1]})")
    final = [u for u in updates if type(u).__name__ == "ToolCallProgress" and getattr(u, "status", None) in ("completed", "failed")]
    check(final and final[-1].status == "completed", "the tool call ends completed")
    check((cwd / "changed.txt").read_text() == "refactored\n", "the CLI ran in the project and edited a file")
    check((cwd / ".env").read_text() == "SECRET=do-not-leak\n", ".env untouched")
    tasks = [p for m, p in conn.ext if m == "cordata/task" and p.get("source") == "claude_code_engineer"]
    check(tasks and tasks[0]["state"] == "running" and tasks[-1]["state"] == "completed", "a progress task opens and completes — the TUI's progress bar")
    states = lab_states(conn)
    seen = [row for snap in states for row in snap["pods"] if row["name"] == "claude_code_engineer"]
    check(seen and any(row["status"] == "executing" for row in seen) and seen[-1]["status"] == "idle", "the Lab shows the CLI as a pod: EXECUTING while it runs, IDLE after")
    check(any(link["from"] == "orchestrator" and link["to"] == "claude_code_engineer" for snap in states for link in snap["links"]), "…linked from the orchestrator")
    check([t["tool"] for t in seen[-1]["trace"]][:2] == ["Read", "Write"], "…its trace holds the CLI's own tool calls")
    outcome = laya_events(conn, "outcome")[-1]
    check(outcome["actual_route"] == "claude_code" and outcome["agreed"] is True and outcome["clean"] is True, "the turn is scored: the route agreed with the tool actually used")

    # What the child saw, from the stub's own report inside the Strands ToolResult the orchestrator received.
    results = [blk["toolResult"] for m in server._sessions[sid].agent.messages for blk in m["content"] if "toolResult" in blk]
    check(len(results) == 1 and results[0]["status"] == "success", "the orchestrator received exactly one ToolResult, status=success")
    check(results[0]["content"][0]["text"].startswith("Claude Code finished"), "…whose text block is the human summary")
    body = next(c["json"] for c in results[0]["content"] if "json" in c)
    report = json.loads(body["result"].split("REPORT:", 1)[1])
    check(report["https_proxy"] == vault_addr, f"the child was spawned with HTTPS_PROXY at the vault ({report['https_proxy']})")
    check(report["env_read"] == "denied" and report["env_write"] == "denied", "…and could not touch .env")
    check(not [k for k in report["env_keys"] if k.endswith(("_API_KEY", "_TOKEN")) or k.startswith("OLLAMA_")], "…and held no credential variables")
    check(body["files_changed"] == ["changed.txt"] and body["exit_code"] == 0 and body["confinement"] == "seatbelt", "the structured result carries the changed files, exit code and confinement")


async def test_gate_modes(vault_addr: str) -> None:
    set_vault(vault_addr)
    lab_module.get().reset()
    cwd = claude_project("claude-plan")
    orchestrator_model([call("claude_code_engineer", {"task": "refactor everything"}), text("ok")])
    server, conn = new_server(laya("claude_code"))
    created = await server.new_session(cwd=str(cwd))
    await server.set_session_mode(created.session_id, "plan")
    await server.prompt(created.session_id, [acp.text_block("refactor everything")])
    await settle()
    check(not conn.asked and not (cwd / "changed.txt").exists(), "plan mode: the CLI is denied without a prompt and nothing ran")

    # vault configured but unreachable → the gate refuses before any prompt, even in auto mode
    dead = socket.socket()
    dead.bind(("127.0.0.1", 0))
    port = dead.getsockname()[1]
    dead.close()
    set_vault(f"http://127.0.0.1:{port}")
    lab_module.get().reset()
    cwd = claude_project("claude-novault")
    orchestrator_model([call("claude_code_engineer", {"task": "refactor everything"}), text("ok")])
    server, conn = new_server(laya("claude_code"))
    created = await server.new_session(cwd=str(cwd))
    await server.set_session_mode(created.session_id, "auto")
    await server.prompt(created.session_id, [acp.text_block("refactor everything")])
    await settle()
    check(not conn.asked and not (cwd / "changed.txt").exists(), "vault unreachable (egressPolicy deny): refused even in auto mode, no prompt, nothing ran")
    denied = json.dumps([m for m in server._sessions[created.session_id].agent.messages])
    check("was not run" in denied and "vault" in denied.lower(), "…and the orchestrator is told why")
    set_vault(vault_addr)


async def test_trivial_task_is_not_routed(vault_addr: str) -> None:
    set_vault(vault_addr)
    lab_module.get().reset()
    cwd = claude_project("claude-trivial")
    orchestrator_model([text("renamed it")])
    server, conn = new_server(laya("claude_code", 0.97, complexity=0.1, complexity_conf=0.95))
    created = await server.new_session(cwd=str(cwd))
    await server.prompt(created.session_id, [acp.text_block("rename the variable x to count in app.py")])
    await settle()
    decision = laya_events(conn, "decision")[0]
    check(decision["route"] == "claude_code" and decision["action"] == "none" and decision["tier"] == "fast", f"a trivial task is not sent to the heavy path ({decision['reason'][:70]}…)")
    check("claude_code_engineer tool" not in str(MODELS["orchestrator"].calls[0]["system_prompt"] or ""), "…and no hint pushes the orchestrator toward it")


async def test_architect_on_prism(fake: FakeOmlx, vault_addr: str) -> None:
    set_vault(vault_addr)
    lab_module.get().reset()
    fake.events.clear()
    fake.scripts[ARCH_WIRE] = [text("PLAN: 1. split the lexer out  2. keep the parser pure  3. add tests")]
    cwd = claude_project("architect")
    orchestrator_model([text("orchestrator should not run")])
    server, conn = new_server(laya("architect", 0.97, complexity=0.5))
    created = await server.new_session(cwd=str(cwd))
    check(not fake.loaded, "oMLX starts with nothing loaded (0 loaded · 0.0 B)")
    response = await server.prompt(created.session_id, [acp.text_block("sketch a plan for splitting the lexer from the parser")])
    await settle()
    decision = laya_events(conn, "decision")[0]
    check(decision["route"] == "architect" and decision["action"] == "dispatch", "Laya routes simple architectural planning to the architect specialist")
    check(fake.events == [f"load {ARCH_WIRE}", f"chat {ARCH_WIRE}", f"unload {ARCH_WIRE}"], f"the architect ran on the local Prism model: load → completion → unload ({fake.events})")
    check(all(row["loaded"] for row in fake.chats()), "…and the model was resident when the request arrived")
    check(not fake.loaded, "memory is free again once the task completes")
    check(MODELS["orchestrator"].calls == [], "the cloud/orchestrator model was never called for the plan")
    check(not (cwd / "changed.txt").exists(), "the Claude Code CLI was not started: it is reserved for complex file execution")
    check(response.stop_reason == "end_turn", "the turn ends normally")
    snap = lab_states(conn)[-1]
    architect_pod = next(row for row in snap["pods"] if row["name"] == "architect")
    check(architect_pod["model"] == "prism-ml/Ternary-Bonsai-2-27B-mlx-2bit", f"the Lab pod shows the Prism model ({architect_pod['model']})")
    memory = [p for m, p in conn.ext if m == "cordata/memory"]
    check([p["event"] for p in memory if p["event"].startswith("lease")] == ["lease_load", "lease_unload"], "memory events announce the load and the unload")


async def test_memory_tools_and_heavy_task(fake: FakeOmlx, vault_addr: str, stub: "ollama_cli.OllamaCli") -> None:
    set_vault(vault_addr)
    lab_module.get().reset()
    fake.events.clear()
    cwd = claude_project("memory-tools")
    orchestrator_model(
        [
            call("omlx_resource_manager", {"action": "load", "model": "prism-ml/Ternary-Bonsai-2-27B-mlx-2bit"}),
            call("ollama_resource_manager", {"action": "stop", "model": "llama3:8b"}),
            call("omlx_resource_manager", {"action": "status"}),
            call("claude_code_engineer", {"task": "implement the feature across the repo"}),
            text("all done"),
        ]
    )
    server, conn = new_server(laya("code", 0.9))
    created = await server.new_session(cwd=str(cwd))
    sid = created.session_id
    turn = asyncio.create_task(server.prompt(sid, [acp.text_block("load the planner model, then implement the feature")]))
    await until(lambda: conn.waiting, "only the Claude call reaches the approval prompt")
    check([c.raw_input["task"] for c in conn.asked] == ["implement the feature across the repo"], "load / stop / status never asked; only claude_code_engineer did")
    check(fake.events == [f"load {ARCH_WIRE}"], "the Prism model is resident while the orchestrator waits on the human")
    conn.decide("allow")
    await turn
    await settle()
    check(fake.events == [f"load {ARCH_WIRE}", f"unload {ARCH_WIRE}"], f"the heavy Claude task unloaded the Prism model before spawning it ({fake.events})")
    check(not fake.loaded, "unified memory is clear of Prism weights")
    check(stub.calls and stub.calls[0][1:] == ["stop", "llama3:8b"], "ollama_resource_manager stop ran `ollama stop llama3:8b`")
    memory = [p["event"] for m, p in conn.ext if m == "cordata/memory"]
    check(memory[:2] == ["load", "stop"] and "heavy_task" in memory, f"the memory events narrate it ({memory})")
    results = [b["toolResult"] for m in server._sessions[sid].agent.messages for b in m["content"] if "toolResult" in b]
    check(len(results) == 4 and all(r["status"] == "success" for r in results), "every tool returned a Strands ToolResult with status=success")
    claude_json = next(c["json"] for c in results[-1]["content"] if "json" in c)
    check([r["name"] for r in claude_json["memory_freed"]["freed"]] == [ARCH_WIRE], "the Claude result records the Prism model it freed")


async def test_pull_and_rm_always_ask(stub: "ollama_cli.OllamaCli", vault_addr: str) -> None:
    set_vault(vault_addr)
    lab_module.get().reset()
    cwd = claude_project("pull")
    orchestrator_model(
        [
            call("ollama_resource_manager", {"action": "pull", "model": "qwen3:32b"}),
            call("ollama_resource_manager", {"action": "list"}),
            call("ollama_resource_manager", {"action": "pull", "model": "qwen3:32b"}),
            text("finished"),
        ]
    )
    server, conn = new_server(laya("code", 0.9))
    created = await server.new_session(cwd=str(cwd))
    sid = created.session_id
    await server.set_session_mode(sid, "auto")
    stub.calls.clear()
    turn = asyncio.create_task(server.prompt(sid, [acp.text_block("get the qwen model")]))
    await until(lambda: conn.waiting, "a pull asks even in auto mode")
    check(conn.asked[0].raw_input == {"action": "pull", "model": "qwen3:32b"}, "…showing exactly which model")
    conn.decide("reject")
    await until(lambda: conn.waiting, "after the list runs unasked, the second pull asks again")
    check(not any(call_[1] == "pull" for call_ in stub.calls) and any(call_[1] == "list" for call_ in stub.calls), "the rejected pull never reached the CLI; the list ran without asking")
    conn.decide("allow")
    await turn
    check([c[1:] for c in stub.calls if c[1] == "pull"] == [["pull", "qwen3:32b"]], "an approved pull runs `ollama pull qwen3:32b`, once")

    # plan mode: read-only memory calls run, a stop does not
    lab_module.get().reset()
    orchestrator_model([call("ollama_resource_manager", {"action": "status"}), call("ollama_resource_manager", {"action": "stop", "model": "x:1"}), text("planned")])
    server, conn = new_server(laya("code", 0.9))
    created = await server.new_session(cwd=str(claude_project("plan-memory")))
    await server.set_session_mode(created.session_id, "plan")
    stub.calls.clear()
    await server.prompt(created.session_id, [acp.text_block("what is loaded?")])
    check(not conn.asked and stub.calls == [], "plan mode: a status read is allowed (it touches no CLI), a stop is denied")


async def main() -> None:
    vault_addr, close_vault = listener()
    saved_env = {name: os.environ.get(name) for name in ("OMLX_PRISM_BASE_URL", op.PRISM_ENV, cct.BINARY_ENV)}
    fake_claude = ROOT / "claude"
    fake_claude.write_text(FAKE_CLAUDE)
    fake_claude.chmod(fake_claude.stat().st_mode | stat.S_IXUSR)
    saved_budget, saved_table = mb._DEFAULT, op._TABLE
    try:
        with FakeOmlx({DIGEST_WIRE: [text("digest")]}) as fake:
            stub = quiet_ollama()
            mb._DEFAULT = mb.MemoryBudget(omlx_manager.OmlxManager(fake.base_url), stub, ceiling_gb=48, reserve_gb=10)
            op._TABLE = op.LeaseTable(mb._DEFAULT)
            os.environ["OMLX_PRISM_BASE_URL"] = fake.base_url
            os.environ[op.PRISM_ENV] = "on"
            os.environ[cct.BINARY_ENV] = str(fake_claude)
            op._probe = None
            await test_claude_route(vault_addr)
            await test_gate_modes(vault_addr)
            await test_trivial_task_is_not_routed(vault_addr)
            await test_architect_on_prism(fake, vault_addr)
            await test_memory_tools_and_heavy_task(fake, vault_addr, stub)
            await test_pull_and_rm_always_ask(stub, vault_addr)
    finally:
        for name, value in saved_env.items():
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value
        mb._DEFAULT, op._TABLE = saved_budget, saved_table
        close_vault()
    print("\nPASS: Meta-Harness through ACP — Laya → claude_code_engineer (gated, vault-routed, streamed), Prism architect with lease, memory tools")


if __name__ == "__main__":
    asyncio.run(main())

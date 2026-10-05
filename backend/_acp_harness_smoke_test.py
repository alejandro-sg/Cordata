"""The agentic harness through the real ACP server object, offline.

Models are scripted (no provider needed), Laya's forward pass is replaced by a
fixed answer per test (routing has to be deterministic to assert on), and the
plugins come from a throwaway project. Everything else is the real code path:
SierraLabAcpAgent._build_session, the router, specialists as agents-as-tools,
the plugin host, checkpoints, ext methods and the event forwarding.

1. dispatch: a confident ``debugger`` route runs the specialist directly — the
   orchestrator's model is never called — and the exchange lands in the
   orchestrator's (persisted) history.
2. abstain: Laya's low_confidence flag fails closed — nothing dispatched, the
   turn runs on tiers.strong, and the model is restored afterwards.
3. plugin route: tool scoping — the turn's model sees the routed plugin's tools
   and not another plugin's; both are back afterwards.
4. checkpoints: a turn killed between cycle boundaries leaves a checkpoint; a new
   server process resumes it with /resume-turn and the pending tool runs.
5. ext methods + live events: plugins/list|disable|enable, lsp/status,
   laya/stats, heal/status; task events reach the client as _sierralab/task.
6. command outcomes: a failing shell command's p(ok) is scored and learned.

    backend/.venv/bin/python _acp_harness_smoke_test.py
"""

import asyncio
import json
import os
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).parent.resolve()
_TMP = tempfile.TemporaryDirectory()
ROOT = Path(_TMP.name)
os.environ["SIERRALAB_HOME"] = str(ROOT / "home")
os.environ["LAYA_TRIAGE_MODE"] = "route"
os.environ["LAYA_TRIAGE_LOG"] = str(ROOT / "laya.jsonl")
os.environ.setdefault("BYPASS_TOOL_CONSENT", "true")
os.environ.setdefault("SIERRALAB_REFLECT", "off")  # scripted turns are not lessons; _learning_acp_smoke_test turns it on

import acp  # noqa: E402

import acp_server  # noqa: E402
import agent_common  # noqa: E402
import laya_router  # noqa: E402
import plugin_host  # noqa: E402
import prompt_security  # noqa: E402
import settings_store  # noqa: E402
import specialists  # noqa: E402
from harness_testing import ScriptedModel, check, text, tool as call  # noqa: E402

SESSIONS = ROOT / "sessions"
SESSIONS.mkdir()
agent_common.SESSIONS_DIR = SESSIONS
acp_server.SESSIONS_DIR = SESSIONS
acp_server._INDEX_PATH = SESSIONS / "index.json"
agent_common.ensure_provider_ready = lambda: None
acp_server._fetch_available_models = lambda: []
prompt_security.laya_ready = lambda: True
_real_paths = plugin_host.mcp_config_paths
plugin_host.mcp_config_paths = lambda cwd: [p for p in _real_paths(cwd) if p[0] != "backend"]

MODELS: dict[str, ScriptedModel] = {}


def orchestrator_model(steps):
    MODELS["orchestrator"] = ScriptedModel(steps, model_id="default-model")
    agent_common.build_model = lambda model_id=None: MODELS["orchestrator"]


def specialist_model(name, steps):
    MODELS[name] = ScriptedModel(steps, model_id=f"{name}-model")


specialists._model = lambda tier, role=None: ScriptedModel([text("(unused specialist)")])
_real_build_specialist = specialists.build_specialist


def build_specialist(spec, *, interventions=None, hooks=None, model=None):
    return _real_build_specialist(spec, interventions=interventions, hooks=hooks, model=MODELS.get(spec.name) or ScriptedModel([text("idle")]))


specialists.build_specialist = build_specialist


def laya_answer(route: str, probabilities: dict, *, low=False, risk=0.0, risk_conf=0.5):
    def predict(text_, questions, **kwargs):
        if "command_ok" in questions:
            return {"answers": {"command_ok": {"noul": 0.9}}}
        conf = probabilities.get(route, 0.0)
        return {
            "answers": {
                "route": {"choice": route, "probabilities": probabilities, "answer_confidence": conf, **({"low_confidence": True} if low else {})},
                "risk": {"score": risk, "answer_confidence": risk_conf},
                "complexity": {"score": 1.0, "answer_confidence": 0.7},
            }
        }

    return predict


class Conn:
    def __init__(self) -> None:
        self.updates: list = []
        self.ext: list[tuple[str, dict]] = []

    async def session_update(self, session_id, update, **kwargs):
        self.updates.append(update)

    async def request_permission(self, session_id, tool_call, options, **kwargs):
        return acp.schema.RequestPermissionResponse(outcome=acp.schema.AllowedOutcome(outcome="selected", option_id="allow"))

    async def ext_notification(self, method, params):
        self.ext.append((method, params))


def project(name: str) -> Path:
    path = ROOT / name
    path.mkdir()
    (path / "README.md").write_text("demo\n")
    return path


_SERVERS = []


def fresh_server(predict):
    _SERVERS.append(predict)
    acp_server._ROUTER = laya_router.LayaRouter(predict=predict, calibrator=laya_router.Calibrator(ROOT / f"cal-{len(_SERVERS)}.json"))
    server = acp_server.SierraLabAcpAgent()
    conn = Conn()
    server.on_connect(conn)
    return server, conn


async def settle() -> None:
    """Notifications are scheduled with call_soon_threadsafe; let them flush."""
    for _ in range(5):
        await asyncio.sleep(0.02)


def texts(conn) -> str:
    return "".join(getattr(getattr(u, "content", None), "text", "") or "" for u in conn.updates if type(u).__name__ == "AgentMessageChunk")


def laya_events(conn, event):
    return [p for m, p in conn.ext if m == "sierralab/laya" and p.get("event") == event]


async def test_dispatch() -> None:
    cwd = project("dispatch")
    orchestrator_model([text("orchestrator should not run")])
    specialist_model("debugger", [call("file_read", {"path": "README.md", "mode": "view"}), text("ROOT CAUSE: none, README is fine")])
    server, conn = fresh_server(laya_answer("debugger", {"debugger": 0.97, "code": 0.02, "answer": 0.01}))
    created = await server.new_session(cwd=str(cwd))
    response = await server.prompt(created.session_id, [acp.text_block("fix the failing build")])
    await settle()
    check(response.stop_reason == "end_turn", "a dispatched turn ends normally")
    check(MODELS["orchestrator"].calls == [], "the orchestrator's model was never called (System 2 tokens saved)")
    check(len(MODELS["debugger"].calls) == 2 and "fix the failing build" in MODELS["debugger"].calls[0]["last_text"], "the debugger specialist ran on the user's request")
    decision = laya_events(conn, "decision")[0]
    check(decision["action"] == "dispatch" and decision["route"] == "debugger" and not decision["abstained"], f"the decision is announced ({decision['reason']})")
    starts = [u for u in conn.updates if type(u).__name__ == "ToolCallStart"]
    check([u.title for u in starts][:2] == ["debugger", "file_read"], f"the dispatch and the specialist's own tool call are visible ({[u.title for u in starts]})")
    agent = server._sessions[created.session_id].agent
    check([m["role"] for m in agent.messages] == ["user", "assistant"] and "[debugger specialist" in agent.messages[1]["content"][0]["text"], "the exchange is in the orchestrator's history")
    persisted = list((SESSIONS / f"session_{created.session_id}").rglob("message_*.json"))
    check(len(persisted) == 2, "…and in its session files")
    outcome = laya_events(conn, "outcome")
    check(outcome and outcome[-1]["agreed"] is True and outcome[-1]["clean"] is True, "the specialist turn is scored (agreed, clean)")
    listed = await server.list_sessions(cwd=str(cwd))
    check([s.session_id for s in listed.sessions] == [created.session_id] and listed.sessions[0].title == "fix the failing build", "session/list finds it with its title")


async def test_abstain_escalates() -> None:
    cwd = project("abstain")
    (cwd / ".sierralab").mkdir()
    (cwd / ".sierralab" / "settings.json").write_text(json.dumps({"tiers": {"strong": "frontier-model"}}))
    seen_models = []
    orchestrator_model([lambda m, s, t: (seen_models.append(MODELS["orchestrator"].config["model_id"]), text("answered"))[1]])
    server, conn = fresh_server(laya_answer("architect", {"architect": 0.4, "code": 0.35, "answer": 0.25}, low=True))
    created = await server.new_session(cwd=str(cwd))
    await server.prompt(created.session_id, [acp.text_block("hmm, something about the design?")])
    await settle()
    decision = laya_events(conn, "decision")[0]
    check(decision["abstained"] and decision["action"] == "escalate" and decision["escalate_model"] == "frontier-model", f"low confidence fails closed: escalate ({decision['reason']})")
    check(seen_models == ["frontier-model"], "the turn ran on the frontier tier")
    check(MODELS["orchestrator"].config["model_id"] == "default-model", "…and the default model is restored afterwards")
    check("architect" not in MODELS, "nothing was dispatched on a guess")


async def test_plugin_scope() -> None:
    cwd = project("plugins")
    echo = [sys.executable, str(HERE / "fixtures" / "echo_mcp_server.py")]
    (cwd / ".mcp.json").write_text(
        json.dumps(
            {
                "mcpServers": {
                    "echo": {"command": echo[0], "args": echo[1:], "description": "echoes text back"},
                    "other": {"command": echo[0], "args": echo[1:], "prefix": "other"},
                }
            }
        )
    )
    settings_store.trust_project(cwd)
    orchestrator_model([call("echo", {"text": "hi"}), text("done")])
    server, conn = fresh_server(laya_answer("plugin_echo", {"plugin_echo": 0.96, "code": 0.04}))
    created = await server.new_session(cwd=str(cwd))
    state = server._sessions[created.session_id]
    for _ in range(100):
        if {"echo", "other_echo"} <= set(state.agent.tool_names):
            break
        await asyncio.sleep(0.1)
    check({"echo", "other_echo"} <= set(state.agent.tool_names), "both project plugins started in the background and attached")
    await server.prompt(created.session_id, [acp.text_block("echo hi through the echo server")])
    await settle()
    first_call_tools = MODELS["orchestrator"].calls[0]["tools"]
    check("echo" in first_call_tools and "other_echo" not in first_call_tools, "the routed plugin's tools are offered, the other plugin's are not")
    check({"echo", "other_echo"} <= set(state.agent.tool_names), "scoping is undone after the turn")
    decision = laya_events(conn, "decision")[0]
    check(decision["action"] == "scope", "the decision says so")

    listed = await server.ext_method("_sierralab/plugins/list", {"sessionId": created.session_id})
    names = {row["name"]: row["status"] for row in listed["plugins"]}
    check(names.get("echo") == "running" and names.get("memory") == "running" and names.get("lsp") == "running", f"plugins/list shows MCP and built-in plugins ({names})")
    row = await server.ext_method("_sierralab/plugins/disable", {"sessionId": created.session_id, "name": "echo"})
    check(row["status"] == "disabled" and "echo" not in state.agent.tool_names, "plugins/disable hot-removes from the live session")
    row = await server.ext_method("_sierralab/plugins/enable", {"sessionId": created.session_id, "name": "echo"})
    check(row["status"] == "running" and "echo" in state.agent.tool_names, "plugins/enable hot-adds again")
    tasks = [p for m, p in conn.ext if m == "sierralab/task"]
    check(any(p["title"] == "plugin echo" and p["state"] == "completed" for p in tasks), "plugin start-up is reported as task progress")
    lsp = await server.ext_method("_sierralab/lsp/status", {"sessionId": created.session_id})
    check(lsp["enabled"] is True and isinstance(lsp["servers"], list), "lsp/status answers")
    for row in state.plugins.status():
        if row["status"] == "running":
            state.plugins.disable(row["name"])


async def test_checkpoint_resume() -> None:
    cwd = project("checkpoint")
    ran = []

    from strands import tool

    @tool
    def slow_step() -> str:
        """A step that must not be lost."""
        ran.append(1)
        return "step done"

    original_static = agent_common._STATIC_TOOLS
    agent_common._STATIC_TOOLS = [*original_static, slow_step]
    try:
        orchestrator_model([call("slow_step", {}), text("finished after resume")])
        server, conn = fresh_server(laya_answer("code", {"code": 0.5, "answer": 0.5}))
        created = await server.new_session(cwd=str(cwd))
        sid = created.session_id
        agent = server._sessions[sid].agent
        # Simulate a crash right after the model asked for the tool: run the
        # stream only up to the first checkpoint, write it the way _stream does,
        # and abandon this server.
        result = None
        async for event in agent.stream_async("do the slow step"):
            if "result" in event:
                result = event["result"]
        check(result.stop_reason == "checkpoint" and result.checkpoint.position == "after_model", "the turn paused at the after_model checkpoint")
        acp_server._checkpoint_path(sid).write_text(json.dumps({"checkpoint": result.checkpoint.to_dict()}))
        check(ran == [], "the tool had not run when the process 'died'")

        orchestrator_model([text("finished after resume")])
        server2, conn2 = fresh_server(laya_answer("code", {"code": 0.5, "answer": 0.5}))
        await server2.load_session(cwd=str(cwd), session_id=sid)
        check("interrupted at a checkpoint (after_model, cycle 0)" in texts(conn2), "load_session tells the user a turn can be resumed")
        response = await server2.prompt(sid, [acp.text_block(acp_server.RESUME_TURN_MARKER)])
        await settle()
        check(response.stop_reason == "end_turn" and ran == [1], "/resume-turn runs the pending tool in the new process")
        check("finished after resume" in texts(conn2), "…and the turn completes")
        check(not acp_server._checkpoint_path(sid).exists(), "the checkpoint is cleared once the turn ends")
    finally:
        agent_common._STATIC_TOOLS = original_static


async def test_command_outcomes_and_stats() -> None:
    cwd = project("commands")
    orchestrator_model([call("shell", {"command": "exit 3"}), text("that failed")])
    server, conn = fresh_server(laya_answer("code", {"code": 0.95, "answer": 0.05}))
    created = await server.new_session(cwd=str(cwd))
    await server.set_session_mode(created.session_id, "auto")
    heal_off = acp_server.HealConfig(enabled=False)
    server._sessions[created.session_id].heal._config = lambda: heal_off
    await server.prompt(created.session_id, [acp.text_block("run the thing")])
    await settle()
    predicted = laya_events(conn, "command")
    outcome = laya_events(conn, "command_outcome")
    check(predicted and predicted[0]["p_ok"] == 0.9, "Laya predicted the command's success before it ran")
    check(outcome and outcome[0]["ok"] is False and outcome[0]["log_loss"] > 2.0, f"the stderr failure is scored with log loss ({outcome[0]})")
    turn = laya_events(conn, "outcome")[-1]
    check(turn["clean"] is False and turn["agreed"] is True, "an unresolved failure makes the turn unclean (route still agreed)")
    stats = await server.ext_method("_sierralab/laya/stats", {"sessionId": created.session_id})
    check(stats["stats"]["command_ok"]["n"] == 1 and stats["stats"]["route"]["n"] == 1, f"laya/stats reports both learners ({stats['summary']!r})")
    check(any(k.startswith("command_ok/") for k in stats["weights"]), "the failure moved a decision weight")
    heal = await server.ext_method("_sierralab/heal/status", {"sessionId": created.session_id})
    check(heal["config"]["enabled"] is False, "heal/status answers")
    try:
        await server.ext_method("_sierralab/nope", {})
        raise AssertionError("unknown ext method accepted")
    except acp.RequestError:
        check(True, "unknown ext methods are method-not-found")


async def main() -> None:
    await test_dispatch()
    await test_abstain_escalates()
    await test_plugin_scope()
    await test_checkpoint_resume()
    await test_command_outcomes_and_stats()
    print("\nPASS: ACP harness — dispatch, fail-closed escalation, plugin scoping + hot swap, checkpoint resume, outcome learning")


if __name__ == "__main__":
    asyncio.run(main())

"""Phase 5, deterministic: the whole pipeline over real HTTP, no cloud.

The real ``CordataAcpAgent``, the real ``ollama_cloud`` provider (registry,
ModelRouter, reasoning throttle, endpoint guard), the real specialists and the real
Lab hooks — talking to ``fake_ollama.FakeOllama`` on a loopback port, which answers
each model from a script and records every request. Only Laya's forward pass is
scripted (so "destructive" is a given, not a guess about a 322M classifier).

Scenario: the Orchestrator (glm-5.3-flash, reasoning_effort=low) delegates to the
Debugger (kimi-k2.7-code, reasoning_effort=max), which wants to ``rm -rf build``.
Laya rates the turn destructive, so the call parks at the HITL station; the human
approves. Then a model swap through the picker, and an E-Stop while parked.

    backend/.venv/bin/python _e2e_lab_smoke_test.py
"""

import asyncio
import os
import tempfile
from pathlib import Path

_TMP = tempfile.TemporaryDirectory()
ROOT = Path(_TMP.name)
os.environ["CORDATA_HOME"] = str(ROOT / "home")
os.environ["LAYA_TRIAGE_MODE"] = "route"
os.environ["LAYA_TRIAGE_LOG"] = str(ROOT / "laya.jsonl")
os.environ.setdefault("BYPASS_TOOL_CONSENT", "true")

import acp  # noqa: E402

import acp_server  # noqa: E402
import agent_common  # noqa: E402
import lab as lab_module  # noqa: E402
import laya_router  # noqa: E402
import ollama_cloud  # noqa: E402
import plugin_host  # noqa: E402
import prompt_security  # noqa: E402
import state_store  # noqa: E402
from fake_ollama import FakeOllama  # noqa: E402
from harness_testing import check, text, tool  # noqa: E402

SESSIONS = ROOT / "sessions"
SESSIONS.mkdir()
agent_common.SESSIONS_DIR = SESSIONS
acp_server.SESSIONS_DIR = SESSIONS
acp_server._INDEX_PATH = SESSIONS / "index.json"
acp_server._fetch_available_models = lambda: []  # the oMLX list; unused by this provider
prompt_security.laya_ready = lambda: True
_real_paths = plugin_host.mcp_config_paths
plugin_host.mcp_config_paths = lambda cwd: [p for p in _real_paths(cwd) if p[0] != "backend"]

GLM = "glm-5.3-flash:cloud"
KIMI = "kimi-k2.7-code:cloud"


def destructive_laya():
    def predict(text_, questions, **kwargs):
        if "command_ok" in questions:
            return {"answers": {"command_ok": {"noul": 0.9}}}
        return {
            "answers": {
                "route": {"choice": "code", "probabilities": {"code": 0.95, "answer": 0.05}, "answer_confidence": 0.95},
                "risk": {"score": 2, "answer_confidence": 0.9},  # destructive, confidently
                "complexity": {"score": 1.0, "answer_confidence": 0.7},
            }
        }

    return predict


class HitlConn:
    """A client that parks every permission request until the test decides."""

    def __init__(self) -> None:
        self.updates: list = []
        self.ext: list[tuple[str, dict]] = []
        self.waiting: list[asyncio.Future] = []
        self.asked: list = []

    async def session_update(self, session_id, update, **kwargs):
        self.updates.append(update)

    async def ext_notification(self, method, params):
        self.ext.append((method, params))

    async def request_permission(self, session_id, tool_call, options, **kwargs):
        future = asyncio.get_running_loop().create_future()
        self.asked.append(tool_call)
        self.waiting.append(future)
        option_id = await future
        return acp.schema.RequestPermissionResponse(outcome=acp.schema.AllowedOutcome(outcome="selected", option_id=option_id))

    def decide(self, option_id: str) -> None:
        self.waiting.pop(0).set_result(option_id)

    def states(self) -> list[dict]:
        return [params for method, params in self.ext if method == "cordata/lab"]


async def until(condition, label: str, timeout: float = 10.0) -> None:
    for _ in range(int(timeout / 0.02)):
        if condition():
            return
        await asyncio.sleep(0.02)
    raise AssertionError(f"FAIL (timeout): {label}")


def pod(snapshot: dict, name: str) -> dict:
    return next(row for row in snapshot["pods"] if row["name"] == name)


def subsequence(wanted: list[str], seen: list[str]) -> bool:
    it = iter(seen)
    return all(item in it for item in wanted)


def statuses(conn: HitlConn, name: str) -> list[str]:
    out: list[str] = []
    for state in conn.states():
        row = next((p for p in state["pods"] if p["name"] == name), None)
        if row and (not out or out[-1] != row["status"]):
            out.append(row["status"])
    return out


async def scenario(fake: FakeOllama) -> None:
    project = ROOT / "project"
    (project / "build").mkdir(parents=True)
    (project / "build" / "stale.o").write_text("x")
    (project / "README.md").write_text("demo\n")

    acp_server._ROUTER = laya_router.LayaRouter(predict=destructive_laya(), calibrator=laya_router.Calibrator(ROOT / "cal.json"))
    server = acp_server.CordataAcpAgent()
    conn = HitlConn()
    server.on_connect(conn)
    lab_module.get().reset()

    created = await server.new_session(cwd=str(project))
    sid = created.session_id
    check("GET /api/version" in fake.paths(), "session start probed the daemon (ensure_provider_ready, real)")
    check(isinstance(server._sessions[sid].agent.model, ollama_cloud.OllamaCloudModel), "the orchestrator runs on the ollama_cloud provider")

    # ----------------------------------------------- 1. delegate, park at the HITL station
    turn = asyncio.create_task(server.prompt(sid, [acp.text_block("fix the failing build, then delete the stale build directory")]))
    await until(lambda: conn.waiting, "the destructive call parks at the HITL station")
    await asyncio.sleep(0.2)  # let the Lab's trailing snapshot flush
    snap = conn.states()[-1]

    orchestrator, debugger = pod(snap, "orchestrator"), pod(snap, "debugger")
    check((orchestrator["model"], orchestrator["effort"]) == ("glm-5.3-flash", "low"), f"Orchestrator pod: {orchestrator['model']} @ {orchestrator['effort']}")
    check((debugger["model"], debugger["effort"]) == ("kimi-k2.7-code", "max"), f"Debugger pod: {debugger['model']} @ {debugger['effort']}")
    check(debugger["status"] == "contained", "the Debugger is CONTAINED at the station")
    check(orchestrator["status"] == "executing", "the Orchestrator is EXECUTING (its tool call is the Debugger)")
    link = next(l for l in snap["links"] if l["to"] == "debugger")
    check(link["from"] == "orchestrator" and link["active"] and link["kind"] == "delegation", "the A2A delegation vector orchestrator→debugger is live")
    threat = snap["threat"]
    check(threat["pod"] == "debugger" and threat["tool"] == "shell" and threat["band"] == "destructive" and threat["score"] >= 0.9 and threat["confidence"] >= 0.85, f"Laya threat gauge: {threat['score']:.2f} / confidence {threat['confidence']:.2f} on shell@debugger")
    staged = [row for row in snap["hitl"] if row["status"] == "pending"]
    check(len(staged) == 1 and staged[0]["pod"] == "debugger" and "rm -rf build" in staged[0]["input"], "the HITL row names the pod and the exact command")
    check((project / "build").exists(), "nothing was deleted while it waits")

    glm_first, kimi_first = fake.chats(GLM)[0], fake.chats(KIMI)[0]
    check(glm_first["reasoning_effort"] == "low" and kimi_first["reasoning_effort"] == "max", "on the wire: glm carried low, kimi carried max")
    check(all(r["auth"] == "Bearer ollama" for r in fake.requests if r["path"] == "/v1/chat/completions"), "every chat request carried the dummy bearer key")

    conn.decide("allow")
    await turn
    await asyncio.sleep(0.3)
    final = conn.states()[-1]
    check(not (project / "build").exists(), "[Y] Approve & Execute: the command ran")
    check(all(p["status"] == "idle" for p in final["pods"]), f"every pod is IDLE again ({[p['status'] for p in final['pods']]})")
    check([row["status"] for row in final["hitl"]] == ["approved"], "the station records the approval")
    check(next(l for l in final["links"] if l["to"] == "debugger")["active"] is False, "the delegation vector closed")
    check(len(fake.chats(GLM)) == 2 and len(fake.chats(KIMI)) == 2, "two model calls each (delegate/answer, tool/answer)")
    check({r["body"]["reasoning_effort"] for r in fake.requests if r["path"] == "/v1/chat/completions" and r["body"]["model"] == KIMI} == {"max"}, "the Debugger stayed at max for every call")
    check(subsequence(["synthesizing", "executing", "contained", "executing", "synthesizing", "idle"], statuses(conn, "debugger")), f"Debugger state transitions: {statuses(conn, 'debugger')}")
    check(subsequence(["synthesizing", "executing", "synthesizing", "idle"], statuses(conn, "orchestrator")), f"Orchestrator state transitions: {statuses(conn, 'orchestrator')}")
    ledger = state_store.recent_events(kind="intervention", session_id=sid)
    check(any(e["payload"].get("outcome") == "approved" for e in ledger), "the approval is in the intervention ledger")

    # ----------------------------------------------- 2. swap the orchestrator's model from the picker
    options = await server.set_config_option("model", sid, KIMI)
    model_option = next(o for o in options.config_options if o.id == "model")
    offered = [choice.value for choice in model_option.options]
    check(offered == [GLM, KIMI, "deepseek-v4.1-flash:cloud"] and model_option.current_value == KIMI, f"the picker offers the cloud tags only and shows the swap ({offered})")
    fake.scripts.setdefault(KIMI, []).append(text("swapped and answering"))
    before = len(fake.chats())
    await server.prompt(sid, [acp.text_block("say hi")])
    await asyncio.sleep(0.3)
    swapped = fake.chats()[before]
    check(swapped["model"] == KIMI and swapped["reasoning_effort"] == "low", "the next orchestrator call went to kimi, still on the orchestrator's low budget")
    check(pod(conn.states()[-1], "orchestrator")["model"] == "kimi-k2.7-code", "the Lab shows the swap")
    refused = await server.set_config_option("model", sid, "laguna-xs-2.1")
    check(next(o for o in refused.config_options if o.id == "model").current_value == KIMI, "a local-only tag that is not installed is refused, not 404'd later")

    # ----------------------------------------------- 3. E-Stop while parked at the station
    (project / "build").mkdir()
    fake.scripts[KIMI] = [tool("debugger", {"input": "delete the build dir again"})]  # the orchestrator is on kimi now
    fake.scripts.setdefault(KIMI, []).append(tool("shell", {"command": "rm -rf build"}))  # …and so is the debugger's first step
    turn = asyncio.create_task(server.prompt(sid, [acp.text_block("clean up once more")]))
    await until(lambda: conn.waiting, "a second destructive call parks")
    chats_before = len(fake.chats())
    reply = await server.ext_method("_cordata/lab/isolate", {"sessionId": sid, "pod": "orchestrator"})
    check(reply["isolated"] and set(reply["also"]) == {"architect", "debugger"}, f"E-Stop reply: {reply}")
    conn.decide("reject")
    await turn
    await asyncio.sleep(0.3)
    check((project / "build").exists(), "the isolated agent's call never ran")
    check(len(fake.chats()) == chats_before, "no model request left the process after the E-Stop")
    check(all(p["status"] == "isolated" for p in conn.states()[-1]["pods"]), "every pod reads ISOLATED")

    # ----------------------------------------------- 4. nothing else ever touched the daemon
    paths = set(fake.paths())
    check({p for p in paths if p.startswith("POST")} == {"POST /v1/chat/completions"}, "the only write the daemon ever saw was chat completions — never /api/pull or /api/chat")
    check(paths <= {"GET /api/version", "GET /api/tags", "POST /v1/chat/completions"}, f"…plus read-only version/installed-model probes ({sorted(paths)})")


async def main() -> None:
    scripts = {
        GLM: [tool("debugger", {"input": "make the build pass, then remove the stale build directory"}), text("Delegated to the debugger; the stale build directory is gone.")],
        KIMI: [tool("shell", {"command": "rm -rf build"}), text("ROOT CAUSE: a stale build directory. FIX: removed it. VERIFIED: it is gone.")],
    }
    with FakeOllama(scripts) as fake:
        os.environ["OLLAMA_BASE_URL"] = fake.base_url
        agent_common.ACTIVE_PROVIDER = "ollama_cloud"
        agent_common.OLLAMA_MODEL_ID = ""
        await scenario(fake)
    print("\nALL OK")


if __name__ == "__main__":
    asyncio.run(main())

"""Phase 5, live: the Lab pipeline against the signed-in local Ollama daemon, real
cloud models, and the real Laya classifier (needs network, `ollama signin`, a few
thousand cloud tokens; not part of the offline suite).

The Orchestrator (glm-5.3-flash, reasoning_effort=low) is asked to hand a
destructive shell command to the Debugger specialist (kimi-k2.7-code,
reasoning_effort=max). Whatever the models decide, the harness has to hold:
the call parks at the HITL station (build mode asks before `shell`), the pods and
efforts on screen match what went on the wire, and nothing runs until a human says
so. The only thing scripted is the human.

    backend/.venv/bin/python _e2e_lab_live_test.py
"""

import asyncio
import os
import subprocess
import sys
import tempfile
from pathlib import Path

_TMP = tempfile.TemporaryDirectory()
ROOT = Path(_TMP.name)
os.environ["CORDATA_HOME"] = str(ROOT / "home")
os.environ["LAYA_TRIAGE_LOG"] = str(ROOT / "laya.jsonl")
os.environ.setdefault("BYPASS_TOOL_CONSENT", "true")
os.environ.pop("OLLAMA_BASE_URL", None)  # the real daemon, http://localhost:11434/v1

import acp  # noqa: E402

import acp_server  # noqa: E402
import agent_common  # noqa: E402
import lab as lab_module  # noqa: E402
import ollama_cloud  # noqa: E402
import plugin_host  # noqa: E402
from harness_testing import check  # noqa: E402

SESSIONS = ROOT / "sessions"
SESSIONS.mkdir()
agent_common.SESSIONS_DIR = SESSIONS
acp_server.SESSIONS_DIR = SESSIONS
acp_server._INDEX_PATH = SESSIONS / "index.json"
acp_server._fetch_available_models = lambda: []
_real_paths = plugin_host.mcp_config_paths
plugin_host.mcp_config_paths = lambda cwd: [p for p in _real_paths(cwd) if p[0] != "backend"]


def ollama_list() -> list[str]:
    out = subprocess.run(["ollama", "list"], capture_output=True, text=True, check=True).stdout
    return sorted(line.split()[0] for line in out.splitlines()[1:] if line.strip())


class HumanConn:
    """Records everything; approves when told to."""

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

    def states(self) -> list[dict]:
        return [params for method, params in self.ext if method == "cordata/lab"]

    def text(self) -> str:
        return "".join(getattr(getattr(u, "content", None), "text", "") or "" for u in self.updates if type(u).__name__ == "AgentMessageChunk")


async def main() -> None:
    problem = ollama_cloud.daemon_problem()
    if problem:
        print(f"SKIP: {problem}")
        sys.exit(2)
    before = ollama_list()
    print(f"local models before: {before or 'none'}")

    project = ROOT / "project"
    (project / "build").mkdir(parents=True)
    (project / "build" / "stale.o").write_text("x")

    # Laya's router would otherwise (rightly) hand a prompt about "the debugger" straight
    # to the Debugger specialist, skipping the Orchestrator — the dispatch path. This
    # run is about the Orchestrator delegating, so Laya may hint but not dispatch.
    (ROOT / "home").mkdir(parents=True, exist_ok=True)
    (ROOT / "home" / "settings.json").write_text('{"laya": {"dispatch": "hint"}}')
    agent_common.ACTIVE_PROVIDER = "ollama_cloud"
    agent_common.OLLAMA_MODEL_ID = ""
    sent: list[dict] = []
    ollama_cloud.subscribe_requests(sent.append)

    server = acp_server.CordataAcpAgent()
    conn = HumanConn()
    server.on_connect(conn)
    lab_module.get().reset()
    created = await server.new_session(cwd=str(project))
    sid = created.session_id

    prompt = (
        "You are the orchestrator. Do not run commands yourself. Call your `debugger` tool and tell it to run exactly this "
        "shell command in the project directory: rm -rf build   Then report what happened in one sentence."
    )
    turn = asyncio.create_task(server.prompt(sid, [acp.text_block(prompt)]))

    approved = False
    waited = 0.0
    while not turn.done() and waited < 240:
        if conn.waiting:
            await asyncio.sleep(0.4)  # let the Lab's trailing snapshot land, as a person looking at the screen would
            snap = conn.states()[-1]
            staged = [row for row in snap["hitl"] if row["status"] == "pending"]
            threat = snap["threat"]
            pods = {p["name"]: (p["status"], p["model"], p["effort"]) for p in snap["pods"]}
            print(f"\nHITL STATION  pod={staged[0]['pod'] if staged else '?'}  tool={staged[0]['tool'] if staged else '?'}  input={staged[0]['input'][:70] if staged else '?'!r}")
            print(f"  pods: {pods}")
            print(f"  laya threat: {threat}")
            check(bool(staged), "the pending call is on the HITL station")
            check((project / "build").exists(), "nothing was deleted while it waited")
            conn.waiting.pop(0).set_result("allow")
            approved = True
        await asyncio.sleep(0.2)
        waited += 0.2
    response = await asyncio.wait_for(turn, 120)
    await asyncio.sleep(0.4)

    print(f"\nreply: {conn.text().strip()[:300]!r}")
    final = conn.states()[-1]
    models = {(r["alias"], r["effort"]) for r in sent}
    print(f"requests carried (model, effort): {sorted(models)}  ({len(sent)} requests)")
    check(response.stop_reason in ("end_turn", "max_tokens"), f"the turn ended ({response.stop_reason})")
    check(("glm-5.3-flash", "low") in models, "the Orchestrator ran on glm-5.3-flash @ low")
    check(approved, "the destructive call reached the HITL station (it did not run unasked)")
    check(("kimi-k2.7-code", "max") in models, "the Debugger ran on kimi-k2.7-code @ max")
    check(not (project / "build").exists(), "after [Y] the command ran and the directory is gone")
    check(all(p["status"] == "idle" for p in final["pods"]), "every pod is IDLE at the end")
    link = next((l for l in final["links"] if l["to"] == "debugger"), None)
    check(link is not None and link["count"] >= 1, "the orchestrator→debugger delegation was recorded")
    seen_models = {(p["name"], p["model"], p["effort"]) for state in conn.states() for p in state["pods"] if p["model"]}
    check(("orchestrator", "glm-5.3-flash", "low") in seen_models and ("debugger", "kimi-k2.7-code", "max") in seen_models, f"the Lab showed both models and efforts ({sorted(seen_models)})")
    check(ollama_list() == before, "`ollama list` unchanged — no weights pulled")
    print("\nLIVE OK")


if __name__ == "__main__":
    asyncio.run(main())

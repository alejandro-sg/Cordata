"""LIVE: the Phase 11/12 verification gate against the configured real model (costs cloud tokens; not in CI).

The same three scenarios the hermetic suites prove with scripted models, with a real model choosing what to do:

1. **LSP & search** — "find every reference to build_widget": which tools does the model reach for?
2. **Self-correction** — the model is told to write a function with a deliberate syntax error through `apply_patch`;
   preflight refuses it before anyone is asked, the model fixes it, and only then is the person asked.
3. **Rejection learning** — turn 1: the person rejects the model's debug-output change with a note; the ReflectionAgent
   (a real, tool-less model call) states the rule. Turn 2, a new session: a similar request — does the proposal follow it?

Runs the real ACP server object in-process: no daemon, no Discord (``CORDATA_DISCORD=off``), Laya off (no local
weights), a throwaway ``CORDATA_HOME`` (nothing written to ``~/.cordata``), ``context.maxSteps`` 10 per turn.
Prints what happened rather than asserting a real model's choices; exits non-zero only on a harness failure.

    backend/.venv/bin/python _phase12_gate_live_test.py
"""

import asyncio
import json
import os
import sys
import tempfile
import time
from pathlib import Path

_TMP = tempfile.TemporaryDirectory(prefix="gate-live-")
ROOT = Path(_TMP.name)
os.environ["CORDATA_HOME"] = str(ROOT / "home")
os.environ["CORDATA_STATE_DB"] = str(ROOT / "state.db")
os.environ["CORDATA_DISCORD"] = "off"
os.environ["LAYA_TRIAGE_MODE"] = "off"
os.environ["CORDATA_PRISM"] = "off"
os.environ["CORDATA_REFLECT"] = "on"
(ROOT / "home").mkdir()
(ROOT / "home" / "settings.json").write_text(json.dumps({"context.maxSteps": 10, "lsp.enabled": True}))

import provider_settings  # noqa: F401,E402

import acp  # noqa: E402

import acp_server  # noqa: E402
import rejection_memory as rm  # noqa: E402


class Door:
    """A person at a TUI: answers each approval with ``answers.pop(0)`` (allow / reject + note)."""

    def __init__(self, answers):
        self.answers = list(answers)
        self.asked: list[dict] = []
        self.text: list[str] = []

    async def session_update(self, session_id, update, **kwargs):
        content = getattr(update, "content", None)
        if type(update).__name__ == "AgentMessageChunk" and getattr(content, "text", None):
            self.text.append(content.text)

    async def request_permission(self, session_id, tool_call, options, **kwargs):
        raw = getattr(tool_call, "raw_input", None) or {}
        self.asked.append(raw)
        answer = self.answers.pop(0) if self.answers else ("allow", "")
        if answer[0] == "reject":
            return acp.schema.RequestPermissionResponse(outcome=acp.schema.AllowedOutcome(outcome="selected", option_id="reject"), field_meta={"cordata": {"decidedBy": "gui", "note": answer[1]}})
        return acp.schema.RequestPermissionResponse(outcome=acp.schema.AllowedOutcome(outcome="selected", option_id="allow"))

    async def ext_notification(self, method, params):
        pass


def tool_calls(agent) -> list[tuple[str, dict]]:
    out = []
    for message in agent.messages:
        for block in message.get("content") or []:
            if isinstance(block, dict) and "toolUse" in block:
                out.append((block["toolUse"]["name"], block["toolUse"].get("input") or {}))
    return out


def results(agent) -> list[str]:
    out = []
    for message in agent.messages:
        for block in message.get("content") or []:
            if isinstance(block, dict) and "toolResult" in block:
                out.append(" ".join(c.get("text", "") for c in block["toolResult"].get("content") or [] if isinstance(c, dict))[:300])
    return out


async def turn(cwd: Path, prompt: str, answers) -> tuple[Door, object, float]:
    server = acp_server.CordataAcpAgent()
    door = Door(answers)
    server.on_connect(door)
    sid = (await server.new_session(cwd=str(cwd))).session_id
    host = server._sessions[sid].plugins
    for _ in range(100):  # the built-in LSP tools start in the background
        if all(r["status"] == "running" for r in host.status() if r["transport"] == "builtin"):
            break
        await asyncio.sleep(0.05)
    started = time.monotonic()
    await asyncio.wait_for(server.prompt(sid, [acp.text_block(prompt)]), 300)
    return door, server._sessions[sid].agent, time.monotonic() - started


def project(name: str, files: dict[str, str]) -> Path:
    path = ROOT / name
    for rel, body in files.items():
        (path / rel).parent.mkdir(parents=True, exist_ok=True)
        (path / rel).write_text(body)
    return path


async def gate_1() -> dict:
    cwd = project("g1", {
        "pkg/__init__.py": "",
        "pkg/widgets.py": "def build_widget(size):\n    return {'size': size}\n\n\nclass Factory:\n    def make(self):\n        return build_widget(3)\n",
        "app.py": "from pkg.widgets import build_widget\n\nW = build_widget(1)\n",
        "pyproject.toml": "[project]\nname = 'g1'\nversion = '0'\n",
        **{f"pkg/filler_{i}.py": "".join(f"def f{i}_{j}():\n    return {j}\n\n" for j in range(200)) for i in range(3)},
    })
    door, agent, took = await turn(cwd, "Find every place `build_widget` is defined or referenced in this project. Don't change anything; just list them.", [])
    calls = tool_calls(agent)
    names = [n for n, _ in calls]
    whole_reads = [i.get("path") for n, i in calls if n == "file_read" and str(i.get("mode") or "view") == "view"]
    return {"tools": names, "inputs": [i for _, i in calls], "results": [r[:240] for r in results(agent)], "whole_file_reads": whole_reads, "answer": "".join(door.text)[-400:], "seconds": round(took, 1)}


async def gate_2() -> dict:
    cwd = project("g2", {"calc.py": "def add(a, b):\n    return a + b\n"})
    prompt = (
        "This is a test of the harness's preflight check. Use apply_patch to add a function `mul(a, b)` to calc.py, and on "
        "your FIRST attempt write it with a deliberate syntax error: `def mul(a, b) return a * b` (no colon, one line). "
        "When the tool refuses it, fix the patch and try again."
    )
    door, agent, took = await turn(cwd, prompt, [("allow", "")])
    calls = [n for n, _ in tool_calls(agent)]
    refused = [r for r in results(agent) if "[preflight]" in r]
    return {
        "tools": calls,
        "preflight_refusals": len(refused),
        "first_refusal": refused[0][:220] if refused else None,
        "approvals_asked": len(door.asked),
        "asked_patch_parses": all(_parses(a.get("patch", "")) for a in door.asked),
        "calc.py": (cwd / "calc.py").read_text(),
        "seconds": round(took, 1),
    }


def _parses(patch: str) -> bool:
    added = "\n".join(line[1:] for line in patch.splitlines() if line.startswith("+") and not line.startswith("+++"))
    try:
        compile(added, "x", "exec")
        return True
    except SyntaxError:
        return False


async def gate_3() -> dict:
    cwd = project("g3", {
        "app.py": "def handle(order):\n    return order['total']\n",
        "report.py": "def summarize(orders):\n    return sum(o['total'] for o in orders)\n",
    })
    door1, agent1, took1 = await turn(cwd, "Add debug output to handle() in app.py so I can see each order as it is handled.", [("reject", "no print() debugging in this project — use the logging module (logger.debug)")])
    first = door1.asked[0] if door1.asked else {}
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline and not rm.rules(str(cwd)) and any(r.get("status") == "pending" for r in rm.records(str(cwd))):
        await asyncio.sleep(0.5)
    rules = rm.rules(str(cwd))
    door2, agent2, took2 = await turn(cwd, "Add debug output to summarize() in report.py so I can see the orders it sums.", [("allow", "")])
    second = door2.asked[0] if door2.asked else {}
    text2 = json.dumps(second)
    return {
        "turn1_proposed": json.dumps(first)[:300],
        "rules": [(r["rule"], r["basis"]) for r in rules],
        "records": [(r["status"], r.get("rule")) for r in rm.records(str(cwd))],
        "turn2_proposed": text2[:400],
        "turn2_uses_print": "print(" in text2,
        "turn2_uses_logging": "logging" in text2 or "logger" in text2 or "log.debug" in text2,
        "report.py": (cwd / "report.py").read_text(),
        "seconds": round(took1 + took2, 1),
    }


async def main() -> None:
    which = sys.argv[1:] or ["1", "2", "3"]
    out = {}
    for name, gate in (("1", gate_1), ("2", gate_2), ("3", gate_3)):
        if name in which:
            print(f"\n=== gate {name} ===", flush=True)
            out[name] = await gate()
            print(json.dumps(out[name], indent=2), flush=True)


if __name__ == "__main__":
    asyncio.run(main())

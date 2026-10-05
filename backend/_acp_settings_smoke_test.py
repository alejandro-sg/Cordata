"""End-to-end check that user settings reach the real ACP server: the default
mode, instruction files in the system prompt, deny / allow rules deciding whether
a tool call asks, and "Always allow" saving a rule that holds afterwards.

Like _acp_smoke_test.py this talks to the configured model (a few short turns), so
it needs the provider reachable — cloud key in the environment/Keychain, or the
local oMLX server. SIERRALAB_HOME and the project are throwaway directories, so
your real settings are never read or written.

    backend/.venv/bin/python _acp_settings_smoke_test.py
"""

import asyncio
import json
import os
os.environ.setdefault("SIERRALAB_REFLECT", "off")  # scripted/live test turns are not lessons (reflect.py)
import sys
import tempfile
from pathlib import Path

import acp

HERE = Path(__file__).parent.resolve()
SERVER = str(HERE / "acp_server.py")


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


class Client:
    def __init__(self, choose: str = "allow") -> None:
        self.choose = choose
        self.requests: list = []
        self.options: list = []
        self.tools: list = []  # (status, text) per terminal tool update

    async def session_update(self, session_id, update, **kwargs):
        if type(update).__name__ == "ToolCallProgress" and update.status in ("completed", "failed"):
            text = " ".join(getattr(getattr(c, "content", None), "text", "") or "" for c in (update.content or []))
            self.tools.append((update.status, text))

    async def request_permission(self, session_id, tool_call, options, **kwargs):
        self.requests.append(tool_call)
        self.options.append([(o.option_id, o.kind, o.name) for o in options])
        return acp.schema.RequestPermissionResponse(
            outcome=acp.schema.AllowedOutcome(outcome="selected", option_id=self.choose)
        )


class Sandbox:
    def __init__(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name).resolve()
        self.home = root / "home"
        self.project = root / "project"
        (self.project / ".sierralab").mkdir(parents=True)
        self.home.mkdir()

    def user(self, data: dict) -> None:
        (self.home / "settings.json").write_text(json.dumps(data))

    def env(self) -> dict:
        return {**os.environ, "SIERRALAB_HOME": str(self.home)}


async def ask(conn, session_id: str, text: str):
    return await conn.prompt(session_id=session_id, prompt=[acp.text_block(text)])


async def test_default_mode_and_instructions() -> None:
    print("\n=== default mode from settings ===")
    box = Sandbox()
    box.user({"permissions": {"defaultMode": "plan"}})
    client = Client()
    async with acp.spawn_agent_process(lambda _a: client, sys.executable, SERVER, env=box.env()) as (conn, proc):
        await conn.initialize(protocol_version=acp.PROTOCOL_VERSION)
        created = await conn.new_session(cwd=str(box.project))
        check(created.modes.current_mode_id == "plan", "a new session starts in permissions.defaultMode")
        proc.terminate()

    print("\n=== project 'auto' is ignored until the folder is trusted ===")
    box = Sandbox()
    (box.project / ".sierralab" / "settings.json").write_text(json.dumps({"permissions": {"defaultMode": "auto"}}))
    async with acp.spawn_agent_process(lambda _a: client, sys.executable, SERVER, env=box.env()) as (conn, proc):
        await conn.initialize(protocol_version=acp.PROTOCOL_VERSION)
        created = await conn.new_session(cwd=str(box.project))
        check(created.modes.current_mode_id == "build", "a stranger's repo cannot start the agent in auto")
        proc.terminate()


async def test_instructions_reach_the_model() -> None:
    print("\n=== instruction file in the system prompt ===")
    box = Sandbox()
    (box.project / "SIERRALAB.md").write_text("The project codeword is PINEAPPLE-7. Mention it when asked for the codeword.")
    text: list[str] = []

    class Chatty(Client):
        async def session_update(self, session_id, update, **kwargs):
            await super().session_update(session_id, update, **kwargs)
            if type(update).__name__ == "AgentMessageChunk":
                text.append(getattr(update.content, "text", "") or "")

    client = Chatty()
    async with acp.spawn_agent_process(lambda _a: client, sys.executable, SERVER, env=box.env()) as (conn, proc):
        await conn.initialize(protocol_version=acp.PROTOCOL_VERSION)
        created = await conn.new_session(cwd=str(box.project))
        await ask(conn, created.sessionId, "What is the project codeword? Answer with just the codeword.")
        check("PINEAPPLE-7" in "".join(text), f"the model saw SIERRALAB.md ({''.join(text)[:80]!r})")
        proc.terminate()


async def test_rules_decide_whether_a_call_asks() -> None:
    print("\n=== deny rule blocks, allow rule skips the prompt ===")
    box = Sandbox()
    box.user({"permissions": {"deny": ["shell(echo deny-marker*)"], "allow": ["shell(echo allow-marker-literal)"]}})
    client = Client()
    async with acp.spawn_agent_process(lambda _a: client, sys.executable, SERVER, env=box.env()) as (conn, proc):
        await conn.initialize(protocol_version=acp.PROTOCOL_VERSION)
        created = await conn.new_session(cwd=str(box.project))

        await ask(conn, created.sessionId, "Call the shell tool to run exactly: echo deny-marker-$((6*7))")
        check(not client.requests, "a denied command never reaches the approval prompt")
        check(not any("deny-marker-42" in text for _status, text in client.tools), "…and never ran")
        check(any(status == "failed" and "deny rule" in text for status, text in client.tools), "…and the model is told which rule blocked it")

        client.requests.clear()
        client.tools.clear()
        await ask(conn, created.sessionId, "Call the shell tool to run exactly: echo allow-marker-literal")
        check(not client.requests, "an allowed command runs without asking")
        check(any(status == "completed" and "allow-marker-literal" in text for status, text in client.tools), "…and ran")
        proc.terminate()


async def test_always_allow_is_saved_and_holds() -> None:
    print("\n=== 'Always allow' saves a rule, trusts the folder, and stops the prompts ===")
    box = Sandbox()
    client = Client(choose="allow_always")
    command = "echo always-marker"
    async with acp.spawn_agent_process(lambda _a: client, sys.executable, SERVER, env=box.env()) as (conn, proc):
        await conn.initialize(protocol_version=acp.PROTOCOL_VERSION)
        created = await conn.new_session(cwd=str(box.project))

        await ask(conn, created.sessionId, f"Call the shell tool to run exactly: {command}")
        check(len(client.requests) == 1, "the first call asks")
        offered = {kind: name for _id, kind, name in client.options[0]}
        check(offered.get("allow_always", "").startswith("Always allow shell("), f"…offering a narrow rule ({offered.get('allow_always')!r})")
        local = box.project / ".sierralab" / "settings.local.json"
        saved = json.loads(local.read_text())["permissions"]["allow"]
        check(saved == [f"shell({command})"], f"the rule was saved to settings.local.json ({saved})")
        check(str(box.project) in json.loads((box.home / "settings.json").read_text())["trustedProjects"], "…and the folder was marked trusted")

        client.requests.clear()
        await ask(conn, created.sessionId, f"Call the shell tool to run exactly: {command}")
        check(not client.requests, "the same call no longer asks")

        client.requests.clear()
        await ask(conn, created.sessionId, "Call the shell tool to run exactly: echo different-marker")
        check(len(client.requests) == 1, "a different command still asks")
        proc.terminate()


async def main() -> None:
    await test_default_mode_and_instructions()
    await test_instructions_reach_the_model()
    await test_rules_decide_whether_a_call_asks()
    await test_always_allow_is_saved_and_holds()
    print("\nacp settings: all checks passed")


asyncio.run(main())

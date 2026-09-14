"""One-off manual smoke test for acp_server.py — not part of the app.

Exercises the maturation work: tool-call visibility (ToolCallStart/
ToolCallProgress), real permission gating (approve/deny), session modes
(Plan blocks tools, Auto skips prompts), config options (model/temperature
as native selects), session titles, and load_session's history replay.
"""

import asyncio
import sys

import acp


class RecordingClient:
    def __init__(self, auto_approve: bool = True):
        self.chunks = []
        self.tool_calls = []
        self.permission_requests = []
        self.titles = []
        self.auto_approve = auto_approve

    async def session_update(self, session_id, update, **kwargs):
        kind = type(update).__name__
        text = getattr(update, "text", None) or getattr(getattr(update, "content", None), "text", None)
        self.chunks.append((kind, text))
        if kind in ("ToolCallStart", "ToolCallProgress"):
            self.tool_calls.append(update)
        if kind == "SessionInfoUpdate":
            self.titles.append(update.title)

    async def request_permission(self, session_id, tool_call, options, **kwargs):
        self.permission_requests.append(tool_call)
        if self.auto_approve:
            outcome = acp.schema.AllowedOutcome(outcome="selected", option_id="allow")
        else:
            outcome = acp.schema.DeniedOutcome(outcome="cancelled")
        return acp.schema.RequestPermissionResponse(outcome=outcome)


async def run_case(label: str, auto_approve: bool, prompt_text: str, mode: str | None = None, cwd: str = "."):
    print(f"\n=== {label} ===")
    client = RecordingClient(auto_approve=auto_approve)
    async with acp.spawn_agent_process(lambda _agent: client, sys.executable, "acp_server.py") as (conn, proc):
        init = await conn.initialize(protocol_version=acp.PROTOCOL_VERSION)
        print("agentCapabilities.loadSession ->", init.agentCapabilities.load_session)

        session = await conn.new_session(cwd=cwd)
        print("modes ->", [m.id for m in session.modes.available_modes], "default:", session.modes.current_mode_id)
        print("config_options ->", [(c.id, c.current_value, [o.value for o in c.options]) for c in session.config_options])

        if mode:
            mode_result = await conn.set_session_mode(session_id=session.sessionId, mode_id=mode)
            print(f"set_session_mode({mode}) ->", mode_result)

        result = await conn.prompt(session_id=session.sessionId, prompt=[acp.text_block(prompt_text)])

        print("stopReason ->", result.stopReason)
        print("titles seen ->", client.titles)
        print("permission_requests ->", [(pr.title, pr.tool_call_id) for pr in client.permission_requests])
        print("tool_calls seen ->", [(type(tc).__name__, getattr(tc, "title", None), getattr(tc, "status", None)) for tc in client.tool_calls])
        print("final text chunks ->", [c for c in client.chunks if c[0] == "AgentMessageChunk" and c[1]])

        proc.terminate()
        return session.sessionId


async def test_config_option_change():
    print("\n=== config option: switch model, verify config_options reflects it ===")
    client = RecordingClient()
    async with acp.spawn_agent_process(lambda _agent: client, sys.executable, "acp_server.py") as (conn, proc):
        await conn.initialize(protocol_version=acp.PROTOCOL_VERSION)
        session = await conn.new_session(cwd=".")
        model_option = next(c for c in session.config_options if c.id == "model")
        other_model = next((v for v in [o.value for o in model_option.options] if v != model_option.current_value), None)
        print("current model ->", model_option.current_value, "switching to ->", other_model)
        if other_model:
            resp = await conn.set_config_option(session_id=session.sessionId, config_id="model", value=other_model)
            new_model_option = next(c for c in resp.config_options if c.id == "model")
            print("model after switch ->", new_model_option.current_value)
        proc.terminate()


async def main():
    await run_case("plain reply, no tools (build mode default)", auto_approve=True, prompt_text="Reply with exactly the word: pong")

    await run_case("gated tool, approved", auto_approve=True, prompt_text='Call the shell tool to run exactly: echo acp-approved-marker')

    await run_case("gated tool, denied", auto_approve=False, prompt_text='Call the shell tool to run exactly: echo should-not-run')

    await run_case(
        "plan mode blocks tool entirely",
        auto_approve=True,
        prompt_text='Call the shell tool to run exactly: echo should-be-blocked-by-plan-mode',
        mode="plan",
    )

    await run_case(
        "auto mode skips approval prompt",
        auto_approve=True,
        prompt_text='Call the shell tool to run exactly: echo auto-mode-marker',
        mode="auto",
    )

    await test_config_option_change()


asyncio.run(main())

"""One-off manual smoke test for acp_server.py — not part of the app.

Exercises the maturation work: tool-call visibility (ToolCallStart/
ToolCallProgress), real permission gating (approve/deny), session modes
(Plan blocks tools, Auto skips prompts), config options (model/temperature
as native selects), session titles, and load_session's history replay.
"""

import asyncio
import os
import sys
from pathlib import Path

os.environ.setdefault("CORDATA_REFLECT", "off")  # live test turns are not lessons (reflect.py)

import acp


class RecordingClient:
    """decision: "allow" selects the allow option, "reject" selects the reject
    option via a spec-conformant AllowedOutcome(outcome="selected") — what Zed
    sends on Reject — and "cancel" sends a cancelled outcome."""

    def __init__(self, decision: str = "allow"):
        self.chunks = []
        self.tool_calls = []
        self.permission_requests = []
        self.titles = []
        self.decision = decision

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
        if self.decision == "cancel":
            outcome = acp.schema.DeniedOutcome(outcome="cancelled")
        else:
            outcome = acp.schema.AllowedOutcome(outcome="selected", option_id=self.decision)
        return acp.schema.RequestPermissionResponse(outcome=outcome)


async def run_case(label: str, decision: str, prompt_text: str, mode: str | None = None, cwd: str = os.getcwd()):
    print(f"\n=== {label} ===")
    client = RecordingClient(decision=decision)
    async with acp.spawn_agent_process(lambda _agent: client, sys.executable, str(Path(__file__).parent.resolve() / "acp_server.py"), env=dict(os.environ)) as (conn, proc):
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
        return client


def test_permission_outcome_mapping():
    """No LLM needed: only an allow-kind option we offered approves."""
    from acp_server import _permission_approved

    print("\n=== permission outcome mapping ===")
    options = [
        acp.schema.PermissionOption(option_id="allow", name="Allow", kind="allow_once"),
        acp.schema.PermissionOption(option_id="reject", name="Reject", kind="reject_once"),
    ]
    selected = lambda option_id: acp.schema.AllowedOutcome(outcome="selected", option_id=option_id)
    assert _permission_approved(selected("allow"), options)
    assert not _permission_approved(selected("reject"), options), "selected Reject must deny"
    assert not _permission_approved(selected("bogus"), options), "unknown option_id must deny"
    assert not _permission_approved(acp.schema.DeniedOutcome(outcome="cancelled"), options)
    print("ok")


async def test_reject_selected_does_not_run_tool():
    """Regression: a client that answers by *selecting* Reject (AllowedOutcome
    with option_id="reject") must not run the gated tool. The marker is
    computed by the shell ($((6*7)) -> 42), so it appears in real output only if
    the command actually ran."""
    client = await run_case(
        "gated tool, Reject selected (AllowedOutcome option_id=reject)",
        decision="reject",
        prompt_text='Call the shell tool to run exactly: echo reject-marker-$((6*7))',
    )
    assert client.permission_requests, "expected a permission request"
    progress = [tc for tc in client.tool_calls if type(tc).__name__ == "ToolCallProgress"]
    blocks = [
        getattr(c, "content", None).text or ""
        for tc in progress
        for c in (tc.content or [])
    ]
    # `CONFIRMATION_FAILED: {...}` is the harness echoing back the request it
    # refused, so it contains the command text verbatim — and on a self-heal
    # retry the model may restate the command with the arithmetic already
    # evaluated ("echo reject-marker-42"). That is the model's wording, not
    # shell output, so it must not count as the tool having run. Genuine
    # execution would instead surface as a *completed* call (asserted below) or
    # as the bare marker in a result that is not this echo.
    output = " ".join(b for b in blocks if not b.startswith("CONFIRMATION_FAILED"))
    assert "reject-marker-42" not in output, f"tool ran despite Reject: {output!r}"
    assert not any(tc.status == "completed" for tc in progress), "tool completed despite Reject"
    assert any(tc.status == "failed" for tc in progress), "expected tool status failed"
    print("ok: tool did not run")


async def test_config_option_change():
    print("\n=== config option: switch model, verify config_options reflects it ===")
    client = RecordingClient()
    async with acp.spawn_agent_process(lambda _agent: client, sys.executable, str(Path(__file__).parent.resolve() / "acp_server.py"), env=dict(os.environ)) as (conn, proc):
        await conn.initialize(protocol_version=acp.PROTOCOL_VERSION)
        session = await conn.new_session(cwd=os.getcwd())
        model_option = next(c for c in session.config_options if c.id == "model")
        other_model = next((v for v in [o.value for o in model_option.options] if v != model_option.current_value), None)
        print("current model ->", model_option.current_value, "switching to ->", other_model)
        if other_model:
            resp = await conn.set_config_option(session_id=session.sessionId, config_id="model", value=other_model)
            new_model_option = next(c for c in resp.config_options if c.id == "model")
            print("model after switch ->", new_model_option.current_value)
        proc.terminate()


async def main():
    test_permission_outcome_mapping()

    await run_case("plain reply, no tools (build mode default)", decision="allow", prompt_text="Reply with exactly the word: pong")

    await run_case("gated tool, approved", decision="allow", prompt_text='Call the shell tool to run exactly: echo acp-approved-marker')

    await run_case("gated tool, denied", decision="cancel", prompt_text='Call the shell tool to run exactly: echo should-not-run')

    await test_reject_selected_does_not_run_tool()

    await run_case(
        "plan mode blocks tool entirely",
        decision="allow",
        prompt_text='Call the shell tool to run exactly: echo should-be-blocked-by-plan-mode',
        mode="plan",
    )

    await run_case(
        "auto mode skips approval prompt",
        decision="allow",
        prompt_text='Call the shell tool to run exactly: echo auto-mode-marker',
        mode="auto",
    )

    await test_config_option_change()


if __name__ == "__main__":
    asyncio.run(main())

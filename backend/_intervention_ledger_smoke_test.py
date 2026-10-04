"""Regression check for the intervention half of the ledger: an approval the
gate staged, the answer it was closed with, and the replay of that answer when
the turn comes back after a crash.

Runs the real `_resolve_interrupts` — no model server and no ACP connection, so
it is a few hundred milliseconds rather than the ~8s a full session costs.
Hermetic: `SIERRALAB_STATE_DB` points at a temp file before `state_store` (and
therefore `acp_server`) is imported, so a run never touches the developer's real
ledger. Run directly:

    backend/.venv/bin/python _intervention_ledger_smoke_test.py
"""

import asyncio
import json
import os
import tempfile
import types
from pathlib import Path

_TMP = tempfile.TemporaryDirectory()
os.environ["SIERRALAB_STATE_DB"] = str(Path(_TMP.name) / "state.db")

import acp  # noqa: E402
import settings_store  # noqa: E402
import state_store  # noqa: E402
from acp_server import SierraLabAcpAgent, _settled_intervention  # noqa: E402


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


class FakeConn:
    """Just enough of the ACP client for `request_permission`."""

    def __init__(self, option_id: str = "allow") -> None:
        self.option_id = option_id
        self.asks: list = []

    async def request_permission(self, session_id, tool_call, options, **kwargs):
        self.asks.append((tool_call, list(options)))
        return acp.schema.RequestPermissionResponse(
            outcome=acp.schema.AllowedOutcome(outcome="selected", option_id=self.option_id)
        )


def fake_agent(conn=None):
    """A stand-in `SierraLabAcpAgent` for the interrupt path. `_resolve_interrupts`
    touches only `_conn`, `_sessions` and `_remember_always`; an empty registry
    means the router-hook rejection note is skipped, which is the only other
    thing it reaches for. `saved_rules` records what always-allow would have
    written, so no check here can touch the machine's real settings files."""
    agent = types.SimpleNamespace(_conn=conn, _sessions={}, saved_rules=[])
    agent._remember_always = lambda session_id, rule: agent.saved_rules.append(rule)
    return agent


def interrupt(interrupt_id: str, name: str, tool_input, *, always_rule=None, risk=None) -> object:
    payload = {"name": name, "input": tool_input, "tool_use_id": "call-1", "always_rule": always_rule}
    if risk:
        payload["risk"] = risk
    return types.SimpleNamespace(id=interrupt_id, reason=json.dumps(payload))


def events(kind: str) -> list:
    return state_store.recent_events(kind=kind, limit=50)


def test_a_prompt_is_staged_then_resolved() -> None:
    print("\n=== staged, answered, resolved ===")
    conn = FakeConn("allow")
    agent = fake_agent(conn)
    risk = {"event": "risk", "score": 0.12, "band": "safe", "label": "read_only", "confidence": 0.9}
    item = interrupt("int-approved", "shell", {"command": "ls"}, always_rule="shell(ls)", risk=risk)

    responses = asyncio.run(SierraLabAcpAgent._resolve_interrupts(agent, "sess-1", [item]))

    check(responses == [{"interruptResponse": {"interruptId": "int-approved", "response": True}}], "an allowed prompt resumes the tool")
    check(len(conn.asks) == 1, "the user is asked exactly once")
    tools, options = conn.asks[0]
    check([o.option_id for o in options] == ["allow", "allow_always", "reject"], "the prompt offers the always-allow rule")
    check(tools.raw_input == {"command": "ls"}, "the prompt carries the tool input")

    row = state_store.get_intervention("int-approved")
    check(row["status"] == "approved", "the row is closed out as approved")
    check(row["decided_by"] == "user", "the decision is attributed to the user")
    check(row["risk_score"] == 0.12, "the Laya score is on the row")
    check(row["risk_label"] == "safe", "the band, not the ordinal, is what the row stores")
    check(state_store.pending_interventions("sess-1") == [], "nothing is left staged")

    phases = [row["payload"].get("phase") for row in events("intervention")]
    check(phases == ["resolved", "requested"], "both transitions are on the ledger, newest first")

    asked = [row for row in events("intervention") if row["payload"]["phase"] == "requested"][0]
    check(asked["payload"]["risk"]["score"] == 0.12, "the request event carries the full risk reading")
    check(asked["session_id"] == "sess-1", "the event is filed under the session")


def test_always_allow_is_recorded_as_a_rule() -> None:
    print("\n=== always allow ===")
    conn = FakeConn("allow_always")
    agent = fake_agent(conn)
    item = interrupt("int-always", "shell", {"command": "git status"}, always_rule="shell(git status)")

    responses = asyncio.run(SierraLabAcpAgent._resolve_interrupts(agent, "sess-2", [item]))
    check(responses[0]["interruptResponse"]["response"] is True, "always-allow approves the call")
    check(agent.saved_rules == ["shell(git status)"], "and the rule is saved for the project")

    row = state_store.get_intervention("int-always")
    check(row["status"] == "approved" and row["rule"] == "shell(git status)", "the row records the rule that was saved")

    resolved = [row for row in events("intervention") if row["payload"]["phase"] == "resolved"][0]
    check(resolved["payload"]["always"] is True, "the event distinguishes always-allow from allow-once")


def test_a_rejected_prompt_is_recorded_as_rejected() -> None:
    print("\n=== rejected ===")
    agent = fake_agent(FakeConn("reject"))
    item = interrupt("int-rejected", "file_write", {"path": "x"})

    responses = asyncio.run(SierraLabAcpAgent._resolve_interrupts(agent, "sess-3", [item]))

    check(responses[0]["interruptResponse"]["response"] is False, "a rejected prompt blocks the call")
    row = state_store.get_intervention("int-rejected")
    check(row["status"] == "rejected" and row["decided_by"] == "user", "the rejection is attributed to the user")


def test_no_client_is_a_rejection_not_a_user_decision() -> None:
    print("\n=== no client attached ===")
    agent = fake_agent(None)
    item = interrupt("int-headless", "shell", {"command": "rm -rf /"})

    responses = asyncio.run(SierraLabAcpAgent._resolve_interrupts(agent, "sess-4", [item]))

    check(responses[0]["interruptResponse"]["response"] is False, "with nobody to ask, the call does not run")
    row = state_store.get_intervention("int-headless")
    check(row["status"] == "rejected", "the row is closed as rejected")
    check(row["decided_by"] == "no-client", "and it does not read as a decision a person made")


def test_a_recorded_decision_is_replayed_not_re_asked() -> None:
    print("\n=== replay after a crash ===")
    payload = {"name": "shell", "input": {"command": "make test"}, "tool_use_id": "call-9", "always_rule": "shell(make test)"}
    state_store.stage_intervention(
        session_id="sess-5", tool="shell", tool_input={"command": "make test"}, intervention_id="int-replay"
    )
    state_store.resolve_intervention("int-replay", "approved", decided_by="user", rule="shell(make test)")

    conn = FakeConn("reject")  # would reject if it were asked: it must not be
    agent = fake_agent(conn)
    item = types.SimpleNamespace(id="int-replay", reason=json.dumps(payload))

    responses = asyncio.run(SierraLabAcpAgent._resolve_interrupts(agent, "sess-5", [item]))

    check(conn.asks == [], "the recorded answer is replayed instead of asking again")
    check(responses[0]["interruptResponse"]["response"] is True, "the turn resumes with the answer the user gave")
    check(state_store.get_intervention("int-replay")["status"] == "approved", "replaying does not rewrite the row")
    check(events("intervention")[0]["payload"]["phase"] == "replayed", "the ledger says the decision was replayed")


def test_a_mismatched_row_re_prompts() -> None:
    print("\n=== id reuse ===")
    state_store.stage_intervention(session_id="sess-6", tool="shell", tool_input={"command": "ls"}, intervention_id="int-reuse")
    state_store.resolve_intervention("int-reuse", "approved", decided_by="user")

    check(
        _settled_intervention("int-reuse", "sess-6", "shell", {"command": "ls"}) is not None,
        "an identical call in the same session is matched",
    )
    check(
        _settled_intervention("int-reuse", "sess-6", "shell", {"command": "rm -rf /"}) is None,
        "a different call under the same id is not",
    )
    check(_settled_intervention("int-reuse", "sess-6", "file_write", {"command": "ls"}) is None, "a different tool is not")
    check(_settled_intervention("never-staged", "sess-6", "shell", {"command": "ls"}) is None, "an unknown id is not")
    # Strands derives the id from the tool-use id, which another session's model
    # could in principle mint too — one session's answer must not decide another's.
    check(
        _settled_intervention("int-reuse", "sess-other", "shell", {"command": "ls"}) is None,
        "another session's answer is not lent to this one",
    )

    state_store.stage_intervention(session_id="sess-6", tool="shell", tool_input={"command": "ls"}, intervention_id="int-live")
    check(
        _settled_intervention("int-live", "sess-6", "shell", {"command": "ls"}) is None,
        "a still-staged row is not a decision",
    )

    conn = FakeConn("allow")
    agent = fake_agent(conn)
    item = interrupt("int-reuse", "file_write", {"path": "x"})
    responses = asyncio.run(SierraLabAcpAgent._resolve_interrupts(agent, "sess-6", [item]))
    check(len(conn.asks) == 1, "a mismatched row means the user is asked")
    check(responses[0]["interruptResponse"]["response"] is True, "and the fresh answer decides")
    check(state_store.get_intervention("int-reuse")["tool"] == "file_write", "the row now describes the call it decided")


def test_an_unparseable_reason_still_gets_a_row() -> None:
    print("\n=== malformed interrupt ===")
    agent = fake_agent(FakeConn("reject"))
    item = types.SimpleNamespace(id="int-garbled", reason="not json at all")

    responses = asyncio.run(SierraLabAcpAgent._resolve_interrupts(agent, "sess-7", [item]))

    check(responses[0]["interruptResponse"]["response"] is False, "a malformed interrupt is refused, not crashed on")
    row = state_store.get_intervention("int-garbled")
    check(row["tool"] == "not json at all", "the raw reason is recorded rather than hidden")


def test_the_gate_puts_its_reasoning_on_the_prompt() -> None:
    print("\n=== the gate's payload ===")
    from laya_router import RouteDecision
    from acp_server import _ApprovalHandler

    # The real settings files are the developer's own; the point of this check is
    # the payload, so the gate is handed an empty settings view it cannot be
    # surprised by.
    real_current = settings_store.current
    settings_store.current = lambda cwd: settings_store.Settings()
    try:
        decision = RouteDecision(
            target=None, risk="destructive", risk_confidence=0.95, laya_choice="destructive"
        )

        class Hooks:
            router = types.SimpleNamespace(calibrator=None)

            def current(self, agent):
                return decision

            def armed(self, agent):
                return decision.armed

        agent = object()
        handler = _ApprovalHandler(str(Path(_TMP.name)), "build", router_hooks=Hooks())
        handler.session_agent = agent
        event = types.SimpleNamespace(tool_use={"name": "shell", "input": {"command": "rm -rf x"}, "toolUseId": "call-7"})

        result = handler.before_tool_call(event)
        payload = json.loads(result.prompt)
        check(payload["name"] == "shell" and payload["tool_use_id"] == "call-7", "the prompt names the call")
        check(payload["risk"]["band"] == "destructive", "the scored band rides along with the prompt")
        check(payload["risk"]["score"] > 0.8, "a confident destructive reading scores above 0.8")
        check("reason" in payload, "an armed turn says why it is asking in build mode too")

        # No decision to score (router off, or it abstained): the prompt is still
        # a prompt, minus the risk field — never a placeholder score.
        class NoDecision(Hooks):
            def current(self, agent):
                return None

        plain = _ApprovalHandler(str(Path(_TMP.name)), "build", router_hooks=NoDecision())
        plain.session_agent = agent
        check("risk" not in json.loads(plain.before_tool_call(event).prompt), "no decision means no risk field, not a zero")

        bare = _ApprovalHandler(str(Path(_TMP.name)), "build")
        check("risk" not in json.loads(bare.before_tool_call(event).prompt), "no router hooks at all is the same")
    finally:
        settings_store.current = real_current


if __name__ == "__main__":
    test_a_prompt_is_staged_then_resolved()
    test_always_allow_is_recorded_as_a_rule()
    test_a_rejected_prompt_is_recorded_as_rejected()
    test_no_client_is_a_rejection_not_a_user_decision()
    test_a_recorded_decision_is_replayed_not_re_asked()
    test_a_mismatched_row_re_prompts()
    test_an_unparseable_reason_still_gets_a_row()
    test_the_gate_puts_its_reasoning_on_the_prompt()
    _TMP.cleanup()
    print("\nintervention ledger: all checks passed")

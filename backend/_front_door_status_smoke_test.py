"""Phase 7 of the orchestrator (docs/orchestrator-plan.md): quota and handoffs at the front doors, at zero tokens.

One payload (``orchestrator/status.py``, read from ``state.db``) behind three renderings: the TUI's ``/status`` (ACP ext
method ``_sierralab/orchestrator/status``; the Rust side's rendering is unit-tested in ``acp-tui``), Discord's
``!status`` card, and ``python -m orchestrator status``. Live run events reach the TUI as ``_sierralab/orchestrator``
notifications. Nothing here may build or call a model.

    backend/.venv/bin/python _front_door_status_smoke_test.py
"""

from __future__ import annotations

import asyncio
import os
import sys
import tempfile
import time
from pathlib import Path

_HOME = tempfile.mkdtemp(prefix="status-home-")
os.environ["SIERRALAB_HOME"] = _HOME
os.environ["SIERRALAB_STATE_DB"] = str(Path(_HOME) / "state.db")

import provider_settings  # noqa: F401,E402

from daemon import discord_cards as cards  # noqa: E402
from harness_testing import check  # noqa: E402
from orchestrator import handoff, machine  # noqa: E402
from orchestrator.quota_ledger import DEFAULT_POOLS, QuotaLedger  # noqa: E402
from orchestrator.status import render, status_data  # noqa: E402
from workers import WorkerResult  # noqa: E402


def seed() -> None:
    ledger = QuotaLedger(DEFAULT_POOLS)
    result = WorkerResult("claude_code", "completed", 0, 1.0, 1.25, 30_000, 2_000, [], "", "", provider_quota={"status": "allowed", "windows": {"five_hour": {"utilization": 0.62, "resets_at": time.time() + 3600}}})
    ledger.record_result(session_id="run-a", step_id="s1", family="claude_code", model="sonnet", result=result)
    ledger.record_result(session_id="run-a", step_id="s2", family="antigravity", model="gemini-3.8-flash-medium", result=WorkerResult("agy", "completed", 0, 1.0, 0.0, 400_000, 20_000, [], "", ""))
    handoff.save(handoff.build(session_id="run-a", reason="provider_rate_limit", plan_id="p", step_index=1, step_id="s2", step_status="pending", checkpoint_hash=None, from_worker="claude_code:sonnet", to_worker="antigravity:gemini-3.8-flash-medium"))
    machine.save(machine.RunRecord(run_id="run-a", goal="add retries to the client", cwd="/tmp", state="done", handoffs=["h1"]))


def test_payload() -> None:
    print("\n=== the payload ===")
    import agent_common  # noqa: F401  (so a model build would be visible below)

    built = []
    real = agent_common.build_model
    agent_common.build_model = lambda *a, **k: built.append(1) or real(*a, **k)
    try:
        data = status_data()
    finally:
        agent_common.build_model = real
    pools = {p["pool_id"]: p for p in data["pools"]}
    check(not built, "no model is built to answer a status")
    check(pools["claude_code"]["source"] == "provider" and pools["claude_code"]["pct"] >= 0.62, f"the provider's own reading leads the Claude Code pool ({pools['claude_code']['pct']})")
    check(pools["antigravity"]["used"] == 420_000, "the Antigravity pool is metered in tokens")
    check(data["runs"][0]["run_id"] == "run-a" and data["runs"][0]["handoffs"] == 1, "recent runs, with their handoffs")
    check(data["handoffs"][0]["reason"] == "provider_rate_limit", "recent handoffs")
    text = render(data, marks=True)
    check("claude_code" in text and "█" in text and "· provider" in text and "420k/8M tokens" in text, "the CLI rendering has gauges and units")


def test_discord() -> None:
    print("\n=== Discord !status ===")
    data = status_data()
    card = cards.status_card([], quota=data)
    fields = {name: value for name, value, _ in card["fields"]}
    check("Quota" in fields and "```" in fields["Quota"] and "█" in fields["Quota"], "the status card carries a gauge per pool")
    check("Last handoff" in fields and "provider_rate_limit" in fields["Last handoff"] and "→" in fields["Last handoff"], "…and the last handoff")
    check("no model call" in card["footer"], "…answered from state, as the footer says")
    rows = [{"project": "p", "state": "working", "detail": "", "since": 5, "pending": 0, "kind": "chat", "mission": ""}]
    busy = cards.status_card(rows, quota=data)
    check(busy["fields"][0][0].startswith("p ·") and any(name == "Quota" for name, _, _ in busy["fields"]), "the gauges follow the sessions on a busy card")
    check(cards.status_card([], quota=None)["fields"] == [], "no orchestrator data, no extra fields")


def test_acp() -> None:
    print("\n=== the TUI's /status (ACP ext method) and live events ===")
    import acp_server

    agent = acp_server.SierraLabAcpAgent()
    data = asyncio.run(agent.ext_method("_sierralab/orchestrator/status", {}))
    check(data["runs"][0]["run_id"] == "run-a" and data["pools"], "the ext method returns the same payload")
    sent: list[tuple[str, dict]] = []
    agent._send_ext = lambda method, payload: sent.append((method, payload))  # type: ignore[method-assign]
    agent._forward_event("orchestrator", {"kind": "handoff", "run_id": "run-a", "reason": "provider_rate_limit"})
    check(sent and sent[0][0] == "sierralab/orchestrator" and sent[0][1]["kind"] == "handoff", "an orchestrator event is forwarded as _sierralab/orchestrator")

    import orchestrate_tool

    check('harness_events.emit("orchestrator"' in Path(orchestrate_tool.__file__).read_text(), "the orchestrate tool puts its run events on the harness bus")


def main() -> int:
    seed()
    test_payload()
    test_discord()
    test_acp()
    print("\nfront-door status: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())

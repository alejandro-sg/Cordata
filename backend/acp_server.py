"""ACP front door: the same Strands agent (agent_common.py) over the Agent
Client Protocol (https://agentclientprotocol.com), for editors like Zed that
speak ACP over stdio. See main.py for the AG-UI/web front door.

Run directly — an ACP client spawns this process and talks JSON-RPC over
its stdin/stdout, so don't print anything else to stdout:

    python acp_server.py

A conversation resumes across restarts and across front doors: this uses
the same on-disk session (session_store.py) keyed by session id that main.py uses
keyed by AG-UI threadId (see agent_common.py).

Parity with the AG-UI front door, and with Zed's own bundled Claude Code
integration: real tool-call visibility, real approval-before-execution on
dangerous tools, session modes (Plan/Build/Auto), model + temperature as
native config-option dropdowns, session titles, and @-mention/resource
content actually reaching the model instead of being silently dropped. Every
mechanism below is wired to something this backend can already really do
(model_control_tool.py's model/temperature switching, the same tool set
main.py gates) — nothing here is a cosmetic stand-in for a capability that
doesn't exist.

The agentic harness on top (docs/agentic-harness.md), per session:

* **Laya router** (laya_router.py) — one typed forward pass per user turn picks
  the target (orchestrator / architect / debugger / an MCP plugin), with
  abstention that fails closed (escalate to the frontier tier, never dispatch on
  a guess); confident specialist routes are dispatched directly; outcomes feed
  online calibration.
* **Specialists** (specialists.py) — Architect and Debugger as Strands
  agents-as-tools sharing this session's approval handler.
* **Plugins** (plugin_host.py) — MCP servers, the memory store and the LSP tools
  as hot-swappable plugins (``_sierralab/plugins/*`` ext methods).
* **LSP** (lsp_bridge.py) — language servers started for the project; edits get
  fresh diagnostics appended.
* **Self-healing** (self_heal.py) — failing commands are diagnosed, patched in a
  sandbox copy, verified, and the fix applied through the normal gate.
* **Checkpoints** — Strands pauses at every ReAct cycle boundary; the checkpoint
  is written next to the session so an interrupted turn can be resumed.
* Live state (tasks/progress, heal phases, plugin and LSP changes) goes to the
  client as ``_sierralab/<kind>`` extension notifications.
"""

import asyncio
import json
import collections
import logging
import os
import threading
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path

import acp
import acp.stdio
from strands.interventions import Confirm, Deny, InterventionHandler, Proceed

import checkpoints
import edit_preview
import bandit
import claude_code_tool
import context_diet
import context_offload
import session_store
import skill_search
import fix_memory
import context_digest_tool
import harness_events
import intervention_view
import kg
import lab as lab_module
import learned
import laya_guard
import laya_risk
import outcome_hooks
import outcomes
import optimize
import secret_guard
import stall_guard
import lsp_bridge
import ollama_resource_tool
import omlx_provider
import omlx_resource_tool
import patch_tool
import prompts
import reflect
import settings as app_settings
import settings_store
import specialists
import state_store
import orchestrate_tool
import trace_store
import vault_proxy
import workspace_context
import adaptive_effort
import code_nav
import code_search
import env_profile
import os_context
import plugin_host
import preflight
import rejection_memory
from laya_router import BASE_TARGETS, LayaRouter, Policy, RouteDecision, RouterHooks, plugin_targets, summarize_stats
from plugin_host import PluginHost, PluginSpec, run_blocking
from self_heal import HealConfig, SelfHealHooks

# Last: importing agent_common chdirs the process into backend/workspace/, after
# which bare-module imports only resolve when this file runs as a script.
from agent_common import GENERATED_TOOL_NAMES, MCP_TOOL_NAMES, SESSIONS_DIR, build_agent_for_session, current_generated_tool_names, start_mcp_clients  # noqa: E402
from model_control_tool import _context_window_limit, _fetch_available_models  # noqa: E402

# Set-identical to main.py's _APPROVAL_REQUIRED_TOOLS — both gate `editor`
# and every MCP-server-provided tool too (main.py's own comment on
# MCP_TOOL_NAMES explains why); this repo has no shared source for the two
# lists, so keep them in sync by hand when either changes. apply_patch is
# ACP-only (the harness's patch tool). Plugins added at runtime are gated by
# PluginHost.requires_approval, not by this static set.
APPROVAL_REQUIRED_TOOLS = {"shell", "python_repl", "file_write", "editor", "apply_patch", claude_code_tool.TOOL_NAME, orchestrate_tool.TOOL_NAME, plugin_host.RESOURCE_TOOL, *GENERATED_TOOL_NAMES, *MCP_TOOL_NAMES}

# Plan mode runs only these (reading code is part of planning, as in Claude
# Code's and OpenCode's plan modes); everything else is denied.
PLAN_MODE_TOOLS = {"file_read", "code_search", "code_outline", "environment_profile", "architect", "context_digest", *(t.tool_name for t in lsp_bridge.LSP_TOOLS)}

# editor's read-only commands: like file_read, they never ask (and run in plan mode).
_EDITOR_READ_COMMANDS = {"view", "find_line"}


#: Memory-management calls that only read: they run in plan mode too, and never ask.
_READ_ONLY_ACTIONS = {
    "ollama_resource_manager": {"list", "ps", "status"},
    "omlx_resource_manager": {"status"},
    # Listing an MCP server's resources or templates is metadata; reading one is a call into the server and asks.
    plugin_host.RESOURCE_TOOL: {"list", "templates"},
}


def _read_only_call(tool_name: str, tool_input) -> bool:
    if not isinstance(tool_input, dict):
        return False
    if tool_name == "editor":
        return tool_input.get("command") in _EDITOR_READ_COMMANDS
    reads = _READ_ONLY_ACTIONS.get(tool_name)
    return reads is not None and str(tool_input.get("action", "")).strip().lower() in reads


def _forced_ask(tool_name: str, tool_input) -> str | None:
    """Why this call asks for a person even in ``auto`` mode, or None. Pulling or
    deleting model weights is a multi-gigabyte download / an irreversible delete: no
    permission mode and no Laya reading waves it through."""
    if tool_name == "ollama_resource_manager" and ollama_resource_tool.asks_for_approval(tool_input):
        return "pulling or deleting model weights always needs your approval"
    return None


# A turn Laya rates destructive with confidence arms the gate for these, even in auto mode.
_ARMABLE_TOOLS = {"shell", "python_repl", "file_write", "editor", "apply_patch", claude_code_tool.TOOL_NAME, orchestrate_tool.TOOL_NAME, "ollama_resource_manager"}

_TOOL_KIND: dict[str, str] = {
    "shell": "execute",
    "python_repl": "execute",
    "file_write": "edit",
    "editor": "edit",
    "apply_patch": "edit",
    "file_read": "read",
    "http_request": "fetch",
    "web_fetch": "fetch",
    "agent_memory": "other",
    "a2a_client": "fetch",
    "architect": "think",
    "debugger": "other",
    claude_code_tool.TOOL_NAME: "execute",
    orchestrate_tool.TOOL_NAME: "execute",
    "context_digest": "think",
    "ollama_resource_manager": "other",
    "omlx_resource_manager": "other",
    context_offload.RETRIEVE_TOOL: "read",
    **{t.tool_name: "search" for t in lsp_bridge.LSP_TOOLS},
}


def _baseline_reward(cwd: str, exclude: str = "") -> float | None:
    """The mean reward of the project's recent turns that read no learned skill — what a skill's own turns are
    compared against."""
    scores = []
    for record, update in outcomes.recent(40, project=cwd):
        if record.id == exclude or record.skills:
            continue
        scores.append(outcomes.score(record, update).value)
    return sum(scores) / len(scores) if len(scores) >= 3 else None


def _bandit_policy(preferences) -> dict:
    """The bandit's knobs, read once per turn from settings. ``off`` unless a person turned it on; the bandit
    itself holds no policy and cannot decide anything these do not hand it."""
    return {
        "mode": str(preferences.get("learning.bandit", "off") or "off"),
        "explore": float(preferences.get("learning.banditExplore", 0.1) or 0.0),
        "min_pulls": int(float(preferences.get("learning.banditMinPulls", 5) or 5)),
        "lam": float(preferences.get("learning.banditLambda", 1.0) or 0.0),
        "mu": float(preferences.get("learning.banditMu", 1.0) or 0.0),
    }


def _bandit_route_arms(decision: "RouteDecision") -> list[str]:
    """The actions the bandit may choose between for this turn — never more than the base policy already allowed.

    A dispatch may be declined (the orchestrator handles the turn instead); a hint may be dropped; nothing is
    offered that would *add* an action the policy did not produce. An abstention is left alone — fail-closed is
    not the bandit's to reopen — and a ``kind="tool"`` target is the ``engine`` point's job, not this one's.
    """
    if decision.abstained or decision.target is None:
        return []
    if decision.action == "dispatch":
        return ["dispatch", "hint"]
    if decision.action == "hint":
        return ["hint", "none"]
    if decision.action == "none" and decision.hint():
        return ["none", "hint"]
    return []


def _bandit_stub(mode: str, decision: "RouteDecision") -> dict:
    """The ledger-shaped skeleton the bandit derives a context from — the same fields a finished turn record
    carries, so the context a choice is made in is the context it is later learned from."""
    return {"mode": mode, "route": {"target": decision.target.id if decision.target else None, "risk": decision.risk, "tier": decision.tier()}}


def _maintain(cwd: str) -> None:
    """Off the turn's path: judge earlier turns' changes, keep the snapshot history bounded, and fold whatever
    the judging has now made learnable into the bandit's table (a turn only becomes learnable once reconciled)."""
    outcomes.reconcile(cwd)
    checkpoints.prune(cwd)
    _bandit_learn(cwd)


def _bandit_learn(cwd: str) -> None:
    """Rebuild the bandit's table from the project's ledger. A no-op with the bandit off; never raises."""
    try:
        preferences = settings_store.current(cwd)
        policy = _bandit_policy(preferences)
        if policy["mode"] == "off":
            return
        bandit.for_project(cwd).learn(cwd, lam=policy["lam"], mu=policy["mu"])
    except Exception:
        logging.getLogger(__name__).debug("bandit: learning pass failed", exc_info=True)


def _optimize_artifacts(cwd: str, preferences: dict) -> None:
    """Draft a sharper variant of a live lesson or prompt overlay whose turns went badly (``optimize.py``). A
    no-op unless ``learning.optimizePrompts`` is on; never raises. Also off the turn's path, and off during an eval
    run (``SIERRALAB_REFLECT=off``), where a scripted turn is not evidence about anything."""
    try:
        if preferences.get("learning.optimizePrompts", False) is not True:
            return
        if os.environ.get("SIERRALAB_REFLECT", "").lower() == "off":
            return
        for kind in ("skill", "prompt"):
            optimize.cycle(cwd, kind=kind, preferences=preferences)
    except Exception:
        logging.getLogger(__name__).debug("optimize: drafting pass failed", exc_info=True)


def _note_answer(session_id: str, tool_name: str, approved: bool, key: str) -> None:
    """The person's answer to an approval, for the turn's outcome record."""
    recorder = outcomes.active(session_id)
    if recorder is not None:
        recorder.approval("approved" if approved else "rejected", tool_name, key=key)


def _laya_loaded() -> bool:
    """Whether Laya's weights are in memory — asked, never triggered: a turn must not wait on a model load."""
    from prompt_security import laya_ready

    return laya_ready()


def _route_of(router_hooks, agent) -> str | None:
    """The id of the route Laya chose for the turn in progress ("web", "code", …), or None."""
    decision = router_hooks.current(agent)
    target = getattr(decision, "target", None)
    return getattr(target, "id", None)


def _tool_kind(tool_name: str) -> str:
    return _TOOL_KIND.get(tool_name, "other")


def _text_of(blocks) -> str:
    return " ".join(b["text"] for b in blocks or [] if isinstance(b, dict) and isinstance(b.get("text"), str)).strip()


def _turn_context(agent) -> tuple[str, str]:
    """``(what the person asked, what the agent said just before the call)`` for an approval card.

    The newest assistant message is the one carrying the tool call being asked about, so its text
    blocks are the agent's own account of why; an older one would be a different step's. The goal is
    the newest user message that is the person's words rather than a tool result. Either may be empty
    — that is reported as empty, not guessed at."""
    goal = said = ""
    seen_assistant = False
    for message in reversed(list(getattr(agent, "messages", None) or [])):
        if not isinstance(message, dict):
            continue
        blocks = message.get("content") or []
        if message.get("role") == "assistant" and not seen_assistant:
            seen_assistant = True
            said = _text_of(blocks)
        elif message.get("role") == "user" and not any(isinstance(b, dict) and "toolResult" in b for b in blocks):
            text = _text_of(blocks)
            if text:
                goal = text
                break
    return goal, said


_ALLOW_KINDS = {"allow_once", "allow_always"}


def _permission_approved(outcome, options: list) -> bool:
    """True only when the client selected an option we offered as an allow kind.

    AllowedOutcome just means outcome == "selected" — it is also what a client
    sends when the user picks "Reject" — so the selected option_id has to be
    looked up in what we offered. Cancelled, unknown option ids, and reject
    kinds all deny.
    """
    if not isinstance(outcome, acp.schema.AllowedOutcome):
        return False
    selected = next((o for o in options if o.option_id == outcome.option_id), None)
    return selected is not None and selected.kind in _ALLOW_KINDS


def _answered_by(result) -> str | None:
    """The front door that answered, when the client said so in ACP's `_meta` slot.

    The daemon relays a permission answer itself when the decision came from a
    remote door — the Discord bot — and stamps `_meta.sierralab.decidedBy` on the
    response it writes to this child. Everything else (a TUI, the GUI's modal, a
    Zed session) sends no `_meta` and is a person on the socket. Returns None
    rather than a guess when the slot is absent or shaped differently."""
    meta = getattr(result, "field_meta", None)
    slot = meta.get("sierralab") if isinstance(meta, dict) else None
    door = slot.get("decidedBy") if isinstance(slot, dict) else None
    return str(door) if isinstance(door, str) and door else None


def _answer_note(result) -> str:
    """What the person wrote about a rejection, when a front door relayed it (`_meta.sierralab.note`). Their text, capped."""
    meta = getattr(result, "field_meta", None)
    slot = meta.get("sierralab") if isinstance(meta, dict) else None
    note = slot.get("note") if isinstance(slot, dict) else None
    return str(note).strip()[:500] if isinstance(note, str) else ""


def _settled_intervention(
    interrupt_id: str, session_id: str, tool_name: str, tool_input, since: float | None = None
) -> dict | None:
    """A previously recorded decision for this exact pause, if there is one.

    Strands derives the interrupt id from the tool call, not at random —
    `v1:before_tool_call:{toolUseId}:{uuid5(handler name)}` — so the same tool
    call raised again in the same session produces the same id. That is the case
    this exists for: `_stream` reports a turn's interrupts from two places (the
    `tool_interrupt_event` and `AgentResult.interrupts`), so without a record to
    consult the same call gets a second prompt within one turn. Replaying the
    answer is then the honest thing — the user already answered it.

    Only an *identical* call in the *same session* counts. The id is derived
    from a tool-use id that another session's model could in principle have
    minted too, and lending an answer across sessions would approve something no
    one was shown; a row whose tool or input differs is likewise a different
    pause. And only one staged during *this turn* (`since`): a local server that
    numbers its tool calls (`call_0`) reuses an id every turn, so without the
    bound an approval given last week for `ls` would silently approve the next
    `ls`, and a rejection would silently reject it forever. Anything unreadable,
    mismatched, stale or from another session re-prompts — the fail-closed
    direction.
    """
    row = state_store.get_intervention(interrupt_id)
    if not row or row.get("status") == "staged":
        return None
    if since is not None and (row.get("ts") or 0) < since:
        return None
    if row.get("session_id") != session_id or row.get("tool") != tool_name or row.get("tool_input") != tool_input:
        logging.getLogger(__name__).warning(
            "intervention %s does not match the call it is being replayed for (%r/%r != %r/%r); asking again",
            interrupt_id,
            row.get("session_id"),
            row.get("tool"),
            session_id,
            tool_name,
        )
        return None
    return row


# How much of a running specialist's latest reasoning one progress update carries.
_REASONING_TAIL_CHARS = 120


# Session Modes (https://agentclientprotocol.com/protocol/session-modes) —
# the same three-way split Claude Code itself exposes (plan / default /
# bypass-permissions), mapped onto real behavioral differences in
# _ApprovalHandler.before_tool_call below, not just relabeled UI state:
#   plan  — no tool actually runs; every call is denied with a message
#           telling the model to describe its plan instead.
#   build — today's default: only APPROVAL_REQUIRED_TOOLS pause for a real
#           permission prompt, everything else runs immediately.
#   auto  — routine calls never pause. Opt-in only; never the default. It does not
#           lift the floor in before_tool_call: deny rules, the vault's fail-closed
#           egress, a call Laya rates destructive, and model pulls/deletes still ask.
_MODES = [
    acp.schema.SessionMode(id="plan", name="Plan", description="Discuss and plan only — no tool actually runs."),
    acp.schema.SessionMode(id="build", name="Build", description="Tools run after your approval (default)."),
    acp.schema.SessionMode(
        id="auto",
        name="Auto",
        description="Routine tool calls run without asking. Still asks: anything Laya rates destructive, deny rules, model pulls/deletes. Use with caution.",
    ),
]
_MODE_IDS = {mode.id for mode in _MODES}
_DEFAULT_MODE = "build"

# Config option "temperature" exposes strands' set_temperature tool as a
# select instead of a free-float, since SessionConfigOptionSelect only takes
# discrete option ids — matches the granularity a picker in an editor UI
# actually wants (three presets, not a number field).
_TEMPERATURE_PRESETS = [("focused", 0.2, "Focused"), ("balanced", 0.7, "Balanced"), ("creative", 1.2, "Creative")]


def _closest_temperature_preset(temperature: float | None) -> str:
    if temperature is None:
        return "balanced"
    return min(_TEMPERATURE_PRESETS, key=lambda preset: abs(preset[1] - temperature))[0]


def _config_options(agent) -> list:
    """The live config-option list for one session's agent — always derived
    fresh from the agent's actual model config, never a separately-tracked
    copy that could drift from what's really active.
    """
    config = agent.model.get_config()
    current_model = config.get("model_id")
    current_temperature = (config.get("params") or {}).get("temperature")

    import ollama_cloud

    if isinstance(agent.model, ollama_cloud.OllamaCloudModel):
        # The registry (cloud tags, plus a local one already on disk), not oMLX's list.
        available_models = [spec.wire_id for spec in ollama_cloud.routable(agent.model.base_url)]
    else:
        available_models = _fetch_available_models()
    if current_model and current_model not in available_models:
        available_models = [current_model, *available_models]

    return [
        acp.schema.SessionConfigOptionSelect(
            type="select",
            id="model",
            name="Model",
            category="model",
            current_value=current_model or "",
            options=[acp.schema.SessionConfigSelectOption(value=model_id, name=model_id) for model_id in available_models],
        ),
        acp.schema.SessionConfigOptionSelect(
            type="select",
            id="temperature",
            name="Temperature",
            category="model_config",
            current_value=_closest_temperature_preset(current_temperature),
            options=[
                acp.schema.SessionConfigSelectOption(value=preset_id, name=label)
                for preset_id, _value, label in _TEMPERATURE_PRESETS
            ],
        ),
    ]


class _ApprovalHandler(InterventionHandler):
    """Gates dangerous tools via Strands' native Interventions system
    (strands.interventions — a transport-agnostic pause/resume mechanism
    built on the same Interrupt/InterruptException primitive ag_ui_strands
    uses internally for the web front door's HITL, but with no AG-UI runtime
    underneath it here).

    One instance per session, held onto directly (see SierraLabAcpAgent's
    _sessions dict) so set_session_mode() can flip ``self.mode`` on the same
    object the running agent's hook registry already points at — no need to
    rebuild the Agent (and lose its conversation state) just to change modes.

    Strands' ``Interrupt`` dataclass carries only ``id``/``name``/``reason``/
    ``response`` — no separate metadata slot — so the tool name/input/
    toolUseId this handler learns from ``event.tool_use`` (only available
    *here*, at interrupt-raise time) is packed into ``reason`` as JSON and
    read back in AcpAgent.prompt() once the interrupt actually resolves.
    """

    name = "acp-approval"

    def __init__(self, cwd: str | None = None, mode: str = "build", plugins=None, heal=None, router_hooks=None, vault=None) -> None:
        self.mode = mode
        self.cwd = cwd or os.getcwd()
        self.plugins = plugins  # PluginHost: runtime plugin tools' approval
        self.heal = heal  # SelfHealHooks: selfHeal.autoApply pre-approvals
        self.router_hooks = router_hooks  # RouterHooks: Laya's risk arming
        self.vault = vault  # vault_proxy.VaultStatus: egress proxy health
        self.session_agent = None  # the orchestrator; specialists share this handler
        self.session_id: str | None = None  # set by _build_session; keys the outcome recorder
        self.lab = None  # lab.Lab: per-pod Laya dial and the pod name carried on a prompt
        #: Laya Guard's reading of each call, by tool_use id (bounded): what the Lab's gauge shows for it.
        self._call_readings: "collections.OrderedDict[str, dict]" = collections.OrderedDict()
        #: tool_use id → checkpoint id, so a call that pauses for approval and is re-entered on resume is snapshotted once.
        self._checkpointed: "collections.OrderedDict[str, str | None]" = collections.OrderedDict()
        self._unprotected_said: set[str] = set()
        #: The bandit's view of this turn, stashed by the orchestrator thread so a tool call's ask decision is
        #: made in the same context the turn's other choices were (`SierraLabAcpAgent._apply_bandit`). None when
        #: the bandit is off or the turn abstained, in which case the ask arm is not consulted at all.
        self._bandit_decision: "RouteDecision | None" = None
        self._bandit_route: dict | None = None

    def before_tool_call(self, event):
        """Preflight for an edit (``preflight.py`` — before anyone is asked about it), then the decision
        (``_decide``), then — for any call that may change files and was not refused — a snapshot of the
        project (``checkpoints.py``), so that whatever ``auto`` mode or an approval let through can be undone.

        Only an edit takes the async path (the registry awaits it): its checks wait on a linter and a language
        server, and they run on a worker thread so the event loop keeps streaming and honouring cancel."""
        if self._wants_preflight(event):
            return self._preflighted(event)
        return self._gate(event)

    def _gate(self, event):
        decision = self._decide(event)
        if not isinstance(decision, Deny):
            try:
                self._checkpoint(event)
            except Exception:
                logging.getLogger(__name__).debug("checkpoint failed", exc_info=True)
        return decision

    def _wants_preflight(self, event) -> bool:
        """An edit that would otherwise go ahead or be asked about. Plan mode and a deny rule refuse it anyway, so
        it is not worth checking."""
        tool_name = str(event.tool_use.get("name") or "")
        tool_input = event.tool_use.get("input")
        if self.mode == "plan" or not preflight.is_edit(tool_name, tool_input):
            return False
        preferences = settings_store.current(self.cwd)
        if preferences.get("preflight.enabled", True) is False:
            return False
        manager = lsp_bridge.manager_for(self.cwd) if preferences.get("lsp.enabled", True) is not False else None
        if not preflight.applicable(tool_name, tool_input, manager, str(preferences.get("preflight.tests", "") or "")):
            return False
        verdict = settings_store.evaluate(preferences, tool_name, tool_input, self.cwd)
        return not (verdict is not None and verdict.action == "deny")

    async def _preflighted(self, event):
        try:
            refusal = await asyncio.to_thread(self._preflight, event)
        except Exception:
            logging.getLogger(__name__).warning("preflight failed; the edit goes to the gate unchecked", exc_info=True)
            refusal = None
        if refusal is not None:
            return Deny(reason=refusal)
        return self._gate(event)

    def _preflight(self, event) -> str | None:
        """The refusal to hand the agent, or None when the edit may go on to the gate (preflight.py: the loop)."""
        tool_name = str(event.tool_use.get("name") or "")
        tool_input = event.tool_use.get("input")
        preferences = settings_store.current(self.cwd)
        recorder = outcomes.active(self.session_id)
        turn = recorder.id if recorder is not None else ""
        track = preflight.tracker(self.session_id or "")
        stopped = track.stopped(turn)
        if stopped:
            return stopped
        manager = lsp_bridge.manager_for(self.cwd) if preferences.get("lsp.enabled", True) is not False else None
        egress_blocked = self.vault is not None and self.vault.blocks_egress
        report = preflight.check(
            self.cwd,
            tool_name,
            tool_input,
            manager=manager,
            # A test command runs the project's code, which may reach the network: not while the vault refuses egress.
            tests="" if egress_blocked else str(preferences.get("preflight.tests", "") or ""),
            trusted=bool(getattr(preferences, "trusted", False)),
        )
        if report.ok:
            track.passed(turn, report.files)
            if report.ran and not report.cached:
                preflight.announce("passed", self.session_id, tool_name, report)
            return None
        retries = max(0, int(float(preferences.get("preflight.maxRetries", preflight.DEFAULT_RETRIES))))
        attempt = track.failed(turn, report.files)
        if attempt <= retries:
            preflight.announce("failed", self.session_id, tool_name, report, attempt=attempt, retries=retries)
            return preflight.failure_text(tool_name, report, attempt, retries)
        rolled = None
        if turn and preferences.get("preflight.rollback", True) is not False and preferences.get("checkpoints.enabled", True) is not False:
            rolled = preflight.rollback(self.cwd, turn, self.session_id or "")
        text = preflight.give_up_text(tool_name, report, retries, rolled)
        track.give_up(turn, "[preflight] Editing has stopped for this turn: an edit failed its checks too many times. Report to the user instead of editing.")
        preflight.announce("gave_up", self.session_id, tool_name, report, attempt=attempt, retries=retries, rolled_back=(rolled or {}).get("count", 0), undo_with=(rolled or {}).get("undo_with"))
        return text

    def _checkpoint(self, event) -> None:
        tool_name = str(event.tool_use.get("name") or "")
        tool_input = event.tool_use.get("input")
        if not checkpoints.writes_files(tool_name, tool_input, GENERATED_TOOL_NAMES):
            return
        use_id = str(event.tool_use.get("toolUseId") or "")
        if use_id and use_id in self._checkpointed:
            return
        preferences = settings_store.current(self.cwd)
        if not preferences.get("checkpoints.enabled", True):
            return
        recorder = outcomes.active(self.session_id)
        pod = self.lab.pod_of(event.agent) if self.lab is not None else None
        agent = pod.name if pod is not None else str(getattr(event.agent, "name", "") or "")
        result = checkpoints.take(
            self.cwd,
            kind="pre",
            tool=tool_name,
            agent=agent,
            session_id=self.session_id or "",
            turn=recorder.id if recorder is not None else "",
            call=laya_guard.render_call(tool_name, tool_input, self.cwd).split(" tool: ", 1)[-1].replace("\n", " ")[:200],
            paths=outcome_hooks.edit_paths(tool_name, tool_input),
            max_file_mb=float(preferences.get("checkpoints.maxFileMb", checkpoints.DEFAULT_MAX_FILE_MB)),
        )
        if use_id:
            self._checkpointed[use_id] = result.id
            while len(self._checkpointed) > 128:
                self._checkpointed.popitem(last=False)
        if recorder is not None:
            recorder.checkpoint(result.ok, result.id, result.reason, len(result.outside))
        if (not result.ok or result.outside) and (result.reason or "outside") not in self._unprotected_said:
            # Said once per reason per session, in the ledger and to the client: a change that cannot be
            # undone is exactly what the person should be told, and not on every call.
            self._unprotected_said.add(result.reason or "outside")
            payload = {"event": "checkpoint", "phase": "unprotected" if not result.ok else "outside", "tool": tool_name, "reason": result.reason, "outside": result.outside[:5]}
            harness_events.emit("checkpoint", payload)
            state_store.append_event("checkpoint", payload, session_id=self.session_id, source="checkpoints")

    def _can_egress(self, tool_name: str) -> bool:
        """Could this call reach the network around the vault? The named egress tools, and — found in the Phase 11
        audit — two kinds the name list missed: a generated skill (it runs in this process and can open its own
        connection) and an external plugin's tool (its server is a process of its own, started with whatever proxy
        environment existed then). With the vault down and `egressPolicy: deny`, all of them are refused."""
        if tool_name in vault_proxy.EGRESS_TOOLS or tool_name in MCP_TOOL_NAMES or tool_name == plugin_host.RESOURCE_TOOL:
            return True
        try:
            if tool_name in current_generated_tool_names():
                return True
        except Exception:
            return tool_name in GENERATED_TOOL_NAMES
        return self.plugins is not None and self.plugins.requires_approval(tool_name) is not None

    def _decide(self, event):
        """Mode first, with the user's permission rules (settings_store.py) laid
        over it: a ``deny`` rule blocks in every mode — ``auto`` included — and an
        ``allow`` / ``ask`` rule decides whether a call pauses for approval.

        Then the harness layers, none of which can *remove* a deny or plan block:
        Laya arming (a confidently destructive turn asks even in auto mode), the
        self-heal loop's two bounded pre-approvals, and per-plugin approval."""
        tool_name = event.tool_use.get("name")
        tool_input = event.tool_use.get("input")

        # A step repeated with the same result, over and over, is not run again (stall_guard.py).
        if self.session_id is not None:
            who = "orchestrator" if event.agent is self.session_agent else str(getattr(event.agent, "name", "") or "agent")  # as StallHooks names them
            stalled = stall_guard.for_session(self.session_id).before(who, tool_name, tool_input)
            if stalled:
                return Deny(reason=stalled)

        # Read once: `evaluate` below and the risk score recorded with the
        # prompt must come from the same settings, or the ledger's explanation
        # of why a call was held up disagrees with the decision itself.
        preferences = settings_store.current(self.cwd)
        verdict = settings_store.evaluate(preferences, tool_name, tool_input, self.cwd)
        if verdict is not None and verdict.action == "deny":
            return Deny(reason=f"{tool_name} was not run: blocked by a {verdict.explain()}. Do something else or ask the user.")
        egress = self._can_egress(tool_name)
        if self.vault is not None and self.vault.blocks_egress and egress:
            # The vault may have come up since the session started; the refusal
            # below tells the user to start it, so honour that without a restart.
            self.vault = vault_proxy.reprobe(self.vault)
        if self.vault is not None and self.vault.blocks_egress and egress:
            # Fail closed *before* the mode logic: with the vault enabled and
            # unreachable, nothing that could carry a credential off the machine
            # may run, and `auto` mode must not be able to wave it through.
            # There is no prompt here on purpose — a pause the user answers "yes"
            # to is exactly the fail-open path this refuses.
            return Deny(
                reason=(
                    f"{tool_name} was not run: {self.vault.reason}. "
                    "Start the Agent Vault, or set vault.egressPolicy to \"proxy\" to route through "
                    "the configured address without vault credentials."
                )
            )
        read_only = _read_only_call(tool_name, tool_input)
        if self.mode == "plan":
            if tool_name in PLAN_MODE_TOOLS or read_only:
                return Proceed()
            return Deny(reason=f"Plan mode is active — {tool_name} was not run. Describe the change instead of making it.")
        forced = _forced_ask(tool_name, tool_input)
        # Laya Guard: this call, by this agent, scored on its own — not the turn's one reading, and
        # not the orchestrator's standing in for a specialist's. A call at or above the calling
        # agent's gate asks in every mode, auto included; below it, nothing is added or waived.
        pod = self.lab.pod_of(event.agent) if self.lab is not None else None
        call_risk = self._call_risk(tool_name, tool_input, read_only, preferences)
        gate = laya_guard.call_gate(preferences, pod.dial if pod is not None else None)
        call_armed = call_risk is not None and call_risk.score >= gate
        turn_armed = (
            self.router_hooks is not None
            and self.session_agent is not None
            and tool_name in _ARMABLE_TOOLS
            and not read_only
            and (self.router_hooks.armed(self.session_agent) or self._dial_armed(event, preferences))
        )
        armed = forced is not None or turn_armed or call_armed or self._bandit_ask(call_risk, gate, preferences)
        if self.mode == "auto" and not armed:
            self._note_call(event, call_risk, "auto")
            return Proceed()
        if not armed and verdict is not None and verdict.action == "allow":
            self._note_call(event, call_risk, "allowed")
            return Proceed()
        if not armed and self.heal is not None and self.heal.preapproved(tool_name, tool_input):
            harness_events.emit("heal", {"event": "heal", "phase": "auto_approved", "tool": tool_name})
            return Proceed()
        plugin_gate = self.plugins.requires_approval(tool_name) if self.plugins is not None else None
        if tool_name == plugin_host.RESOURCE_TOOL and self.plugins is not None and isinstance(tool_input, dict):
            # Listing is metadata (read_only, never asks); a read asks as that server's own tools do.
            plugin_gate = None if read_only else self.plugins.requires_approval_for_server(str(tool_input.get("server") or ""))
        if not armed and verdict is None:
            if plugin_gate is False:
                return Proceed()
            if plugin_gate is None and (tool_name not in APPROVAL_REQUIRED_TOOLS or read_only):
                return Proceed()

        risk = self._risk(preferences)
        # The reading shown with the prompt is the higher of the call's and the turn's: it is what made it ask.
        events = [e for e in ((call_risk.to_event() if call_risk is not None else None), (risk.to_event() if risk else None)) if e]
        shown = max(events, key=lambda e: e.get("score") or 0.0) if events else None
        if forced:
            why = forced
        elif call_armed:
            why = f"Laya Guard: {call_risk.reason} — risk {call_risk.score:.2f} is at or above this agent's gate of {gate:.2f}"
        else:
            why = "Laya rated this turn destructive — asking even in auto mode"
        self._note_call(event, call_risk, "asked", shown)
        payload = json.dumps(
            {
                "name": tool_name,
                "pod": pod.name if pod is not None else None,
                "input": tool_input,
                "tool_use_id": event.tool_use.get("toolUseId"),
                "always_rule": None if armed else settings_store.suggest_rule(tool_name, tool_input),
                **({"reason": why} if armed else {}),
                **({"risk": shown} if shown else {}),
            }
        )
        return Confirm(prompt=payload)

    def _bandit_ask(self, call_risk, gate: float, preferences) -> bool:
        """Whether the bandit adds an ask for this call — the *only* bandit read inside an approval, and a
        tighten-only OR-term: it can turn a would-be ``Proceed`` into a ``Confirm`` and can never do the reverse.

        Additive-only by construction, in three ways at once: the only arm that changes anything is ``ask`` (the
        other, ``none``, is the status quo — there is no arm that *removes* a prompt), the precondition is that the
        Guard already scored this call **at or above this agent's gate**, and the return is ORed into ``armed`` so
        nothing downstream can be un-armed by it. Because that precondition is exactly ``call_armed``, with the
        guard reading the same call the bandit adds nothing in v1 — deliberately. Its job here is to put the *ask*
        decision on the turn's trajectory (``choices["ask"]``) so a later scorer can be judged against it, and to
        make the clamp structural rather than a review note. Suppressing a discretionary ask is out of scope for
        v1 and has no code path.

        False whenever the bandit is off/shadow, the turn is not one the bandit looked at (abstained, or before
        ``_apply_bandit``), the call is unscored, or the posterior is too thin.
        """
        decision = self._bandit_decision
        stub = self._bandit_route
        if decision is None or stub is None or call_risk is None:
            return False
        try:
            policy = _bandit_policy(preferences)
            if policy["mode"] != "live" or float(call_risk.score) < float(gate):
                return False
            recorder = outcomes.active(self.session_id)
            table = bandit.for_project(self.cwd)
            arm, info = table.choose(
                "ask",
                bandit.context(stub, "ask"),
                ["none", "ask"],
                "none",
                mode=policy["mode"],
                explore=policy["explore"],
                min_pulls=policy["min_pulls"],
                lam=policy["lam"],
                mu=policy["mu"],
            )
            bandit.decision_event(info, turn=recorder.id if recorder is not None else "", project=self.cwd)
            # Record the arm either way: the "none" half is what the "ask" arm is learned against.
            decision.choices["ask"] = arm
            return arm == "ask"
        except Exception:
            logging.getLogger(__name__).debug("bandit: ask read failed", exc_info=True)
            return False

    def _call_risk(self, tool_name: str, tool_input, read_only: bool, preferences) -> "laya_guard.CallRisk | None":
        """Laya Guard's reading of this one call, or None (a tool that only reads, the guard off, or a
        failure — an unreadable call is never *scored safe*, it is simply not scored)."""
        if read_only or tool_name not in _ARMABLE_TOOLS:
            return None
        try:
            router = getattr(self.router_hooks, "router", None)
            return laya_guard.assess_call(tool_name, tool_input, cwd=self.cwd, preferences=preferences, calibrator=getattr(router, "calibrator", None))
        except Exception:
            logging.getLogger(__name__).debug("laya guard failed", exc_info=True)
            return None

    def reading_for(self, tool_use_id: str | None, agent) -> dict | None:
        """What the gauge should show for the call about to run: the higher of Laya Guard's reading of
        the call and Laya's of the turn. None when there is neither."""
        turn = self._risk(settings_store.current(self.cwd))
        events = [e for e in ((self._call_readings.get(str(tool_use_id)) if tool_use_id else None), (turn.to_event() if turn else None)) if e]
        return max(events, key=lambda e: e.get("score") or 0.0) if events else None

    def _record_approval(self, event, kind: str, score: float | None) -> None:
        recorder = outcomes.active(self.session_id)
        if recorder is not None:
            recorder.approval(kind, str(event.tool_use.get("name") or ""), score, key=str(event.tool_use.get("toolUseId") or "") or None)

    def _note_call(self, event, call_risk, decision: str, gauge: dict | None = None) -> None:
        """Show a scored call on its agent's pod and gauge, and leave a trail when auto mode let a
        call that was not trivially safe run: "who approved that?" must have an answer even when the
        answer is "auto mode, at 0.55"."""
        if call_risk is not None or decision == "asked":
            self._record_approval(event, decision, call_risk.score if call_risk is not None else None)
        if call_risk is None or self.lab is None:
            return
        pod = self.lab.pod_of(event.agent)
        tool = event.tool_use.get("name") or "tool"
        reading = call_risk.to_event()
        tool_use_id = str(event.tool_use.get("toolUseId") or "")
        if tool_use_id:
            self._call_readings[tool_use_id] = reading
            while len(self._call_readings) > 64:
                self._call_readings.popitem(last=False)
        if pod is not None:
            self.lab.note_call(pod.name, tool, reading, decision, gauge=gauge or self.reading_for(tool_use_id, event.agent))
        safe, _ = laya_risk.thresholds(settings_store.current(self.cwd))
        if decision == "auto" and call_risk.score >= safe:
            summary = secret_guard.redact(" ".join(str(event.tool_use.get("input")).split()))[:200]
            state_store.append_event(
                "auto_approved",
                {"pod": pod.name if pod is not None else None, "tool": tool, "score": call_risk.score, "band": call_risk.band, "reason": call_risk.reason, "source": call_risk.source, "call": summary},
                session_id=self.lab.session_id,
                source="laya_guard",
            )

    def _dial_armed(self, event, preferences) -> bool:
        """The Lab's per-pod Laya dial: a score above this pod's dial arms the gate
        like a router-armed turn. Tightens only — see ``lab.Lab.dial_armed``."""
        if self.lab is None:
            return False
        pod = self.lab.pod_of(event.agent)
        if pod is None or pod.dial is None:
            return False
        risk = self._risk(preferences)
        return risk is not None and self.lab.dial_armed(event.agent, risk.score)

    def _risk(self, preferences) -> laya_risk.RiskAssessment | None:
        """Laya's 0.0-1.0 destructiveness score for the turn in progress.

        `None` when there is nothing to score — the router is off, or it
        abstained before the tool gate ran. Carried through the interrupt's
        payload (Strands' `Interrupt` has no metadata slot) so the staged
        intervention and the `intervention` event can both say *why* the gate
        paused, not just that it did.
        """
        if self.router_hooks is None or self.session_agent is None:
            return None
        decision = self.router_hooks.current(self.session_agent)
        if decision is None:
            return None
        router = getattr(self.router_hooks, "router", None)
        return laya_risk.assess(
            decision,
            preferences=preferences,
            calibrator=getattr(router, "calibrator", None),
        )


def _replay_history(messages: list[dict]) -> list:
    """Turn an Agent's restored message history into ACP session/update
    notifications, for load_session — Zed rebuilds its chat view entirely
    from these, since it has no other way to see a conversation that started
    before this process existed.
    """
    updates = []
    for message in messages:
        role = message.get("role")
        for block in message.get("content") or []:
            text = block.get("text") if isinstance(block, dict) else None
            if not text:
                continue
            if role == "user":
                updates.append(acp.update_user_message_text(text))
            elif role == "assistant":
                updates.append(acp.update_agent_message_text(text))
    return updates


def _project_system_prompt(cwd: str) -> str:
    """agent_common.SYSTEM_PROMPT hardcodes backend/workspace/ as the
    sandbox root — correct for the AG-UI/web front door, actively wrong
    here: an ACP client (Zed) is meant to work on whatever project it has
    open, told to us via new_session/load_session's own ``cwd`` — a totally
    different project every time, not one fixed scratch directory. Real bug
    this fixes: the agent used to answer "what project are we in?" by
    describing this repo's own backend, regardless of what Zed had open,
    because nothing here ever read the ``cwd`` the protocol was already
    handing over.
    """
    return (
        f"You are a coding agent working in the project at {cwd}. Your shell, file, and code tools "
        "operate relative to this directory — explore it first if you're not sure what's there. "
        "To find code, search — don't read: code_search (ripgrep) finds where a name or string is defined or used, "
        "lsp_definition / lsp_references / lsp_hover give the exact semantic answer once you have a position, and "
        "code_outline lists a file's classes and functions or returns one of them. Read a whole file only when it is "
        "short; a long one comes back as an outline. "
        "For multi-line or multi-file changes prefer apply_patch (a unified diff). After a command or "
        "test fails, read the [self-heal] notes appended to its result: they carry a root cause and, "
        "usually, a patch already verified in a sandbox copy. "
        "For a large refactor or implementation across many files, hand the whole task to claude_code_engineer "
        "(it runs the Claude Code CLI headless in this project and reports back); for multi-step work where a plan, "
        "an independent review and a check against the plan matter, use orchestrate. Judge either's result before "
        "you tell the user it is done. For planning, ask the architect; to condense bulk text, use context_digest. "
        + ollama_resource_tool.MEMORY_GUIDANCE
    )


def _is_chat_scratch(cwd: str) -> bool:
    """The folder a Discord chat with no project runs in (daemon/chat.py `workspace_dir`). Compared by path rather
    than imported: the daemon package is not this process's to load."""
    try:
        return os.path.realpath(cwd) == os.path.realpath(settings_store.user_dir() / "discord-workspace")
    except OSError:
        return False


def _chat_system_prompt(cwd: str) -> str:
    """For a chat with no project. The coding prompt above told the model its tools "operate relative to" an empty
    scratch folder, and it concluded — and told the operator, three times — that it could not see ~/Downloads, then
    guessed a filename instead of listing the folder. The tools were never confined; the prompt was."""
    home = str(Path.home())
    return (
        f"You are the operator's personal agent on their Mac, reached from a chat. {cwd} is an empty scratch folder, "
        f"not a project: their files are where they keep them — home is {home} (Downloads, Documents, Desktop, …). "
        "Your tools reach any path on this machine; use absolute paths. Never guess a filename: list first. "
        f"To find a file by name, call file_read with mode \"find\" and a glob of one keyword, e.g. path "
        f"\"{home}/Downloads/*figma*\" (case does not matter); then file_read the path it found (a PDF comes back as "
        "text). shell (ls, mdfind) works too but asks the "
        "operator first. Risky calls ask first; reading does not. "
        + ollama_resource_tool.MEMORY_GUIDANCE
    )


def _extract_prompt_text(blocks: list) -> str:
    """Text + @-mentioned content from one prompt turn.

    Previously only TextContentBlock was read — every ResourceContentBlock
    (a plain @-mention, Zed sends just a name/uri/description, no content)
    and EmbeddedResourceContentBlock (Zed inlined the actual file text) was
    silently dropped, so mentioning a file in Zed never actually reached the
    model. An embedded resource's real text is inlined directly; a bare
    reference is turned into a line the model can act on itself (it has
    file_read).
    """
    parts: list[str] = []
    for block in blocks:
        block_type = getattr(block, "type", None)
        if block_type == "text":
            parts.append(block.text)
        elif block_type == "resource_link":
            label = getattr(block, "title", None) or getattr(block, "name", block.uri)
            parts.append(f"[Referenced file: {label} ({block.uri})]")
        elif block_type == "resource":
            resource = block.resource
            uri = getattr(resource, "uri", "unknown")
            text = getattr(resource, "text", None)
            parts.append(f"[Attached: {uri}]\n{text}" if text is not None else f"[Attached binary resource: {uri}]")
    return "\n\n".join(part for part in parts if part)


def _apply_model(agent, value) -> bool:
    """Switch the session's model; False if the provider doesn't offer it."""
    if not isinstance(value, str):
        return False
    import ollama_cloud

    if isinstance(agent.model, ollama_cloud.OllamaCloudModel):
        # Through the registry, so a local-weights tag that is not installed is
        # refused with a reason instead of 404-ing on the next request.
        try:
            spec = agent.model.switch_model(value)
        except (ollama_cloud.UnknownModel, ollama_cloud.ModelUnavailable) as error:
            logging.getLogger(__name__).warning("model=<%s> | %s, keeping the current model", value, error)
            return False
        import agent_common

        agent_common.OLLAMA_MODEL_ID = spec.alias  # new sessions inherit the switch
        return True
    available = _fetch_available_models()
    if available and value not in available:
        logging.getLogger(__name__).warning("model=<%s> | not offered by the provider, keeping the current model", value)
        return False
    config_update: dict = {"model_id": value}
    limit = _context_window_limit(value)
    if limit is not None:
        config_update["context_window_limit"] = limit
    agent.model.update_config(**config_update)

    import agent_common

    agent_common.ACTIVE_MODEL_ID = value
    return True


def _model_for_vision(agent, vision: dict) -> tuple[object | None, str | None]:
    """Point the session's model at the vision model for one screenshot turn. Returns a function that
    puts the session's own model back, or ``None`` and the reason the image cannot be sent.

    Not `_apply_model`: that is a switch the person makes, and it moves the defaults later sessions
    inherit. This is one turn's detour, and it checks what a model switch does not — where the pixels
    would go, and whether the turn's history can forget them afterwards:

    * **Ollama only.** Its local daemon serves local tags and `:cloud` tags behind one endpoint, so
      "is this on my machine" has an answer (the tag, and `/api/tags`). oMLX would load a vision model
      outside the memory budget (`PrismModel.require_loaded`), and any other provider is off-machine
      as far as this harness can vouch — both are refused rather than guessed at.
    * **Local unless the person said otherwise** (`screenshot_cloud_ok`, from `capture.screenshotCloud`).
    * **Never pulled.** A local tag that is not installed is refused, the registry's own rule.
    * **Forgettable.** The image is stripped from the history when the turn ends and the snapshot
      re-saved; a legacy per-message store cannot rewrite what it already wrote, so it is refused.
    """
    import ollama_cloud

    model = agent.model
    target = str(vision.get("model") or "").strip()
    if not isinstance(model, ollama_cloud.OllamaCloudModel):
        return None, f"the screenshot was not sent: vision turns run on the Ollama provider, and this session uses {type(model).__name__}"
    if not isinstance(getattr(agent, "_session_manager", None), session_store.SnapshotSessionManager | type(None)):
        return None, "the screenshot was not sent: this session's store cannot remove an image after the turn"
    cloud = os_context.is_cloud_model(target)
    if cloud and not vision.get("cloud_ok"):
        return None, f"the screenshot was not sent: {target} is a cloud model and screenshots stay on this machine (capture.screenshotCloud)"
    if not cloud and target not in ollama_cloud.installed_local_tags(model.base_url):
        return None, f"the screenshot was not sent: {target} is not installed in the local Ollama daemon (this harness never pulls models)"
    before_id = model.get_config().get("model_id")
    before_spec = getattr(model, "spec", None)
    model.update_config(model_id=target)

    def restore() -> None:
        model.update_config(model_id=before_id)
        if before_spec is not None:
            model.spec = before_spec

    return restore, None


def _apply_temperature(agent, value) -> bool:
    preset = next((p for p in _TEMPERATURE_PRESETS if p[0] == value), None)
    if preset is None:
        return False
    current_params = dict(agent.model.get_config().get("params") or {})
    current_params["temperature"] = preset[1]
    agent.model.update_config(params=current_params)

    import agent_common

    agent_common.ACTIVE_TEMPERATURE = preset[1]
    return True


def _apply_startup_preferences(preferences) -> None:
    """Preferences that configure modules which read them when a session is
    built (Laya's mode and gate, the sub-agent model tiers). A value in a
    settings file beats the env/.env default; unset keys leave the env alone."""
    if preferences.get("laya.mode"):
        app_settings.LAYA_TRIAGE_MODE = preferences.get("laya.mode")
    if preferences.get("laya.threshold") is not None:
        app_settings.LAYA_TRIAGE_THRESHOLD = float(preferences.get("laya.threshold"))
    for tier in ("fast", "standard", "strong"):
        if preferences.get(f"tiers.{tier}"):
            setattr(app_settings, f"MODEL_TIER_{tier.upper()}", preferences.get(f"tiers.{tier}"))


def _router_policy(preferences) -> Policy:
    return Policy(
        gate=float(preferences.get("laya.threshold", app_settings.LAYA_TRIAGE_THRESHOLD)),
        min_confidence=float(preferences.get("laya.minConfidence", 0.5)),
        dispatch=preferences.get("laya.dispatch", "direct"),
        team_dispatch=preferences.get("laya.teamDispatch", "direct"),
        on_abstain=preferences.get("laya.onAbstain", "escalate"),
        frontier_model=preferences.get("tiers.strong") or app_settings.MODEL_TIER_STRONG or None,
    )


def _heal_config(preferences) -> HealConfig:
    return HealConfig(
        enabled=preferences.get("selfHeal.enabled", True) is not False,
        max_attempts=int(preferences.get("selfHeal.maxAttempts", 3)),
        verify=preferences.get("selfHeal.verify", "sandbox"),
        auto_apply=preferences.get("selfHeal.autoApply", False) is True,
        memory=preferences.get("selfHeal.memory", True) is not False,
    )


# The marker a client sends (acp-tui's /resume-turn) to continue a turn that a
# crash or kill interrupted between two checkpoints.
RESUME_TURN_MARKER = "[resume interrupted turn]"
#: What a client sends when it resumes on its own (``selfHeal.autoResume``): the same turn, but counted, so a
#: turn that crashes the process every time it is resumed is tried twice and then left for a person.
AUTO_RESUME_MARKER = "[auto-resume interrupted turn]"
MAX_AUTO_RESUMES = 2
#: The longest single JSON-RPC line this child reads from its ACP client (see `_main`).
STDIO_LINE_LIMIT = 32 * 1024 * 1024
#: After a cancel, how long the running turn is given to stop on its own (Strands' next checkpoint, a tool that polls the
#: cancel signal) before it is abandoned.
CANCEL_GRACE_SECONDS = 3.0
#: Model calls one turn may make before it pauses (`context.maxSteps`; 0 = no cap). Enough for a large task, small
#: enough that a loop which will not converge does not run on spending the person's tokens.
DEFAULT_MAX_STEPS = 50
AUTO_RESUME_MAX_AGE = 30 * 60


def _is_resume(text: str) -> bool:
    return text.strip() in (RESUME_TURN_MARKER, AUTO_RESUME_MARKER)


def _resume_marker_path(session_id: str) -> Path:
    return SESSIONS_DIR / f"session_{session_id}.resume.json"


def _checkpoint_fingerprint(session_id: str) -> str | None:
    try:
        info = _checkpoint_path(session_id).stat()
    except OSError:
        return None
    return f"{info.st_mtime_ns}:{info.st_size}"


def _auto_resume_attempts(session_id: str) -> int:
    """Automatic resumes already spent on *this* checkpoint. A turn that got further before dying wrote a new
    checkpoint, so its count starts again — only a turn that dies at the same point every time runs out."""
    try:
        data = json.loads(_resume_marker_path(session_id).read_text())
    except (OSError, ValueError):
        return 0
    return int(data.get("attempts", 0)) if data.get("fingerprint") == _checkpoint_fingerprint(session_id) else 0


def _count_auto_resume(session_id: str) -> None:
    try:
        _resume_marker_path(session_id).write_text(json.dumps({"fingerprint": _checkpoint_fingerprint(session_id), "attempts": _auto_resume_attempts(session_id) + 1}))
    except OSError:
        pass
_LAYA_DECIDE_TIMEOUT = 2.0
_SPECIALIST_CONTEXT_CHARS = 4000


def _checkpoint_path(session_id: str) -> Path:
    return SESSIONS_DIR / f"session_{session_id}.checkpoint.json"


_INDEX_PATH = SESSIONS_DIR / "index.json"
_INDEX_LOCK = threading.Lock()


def _read_index() -> dict:
    try:
        data = json.loads(_INDEX_PATH.read_text(encoding="utf-8"))
        return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        return {}


def _touch_index(session_id: str, cwd: str, title: str | None = None) -> None:
    """sessions/index.json: id -> cwd / title / updatedAt, for session/list."""
    with _INDEX_LOCK:
        data = _read_index()
        entry = data.get(session_id, {})
        entry.update({"cwd": cwd, "updatedAt": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())})
        if title:
            entry["title"] = title
        data[session_id] = entry
        try:
            tmp = _INDEX_PATH.with_suffix(".tmp")
            tmp.write_text(json.dumps(data, indent=1), encoding="utf-8")
            os.replace(tmp, _INDEX_PATH)
        except OSError:
            pass


def _recent_context(messages: list, limit: int = _SPECIALIST_CONTEXT_CHARS) -> str:
    """The last turns' text, newest last, for a specialist that starts fresh."""
    parts: list[str] = []
    used = 0
    for message in reversed(messages):
        texts = [b["text"] for b in message.get("content") or [] if isinstance(b, dict) and isinstance(b.get("text"), str)]
        if not texts:
            continue
        line = f"{message.get('role')}: {' '.join(texts)}"
        if used + len(line) > limit:
            break
        parts.append(line)
        used += len(line)
    return "\n".join(reversed(parts))


class _SpecialistProgress:
    """A running specialist's activity, rendered into its tool card: the tools
    it called and the tail of what it is writing."""

    def __init__(self) -> None:
        self.lines: list[str] = []
        self.seen: set[str] = set()
        self.text = ""

    def feed(self, data: dict) -> bool:
        changed = False
        if data.get("type") == "tool_use_stream":
            current = data.get("current_tool_use") or {}
            tool_id = current.get("toolUseId")
            if tool_id and tool_id not in self.seen:
                self.seen.add(tool_id)
                self.lines.append(f"→ {current.get('name') or 'tool'}")
                changed = True
        elif isinstance(data.get("data"), str):
            self.text += data["data"]
            changed = len(self.text) % 48 < len(data["data"])  # re-render every ~48 chars
        return changed

    def render(self, name: str) -> str:
        lines = [f"{name}: " + line for line in self.lines[-8:]]
        tail = " ".join(self.text.split())[-_REASONING_TAIL_CHARS:]
        if tail:
            lines.append(f"thinking: {name} - {tail}")
        return "\n".join(lines)


@dataclass
class _Session:
    agent: object
    handler: _ApprovalHandler
    cwd: str
    router_hooks: RouterHooks
    heal: SelfHealHooks
    specialists: dict = field(default_factory=dict)
    plugins: PluginHost | None = None
    #: The settings resolved for this session's cwd. Held so a turn can tell
    #: whether the trace corpus is switched on (trace_store.enabled) without
    #: re-reading the files, which would pick up an edit made mid-session and
    #: disagree with every other decision this session already made from these.
    preferences: object = None


_PLUGIN_HOSTS: dict[tuple[str, bool], PluginHost] = {}
_ROUTER: LayaRouter | None = None


def _plugin_host(cwd: str, reserved: set[str], traces_enabled: bool = False) -> PluginHost:
    """One host per project per process. The memory store and the LSP tools are
    built-in plugins: listed, swappable and disable-able like an MCP server.

    `traces_enabled` is part of the cache key rather than a runtime toggle: a
    trace tool with nothing behind it is worse than no trace tool, so the plugin
    only exists when `memory.traces` says the corpus does. That means `/plugins`
    lists it exactly when it works."""
    host = _PLUGIN_HOSTS.get((cwd, traces_enabled))
    if host is None:
        from agent_memory_tool import agent_memory

        builtins = [
            PluginSpec(
                name="memory",
                kind="memory",
                transport="builtin",
                source="built-in",
                description="long-term semantic memory (Qdrant); swap in another memory MCP server by disabling this",
                builtin_tools=lambda: [agent_memory],
            ),
            PluginSpec(
                name="lsp",
                kind="lsp",
                transport="builtin",
                source="built-in",
                description="language-server tools: diagnostics, definitions, references, hover, symbols",
                builtin_tools=lambda: list(lsp_bridge.LSP_TOOLS),
            ),
        ]
        if traces_enabled:
            builtins.append(
                PluginSpec(
                    name="traces",
                    kind="memory",
                    transport="builtin",
                    source="built-in",
                    description="search this project's recorded execution traces (memory.traces)",
                    # Built on first use: `_tool` imports strands and builds the
                    # @tool wrapper, which a plugin that is switched off must
                    # not pay for.
                    builtin_tools=lambda: [trace_store._tool()],
                    # `allow` (not `ask`): searching the corpus changes nothing,
                    # and a built-in does not need trust for it to be honoured.
                    # `untrusted_output` stays at its default: a trace holds
                    # command output and file contents, which is exactly the
                    # text the guard exists to fence off.
                    approval="allow",
                )
            )
        host = _PLUGIN_HOSTS[(cwd, traces_enabled)] = PluginHost(cwd, builtins=builtins, reserved_names=reserved)
    return host


def _router() -> LayaRouter:
    global _ROUTER
    if _ROUTER is None:
        _ROUTER = LayaRouter()
    return _ROUTER


def _settle_cancelled_turn(agent) -> None:
    """Leave a cancelled agent reusable.

    A turn cancelled right after an approval was answered (an E-Stop, or Esc a
    moment after "Allow") is abandoned while Strands still believes it is mid-
    resume, so the *next* prompt dies with "must resume from interrupt". It also
    leaves the model's last tool call without a result, which providers reject.
    Close both: drop the interrupt state, and answer any dangling tool call with
    an error result saying it did not run."""
    state = getattr(agent, "_interrupt_state", None)
    if state is not None and getattr(state, "activated", False):
        state.deactivate()
    messages = getattr(agent, "messages", None)
    if not messages or messages[-1].get("role") != "assistant":
        return
    uses = [block["toolUse"] for block in messages[-1].get("content") or [] if isinstance(block, dict) and "toolUse" in block]
    if not uses:
        return
    message = {
        "role": "user",
        "content": [
            {"toolResult": {"toolUseId": use["toolUseId"], "status": "error", "content": [{"text": "cancelled: this call did not run"}]}}
            for use in uses
        ],
    }
    messages.append(message)
    manager = getattr(agent, "_session_manager", None)
    if manager is not None:
        try:
            session_store.record(manager, message, agent)
        except Exception:
            logging.getLogger(__name__).debug("could not persist the cancelled tool results", exc_info=True)


class SierraLabAcpAgent:
    """Implements initialize, new_session, load_session, list_sessions, prompt,
    cancel, set_session_mode, set_config_option and the ``_sierralab/*``
    extension methods. fork_session/resume_session/authenticate aren't wired up
    (nothing here needs forking or auth).
    """

    def __init__(self) -> None:
        self._conn: acp.Client | None = None
        self._sessions: dict[str, _Session] = {}
        self._cancelled: set[str] = set()
        #: session -> the agent whose invocation is running right now (see `cancel`).
        self._streaming: dict[str, Agent] = {}
        self._loop: asyncio.AbstractEventLoop | None = None
        self._unsubscribe = None
        self._last_session: str | None = None

    def on_connect(self, conn: acp.Client) -> None:
        self._conn = conn

    async def initialize(self, protocol_version: int, **kwargs) -> acp.InitializeResponse:
        return acp.InitializeResponse(
            protocolVersion=acp.PROTOCOL_VERSION,
            agentCapabilities=acp.schema.AgentCapabilities(
                load_session=True,
                session_capabilities=acp.schema.SessionCapabilities(list=acp.schema.SessionListCapabilities()),
            ),
        )

    # ------------------------------------------------------- live events

    def _send_ext(self, method: str, payload: dict) -> None:
        """Thread-safe: schedule one extension notification on the ACP loop."""
        loop, conn = self._loop, self._conn
        if loop is None or conn is None or loop.is_closed():
            return

        async def send() -> None:
            try:
                await conn.ext_notification(method, payload)
            except Exception:
                logging.getLogger(__name__).debug("%s notification not delivered", method, exc_info=True)

        loop.call_soon_threadsafe(lambda: loop.create_task(send()))

    def _forward_event(self, kind: str, payload: dict) -> None:
        if "sessionId" not in payload and self._last_session:
            payload = {**payload, "sessionId": self._last_session}
        self._send_ext(f"sierralab/{kind}", payload)

    def _laya_notifier(self, session_id: str):
        """A thread-safe callback that forwards Laya's live triage events to the
        client as an ACP *extension notification* (method ``_sierralab/laya``).
        The ACP spec says a client that doesn't understand an extension
        notification must ignore it, so Zed and other clients are unaffected;
        acp-tui renders it as the live "◆ laya" line.
        """

        if self._loop is None:
            self._loop = asyncio.get_running_loop()

        def notify(payload: dict) -> None:
            self._send_ext("sierralab/laya", {"sessionId": session_id, **payload})

        return notify

    # ----------------------------------------------------------- sessions

    def _build_session(self, session_id: str, cwd: str) -> _Session:
        # Process-wide, not per-agent: strands_tools' file_read/file_write/
        # editor have no per-call working-directory parameter at all (only
        # shell does, via its own work_dir kwarg) — they just resolve
        # relative paths against os.getcwd(). A real chdir is the only fix
        # that covers all of them uniformly. Safe here specifically because
        # this ACP server process is per-Zed-connection (one project at a
        # time from the user's perspective), unlike main.py's AG-UI process,
        # which serves many concurrent web threads and deliberately never
        # touches cwd after its own one-time startup chdir into workspace/.
        try:
            os.chdir(cwd)
        except OSError as error:
            logging.getLogger(__name__).warning("cwd=<%s> | failed to chdir, tools will use the previous cwd: %s", cwd, error)
        if self._loop is None:
            self._loop = asyncio.get_running_loop()
        if self._unsubscribe is None:
            self._unsubscribe = harness_events.subscribe(self._forward_event)
        self._last_session = session_id

        preferences = settings_store.current(cwd)
        for warning in preferences.warnings:
            logging.getLogger(__name__).warning("settings: %s", warning)
        _apply_startup_preferences(preferences)

        # After `_apply_startup_preferences` and after `agent_common` has been
        # imported, which is where `settings.py` takes this harness's own
        # provider keys out of the environment — see `vault_proxy.install` for
        # why the order matters to the placeholders it writes.
        vault = vault_proxy.install(preferences)
        if vault.enabled:
            logging.getLogger(__name__).info("vault: %s", vault.reason)
            harness_events.emit(
                "vault",
                {
                    "event": "vault",
                    "phase": "blocked" if vault.blocks_egress else "ready",
                    "addr": vault.addr,
                    "reason": vault.reason,
                    "credentialed": vault.credentialed,
                },
            )

        # The project the search and outline tools read (process-wide, like the chdir above).
        code_search.configure(cwd)
        code_nav.configure(cwd)
        router = _router()
        router.policy = _router_policy(preferences)
        lsp_enabled = preferences.get("lsp.enabled", True) is not False
        manager = lsp_bridge.manager_for(cwd)
        manager.enabled = lsp_enabled

        reserved = {
            "shell", "editor", "file_read", "file_write", "python_repl", "http_request", "web_fetch", "apply_patch", "architect", "debugger",
            claude_code_tool.TOOL_NAME, orchestrate_tool.TOOL_NAME, "omlx_resource_manager", "ollama_resource_manager", "context_digest", "enable_tools", "search_skills",
            context_offload.RETRIEVE_TOOL, "code_search", "code_outline", "environment_profile", plugin_host.RESOURCE_TOOL,
        }
        plugins = _plugin_host(cwd, reserved, trace_store.enabled(preferences))
        heal = SelfHealHooks(config=lambda: _heal_config(settings_store.current(cwd)), session_id=session_id, root=cwd)
        router_hooks = RouterHooks(router, notify=self._laya_notifier(session_id), plugin_host=plugins)
        handler = _ApprovalHandler(
            cwd,
            preferences.get("permissions.defaultMode", _DEFAULT_MODE),
            plugins=plugins,
            heal=heal,
            router_hooks=router_hooks,
            vault=vault,
        )
        specialist_tools, specialist_agents = specialists.specialist_tools(
            interventions=[handler],
            # A specialist's kit is fixed, so it gets the two history levers (cap one result, age old output)
            # and not the cold-tool scoping.
            hooks_factory=lambda: [
                code_nav.LargeFileHooks(code_nav.threshold_from(lambda: settings_store.current(cwd)), root=cwd),
                lsp_bridge.LspEditHooks(), router_hooks, outcome_hooks.OutcomeHooks(session_id), stall_guard.StallHooks(session_id),
                context_diet.ContextDiet(lambda: settings_store.current(cwd), scope_tools=False),
            ],
        )
        instructions = settings_store.load_instructions(cwd)
        context = workspace_context.summary(cwd, specialists=list(specialist_agents))
        system_prompt = (_chat_system_prompt(cwd) if _is_chat_scratch(cwd) else _project_system_prompt(cwd)) + f"\n\n{context}" + (f"\n\n{instructions}" if instructions else "") + prompts.live_text(cwd)
        # Order matters for AfterInvocationEvent (callbacks run in reverse):
        # self-heal is registered last so it runs first and can set `resume`
        # before the router decides whether the turn is over.
        agent = build_agent_for_session(
            session_id,
            interventions=[handler],
            system_prompt=system_prompt,
            # The Meta-Harness (Phase 6/7): the Claude Code CLI wrapper, the two memory
            # managers and the local digest. ACP-only, like the specialists — the web front
            # door has no vault status to route the CLI's egress through, no approval gate for it.
            extra_tools=[
                patch_tool.apply_patch,
                *specialist_tools,
                claude_code_tool.claude_code_engineer,
                orchestrate_tool.orchestrate,
                omlx_resource_tool.omlx_resource_manager,
                ollama_resource_tool.ollama_resource_manager,
                context_digest_tool.context_digest,
                context_diet.enable_tools,
                env_profile.environment_profile,
                plugin_host.resources_tool(plugins),
            ],
            # A result over the limit is set aside whole — written to ~/.sierralab/offloaded/<session>/, a head+tail preview and
            # a reference in context, `retrieve_offloaded_content` for the slice the agent wants (Strands' ContextOffloader).
            # What a specialist or the Claude Code CLI returns is the answer itself, so those are never set aside.
            offloader=context_offload.build(session_id, preferences=lambda: settings_store.current(cwd), never=set(specialist_agents)),
            summarizer=str(preferences.get("context.summaryModel", "fast")),
            project_root=cwd,
            # What each message carries along (ephemeral, never stored): the MCP servers this project has *now* — a
            # server discovered in the workspace and started in the background is part of the agent's picture from the
            # next message on — and the rules this person's rejections taught (rejection_memory.py).
            extra_plugins=[plugin_host.capabilities_injector(plugins), rejection_memory.constraints_injector(cwd, lambda: settings_store.current(cwd)), os_context.injector(session_id)],
            web_fetch_mode=str(preferences.get("context.webFetch", "agentic")),
            extra_hooks=[
                # Ollama Cloud's reasoning_effort per model call, from the project's size and Laya's reading of the turn.
                adaptive_effort.AdaptiveEffort(
                    cwd,
                    complexity=lambda agent: (lambda decision: decision.tier() if decision is not None else None)(router_hooks.current(agent)),
                    enabled=lambda: settings_store.current(cwd).get("reasoning.adaptive", True) is not False,
                    notify=lambda payload: harness_events.emit("effort", {**payload, "sessionId": session_id}),
                ),
                lsp_bridge.LspEditHooks(), router_hooks, heal, outcome_hooks.OutcomeHooks(session_id, "orchestrator"), stall_guard.StallHooks(session_id, "orchestrator"),
                # Last, so it reads the route the router just set (BeforeInvocation runs in order) and caps a
                # tool result before the hooks registered earlier see it (AfterToolCall runs in reverse).
                context_diet.ContextDiet(lambda: settings_store.current(cwd), route=lambda agent: _route_of(router_hooks, agent)),
            ],
            exclude_tools={"agent_memory"},
            use_triage=False,
            untrusted_tools=plugins.untrusted_names,
            skill_dirs=learned.live_dirs(cwd),
            checkpointing=True,
            # Skills: the few that fit the request, found by embedding search; Laya decides whether one clearly
            # does (the router's own forward pass and calibrator — `skill_search.SkillDecider`).
            skill_options={
                "decider": skill_search.SkillDecider(
                    router.predict,
                    router.calibrator,
                    min_confidence=router.policy.min_confidence,
                    ready=_laya_loaded,
                ),
                "preferences": lambda: settings_store.current(cwd),
                "notify": lambda payload: harness_events.emit("skills", payload),
            },
        )
        handler.session_agent = agent
        handler.session_id = session_id
        # What a module-level Strands tool cannot read from its arguments: this session's
        # project, the vault's *live* status (the handler re-probes it), settings, the Lab.
        claude_code_tool.configure(
            cwd=lambda: cwd,
            vault=lambda: handler.vault,
            preferences=lambda: settings_store.current(cwd),
            session_id=lambda: session_id,
            lab=lambda: lab_module.get(),
        )
        orchestrate_tool.configure(cwd=lambda: cwd, vault=lambda: handler.vault, preferences=lambda: settings_store.current(cwd), session_id=lambda: session_id)
        self._wire_lab(session_id, cwd, agent, handler, specialist_agents)
        if preferences.get("model"):
            _apply_model(agent, preferences.get("model"))
        if preferences.get("temperature"):
            _apply_temperature(agent, preferences.get("temperature"))

        plugins.attach(agent)  # specialists keep their fixed kit; plugins reach the orchestrator
        threading.Thread(target=self._start_background, args=(plugins, manager, lsp_enabled), name="harness-start", daemon=True).start()
        if app_settings.LAYA_TRIAGE_MODE != "off":
            from prompt_security import warm_up_laya

            warm_up_laya()

        state = _Session(agent, handler, cwd, router_hooks, heal, specialist_agents, plugins, preferences)
        self._sessions[session_id] = state
        # The settings this session actually ran with, frozen at build time.
        # `values` is the merged, validated dotted-key view; `sources` says which
        # file set each key, which is what makes "who loosened that rule?"
        # answerable after the files on disk have moved on.
        state_store.record_session_config(
            session_id,
            cwd=cwd,
            mode=handler.mode,
            settings={"values": preferences.values, "sources": preferences.sources, "trusted": preferences.trusted},
        )
        _touch_index(session_id, cwd)
        return state

    def _wire_lab(self, session_id: str, cwd: str, agent, handler, specialist_agents: dict) -> None:
        """Register the orchestrator and the specialists as Lab pods and point the
        Lab's providers (threat reading, MCP list) at this session."""
        lab = lab_module.get()
        if lab.session_id not in (None, session_id):
            lab.reset()  # a different session: its pods are not the previous one's (isolation included)
        lab.session_id = session_id
        handler.lab = lab
        lab.connect_models()

        def risk_event(agent, tool_use=None) -> dict | None:
            return handler.reading_for((tool_use or {}).get("toolUseId"), agent)

        def mcp_names() -> list[str]:
            import mcp_manager

            return [str(server.get("name")) for server in mcp_manager.list_servers()]

        lab.risk_provider = risk_event
        lab.mcp_provider = mcp_names
        lab.mode_provider = lambda: handler.mode
        agent.hooks.add_hook(lab.attach(agent, "orchestrator", role="orchestrator"))
        for name, specialist in specialist_agents.items():
            specialist.hooks.add_hook(lab.attach(specialist, name, role=name, parent="orchestrator"))
        lab.emit(force=True)

    @staticmethod
    def _start_background(plugins: PluginHost, manager, lsp_enabled: bool) -> None:
        """Plugins and language servers start off the request path; their
        progress arrives as task events (progress bars in acp-tui)."""
        try:
            if not any(row["status"] in ("running", "starting") for row in plugins.status()):
                plugins.start_autostart()
            if lsp_enabled:
                manager.start_all()
        except Exception:
            logging.getLogger(__name__).warning("background start failed", exc_info=True)

    async def new_session(self, cwd: str, **kwargs) -> acp.NewSessionResponse:
        session_id = str(uuid.uuid4())
        state = self._build_session(session_id, cwd)
        return acp.NewSessionResponse(
            sessionId=session_id,
            modes=acp.schema.SessionModeState(current_mode_id=state.handler.mode, available_modes=_MODES),
            config_options=_config_options(state.agent),
        )

    async def load_session(self, cwd: str, session_id: str, **kwargs) -> acp.LoadSessionResponse:
        # A session this process already holds is *attached to*, not rebuilt: the daemon's
        # conversation hub runs one child per conversation and lets a second front door (a
        # terminal) load it while it is live — possibly mid-turn. Rebuilding would replace the
        # running agent with last turn's snapshot under it. Its history is replayed from memory.
        state = self._sessions.get(session_id) or self._build_session(session_id, cwd)

        if self._conn is not None:
            for update in _replay_history(state.agent.messages):
                await self._conn.session_update(session_id=session_id, update=update)
            checkpoint = _checkpoint_path(session_id)
            if checkpoint.exists():
                try:
                    data = json.loads(checkpoint.read_text())
                    where = f"{data['checkpoint'].get('position')}, cycle {data['checkpoint'].get('cycle_index')}"
                except (OSError, ValueError, KeyError):
                    where = "unknown point"
                age = time.time() - checkpoint.stat().st_mtime
                attempts = _auto_resume_attempts(session_id)
                wanted = settings_store.current(cwd).get("selfHeal.autoResume", True) is not False
                auto = wanted and age <= AUTO_RESUME_MAX_AGE and attempts < MAX_AUTO_RESUMES
                if auto:
                    advice = "Resuming it automatically."
                elif not wanted:
                    advice = "Send /resume-turn to continue it from there."
                elif attempts >= MAX_AUTO_RESUMES:
                    advice = f"It was resumed automatically {attempts} times from this point and stopped again, so it is not being retried on its own. Send /resume-turn to try once more."
                else:
                    advice = "It is too old to resume on its own (over 30 minutes). Send /resume-turn to continue it from there."
                await self._conn.session_update(
                    session_id=session_id,
                    update=acp.update_agent_message_text(f"\n[This session's last turn was interrupted at a checkpoint ({where}). {advice}]\n"),
                )
                self._send_ext("sierralab/resume", {"sessionId": session_id, "available": True, "auto": bool(auto), "where": where, "ageSeconds": int(age), "attempts": attempts, "max": MAX_AUTO_RESUMES})
            # Approvals this session was parked on when its process went away.
            # The child that was blocked on the prompt is gone, so the request
            # will never arrive again — the ledger row is all that is left of
            # it, and saying so beats a resume that silently drops the question.
            pending = state_store.pending_interventions(session_id)
            if pending:
                asked = "\n".join(
                    f"  · {row.get('tool') or 'a tool'}"
                    + (f" — risk {row['risk_score']:.2f}" if row.get("risk_score") is not None else "")
                    + (f" ({row['note']})" if row.get("note") else "")
                    for row in pending
                )
                await self._conn.session_update(
                    session_id=session_id,
                    update=acp.update_agent_message_text(
                        f"\n[A previous run of this session was stopped while waiting for approval of "
                        f"{len(pending)} call{'s' if len(pending) != 1 else ''}, and it will not be asked again:\n"
                        f"{asked}\nSend the request again to be asked a second time.]\n"
                    ),
                )
            if state.handler.vault is not None and state.handler.vault.blocks_egress:
                # Worth saying at resume rather than only at the first refusal:
                # the model can do a lot of work before it wants the network,
                # and "why is nothing able to reach anything" should not be a
                # question the user answers from the logs.
                vault = state.handler.vault
                await self._conn.session_update(
                    session_id=session_id,
                    update=acp.update_agent_message_text(
                        f"\n[Egress is blocked this session: {vault.reason}. shell, python_repl and "
                        "http_request will be refused until the vault is reachable, or until "
                        "vault.egressPolicy is set to \"proxy\".]\n"
                    ),
                )

        # Mode isn't persisted across restarts (the session manager only
        # persists agent messages) — a reloaded session comes back in the
        # configured default (permissions.defaultMode, "build" unless the user
        # chose otherwise), never a previously-chosen plan/auto.
        return acp.LoadSessionResponse(
            modes=acp.schema.SessionModeState(current_mode_id=state.handler.mode, available_modes=_MODES),
            config_options=_config_options(state.agent),
        )

    async def list_sessions(self, cwd: str | None = None, cursor: str | None = None, **kwargs) -> acp.schema.ListSessionsResponse:
        index = _read_index()
        rows = [
            acp.schema.SessionInfo(session_id=sid, cwd=meta.get("cwd", ""), title=meta.get("title"), updated_at=meta.get("updatedAt"))
            for sid, meta in index.items()
            if session_store.exists(sid, SESSIONS_DIR) and (cwd is None or os.path.realpath(meta.get("cwd", "")) == os.path.realpath(cwd))
        ]
        rows.sort(key=lambda r: r.updated_at or "", reverse=True)
        return acp.schema.ListSessionsResponse(sessions=rows[:50])

    def _known(self, session_id: str) -> "_Session | None":
        """The session this id names — or, for an id this process never minted, the one it is hosting.

        Under the daemon a front door that is not the driving client (the GUI's mode switch, the
        Discord bot) knows the *daemon's* id for this child, not the ACP id `new_session` made here.
        An id nobody here owns can only mean "the session in this process", so it is resolved to the
        newest one rather than silently doing nothing. An id that *is* known is never remapped."""
        state = self._sessions.get(session_id)
        if state is None and self._last_session:
            state = self._sessions.get(self._last_session)
        return state

    async def set_session_mode(self, session_id: str, mode_id: str, **kwargs) -> acp.SetSessionModeResponse | None:
        state = self._known(session_id)
        session_id = next((sid for sid, s in self._sessions.items() if s is state), session_id)
        if state is None or mode_id not in _MODE_IDS:
            return None
        state.handler.mode = mode_id
        # ACP has no push for a client-initiated change, so another front end watching this
        # session (the Lab) would never learn of it: publish a snapshot, which carries the mode.
        lab = lab_module.get()
        if lab.session_id == session_id:
            lab.emit(force=True)
        return acp.schema.SetSessionModeResponse()

    async def set_config_option(self, config_id: str, session_id: str, value, **kwargs):
        state = self._known(session_id)
        if state is None:
            return None
        if config_id == "model" and isinstance(value, str):
            _apply_model(state.agent, value)
        elif config_id == "temperature" and isinstance(value, str):
            _apply_temperature(state.agent, value)
        return acp.schema.SetSessionConfigOptionResponse(config_options=_config_options(state.agent))

    async def cancel(self, session_id: str, **kwargs) -> None:
        self._cancelled.add(session_id)
        # Strands' own cancellation (docs: agent-loop → Cancellation): the loop stops at its next checkpoint — before the
        # next model call, before the next tool — with a `cancelled` result, and a tool that polls
        # `tool_context.cancel_signal` (the Claude Code run, which spends the person's usage) stops mid-flight.
        # Only an agent that is running has anything to cancel: the signal is cleared when an invocation *completes*,
        # so one set on an idle agent would silently cancel its next turn (lab.isolate guards the same way).
        agent = self._streaming.get(session_id)
        if agent is not None:
            try:
                agent.cancel()
            except Exception:
                logging.getLogger(__name__).debug("agent.cancel failed", exc_info=True)

    # ------------------------------------------------------ ext methods

    async def ext_method(self, method: str, params: dict) -> dict:
        """``_sierralab/*`` requests from acp-tui: plugins, LSP, Laya stats,
        self-heal state. Unknown methods raise (JSON-RPC method-not-found)."""
        name = method.removeprefix("_").removeprefix("sierralab/")
        session_id = params.get("sessionId") or self._last_session
        state = self._sessions.get(session_id) if session_id else None
        cwd = state.cwd if state else os.getcwd()
        if name == "plugins/list":
            host = state.plugins if state else _plugin_host(cwd, set())
            return {"plugins": host.status(), "warnings": host.warnings}
        if name in ("plugins/enable", "plugins/disable"):
            host = state.plugins if state else _plugin_host(cwd, set())
            target = str(params.get("name") or "")
            persist = bool(params.get("persist", True))
            fn = host.enable if name.endswith("enable") else host.disable
            row = await run_blocking(fn, target, persist)
            return row
        if name == "plugins/reload":
            host = state.plugins if state else _plugin_host(cwd, set())
            host.discover()
            return {"plugins": host.status(), "warnings": host.warnings}
        if name == "lsp/status":
            manager = lsp_bridge.manager_for(cwd)
            return {"servers": manager.status(), "enabled": manager.enabled}
        if name == "laya/stats":
            router = _router()
            return {
                "summary": summarize_stats(router.calibrator),
                "stats": router.calibrator.stats(),
                "weights": router.calibrator.weights(),
                "policy": router.policy.__dict__,
            }
        if name == "os-context":
            # The menu bar's capture, held for this session's next turn (os_context.py). Registered
            # here rather than sent inside the prompt so the person's words stay the prompt and the
            # screen arrives as wrapped untrusted data — and so it is gone at the turn's end.
            if not session_id:
                raise acp.RequestError.invalid_params("os-context needs a sessionId")
            payload = {key: value for key, value in params.items() if key != "sessionId"}
            os_context.register(session_id, payload)
            return {
                "ok": True,
                "app": str(payload.get("app_name") or ""),
                "denied": os_context.is_denied(str(payload.get("app_bundle_id") or "")),
            }
        if name.startswith("lab/"):
            return await self._lab_control(name.removeprefix("lab/"), params, session_id)
        if name.startswith("skills/"):
            return await self._skills_control(name.removeprefix("skills/"), params, cwd)
        if name.startswith("checkpoints/"):
            return await self._checkpoint_control(name.removeprefix("checkpoints/"), params, session_id, cwd)
        if name.startswith("conversation/"):
            return await self._conversation_control(name.removeprefix("conversation/"), params, session_id, state)
        if name == "heal/status":
            heal = state.heal if state else None
            episodes = [
                {"command": ep.command, "status": ep.status, "attempts": len(ep.attempts), "root_cause": ep.root_cause, "verified": ep.verified}
                for ep in (heal.episodes.values() if heal else [])
            ]
            config = heal.config if heal else HealConfig()
            memory = await run_blocking(fix_memory.stats, cwd) if config.memory else None
            return {"episodes": episodes, "config": config.__dict__, "memory": memory}
        raise acp.RequestError.method_not_found(method)

    async def _skills_control(self, action: str, params: dict, cwd: str) -> dict:
        """``skills/list`` · ``skills/show`` · ``skills/promote`` · ``skills/reject`` · ``skills/retire``: the learned skills
        for this project. A person's promote goes through the same filter as the gate's; it skips the evaluation,
        and the ledger says who did it."""
        name = str(params.get("name") or "")
        if action == "list":
            rows = await run_blocking(learned.list_skills, cwd)
            return {"skills": [{k: r.get(k) for k in ("name", "status", "scope", "description", "created", "uses", "reason", "signals", "evals", "promoted_by")} for r in rows[:60]],
                    "promotion": settings_store.current(cwd).get("learning.promotion", "off")}
        if action == "show":
            row = learned.find(name, cwd)
            if row is None:
                return {"ok": False, "reason": f"no learned skill {name!r}"}
            return {"ok": True, "skill": {k: row.get(k) for k in ("name", "status", "scope", "description", "signals", "evals", "uses", "rewards", "reason")}, "text": learned.read_text(row)}
        if action == "promote":
            return await run_blocking(learned.promote, name, cwd, evidence={"by": "person"}, by="person")
        if action == "reject":
            return await run_blocking(learned.reject, name, cwd, str(params.get("reason") or "rejected by a person"))
        if action == "retire":
            return await run_blocking(learned.retire, name, cwd, str(params.get("reason") or "retired by a person"), "person")
        raise acp.RequestError.method_not_found(f"_sierralab/skills/{action}")

    async def _checkpoint_control(self, action: str, params: dict, session_id: str | None, cwd: str) -> dict:
        """``checkpoints/list`` · ``checkpoints/undo`` · ``checkpoints/restore``. Anything that changes files is a
        dry run unless the caller says ``apply`` — and is refused while a turn is running, because a restore under a
        working agent is two writers on one tree. Applied restores are audited in the ledger."""
        if action == "list":
            preferences = settings_store.current(cwd)
            return {
                "enabled": bool(preferences.get("checkpoints.enabled", True)),
                "project": cwd,
                "turns": await run_blocking(checkpoints.turns, cwd, int(params.get("limit") or 8)),
            }
        if action not in ("undo", "restore"):
            raise acp.RequestError.method_not_found(f"_sierralab/checkpoints/{action}")
        apply = bool(params.get("apply"))
        if apply and outcomes.active(session_id) is not None:
            return {"ok": False, "reason": "a turn is running — stop it first, then undo"}
        paths = [str(p) for p in params.get("paths") or []] or None
        if action == "undo":
            result = await run_blocking(lambda: checkpoints.undo_turn(cwd, params.get("turn") or None, dry_run=not apply, paths=paths, session_id=session_id or ""))
        else:
            result = await run_blocking(lambda: checkpoints.restore(cwd, str(params.get("id") or ""), paths=paths, dry_run=not apply, session_id=session_id or ""))
        if apply:
            audit = {"action": action, "ok": result.get("ok"), "target": result.get("target"), "turn": result.get("turn"), "files": result.get("count"), "undo_with": result.get("undo_with")}
            state_store.append_event("checkpoint_restore", audit, session_id=session_id, source="checkpoints")
            harness_events.emit("checkpoint", {"event": "checkpoint", "phase": "restored", "files": result.get("count"), "ok": result.get("ok")})
        return result

    async def _conversation_control(self, action: str, params: dict, session_id: str | None, state: "_Session | None") -> dict:
        """``conversation/list`` · ``conversation/rewind`` — take the *conversation* back (the agent forgets the last turn or
        turns). The files are not touched: that is ``checkpoints/undo``. Refused while a turn is running (a rewind under a
        working agent is two writers on one history). The state it leaves is saved as a checkpoint, so it can be undone."""
        if state is None or session_id is None:
            return {"ok": False, "reason": "no such session"}
        if action == "list":
            return session_store.listing(state.agent, SESSIONS_DIR, int(params.get("limit") or 15))
        if action != "rewind":
            raise acp.RequestError.method_not_found(f"_sierralab/conversation/{action}")
        if session_id in self._streaming or outcomes.active(session_id) is not None:
            return {"ok": False, "reason": "a turn is running — stop it first, then go back"}
        index = params.get("index")
        result = await session_store.go_back(state.agent, SESSIONS_DIR, turns=int(params.get("turns") or 1), to=str(params.get("to") or "") or None, index=int(index) if index else None)
        if result.get("ok"):
            # What a half-finished turn left beside the history it belonged to is stale now.
            _checkpoint_path(session_id).unlink(missing_ok=True)
            _resume_marker_path(session_id).unlink(missing_ok=True)
            audit = {"turn": result.get("turn"), "now": result.get("now"), "forgot": len(result.get("forgot") or []), "restored": result.get("to"), "undo_with": result.get("undo_with")}
            state_store.append_event("conversation_rewind", audit, session_id=session_id, source="session_store")
            self._send_ext("sierralab/conversation", {"sessionId": session_id, "event": "rewound", "now": result.get("now"), "forgot": result.get("forgot") or []})
        return result

    async def _lab_control(self, action: str, params: dict, session_id: str | None) -> dict:
        """The Lab Master's controls. Each is audited in the ledger before it takes
        effect — an E-Stop or a loosened dial is exactly what "who did that?" is for."""
        lab = lab_module.get()
        pod = str(params.get("pod") or "")
        if action == "state":
            return lab.snapshot()
        try:
            if action == "select":
                lab.select(pod or None)
                return {"focus": lab.focus}
            if action not in ("isolate", "release", "dial", "inject", "effort"):
                raise acp.RequestError.method_not_found(f"_sierralab/lab/{action}")
            audit = {"action": action, "pod": pod}
            if action in ("dial", "effort"):
                audit["value"] = params.get("value")
            if action == "inject":
                audit["chars"] = len(str(params.get("text") or ""))
            state_store.append_event("lab", audit, session_id=session_id, source="lab")
            if action == "isolate":
                # The pod and everything it spawned: stopping an orchestrator while
                # its specialist keeps working would not be a stop.
                reply = lab.isolate(pod)
                reply["also"] = []
                for descendant in lab.descendants(pod):
                    lab.isolate(descendant, reason=f"parent {pod} isolated")
                    reply["also"].append(descendant)
                if pod == "orchestrator" and session_id:
                    self._cancelled.add(session_id)
                return reply
            if action == "release":
                return lab.release(pod)
            if action == "dial":
                return lab.set_dial(pod, float(params.get("value")))
            if action == "effort":
                return lab.set_effort(pod, str(params.get("value") or ""))
            return lab.inject(pod, str(params.get("text") or ""))
        except KeyError as error:
            raise acp.RequestError.invalid_params({"error": str(error.args[0] if error.args else error)})
        except (ValueError, TypeError, PermissionError) as error:
            raise acp.RequestError.invalid_params({"error": str(error)})

    async def ext_notification(self, method: str, params: dict) -> None:
        return None

    # ------------------------------------------------------ interrupts

    async def _resolve_interrupts(self, session_id: str, interrupts: list, since: float | None = None) -> list:
        """Turn each unanswered Strands interrupt into a real ACP permission
        prompt, and build the resume payload strands.interrupt._InterruptState.resume
        expects: a list of {"interruptResponse": {"interruptId", "response"}}.

        Every prompt is staged in `state.db` before it is shown and closed out
        with what the user chose, so the prompt and its answer outlive the call
        that raised them — and a call reported twice (this method's caller
        learns about a turn's interrupts from two places) is answered from the
        record instead of being put to the user a second time. See
        `_settled_intervention` for what does and does not count as the same
        call.
        """
        responses = []
        state = self._sessions.get(session_id)
        for interrupt in interrupts:
            risk: dict = {}
            try:
                payload = json.loads(interrupt.reason)
                tool_name = payload.get("name", "tool")
                tool_input = payload.get("input")
                tool_use_id = payload.get("tool_use_id") or interrupt.id
                always_rule = payload.get("always_rule")
                why = payload.get("reason")
                risk = payload.get("risk") or {}
                pod_name = payload.get("pod") or "orchestrator"
            except (TypeError, ValueError):
                pod_name = "orchestrator"
                tool_name, tool_input, tool_use_id, always_rule, why = str(interrupt.reason), None, interrupt.id, None, None

            settled = _settled_intervention(interrupt.id, session_id, tool_name, tool_input, since)
            if settled is not None:
                approved = settled["status"] == "approved"
                state_store.append_event(
                    "intervention",
                    {
                        "phase": "replayed",
                        "interrupt_id": interrupt.id,
                        "tool": tool_name,
                        "outcome": settled["status"],
                        "decided_by": settled.get("decided_by"),
                        "rule": settled.get("rule"),
                    },
                    session_id=session_id,
                    source="approval",
                )
                if not approved and state is not None:
                    state.router_hooks.note_rejection(state.agent)
                _note_answer(session_id, tool_name, approved, interrupt.id)
                responses.append({"interruptResponse": {"interruptId": interrupt.id, "response": approved}})
                continue

            lab_module.get().stage(interrupt.id, pod_name, tool_name, tool_input, risk or None, why)
            state_store.stage_intervention(
                session_id=session_id,
                tool=tool_name,
                tool_input=tool_input,
                intervention_id=interrupt.id,
                risk_score=risk.get("score"),
                # The band, not the ordinal: safe/elevated/destructive is in the
                # same units as the settings thresholds, so a row answers "did
                # this cross the destructive line" without re-deriving it. The
                # full reading — ordinal and confidence included — is in the
                # event.
                risk_label=risk.get("band"),
                note=why,
            )
            state_store.append_event(
                "intervention",
                {
                    "phase": "requested",
                    "interrupt_id": interrupt.id,
                    "tool": tool_name,
                    "input": tool_input,
                    "risk": risk or None,
                    "note": why,
                    "always_rule": always_rule,
                },
                session_id=session_id,
                source="approval",
            )

            approved = False
            option_id = None
            note = ""
            answered_by = "user" if self._conn is not None else "no-client"
            if self._conn is not None:
                options = [acp.schema.PermissionOption(option_id="allow", name="Allow", kind="allow_once")]
                if always_rule:
                    options.append(
                        acp.schema.PermissionOption(option_id="allow_always", name=f"Always allow {always_rule}", kind="allow_always")
                    )
                options.append(acp.schema.PermissionOption(option_id="reject", name="Reject", kind="reject_once"))
                result = await self._conn.request_permission(
                    session_id=session_id,
                    tool_call=acp.schema.ToolCallUpdate(
                        tool_call_id=tool_use_id,
                        title=f"Run {tool_name}" + (f" — {why}" if why else ""),
                        kind=_tool_kind(tool_name),
                        raw_input=tool_input,
                    ),
                    options=options,
                    # ACP's `_meta` slot: the exact command or diff, secret-masked, plus the pod
                    # and Laya's reading — what the daemon's gRPC bridge hands the GUI's containment
                    # modal. A client that does not know it (Zed, acp-tui) ignores it.
                    sierralab=intervention_view.build(
                        tool_name,
                        tool_input,
                        state.cwd if state else os.getcwd(),
                        interrupt_id=interrupt.id,
                        pod=pod_name,
                        risk=risk or None,
                        reason=why,
                        armed=bool(why),
                        project=state.cwd if state else os.getcwd(),
                        **dict(zip(("goal", "said"), _turn_context(state.agent) if state else ("", ""))),
                    ),
                )
                approved = _permission_approved(result.outcome, options)
                option_id = getattr(result.outcome, "option_id", None)
                # Which front door answered. Under the daemon the answer may have come from the
                # Discord bot rather than a client on the socket, and the daemon relays that in
                # ACP's `_meta` slot (supervisor.answer_permission) — the same request the GUI's
                # containment modal would have answered, told apart only here. A client that
                # sends no `_meta` is a person at a TUI or the GUI, which is what "user" has
                # always meant; anything else keeps the door's own name so the ledger can
                # account for who decided what.
                answered_by = _answered_by(result) or answered_by
                note = _answer_note(result)
                if approved and always_rule and option_id == "allow_always":
                    self._remember_always(session_id, always_rule)
            if not approved and state is not None:
                state.router_hooks.note_rejection(state.agent)
                # A person said no: keep the change and their reason, and let the ReflectionAgent turn it into a rule
                # later turns are held to (rejection_memory.py). A fail-closed "no client" rejection teaches nothing.
                rejection_memory.capture(
                    state.cwd,
                    session_id=session_id,
                    tool_name=tool_name,
                    tool_input=tool_input,
                    decided_by=answered_by,
                    note=note,
                    goal=_turn_context(state.agent)[0],
                    preferences=settings_store.current(state.cwd),
                )
            _note_answer(session_id, tool_name, approved, interrupt.id)

            rule = always_rule if (approved and option_id == "allow_always") else None
            lab_module.get().resolve(interrupt.id, "approved" if approved else "rejected", answered_by)
            state_store.resolve_intervention(
                interrupt.id,
                "approved" if approved else "rejected",
                # Honest, and it matters: with no client attached nothing was
                # asked, and a rejection that reads as a user decision would
                # misattribute a fail-closed default to a person. `answered_by`
                # is that name, or the front door that relayed the answer.
                decided_by=answered_by,
                rule=rule,
            )
            state_store.append_event(
                "intervention",
                {
                    "phase": "resolved",
                    "interrupt_id": interrupt.id,
                    "tool": tool_name,
                    "outcome": "approved" if approved else "rejected",
                    "decided_by": answered_by,
                    "rule": rule,
                    "always": option_id == "allow_always",
                },
                session_id=session_id,
                source="approval",
            )

            responses.append({"interruptResponse": {"interruptId": interrupt.id, "response": approved}})
        return responses

    def _remember_always(self, session_id: str, rule: str) -> None:
        """"Always allow": save the rule for this project (settings.local.json,
        this machine only). Choosing it is an explicit decision about the folder,
        so it is also marked trusted — otherwise its own allow rule would be ignored."""
        state = self._sessions.get(session_id)
        cwd = state.cwd if state else os.getcwd()
        try:
            settings_store.add_rule("allow", rule, "local", cwd)
            settings_store.trust_project(cwd)
        except (OSError, ValueError) as error:
            logging.getLogger(__name__).warning("could not save always-allow rule %r: %s", rule, error)

    async def _send_tool_result(self, session_id: str, result: dict, traced: dict | None = None) -> None:
        """One finished tool call as a terminal ACP tool-call update.

        ``traced`` is the in-flight turn's trace collector, when there is one.
        The result is where a call's outcome — and the error text that explains
        it — actually lives, and this is the only place that sees it."""
        tool_use_id = result.get("toolUseId")
        if not tool_use_id or self._conn is None:
            return
        status = "completed" if result.get("status") == "success" else "failed"
        if traced is not None:
            entry = traced.setdefault(tool_use_id, {})
            entry["status"] = status
            text = " ".join(
                block["text"] for block in result.get("content") or []
                if isinstance(block, dict) and block.get("text")
            )
            if status == "failed":
                entry["error"] = text or "tool reported failure"
            elif text:
                entry["result"] = text
        content = [
            acp.tool_content(acp.text_block(block["text"]))
            for block in result.get("content") or []
            if isinstance(block, dict) and block.get("text")
        ]
        await self._conn.session_update(
            session_id=session_id,
            update=acp.update_tool_call(tool_use_id, status=status, content=content or None),
        )

    # ---------------------------------------------------------- turns

    async def _stream(self, session_id: str, agent, agent_input, *, checkpoints: bool) -> tuple[str, object]:
        """`_stream_turn`, with the running agent registered so a cancel can reach it (`cancel`)."""
        agent.cancel_signal.clear()  # idle until now: whatever is set is stale, and would cancel this turn before it began
        self._streaming[session_id] = agent
        try:
            return await self._stream_turn(session_id, agent, agent_input, checkpoints=checkpoints)
        finally:
            if self._streaming.get(session_id) is agent:
                del self._streaming[session_id]

    def _turn_budget(self, session_id: str) -> tuple[int, int]:
        """`(steps, tokens)` one turn may spend — `context.maxSteps` / `context.maxTurnUsage`, 0 meaning no cap."""
        state = self._sessions.get(session_id)
        preferences = settings_store.current(state.cwd) if state is not None else {}
        values = []
        for key, default in (("context.maxSteps", DEFAULT_MAX_STEPS), ("context.maxTurnUsage", 0)):
            try:
                values.append(max(0, int(float(preferences.get(key, default)))))
            except (TypeError, ValueError):
                values.append(default)
        return values[0], values[1]

    async def _stream_turn(self, session_id: str, agent, agent_input, *, checkpoints: bool) -> tuple[str, object]:
        """Run one turn of ``agent`` to its end, translating Strands events into
        ACP updates. Loops through approval interrupts (each becomes a real ACP
        permission prompt) and, with ``checkpoints``, through Strands checkpoint
        pauses — each one is written beside the session before resuming, so a
        crash mid-turn leaves a resumable point. Returns (stop reason, result)."""
        seen_tool_calls: set[str] = set()
        last_progress: dict[str, str] = {}
        specialist_progress: dict[str, _SpecialistProgress] = {}
        # tool_use_id -> what it was called with, what it returned. Fed into
        # trace_store at the end of the turn; empty (and never written) when
        # memory.traces is off, so the collector costs one dict when unused.
        traced: dict[str, dict] = {}
        last_result = None
        # What the user asked, before an approval resume replaces `agent_input`
        # with the interrupt responses — otherwise the turns that hit a prompt,
        # the ones most worth tracing, would be recorded with no prompt at all.
        prompt = agent_input if isinstance(agent_input, str) else None
        # Bounds what `_settled_intervention` may replay: a decision from before
        # this turn began is not this turn's to reuse.
        turn_started = time.time()
        # What this turn may spend (`context.maxSteps`, `context.maxTurnUsage`). Strands checks `limits` at the top of every loop
        # cycle and answers with a `limit_*` stop reason and a history that is still valid — but a limit is *per invocation*,
        # and a turn is several: each approval, and (with `checkpoints`) every cycle, resumes a fresh one whose counters start
        # at zero. So an invocation that runs the whole loop (a specialist's) is handed what is left of the turn's budget as
        # `limits`; a checkpointed turn is checked here, at the `after_tools` pause — the one point where every tool call has
        # its result — against the cycle the checkpoint itself reports.
        metrics = agent.event_loop_metrics
        start_cycles = metrics.cycle_count
        start_tokens = int(metrics.accumulated_usage.get("totalTokens", 0) or 0)

        web = getattr(agent, "web_usage", None)
        start_web = web.total if web is not None else 0

        def used_tokens() -> int:
            return int(metrics.accumulated_usage.get("totalTokens", 0) or 0) - start_tokens + ((web.total - start_web) if web is not None else 0)

        max_steps, max_tokens = self._turn_budget(session_id)
        cancelled_at: float | None = None
        while True:
            stop_reason = "end_turn"
            pending_interrupts: list = []
            checkpoint = None
            limits: dict[str, int] = {}
            if not checkpoints:
                if max_steps:
                    limits["turns"] = max(1, max_steps - (metrics.cycle_count - start_cycles))
                if max_tokens:
                    limits["total_tokens"] = max(1, max_tokens - used_tokens())

            async for event in agent.stream_async(agent_input, limits=limits or None):
                if session_id in self._cancelled:
                    # Strands was told to stop (`cancel`) and does at its next checkpoint, sending a `cancelled` result of
                    # its own — which lets a tool that polls the signal wind down first. A moment is allowed for that; a
                    # tool that never looks is not waited for.
                    cancelled_at = cancelled_at or time.monotonic()
                    if time.monotonic() - cancelled_at > CANCEL_GRACE_SECONDS:
                        _settle_cancelled_turn(agent)
                        return "cancelled", last_result
                if "tool_interrupt_event" in event:
                    interrupts = event["tool_interrupt_event"].get("interrupts") or []
                    if interrupts:
                        stop_reason = "interrupt"
                        pending_interrupts = list(interrupts)
                    continue

                if "result" in event:
                    last_result = event["result"]
                    reason = last_result.stop_reason
                    if reason == "interrupt":
                        # Belt-and-suspenders with the tool_interrupt_event
                        # branch above: AgentResult.interrupts carries the
                        # same list, so this still works even if that event
                        # was ever skipped for some interrupt path.
                        stop_reason = "interrupt"
                        pending_interrupts = pending_interrupts or list(last_result.interrupts or [])
                    elif reason == "checkpoint":
                        stop_reason, checkpoint = "checkpoint", last_result.checkpoint
                    else:
                        stop_reason = reason if reason in ("end_turn", "max_tokens", "cancelled", "max_turn_requests") or str(reason).startswith("limit_") else "end_turn"
                    continue
                if self._conn is None:
                    continue

                event_type = event.get("type")
                if event_type == "tool_use_stream":
                    current = event.get("current_tool_use") or {}
                    tool_use_id = current.get("toolUseId")
                    if tool_use_id and tool_use_id not in seen_tool_calls:
                        seen_tool_calls.add(tool_use_id)
                        tool_name = current.get("name") or "tool"
                        traced.setdefault(tool_use_id, {}).update(name=tool_name, input=current.get("input"))
                        await self._conn.session_update(
                            session_id=session_id,
                            update=acp.start_tool_call(
                                tool_use_id,
                                tool_name,
                                kind=_tool_kind(tool_name),
                                status="pending",
                                raw_input=current.get("input"),
                            ),
                        )
                    continue

                if event_type == "tool_stream":
                    stream_event = event.get("tool_stream_event") or {}
                    tool_use = stream_event.get("tool_use") or {}
                    tool_use_id = tool_use.get("toolUseId")
                    data = stream_event.get("data") or {}
                    rendered = None
                    if tool_use_id and isinstance(data, dict) and isinstance(data.get("progress"), str):
                        rendered = data["progress"]  # a tool that narrates its own progress (claude_code_engineer, orchestrate)
                    elif tool_use_id and isinstance(data, dict) and tool_use.get("name") in self._specialist_names(session_id):
                        progress = specialist_progress.setdefault(tool_use_id, _SpecialistProgress())
                        if progress.feed(data):
                            rendered = progress.render(tool_use.get("name"))
                    # Progress is re-yielded whole on every update; a client only
                    # needs to hear when the rendered text actually changed.
                    if rendered and last_progress.get(tool_use_id) != rendered:
                        last_progress[tool_use_id] = rendered
                        await self._conn.session_update(
                            session_id=session_id,
                            update=acp.update_tool_call(
                                tool_use_id,
                                status="in_progress",
                                content=[acp.tool_content(acp.text_block(rendered))],
                            ),
                        )
                    continue

                if event_type == "tool_result":
                    await self._send_tool_result(session_id, event.get("tool_result") or {}, traced)
                    continue

                # Strands >= 1.5x no longer yields a "tool_result" event from
                # stream_async (confirmed against the installed 1.55.1 and 1.57.2: the
                # only events are tool_use_stream, "message", and "result") —
                # the finished tool's result arrives inside the user-role
                # "message" event that carries it into the next model call.
                # Without this branch every ToolCallStart stayed "pending"
                # forever in the client. Both branches stay: an older Strands
                # that emits both just sends the same terminal update twice.
                if "message" in event:
                    message = event["message"]
                    if isinstance(message, dict) and message.get("role") == "user":
                        for block in message.get("content") or []:
                            tool_result = block.get("toolResult") if isinstance(block, dict) else None
                            if tool_result:
                                await self._send_tool_result(session_id, tool_result, traced)
                    elif isinstance(message, dict) and message.get("role") == "assistant":
                        # The model finished asking for its tools; none has run yet.
                        # Send each call's complete input (the start event only had
                        # what had streamed so far) and, for edits, the change as
                        # ACP diff content read from disk *before* it happens.
                        for block in message.get("content") or []:
                            tool_use = block.get("toolUse") if isinstance(block, dict) else None
                            if tool_use and tool_use.get("toolUseId"):
                                await self._send_tool_preview(session_id, tool_use)
                    continue

                if "data" in event:
                    streaming_pod = lab_module.get().pod_of(agent)
                    if streaming_pod is not None:
                        lab_module.get().stream_text(streaming_pod.name, event["data"])
                    await self._conn.session_update(
                        session_id=session_id,
                        update=acp.update_agent_message_text(event["data"]),
                    )
                elif "reasoningText" in event:
                    await self._conn.session_update(
                        session_id=session_id,
                        update=acp.update_agent_thought_text(event["reasoningText"]),
                    )

            if stop_reason == "cancelled" or (session_id in self._cancelled and stop_reason != "interrupt"):
                _settle_cancelled_turn(agent)
                return "cancelled", last_result
            if stop_reason == "checkpoint" and checkpoint is not None and checkpoint.position == "after_tools":
                if max_steps and checkpoint.cycle_index + 1 >= max_steps:
                    stop_reason = "limit_turns"
                elif max_tokens and used_tokens() >= max_tokens:
                    stop_reason = "limit_total_tokens"
            if stop_reason.startswith("limit_"):
                await self._say_limit(session_id, stop_reason, max_steps, max_tokens)
                self._ingest_turn(session_id, prompt, traced)
                return "max_turn_requests", last_result
            if stop_reason == "checkpoint" and checkpoint is not None:
                payload = {"checkpoint": checkpoint.to_dict(), "ts": time.time()}
                if checkpoints:
                    try:
                        _checkpoint_path(session_id).write_text(json.dumps(payload), encoding="utf-8")
                    except OSError:
                        pass
                agent_input = {"checkpointResume": {"checkpoint": checkpoint.to_dict()}}
                continue
            if stop_reason != "interrupt":
                self._ingest_turn(session_id, prompt, traced)
                return stop_reason, last_result
            agent_input = await self._resolve_interrupts(session_id, pending_interrupts, turn_started)

    async def _say_limit(self, session_id: str, reason: str, max_steps: int, max_tokens: int) -> None:
        """Tell the person *why* the turn stopped, and that nothing was lost — a silent stop reads as a hang."""
        if self._conn is None:
            return
        what = f"{max_steps} steps" if reason == "limit_turns" else f"{max_tokens:,} tokens"
        key = "context.maxSteps" if reason == "limit_turns" else "context.maxTurnUsage"
        await self._conn.session_update(
            session_id=session_id,
            update=acp.update_agent_message_text(
                f"\n\n— Paused after {what} on this turn (`{key}`). Nothing is lost: say \"continue\" and I carry on from here, or raise the limit."
            ),
        )

    def _ingest_turn(self, session_id: str, prompt: str | None, traced: dict) -> None:
        """Feed the finished turn to the local stores, if either is switched on.

        Called only on the terminal exit from `_stream` — not on an interrupt,
        which resumes the same turn on the next loop pass and would otherwise be
        ingested once per approval prompt. Fire-and-forget in a thread: the
        embedding is the slow part of a trace, a graph write is a LanceDB merge,
        and the reply is already on its way to the client."""
        state = self._sessions.get(session_id)
        preferences = getattr(state, "preferences", None)
        tracing = bool(traced) and trace_store.enabled(preferences)
        graphing = kg.enabled(preferences)
        if not tracing and not graphing:
            return
        # `entry` already carries exactly name/input/status/error/result, which
        # is what record_turn reads; pass a copy so the collector is not shared
        # with the ingest thread.
        calls = [dict(entry) for entry in traced.values()]
        cwd = getattr(state, "cwd", None)
        log = logging.getLogger(__name__)

        def ingest() -> None:
            if tracing:
                try:
                    trace_store.record_turn(session_id=session_id, prompt=prompt, tool_calls=calls, preferences=preferences)
                except Exception:  # never let tracing break a turn
                    log.debug("trace ingest failed", exc_info=True)
            if graphing:
                # `kg.ingest_turn` swallows its own failures; this is belt and
                # braces so one store's bug cannot cost the other's rows.
                try:
                    kg.ingest_turn(session_id=session_id, cwd=cwd, tool_calls=calls, preferences=preferences)
                except Exception:
                    log.debug("kg ingest failed", exc_info=True)

        threading.Thread(target=ingest, name="trace-ingest", daemon=True).start()

    async def _send_tool_preview(self, session_id: str, tool_use: dict) -> None:
        if self._conn is None:
            return
        name = tool_use.get("name") or ""
        tool_input = tool_use.get("input")
        diffs = edit_preview.preview(name, tool_input) if name in ("editor", "file_write", "apply_patch") else []
        await self._conn.session_update(
            session_id=session_id,
            update=acp.update_tool_call(
                tool_use["toolUseId"],
                raw_input=tool_input,
                content=[acp.tool_diff_content(path, new_text, old_text) for path, old_text, new_text in diffs] or None,
            ),
        )

    def _specialist_names(self, session_id: str) -> set[str]:
        state = self._sessions.get(session_id)
        return set(state.specialists) if state else set()

    async def _decide(self, state: _Session, text: str) -> RouteDecision | None:
        if app_settings.LAYA_TRIAGE_MODE != "route" or not text:
            return None
        from prompt_security import laya_ready

        router = _router()
        router.policy = _router_policy(settings_store.current(state.cwd))
        targets = [*BASE_TARGETS, *plugin_targets(state.plugins.descriptions() if state.plugins else [])]
        timeout = _LAYA_DECIDE_TIMEOUT if laya_ready() else app_settings.LAYA_TRIAGE_TIMEOUT
        try:
            return await asyncio.wait_for(asyncio.to_thread(router.decide, text, targets), timeout)
        except asyncio.TimeoutError:
            decision = RouteDecision(target=None, error=f"Laya not ready within {timeout:g}s")
            return router._abstain(decision, "no decision: Laya still loading")

    def _apply_bandit(self, state: _Session, decision: RouteDecision, text: str) -> dict:
        """Let the contextual bandit pick among the actions the base policy *already allowed* this turn.

        Runs between ``router.decide`` and ``recorder.set_route``, so what the ledger records — and what the turn is
        later learned from — is the choice actually taken. Additive-only by construction: the arms are the base
        decision's own action and a milder one the policy already permits (`_bandit_route_arms`), never a new
        action, and an abstained decision is left alone — fail-closed is not this module's to reopen. Returns the
        extra route keys (``choices`` / ``bandit_mode``) for the caller's ``set_route``.
        """
        handler = state.handler
        handler._bandit_decision = decision if not (decision is None or decision.abstained) else None
        handler._bandit_route = None
        if decision is None or decision.abstained:
            return {}
        policy = _bandit_policy(settings_store.current(state.cwd))
        if policy["mode"] not in ("shadow", "live"):
            return {}
        recorder = outcomes.active(state.handler.session_id)
        turn = recorder.id if recorder is not None else ""
        stub = _bandit_stub(state.handler.mode, decision)
        handler._bandit_route = stub
        table = bandit.for_project(state.cwd)
        knobs = {"mode": policy["mode"], "explore": policy["explore"], "min_pulls": policy["min_pulls"], "lam": policy["lam"], "mu": policy["mu"]}
        choices: dict = {}
        # engine: the heavy Claude Code path against the orchestrator's own tools. Offered only for a
        # kind="tool" target, whose two actions the policy already weighs by Laya's complexity reading.
        if decision.target is not None and decision.target.kind == "tool":
            arm, info = table.choose("engine", bandit.context(stub, "engine"), ["tool", "none"], decision.action, **knobs)
            bandit.decision_event(info, turn=turn, project=state.cwd)
            choices["engine"] = arm
            if info["acted"]:
                decision.reason = f"{decision.reason} · bandit chose {arm} over {decision.action}"
                decision.action, decision.suppress_hint = arm, arm == "none"
        # route: dispatch vs. hint vs. none, among the ones the base policy allowed.
        arms = _bandit_route_arms(decision)
        if arms:
            arm, info = table.choose("route", bandit.context(stub, "route"), arms, decision.action, **knobs)
            bandit.decision_event(info, turn=turn, project=state.cwd)
            choices["route"] = arm
            if info["acted"]:
                decision.reason = f"{decision.reason} · bandit chose {arm} over {decision.action}"
                decision.action = arm
                if arm == "none":
                    decision.suppress_hint = True
                elif arm == "hint":
                    decision.suppress_hint = False
        decision.choices.update(choices)
        # By reference on purpose: the ``ask`` arm is decided later, on a tool call, and lands in this very dict.
        return {"choices": decision.choices, "bandit_mode": policy["mode"]}

    async def _dispatch(self, session_id: str, state: _Session, decision: RouteDecision, text: str) -> str:
        """Run a confidently-routed specialist directly — the orchestrator's
        model call that would only have decided to delegate is skipped. The
        exchange is then written into the orchestrator's history (and session
        file) so the conversation continues normally next turn."""
        name = decision.target.id
        specialist = state.specialists[name]
        specialist.messages.clear()  # specialists start fresh; context is passed explicitly
        context = _recent_context(state.agent.messages)
        prompt = text if not context else f"{text}\n\nConversation so far (for context):\n{context}"
        state.router_hooks.prime(specialist, decision)
        tool_use_id = f"dispatch-{uuid.uuid4().hex[:8]}"
        if self._conn is not None:
            await self._conn.session_update(
                session_id=session_id,
                update=acp.start_tool_call(tool_use_id, name, kind=_tool_kind(name), status="in_progress", raw_input={"input": text, "routed_by": "laya"}),
            )
        lab_module.get().link("orchestrator", name, "dispatch", label="laya")
        try:
            stop_reason, result = await self._stream(session_id, specialist, prompt, checkpoints=False)
        finally:
            lab_module.get().link("orchestrator", name, "dispatch", active=False, label="laya")
        reply = str(result).strip() if result is not None else ""
        if self._conn is not None:
            await self._conn.session_update(
                session_id=session_id,
                update=acp.update_tool_call(tool_use_id, status="completed" if stop_reason == "end_turn" else "failed", content=None),
            )
        if stop_reason == "end_turn":
            orchestrator = state.agent
            for message in (
                {"role": "user", "content": [{"text": text}]},
                {"role": "assistant", "content": [{"text": f"[{name} specialist, routed by Laya]\n{reply}"}]},
            ):
                orchestrator.messages.append(message)
                manager = getattr(orchestrator, "_session_manager", None)
                if manager is not None:
                    try:
                        session_store.record(manager, message, orchestrator)
                    except Exception:
                        logging.getLogger(__name__).debug("could not persist dispatched turn", exc_info=True)
        return stop_reason

    @staticmethod
    def _tokens_used(state: "_Session") -> tuple[int, int]:
        """Cumulative (input, output) tokens across the orchestrator and every specialist."""
        agents = [state.agent, *[getattr(s, "agent", s) for s in (state.specialists or {}).values()]]
        totals = [outcome_hooks.usage_of(agent) for agent in agents]
        # …and the `web_fetch` analyst, which runs as an agent of its own and so is in nobody's metrics (core_tools.UsageTally).
        web = getattr(state.agent, "web_usage", None)
        extra = (web.input, web.output) if web is not None else (0, 0)
        return sum(t[0] for t in totals) + extra[0], sum(t[1] for t in totals) + extra[1]

    def _learn_from(self, record: "outcomes.TurnRecord", state: "_Session") -> None:
        """After a turn: judge the learned skills and prompt overlays it had in front of it, and — when it cost
        something — draft a lesson from it, and rewrite a live artifact whose turns have gone badly. All of it is
        off the turn's path (a background thread) and none of it can fail the turn."""
        data = record.to_json()
        preferences = settings_store.current(state.cwd)

        def run() -> None:
            try:
                reward = outcomes.score(record).value
                baseline = _baseline_reward(state.cwd, exclude=data["id"])
                if data.get("skills"):
                    learned.record_use(state.cwd, data["skills"], reward, data["id"], baseline=baseline)
                overlay_names = prompts.live_names(state.cwd)
                if overlay_names:
                    prompts.record_use(state.cwd, overlay_names, reward, data["id"], baseline=baseline)
                reflect.reflect(data, state.cwd, session_id=data.get("session_id"), preferences=preferences)
                _optimize_artifacts(state.cwd, preferences)
                # The turn is judged now; if a reconcile landed it has become learnable, fold it in.
                _bandit_learn(state.cwd)
            except Exception:
                logging.getLogger(__name__).debug("learning from a turn failed", exc_info=True)

        threading.Thread(target=run, name="learn-from-turn", daemon=True).start()

    async def prompt(self, session_id: str, prompt: list, **kwargs) -> acp.PromptResponse:
        """One turn, recorded: the person's prompt to the stop reason becomes one outcome record
        (``outcomes.py``) — what was asked, what ran, how the approvals went, whether the checks passed,
        what changed. Recording is a side channel: nothing in it can fail or alter the turn."""
        state = self._sessions.get(session_id)
        if state is None:
            state = self._build_session(session_id, os.getcwd())
        text = _extract_prompt_text(prompt)
        recorder = None
        started = time.monotonic()
        tokens_before = self._tokens_used(state)
        if text.strip() and not _is_resume(text):
            try:
                recorder = outcomes.begin(session_id, goal=text, mode=state.handler.mode, project=state.cwd)
                recorder.heal_baseline(len(state.heal.episodes))
                # Judge what the person did with earlier turns' changes — off the turn's path.
                threading.Thread(target=_maintain, args=(state.cwd,), name="outcomes-reconcile", daemon=True).start()
            except Exception:
                logging.getLogger(__name__).debug("outcomes: could not begin a turn", exc_info=True)
        stop = "error"
        try:
            response = await self._prompt(session_id, prompt, **kwargs)
            stop = str(getattr(response, "stop_reason", None) or "end_turn")
            return response
        except asyncio.CancelledError:
            stop = "cancelled"
            raise
        finally:
            self._announce_turn(session_id, state, started, tokens_before, stop)
            # One turn only: a screen the person has left must not follow them into the next one.
            os_context.clear(session_id)
            if recorder is not None:
                try:
                    if recorder.checkpointed:
                        # The project as the turn left it: the other end of "what did this turn change?",
                        # and the baseline against which a later edit by the person is recognised.
                        recorder.checkpoint_end(checkpoints.take(state.cwd, kind="end", session_id=session_id, turn=recorder.id).id)
                    after = self._tokens_used(state)
                    recorder.usage(max(0, after[0] - tokens_before[0]), max(0, after[1] - tokens_before[1]))
                    finished = outcomes.finish(session_id, stop, heal_episodes=list(state.heal.episodes.values()))
                    if finished is not None:
                        self._learn_from(finished, state)
                except Exception:
                    logging.getLogger(__name__).debug("outcomes: could not finish a turn", exc_info=True)

    def _announce_turn(self, session_id: str, state: "_Session", started: float, tokens_before: tuple[int, int], stop: str) -> None:
        """What the turn cost, as ``_sierralab/turn``: a front door that cannot see tokens (the Discord
        bot, the TUI's status line) shows it without a model call of its own. Never raises."""
        try:
            after = self._tokens_used(state)
            recorder = outcomes.active(session_id)
            self._send_ext(
                "sierralab/turn",
                {
                    "sessionId": session_id,
                    "stop": stop,
                    "seconds": round(time.monotonic() - started, 1),
                    "tokensIn": max(0, after[0] - tokens_before[0]),
                    "tokensOut": max(0, after[1] - tokens_before[1]),
                    "calls": recorder.call_count if recorder is not None else 0,
                },
            )
        except Exception:
            logging.getLogger(__name__).debug("turn announcement failed", exc_info=True)

    async def _prompt(self, session_id: str, prompt: list, **kwargs) -> acp.PromptResponse:
        state = self._sessions.get(session_id)
        if state is None:
            # Defensive only — the ACP protocol always calls new_session or
            # load_session (both of which carry the real cwd) before prompt.
            # No cwd is available here, so this just keeps whatever
            # directory the process is already in rather than guessing.
            state = self._build_session(session_id, os.getcwd())
        agent = state.agent
        self._last_session = session_id

        is_first_turn = len(agent.messages) == 0
        text = _extract_prompt_text(prompt)

        if is_first_turn and self._conn is not None and text:
            title = text.strip().splitlines()[0][:60]
            await self._conn.session_update(
                session_id=session_id,
                update=acp.schema.SessionInfoUpdate(title=title, session_update="session_info_update"),
            )
            _touch_index(session_id, state.cwd, title)
        else:
            _touch_index(session_id, state.cwd)

        self._cancelled.discard(session_id)
        if text.strip() and not _is_resume(text):
            lab = lab_module.get()
            if lab.session_id == session_id:
                lab.set_mission(text)

        checkpoint_file = _checkpoint_path(session_id)
        # Set only by a screenshot turn: how to put the session's model back, and what was seen.
        restore = None
        vision: dict | None = None
        if _is_resume(text):
            if not checkpoint_file.exists():
                if self._conn is not None:
                    await self._conn.session_update(session_id=session_id, update=acp.update_agent_message_text("No interrupted turn to resume."))
                return acp.PromptResponse(stopReason="end_turn")
            if text.strip() == AUTO_RESUME_MARKER:
                if _auto_resume_attempts(session_id) >= MAX_AUTO_RESUMES:
                    if self._conn is not None:
                        await self._conn.session_update(session_id=session_id, update=acp.update_agent_message_text(f"Automatic resume already tried {MAX_AUTO_RESUMES} times from this checkpoint; not retrying on its own. /resume-turn tries again."))
                    return acp.PromptResponse(stopReason="end_turn")
                _count_auto_resume(session_id)
            data = json.loads(checkpoint_file.read_text())
            agent_input: object = {"checkpointResume": {"checkpoint": data["checkpoint"]}}
        else:
            checkpoint_file.unlink(missing_ok=True)  # a new turn supersedes an abandoned one
            _resume_marker_path(session_id).unlink(missing_ok=True)
            agent_input = text
            # The conversation as it is before this turn, so "go back" can return to it (session_store.py).
            await session_store.begin_turn(agent, text, SESSIONS_DIR)
            # A screenshot from the menu bar: this turn runs on the vision model with the image beside
            # the person's words, and skips the router — a specialist or the team would never see it.
            vision = os_context.vision_turn(os_context.pending(session_id))
            if vision is not None:
                restore, why = _model_for_vision(agent, vision)
                if restore is None:
                    os_context.not_sent(session_id, why or "the screenshot was not sent")
                    vision = None
                else:
                    agent_input = [{"text": text}, os_context.image_block(vision)]
            decision = None if vision is not None else await self._decide(state, text)
            if decision is not None:
                extra = self._apply_bandit(state, decision, text)
                recorder = outcomes.active(session_id)
                if recorder is not None:
                    recorder.set_route(
                        {
                            "target": decision.target.id if decision.target else None,
                            "action": decision.action,
                            "tier": decision.tier(),
                            "abstained": decision.abstained,
                            "confidence": round(decision.calibrated, 3),
                            "risk": decision.risk,
                            **extra,
                        }
                    )
                self._send_ext("sierralab/laya", {"sessionId": session_id, "mode": "route", "router": "v2", **decision.to_event()})
                if decision.action == "dispatch" and decision.target and decision.target.id in state.specialists:
                    stop_reason = await self._dispatch(session_id, state, decision, text)
                    return acp.PromptResponse(stopReason=stop_reason if stop_reason in ("end_turn", "max_tokens", "cancelled", "max_turn_requests") else "end_turn")
                state.router_hooks.prime(agent, decision)

        try:
            # No mid-turn checkpoints for a screenshot turn: a checkpoint is the history written to disk,
            # and an interrupted one would keep the image there for a resume nobody may ask for.
            stop_reason, _result = await self._stream(session_id, agent, agent_input, checkpoints=restore is None)
        finally:
            if restore is not None:
                # The detour ends with the turn, however it ended: the session's own model back, and
                # the image out of the history before the conversation is saved again.
                restore()
                if os_context.strip_images(agent.messages, (vision or {}).get("app", "the frontmost window")):
                    manager = getattr(agent, "_session_manager", None)
                    if manager is not None:
                        try:
                            manager.sync_agent(agent)
                        except Exception:
                            logging.getLogger(__name__).warning("os-context: the image was removed from memory but the session could not be re-saved", exc_info=True)
        if stop_reason != "cancelled":
            checkpoint_file.unlink(missing_ok=True)
            _resume_marker_path(session_id).unlink(missing_ok=True)
        return acp.PromptResponse(stopReason=stop_reason if stop_reason in ("end_turn", "max_tokens", "cancelled", "max_turn_requests") else "end_turn")


class _ProtocolStdout:
    """``sys.stdout`` while serving ACP: text writes (``print`` — Strands'
    default PrintingCallbackHandler, strands_tools' rich panels, a stray debug
    line) go to stderr; only the binary ``.buffer`` is the real stdout, which
    acp.stdio's transport writes JSON-RPC frames to.

    Why this exists: ACP frames and ``print()`` shared one stream, so streamed
    reply text printed *without a newline* glued itself onto the next frame
    (``pong{"jsonrpc":...}``). The client could not parse that line, answered
    ``-32700 Parse error`` and dropped the frame — every agent_message_chunk
    could vanish, deterministically with PYTHONUNBUFFERED=1 and at random
    (whenever the 8 KB pipe buffer flushed) without it.
    """

    def __init__(self, protocol_file, stray) -> None:
        # `.buffer` is what acp.stdio's Windows transport writes to;
        # `.fileno()` is what its POSIX path hands to loop.connect_write_pipe —
        # both must be the protocol stream, never stderr.
        self.buffer = protocol_file
        self._protocol = protocol_file
        self._stray = stray

    def fileno(self) -> int:
        return self._protocol.fileno()

    def close(self) -> None:
        self._protocol.close()

    def write(self, text: str) -> int:
        return self._stray.write(text)

    def flush(self) -> None:
        self._stray.flush()

    def isatty(self) -> bool:
        return False

    def __getattr__(self, name):
        return getattr(self._stray, name)


def _protect_protocol_stdout() -> None:
    """Reserve the real stdout for ACP frames and send everything else to stderr.

    fd 1 is re-pointed at stderr too, so a child process that inherits stdout
    (or any C-level write) can't reach the protocol stream either.
    """
    import sys

    sys.stdout.flush()
    protocol_fd = os.dup(1)
    os.dup2(2, 1)
    sys.stdout = _ProtocolStdout(os.fdopen(protocol_fd, "wb", buffering=0), sys.stderr)


async def _main() -> None:
    # Runs inside this process's one real, long-running event loop — see
    # start_mcp_clients' own docstring for why that matters: whichever
    # SierraLabAcpAgent session first calls build_agent_for_session already
    # tries to start these same MCP clients itself, from a throwaway
    # thread/loop that doesn't survive to serve a real tool call afterward.
    _protect_protocol_stdout()
    await start_mcp_clients()
    # One JSON-RPC message is one line, and asyncio's default line limit is 64 KiB: a prompt with a
    # large embedded file, or the menu bar's screenshot (`_sierralab/os-context`, up to 4 MiB of
    # JPEG as base64), would otherwise end the connection. The supervisor's side reads with a far
    # larger limit (`Supervisor.FRAME_LIMIT`); this keeps the two in the same order of magnitude.
    reader, writer = await acp.stdio.stdio_streams(limit=STDIO_LINE_LIMIT)
    # acp.run_agent's params are misleadingly named: the first stream must be
    # a StreamWriter (what the connection writes outgoing messages to), the
    # second a StreamReader (what it reads incoming messages from) — the
    # opposite of the "write to input / read from output" in its docstring.
    await acp.run_agent(SierraLabAcpAgent(), writer, reader)


if __name__ == "__main__":
    asyncio.run(_main())

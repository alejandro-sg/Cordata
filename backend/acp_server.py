"""ACP front door: the same Strands agent (agent_common.py) over the Agent
Client Protocol (https://agentclientprotocol.com), for editors like Zed that
speak ACP over stdio. See main.py for the AG-UI/web front door.

Run directly — an ACP client spawns this process and talks JSON-RPC over
its stdin/stdout, so don't print anything else to stdout:

    python acp_server.py

A conversation resumes across restarts and across front doors: this uses
the same on-disk FileSessionManager keyed by session id that main.py uses
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

import edit_preview
import harness_events
import kg
import laya_risk
import lsp_bridge
import patch_tool
import settings as app_settings
import settings_store
import specialists
import state_store
import team_pipeline
import trace_store
import vault_proxy
import workspace_context
from laya_router import BASE_TARGETS, LayaRouter, Policy, RouteDecision, RouterHooks, plugin_targets, summarize_stats
from plugin_host import PluginHost, PluginSpec, run_blocking
from self_heal import HealConfig, SelfHealHooks

# Last: importing agent_common chdirs the process into backend/workspace/, after
# which bare-module imports only resolve when this file runs as a script.
from agent_common import GENERATED_TOOL_NAMES, MCP_TOOL_NAMES, SESSIONS_DIR, build_agent_for_session, start_mcp_clients  # noqa: E402
from model_control_tool import _context_window_limit, _fetch_available_models  # noqa: E402

# Set-identical to main.py's _APPROVAL_REQUIRED_TOOLS — both gate `editor`
# and every MCP-server-provided tool too (main.py's own comment on
# MCP_TOOL_NAMES explains why); this repo has no shared source for the two
# lists, so keep them in sync by hand when either changes. apply_patch is
# ACP-only (the harness's patch tool). Plugins added at runtime are gated by
# PluginHost.requires_approval, not by this static set.
APPROVAL_REQUIRED_TOOLS = {"shell", "python_repl", "file_write", "editor", "apply_patch", *GENERATED_TOOL_NAMES, *MCP_TOOL_NAMES}

# Plan mode runs only these (reading code is part of planning, as in Claude
# Code's and OpenCode's plan modes); everything else is denied.
PLAN_MODE_TOOLS = {"file_read", "think", "current_time", "calculator", "architect", *(t.tool_name for t in lsp_bridge.LSP_TOOLS)}

# editor's read-only commands: like file_read, they never ask (and run in plan mode).
_EDITOR_READ_COMMANDS = {"view", "find_line"}


def _read_only_call(tool_name: str, tool_input) -> bool:
    return tool_name == "editor" and isinstance(tool_input, dict) and tool_input.get("command") in _EDITOR_READ_COMMANDS


# A turn Laya rates destructive with confidence arms the gate for these, even in auto mode.
_ARMABLE_TOOLS = {"shell", "python_repl", "file_write", "editor", "apply_patch"}

_TOOL_KIND: dict[str, str] = {
    "shell": "execute",
    "python_repl": "execute",
    "file_write": "edit",
    "editor": "edit",
    "apply_patch": "edit",
    "file_read": "read",
    "http_request": "fetch",
    "think": "think",
    "agent_memory": "other",
    "local_swarm": "other",
    "local_graph": "other",
    "team": "other",
    "a2a_client": "fetch",
    "architect": "think",
    "debugger": "other",
    **{t.tool_name: "search" for t in lsp_bridge.LSP_TOOLS},
}


def _tool_kind(tool_name: str) -> str:
    return _TOOL_KIND.get(tool_name, "other")


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


def _settled_intervention(interrupt_id: str, session_id: str, tool_name: str, tool_input) -> dict | None:
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
    pause. Anything unreadable, mismatched or from another session re-prompts —
    the fail-closed direction.
    """
    row = state_store.get_intervention(interrupt_id)
    if not row or row.get("status") == "staged":
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


# How much of a running sub-agent's latest reasoning one progress update carries.
_REASONING_TAIL_CHARS = 120


def _render_swarm_progress(entries: list, reasoning: dict | None = None) -> str:
    """Render local_swarm/local_graph's cumulative progress log (the same
    node_start/node_stop/handoff shape sub_agent_common.progress_entry
    builds for both tools) into plain text for one ACP tool-call update.

    Strands only wraps an async-generator tool's own intermediate yields
    (everything before its final return value) as a top-level {"type":
    "tool_stream", "tool_stream_event": {"tool_use", "data"}} event —
    confirmed against the installed strands/types/_events.py's
    ToolStreamEvent. Before the branch below existed, this file's prompt()
    loop had no case for "tool_stream" at all, so every {"state":
    {"swarm_progress": [...]}} local_swarm/local_graph yields mid-run (see
    their own stream_async loops) was silently dropped here — Zed saw the
    tool start and its final result, nothing in between, unlike the
    CopilotKit frontend's SwarmProgress.tsx, which reads the same state
    through AG-UI's separate StateSnapshotEvent translation
    (ag_ui_strands's ToolStreamEventHandler).
    """
    lines = []
    for entry in entries:
        kind = entry.get("type")
        if kind == "node_start":
            lines.append(f"started: {entry.get('node')}")
        elif kind == "node_stop":
            lines.append(f"finished: {entry.get('node')} - {entry.get('summary', '')}")
        elif kind == "handoff":
            frm = ", ".join(entry.get("from") or [])
            to = ", ".join(entry.get("to") or [])
            lines.append(f"handoff: {frm} -> {to} - {entry.get('message', '')}")

    # Live "what is each running sub-agent thinking right now": the tail of the
    # reasoning each node has streamed so far (sub_agent_common.reasoning_delta
    # feeds it). Only nodes that have started and not finished get a line, so a
    # finished node's long reasoning never lingers. Without this the ACP client
    # saw the orchestrator's own thoughts but nothing of the sub-agents'.
    finished = {e.get("node") for e in entries if e.get("type") == "node_stop"}
    for entry in entries:
        node = entry.get("node") if entry.get("type") == "node_start" else None
        text = (reasoning or {}).get(node) if node and node not in finished else None
        if text:
            tail = " ".join(text.split())[-_REASONING_TAIL_CHARS:]
            lines.append(f"thinking: {node} - {tail}")
    return "\n".join(lines)


# Session Modes (https://agentclientprotocol.com/protocol/session-modes) —
# the same three-way split Claude Code itself exposes (plan / default /
# bypass-permissions), mapped onto real behavioral differences in
# _ApprovalHandler.before_tool_call below, not just relabeled UI state:
#   plan  — no tool actually runs; every call is denied with a message
#           telling the model to describe its plan instead.
#   build — today's default: only APPROVAL_REQUIRED_TOOLS pause for a real
#           permission prompt, everything else runs immediately.
#   auto  — nothing pauses, ever. Opt-in only; never the default.
_MODES = [
    acp.schema.SessionMode(id="plan", name="Plan", description="Discuss and plan only — no tool actually runs."),
    acp.schema.SessionMode(id="build", name="Build", description="Tools run after your approval (default)."),
    acp.schema.SessionMode(id="auto", name="Auto", description="Tools run with no approval prompts. Use with caution."),
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

    def before_tool_call(self, event):
        """Mode first, with the user's permission rules (settings_store.py) laid
        over it: a ``deny`` rule blocks in every mode — ``auto`` included — and an
        ``allow`` / ``ask`` rule decides whether a call pauses for approval.

        Then the harness layers, none of which can *remove* a deny or plan block:
        Laya arming (a confidently destructive turn asks even in auto mode), the
        self-heal loop's two bounded pre-approvals, and per-plugin approval."""
        tool_name = event.tool_use.get("name")
        tool_input = event.tool_use.get("input")

        # Read once: `evaluate` below and the risk score recorded with the
        # prompt must come from the same settings, or the ledger's explanation
        # of why a call was held up disagrees with the decision itself.
        preferences = settings_store.current(self.cwd)
        verdict = settings_store.evaluate(preferences, tool_name, tool_input, self.cwd)
        if verdict is not None and verdict.action == "deny":
            return Deny(reason=f"{tool_name} was not run: blocked by a {verdict.explain()}. Do something else or ask the user.")
        if self.vault is not None and self.vault.blocks_egress and tool_name in vault_proxy.EGRESS_TOOLS:
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
        armed = (
            self.router_hooks is not None
            and self.session_agent is not None
            and tool_name in _ARMABLE_TOOLS
            and not read_only
            and self.router_hooks.armed(self.session_agent)
        )
        if self.mode == "auto" and not armed:
            return Proceed()
        if not armed and verdict is not None and verdict.action == "allow":
            return Proceed()
        if not armed and self.heal is not None and self.heal.preapproved(tool_name, tool_input):
            harness_events.emit("heal", {"event": "heal", "phase": "auto_approved", "tool": tool_name})
            return Proceed()
        plugin_gate = self.plugins.requires_approval(tool_name) if self.plugins is not None else None
        if not armed and verdict is None:
            if plugin_gate is False:
                return Proceed()
            if plugin_gate is None and (tool_name not in APPROVAL_REQUIRED_TOOLS or read_only):
                return Proceed()

        risk = self._risk(preferences)
        payload = json.dumps(
            {
                "name": tool_name,
                "input": tool_input,
                "tool_use_id": event.tool_use.get("toolUseId"),
                "always_rule": None if armed else settings_store.suggest_rule(tool_name, tool_input),
                **({"reason": "Laya rated this turn destructive — asking even in auto mode"} if armed else {}),
                **({"risk": risk.to_event()} if risk else {}),
            }
        )
        return Confirm(prompt=payload)

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
        "For multi-line or multi-file changes prefer apply_patch (a unified diff). After a command or "
        "test fails, read the [self-heal] notes appended to its result: they carry a root cause and, "
        "usually, a patch already verified in a sandbox copy."
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
    )


# The marker a client sends (acp-tui's /resume-turn) to continue a turn that a
# crash or kill interrupted between two checkpoints.
RESUME_TURN_MARKER = "[resume interrupted turn]"
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

        router = _router()
        router.policy = _router_policy(preferences)
        lsp_enabled = preferences.get("lsp.enabled", True) is not False
        manager = lsp_bridge.manager_for(cwd)
        manager.enabled = lsp_enabled

        reserved = {"shell", "editor", "file_read", "file_write", "python_repl", "http_request", "apply_patch", "architect", "debugger"}
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
            hooks_factory=lambda: [lsp_bridge.LspEditHooks(), router_hooks],
        )
        instructions = settings_store.load_instructions(cwd)
        context = workspace_context.summary(cwd, specialists=list(specialist_agents))
        system_prompt = _project_system_prompt(cwd) + f"\n\n{context}" + (f"\n\n{instructions}" if instructions else "")
        # Order matters for AfterInvocationEvent (callbacks run in reverse):
        # self-heal is registered last so it runs first and can set `resume`
        # before the router decides whether the turn is over.
        agent = build_agent_for_session(
            session_id,
            interventions=[handler],
            system_prompt=system_prompt,
            extra_tools=[patch_tool.apply_patch, *specialist_tools],
            extra_hooks=[lsp_bridge.LspEditHooks(), router_hooks, heal],
            exclude_tools={"agent_memory"},
            use_triage=False,
            untrusted_tools=plugins.untrusted_names,
            checkpointing=True,
        )
        handler.session_agent = agent
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
        state = self._build_session(session_id, cwd)

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
                await self._conn.session_update(
                    session_id=session_id,
                    update=acp.update_agent_message_text(
                        f"\n[This session's last turn was interrupted at a checkpoint ({where}). "
                        "Send /resume-turn to continue it from there.]\n"
                    ),
                )
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

        # Mode isn't persisted across restarts (FileSessionManager only
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
            if (SESSIONS_DIR / f"session_{sid}").exists() and (cwd is None or os.path.realpath(meta.get("cwd", "")) == os.path.realpath(cwd))
        ]
        rows.sort(key=lambda r: r.updated_at or "", reverse=True)
        return acp.schema.ListSessionsResponse(sessions=rows[:50])

    async def set_session_mode(self, session_id: str, mode_id: str, **kwargs) -> acp.SetSessionModeResponse | None:
        state = self._sessions.get(session_id)
        if state is None or mode_id not in _MODE_IDS:
            return None
        state.handler.mode = mode_id
        return acp.schema.SetSessionModeResponse()

    async def set_config_option(self, config_id: str, session_id: str, value, **kwargs):
        state = self._sessions.get(session_id)
        if state is None:
            return None
        if config_id == "model" and isinstance(value, str):
            _apply_model(state.agent, value)
        elif config_id == "temperature" and isinstance(value, str):
            _apply_temperature(state.agent, value)
        return acp.schema.SetSessionConfigOptionResponse(config_options=_config_options(state.agent))

    async def cancel(self, session_id: str, **kwargs) -> None:
        self._cancelled.add(session_id)

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
        if name == "heal/status":
            heal = state.heal if state else None
            episodes = [
                {"command": ep.command, "status": ep.status, "attempts": len(ep.attempts), "root_cause": ep.root_cause, "verified": ep.verified}
                for ep in (heal.episodes.values() if heal else [])
            ]
            config = heal.config if heal else HealConfig()
            return {"episodes": episodes, "config": config.__dict__}
        raise acp.RequestError.method_not_found(method)

    async def ext_notification(self, method: str, params: dict) -> None:
        return None

    # ------------------------------------------------------ interrupts

    async def _resolve_interrupts(self, session_id: str, interrupts: list) -> list:
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
            except (TypeError, ValueError):
                tool_name, tool_input, tool_use_id, always_rule, why = str(interrupt.reason), None, interrupt.id, None, None

            settled = _settled_intervention(interrupt.id, session_id, tool_name, tool_input)
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
                responses.append({"interruptResponse": {"interruptId": interrupt.id, "response": approved}})
                continue

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
                )
                approved = _permission_approved(result.outcome, options)
                option_id = getattr(result.outcome, "option_id", None)
                if approved and always_rule and option_id == "allow_always":
                    self._remember_always(session_id, always_rule)
            if not approved and state is not None:
                state.router_hooks.note_rejection(state.agent)

            rule = always_rule if (approved and option_id == "allow_always") else None
            state_store.resolve_intervention(
                interrupt.id,
                "approved" if approved else "rejected",
                # Honest, and it matters: with no client attached nothing was
                # asked, and a rejection that reads as a user decision would
                # misattribute a fail-closed default to a person.
                decided_by="user" if self._conn is not None else "no-client",
                rule=rule,
            )
            state_store.append_event(
                "intervention",
                {
                    "phase": "resolved",
                    "interrupt_id": interrupt.id,
                    "tool": tool_name,
                    "outcome": "approved" if approved else "rejected",
                    "decided_by": "user" if self._conn is not None else "no-client",
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
        while True:
            stop_reason = "end_turn"
            pending_interrupts: list = []
            checkpoint = None

            async for event in agent.stream_async(agent_input):
                if session_id in self._cancelled:
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
                        stop_reason = reason if reason in ("end_turn", "max_tokens") else "end_turn"
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
                    state = data.get("state") if isinstance(data, dict) else None
                    entries = (state or {}).get("swarm_progress")
                    if tool_use_id and entries:
                        rendered = _render_swarm_progress(entries, (state or {}).get("reasoning"))
                    elif tool_use_id and isinstance(data, dict) and tool_use.get("name") in self._specialist_names(session_id):
                        progress = specialist_progress.setdefault(tool_use_id, _SpecialistProgress())
                        if progress.feed(data):
                            rendered = progress.render(tool_use.get("name"))
                    # The swarm/graph tools re-yield their whole state every
                    # ~24 reasoning characters; a client only needs to hear
                    # when the rendered text actually changed.
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
                # stream_async (confirmed against the installed 1.55.1: the
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
                    await self._conn.session_update(
                        session_id=session_id,
                        update=acp.update_agent_message_text(event["data"]),
                    )
                elif "reasoningText" in event:
                    await self._conn.session_update(
                        session_id=session_id,
                        update=acp.update_agent_thought_text(event["reasoningText"]),
                    )

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
                self._ingest_turn(session_id, agent_input, traced)
                return stop_reason, last_result
            agent_input = await self._resolve_interrupts(session_id, pending_interrupts)

    def _ingest_turn(self, session_id: str, agent_input, traced: dict) -> None:
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
        prompt = agent_input if isinstance(agent_input, str) else None
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
        stop_reason, result = await self._stream(session_id, specialist, prompt, checkpoints=False)
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
                        manager.append_message(message, orchestrator)
                    except Exception:
                        logging.getLogger(__name__).debug("could not persist dispatched turn", exc_info=True)
        return stop_reason

    async def _dispatch_team(self, session_id: str, state: _Session, decision: RouteDecision, text: str) -> str | None:
        """Laya's confident ``team`` route, executed as a harness-level
        pipeline (team_pipeline.py): one planner call decomposes the task,
        then the exact same Strands graph machinery the local_graph tool uses
        runs it — streamed into one ACP tool call, its result written back
        into the orchestrator's history. Same safety as local_graph's own
        runs: build_agent_nodes' denylist still holds for every node.

        Returns None when the plan is unusable or the pipeline failed; the
        decision is then reclassified to an advisory hint and the caller must
        continue the ordinary turn — a pipeline failure never ends or hides a
        turn. On success returns "end_turn"."""
        context = _recent_context(state.agent.messages)
        plan = await team_pipeline.plan_team_async(text, state.agent, context)
        if plan is None:
            decision.action, decision.reason = "hint", "team pipeline planning failed; the orchestrator decides"
            return None

        tool_use_id = f"team-{uuid.uuid4().hex[:8]}"
        if self._conn is not None:
            await self._conn.session_update(
                session_id=session_id,
                update=acp.start_tool_call(
                    tool_use_id,
                    "team",
                    kind=_tool_kind("team"),
                    status="in_progress",
                    raw_input={"task": text, "routed_by": "laya", "nodes": [entry.get("name") for entry in plan["agents"]]},
                ),
            )

        # Stream_graph re-yields the whole cumulative state on every event;
        # like the tool_stream branch in _stream, the client only hears when
        # the rendered text actually changed.
        last_rendered = None
        result = None
        try:
            async for value in team_pipeline.stream_team(plan["task"] or text, plan, state.agent):
                if "state" in value:
                    state_data = value["state"]
                    rendered = _render_swarm_progress(state_data.get("swarm_progress") or [], state_data.get("reasoning"))
                    if rendered and rendered != last_rendered and self._conn is not None:
                        last_rendered = rendered
                        await self._conn.session_update(
                            session_id=session_id,
                            update=acp.update_tool_call(
                                tool_use_id,
                                status="in_progress",
                                content=[acp.tool_content(acp.text_block(rendered))],
                            ),
                        )
                else:
                    result = value  # the result dict stream_graph yields last
        except Exception as error:
            logging.getLogger(__name__).warning("team pipeline failed: %s: %s", type(error).__name__, error)
            if self._conn is not None:
                await self._conn.session_update(
                    session_id=session_id,
                    update=acp.update_tool_call(tool_use_id, status="failed", content=None),
                )
            decision.action, decision.reason = "hint", "team pipeline execution failed; the orchestrator decides"
            return None

        result = result or {"status": "FAILED", "execution_time_ms": 0, "results": {}}
        succeeded = str(result.get("status", "")).upper() == "COMPLETED"
        summary = "\n".join(f"{name}: {summary}".strip()[:400] for name, summary in (result.get("results") or {}).items())
        if self._conn is not None:
            await self._conn.session_update(
                session_id=session_id,
                update=acp.update_tool_call(
                    tool_use_id,
                    status="completed" if succeeded else "failed",
                    content=[acp.tool_content(acp.text_block(summary))] if summary else None,
                ),
            )

        if succeeded or result.get("results"):
            orchestrator = state.agent
            for message in (
                {"role": "user", "content": [{"text": text}]},
                {"role": "assistant", "content": [{"text": f"[team pipeline, routed by Laya — status {result.get('status')}]\n{summary}"}]},
            ):
                orchestrator.messages.append(message)
                manager = getattr(orchestrator, "_session_manager", None)
                if manager is not None:
                    try:
                        manager.append_message(message, orchestrator)
                    except Exception:
                        logging.getLogger(__name__).debug("could not persist team turn", exc_info=True)

        # The orchestrator never streamed, so no BeforeInvocation hook fired;
        # score the turn manually, exactly as the specialist dispatch's hooks
        # would (begin_external_turn + finish_turn).
        turn = state.router_hooks.begin_external_turn(state.agent, decision, text)
        state.router_hooks.finish_turn(turn, "end_turn", dispatched_ok=succeeded)
        return "end_turn"

    async def prompt(self, session_id: str, prompt: list, **kwargs) -> acp.PromptResponse:
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

        checkpoint_file = _checkpoint_path(session_id)
        if text.strip() == RESUME_TURN_MARKER:
            if not checkpoint_file.exists():
                if self._conn is not None:
                    await self._conn.session_update(session_id=session_id, update=acp.update_agent_message_text("No interrupted turn to resume."))
                return acp.PromptResponse(stopReason="end_turn")
            data = json.loads(checkpoint_file.read_text())
            agent_input: object = {"checkpointResume": {"checkpoint": data["checkpoint"]}}
        else:
            checkpoint_file.unlink(missing_ok=True)  # a new turn supersedes an abandoned one
            agent_input = text
            decision = await self._decide(state, text)
            if decision is not None:
                self._send_ext("sierralab/laya", {"sessionId": session_id, "mode": "route", "router": "v2", **decision.to_event()})
                if decision.action == "dispatch" and decision.target and decision.target.id in state.specialists:
                    stop_reason = await self._dispatch(session_id, state, decision, text)
                    return acp.PromptResponse(stopReason=stop_reason if stop_reason in ("end_turn", "max_tokens", "cancelled") else "end_turn")
                if decision.action == "dispatch" and decision.target and decision.target.id == "team":
                    # None means the plan/pipeline failed open: the decision is
                    # already reclassified to a hint, and prime below applies it
                    # for the ordinary turn (fall-through, no return).
                    stop_reason = await self._dispatch_team(session_id, state, decision, text)
                    if stop_reason is not None:
                        return acp.PromptResponse(stopReason=stop_reason if stop_reason in ("end_turn", "max_tokens", "cancelled") else "end_turn")
                state.router_hooks.prime(agent, decision)

        stop_reason, _result = await self._stream(session_id, agent, agent_input, checkpoints=True)
        if stop_reason != "cancelled":
            checkpoint_file.unlink(missing_ok=True)
        return acp.PromptResponse(stopReason=stop_reason if stop_reason in ("end_turn", "max_tokens", "cancelled") else "end_turn")


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
    reader, writer = await acp.stdio.stdio_streams()
    # acp.run_agent's params are misleadingly named: the first stream must be
    # a StreamWriter (what the connection writes outgoing messages to), the
    # second a StreamReader (what it reads incoming messages from) — the
    # opposite of the "write to input / read from output" in its docstring.
    await acp.run_agent(SierraLabAcpAgent(), writer, reader)


if __name__ == "__main__":
    asyncio.run(_main())

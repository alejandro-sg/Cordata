"""The approval gate: which tools ask, which run in plan mode, the session modes, how a permission answer is read,
the replay of an answer already given, and ``_ApprovalHandler`` — preflight → deny rules → vault → mode → Laya Guard
→ ask or proceed, with a checkpoint before anything that may change files.

Split out of ``acp_server.py``, which re-exports every name here.
"""

import asyncio
import json
import collections
import logging
import os
import acp
import acp.stdio
from strands.interventions import Confirm, Deny, InterventionHandler, Proceed
import checkpoints
import bandit
import claude_code_tool
import harness_events
import laya_guard
import laya_risk
import outcome_hooks
import outcomes
import secret_guard
import stall_guard
import lsp_bridge
import ollama_resource_tool
import settings_store
import state_store
import orchestrate_tool
import vault_proxy
import plugin_host
import preflight
from laya_router import RouteDecision
from agent_common import GENERATED_TOOL_NAMES, MCP_TOOL_NAMES, current_generated_tool_names  # noqa: E402
from acp_learning import _bandit_policy  # noqa: E402

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
    remote door — the Discord bot — and stamps `_meta.cordata.decidedBy` on the
    response it writes to this child. Everything else (a TUI, the GUI's modal, a
    Zed session) sends no `_meta` and is a person on the socket. Returns None
    rather than a guess when the slot is absent or shaped differently."""
    meta = getattr(result, "field_meta", None)
    slot = meta.get("cordata") if isinstance(meta, dict) else None
    door = slot.get("decidedBy") if isinstance(slot, dict) else None
    return str(door) if isinstance(door, str) and door else None


def _answer_note(result) -> str:
    """What the person wrote about a rejection, when a front door relayed it (`_meta.cordata.note`). Their text, capped."""
    meta = getattr(result, "field_meta", None)
    slot = meta.get("cordata") if isinstance(meta, dict) else None
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


class _ApprovalHandler(InterventionHandler):
    """Gates dangerous tools via Strands' native Interventions system
    (strands.interventions — a transport-agnostic pause/resume mechanism
    built on the same Interrupt/InterruptException primitive ag_ui_strands
    uses internally for the web front door's HITL, but with no AG-UI runtime
    underneath it here).

    One instance per session, held onto directly (see CordataAcpAgent's
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
        #: made in the same context the turn's other choices were (`CordataAcpAgent._apply_bandit`). None when
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

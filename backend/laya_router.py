"""Laya as System 1: the harness's typed router, abstention gate and outcome
learner. The LLM (System 2) stays the thing that generates text and calls tools.

Per user turn, **one** Laya forward pass (~18 ms warm on Apple-silicon MPS,
measured) answers three typed questions:

* ``route``      (choice) — which target should handle the turn: the
  orchestrator itself (``answer`` / ``code`` / ``web`` / ``memory`` / ``team``),
  a specialist (``architect`` / ``debugger``), or a running MCP plugin
  (``plugin_<name>``, from the plugin's own description);
* ``risk``       (score)  — read-only / edits files / destructive;
* ``complexity`` (score)  — trivial / moderate / hard (sub-agent model tier).

Then policy, in this order:

1. **Abstain → fail closed.** Laya's own abstention flag (``min_confidence``
   on ``answer_confidence``) or a calibrated confidence under the gate
   (``laya.threshold``) means *no routing action*: with ``laya.onAbstain =
   escalate`` (default) the turn runs on the frontier model tier
   (``tiers.strong``) instead of the default model; ``llm`` lets the default
   LLM decide unaided. Either way nothing is dispatched or hidden on a guess.
2. **Confident specialist** (``architect`` / ``debugger``) with ``laya.dispatch
   = direct`` (default): the ACP server runs that specialist directly — the
   orchestrator's model call that would only have decided to delegate is
   skipped. ``hint`` mode keeps the old advisory line instead. A confident
   ``team`` route is always a hint naming the ``orchestrate`` tool: running it
   spends coding-CLI quota and edits files, so the call goes through the
   approval gate like any other (``laya.teamDispatch`` is accepted and ignored;
   the in-process team pipeline it used to switch is gone).
3. **Confident plugin**: tool scoping — the turn's model calls see the core
   tools plus that plugin's tools only (other plugins' schemas are not sent),
   and a short hint names the plugin.
4. **Confident orchestrator target**: the fixed advisory hint, as before.
5. **Risk**: a confident ``destructive`` turn *arms* the approval gate — in
   auto mode, shell / edit calls ask anyway for that turn. Laya can only add
   prompts, never remove one.

Outcomes flow back (``laya_calibration``): every shell command gets a Laya
P(succeeds) before it runs and its exit status after (``command_ok``); every
turn's route is scored by whether it agreed with what actually happened and
whether the turn ended clean (no unresolved stderr failure, no rejected
approval, no error). Binning maps, decision weights and proper scores update
online and persist per user.
"""

from __future__ import annotations

import json
import logging
import re
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from strands.hooks import (
    AfterInvocationEvent,
    AfterToolCallEvent,
    BeforeInvocationEvent,
    BeforeToolCallEvent,
    HookProvider,
    HookRegistry,
)

import secret_guard
import settings
from laya_calibration import Calibrator
from laya_triage import actual_route, user_text

logger = logging.getLogger(__name__)


@dataclass(frozen=True)
class Target:
    id: str
    description: str
    kind: str  # orchestrator | specialist | plugin | tool
    plugin: str | None = None


BASE_TARGETS: tuple[Target, ...] = (
    Target("answer", "a conversational answer or explanation; no tools needed", "orchestrator"),
    Target("code", "reading, editing, building or running code and files in this project", "orchestrator"),
    Target("architect", "designing or planning a change before any code is written", "specialist"),
    Target(
        "claude_code",
        "a large refactor or implementation spanning many files that needs coordinated edits and test runs carried out end to end",
        "tool",
    ),
    Target("debugger", "fixing a failing command, test, build or error message", "specialist"),
    Target("team", "a large task with several independent parts for a team of specialists", "orchestrator"),
    Target("web", "looking something up on the web", "orchestrator"),
    Target("memory", "recalling or storing something the user said before", "orchestrator"),
)

#: Which tool a ``kind="tool"`` target routes to (and, in ``_SPECIALIST_TOOLS`` below, back).
TOOL_FOR_TARGET = {"claude_code": "claude_code_engineer"}

RISK_LEVELS = ("read_only", "edits_files", "destructive")
COMPLEXITY_LEVELS = ("trivial", "moderate", "hard")

_HINTS = {
    "code": "this looks like work on files or code: explore with file_read / lsp_* before editing",
    "web": "this looks like a web lookup, so http_request is the likely fit",
    "memory": "this looks like recalling or storing something the user said, so agent_memory is the likely fit",
    "team": "this looks like a multi-step task: the orchestrate tool can plan it, run it on the coding CLIs and audit the result",
    "architect": "this looks like design work: the architect specialist can plan it first",
    "debugger": "this looks like a failure to fix: the debugger specialist can diagnose and patch it",
    "claude_code": (
        "this is a large multi-file change to carry out end to end: hand the whole task to the claude_code_engineer tool "
        "(it edits files and runs the tests headless) instead of editing the files yourself"
    ),
}

COMMAND_QUESTION = {
    "command_ok": {
        "type": "noul",
        "instructions": "Will this shell command succeed (exit code 0) in a typical software project?",
        "criteria": {"true": "it succeeds", "false": "it fails with an error"},
    }
}


def plugin_targets(descriptions: list[tuple[str, str, str]]) -> list[Target]:
    out = []
    for name, kind, description in descriptions:
        label = "plugin_" + re.sub(r"[^a-z0-9_]", "_", name.lower())
        out.append(Target(label, f"using the {name} {kind} plugin: {description}"[:160], "plugin", plugin=name))
    return out


def questions_for(targets: list[Target]) -> dict[str, dict[str, Any]]:
    return {
        "route": {
            "type": "choice",
            "instructions": "Which part of a coding assistant should handle this request?",
            "criteria": {t.id: t.description for t in targets},
        },
        "risk": {"type": "score", "instructions": "How risky are the actions this request needs?", "criteria": list(RISK_LEVELS)},
        "complexity": {"type": "score", "instructions": "How complex is this task?", "criteria": list(COMPLEXITY_LEVELS)},
    }


@dataclass
class RouteDecision:
    target: Target | None
    confidence: float = 0.0  # weighted probability of the chosen target
    calibrated: float = 0.0
    laya_choice: str | None = None
    laya_confidence: float = 0.0
    low_confidence: bool = False
    abstained: bool = True
    reason: str = ""
    action: str = "none"  # dispatch | scope | tool | hint | escalate | none
    probabilities: dict = field(default_factory=dict)
    risk: str | None = None
    risk_confidence: float = 0.0
    complexity: float | None = None
    complexity_confidence: float = 0.0
    latency_ms: float | None = None
    error: str | None = None
    escalate_model: str | None = None
    suppress_hint: bool = False  # the route is confident but the policy declined to act on it
    #: Which arm the contextual bandit chose at each decision point this turn (`bandit.py`), so the turn's outcome
    #: can be attributed back to the choices that were made. Empty when the bandit is off. Never affects a guard.
    choices: dict = field(default_factory=dict)

    @property
    def armed(self) -> bool:
        return self.risk == "destructive" and self.risk_confidence >= 0.8

    def tier(self) -> str | None:
        if self.complexity is None or self.complexity_confidence < 0.6:
            return None
        return "fast" if self.complexity < 0.6 else "strong" if self.complexity > 1.4 else "standard"

    def hint(self) -> str | None:
        if self.abstained or self.target is None or self.suppress_hint:
            return None
        if self.target.kind == "plugin":
            text = f"this looks like a job for the {self.target.plugin} plugin's tools"
        else:
            text = _HINTS.get(self.target.id)
        if not text:
            return None
        return f"Routing hint from a fast local classifier (calibrated confidence {self.calibrated:.2f}; advisory): {text}."

    def to_event(self) -> dict[str, Any]:
        top = sorted(self.probabilities.items(), key=lambda kv: kv[1], reverse=True)[:3]
        return {
            "event": "decision",
            "route": self.target.id if self.target else None,
            "target_kind": self.target.kind if self.target else None,
            "route_confidence": round(self.confidence, 4),
            "calibrated": round(self.calibrated, 4),
            "laya_choice": self.laya_choice,
            "low_confidence": self.low_confidence,
            "abstained": self.abstained,
            "reason": self.reason,
            "action": self.action,
            "top": [[k, round(v, 3)] for k, v in top],
            "risk": self.risk,
            "risk_confidence": round(self.risk_confidence, 3),
            "armed": self.armed,
            "complexity": self.complexity,
            "complexity_confidence": round(self.complexity_confidence, 3),
            "tier": self.tier(),
            "latency_ms": self.latency_ms,
            "escalate_model": self.escalate_model,
            "error": self.error,
            # fields the v1 client line already understands
            "gate": settings.LAYA_TRIAGE_THRESHOLD,
            "needs_team": 1.0 if self.target and self.target.id == "team" else 0.0,
        }


@dataclass
class Policy:
    gate: float = 0.85
    min_confidence: float = 0.5
    dispatch: str = "direct"  # direct | hint
    # Accepted for old settings files and ignored: a ``team`` route is always a
    # hint naming the orchestrate tool (see the module docstring).
    team_dispatch: str = "hint"
    on_abstain: str = "escalate"  # escalate | llm
    frontier_model: str | None = None


def _default_predict(text: str, questions: dict, **kwargs) -> dict | None:
    from prompt_security import laya_predict

    return laya_predict(text, questions, **kwargs)


class LayaRouter:
    def __init__(
        self,
        predict: Callable[..., dict | None] = _default_predict,
        calibrator: Calibrator | None = None,
        policy: Policy | None = None,
    ) -> None:
        self._predict = predict
        self.calibrator = calibrator or Calibrator()
        self.policy = policy or Policy()

    @property
    def predict(self) -> Callable[..., dict | None]:
        """The forward pass this router asks its questions with — shared, so the skill decider
        (`skill_search.SkillDecider`) asks Laya through the same lazily-loaded instance and the same calibrator."""
        return self._predict

    # --------------------------------------------------------------- turns

    def decide(self, text: str, targets: list[Target]) -> RouteDecision:
        """One forward pass, then calibration and policy. Never raises."""
        started = time.perf_counter()
        policy = self.policy
        try:
            result = self._predict(secret_guard.redact(text), questions_for(targets), min_confidence=policy.min_confidence)
        except Exception as error:  # the classifier must never break a turn
            result, error_text = None, f"{type(error).__name__}: {error}"
        else:
            error_text = None if result is not None else "laya unavailable (weights not loaded or predict failed)"
        latency = round((time.perf_counter() - started) * 1000, 1)
        if result is None:
            return self._abstain(RouteDecision(target=None, latency_ms=latency, error=error_text), "no decision: " + (error_text or ""))

        answers = result["answers"]
        route = answers["route"]
        probabilities = {k: float(v) for k, v in (route.get("probabilities") or {}).items()}
        by_id = {t.id: t for t in targets}
        label, weighted = self.calibrator.choose("route", probabilities or {route["choice"]: route["answer_confidence"]})
        decision = RouteDecision(
            target=by_id.get(label),
            confidence=weighted,
            calibrated=self.calibrator.calibrated("route", label, weighted),
            laya_choice=route.get("choice"),
            laya_confidence=float(route.get("answer_confidence", 0.0)),
            low_confidence=bool(route.get("low_confidence")),
            probabilities=probabilities,
            latency_ms=latency,
        )
        risk = answers.get("risk") or {}
        if risk:
            level = float(risk.get("score", 0.0))
            decision.risk = RISK_LEVELS[min(len(RISK_LEVELS) - 1, max(0, int(round(level))))]
            decision.risk_confidence = float(risk.get("answer_confidence", 0.0))
        complexity = answers.get("complexity") or {}
        if complexity:
            decision.complexity = float(complexity.get("score", 0.0))
            decision.complexity_confidence = float(complexity.get("answer_confidence", 0.0))

        if decision.target is None:
            return self._abstain(decision, f"unknown route {label!r}")
        # Laya's flag means its *best* answer is under min_confidence — the whole
        # distribution is flat, so no option (re-weighted or not) is trustworthy.
        if decision.low_confidence:
            return self._abstain(decision, f"Laya abstained (answer_confidence {decision.laya_confidence:.2f} < min_confidence {policy.min_confidence:.2f})")
        if decision.confidence < policy.min_confidence:
            return self._abstain(decision, f"weighted confidence {decision.confidence:.2f} < min_confidence {policy.min_confidence:.2f}")
        if decision.calibrated < policy.gate:
            return self._abstain(decision, f"calibrated confidence {decision.calibrated:.2f} < gate {policy.gate:.2f}")

        decision.abstained = False
        kind = decision.target.kind
        if kind == "specialist" and policy.dispatch == "direct":
            decision.action, decision.reason = "dispatch", f"dispatched to the {label} specialist"
        elif kind == "tool":
            # The Claude Code CLI wrapper is the heavy path: acted on only when Laya is not
            # confident the task is trivial. A small change stays with the orchestrator's own
            # tools. The turn is still the orchestrator's — "tool" is a strong hint (it names the
            # tool), never a call made for it: the call goes through the same approval gate.
            if decision.tier() == "fast":
                decision.suppress_hint = True
                decision.action, decision.reason = "none", f"{label} is the heavy path and Laya rates this task trivial; the orchestrator handles it"
            else:
                decision.action, decision.reason = "tool", f"routed to the {TOOL_FOR_TARGET.get(label, label)} tool"
        elif kind == "plugin":
            decision.action, decision.reason = "scope", f"tools scoped to the {decision.target.plugin} plugin"
        elif decision.hint():
            decision.action, decision.reason = "hint", "advisory hint"
        else:
            decision.action, decision.reason = "none", "confident; nothing to change"
        return decision

    def _abstain(self, decision: RouteDecision, why: str) -> RouteDecision:
        decision.abstained = True
        if self.policy.on_abstain == "escalate" and self.policy.frontier_model:
            decision.action, decision.escalate_model = "escalate", self.policy.frontier_model
            decision.reason = f"{why} — escalated to {self.policy.frontier_model}"
        else:
            decision.action = "none"
            decision.reason = f"{why} — the LLM decides alone" + ("" if self.policy.frontier_model else " (no tiers.strong set to escalate to)")
        return decision

    def record_turn(self, decision: RouteDecision, success: bool) -> dict[str, float] | None:
        if decision.target is None or decision.abstained and decision.error:
            return None
        return self.calibrator.update("route", decision.target.id, decision.confidence, success)

    # ------------------------------------------------------------ commands

    def predict_command(self, command: str) -> float | None:
        try:
            result = self._predict(secret_guard.redact(command), COMMAND_QUESTION)
        except Exception:
            return None
        if not result:
            return None
        try:
            return float(result["answers"]["command_ok"]["noul"])
        except (KeyError, TypeError, ValueError):
            return None

    def record_command(self, p_ok: float, ok: bool) -> dict[str, float]:
        return self.calibrator.update("command_ok", "true", p_ok, ok)


# ------------------------------------------------------------------- hooks


def add_prompt_block(agent: Any, text: str) -> dict:
    """Append ``text`` to the system prompt as its own content block and return
    that block. Block-level, like AgentSkills' own injection: rewriting the
    prompt as one string would merge AgentSkills' block into it, so on the next
    turn it couldn't find (and replace) its block and would append it again —
    the skills list would grow by one copy per hinted turn."""
    block = {"text": text}
    blocks = list(agent.system_prompt_content or [])
    blocks.append(block)
    agent.system_prompt = blocks
    return block


def remove_prompt_block(agent: Any, block: dict) -> None:
    blocks = list(agent.system_prompt_content or [])
    if block in blocks:
        blocks.remove(block)
        agent.system_prompt = blocks or None


_SHELL_EXIT = re.compile(r"Exit Code:\s*(-?\d+)")
_SPECIALIST_TOOLS = {"architect": "architect", "debugger": "debugger", **{tool: target for target, tool in TOOL_FOR_TARGET.items()}}


def shell_commands(tool_input: Any) -> list[str]:
    if not isinstance(tool_input, dict):
        return []
    command = tool_input.get("command")
    if isinstance(command, str):
        return [command]
    if isinstance(command, list):
        return [c if isinstance(c, str) else str(c.get("command", "")) for c in command]
    return []


def command_ok(result: Any) -> bool | None:
    """Did a shell tool result succeed? Exit codes when present, else status."""
    if not isinstance(result, dict):
        return None
    text = "\n".join(c.get("text", "") for c in result.get("content") or [] if isinstance(c, dict))
    codes = [int(code) for code in _SHELL_EXIT.findall(text)]
    if codes:
        return all(code == 0 for code in codes)
    status = result.get("status")
    return None if status is None else status == "success"


def normalize_command(command: str) -> str:
    return " ".join(command.split())


@dataclass
class _Turn:
    decision: RouteDecision | None
    text: str
    started: float = field(default_factory=time.perf_counter)
    started_unix: float = field(default_factory=time.time)
    tools: list[str] = field(default_factory=list)
    commands: dict[str, tuple[str, float | None]] = field(default_factory=dict)  # tool_use_id -> (command, p_ok)
    failing: dict[str, bool] = field(default_factory=dict)  # normalized command -> last outcome ok
    rejected: int = 0
    seen_calls: set[str] = field(default_factory=set)
    hint: str | None = None
    hint_block: dict | None = None
    hidden_tools: dict[str, Any] = field(default_factory=dict)
    base_model: str | None = None


class RouterHooks(HookProvider):
    """The orchestrator side of the router: applies a decision for the turn
    (hint / tool scoping / escalation), predicts and learns command outcomes,
    and scores the turn when it ends."""

    def __init__(
        self,
        router: LayaRouter,
        *,
        notify: Callable[[dict[str, Any]], None] | None = None,
        plugin_host: Any = None,
        log_path: str | Path | None = None,
        predict_commands: bool = True,
    ) -> None:
        self.router = router
        self._notify = notify
        self.plugin_host = plugin_host
        self._log_path = Path(log_path or settings.LAYA_TRIAGE_LOG)
        self.predict_commands = predict_commands
        self._primed: dict[int, RouteDecision] = {}
        self._turns: dict[int, _Turn] = {}
        self._lock = threading.Lock()

    def register_hooks(self, registry: HookRegistry) -> None:
        registry.add_callback(BeforeInvocationEvent, self._on_start)
        registry.add_callback(BeforeToolCallEvent, self._on_tool)
        registry.add_callback(AfterToolCallEvent, self._after_tool)
        registry.add_callback(AfterInvocationEvent, self._on_end)

    # -------------------------------------------------------------- public

    def prime(self, agent: Any, decision: RouteDecision) -> None:
        """The ACP server decided before streaming; use that for the next turn."""
        with self._lock:
            self._primed[id(agent)] = decision

    def current(self, agent: Any) -> RouteDecision | None:
        turn = self._turns.get(id(agent))
        return turn.decision if turn else None

    def armed(self, agent: Any) -> bool:
        decision = self.current(agent)
        return bool(decision and decision.armed)

    def note_rejection(self, agent: Any) -> None:
        turn = self._turns.get(id(agent))
        if turn is not None:
            turn.rejected += 1

    def unresolved_failures(self, agent: Any) -> list[str]:
        turn = self._turns.get(id(agent))
        return [cmd for cmd, ok in (turn.failing.items() if turn else []) if not ok]

    def _emit(self, payload: dict[str, Any]) -> None:
        if self._notify is None:
            return
        try:
            self._notify({**payload, "mode": "route", "router": "v2"})
        except Exception as error:
            logger.warning("laya router: notification failed: %s", error)

    # --------------------------------------------------------------- hooks

    def _on_start(self, event: BeforeInvocationEvent) -> None:
        text = user_text(event.messages)
        if text is None or text.startswith("[self-heal]"):
            return  # an approval resume or a self-heal continuation of the same turn
        agent = event.agent
        key = id(agent)
        with self._lock:
            decision = self._primed.pop(key, None)
        previous = self._turns.pop(key, None)
        if previous is not None:
            self._restore(agent, previous)
        turn = _Turn(decision=decision, text=secret_guard.redact(text))
        self._turns[key] = turn
        if decision is None:
            return
        if decision.escalate_model:
            try:
                turn.base_model = agent.model.get_config().get("model_id")
                agent.model.update_config(model_id=decision.escalate_model)
            except Exception as error:
                logger.warning("laya router: could not escalate the model: %s", error)
                turn.base_model = None
        if decision.action == "scope" and self.plugin_host is not None and decision.target is not None:
            keep = set(self.plugin_host.tool_names(decision.target.plugin))
            for status in self.plugin_host.status():
                for name in status["tools"]:
                    if name not in keep:
                        tool_obj = agent.tool_registry.registry.pop(name, None)
                        if tool_obj is not None:
                            turn.hidden_tools[name] = tool_obj
        hint = decision.hint()
        if hint:
            try:
                turn.hint_block = add_prompt_block(agent, hint)
                turn.hint = hint
            except Exception as error:
                logger.warning("laya router: could not apply hint: %s", error)

    def _on_tool(self, event: BeforeToolCallEvent) -> None:
        turn = self._turns.get(id(event.agent))
        if turn is None:
            return
        name = str(event.tool_use.get("name") or "")
        tool_use_id = str(event.tool_use.get("toolUseId"))
        if tool_use_id in turn.seen_calls:
            return  # the same call again, resumed after an approval prompt
        turn.seen_calls.add(tool_use_id)
        if name == "subagent":
            # The specialists are `subagent` presets now; a call to one is still that specialist's route.
            role = (event.tool_use.get("input") or {}).get("agent_type") if isinstance(event.tool_use.get("input"), dict) else None
            if role in ("architect", "debugger"):
                name = role
        turn.tools.append(name)
        if name != "shell" or not self.predict_commands:
            return
        from prompt_security import laya_ready

        commands = shell_commands(event.tool_use.get("input"))
        if not commands or not laya_ready():
            return
        command = " && ".join(commands)
        p_ok = self.router.predict_command(command)
        turn.commands[tool_use_id] = (command, p_ok)
        if p_ok is not None:
            self._emit({"event": "command", "tool_use_id": tool_use_id, "p_ok": round(p_ok, 3)})

    def _after_tool(self, event: AfterToolCallEvent) -> None:
        turn = self._turns.get(id(event.agent))
        if turn is None or event.tool_use.get("name") != "shell":
            return
        ok = command_ok(event.result)
        if ok is None:
            return
        tool_use_id = str(event.tool_use.get("toolUseId"))
        command, p_ok = turn.commands.get(tool_use_id, (" && ".join(shell_commands(event.tool_use.get("input"))), None))
        turn.failing[normalize_command(command)] = ok
        payload: dict[str, Any] = {"event": "command_outcome", "tool_use_id": tool_use_id, "ok": ok}
        if p_ok is not None:
            scores = self.router.record_command(p_ok, ok)
            payload.update({"p_ok": round(p_ok, 3), "log_loss": round(scores["log_loss_raw"], 4)})
        self._emit(payload)

    def _on_end(self, event: AfterInvocationEvent) -> None:
        key = id(event.agent)
        turn = self._turns.get(key)
        if turn is None:
            return
        stop_reason = getattr(event.result, "stop_reason", None)
        if stop_reason in ("interrupt", "checkpoint") or event.resume is not None:
            return  # the same turn continues (approval, checkpoint pause, or a self-heal resume)
        self._turns.pop(key, None)
        self._restore(event.agent, turn)
        self.finish_turn(turn, str(stop_reason) if stop_reason else None)

    def _restore(self, agent: Any, turn: _Turn) -> None:
        if turn.hint_block is not None:
            try:
                remove_prompt_block(agent, turn.hint_block)
            except Exception:
                pass
            turn.hint, turn.hint_block = None, None
        for name, tool_obj in turn.hidden_tools.items():
            agent.tool_registry.registry.setdefault(name, tool_obj)
        turn.hidden_tools.clear()
        if turn.base_model:
            try:
                agent.model.update_config(model_id=turn.base_model)
            except Exception:
                pass
            turn.base_model = None

    # -------------------------------------------------------------- scoring

    def target_of(self, tools: list[str]) -> str:
        for name in tools:
            if name in _SPECIALIST_TOOLS:
                return _SPECIALIST_TOOLS[name]
            if self.plugin_host is not None:
                spec = self.plugin_host.owner(name)
                if spec is not None and spec.transport != "builtin":
                    return "plugin_" + re.sub(r"[^a-z0-9_]", "_", spec.name.lower())
        return {
            "chat": "answer",
            "files_and_code": "code",
            "web_research": "web",
            "memory": "memory",
            "team": "team",
            "other_tools": "code",
        }[actual_route(tools)]

    def finish_turn(self, turn: _Turn, stop_reason: str | None, dispatched_ok: bool | None = None) -> None:
        decision = turn.decision
        unresolved = [cmd for cmd, ok in turn.failing.items() if not ok]
        clean = stop_reason in ("end_turn", None) and not unresolved and turn.rejected == 0
        if dispatched_ok is not None:
            clean = clean and dispatched_ok
        actual = turn.decision.target.id if decision and decision.action == "dispatch" and decision.target else self.target_of(turn.tools)
        agreed = bool(decision and decision.target and decision.target.id == actual)
        scores = None
        if decision is not None and decision.target is not None and decision.error is None:
            scores = self.router.record_turn(decision, agreed and clean)
        record = {
            "ts": round(turn.started_unix, 3),
            "router": "v2",
            "mode": "route",
            "text": turn.text[:500],
            "actual": {
                "route": actual,
                "tools": turn.tools,
                "unresolved_failures": unresolved,
                "rejected": turn.rejected,
                "stop_reason": stop_reason,
                "turn_ms": round((time.perf_counter() - turn.started) * 1000, 1),
            },
            "laya": decision.to_event() if decision else None,
            "clean": clean,
            "agreed": agreed,
            "scores": scores,
        }
        self._emit(
            {
                "event": "outcome",
                "actual_route": actual,
                "agreed": agreed if decision and decision.target else None,
                "clean": clean,
                "tools": turn.tools,
                "tiers": {},
                "stop_reason": stop_reason,
                "turn_ms": record["actual"]["turn_ms"],
                "log_loss": round(scores["log_loss_cal"], 4) if scores else None,
            }
        )
        try:
            self._log_path.parent.mkdir(parents=True, exist_ok=True)
            with self._log_path.open("a", encoding="utf-8") as handle:
                handle.write(json.dumps(record, ensure_ascii=False) + "\n")
        except OSError as error:
            logger.warning("laya router: could not write %s: %s", self._log_path, error)

    def begin_external_turn(self, agent: Any, decision: RouteDecision, text: str) -> _Turn:
        """A turn the orchestrator never sees (direct specialist dispatch)."""
        return _Turn(decision=decision, text=secret_guard.redact(text))


def summarize_stats(calibrator: Calibrator) -> str:
    stats = calibrator.stats()
    if not stats:
        return "no outcomes recorded yet"
    rows = []
    for question, s in sorted(stats.items()):
        rows.append(
            f"{question}: n={s['n']} success={s['success_rate']:.0%}  log-loss raw {s['log_loss_raw']:.3f} → calibrated {s['log_loss_calibrated']:.3f}"
            f"  brier {s['brier_raw']:.3f} → {s['brier_calibrated']:.3f}"
        )
    weights = calibrator.weights()
    if weights:
        rows.append("decision weights: " + ", ".join(f"{k} {v:+.2f}" for k, v in sorted(weights.items())))
    return "\n".join(rows)

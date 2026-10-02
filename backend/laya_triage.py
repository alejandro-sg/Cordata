"""Laya as the orchestrator's fast "System 1" triage; the LLM stays the
"System 2" that generates text and calls tools.

Laya (github.com/NandhaKishorM/laya) is a non-autoregressive decision engine:
typed answers (choice / noul / score) with calibrated confidence in one forward
pass (tens of ms), no text generation, no tool calls. That makes it a fit for
the *control plane* around the orchestrator — which kind of work a request is,
whether a team is warranted, how heavy a model it deserves — and a poor fit for
anything generative. Modes (``LAYA_TRIAGE_MODE``, default ``route``):

* ``route`` — per user turn, classify the request (in a worker thread, with a
  timeout), then:
  - if the route's confidence clears ``LAYA_TRIAGE_THRESHOLD`` and the route
    is one with a clear tool affinity (files/code, web, memory, team), add ONE
    short advisory hint to the orchestrator's system prompt *for that turn
    only* — restored afterwards, never written to conversation history or the
    persisted session. The hint is assembled from fixed strings keyed by
    Laya's enum answer, never from the user's text.
  - a confident ``team`` route points the hint at local_graph / local_swarm.
    P(needs a team) can too, via ``LAYA_TRIAGE_TEAM_THRESHOLD`` — off by
    default: zero-shot, that primitive rated "Reply with exactly the word:
    pong" at 0.90 in a live run.
  - Laya's complexity estimate (when confident) is the default model *tier*
    for the sub-agents the orchestrator spawns this turn
    (``suggested_tier``, consumed by sub_agent_common).
  - below the gate, or on timeout / error, nothing changes: the LLM decides
    exactly as it did before Laya existed.
  It never blocks a tool or forces one — the shipped checkpoints are near
  chance zero-shot (README: 0.36 vs a 0.46 majority-class baseline) and only
  become reliable after fine-tuning on your own decisions, so the gate and the
  advisory framing are the safety, and the log (below) is how you find out
  where Laya is trustworthy.
* ``shadow`` — log only; no hint, no tiering.
* ``off`` — nothing.

Every mode but ``off`` appends one JSON line per turn to ``LAYA_TRIAGE_LOG``:
Laya's decision beside the route the orchestrator actually took (judged from
the tools it called), the hint applied, and the tiers used.
``_laya_triage_report.py`` turns it into agreement-by-confidence numbers — the
evidence for raising or lowering the gate — and ``_laya_triage_export.py``
into labelled data for fine-tuning. "Actual" means what the orchestrator
*did*, not what was *right*; agreement is the bar for trusting Laya's routing,
not ground truth.

Built on Strands' hook system (docs: "Hooks" / "Hook events"):
``BeforeInvocationEvent`` (input messages), ``BeforeToolCallEvent`` (tool
name/input), ``AfterInvocationEvent`` (``result.stop_reason``), and the public
``Agent.system_prompt`` setter for the per-turn hint. The log holds user
prompts, so it is local, gitignored, and secret-masked (secret_guard.redact).
"""

import json
import logging
import re
import threading
import time
from collections.abc import Callable, Iterable
from concurrent.futures import Future, ThreadPoolExecutor
from concurrent.futures import TimeoutError as FutureTimeout
from pathlib import Path
from typing import Any

from strands.hooks import AfterInvocationEvent, BeforeInvocationEvent, BeforeToolCallEvent, HookProvider, HookRegistry

import secret_guard
import settings
from prompt_security import laya_predict, warm_up_laya

logger = logging.getLogger(__name__)

# Route labels double as Laya choice keys, so they are semantic words — the
# Laya README warns against boolean-word labels (true/false/yes/no) in
# `choice` questions, which checkpoints can follow instead of the options.
ROUTES = ("chat", "files_and_code", "web_research", "memory", "team", "other_tools")

TRIAGE_QUESTIONS: dict[str, dict[str, Any]] = {
    "route": {
        "type": "choice",
        "instructions": "Which kind of work does this request need?",
        "criteria": {
            "chat": "a conversational answer; no tools needed",
            "files_and_code": "reading, writing or running code and files on this machine",
            "web_research": "looking something up on the web",
            "memory": "recalling or storing something the user told us before",
            "team": "a multi-step task that needs several specialists working together",
            "other_tools": "a built-in utility such as a calculator, the clock, model settings or a custom skill",
        },
    },
    # noul criteria must be keyed true/false (Laya README, "Honest limits").
    "needs_team": {
        "type": "noul",
        "instructions": "Does this task need several specialists working together?",
        "criteria": {"true": "yes, several specialists are needed", "false": "no, one agent can do it alone"},
    },
    "complexity": {
        "type": "score",
        "instructions": "How complex is this task?",
        "criteria": ["trivial", "moderate", "hard"],
    },
}

# What the orchestrator actually did, derived from the tools it called.
# Precedence: the most "expensive" thing it reached for wins.
_TOOL_ROUTE = {
    "local_swarm": "team",
    "local_graph": "team",
    "http_request": "web_research",
    "shell": "files_and_code",
    "python_repl": "files_and_code",
    "file_read": "files_and_code",
    "file_write": "files_and_code",
    "editor": "files_and_code",
    "agent_memory": "memory",
}
# `think` is the model reasoning aloud, not a capability it reached for.
_NO_ROUTE_EVIDENCE = frozenset({"think"})
_PRECEDENCE = ("team", "web_research", "files_and_code", "memory", "other_tools")

_STYLE_PREFIX = re.compile(r"^\[Style preference:[^\]]*\]\s*")  # main.py's _style_context_builder
_MAX_LOGGED_TEXT = 500


def actual_route(tool_names: Iterable[str]) -> str:
    routes = {_TOOL_ROUTE.get(name, "other_tools") for name in tool_names if name not in _NO_ROUTE_EVIDENCE}
    for route in _PRECEDENCE:
        if route in routes:
            return route
    return "chat"


def user_text(messages: Any) -> str | None:
    """The user's own words from a BeforeInvocationEvent's input, or None when
    this invocation carries none (a resume payload after an approval
    interrupt — acp_server re-enters stream_async with one — is the same
    user turn continuing, not a new one)."""
    if isinstance(messages, str):
        text = messages
    elif isinstance(messages, list):
        text = ""
        for message in reversed(messages):
            if not isinstance(message, dict) or message.get("role") != "user":
                continue
            parts = [
                block["text"]
                for block in message.get("content") or []
                if isinstance(block, dict) and isinstance(block.get("text"), str)
            ]
            if parts:
                text = "\n".join(parts)
                break
    else:
        return None
    text = _STYLE_PREFIX.sub("", text).strip()
    return text or None


def run_triage(text: str) -> dict[str, Any]:
    """Laya's decisions for one user turn, flattened for the log. Never raises."""
    started = time.perf_counter()
    try:
        result = laya_predict(text, TRIAGE_QUESTIONS)
        if result is None:
            return {"error": "laya unavailable (package missing, weights failed to load, or predict raised)"}
        answers = result["answers"]
        route = answers["route"]
        complexity = answers["complexity"]
        return {
            "route": route["choice"],
            "route_confidence": route["answer_confidence"],
            "route_probs": route["probabilities"],
            "needs_team": answers["needs_team"]["noul"],
            "complexity": complexity["score"],
            "complexity_confidence": complexity["answer_confidence"],
            "latency_ms": round((time.perf_counter() - started) * 1000, 1),
            "model": result.get("model"),
        }
    except Exception as error:  # a classifier problem must never surface in a turn
        return {"error": f"{type(error).__name__}: {error}"}


# --- acting on a decision ---------------------------------------------------

# Fixed text per Laya enum answer — nothing here is derived from user input, so
# a prompt cannot steer the orchestrator through the hint.
_ROUTE_HINT = {
    "files_and_code": "this looks like work on files or code, so the file and code tools are the likely fit",
    "web_research": "this looks like a web lookup, so http_request is the likely fit",
    "memory": "this looks like recalling or storing something the user said, so agent_memory is the likely fit",
    "team": "this looks like a multi-part task",
}
_TEAM_HINT = "a team may help: local_graph for independent parts, local_swarm for sequential ones"

# Complexity is Laya's weakest primitive (README: ordinal `score`, SST-5 0.37),
# so a tier is only suggested when Laya is reasonably sure; otherwise None and
# the sub-agent just gets the provider's own model.
_TIER_MIN_CONFIDENCE = 0.6
_FAST_BELOW = 0.6  # expected level on a 0..2 scale (trivial / moderate / hard)
_STRONG_ABOVE = 1.4


class Decision:
    """Laya's flattened answer for a turn, plus the policy that turns it into
    a hint and a model tier."""

    def __init__(self, data: dict[str, Any]) -> None:
        self.data = data

    @property
    def route(self) -> str:
        return self.data["route"]

    @property
    def route_confidence(self) -> float:
        return float(self.data["route_confidence"])

    @property
    def needs_team(self) -> float:
        return float(self.data["needs_team"])

    def hint(self) -> str | None:
        parts = []
        confident = self.route_confidence >= settings.LAYA_TRIAGE_THRESHOLD
        if confident and self.route in _ROUTE_HINT:
            parts.append(_ROUTE_HINT[self.route])
        team_threshold = settings.LAYA_TRIAGE_TEAM_THRESHOLD
        wants_team = (confident and self.route == "team") or (team_threshold is not None and self.needs_team >= team_threshold)
        if wants_team:
            parts.append(_TEAM_HINT)
        if not parts:
            return None
        return (
            f"Routing hint from a fast local classifier (confidence {self.route_confidence:.2f}; advisory, "
            f"ignore it if it does not fit): {'; '.join(parts)}."
        )

    def tier(self) -> str | None:
        if float(self.data.get("complexity_confidence", 0.0)) < _TIER_MIN_CONFIDENCE:
            return None
        level = float(self.data["complexity"])
        if level < _FAST_BELOW:
            return "fast"
        if level > _STRONG_ABOVE:
            return "strong"
        return "standard"


class _Turn:
    """One user turn: filled in by hooks on the agent thread and by the
    classifier's done-callback on its own thread; written once both are in."""

    def __init__(self, agent: Any, text: str) -> None:
        self.lock = threading.Lock()
        self.agent = agent
        self.agent_id = str(getattr(agent, "agent_id", "agent"))
        self.text = text
        self.started = time.perf_counter()
        self.started_unix = time.time()
        self.tools: list[str] = []
        self.team_agents: list[str] = []
        self.tiers: dict[str, str | None] = {}
        self.stop_reason: str | None = None
        self.incomplete = False
        self.turn_ms: float | None = None
        self.finished = False
        self.laya: dict[str, Any] | None = None
        self.written = False
        # route mode
        self.decision: Decision | None = None
        self.hint: str | None = None
        self.waited_ms: float | None = None
        self.base_prompt: str | None = None  # the system prompt to restore
        self.decision_emitted = False
        self.timed_out = False


# The turn in flight per agent object, so a tool running inside that turn (the
# team tools, building sub-agents) can ask what Laya made of it.
_ACTIVE: dict[int, _Turn] = {}


def suggested_tier(agent: Any) -> str | None:
    """The model tier Laya suggests for sub-agents spawned by ``agent`` this
    turn — None outside route mode, with no confident estimate, or if no turn
    is in flight for that agent."""
    turn = _ACTIVE.get(id(agent))
    if turn is None or turn.decision is None:
        return None
    return turn.decision.tier()


def record_team_tier(agent: Any, node_name: str, tier: str | None) -> None:
    turn = _ACTIVE.get(id(agent))
    if turn is not None:
        with turn.lock:
            turn.tiers[node_name] = tier


class LayaTriageHooks(HookProvider):
    """Classifies each user turn and, in route mode, applies the result.

    One instance may serve many agents (main.py's AG-UI path shares hooks
    across per-thread agents), so turns are keyed by agent identity.
    """

    def __init__(
        self,
        mode: str | None = None,
        log_path: str | Path | None = None,
        predict: Callable[[str], dict[str, Any]] = run_triage,
        notify: Callable[[dict[str, Any]], None] | None = None,
    ) -> None:
        self.mode = mode or settings.LAYA_TRIAGE_MODE
        # Live monitoring: called with a small JSON-able dict when Laya decides
        # ("decision") and when the turn ends ("outcome"). From either the agent's
        # thread or the classifier's worker thread, so it must be thread-safe
        # (acp_server forwards it to the client as an extension notification).
        self._notify = notify
        self._log_path = Path(log_path or settings.LAYA_TRIAGE_LOG)
        self._predict = predict
        # One worker: Laya serializes forward passes anyway (prompt_security's
        # lock), and it keeps classification order = turn order.
        self._executor = ThreadPoolExecutor(max_workers=1, thread_name_prefix="laya-triage")
        self._write_lock = threading.Lock()

    def register_hooks(self, registry: HookRegistry) -> None:
        registry.add_callback(BeforeInvocationEvent, self._on_start)
        registry.add_callback(BeforeToolCallEvent, self._on_tool)
        registry.add_callback(AfterInvocationEvent, self._on_end)

    def _emit(self, payload: dict[str, Any]) -> None:
        """Tell the live monitor; never lets a monitoring problem touch a turn."""
        if self._notify is None:
            return
        try:
            self._notify({**payload, "mode": self.mode})
        except Exception as error:
            logger.warning("laya triage: live notification failed: %s", error)

    def _emit_decision(self, turn: _Turn, laya: dict[str, Any] | None, *, late: bool = False, why: str = "") -> None:
        """Report Laya's decision once. A "no decision yet" notice (the timeout
        case, ``laya`` None) does not use up the one report: the real answer,
        if it ever lands, is still reported — marked late."""
        if laya is None:
            self._emit({"event": "decision", "error": why or "no decision"})
            return
        with turn.lock:
            if turn.decision_emitted:
                return
            turn.decision_emitted = True
        if "route" not in laya:
            self._emit({"event": "decision", "error": laya.get("error") or why or "no decision"})
            return
        decision = Decision(laya)
        self._emit(
            {
                "event": "decision",
                "route": laya["route"],
                "route_confidence": laya["route_confidence"],
                "needs_team": laya["needs_team"],
                "complexity": laya["complexity"],
                "complexity_confidence": laya["complexity_confidence"],
                "latency_ms": laya.get("latency_ms"),
                "gate": settings.LAYA_TRIAGE_THRESHOLD,
                "hint": turn.hint,  # None unless route mode actually applied one
                "tier": decision.tier() if self.mode == "route" else None,
                "waited_ms": turn.waited_ms,
                "late": late,
            }
        )

    # --- hook callbacks (agent thread) -----------------------------------

    def _on_start(self, event: BeforeInvocationEvent) -> None:
        text = user_text(event.messages)
        if text is None:
            return  # a resume of the current turn
        key = id(event.agent)
        previous = _ACTIVE.pop(key, None)
        if previous is not None:
            # The last turn never reached AfterInvocation with a final stop
            # reason (cancelled mid-approval, say): keep what it did.
            self._restore_prompt(previous)
            previous.incomplete = True
            self._finish(previous)

        turn = _Turn(event.agent, secret_guard.redact(text))
        _ACTIVE[key] = turn
        # Classify the user's words, masked (a pasted key must not reach the
        # classifier's logs either).
        future = self._executor.submit(self._predict, turn.text)
        future.add_done_callback(lambda done, turn=turn: self._on_laya_done(turn, done))

        if self.mode == "route":
            self._route(turn, future)

    def _route(self, turn: _Turn, future: Future) -> None:
        """Wait (bounded) for the decision and apply the hint. Anything short
        of a clean, confident decision leaves the turn exactly as it was."""
        started = time.perf_counter()
        try:
            laya = future.result(timeout=settings.LAYA_TRIAGE_TIMEOUT)
        except FutureTimeout:
            laya = None  # it will still be logged when it lands
        except Exception as error:  # a classifier that raised: an error, not a timeout
            laya = {"error": f"{type(error).__name__}: {error}"}
        turn.waited_ms = round((time.perf_counter() - started) * 1000, 1)
        if not laya or "route" not in laya:
            if laya:  # a classifier error: report it now
                self._emit_decision(turn, laya)
                return
            with turn.lock:
                turn.timed_out = True
                landed = turn.laya  # may have arrived between the timeout and here
            self._emit_decision(turn, None, why=f"timed out after {settings.LAYA_TRIAGE_TIMEOUT:g}s — the LLM decides alone")
            if landed is not None:
                self._emit_decision(turn, landed, late=True)
            return
        decision = Decision(laya)
        with turn.lock:
            turn.decision = decision
        hint = decision.hint()
        if hint:
            try:
                base = turn.agent.system_prompt or ""
                turn.agent.system_prompt = f"{base}\n\n{hint}" if base else hint
                turn.base_prompt = base
                turn.hint = hint
            except Exception as error:  # an agent whose prompt can't be set just goes without a hint
                logger.warning("laya triage: could not apply hint: %s", error)
        self._emit_decision(turn, laya)

    def _on_tool(self, event: BeforeToolCallEvent) -> None:
        turn = _ACTIVE.get(id(event.agent))
        if turn is None:
            return
        name = str(event.tool_use.get("name") or "")
        tool_input = event.tool_use.get("input")
        with turn.lock:
            turn.tools.append(name)
            if name in ("local_swarm", "local_graph") and isinstance(tool_input, dict):
                for spec in tool_input.get("agents") or []:
                    if isinstance(spec, dict) and spec.get("name"):
                        turn.team_agents.append(str(spec["name"]))

    def _on_end(self, event: AfterInvocationEvent) -> None:
        key = id(event.agent)
        turn = _ACTIVE.get(key)
        if turn is None:
            return
        stop_reason = getattr(event.result, "stop_reason", None)
        if stop_reason == "interrupt":
            return  # waiting on an approval; the same turn (and hint) continues on resume
        _ACTIVE.pop(key, None)
        self._restore_prompt(turn)
        turn.stop_reason = str(stop_reason) if stop_reason is not None else None
        self._finish(turn)

    @staticmethod
    def _restore_prompt(turn: _Turn) -> None:
        if turn.hint is not None and turn.base_prompt is not None:
            try:
                turn.agent.system_prompt = turn.base_prompt or None
            except Exception as error:
                logger.warning("laya triage: could not restore system prompt: %s", error)

    # --- completion (either thread) --------------------------------------

    def _finish(self, turn: _Turn) -> None:
        with turn.lock:
            turn.turn_ms = round((time.perf_counter() - turn.started) * 1000, 1)
            turn.finished = True
        self._maybe_write(turn)

    def _on_laya_done(self, turn: _Turn, future: Future) -> None:
        try:
            laya = future.result()
        except Exception as error:
            laya = {"error": f"{type(error).__name__}: {error}"}
        with turn.lock:
            turn.laya = laya
            # Shadow mode reports here. Route mode reports from _route (which owns
            # the hint) — except after a timeout, when this is the late answer the
            # user would otherwise never see. Decided under the lock so it can't
            # interleave with _route setting timed_out.
            report = self.mode != "route" or turn.timed_out
        if report:
            self._emit_decision(turn, laya, late=self.mode == "route")
        self._maybe_write(turn)

    def _maybe_write(self, turn: _Turn) -> None:
        with turn.lock:
            if turn.written or not turn.finished or turn.laya is None:
                return
            turn.written = True
            record = {
                "ts": round(turn.started_unix, 3),
                "agent": turn.agent_id,
                "mode": self.mode,
                "text": turn.text[:_MAX_LOGGED_TEXT],
                "text_chars": len(turn.text),
                "actual": {
                    "route": actual_route(turn.tools),
                    "tools": list(turn.tools),
                    "team_agents": list(turn.team_agents),
                    "tiers": dict(turn.tiers),
                    "stop_reason": turn.stop_reason,
                    "incomplete": turn.incomplete,
                    "turn_ms": turn.turn_ms,
                },
                "laya": turn.laya,
                "hint": turn.hint,
                "waited_ms": turn.waited_ms,
            }
        laya_route = (record["laya"] or {}).get("route")
        self._emit(
            {
                "event": "outcome",
                "actual_route": record["actual"]["route"],
                "agreed": (laya_route == record["actual"]["route"]) if laya_route else None,
                "tools": record["actual"]["tools"],
                "tiers": record["actual"]["tiers"],
                "stop_reason": record["actual"]["stop_reason"],
                "turn_ms": record["actual"]["turn_ms"],
            }
        )
        try:
            line = json.dumps(record, ensure_ascii=False)
            with self._write_lock:
                self._log_path.parent.mkdir(parents=True, exist_ok=True)
                with self._log_path.open("a", encoding="utf-8") as handle:
                    handle.write(line + "\n")
        except OSError as error:
            logger.warning("laya triage: could not write %s: %s", self._log_path, error)


# Kept under its old name: the shadow-only class was this one with mode="shadow".
LayaTriageShadowHooks = LayaTriageHooks


def triage_hooks(notify: Callable[[dict[str, Any]], None] | None = None) -> list[HookProvider]:
    """The hooks to add to an orchestrator Agent: ``route`` (default) or
    ``shadow`` mode returns one, ``off`` returns none. Also starts loading
    Laya's weights in the background so the first turn isn't the one to pay."""
    mode = settings.LAYA_TRIAGE_MODE
    if mode in ("route", "shadow"):
        warm_up_laya()
        return [LayaTriageHooks(mode=mode, notify=notify)]
    if mode not in ("", "off"):
        logger.warning("LAYA_TRIAGE_MODE=%r not recognised (use 'route', 'shadow' or 'off'); triage stays off", mode)
    return []

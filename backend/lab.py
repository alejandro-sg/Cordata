"""The Experimental Lab: live state of every agent in a session, and the controls
the Lab Master (the person at the terminal) has over them.

One ``Lab`` per process (an ACP child serves one project). It is a plain,
thread-safe model — pods, the delegation links between them, the threat reading
for the pending tool call, staged HITL interventions, memory reads — plus a
snapshot that is JSON all the way down. Two things consume it:

* the ACP server forwards every change as the ``_cordata/lab`` extension
  notification (``harness_events`` → ``acp_server._forward_event``), which the
  Rust TUI renders as the Lab view;
* the daemon taps the same frames and re-serves the latest snapshot per session
  on ``GET /lab`` and ``WS /lab/ws`` for a remote canvas.

Nothing in the snapshot is a second source of truth: statuses are derived from
Strands hook events (``LabHooks``), model/effort from what ``ollama_cloud``
actually put on the wire, threat from ``laya_risk``, HITL from the same staged
intervention the approval prompt shows.

**Controls**, each enforced where the agent loop already has a seam:

* ``isolate`` (E-Stop) — marks the pod ISOLATED and ``agent.cancel()``s it. From
  then on its hooks refuse every model call (``BeforeModelCallEvent.cancel``) and
  every tool call (``BeforeToolCallEvent.cancel_tool``), so a pod that was
  mid-loop stops at the next boundary and cannot be talked back into acting.
  An in-process pod has no OS process of its own to kill and the vault proxy
  holds credential *placeholders* per process, not per agent — so there is no
  per-pod ticket to revoke; refusing every call is the real stop.
* ``set_dial`` — the per-pod destructiveness threshold. It can only *tighten* the
  approval gate: a turn whose Laya score exceeds the pod's dial is "armed" (asks
  even in auto mode) like a router-armed one. It never loosens the router's own
  destructive gate (``laya_router.RouteDecision.armed``), which stays authoritative.
* ``inject`` — a directive for a pod, merged into its conversation just before
  its next model call (so a running loop takes it between iterations and an idle
  or contained one takes it when it next runs). Attributed as an operator message.
* ``decide`` / the staged HITL list — what the approval prompt is waiting on.

Everything that reaches a snapshot (stream text, tool inputs, system prompts,
memory snippets, guidance) passes through ``secret_guard.redact`` first.
"""

from __future__ import annotations

import collections
import contextvars
import logging
import os
import resource
import threading
import time
import weakref
from collections.abc import Callable
from typing import Any

from strands.hooks import (
    AfterInvocationEvent,
    AfterModelCallEvent,
    AfterToolCallEvent,
    BeforeInvocationEvent,
    BeforeModelCallEvent,
    BeforeToolCallEvent,
    HookProvider,
    HookRegistry,
)

import harness_events

logger = logging.getLogger(__name__)

STATUSES = ("idle", "synthesizing", "executing", "contained", "isolated")
SNAPSHOT_VERSION = 1

#: The router's own destructive gate (laya_router.RouteDecision.armed). The dial
#: can sit anywhere in [DIAL_MIN, DIAL_DEFAULT]; above the default it would claim a
#: looser gate than the router enforces, so it is not offered.
DIAL_DEFAULT = 0.8
DIAL_MIN = 0.3

_MAX_TRACE = 24
_MAX_STREAM = 1200
_MAX_PROMPT = 4000
_MAX_MEMORY = 10
_MIN_EMIT_INTERVAL = 0.08

#: The pod whose tool call is executing, for code a tool runs (memory reads).
current_pod: contextvars.ContextVar[str | None] = contextvars.ContextVar("lab_current_pod", default=None)


def _redact(text: Any, limit: int | None = None) -> str:
    value = "" if text is None else str(text)
    try:
        import secret_guard

        value = secret_guard.redact(value)
    except Exception:  # redaction must never be the reason a snapshot fails
        logger.debug("lab: redact failed", exc_info=True)
    return value if limit is None or len(value) <= limit else value[:limit] + "…"


def _short(value: Any, limit: int = 160) -> str:
    text = value if isinstance(value, str) else repr(value)
    text = " ".join(text.split())
    return _redact(text, limit)


class Pod:
    """One agent's containment pod."""

    def __init__(self, pod_id: str, name: str, role: str, parent: str | None = None) -> None:
        self.id = pod_id
        self.name = name
        self.role = role
        self.parent = parent
        self.model: str | None = None
        self.effort: str | None = None
        self.status = "idle"
        self.since = time.time()
        self.isolated = False
        self.dial: float | None = None  # None = the default gate
        self.system_prompt = ""
        self.messages = 0
        self.trace: collections.deque[dict[str, Any]] = collections.deque(maxlen=_MAX_TRACE)
        self.stream = ""
        self.memory: collections.deque[dict[str, Any]] = collections.deque(maxlen=_MAX_MEMORY)
        self.guidance: list[str] = []
        self.busy_ms = 0.0
        self.calls = 0
        #: Laya Guard, per agent: the last call's destructiveness (None until one is scored), the
        #: highest this session, how many calls ran in auto mode and how many it stopped to ask about.
        self.risk_last: float | None = None
        self.risk_peak = 0.0
        self.auto_approved = 0
        self.asked = 0
        self.pending_tool: str | None = None
        self._agent: weakref.ReferenceType | None = None
        self._tool_started: dict[str, float] = {}
        self._model_started: float | None = None
        #: Callables that stop whatever this pod is running. An agent pod is stopped by
        #: ``agent.cancel()``; an *external* pod (a CLI child process) has no agent, so
        #: whoever started the process registers how to kill it here.
        self.killers: list[Callable[[], None]] = []

    @property
    def agent(self) -> Any:
        return self._agent() if self._agent else None

    def to_dict(self, *, detail: bool) -> dict[str, Any]:
        agent = self.agent
        row: dict[str, Any] = {
            "id": self.id,
            "name": self.name,
            "role": self.role,
            "effort_adjustable": bool(agent is not None and _reasoning_controls(getattr(agent, "model", None))),
            "parent": self.parent,
            "model": self.model,
            "effort": self.effort,
            "status": self.status,
            "since": self.since,
            "isolated": self.isolated,
            "dial": self.dial if self.dial is not None else DIAL_DEFAULT,
            "dial_default": DIAL_DEFAULT,
            "calls": self.calls,
            "busy_ms": round(self.busy_ms),
            "pending_tool": self.pending_tool,
            "guidance_pending": len(self.guidance),
            "risk_last": self.risk_last,
            "risk_peak": self.risk_peak,
            "auto_approved": self.auto_approved,
            "asked": self.asked,
            "messages": self.messages,
            "trace": list(self.trace)[-6:],
            "memory": list(self.memory)[-4:],
            "stream": self.stream[-160:],
        }
        if detail:
            row.update(
                {
                    "system_prompt": _redact(self.system_prompt, _MAX_PROMPT),
                    "trace": list(self.trace),
                    "memory": list(self.memory),
                    "stream": self.stream,
                    "guidance": [_redact(text, 400) for text in self.guidance],
                }
            )
        return row


class Lab:
    def __init__(self) -> None:
        self._lock = threading.RLock()
        self.pods: dict[str, Pod] = {}
        self.links: dict[tuple[str, str], dict[str, Any]] = {}
        self.threat: dict[str, Any] | None = None
        self.hitl: dict[str, dict[str, Any]] = {}
        self.session_id: str | None = None
        self.focus: str | None = None
        self.mcp_provider: Callable[[], list[str]] = lambda: []
        #: The session's permission mode (plan | build | auto), read at every snapshot so a GUI shows
        #: the mode actually in force — including one a different front end set.
        self.mode_provider: Callable[[], str] = lambda: ""
        #: What the team is working on: the person's latest request, redacted and shortened.
        self.mission = ""
        self.mission_ts = 0.0
        #: (agent, tool_use) -> the reading the gauge should show for the call about to run.
        self.risk_provider: Callable[..., dict[str, Any] | None] = lambda agent, tool_use=None: None
        self.default_dial = DIAL_DEFAULT
        self._last_emit = 0.0
        self._timer: threading.Timer | None = None
        self._seq = 0
        self._cpu_mark = (time.monotonic(), time.process_time())
        self._cpu = 0.0
        self._last_tool_pod: str | None = None

    def connect_models(self) -> None:
        """Pods report the model and reasoning effort each request actually carries
        (``ollama_cloud``'s request listener), attributed by the pod whose model call
        is in flight. Idempotent."""
        if getattr(self, "_unsubscribe_models", None) is not None:
            return
        import ollama_cloud
        import omlx_provider

        def on_request(info: dict[str, Any]) -> None:
            pod = self.pods.get(current_pod.get() or "")
            if pod is not None:
                self.note_model(pod, info.get("alias"), info.get("effort"))

        # Both providers report what each request actually carries: the Ollama Cloud models
        # (with their reasoning effort) and the local Prism models on oMLX (no effort knob).
        unsubscribe = [ollama_cloud.subscribe_requests(on_request), omlx_provider.subscribe_requests(on_request)]
        self._unsubscribe_models = lambda: [off() for off in unsubscribe]

    # ------------------------------------------------------------ registry

    def pod(self, name: str, *, role: str | None = None, parent: str | None = None) -> Pod:
        pod_id = name
        with self._lock:
            existing = self.pods.get(pod_id)
            if existing is not None:
                if parent and not existing.parent:
                    existing.parent = parent
                return existing
            pod = Pod(pod_id, name, role or name, parent)
            self.pods[pod_id] = pod
        self.emit(force=True)
        return pod

    def reset(self) -> None:
        with self._lock:
            self.pods.clear()
            self.links.clear()
            self.threat = None
            self.hitl.clear()
            self._last_tool_pod = None
            self.mission, self.mission_ts = "", 0.0
        self.emit(force=True)

    def set_mission(self, text: str) -> None:
        """The task now under way, as the person phrased it (secrets masked, one short paragraph).
        Shown at the top of the Lab so the first thing it says is *what the team is doing*."""
        flat = " ".join((text or "").split())
        with self._lock:
            self.mission = _redact(flat, 300) if flat else ""
            self.mission_ts = time.time() if flat else 0.0
        self.emit(force=True)

    def pod_of(self, agent: Any) -> Pod | None:
        with self._lock:
            for pod in self.pods.values():
                if pod.agent is agent:
                    return pod
        return None

    def attach(self, agent: Any, name: str, *, role: str | None = None, parent: str | None = None, hooks: bool = True) -> "LabHooks":
        """Register ``agent`` as a pod and return the hook provider to give it."""
        pod = self.pod(name, role=role, parent=parent)
        pod._agent = weakref.ref(agent) if agent is not None else None
        if agent is not None:
            pod.system_prompt = getattr(agent, "system_prompt", "") or ""
        return LabHooks(self, pod)

    # --------------------------------------------------------------- state

    def set_status(self, pod: Pod, status: str) -> None:
        if pod.isolated and status != "isolated":
            status = "isolated"
        with self._lock:
            if pod.status == status:
                return
            pod.status = status
            pod.since = time.time()
        self.emit(force=True)

    def note_model(self, pod: Pod, model: str | None, effort: str | None) -> None:
        with self._lock:
            changed = (pod.model, pod.effort) != (model, effort)
            pod.model, pod.effort = model, effort
        if changed:
            self.emit()

    def link(self, source: str, target: str, kind: str = "delegation", *, active: bool = True, label: str = "") -> None:
        with self._lock:
            key = (source, target)
            row = self.links.setdefault(key, {"from": source, "to": target, "kind": kind, "count": 0})
            row.update(active=active, kind=kind, label=label or row.get("label", ""), ts=time.time())
            if active:
                row["count"] += 1
        self.emit(force=True)

    def stream_text(self, pod_name: str, text: str) -> None:
        pod = self.pods.get(pod_name)
        if pod is None or not text:
            return
        with self._lock:
            pod.stream = (pod.stream + _redact(text))[-_MAX_STREAM:]
        self.emit()

    def memory_hit(self, source: str, query: str, hits: list[dict[str, Any]], *, pod_name: str | None = None) -> None:
        """A read of a memory store (``lancedb`` | ``kg`` | ``qdrant``), attributed to
        the pod whose tool is running. Never raises."""
        try:
            target = pod_name or current_pod.get() or self._last_tool_pod
            pod = self.pods.get(target or "")
            if pod is None:
                return
            rows = []
            for hit in hits[:6]:
                rows.append(
                    {
                        "id": _short(hit.get("id") or hit.get("label") or hit.get("kind") or "", 60),
                        "label": _short(hit.get("label") or hit.get("text") or hit.get("summary") or hit.get("id") or "", 80),
                        "score": hit.get("score"),
                    }
                )
            with self._lock:
                pod.memory.append({"source": source, "query": _short(query, 80), "count": len(hits), "hits": rows, "ts": time.time()})
            self.emit(force=True)
        except Exception:
            logger.debug("lab: memory_hit failed", exc_info=True)

    def set_threat(self, pod_name: str, tool: str, risk: dict[str, Any] | None, *, pending: bool = True) -> None:
        with self._lock:
            if risk is None:
                self.threat = None if not pending else {"pod": pod_name, "tool": tool, "score": None, "confidence": None, "band": None, "pending": True}
            else:
                self.threat = {
                    "pod": pod_name,
                    "tool": tool,
                    "score": risk.get("score"),
                    "confidence": risk.get("confidence"),
                    "band": risk.get("band"),
                    "label": risk.get("label"),
                    "pending": pending,
                    "ts": time.time(),
                }
        self.emit(force=True)

    def note_call(self, pod_name: str, tool: str, risk: dict[str, Any] | None, decision: str, gauge: dict[str, Any] | None = None) -> None:
        """Laya Guard scored one call by ``pod_name``: remember it on the pod and show it on the gauge.
        ``decision`` is ``auto`` (ran without asking, in auto mode), ``asked`` (held for a person) or
        ``allowed`` (ran under a permission rule or a mode that does not ask). ``risk`` is the call's
        own reading (it feeds the pod's counters); ``gauge`` is what the gauge should show — the higher
        of the call's and the turn's, because that is what made it ask."""
        if risk is None:
            return
        score = risk.get("score")
        with self._lock:
            pod = self.pods.get(pod_name)
            if pod is not None and isinstance(score, (int, float)):
                pod.risk_last = float(score)
                pod.risk_peak = max(pod.risk_peak, float(score))
                if decision == "auto":
                    pod.auto_approved += 1
                elif decision == "asked":
                    pod.asked += 1
        self.set_threat(pod_name, tool, gauge or risk, pending=decision == "asked")

    # ---------------------------------------------------------------- HITL

    def stage(self, intervention_id: str, pod_name: str, tool: str, tool_input: Any, risk: dict[str, Any] | None, note: str | None = None) -> None:
        with self._lock:
            self.hitl[intervention_id] = {
                "id": intervention_id,
                "pod": pod_name,
                "tool": tool,
                "input": _redact(tool_input if isinstance(tool_input, str) else _jsonish(tool_input), 4000),
                "risk": risk,
                "note": _redact(note, 300) if note else None,
                "status": "pending",
                "ts": time.time(),
            }
            pod = self.pods.get(pod_name)
        if pod is not None:
            self.set_status(pod, "contained")
            # Whoever delegated to it is not idle: its turn is in flight, waiting on
            # the same human. (The interrupt unwinding through its hooks has just
            # marked it idle; that was true of the invocation, not of the turn.)
            for ancestor in self.ancestors(pod_name):
                parent = self.pods.get(ancestor)
                if parent is not None and not parent.isolated:
                    self.set_status(parent, "executing")
        self.emit(force=True)

    def ancestors(self, pod_name: str) -> list[str]:
        """The parent chain of a pod, nearest first."""
        with self._lock:
            chain: list[str] = []
            current = self.pods.get(pod_name)
            while current is not None and current.parent and current.parent not in chain:
                chain.append(current.parent)
                current = self.pods.get(current.parent)
            return chain

    def resolve(self, intervention_id: str, outcome: str, decided_by: str = "user") -> None:
        with self._lock:
            entry = self.hitl.get(intervention_id)
            if entry is None:
                return
            entry.update(status=outcome, decided_by=decided_by, resolved_ts=time.time())
            pod = self.pods.get(entry["pod"])
            if len(self.hitl) > 20:  # keep the history short; settled rows age out first
                for key in [k for k, v in self.hitl.items() if v["status"] != "pending"][:-10]:
                    self.hitl.pop(key, None)
        if pod is not None and not pod.isolated:
            self.set_status(pod, "executing" if outcome == "approved" else "synthesizing")
        self.emit(force=True)

    # ------------------------------------------------------------ controls

    def isolate(self, pod_name: str, *, reason: str = "lab master") -> dict[str, Any]:
        """E-Stop. Returns what was done, for the control reply."""
        pod = self.pods.get(pod_name)
        if pod is None:
            raise KeyError(f"no such pod: {pod_name}")
        agent = pod.agent
        with self._lock:
            in_flight = pod.status != "idle"
            pod.isolated = True
            pod.guidance.clear()
        cancelled = False
        # Only an agent that is doing something has anything to cancel. Strands
        # clears the cancel signal when an invocation *completes*, so signalling an
        # idle one would leave it set and silently cancel its next turn after a
        # release — the isolation flag alone already refuses everything it could do.
        if agent is not None and in_flight:
            try:
                agent.cancel()
                cancelled = True
            except Exception:
                logger.debug("lab: agent.cancel failed", exc_info=True)
        for kill in list(pod.killers):
            try:
                kill()
                cancelled = True
            except Exception:
                logger.debug("lab: killer for %s failed", pod_name, exc_info=True)
        pod.trace.append({"tool": "ISOLATE", "status": "failed", "input": _short(reason), "ts": time.time(), "ms": 0})
        self.set_status(pod, "isolated")
        self.emit(force=True)
        return {"pod": pod_name, "isolated": True, "cancelled": cancelled}

    def descendants(self, pod_name: str) -> list[str]:
        """Pods whose parent chain leads to ``pod_name`` (not including it)."""
        with self._lock:
            found: list[str] = []
            frontier = [pod_name]
            while frontier:
                parent = frontier.pop()
                for pod in self.pods.values():
                    if pod.parent == parent and pod.id not in found and pod.id != pod_name:
                        found.append(pod.id)
                        frontier.append(pod.id)
            return found

    def release(self, pod_name: str) -> dict[str, Any]:
        pod = self.pods.get(pod_name)
        if pod is None:
            raise KeyError(f"no such pod: {pod_name}")
        with self._lock:
            pod.isolated = False
        agent = pod.agent
        if agent is not None:
            try:
                agent.cancel_signal.clear()  # a stale E-Stop must not cancel the next turn
            except Exception:
                logger.debug("lab: could not clear the cancel signal", exc_info=True)
        self.set_status(pod, "idle")
        return {"pod": pod_name, "isolated": False}

    def set_dial(self, pod_name: str, value: float) -> dict[str, Any]:
        pod = self.pods.get(pod_name)
        if pod is None:
            raise KeyError(f"no such pod: {pod_name}")
        value = round(min(DIAL_DEFAULT, max(DIAL_MIN, float(value))), 2)
        with self._lock:
            pod.dial = None if value >= DIAL_DEFAULT else value
        self.emit(force=True)
        return {"pod": pod_name, "dial": value}

    def set_effort(self, pod_name: str, level: str) -> dict[str, Any]:
        """Change a pod's ``reasoning_effort`` for its *next* model call — between agent-loop
        iterations included. The level lands on the ``ReasoningControl`` every Ollama Cloud
        model behind the pod shares (a router's candidates share one), and on the role table so
        an agent built later for the same role follows. A pod whose model has no such knob
        (a local or Anthropic model) refuses rather than pretend."""
        import ollama_cloud

        pod = self.pods.get(pod_name)
        if pod is None:
            raise KeyError(f"no such pod: {pod_name}")
        value = ollama_cloud.normalize_effort(level)
        if value is None:
            raise ValueError("reasoning_effort needs a level: low, medium, high or max")
        agent = pod.agent
        controls = _reasoning_controls(getattr(agent, "model", None)) if agent is not None else []
        if not controls:
            raise ValueError(f"{pod_name} has no reasoning_effort knob: its model is not an Ollama Cloud model")
        for control in controls:
            control.set(value)
        if ollama_cloud.ROLES.get(pod.role) is not None:
            ollama_cloud.ROLES.set(pod.role, effort=value)
        with self._lock:
            pod.effort = value
        self.emit(force=True)
        return {"pod": pod_name, "effort": value}

    def dial_armed(self, agent: Any, score: float | None) -> bool:
        """Does this pod's dial arm the approval gate for a turn scored ``score``?
        Only a dial *below* the default can — it tightens, never loosens."""
        pod = self.pod_of(agent)
        if pod is None or pod.dial is None or score is None:
            return False
        return score > pod.dial

    def inject(self, pod_name: str, text: str) -> dict[str, Any]:
        pod = self.pods.get(pod_name)
        if pod is None:
            raise KeyError(f"no such pod: {pod_name}")
        text = (text or "").strip()
        if not text:
            raise ValueError("guidance is empty")
        with self._lock:
            if pod.isolated:
                raise PermissionError(f"{pod_name} is isolated; release it first")
            pod.guidance.append(text[:2000])
        self.emit(force=True)
        return {"pod": pod_name, "queued": len(pod.guidance)}

    def take_guidance(self, pod: Pod) -> list[str]:
        with self._lock:
            taken, pod.guidance = pod.guidance, []
        if taken:
            pod.trace.append({"tool": "GUIDANCE", "status": "completed", "input": _short(" / ".join(taken)), "ts": time.time(), "ms": 0})
            self.emit(force=True)
        return taken

    def select(self, pod_name: str | None) -> None:
        with self._lock:
            self.focus = pod_name if pod_name in self.pods else None
        self.emit(force=True)

    # ------------------------------------------------------- external pods

    def external_start(self, name: str, *, role: str, parent: str = "orchestrator", model: str | None = None, detail: str = "") -> Pod:
        """A pod for work that is not a Strands agent — the Claude Code CLI child
        process. It has no model calls to hook, so its owner reports what it does
        (``external_trace``) and how to stop it (``pod.killers``). Refuses to start on a
        pod the lab master has isolated: E-Stop means the orchestrator cannot respawn it."""
        pod = self.pod(name, role=role, parent=parent)
        if pod.isolated:
            raise PermissionError(f"{name} is isolated by the lab master; release it first")
        with self._lock:
            pod.model = model or pod.model
            pod.stream = ""
            pod.calls += 1
            pod._model_started = time.monotonic()
            pod.pending_tool = detail or None
        self.link(parent, name, "delegation", label=_short(detail, 40))
        self.set_status(pod, "executing")
        return pod

    def external_trace(self, pod_name: str, tool: str, detail: str, *, status: str = "completed", tool_id: str | None = None) -> None:
        """One tool call the external process made, for the pod's trace. A ``running``
        entry with an id is closed by a later call with the same id."""
        pod = self.pods.get(pod_name)
        if pod is None:
            return
        with self._lock:
            if tool_id:
                for row in reversed(pod.trace):
                    if row.get("id") == tool_id:
                        row.update(status=status, ms=round((time.time() - row.get("ts", time.time())) * 1000))
                        break
                else:
                    pod.trace.append({"id": tool_id, "tool": tool, "status": status, "input": _short(detail), "ts": time.time(), "ms": 0})
            else:
                pod.trace.append({"tool": tool, "status": status, "input": _short(detail), "ts": time.time(), "ms": 0})
            pod.pending_tool = tool if status == "running" else pod.pending_tool
        self.emit()

    def external_done(self, pod_name: str, *, ok: bool, summary: str = "") -> None:
        pod = self.pods.get(pod_name)
        if pod is None:
            return
        with self._lock:
            started, pod._model_started = pod._model_started, None
            if started is not None:
                pod.busy_ms += (time.monotonic() - started) * 1000
            pod.pending_tool = None
            pod.killers.clear()
            if summary:
                pod.stream = _redact(summary, _MAX_STREAM)
        if pod.parent:
            self.link(pod.parent, pod_name, "delegation", active=False)
        self.set_status(pod, "isolated" if pod.isolated else "idle")

    # ------------------------------------------------------------ snapshot

    def _resources(self) -> dict[str, Any]:
        now, cpu = time.monotonic(), time.process_time()
        mark_wall, mark_cpu = self._cpu_mark
        if now - mark_wall >= 0.5:
            self._cpu = max(0.0, (cpu - mark_cpu) / (now - mark_wall) * 100)
            self._cpu_mark = (now, cpu)
        rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        rss_mb = rss / (1024 * 1024) if os.uname().sysname == "Darwin" else rss / 1024
        try:
            mcp = list(self.mcp_provider())
        except Exception:
            mcp = []
        return {"cpu_percent": round(self._cpu, 1), "rss_mb": round(rss_mb, 1), "scope": "process", "mcp": mcp}

    def _mode(self) -> str:
        try:
            return str(self.mode_provider() or "")
        except Exception:
            return ""

    def snapshot(self) -> dict[str, Any]:
        with self._lock:
            focus = self.focus
            return {
                "event": "state",
                "version": SNAPSHOT_VERSION,
                "seq": self._seq,
                "ts": time.time(),
                "sessionId": self.session_id,
                "focus": focus,
                "pods": [pod.to_dict(detail=pod.id == focus) for pod in self.pods.values()],
                "links": [dict(row) for row in self.links.values()],
                "threat": dict(self.threat) if self.threat else None,
                "hitl": [dict(row) for row in self.hitl.values()],
                "resources": self._resources(),
                "mode": self._mode(),
                "mission": self.mission,
                "mission_ts": self.mission_ts,
            }

    def emit(self, *, force: bool = False) -> None:
        """Publish a snapshot. Unforced calls (stream text, counters) are rate-limited,
        but a limited call is *deferred*, not dropped — the last change before a quiet
        spell must still reach the client, or a pod would sit on a stale status."""
        now = time.monotonic()
        with self._lock:
            if not force and now - self._last_emit < _MIN_EMIT_INTERVAL:
                if self._timer is None:
                    self._timer = threading.Timer(_MIN_EMIT_INTERVAL, self._flush)
                    self._timer.daemon = True
                    self._timer.start()
                return
            self._last_emit = now
            self._seq += 1
            timer, self._timer = self._timer, None
        if timer is not None:
            timer.cancel()
        try:
            harness_events.emit("lab", self.snapshot())
        except Exception:
            logger.debug("lab: emit failed", exc_info=True)

    def _flush(self) -> None:
        with self._lock:
            self._timer = None
        self.emit(force=True)


def _reasoning_controls(model: Any) -> list[Any]:
    """Every ``ReasoningControl`` behind a model: one per Ollama Cloud model, found through
    router candidates (nested routers included). Empty for any other provider."""
    found: list[Any] = []
    seen: set[int] = set()
    stack = [model]
    while stack:
        current = stack.pop()
        if current is None or id(current) in seen:
            continue
        seen.add(id(current))
        control = getattr(current, "reasoning", None)
        if control is not None and hasattr(control, "set") and not any(control is existing for existing in found):
            found.append(control)
        for candidate in getattr(current, "candidates", None) or ():
            stack.append(getattr(candidate, "model", None))
    return found


def _jsonish(value: Any) -> str:
    import json

    try:
        return json.dumps(value, indent=2, default=str, ensure_ascii=False)
    except (TypeError, ValueError):
        return repr(value)


# ----------------------------------------------------------------- the hooks


class LabHooks(HookProvider):
    """Keeps one pod's status/trace/model current from Strands lifecycle events and
    enforces its controls (isolation, guidance injection) at the same seams."""

    def __init__(self, lab: Lab, pod: Pod) -> None:
        self.lab, self.pod = lab, pod

    def register_hooks(self, registry: HookRegistry, **kwargs: Any) -> None:
        registry.add_callback(BeforeInvocationEvent, self._on_invocation)
        registry.add_callback(BeforeModelCallEvent, self._before_model)
        registry.add_callback(AfterModelCallEvent, self._after_model)
        registry.add_callback(BeforeToolCallEvent, self._before_tool)
        registry.add_callback(AfterToolCallEvent, self._after_tool)
        registry.add_callback(AfterInvocationEvent, self._on_done)

    def _bind(self, event: Any) -> None:
        agent = getattr(event, "agent", None)
        if agent is not None and self.pod.agent is not agent:
            self.pod._agent = weakref.ref(agent)

    def _on_invocation(self, event: BeforeInvocationEvent) -> None:
        self._bind(event)
        if self.pod.isolated:
            event.cancel = f"{self.pod.name} is isolated by the lab master"
            return
        self.pod.stream = ""
        self.lab.set_status(self.pod, "synthesizing")

    def _before_model(self, event: BeforeModelCallEvent) -> None:
        self._bind(event)
        pod = self.pod
        if pod.isolated:
            event.cancel = f"{pod.name} is isolated by the lab master"
            return
        guidance = self.lab.take_guidance(pod)
        if guidance:
            _merge_guidance(event.agent, guidance)
        agent = event.agent
        pod.messages = len(getattr(agent, "messages", []) or [])
        current_pod.set(pod.name)
        if pod.model is None:  # non-Ollama providers: the configured model id
            try:
                pod.model = agent.model.get_config().get("model_id")
            except Exception:
                pass
        pod._model_started = time.monotonic()
        self.lab.set_status(pod, "synthesizing")

    def _after_model(self, event: AfterModelCallEvent) -> None:
        pod = self.pod
        started, pod._model_started = pod._model_started, None
        if started is not None:
            pod.busy_ms += (time.monotonic() - started) * 1000
        pod.calls += 1
        response = event.stop_response
        if response is not None:
            parts = [block.get("text", "") for block in (response.message.get("content") or []) if isinstance(block, dict)]
            text = "".join(parts).strip()
            if text:
                pod.stream = _redact(text, _MAX_STREAM)
        self.lab.emit()

    def _before_tool(self, event: BeforeToolCallEvent) -> None:
        self._bind(event)
        pod = self.pod
        name = event.tool_use.get("name") or "tool"
        if pod.isolated:
            event.cancel_tool = f"{name} was not run: {pod.name} is isolated by the lab master."
            return
        tool_id = event.tool_use.get("toolUseId") or name
        pod._tool_started[tool_id] = time.monotonic()
        pod.pending_tool = name
        pod.trace.append({"id": tool_id, "tool": name, "status": "running", "input": _short(event.tool_use.get("input")), "ts": time.time(), "ms": 0})
        self.lab._last_tool_pod = pod.name
        current_pod.set(pod.name)
        # Delegation: a specialist run as a tool is a vector from its caller.
        if name in self.lab.pods and name != pod.name:
            self.lab.link(pod.name, name, "delegation", label=_short(event.tool_use.get("input"), 40))
        risk = self.lab.risk_provider(event.agent, event.tool_use)
        if risk is not None:
            self.lab.set_threat(pod.name, name, risk, pending=True)
        self.lab.set_status(pod, "executing")

    def _after_tool(self, event: AfterToolCallEvent) -> None:
        pod = self.pod
        tool_id = event.tool_use.get("toolUseId") or event.tool_use.get("name") or "tool"
        started = pod._tool_started.pop(tool_id, None)
        elapsed = (time.monotonic() - started) * 1000 if started is not None else 0.0
        pod.busy_ms += elapsed
        status = "failed" if (event.result or {}).get("status") == "error" or event.exception is not None else "completed"
        for row in reversed(pod.trace):
            if row.get("id") == tool_id:
                row.update(status=status, ms=round(elapsed))
                break
        name = event.tool_use.get("name")
        if name in self.lab.pods and name != pod.name:
            self.lab.link(pod.name, name, "delegation", active=False)
        pod.pending_tool = None
        with self.lab._lock:
            if self.lab.threat and self.lab.threat.get("pod") == pod.name:
                self.lab.threat["pending"] = False
        self.lab.set_status(pod, "synthesizing")

    def _on_done(self, event: AfterInvocationEvent) -> None:
        self.lab.set_status(self.pod, "isolated" if self.pod.isolated else "idle")


def _merge_guidance(agent: Any, guidance: list[str]) -> None:
    """Put operator directives into the conversation the model is about to read.

    Merged into the trailing user message when there is one (a tool result, or the
    prompt itself) — a second consecutive user turn is legal for the providers here
    but breaks the tool_use/tool_result pairing some of them require. Attributed, so
    the model (and the transcript) can tell an operator directive from tool output.
    """
    block = {"text": "[operator directive — from the person running this session, outranks earlier plans]\n" + "\n".join(guidance)}
    messages = getattr(agent, "messages", None)
    if messages is None:
        return
    if messages and messages[-1].get("role") == "user":
        messages[-1].setdefault("content", []).append(block)
    else:
        messages.append({"role": "user", "content": [block]})


# ------------------------------------------------------------- process-wide

_LAB: Lab | None = None
_LAB_LOCK = threading.Lock()


def get() -> Lab:
    global _LAB
    with _LAB_LOCK:
        if _LAB is None:
            _LAB = Lab()
        return _LAB


def memory_hit(source: str, query: str, hits: list[dict[str, Any]]) -> None:
    """Module-level shortcut for the memory stores: a no-op until a Lab exists."""
    if _LAB is not None:
        _LAB.memory_hit(source, query, hits)

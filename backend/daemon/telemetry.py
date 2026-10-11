"""Daemon state → protobuf, and the latest-wins hub that fans it out.

Two jobs, both about not letting a slow reader hurt the daemon:

**Mapping.** The agent child emits the Lab as one JSON snapshot per change (``lab.py``); the
daemon holds approvals (``supervisor.PendingPermission``), diffs, the oMLX/Ollama memory picture
and its own counters. ``lab_to_proto`` & co. turn those into the typed messages of
``proto/harness.proto``. Nothing is invented here: a field the child did not send is unset, not
zero (``Threat.score`` is ``optional`` so the gauge can say "—").

**The hub.** ``StreamTelemetry`` is *state*, so a reader that falls behind is served the newest
state, not a growing queue of old ones. Each subscriber holds, per session, only the latest Lab
snapshot, plus the latest copy of each daemon-wide section; a change overwrites and sets an
event. The sender loop wakes, takes everything, and writes frames — ``dropped`` records how many
overwrites happened in between, so the client can *see* that it conflated. There is no queue to
overflow and no producer that can block: the producer side is a dict assignment.
Only ``memory_event`` notices (a load, an unload) are one-off, kept in a small bounded deque.
"""

from __future__ import annotations

import asyncio
import logging
import time
from collections import deque
from typing import Any

import memory_budget
import omlx_manager
import secret_patterns

from . import harness_pb2 as pb
from .supervisor import PendingPermission, Supervisor

logger = logging.getLogger(__name__)

#: The child's own thresholds, mirrored rather than imported: both `lab` (the dial range) and
#: `laya_risk` (the score bands) reach the agent stack, and the daemon is stdlib-only on purpose.
#: `_grpc_bridge_smoke_test` asserts these equal the originals so the copy cannot drift — a drifted
#: dial clamp would be a gate a client could *loosen*, the one direction that must not exist.
DIAL_MIN, DIAL_DEFAULT = 0.3, 0.8  # lab.DIAL_MIN, lab.DIAL_DEFAULT
RISK_SAFE, RISK_GATE = 0.2, 0.8  # laya_risk.DEFAULT_SAFE, laya_risk.DEFAULT_DESTROY

SCHEMA_VERSION = "1"
_STATUS = {
    "idle": pb.POD_STATUS_IDLE,
    "synthesizing": pb.POD_STATUS_SYNTHESIZING,
    "executing": pb.POD_STATUS_EXECUTING,
    "contained": pb.POD_STATUS_CONTAINED,
    "isolated": pb.POD_STATUS_ISOLATED,
}


def _opt_float(value: Any) -> float | None:
    return float(value) if isinstance(value, (int, float)) and not isinstance(value, bool) else None


def pod_to_proto(row: dict[str, Any]) -> pb.Pod:
    pod = pb.Pod(
        id=str(row.get("id") or ""),
        name=str(row.get("name") or ""),
        role=str(row.get("role") or ""),
        parent=str(row.get("parent") or ""),
        model=str(row.get("model") or ""),
        effort=str(row.get("effort") or ""),
        status=_STATUS.get(str(row.get("status")), pb.POD_STATUS_UNSPECIFIED),
        isolated=bool(row.get("isolated")),
        dial=float(row.get("dial") or 0.0),
        dial_default=float(row.get("dial_default") or 0.0),
        calls=int(row.get("calls") or 0),
        busy_ms=int(row.get("busy_ms") or 0),
        pending_tool=str(row.get("pending_tool") or ""),
        guidance_pending=int(row.get("guidance_pending") or 0),
        stream_tail=str(row.get("stream") or "")[-400:],
        effort_adjustable=bool(row.get("effort_adjustable")),
        since_ms=int(float(row.get("since") or 0) * 1000),
        risk_peak=float(row.get("risk_peak") or 0.0),
        auto_approved=int(row.get("auto_approved") or 0),
        asked=int(row.get("asked") or 0),
    )
    if (risk_last := _opt_float(row.get("risk_last"))) is not None:
        pod.risk_last = risk_last
    for entry in (row.get("trace") or [])[-12:]:
        pod.trace.add(
            tool=str(entry.get("tool") or ""),
            status=str(entry.get("status") or ""),
            input=str(entry.get("input") or "")[:200],
            ms=int(entry.get("ms") or 0),
            ts_ms=int(float(entry.get("ts") or 0) * 1000),
        )
    return pod


def memory_hit_to_proto(pod_id: str, row: dict[str, Any]) -> pb.MemoryHit:
    """One store read an agent made. The child already masked the query and kept the first few hits;
    this only bounds what crosses the wire. A store that returned no distance leaves `score` unset
    rather than 0.0 — the GUI draws "—" for it."""
    hit = pb.MemoryHit(
        pod=pod_id,
        source=str(row.get("source") or ""),
        query=str(row.get("query") or "")[:120],
        count=int(row.get("count") or 0),
        ts_ms=int(float(row.get("ts") or 0) * 1000),
    )
    for entry in (row.get("hits") or [])[:6]:
        item = hit.hits.add(id=str(entry.get("id") or "")[:60], label=str(entry.get("label") or "")[:80])
        if (score := _opt_float(entry.get("score"))) is not None:
            item.score = score
    return hit


def lab_to_proto(snapshot: dict[str, Any]) -> pb.LabState:
    state = pb.LabState(lab_seq=int(snapshot.get("seq") or 0), focus=str(snapshot.get("focus") or ""), default_dial=0.8, mode=str(snapshot.get("mode") or ""), mission=str(snapshot.get("mission") or ""), mission_ms=int(float(snapshot.get("mission_ts") or 0) * 1000))
    for row in snapshot.get("pods") or []:
        state.pods.append(pod_to_proto(row))
        # The pod's own recent reads (lab.Pod.memory, newest last) — the canvas shows them reaching
        # the pod. The daemon maps the plain snapshot dict; it never imports the agent stack.
        for record in row.get("memory") or []:
            state.memory.append(memory_hit_to_proto(str(row.get("id") or ""), record))
    for row in snapshot.get("links") or []:
        state.links.add(
            **{"from": str(row.get("from") or "")},
            to=str(row.get("to") or ""),
            kind=str(row.get("kind") or ""),
            active=bool(row.get("active")),
            count=int(row.get("count") or 0),
            label=str(row.get("label") or ""),
        )
    threat = snapshot.get("threat")
    if isinstance(threat, dict):
        message = state.threat
        message.pod, message.tool = str(threat.get("pod") or ""), str(threat.get("tool") or "")
        message.band, message.pending = str(threat.get("band") or ""), bool(threat.get("pending"))
        if (score := _opt_float(threat.get("score"))) is not None:
            message.score = score
        if (confidence := _opt_float(threat.get("confidence"))) is not None:
            message.confidence = confidence
    resources = snapshot.get("resources")
    if isinstance(resources, dict):
        state.resources.cpu_percent = float(resources.get("cpu_percent") or 0.0)
        state.resources.rss_mb = float(resources.get("rss_mb") or 0.0)
        state.resources.mcp.extend(str(name) for name in resources.get("mcp") or [])
    return state


def request_to_proto(pending: PendingPermission) -> pb.InterventionRequest:
    """The question as the GUI will see it. The child already masked its view; the daemon masks
    again (it may be talking to an older child that attached no view at all) and counts."""
    meta = pending.meta or {}
    raw = pending.raw_input
    tool = str(meta.get("tool") or (pending.title or "tool").removeprefix("Run ").split(" — ")[0])
    kind = str(meta.get("kind") or "")
    command, diff, input_json = str(meta.get("command") or ""), str(meta.get("diff") or ""), str(meta.get("input_json") or "")
    if not input_json and raw is not None:
        import json

        try:
            input_json = json.dumps(raw, indent=2, default=str, ensure_ascii=False)
        except (TypeError, ValueError):
            input_json = repr(raw)
    if not kind:
        kind = "shell" if pending.kind == "execute" else "other"
        if kind == "shell" and isinstance(raw, dict):
            command = str(raw.get("command") or raw.get("code") or "")
    redactions = int(meta.get("redactions") or 0)
    parts = []
    for text in (command, diff, input_json, str(meta.get("reason") or "")):
        masked, n = secret_patterns.scrub(text)
        redactions += n
        parts.append(masked)
    command, diff, input_json, reason = parts
    request = pb.InterventionRequest(
        intervention_id=pending.public_id,
        session_id=pending.session_id,
        pod=str(meta.get("pod") or "orchestrator"),
        tool=tool,
        kind={"shell": pb.PAYLOAD_KIND_SHELL_COMMAND, "diff": pb.PAYLOAD_KIND_FILE_DIFF}.get(kind, pb.PAYLOAD_KIND_OTHER),
        command=command,
        diff=diff,
        input_json=input_json,
        risk_band=str(meta.get("risk_band") or ""),
        armed=bool(meta.get("armed")),
        reason=reason,
        requested_ms=int(pending.ts * 1000),
        redactions=redactions,
    )
    if (score := _opt_float(meta.get("risk_score"))) is not None:
        request.risk_score = score
        # A missing band is derived rather than left blank, and `armed` only ever *tightens*: a
        # request scored at or above the router's own gate is shown as armed whatever the child
        # said. Both are one-directional — they can make a human look, never stop one looking —
        # which is the only kind of disagreement this side is allowed to introduce with a child
        # that is normally in the same trust boundary but may be an older build with no view.
        if not request.risk_band:
            request.risk_band = "destructive" if score >= RISK_GATE else ("elevated" if score >= RISK_SAFE else "safe")
        if score >= RISK_GATE:
            request.armed = True
    if (confidence := _opt_float(meta.get("risk_confidence"))) is not None:
        request.risk_confidence = confidence
    return request


def diff_ref(entry: dict[str, Any]) -> pb.DiffRef:
    return pb.DiffRef(
        id=str(entry.get("id") or ""),
        source=str(entry.get("source") or ""),
        files=[str(f) for f in (entry.get("files") or [])][:200],
        lines=int(entry.get("lines") or str(entry.get("unified") or "").count("\n")),
        truncated=bool(entry.get("truncated")),
        ts_ms=int(float(entry.get("ts") or time.time()) * 1000),
        stat=str(entry.get("stat") or "")[:2000],
        session_id=str(entry.get("session_id") or ""),
    )


def memory_event_proto(payload: dict[str, Any]) -> pb.MemoryEvent:
    detail_keys = ("size_gb", "memory_gb", "held_gb", "free_after_gb", "freed", "reason")
    detail = ", ".join(f"{k}={payload[k]}" for k in detail_keys if payload.get(k) not in (None, [], ""))
    return pb.MemoryEvent(event=str(payload.get("event") or ""), runtime=str(payload.get("runtime") or ""), model=str(payload.get("model") or ""), detail=detail[:300])


# --------------------------------------------------------------------- memory view


def read_memory(budget: memory_budget.MemoryBudget) -> pb.OmlxState:
    """oMLX and Ollama residents against the harness's budget, in one read. Never raises: a
    server that does not answer is reported ``up=False``, which is a fact, not an error."""
    state = pb.OmlxState(budget_gb=max(0.0, budget.ceiling_gb - budget.reserve_gb))
    resident = 0.0
    try:
        if budget.omlx.healthy():
            status = budget.omlx.status()
            state.up, state.pool_gb, state.loaded_gb = True, status.ceiling_gb, status.memory_gb
            resident += status.memory_gb
            for row in status.models:
                state.models.add(id=row.id, loaded=row.loaded, size_gb=round(row.size_gb, 3), pinned=row.pinned)
    except omlx_manager.OmlxError:
        state.up = False
    try:
        for row in budget.ollama.running():
            state.ollama_resident.append(f"{row.name} ({row.size_gb:.1f} GB)")
            resident += row.size_gb
    except Exception:
        logger.debug("telemetry: ollama ps unreadable", exc_info=True)
    state.used_gb = round(resident, 3)
    state.free_gb = round(state.budget_gb - resident, 3)
    return state


# ------------------------------------------------------------------------- the hub


class Subscriber:
    """One StreamTelemetry reader's pending state. See the module docstring."""

    def __init__(self, session_filter: str = "") -> None:
        self.filter = session_filter
        self.event = asyncio.Event()
        self.labs: dict[str, dict[str, Any]] = {}
        self.sections: dict[str, Any] = {}
        self.memory_events: deque[dict[str, Any]] = deque(maxlen=64)
        self.dropped = 0
        self.seq = 0

    def wants(self, session_id: str) -> bool:
        return not self.filter or self.filter == session_id

    def push_lab(self, session_id: str, state: dict[str, Any]) -> None:
        if not self.wants(session_id):
            return
        if session_id in self.labs:
            self.dropped += 1  # conflated: the previous, never-sent snapshot is replaced
        self.labs[session_id] = state
        self.event.set()

    def push_section(self, name: str, value: Any) -> None:
        self.sections[name] = value
        self.event.set()

    def push_memory_event(self, payload: dict[str, Any]) -> None:
        self.memory_events.append(payload)
        self.event.set()

    def take(self) -> list[pb.TelemetryFrame]:
        """Everything pending, as frames; the subscriber is empty afterwards."""
        labs, self.labs = self.labs, {}
        sections, self.sections = self.sections, {}
        events = list(self.memory_events)
        self.memory_events.clear()
        dropped, self.dropped = self.dropped, 0
        self.event.clear()
        frames: list[pb.TelemetryFrame] = []

        def frame(session_id: str, lab: dict[str, Any] | None) -> pb.TelemetryFrame:
            self.seq += 1
            message = pb.TelemetryFrame(seq=self.seq, ts_ms=int(time.time() * 1000), dropped=dropped if not frames else 0, session_id=session_id)
            if lab is not None:
                message.lab.CopyFrom(lab_to_proto(lab))
            return message

        for session_id, lab in labs.items():
            frames.append(frame(session_id, lab))
        if not frames and (sections or events):
            frames.append(frame("", None))
        if frames:
            head = frames[0]
            if "omlx" in sections:
                head.omlx.CopyFrom(sections["omlx"])
            if "daemon" in sections:
                head.daemon.CopyFrom(sections["daemon"])
            if "diffs" in sections:
                head.diffs.SetInParent()  # present even when empty: "the list is now empty"
                head.diffs.items.extend(sections["diffs"])
            if "pending" in sections:
                head.pending.SetInParent()
                head.pending.items.extend(sections["pending"])
            if events:
                head.memory_event.CopyFrom(memory_event_proto(events[-1]))
                for extra in events[:-1]:
                    frames.append(frame("", None))
                    frames[-1].memory_event.CopyFrom(memory_event_proto(extra))
        return frames


class TelemetryHub:
    def __init__(
        self,
        supervisor: Supervisor,
        *,
        daemon_state: Any = None,
        ingest: Any = None,
        budget: memory_budget.MemoryBudget | None = None,
        omlx_interval: float = 2.0,
        daemon_interval: float = 5.0,
    ) -> None:
        self.supervisor = supervisor
        self.daemon_state = daemon_state
        self.ingest = ingest
        self._budget = budget
        self.omlx_interval = omlx_interval
        self.daemon_interval = daemon_interval
        self.subscribers: set[Subscriber] = set()
        self.current: dict[str, Any] = {}
        self._tasks: list[asyncio.Task] = []
        self._stops: list[Any] = []

    @property
    def budget(self) -> memory_budget.MemoryBudget:
        return self._budget or memory_budget.default_budget()

    # ---------------------------------------------------------- subscribing

    def subscribe(self, session_filter: str = "") -> Subscriber:
        """A new reader, primed with *everything* so its first frame leaves it whole."""
        sub = Subscriber(session_filter)
        for session_id, lab in self.supervisor.lab_states().items():
            sub.push_lab(session_id, lab)
        self.current["daemon"] = self._daemon_section()
        self.current["pending"] = [pb.PendingIntervention(request=request_to_proto(p)) for p in self.supervisor.all_pending()]
        self.current["diffs"] = self._diff_section()
        for name, value in self.current.items():
            sub.push_section(name, value)
        sub.event.set()
        self.subscribers.add(sub)
        return sub

    def unsubscribe(self, sub: Subscriber) -> None:
        self.subscribers.discard(sub)

    # -------------------------------------------------------------- inputs

    def push_lab(self, session_id: str, state: dict[str, Any]) -> None:
        for sub in list(self.subscribers):
            sub.push_lab(session_id, state)

    def push_section(self, name: str, value: Any) -> None:
        self.current[name] = value
        for sub in list(self.subscribers):
            sub.push_section(name, value)

    def push_memory_event(self, payload: dict[str, Any]) -> None:
        for sub in list(self.subscribers):
            sub.push_memory_event(payload)

    def push_pending(self) -> None:
        self.push_section("pending", [pb.PendingIntervention(request=request_to_proto(p)) for p in self.supervisor.all_pending()])

    def _diff_section(self) -> list[pb.DiffRef]:
        return [diff_ref(entry) for entry in reversed(list(self.supervisor.diffs.values()))]

    def _daemon_section(self) -> pb.DaemonState:
        sessions = self.supervisor.sessions()
        state = self.daemon_state
        observer = getattr(state, "observer", None)
        status = observer.status() if observer is not None else {}
        stats = self.ingest.stats() if self.ingest is not None else {}
        return pb.DaemonState(
            version=SCHEMA_VERSION,
            uptime_s=time.time() - getattr(state, "started", time.time()),
            sessions=len(sessions),
            alive=sum(1 for s in sessions if s["alive"]),
            observer_enabled=bool(status.get("enabled")),
            ingest_accepted=int(stats.get("accepted", 0)),
            ingest_rejected=int(stats.get("rejected", 0)),
            telemetry_subscribers=len(self.subscribers),
        )

    async def refresh_memory(self) -> pb.OmlxState:
        value = await asyncio.to_thread(read_memory, self.budget)
        self.push_section("omlx", value)
        return value

    # ------------------------------------------------------------ lifecycle

    async def start(self) -> None:
        feed = self.supervisor.watch_lab()

        async def lab_feed() -> None:
            while True:
                frame = await feed.get()
                self.push_lab(str(frame.get("sessionId") or ""), frame.get("state") or {})

        async def polls() -> None:
            last_omlx = last_daemon = 0.0
            while True:
                await asyncio.sleep(0.25)
                if not self.subscribers:
                    continue
                now = time.monotonic()
                if now - last_omlx >= self.omlx_interval:
                    last_omlx = now
                    try:
                        await self.refresh_memory()
                    except Exception:
                        logger.debug("telemetry: memory refresh failed", exc_info=True)
                if now - last_daemon >= self.daemon_interval:
                    last_daemon = now
                    self.push_section("daemon", self._daemon_section())

        self._feed = feed
        self._tasks = [asyncio.create_task(lab_feed(), name="telemetry-lab"), asyncio.create_task(polls(), name="telemetry-polls")]

        def on_intervention(_: dict[str, Any]) -> None:
            self.push_pending()

        def on_diff(_: dict[str, Any]) -> None:
            self.push_section("diffs", self._diff_section())

        def on_memory(payload: dict[str, Any]) -> None:
            self.push_memory_event(payload)
            # The weights just moved: do not wait for the next poll to say so.
            task = asyncio.get_running_loop().create_task(self.refresh_memory())
            task.add_done_callback(lambda t: t.exception() if not t.cancelled() else None)

        self._stops = [
            self.supervisor.listen("intervention", on_intervention),
            self.supervisor.listen("diff", on_diff),
            self.supervisor.listen("memory", on_memory),
        ]

    async def stop(self) -> None:
        for stop in self._stops:
            stop()
        self._stops = []
        for task in self._tasks:
            task.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        self._tasks = []
        feed = getattr(self, "_feed", None)
        if feed is not None:
            self.supervisor.unwatch_lab(feed)

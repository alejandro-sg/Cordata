"""The gRPC telemetry bridge: the daemon's typed window for the native Lab GUI.

Replaces what would otherwise be a bespoke WebSocket dialect with the schema in
``proto/harness.proto`` — typed messages, real streaming semantics (server, client and
bidirectional), flow control from HTTP/2 instead of hand-rolled queues, and a way to evolve the
schema without breaking an older client. It is a *second front door* beside ``/ws``: the TUI
keeps driving sessions over the WebSocket; the GUI watches, approves and nudges over gRPC.

What this module is, and is not:

* It **relays** and **reports**. It never decides anything the harness decides. An approval is a
  human's click forwarded into the paused ACP request (``Supervisor.answer_permission``), restricted
  to the options the *child offered*; a knob is the child's own ``_sierralab/lab/*`` control called
  on the user's behalf; the memory unload is the same ``memory_budget`` code the agent's tools use.
  Strands routing, Laya gating, LanceDB and the vault stay where they were.
* It is **loopback-only and token-gated**. ``127.0.0.1`` is not configurable; every RPC needs the
  ``x-daemon-token`` metadata (the 0600 secret in ``daemon.json``), compared in constant time.
  gRPC over a loopback socket needs no TLS for the same reason the HTTP side does not; what it
  does need is the token, because loopback is not an authorization boundary.
* It is **bounded everywhere a client could grow it**: telemetry is latest-wins per subscriber
  (``telemetry.Subscriber``), the intervention fan-out queue is capped, ingest is capped and
  deduplicated, a snippet or diff has a size limit, and messages are capped at 16 MiB.
* Every control a GUI exercises is **audited** (``state.db``, kind ``gui``): who approved the
  destructive diff, who unloaded the models.

Two front doors can answer the same question; the first to answer wins and the other is told
(``Supervisor._admit_client_frame``): a GUI click and a TUI keypress cannot both reach the child.
"""

from __future__ import annotations

import asyncio
import hmac
import json
import logging
import math
import os
import re
import time
from collections import deque
from collections.abc import AsyncIterator
from typing import Any

import grpc

import memory_budget
import state_store

from . import harness_pb2 as pb
from . import harness_pb2_grpc as rpc
from .ingest import IngestSink
from .supervisor import PendingPermission, Supervisor
from .telemetry import SCHEMA_VERSION, DIAL_DEFAULT, DIAL_MIN, TelemetryHub, request_to_proto

logger = logging.getLogger(__name__)

TOKEN_KEY = "x-daemon-token"
DEFAULT_GRPC_PORT = 8766
MAX_MESSAGE = 16 * 1024 * 1024
HEARTBEAT_S = 5.0
INTERVENTION_QUEUE = 256
_POD = re.compile(r"^[A-Za-z0-9_.-]{1,64}$")
_EFFORT = ("low", "medium", "high", "max")
#: The permission modes, mirrored from `acp_server._MODE_IDS` (the daemon never imports the agent
#: stack); `_grpc_bridge_smoke_test` asserts the copy is equal. A mode the child does not know is
#: refused here and by the child alike.
MODES = ("plan", "build", "auto")
#: A directive longer than this is cut (lab.Lab.inject cuts at 2000 as well).
MAX_DIRECTIVE = 1500


def grpc_port_from_env() -> int | None:
    raw = os.environ.get("SIERRALAB_GRPC_PORT")
    if raw is None or raw == "":
        return None
    return int(raw)


class _Interventions:
    """Fan-out of approval events to every attached InterventionStream. Bounded per reader:
    these are rare, but a stuck reader must not be able to grow the daemon."""

    def __init__(self) -> None:
        self.readers: set[asyncio.Queue[pb.InterventionEvent]] = set()

    def attach(self) -> asyncio.Queue[pb.InterventionEvent]:
        queue: asyncio.Queue[pb.InterventionEvent] = asyncio.Queue(maxsize=INTERVENTION_QUEUE)
        self.readers.add(queue)
        return queue

    def detach(self, queue: asyncio.Queue[pb.InterventionEvent]) -> None:
        self.readers.discard(queue)

    def publish(self, event: pb.InterventionEvent) -> None:
        for queue in list(self.readers):
            try:
                queue.put_nowait(event)
            except asyncio.QueueFull:
                try:
                    queue.get_nowait()  # a reader 256 events behind wants the newest
                    queue.put_nowait(event)
                except (asyncio.QueueEmpty, asyncio.QueueFull):
                    pass


_OUTCOME = {"approved": pb.OUTCOME_APPROVED, "rejected": pb.OUTCOME_REJECTED, "aborted": pb.OUTCOME_ABORTED}


class HarnessService(rpc.HarnessServicer):
    def __init__(
        self,
        supervisor: Supervisor,
        hub: TelemetryHub,
        ingest: IngestSink,
        *,
        token: str,
        budget: memory_budget.MemoryBudget | None = None,
        daemon_id: str = "sierralab-daemon",
    ) -> None:
        self.supervisor = supervisor
        self.hub = hub
        self.ingest = ingest
        self._token = token
        self._budget = budget
        self.daemon_id = daemon_id
        self.interventions = _Interventions()
        self._unlisten = supervisor.listen("intervention", self._on_intervention)

    def close(self) -> None:
        self._unlisten()

    @property
    def budget(self) -> memory_budget.MemoryBudget:
        return self._budget or memory_budget.default_budget()

    # ----------------------------------------------------------------- auth

    async def _auth(self, context: grpc.aio.ServicerContext) -> None:
        supplied = next((value for key, value in context.invocation_metadata() if key == TOKEN_KEY), "")
        if not supplied or not hmac.compare_digest(str(supplied), self._token):
            await context.abort(grpc.StatusCode.UNAUTHENTICATED, "bad or missing daemon token")

    @staticmethod
    def _audit(action: str, payload: dict[str, Any], session_id: str | None = None) -> None:
        state_store.append_event("gui", {"action": action, **payload}, session_id=session_id or None, source="grpc")

    # ------------------------------------------------------------ telemetry

    async def StreamTelemetry(self, request: pb.TelemetryRequest, context: grpc.aio.ServicerContext) -> AsyncIterator[pb.TelemetryFrame]:
        await self._auth(context)
        sub = self.hub.subscribe(request.session_id)
        try:
            # A memory read for the first frame, so a client is whole after one message (the
            # refresh pushes to every subscriber, this one included).
            await self.hub.refresh_memory()
            while True:
                await sub.event.wait()
                for frame in sub.take():
                    yield frame
        finally:
            self.hub.unsubscribe(sub)

    # -------------------------------------------------------- interventions

    def _on_intervention(self, payload: dict[str, Any]) -> None:
        pending: PendingPermission = payload["pending"]
        if payload["event"] == "requested":
            self.interventions.publish(pb.InterventionEvent(request=request_to_proto(pending)))
        else:
            self.interventions.publish(
                pb.InterventionEvent(
                    resolved=pb.InterventionResolved(
                        intervention_id=pending.public_id,
                        outcome=_OUTCOME.get(payload.get("outcome"), pb.OUTCOME_UNSPECIFIED),
                        decided_by=str(payload.get("decided_by") or ""),
                    )
                )
            )

    async def InterventionStream(self, request_iterator: AsyncIterator[pb.InterventionReply], context: grpc.aio.ServicerContext) -> AsyncIterator[pb.InterventionEvent]:
        await self._auth(context)
        queue = self.interventions.attach()
        # Everything still open is replayed first: a GUI that starts (or reconnects) while the
        # agent is parked on an approval must be able to answer it.
        for pending in self.supervisor.all_pending():
            queue.put_nowait(pb.InterventionEvent(request=request_to_proto(pending)))

        async def read_replies() -> None:
            try:
                async for reply in request_iterator:
                    kind = reply.WhichOneof("reply")
                    if kind == "hello":
                        logger.info("grpc: intervention client %r attached", reply.hello.client)
                    elif kind == "decision":
                        await self._decide(reply.decision, queue)
            except (asyncio.CancelledError, grpc.aio.AioRpcError):
                pass
            except Exception:
                logger.debug("grpc: intervention reader failed", exc_info=True)

        reader = asyncio.create_task(read_replies(), name="grpc-intervention-reader")
        try:
            while not reader.done() or not queue.empty():
                try:
                    yield await asyncio.wait_for(queue.get(), HEARTBEAT_S)
                except asyncio.TimeoutError:
                    yield pb.InterventionEvent(heartbeat=pb.Heartbeat(ts_ms=int(time.time() * 1000)))
        finally:
            reader.cancel()
            await asyncio.gather(reader, return_exceptions=True)
            self.interventions.detach(queue)

    async def _decide(self, decision: pb.InterventionDecision, queue: asyncio.Queue[pb.InterventionEvent]) -> None:
        session_id, _, key = decision.intervention_id.partition("/")
        stale = pb.InterventionEvent(
            resolved=pb.InterventionResolved(intervention_id=decision.intervention_id, outcome=pb.OUTCOME_ABORTED, decided_by="stale")
        )
        if not session_id or not key:
            queue.put_nowait(stale)
            return
        pending = next((p for p in self.supervisor.all_pending() if p.public_id == decision.intervention_id), None)
        pod = str((pending.meta if pending else {}).get("pod") or "")
        try:
            await self.supervisor.answer_permission(session_id, key, decision.approve)
        except (LookupError, ValueError) as error:
            logger.info("grpc: decision for %s not applied: %s", decision.intervention_id, error)
            queue.put_nowait(stale)  # already answered elsewhere, or the session ended: close the modal
            return
        self._audit(
            "approve" if decision.approve else "reject",
            {"intervention": decision.intervention_id, "tool": (pending.meta if pending else {}).get("tool"), "pod": pod, "isolate": decision.isolate, "note": decision.note[:200]},
            session_id,
        )
        if decision.isolate and not decision.approve and pod:
            try:
                await self.supervisor.call(session_id, "_sierralab/lab/isolate", {"pod": pod})
                self._audit("isolate", {"pod": pod}, session_id)
            except (LookupError, asyncio.TimeoutError) as error:
                logger.warning("grpc: could not isolate %s after a rejection: %s", pod, error)

    # --------------------------------------------------------------- ingest

    async def ProactiveIngest(self, request_iterator: AsyncIterator[pb.AstSnippet], context: grpc.aio.ServicerContext) -> pb.IngestSummary:
        await self._auth(context)
        summary = pb.IngestSummary()
        async for snippet in request_iterator:
            summary.received += 1
            verdict = self.ingest.submit(snippet)
            if verdict.status == "accepted":
                summary.accepted += 1
            elif verdict.status == "deduplicated":
                summary.deduplicated += 1
            else:
                summary.rejected += 1
                if len(summary.rejections) < 5:
                    summary.rejections.append(f"{snippet.path}: {verdict.reason}")
        return summary

    # ---------------------------------------------------------------- knobs

    def _default_session(self) -> str:
        rows = [row for row in self.supervisor.sessions() if row["alive"]]
        return rows[-1]["sessionId"] if rows else ""

    async def SetKnob(self, request: pb.KnobRequest, context: grpc.aio.ServicerContext) -> pb.KnobReply:
        """A refused knob is a `KnobReply(ok=False, error=…)`, never a gRPC status: the Rust client
        shows it as an inline "refused — …" notice, whereas a status is how it learns the *daemon*
        is down and drops to offline. A bad argument must not look like a dead bridge."""
        await self._auth(context)
        session_id = request.session_id or self._default_session()
        kind = request.WhichOneof("knob")
        if not session_id:
            return pb.KnobReply(ok=False, error="no live session to apply it to")
        if not _POD.match(request.pod):
            return pb.KnobReply(ok=False, error="pod is required")
        if kind == "reasoning_effort":
            if request.reasoning_effort not in _EFFORT:
                return pb.KnobReply(ok=False, error=f"reasoning_effort must be one of {', '.join(_EFFORT)}")
            method, params = "_sierralab/lab/effort", {"pod": request.pod, "value": request.reasoning_effort}
        elif kind == "laya_dial":
            if not math.isfinite(request.laya_dial):
                return pb.KnobReply(ok=False, error="laya_dial must be a number")
            # Clamped *here* as well as in the child, because the child is not the only thing that
            # can be talked to: a client that skips its own clamp (the GPUI view does one, a
            # scripted gRPC client may not) must not be able to *loosen* the gate by asking. The
            # dial only ever tightens, so the range is the child's own [DIAL_MIN, DIAL_DEFAULT] —
            # a value at or above the default means "no dial", never "a higher gate".
            value = round(min(DIAL_DEFAULT, max(DIAL_MIN, request.laya_dial)), 2)
            method, params = "_sierralab/lab/dial", {"pod": request.pod, "value": value}
        else:
            return pb.KnobReply(ok=False, error="no knob given")
        try:
            reply = await self.supervisor.call(session_id, method, params)
        except (LookupError, asyncio.TimeoutError) as error:
            return pb.KnobReply(ok=False, error=f"the session did not answer: {error or 'timed out'}")
        self._audit("knob", {"knob": kind, "pod": request.pod, "value": params["value"], "ok": "error" not in reply}, session_id)
        if "error" in reply:
            data = (reply["error"] or {}).get("data") or {}
            return pb.KnobReply(ok=False, error=str(data.get("error") or reply["error"].get("message") or reply["error"]))
        result = reply.get("result") or {}
        return pb.KnobReply(ok=True, applied=json.dumps(result))

    async def SetMode(self, request: pb.ModeRequest, context: grpc.aio.ServicerContext) -> pb.ModeReply:
        """Switch the session's permission mode (`plan` | `build` | `auto`) by calling the child's own
        ``session/set_mode`` — the same request the TUI's Tab sends. This relays a person's choice and
        nothing more: *what* `auto` waves through is decided in the child's approval handler, where
        deny rules, the vault's fail-closed egress, Laya's destructive gate and the forced asks live,
        so a client cannot make `auto` mean "approve anything" by being clever. A refusal is a
        `ModeReply(ok=False)`, never a gRPC status (see ``SetKnob``)."""
        await self._auth(context)
        session_id = request.session_id or self._default_session()
        if not session_id:
            return pb.ModeReply(ok=False, error="no live session to apply it to")
        if request.mode not in MODES:
            return pb.ModeReply(ok=False, error=f"mode must be one of {', '.join(MODES)}")
        before = (self.supervisor.lab_states().get(session_id) or {}).get("mode") or ""
        try:
            reply = await self.supervisor.call(session_id, "session/set_mode", {"modeId": request.mode})
        except (LookupError, asyncio.TimeoutError) as error:
            return pb.ModeReply(ok=False, error=f"the session did not answer: {error or 'timed out'}")
        ok = "error" not in reply
        self._audit("mode", {"from": before, "to": request.mode, "ok": ok}, session_id)
        if not ok:
            data = (reply["error"] or {}).get("data") or {}
            return pb.ModeReply(ok=False, error=str(data.get("error") or reply["error"].get("message") or reply["error"]))
        return pb.ModeReply(ok=True, mode=request.mode)

    async def ControlPod(self, request: pb.PodControlRequest, context: grpc.aio.ServicerContext) -> pb.PodControlReply:
        """Stop, release or steer one agent — the child's own `_sierralab/lab/{isolate,release,inject}`
        called on a person's behalf (the same controls the TUI's Lab view has). Like a knob, a refusal
        is a reply, not a status. A STEER directive is length-limited here as well as in the child, and
        reaches the model as an operator directive; it is audited with its length, not its text — the
        ledger records *that* a person steered, not a second copy of whatever they typed."""
        await self._auth(context)
        session_id = request.session_id or self._default_session()
        if not session_id:
            return pb.PodControlReply(ok=False, error="no live session to apply it to")
        if not _POD.match(request.pod):
            return pb.PodControlReply(ok=False, error="pod is required")
        action = {pb.POD_ACTION_ISOLATE: "isolate", pb.POD_ACTION_RELEASE: "release", pb.POD_ACTION_STEER: "inject"}.get(request.action)
        if action is None:
            return pb.PodControlReply(ok=False, error="action must be isolate, release or steer")
        params: dict[str, Any] = {"pod": request.pod}
        if action == "inject":
            text = request.text.strip()
            if not text:
                return pb.PodControlReply(ok=False, error="a directive needs some text")
            params["text"] = text[:MAX_DIRECTIVE]
        try:
            reply = await self.supervisor.call(session_id, f"_sierralab/lab/{action}", params)
        except (LookupError, asyncio.TimeoutError) as error:
            return pb.PodControlReply(ok=False, error=f"the session did not answer: {error or 'timed out'}")
        ok = "error" not in reply
        self._audit("steer" if action == "inject" else action, {"pod": request.pod, "chars": len(params.get("text", "")) or None, "ok": ok}, session_id)
        if not ok:
            data = (reply["error"] or {}).get("data") or {}
            return pb.PodControlReply(ok=False, error=str(data.get("error") or reply["error"].get("message") or reply["error"]))
        result = reply.get("result") or {}
        return pb.PodControlReply(ok=True, affected=[request.pod, *[str(p) for p in result.get("also") or []]], queued=int(result.get("queued") or 0))

    async def UnloadMemory(self, request: pb.UnloadRequest, context: grpc.aio.ServicerContext) -> pb.UnloadReply:
        """[Unload Memory]: drop every unpinned oMLX model (and, if asked, stop Ollama's). The
        same ``memory_budget.unload_all`` the agent's ``omlx_resource_manager`` uses — the daemon
        does not load weights, it makes room for the machine."""
        await self._auth(context)
        runtimes = ("omlx", "ollama") if request.include_ollama else ("omlx",)
        result = await asyncio.to_thread(self.budget.unload_all, runtimes)
        state = await self.hub.refresh_memory()
        self.hub.push_memory_event({"event": "unload_all", "runtime": "omlx", "reason": request.reason or "gui"})
        self._audit("unload_memory", {"reason": request.reason[:200], "unloaded": [r.name for r in result.freed], "errors": result.errors})
        return pb.UnloadReply(ok=result.ok, unloaded=[r.name for r in result.freed], loaded_gb_after=state.loaded_gb, errors=result.errors)

    async def GetDiff(self, request: pb.DiffRequest, context: grpc.aio.ServicerContext) -> pb.DiffArtifact:
        await self._auth(context)
        diffs = self.supervisor.diffs
        entry = diffs.get(request.id) if request.id else (next(reversed(diffs.values())) if diffs else None)
        if entry is None:
            await context.abort(grpc.StatusCode.NOT_FOUND, "no such diff")
        from .telemetry import diff_ref

        return pb.DiffArtifact(ref=diff_ref(entry), unified=str(entry.get("unified") or ""))

    async def Ping(self, request: pb.PingRequest, context: grpc.aio.ServicerContext) -> pb.PingReply:
        await self._auth(context)
        return pb.PingReply(version=SCHEMA_VERSION, ts_ms=int(time.time() * 1000), daemon=self.daemon_id)


class GrpcBridge:
    """Owns the gRPC server, the telemetry hub and the ingest worker for one daemon."""

    def __init__(self, daemon_state: Any, *, port: int | None = None, host: str = "127.0.0.1", budget: memory_budget.MemoryBudget | None = None) -> None:
        self.state = daemon_state
        self.host = host
        self.requested_port = DEFAULT_GRPC_PORT if port is None else port
        self.port: int | None = None
        self._budget = budget
        self._server: grpc.aio.Server | None = None
        self.hub: TelemetryHub | None = None
        self.ingest: IngestSink | None = None
        self.service: HarnessService | None = None

    async def start(self) -> int:
        state = self.state
        import settings_store

        def preferences(workspace: str) -> Any:
            try:
                return settings_store.current(workspace)
            except Exception:
                return None

        def observe(text: str, workspace: str) -> None:
            state.observer.observe(text, project=workspace)

        self.ingest = IngestSink(preferences=preferences, observe=observe)
        self.ingest.start()
        self.hub = TelemetryHub(state.supervisor, daemon_state=state, ingest=self.ingest, budget=self._budget)
        await self.hub.start()
        self.service = HarnessService(state.supervisor, self.hub, self.ingest, token=state.token, budget=self._budget)
        self._server = grpc.aio.server(
            options=[
                ("grpc.max_send_message_length", MAX_MESSAGE),
                ("grpc.max_receive_message_length", MAX_MESSAGE),
                ("grpc.so_reuseport", 0),
                ("grpc.keepalive_time_ms", 30_000),
                ("grpc.keepalive_timeout_ms", 10_000),
                ("grpc.http2.min_ping_interval_without_data_ms", 10_000),
            ]
        )
        rpc.add_HarnessServicer_to_server(self.service, self._server)
        self.port = self._server.add_insecure_port(f"{self.host}:{self.requested_port}")
        if not self.port:
            raise RuntimeError(f"could not bind the gRPC bridge on {self.host}:{self.requested_port}")
        await self._server.start()
        logger.info("daemon: gRPC bridge listening on %s:%s", self.host, self.port)
        return self.port

    async def stop(self, grace: float = 2.0) -> None:
        if self._server is not None:
            await self._server.stop(grace)
            self._server = None
        if self.service is not None:
            self.service.close()
        if self.hub is not None:
            await self.hub.stop()
        if self.ingest is not None:
            await self.ingest.stop()

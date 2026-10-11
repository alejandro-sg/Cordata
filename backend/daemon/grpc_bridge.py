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
import base64
import hmac
import json
import logging
import math
import os
import re
import time
import uuid
from collections import deque
from collections.abc import AsyncIterator
from typing import Any

import grpc

import memory_budget
import os_context
import settings_store
import state_store

from . import harness_pb2 as pb
from . import harness_pb2_grpc as rpc
from .chat import Busy, ChatError, default_cwd, door_name
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
            offer(queue, event)


def offer(queue: "asyncio.Queue[pb.InterventionEvent]", event: pb.InterventionEvent) -> None:
    """Enqueue without ever raising: a reader that is a full queue behind wants the newest, so the oldest goes.
    Every put on a reader's queue goes through here — a bare ``put_nowait`` raises ``QueueFull`` into the stream
    handler and ends the GUI's connection over a backlog."""
    try:
        queue.put_nowait(event)
    except asyncio.QueueFull:
        try:
            queue.get_nowait()
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
        daemon_state: Any | None = None,
    ) -> None:
        self.supervisor = supervisor
        self.hub = hub
        self.ingest = ingest
        self._token = token
        self._budget = budget
        self.daemon_id = daemon_id
        #: The daemon's own state object, for the one RPC that starts a turn (`SubmitTurn`): the
        #: conversations live on the Discord front door it carries, and there is one set per daemon.
        self.daemon_state = daemon_state
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
            offer(queue, pb.InterventionEvent(request=request_to_proto(pending)))

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
            offer(queue, stale)
            return
        pending = next((p for p in self.supervisor.all_pending() if p.public_id == decision.intervention_id), None)
        pod = str((pending.meta if pending else {}).get("pod") or "")
        try:
            await self.supervisor.answer_permission(session_id, key, decision.approve, note=decision.note if not decision.approve else "")
        except (LookupError, ValueError) as error:
            logger.info("grpc: decision for %s not applied: %s", decision.intervention_id, error)
            offer(queue, stale)  # already answered elsewhere, or the session ended: close the modal
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

    async def SubmitTurn(self, request: pb.TurnRequest, context: grpc.aio.ServicerContext) -> pb.TurnReply:
        """A person's prompt from a front end that is not this session's own ACP client — the menu-bar
        deck — carrying the OS context they chose to attach.

        The one RPC that is not a pure relay (the proto header says why). It still decides nothing:
        what it may *start* is bounded here, by the daemon, not by the caller.

        * a conversation the daemon owns gets a real turn (`Chats.send`, the primitive Discord uses);
        * a session another client drives can only be *nudged* — the daemon never issued that session's
          `session/new` and so has no `acp_id` to start a turn with, and a second turn in one child
          would be two writers to one stdin — so the text is queued as guidance and the reply says so;
        * with no session at all, a menubar conversation is opened where chats already run.

        It **accepts and returns**: a turn may run for hours (`chat.PROMPT_TIMEOUT`), so this answers
        with the session and turn id, and progress arrives on `StreamTelemetry` like every other turn.

        The context is never taken on trust: a bundle id on the never-read list is refused outright
        (`os_context.ALWAYS_DENY_BUNDLE_IDS`), so a scripted gRPC client cannot send a password
        manager's window by skipping the Rust capture policy. The daemon only ever tightens.
        """
        await self._auth(context)
        text = request.text.strip()
        if not text:
            return pb.TurnReply(ok=False, error="a turn needs some text")
        session_id = request.session_id or self._default_session()
        ctx = request.context if request.HasField("context") else None
        bundle = str(ctx.app_bundle_id) if ctx is not None else ""
        denied = os_context.refusal(bundle)
        if denied:
            self._audit("submit_turn", {"ok": False, "reason": denied, "app_bundle_id": bundle, "client": request.client[:60]}, session_id or None)
            return pb.TurnReply(ok=False, error=denied, session_id=session_id)

        notices: list[str] = []
        payload: dict[str, Any] = {}
        #: The screenshot's part of the payload, only when it may go to a model — added for a turn the
        #: daemon starts, never for guidance to a session another client drives.
        vision: dict[str, Any] = {}
        if ctx is not None:
            payload = self._os_payload(ctx)
            if ctx.screenshot:
                model = self._vision_model()
                attach, why = os_context.screenshot_plan(True, bool(ctx.screenshot_cloud_ok), model)
                if attach:
                    vision = {
                        "screenshot_b64": base64.b64encode(bytes(ctx.screenshot)).decode("ascii"),
                        "screenshot_mime": str(ctx.screenshot_mime or "image/jpeg"),
                        "screenshot_width": int(ctx.screenshot_width),
                        "screenshot_height": int(ctx.screenshot_height),
                        "screenshot_cloud_ok": bool(ctx.screenshot_cloud_ok),
                        "vision_model": model,
                        "screenshot_attached": True,
                    }
                elif why:
                    # Captured and audited by size, but not sent — said plainly, never silently dropped.
                    notices.append(why)

        turn_id = uuid.uuid4().hex[:12]
        chats = self._chats()
        if chats is None:
            return pb.TurnReply(ok=False, error="the daemon has no conversation front end to run a turn in", session_id=session_id)

        if request.conversation:
            # The hub: a conversation every door shares, named by handle (or "latest" / "new"). A real
            # turn, whoever else is watching — they all see it.
            origin = _door_of(request.client)
            try:
                hub_conv = self._hub_conversation(chats, request.conversation, request.cwd, origin, text)
            except ChatError as error:
                return pb.TurnReply(ok=False, error=str(error))
            if hub_conv.busy:
                return pb.TurnReply(ok=False, error=f"a turn from {door_name(hub_conv.turn_origin)} is already running in this conversation — send yours when it finishes", conversation=hub_conv.handle)
            asyncio.create_task(self._deck_turn(hub_conv.key, text, {**payload, **vision}, turn_id, origin=origin), name=f"grpc-deck-turn-{turn_id}")
            self._audit("submit_turn", {"ok": True, "route": "conversation", "conversation": hub_conv.handle, **self._context_audit(payload, ctx, request.client), **self._vision_audit(vision)}, hub_conv.daemon_id or None)
            self._log_turn("conversation", turn_id, {**payload, **vision}, ctx, request.client)
            return pb.TurnReply(ok=True, session_id=hub_conv.daemon_id, turn_id=turn_id, notices=notices, route="turn", conversation=hub_conv.handle)

        conv = chats.for_session(session_id) if session_id else None

        if conv is not None:
            # A turn the daemon owns: hand it the text and let it run in the background.
            asyncio.create_task(self._deck_turn(conv.key, text, {**payload, **vision}, turn_id), name=f"grpc-deck-turn-{turn_id}")
            self._audit("submit_turn", {"ok": True, "route": "turn", **self._context_audit(payload, ctx, request.client), **self._vision_audit(vision)}, session_id)
            self._log_turn("turn", turn_id, {**payload, **vision}, ctx, request.client)
            return pb.TurnReply(ok=True, session_id=session_id, turn_id=turn_id, notices=notices, route="turn")

        if session_id and self.supervisor.session(session_id) is not None:
            # A session another client drives: guidance for its next model call, not a new turn.
            if vision:
                notices.append("the screenshot was not attached: guidance to a session another client drives is text only")
                vision = {}
            await self._register_os_context(session_id, payload, notices)
            try:
                reply = await self.supervisor.call(session_id, "_sierralab/lab/inject", {"pod": "orchestrator", "text": text[:MAX_DIRECTIVE]})
            except (LookupError, asyncio.TimeoutError) as error:
                return pb.TurnReply(ok=False, error=f"the session did not answer: {error or 'timed out'}", session_id=session_id, turn_id=turn_id, notices=notices)
            if "error" in reply:
                data = (reply["error"] or {}).get("data") or {}
                return pb.TurnReply(ok=False, error=str(data.get("error") or reply["error"].get("message") or reply["error"]), session_id=session_id, turn_id=turn_id, notices=notices)
            notices.append("queued as guidance: this session is driven by another client, so this does not start a new turn")
            self._audit("submit_turn", {"ok": True, "route": "steer", "chars": len(text), **self._context_audit(payload, ctx, request.client), **self._vision_audit(vision)}, session_id)
            self._log_turn("steer", turn_id, payload, ctx, request.client)
            return pb.TurnReply(ok=True, session_id=session_id, turn_id=turn_id, notices=notices, route="steer")

        # Nothing to talk to yet: open a menubar conversation where the other front doors keep theirs.
        cwd = default_cwd(self.supervisor)
        key = f"menubar:{cwd}"
        conv = chats.open(key, cwd=cwd, title=" ".join(text.split())[:80])
        asyncio.create_task(self._deck_turn(conv.key, text, {**payload, **vision}, turn_id), name=f"grpc-deck-turn-{turn_id}")
        self._audit("submit_turn", {"ok": True, "route": "new", "key": conv.key, **self._context_audit(payload, ctx, request.client), **self._vision_audit(vision)}, session_id or None)
        self._log_turn("new", turn_id, {**payload, **vision}, ctx, request.client)
        return pb.TurnReply(ok=True, session_id=conv.daemon_id or "", turn_id=turn_id, notices=notices, route="new")

    @staticmethod
    def _os_payload(ctx: pb.OsContext) -> dict[str, Any]:
        """The capture as `os_context.register` takes it — the text fields only. Screenshot bytes stay
        out: nothing can put them in a prompt yet, and a dict that carried them would put them in the
        ledger and the logs."""
        return {
            "app_name": str(ctx.app_name),
            "app_bundle_id": str(ctx.app_bundle_id),
            "window_title": str(ctx.window_title),
            "selected_text": str(ctx.selected_text),
            "client_redactions": int(ctx.client_redactions),
            "omitted": list(ctx.omitted),
        }

    @staticmethod
    def _context_audit(payload: dict[str, Any], ctx: pb.OsContext | None, client: str) -> dict[str, Any]:
        """What the ledger keeps about a submitted turn: which fields were present and how big, never
        the text itself and never the image. The screenshot is a byte count, and only ever that."""
        if ctx is None:
            return {"client": client[:60], "context": "none"}
        return {
            "client": client[:60],
            "app_bundle_id": str(ctx.app_bundle_id)[:120],
            "app_name": str(ctx.app_name)[:80],
            "title_chars": len(str(ctx.window_title)),
            "selection_chars": len(str(ctx.selected_text)),
            "client_redactions": int(ctx.client_redactions),
            "omitted": [str(item)[:120] for item in ctx.omitted][:8],
            "screenshot_bytes": len(ctx.screenshot),
            "screenshot_cloud_ok": bool(ctx.screenshot_cloud_ok),
        }

    @staticmethod
    def _vision_audit(vision: dict[str, Any]) -> dict[str, Any]:
        """Whether the image went to a model, and which — the ledger's answer to "who saw my screen"."""
        return {"screenshot_sent_to": str(vision.get("vision_model") or "")} if vision else {"screenshot_sent_to": ""}

    def _vision_model(self) -> str:
        """`capture.visionModel` as the daemon's settings read it, or "" — read per turn, so changing it
        takes effect on the next screenshot without a restart."""
        try:
            return str(settings_store.load(default_cwd(self.supervisor)).get(os_context.VISION_KEY) or "").strip()
        except Exception:
            logger.debug("grpc: could not read %s", os_context.VISION_KEY, exc_info=True)
            return ""

    @staticmethod
    def _log_turn(route: str, turn_id: str, payload: dict[str, Any], ctx: pb.OsContext | None, client: str) -> None:
        """One log line per accepted turn, saying what screen context came with it — the line a person
        reads to confirm the deck captured what they meant to share. Every text field is masked again
        here (`os_context._clean`) and the selection is a length and a short excerpt, never the whole of
        it: the whole text goes to the turn, not to a log that outlives it."""
        if not payload:
            logger.info("grpc: submit_turn %s from %s → %s, no screen context", turn_id, client[:60] or "?", route)
            return
        selection = os_context._clean(payload.get("selected_text"), os_context.MAX_SELECTION)
        excerpt = " ".join(selection.split())[:60]
        logger.info(
            "grpc: submit_turn %s from %s → %s, context app=%r (%s) title=%r selection=%d chars %r redactions=%d omitted=%s screenshot=%d bytes%s",
            turn_id,
            client[:60] or "?",
            route,
            os_context._clean(payload.get("app_name"), 80),
            os_context._clean(payload.get("app_bundle_id"), 120),
            os_context._clean(payload.get("window_title"), 120),
            len(selection),
            excerpt + ("…" if len(selection) > len(excerpt) else ""),
            int(payload.get("client_redactions") or 0),
            [str(item)[:80] for item in payload.get("omitted") or []][:4],
            len(ctx.screenshot) if ctx is not None else 0,
            f" → {payload['vision_model']}" if payload.get("vision_model") else "",
        )

    async def _register_os_context(self, session_id: str, payload: dict[str, Any], notices: list[str]) -> None:
        """Register the capture for a session the daemon does not drive. Sent without the daemon's own
        session id (`qualify=False`): the child keys pending context by its *own* session, and for a
        relayed session that id is the one the driving client chose, not this daemon's."""
        if not payload:
            return
        try:
            reply = await self.supervisor.call(session_id, "_sierralab/os-context", dict(payload), qualify=False)
        except (LookupError, asyncio.TimeoutError) as error:
            notices.append(f"the screen context was not attached: {error or 'the session did not answer'}")
            return
        if (reply or {}).get("error"):
            notices.append("the screen context was not attached")

    def _hub_conversation(self, chats: Any, ref: str, cwd: str, origin: str, text: str) -> Any:
        """The conversation `ref` names, opened if `latest` finds none or `new` asks for one. Its agent
        starts with the turn (`Chats.send`), not here: this RPC answers at once."""
        if ref not in ("latest", "new"):
            conv = chats.resolve(ref)
            if conv is None:
                raise ChatError(f"no conversation named {ref!r}")
            return conv
        from .chat import resolve_dir  # noqa: PLC0415

        folder = resolve_dir(cwd) if cwd else default_cwd(self.supervisor)
        conv = chats.latest(folder) if ref == "latest" else None
        return conv or chats.create(cwd=folder, title=" ".join(text.split())[:80], origin=origin)

    async def ListConversations(self, request: pb.ConversationsRequest, context: grpc.aio.ServicerContext) -> pb.ConversationList:
        """The hub's conversations, newest first — the menu bar's picker."""
        await self._auth(context)
        chats = self._chats()
        if chats is None:
            return pb.ConversationList()
        rows = chats.rows(request.cwd or None, int(request.limit) or 20)
        return pb.ConversationList(conversations=[
            pb.Conversation(
                handle=row["handle"], title=row["title"], cwd=row["cwd"], origin=row["origin"], last_used=float(row["lastUsed"]),
                turns=int(row["turns"]), live=bool(row["live"]), busy=bool(row["busy"]), turn_origin=row["turnOrigin"],
            )
            for row in rows
        ])

    async def OpenConversation(self, request: pb.ConversationOpen, context: grpc.aio.ServicerContext) -> pb.Conversation:
        """Resolve or open a conversation without starting a turn — the web app opens, watches, then
        submits, so it never misses the start of its own reply."""
        await self._auth(context)
        chats = self._chats()
        if chats is None:
            await context.abort(grpc.StatusCode.UNAVAILABLE, "the daemon has no conversation hub")
        origin = _door_of(request.origin or "gRPC")
        try:
            conv = self._hub_conversation(chats, request.conversation or "latest", request.cwd, origin, request.title)
        except ChatError as error:
            await context.abort(grpc.StatusCode.NOT_FOUND, str(error))
        if request.title and not conv.title:
            conv.title = request.title[:80]
        row = conv.row()
        return pb.Conversation(
            handle=row["handle"], title=row["title"], cwd=row["cwd"], origin=row["origin"], last_used=float(row["lastUsed"]),
            turns=int(row["turns"]), live=bool(row["live"]), busy=bool(row["busy"]), turn_origin=row["turnOrigin"],
        )

    async def WatchConversation(self, request: pb.ConversationRef, context: grpc.aio.ServicerContext) -> AsyncIterator[pb.ConversationEvent]:
        """One conversation's turns as they happen — whichever door starts them. Bounded per watcher:
        a client that stops reading loses the oldest pieces of a reply, never the daemon's memory."""
        await self._auth(context)
        chats = self._chats()
        conv = chats.resolve(request.handle) if chats is not None else None
        if conv is None:
            await context.abort(grpc.StatusCode.NOT_FOUND, f"no conversation named {request.handle!r}")
            return
        queue: asyncio.Queue[dict[str, Any]] = asyncio.Queue(maxsize=WATCH_QUEUE)

        def put(event: dict[str, Any]) -> None:
            try:
                queue.put_nowait(event)
            except asyncio.QueueFull:
                try:
                    queue.get_nowait()
                    queue.put_nowait(event)
                except (asyncio.QueueEmpty, asyncio.QueueFull):
                    pass

        history = chats.recent(conv.key)
        stop = chats.watch(conv.key, put)
        try:
            for turn in history:
                yield pb.ConversationEvent(handle=conv.handle, kind="history", origin=str(turn["origin"]), text=str(turn["asked"]), delta=str(turn["reply"]), stop=str(turn["stop"]))
            yield pb.ConversationEvent(handle=conv.handle, kind="watching", origin=conv.turn_origin, ts_ms=int(time.time() * 1000))
            while True:
                event = await queue.get()
                yield pb.ConversationEvent(
                    handle=conv.handle,
                    kind=str(event.get("event") or ""),
                    origin=str(event.get("origin") or ""),
                    text=str(event.get("text") or ""),
                    delta=str(event.get("delta") or ""),
                    label=str(event.get("label") or ""),
                    stop=str(event.get("stop") or ""),
                    ts_ms=int(time.time() * 1000),
                )
        finally:
            stop()

    async def _deck_turn(self, key: str, text: str, payload: dict[str, Any], turn_id: str, *, origin: str = "menubar") -> None:
        """Run one menubar turn to its end off the request's path, and record how it went. Nothing here
        can fail the RPC — it has already answered; a failing turn reaches the person through the pod's
        status and `stream_tail` on the telemetry stream, and its reason reaches the ledger."""
        chats = self._chats()
        if chats is None:
            return
        try:
            result = await chats.send(key, text, _DeckSink(), os_context=payload or None, origin=origin)
        except Busy as error:
            self._audit("submit_turn_turn", {"ok": False, "turn": turn_id, "error": str(error)}, None)
        except ChatError as error:
            self._audit("submit_turn_turn", {"ok": False, "turn": turn_id, "error": str(error)[:200]}, None)
            logger.info("grpc: deck turn %s for %s failed: %s", turn_id, key, error)
        except Exception:
            logger.exception("grpc: deck turn %s for %s crashed", turn_id, key)
        else:
            self._audit("submit_turn_turn", {"ok": True, "turn": turn_id, "stop": result.stop, "seconds": round(result.seconds, 1), "notices": result.notices[:4]}, None)

    def _chats(self) -> Any | None:
        """The daemon's conversations, if it has any: the Discord front door owns them (there is one
        set of conversations per daemon, shared by every front door that can start a turn)."""
        chats = getattr(self.daemon_state, "chats", None)
        if chats is not None:
            return chats
        remote = getattr(self.daemon_state, "remote", None)
        return getattr(remote, "chats", None) if remote is not None else None

    async def Ping(self, request: pb.PingRequest, context: grpc.aio.ServicerContext) -> pb.PingReply:
        await self._auth(context)
        return pb.PingReply(version=SCHEMA_VERSION, ts_ms=int(time.time() * 1000), daemon=self.daemon_id)


#: How many events one `WatchConversation` stream may have waiting before the oldest are dropped.
WATCH_QUEUE = 512


def _door_of(client: str) -> str:
    """Which front door a gRPC caller is, from its self-description — for the "[from …]" line other
    doors show. Unknown callers are named by what they said, never guessed into a door."""
    name = (client or "").lower()
    if "menubar" in name:
        return "menubar"
    if "web" in name:
        return "web"
    if "tui" in name or "terminal" in name:
        return "terminal"
    return (client or "gRPC")[:20]


class _DeckSink:
    """What a menubar turn's output goes to. Nothing: the deck has no place to draw a streaming reply,
    and the pod's `stream_tail` on `StreamTelemetry` is where a person sees it work. The sink exists
    because `Chats.send` requires one, and because silence is not the same as losing the words — the
    child still records the turn in the session's own history."""

    def text(self, delta: str) -> None:
        pass

    def activity(self, label: str | None) -> None:
        pass


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
        self.service = HarnessService(state.supervisor, self.hub, self.ingest, token=state.token, budget=self._budget, daemon_state=state)
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

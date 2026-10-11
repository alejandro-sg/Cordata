"""The web app's door into the conversation hub: the same conversations as the terminal, Discord and the
menu bar, run by the daemon on the laptop.

The web backend runs on the mini; the hub runs on the laptop, loopback-only. The laptop opens an SSH
reverse tunnel (`scripts/hub-tunnel.sh`) so the mini's own loopback port reaches the laptop's gRPC bridge,
and the web backend's container reaches that port as `host.docker.internal`. Nothing listens on the
network that did not before, and the daemon token travels as a file only the mini's user can read.

When `SIERRALAB_HUB_GRPC` is set (and a token is available), `main.py` serves the AG-UI endpoint with a
`HubAgent` instead of a local Strands agent: a web turn *is* a hub turn. The thread it runs in maps to a
hub conversation — a thread whose id is a conversation handle (`c3f9a1e`, picked from the sidebar) is
that conversation; any other thread is given one on its first message and remembers it. The reply
streams back as AG-UI text events; an approval the turn waits on is said in the reply, with where to
answer it (the web app has no approval channel into the hub yet).

Configured by:

* `SIERRALAB_HUB_GRPC`        — `host:port` of the tunnelled bridge, e.g. `host.docker.internal:18766`
* `SIERRALAB_HUB_TOKEN_FILE`  — a JSON file with `token` (the laptop's `daemon.json`, copied 0600), or
  `SIERRALAB_HUB_TOKEN`       — the token itself
* `SIERRALAB_HUB_CWD`         — the laptop folder new web conversations work in (default: the daemon's)
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import re
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, AsyncIterator

logger = logging.getLogger(__name__)

HANDLE = re.compile(r"^c[0-9a-f]{6}$")
#: How long a web turn may take before the stream gives up on it. The turn keeps running on the laptop;
#: its answer is in the conversation for any door to see.
TURN_TIMEOUT = 3 * 3600.0
#: How long the laptop has to answer at all before the web app says it is unreachable.
CONNECT_TIMEOUT = 8.0


@dataclass(frozen=True)
class HubConfig:
    target: str
    token: str
    cwd: str = ""

    def __repr__(self) -> str:  # never the token
        return f"HubConfig(target={self.target!r}, cwd={self.cwd!r})"


def configured(environ: dict[str, str] | None = None) -> HubConfig | None:
    """The hub this web backend talks to, or None to keep the local agent."""
    env = os.environ if environ is None else environ
    target = (env.get("SIERRALAB_HUB_GRPC") or "").strip()
    if not target:
        return None
    token = (env.get("SIERRALAB_HUB_TOKEN") or "").strip()
    path = (env.get("SIERRALAB_HUB_TOKEN_FILE") or "").strip()
    if not token and path:
        try:
            token = str(json.loads(Path(path).read_text(encoding="utf-8")).get("token") or "").strip()
        except (OSError, ValueError, AttributeError):
            logger.warning("hub: could not read the token file %s", path)
    if not token:
        logger.warning("hub: SIERRALAB_HUB_GRPC is set but no token is available; keeping the local agent")
        return None
    return HubConfig(target=target, token=token, cwd=(env.get("SIERRALAB_HUB_CWD") or "").strip())


def _stub(config: HubConfig):
    import grpc  # noqa: PLC0415

    from daemon import harness_pb2_grpc as rpc  # noqa: PLC0415

    channel = grpc.aio.insecure_channel(config.target, options=[("grpc.max_receive_message_length", 16 * 1024 * 1024)])
    return channel, rpc.HarnessStub(channel), (("x-daemon-token", config.token),)


async def list_conversations(config: HubConfig) -> list[dict[str, Any]]:
    from daemon import harness_pb2 as pb  # noqa: PLC0415

    channel, stub, meta = _stub(config)
    try:
        reply = await asyncio.wait_for(stub.ListConversations(pb.ConversationsRequest(limit=20), metadata=meta), CONNECT_TIMEOUT)
    finally:
        await channel.close()
    return [
        {"handle": c.handle, "title": c.title, "origin": c.origin, "lastUsed": c.last_used, "turns": c.turns, "busy": c.busy, "turnOrigin": c.turn_origin}
        for c in reply.conversations
    ]


async def history(config: HubConfig, handle: str) -> list[dict[str, Any]]:
    """The conversation's last few turns as the hub remembers them (who asked, what, the reply)."""
    from daemon import harness_pb2 as pb  # noqa: PLC0415

    channel, stub, meta = _stub(config)
    turns: list[dict[str, Any]] = []
    try:
        stream = stub.WatchConversation(pb.ConversationRef(handle=handle), metadata=meta)
        while True:
            event = await asyncio.wait_for(stream.read(), CONNECT_TIMEOUT)
            if _ended(event) or event.kind != "history":
                break
            turns.append({"origin": event.origin, "asked": event.text, "reply": event.delta, "stop": event.stop})
        stream.cancel()
    finally:
        await channel.close()
    return turns


class ThreadMap:
    """Which hub conversation each web thread is: a small JSON file next to the web sessions, 0600."""

    def __init__(self, path: Path) -> None:
        self.path = path
        try:
            self._map: dict[str, str] = json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}
        except (OSError, ValueError):
            self._map = {}

    def get(self, thread_id: str) -> str | None:
        if HANDLE.match(thread_id or ""):
            return thread_id  # picked from the sidebar: the thread *is* the conversation
        return self._map.get(thread_id)

    def set(self, thread_id: str, handle: str) -> None:
        self._map[thread_id] = handle
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            tmp = self.path.with_name(self.path.name + ".tmp")
            fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
            with os.fdopen(fd, "w", encoding="utf-8") as handle_file:
                json.dump(self._map, handle_file)
            os.replace(tmp, self.path)
        except OSError:
            logger.warning("hub: could not save the thread map at %s", self.path, exc_info=True)


def _last_user_text(input_data: Any) -> str:
    for message in reversed(list(getattr(input_data, "messages", None) or [])):
        if getattr(message, "role", "") != "user":
            continue
        content = getattr(message, "content", "")
        if isinstance(content, str):
            return content.strip()
        parts = [getattr(part, "text", "") or (part.get("text", "") if isinstance(part, dict) else "") for part in content or []]
        return " ".join(p for p in parts if p).strip()
    return ""


DOORS = {"terminal": "the terminal", "discord": "Discord", "menubar": "the menu bar", "web": "the web app"}


class HubAgent:
    """An AG-UI agent whose turns run in the laptop's conversation hub. Duck-typed to what
    `ag_ui_strands`' endpoint calls: `run(input_data) -> async iterator of events`."""

    def __init__(self, config: HubConfig, threads: ThreadMap) -> None:
        self.config = config
        self.threads = threads
        self.name = "sierralab_hub"

    async def run(self, input_data: Any, **_: Any) -> AsyncIterator[Any]:
        from ag_ui.core import EventType, RunFinishedEvent, RunStartedEvent, TextMessageContentEvent, TextMessageEndEvent, TextMessageStartEvent  # noqa: PLC0415

        thread_id, run_id = input_data.thread_id, input_data.run_id
        yield RunStartedEvent(type=EventType.RUN_STARTED, thread_id=thread_id, run_id=run_id)
        message_id = f"hub-{uuid.uuid4().hex[:12]}"
        yield TextMessageStartEvent(type=EventType.TEXT_MESSAGE_START, message_id=message_id, role="assistant")
        try:
            async for delta in self._turn(thread_id, _last_user_text(input_data)):
                if delta:
                    yield TextMessageContentEvent(type=EventType.TEXT_MESSAGE_CONTENT, message_id=message_id, delta=delta)
        except Exception as error:  # the person is told; the endpoint must not see a raw traceback
            logger.warning("hub: a web turn failed: %s", error)
            yield TextMessageContentEvent(type=EventType.TEXT_MESSAGE_CONTENT, message_id=message_id, delta=_unreachable(error))
        yield TextMessageEndEvent(type=EventType.TEXT_MESSAGE_END, message_id=message_id)
        yield RunFinishedEvent(type=EventType.RUN_FINISHED, thread_id=thread_id, run_id=run_id)

    async def _turn(self, thread_id: str, text: str) -> AsyncIterator[str]:
        from daemon import harness_pb2 as pb  # noqa: PLC0415

        if not text:
            yield "There was no message to send."
            return
        channel, stub, meta = _stub(self.config)
        try:
            handle = self.threads.get(thread_id)
            if handle is None:
                opened = await asyncio.wait_for(
                    stub.OpenConversation(pb.ConversationOpen(conversation="new", cwd=self.config.cwd, title=" ".join(text.split())[:80], origin="web"), metadata=meta),
                    CONNECT_TIMEOUT,
                )
                handle = opened.handle
                self.threads.set(thread_id, handle)
            stream = stub.WatchConversation(pb.ConversationRef(handle=handle), metadata=meta)
            # Past the replayed history to "watching": from here every event is new.
            while True:
                event = await asyncio.wait_for(stream.read(), CONNECT_TIMEOUT)
                if _ended(event):
                    yield "The hub closed the conversation before the turn started."
                    return
                if event.kind == "watching":
                    break
            reply = await asyncio.wait_for(stub.SubmitTurn(pb.TurnRequest(conversation=handle, text=text, client="web-app"), metadata=meta), CONNECT_TIMEOUT)
            if not reply.ok:
                yield f"Not sent: {reply.error}"
                return
            for notice in reply.notices:
                yield f"_{notice}_\n\n"
            ours = False
            deadline = asyncio.get_running_loop().time() + TURN_TIMEOUT
            while True:
                remaining = deadline - asyncio.get_running_loop().time()
                event = await asyncio.wait_for(stream.read(), max(1.0, remaining))
                if _ended(event):
                    yield "\n\n_The hub stream ended before the turn did — the answer is in the conversation; reopen it to see it._"
                    return
                if event.kind == "turn":
                    ours = event.origin == "web"
                elif event.kind == "text" and ours:
                    yield event.delta
                elif event.kind == "approval" and ours:
                    yield f"\n\n⏸ _Waiting for your approval: {event.label}. Answer it in the menu bar, Discord or the terminal — this reply continues once you do._\n\n"
                elif event.kind == "done" and ours:
                    if event.stop and event.stop != "end_turn":
                        yield f"\n\n_(stopped: {event.stop})_"
                    stream.cancel()
                    return
        finally:
            await channel.close()


def _ended(event: Any) -> bool:
    """A server stream's end: `grpc.aio.EOF`, not None."""
    import grpc  # noqa: PLC0415

    return event is None or event is grpc.aio.EOF


def _unreachable(error: BaseException) -> str:
    text = str(error)
    if isinstance(error, asyncio.TimeoutError) or "UNAVAILABLE" in text or "failed to connect" in text.lower():
        return "The laptop is not reachable right now (asleep, or the hub tunnel is down). Your conversation is still there — send this again when it is back."
    return f"The hub could not run that turn: {text[:300]}"

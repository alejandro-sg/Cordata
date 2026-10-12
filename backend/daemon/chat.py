"""Conversations the daemon holds on a remote person's behalf — what makes "talk to the agent from
Discord" a conversation rather than a one-way steering note.

The problem this exists for. Until now every message in the channel became a *directive*
(`lab.inject`): queued, landing at "the model's next call", answered with "Steering `…` (15 chars
queued)". Nothing the agent said ever came back, so "what can you do?" and "what are you working
on?" both got the same acknowledgement and no answer. A chat needs the other half: a prompt turn the
daemon drives itself, and the reply streamed back.

How it works. One conversation is one `acp_server.py` child that the daemon owns (`Session.owner`),
spoken to as an ACP *client*: `initialize`, `session/new`, `session/prompt`, with the reply read off
the `session/update` frames the supervisor already sees. It deliberately reuses the daemon's own
machinery rather than growing a second one:

- **Approvals** are the supervisor's `PendingPermission` + `answer_permission`, the same relay the
  GUI's containment modal uses — the front door shows them, this module does not decide anything.
- **Steering** while a turn runs is the Lab's `_cordata/lab/inject`, unchanged.
- **One writer per stdin.** An owned session is invisible to a terminal's directory lookup and
  refused by the socket (`Supervisor.find_by_cwd`, `server.session_socket`), so a person's TUI and a
  Discord chat can never be typing into the same child.

Two properties it holds that a naive version would not:

- **Idle children are reaped, conversations are not.** A child holds the whole agent stack in
  memory, and a chat that went quiet an hour ago should not keep it. The conversation (its ACP
  session id, folder and mode) is persisted — mode 0600, bounded — and the next message respawns a
  child and `session/load`s the history. A reply in a thread days later, or after a daemon restart,
  just works. History replayed by that load is muted: it is not new output.
- **A chat cannot outrun the machine.** At most `MAX_LIVE` children at once; a new one first reaps
  the least-recently-used idle one, and says so plainly when every slot is mid-turn instead of
  starting a fourth model stack on a laptop.

Stdlib only, and no `discord` import: the front door is a sink the caller hands in.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import secrets
import time
from collections import deque
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Protocol

import secret_guard

logger = logging.getLogger(__name__)

OWNER = "discord"
#: The supervisor's owner tag for every child this module drives: a hub conversation, whichever front
#: door is talking to it. Owned children are invisible to a terminal's directory lookup; a terminal
#: reaches one only by naming its conversation (`/ws?conversation=…`).
HUB = "hub"
MODES = ("plan", "build", "edit", "auto")
#: How many turns of each conversation the hub remembers for a door that starts watching, and how much
#: of each reply.
RECENT_TURNS = 6
RECENT_REPLY_CHARS = 6000

#: The front doors, as they are named in a conversation's events — and as a person reads them.
DOORS = {"discord": "Discord", "terminal": "the terminal", "menubar": "the menu bar", "web": "the web app"}


def door_name(origin: str) -> str:
    return DOORS.get(origin, origin or "another door")

#: A cold child imports the whole agent stack; this is "give up", not "expected".
HANDSHAKE_TIMEOUT = 120.0
#: A turn may legitimately run for hours (a Claude Code run, a long team plan). `!stop` is the way out.
PROMPT_TIMEOUT = 3 * 3600.0
IDLE_SECONDS = 20 * 60.0
MAX_LIVE = 3
MAX_CHATS = 40
#: What the Lab will accept as one directive (`lab.inject` caps at 2000).
MAX_STEER_CHARS = 2000
#: How long to wait, after a turn's response, for its cost receipt — sent as a notification a tick
#: before the response in practice, but the two are not ordered by the protocol.
RECEIPT_WAIT = 0.5


class ChatError(RuntimeError):
    """Something a person can be told: the message is written to be shown as-is."""


class Busy(ChatError):
    """A turn is already running in this conversation."""


class Sink(Protocol):
    """What a front door gives `send` to be told about a running turn. Synchronous and non-blocking:
    the callbacks run on the supervisor's event loop, between frames."""

    def text(self, delta: str) -> None: ...

    def activity(self, label: str | None) -> None: ...


@dataclass
class TurnResult:
    stop: str
    text: str
    seconds: float
    turn: dict[str, Any] = field(default_factory=dict)  # the child's `_cordata/turn` receipt, when it came
    #: Things the person should be told about this turn that are not the agent's words — today only "I could
    #: not restore this chat's history". Said plainly rather than letting an agent that forgot look like it remembers.
    notices: list[str] = field(default_factory=list)


@dataclass
class Conversation:
    key: str
    cwd: str
    title: str = ""
    acp_id: str = ""
    mode: str = ""
    last_used: float = field(default_factory=time.time)
    #: The name every front door uses for this conversation (`c3f9a1`): short enough to type into a
    #: terminal or a Discord message, stable across restarts. `key` is the door's own (a thread id).
    handle: str = ""
    #: The door that opened it, and the one whose turn is running now ("" when idle).
    origin: str = ""
    turn_origin: str = ""
    #: A turn a *terminal* started (its JSON-RPC id), tracked so other doors queue behind it.
    external_turn: str = ""
    #: Who is told about turns as they happen — every door showing this conversation. Live only.
    watchers: list[Callable[[dict[str, Any]], None]] = field(default_factory=list)
    #: The last few turns (who asked, what, the reply), masked and capped — what a door that starts
    #: watching is shown first, so switching doors does not mean starting blind. Memory only: the full
    #: history is the agent's own, on disk in its session.
    recent: deque = field(default_factory=lambda: deque(maxlen=RECENT_TURNS))
    #: Live state, never persisted.
    daemon_id: str = ""
    busy: bool = False
    replaying: bool = False
    sink: Any = None
    started: float = 0.0
    parts: list[str] = field(default_factory=list)
    receipt: dict[str, Any] | None = None
    replayed: int = 0  # history updates a `session/load` replayed (muted, but counted)
    notices: list[str] = field(default_factory=list)
    #: Running totals for `!usage`; persisted so a daemon restart does not forget what a chat cost.
    tokens_in: int = 0
    tokens_out: int = 0
    turns: int = 0

    def persisted(self) -> dict[str, Any]:
        return {
            # The title is the opening of the first message — secret-masked on its way to disk.
            "cwd": self.cwd, "title": secret_guard.redact(self.title), "acp_id": self.acp_id, "mode": self.mode, "last_used": self.last_used,
            "tokens_in": self.tokens_in, "tokens_out": self.tokens_out, "turns": self.turns, "handle": self.handle, "origin": self.origin,
        }

    def row(self) -> dict[str, Any]:
        """What a front door is shown when it lists conversations. No history: that is the agent's."""
        return {
            "handle": self.handle, "title": secret_guard.redact(self.title), "cwd": self.cwd, "origin": self.origin,
            "lastUsed": self.last_used, "turns": self.turns, "live": bool(self.daemon_id), "busy": self.busy,
            "turnOrigin": self.turn_origin, "mode": self.mode, "acpSessionId": self.acp_id,
        }


# --------------------------------------------------------------------------- where a chat runs


def workspace_dir() -> Path:
    import settings_store  # noqa: PLC0415 — the daemon does not otherwise build a settings view

    path = settings_store.user_dir() / "discord-workspace"
    path.mkdir(parents=True, exist_ok=True)
    try:
        os.chmod(path, 0o700)
    except OSError:
        pass
    return path


def default_mode(cwd: str) -> str:
    """The mode a chat in `cwd` starts in — the project's `permissions.defaultMode`, which is what the child
    will use. Shown in the thread's first line so a remote chat never starts in a mode its operator has not seen."""
    try:
        import settings_store  # noqa: PLC0415

        mode = str(settings_store.current(cwd).get("permissions.defaultMode", "build") or "build")
        return mode if mode in MODES else "build"
    except Exception:
        return "build"


def resolve_dir(raw: str) -> str:
    """A folder the operator named, resolved — or a ChatError saying it is not one."""
    path = Path(raw).expanduser()
    try:
        resolved = path.resolve()
    except (OSError, RuntimeError, ValueError):
        raise ChatError(f"`{raw}` is not a folder I can use.") from None
    if not resolved.is_dir():
        raise ChatError(f"`{raw}` is not a folder on this machine.")
    return str(resolved)


def default_cwd(supervisor: Any, environ: dict[str, str] | None = None) -> str:
    """Where a new chat starts, when the operator did not say: `CORDATA_DISCORD_CWD`, else the
    folder of the newest session a person is driving (the project they are plausibly asking about),
    else a private workspace. Always shown in the thread's first line — never a surprise."""
    env = os.environ if environ is None else environ
    named = (env.get("CORDATA_DISCORD_CWD") or "").strip()
    if named:
        try:
            return resolve_dir(named)
        except ChatError:
            logger.warning("chat: CORDATA_DISCORD_CWD=%r is not a folder; ignoring it", named)
    rows = [row for row in supervisor.sessions() if row.get("alive") and not row.get("owner")]
    if rows:
        return str(max(rows, key=lambda row: row.get("started") or 0)["cwd"])
    return str(workspace_dir())


def chat_store() -> Path | None:
    """Where the daemon keeps its conversations (mode 0600). The file predates the other front doors,
    hence the name; every door's conversations live in it now."""
    try:
        import settings_store  # noqa: PLC0415

        return settings_store.user_dir() / "discord_chats.json"
    except Exception:
        return None


# --------------------------------------------------------------------------- the manager


class Chats:
    """The conversations, their children, and the three supervisor topics that feed them."""

    def __init__(self, supervisor: Any, *, store: Path | None = None, idle_seconds: float = IDLE_SECONDS, max_live: int = MAX_LIVE) -> None:
        self.supervisor = supervisor
        self._store = store
        self.idle_seconds = idle_seconds
        self.max_live = max_live
        self._convs: dict[str, Conversation] = {}
        #: Other keys a conversation answers to — a Discord thread that `!continue`d a conversation another
        #: door opened. Persisted with the conversations.
        self._aliases: dict[str, str] = {}
        self._unsubscribes: list[Callable[[], None]] = []
        self._reaper: asyncio.Task | None = None
        #: Everything this daemon has spent since it started, for sessions of every kind — a terminal's
        #: turn costs the same as a chat's, and "what has this cost me" is the person's question.
        self.totals: dict[str, float] = {"turns": 0, "tokensIn": 0, "tokensOut": 0, "calls": 0, "seconds": 0.0}
        self._load()

    # ------------------------------------------------------------- lifecycle

    def start(self) -> None:
        """Listen to the supervisor. Idempotent: the daemon starts the hub, and the Discord front door
        (which predates it) still calls this when it connects."""
        if self._unsubscribes:
            return
        self._unsubscribes = [
            self.supervisor.listen("update", self._on_update),
            self.supervisor.listen("turn", self._on_turn),
            self.supervisor.listen("session", self._on_session),
            self.supervisor.listen("intervention", self._on_intervention),
        ]

    def start_reaper(self, every: float = 60.0) -> None:
        """Stop idle children on a timer — the daemon's job, not a front door's: with Discord off, a
        conversation the menu bar or a terminal woke would otherwise hold a model stack forever."""
        if self._reaper is not None and not self._reaper.done():
            return

        async def loop() -> None:
            while True:
                await asyncio.sleep(every)
                try:
                    await self.reap_idle()
                except Exception:
                    logger.debug("chat: reaping failed", exc_info=True)

        self._reaper = asyncio.create_task(loop(), name="chat-reaper")

    async def stop(self) -> None:
        if self._reaper is not None:
            self._reaper.cancel()
            self._reaper = None
        for unsubscribe in self._unsubscribes:
            try:
                unsubscribe()
            except Exception:
                logger.debug("chat: unsubscribing failed", exc_info=True)
        self._unsubscribes = []
        self._save()

    # ------------------------------------------------------------- registry

    def get(self, key: str) -> Conversation | None:
        key = str(key)
        conv = self._convs.get(key)
        if conv is None and key in self._aliases:
            conv = self._convs.get(self._aliases[key])
        return conv

    def bind(self, alias: str, key: str) -> Conversation:
        """Make `alias` (a Discord thread id) another name for an existing conversation."""
        conv = self.resolve(key)
        if conv is None:
            raise ChatError("I have no record of this conversation.")
        self._aliases[str(alias)] = conv.key
        self._save()
        return conv

    def aliases_of(self, key: str) -> list[str]:
        return [alias for alias, target in self._aliases.items() if target == key]

    def for_session(self, daemon_id: str) -> Conversation | None:
        return next((c for c in self._convs.values() if c.daemon_id and c.daemon_id == daemon_id), None)

    def all(self) -> list[Conversation]:
        return list(self._convs.values())

    def open(self, key: str, *, cwd: str, title: str = "", origin: str = OWNER) -> Conversation:
        """Register a conversation (no child yet — that happens on the first message)."""
        key = str(key)
        conv = self._convs.get(key)
        if conv is None:
            conv = Conversation(key=key, cwd=cwd, title=title, handle=self._new_handle(), origin=origin)
            self._convs[key] = conv
            self._trim()
        self._save()
        return conv

    def create(self, *, cwd: str, title: str = "", origin: str) -> Conversation:
        """A new conversation opened from a door with no key of its own (a terminal, the menu bar, the
        web app): its handle is its key."""
        handle = self._new_handle()
        conv = Conversation(key=handle, cwd=cwd, title=title, handle=handle, origin=origin)
        self._convs[handle] = conv
        self._trim()
        self._save()
        return conv

    def resolve(self, ref: str) -> Conversation | None:
        """A conversation by its handle (what people type) or its key (what a door stores)."""
        ref = str(ref or "").strip()
        if not ref:
            return None
        return self.get(ref) or next((c for c in self._convs.values() if c.handle == ref), None)

    def latest(self, cwd: str | None = None) -> Conversation | None:
        """The most recently used conversation — in `cwd` when given (a terminal reopening a project
        picks up where any door left it there)."""
        target = None
        if cwd:
            try:
                target = resolve_dir(cwd)
            except ChatError:
                return None
        rows = [c for c in self._convs.values() if target is None or c.cwd == target]
        return max(rows, key=lambda c: c.last_used, default=None)

    def rows(self, cwd: str | None = None, limit: int = 20) -> list[dict[str, Any]]:
        """Conversations, newest first, for a door's picker."""
        target = None
        if cwd:
            try:
                target = resolve_dir(cwd)
            except ChatError:
                target = cwd
        convs = [c for c in self._convs.values() if target is None or c.cwd == target]
        return [c.row() for c in sorted(convs, key=lambda c: c.last_used, reverse=True)[:limit]]

    def watch(self, key: str, callback: Callable[[dict[str, Any]], None]) -> Callable[[], None]:
        """Be told about this conversation's turns — whoever starts them: `turn` (who, what they said),
        `text` deltas, `activity`, `done`. A front door showing the conversation calls this; the door
        that started a turn hears its own turn too, so every door draws from one stream."""
        conv = self.resolve(key)
        if conv is None:
            raise ChatError("I have no record of this conversation.")
        conv.watchers.append(callback)

        def stop() -> None:
            if callback in conv.watchers:
                conv.watchers.remove(callback)

        return stop

    def recent(self, key: str) -> list[dict[str, Any]]:
        """The conversation's last few turns, oldest first: `{origin, asked, reply, stop}`."""
        conv = self.resolve(key)
        return [dict(turn) for turn in conv.recent] if conv is not None else []

    def _remember(self, conv: Conversation, event: dict[str, Any]) -> None:
        kind = event.get("event")
        if kind == "turn":
            conv.recent.append({"origin": event.get("origin", ""), "asked": str(event.get("text") or "")[:2000], "reply": "", "stop": ""})
        elif kind == "text" and conv.recent and not conv.recent[-1]["stop"]:
            turn = conv.recent[-1]
            if len(turn["reply"]) < RECENT_REPLY_CHARS:
                turn["reply"] += str(event.get("delta") or "")
        elif kind == "done" and conv.recent and not conv.recent[-1]["stop"]:
            turn = conv.recent[-1]
            turn["stop"] = str(event.get("stop") or "end_turn")
            turn["reply"] = secret_guard.redact(turn["reply"])

    def _emit(self, conv: Conversation, event: dict[str, Any]) -> None:
        self._remember(conv, event)
        payload = {"handle": conv.handle, **event}
        for callback in list(conv.watchers):
            try:
                callback(payload)
            except Exception:
                logger.debug("chat: a watcher of %s failed", conv.handle, exc_info=True)

    def _new_handle(self) -> str:
        taken = {c.handle for c in self._convs.values()}
        while True:
            handle = "c" + secrets.token_hex(3)
            if handle not in taken:
                return handle

    async def ensure(self, key: str) -> Conversation:
        """Make sure the conversation has a live child with its session loaded (or newly opened), for a
        door that attaches to it rather than sending a turn — a terminal. Refused mid-turn only while
        the child is being (re)started; attaching to a running turn is allowed and is the point."""
        conv = self.resolve(key)
        if conv is None:
            raise ChatError("I have no record of this conversation.")
        await self._ensure_child(conv)
        conv.last_used = time.time()
        self._save()
        return conv

    # ------------------------------------------------------------- turns a terminal starts

    def begin_external(self, conv: Conversation, request_id: str, text: str, origin: str = "terminal") -> None:
        """A terminal attached to this conversation sent `session/prompt` itself (the daemon relays it).
        Recorded so the conversation reads as busy to every other door, and announced to the watchers."""
        conv.busy = True
        conv.external_turn = str(request_id)
        conv.turn_origin = origin
        conv.started = time.monotonic()
        conv.last_used = time.time()
        if not conv.title and text:
            conv.title = " ".join(text.split())[:80]
        self._emit(conv, {"event": "turn", "origin": origin, "text": secret_guard.redact(text)[:4000]})
        self._save()

    def end_external(self, conv: Conversation, stop: str = "end_turn") -> None:
        """The terminal's turn ended (its response passed through, or the turn's receipt arrived first).
        Idempotent: both signals call it."""
        if not conv.external_turn:
            return
        origin = conv.turn_origin
        conv.external_turn = ""
        conv.busy = False
        conv.turn_origin = ""
        conv.last_used = time.time()
        self._emit(conv, {"event": "done", "origin": origin, "stop": stop})
        self._save()

    def forget(self, key: str) -> None:
        self._convs.pop(str(key), None)
        self._save()

    def live(self) -> list[Conversation]:
        return [c for c in self._convs.values() if c.daemon_id]

    # ------------------------------------------------------------- the turn

    async def send(self, key: str, text: str, sink: Sink, *, os_context: dict[str, Any] | None = None, origin: str = OWNER) -> TurnResult:
        conv = self.resolve(key)
        if conv is None:
            raise ChatError("I have no record of this conversation.")
        if conv.busy:
            who = f" (from {door_name(conv.turn_origin)})" if conv.turn_origin and conv.turn_origin != origin else ""
            raise Busy(f"A turn is already running here{who}.")
        conv.busy = True  # claimed before any await: two messages arriving together cannot both start a turn
        conv.turn_origin = origin
        conv.sink, conv.parts, conv.receipt, conv.started = sink, [], None, time.monotonic()
        stop = "error"
        try:
            await self._ensure_child(conv)
            self._emit(conv, {"event": "turn", "origin": origin, "text": secret_guard.redact(text)[:4000]})
            self._tell_terminal(conv, origin, text)
            if os_context:
                # Registered after the child exists: it is keyed by the ACP session id `session/new`
                # returned, which is not the daemon's own id for this session.
                await self._send_os_context(conv, os_context)
            reply = await self.supervisor.call(
                conv.daemon_id,
                "session/prompt",
                {"sessionId": conv.acp_id, "prompt": [{"type": "text", "text": text}]},
                timeout=PROMPT_TIMEOUT,
                qualify=False,
            )
            error = (reply or {}).get("error")
            if error:
                raise ChatError(f"The agent refused that turn: {_why(error)}")
            stop = str(((reply or {}).get("result") or {}).get("stopReason") or "end_turn")
            for _ in range(int(RECEIPT_WAIT / 0.05)):
                if conv.receipt is not None:
                    break
                await asyncio.sleep(0.05)
            notices, conv.notices = conv.notices, []
            return TurnResult(stop=stop, text="".join(conv.parts), seconds=time.monotonic() - conv.started, turn=dict(conv.receipt or {}), notices=notices)
        except asyncio.TimeoutError:
            await self._abort(conv)
            raise ChatError("That turn ran past its time limit and was stopped.") from None
        except LookupError as error:
            conv.daemon_id = ""
            logger.info("chat: %s lost its agent mid-turn: %s", conv.key, error)
            raise ChatError("The agent process went away mid-turn. Send your message again to start a fresh one.") from None
        finally:
            conv.busy = False
            conv.sink = None
            conv.turn_origin = ""
            conv.last_used = time.time()
            if not conv.title and text:
                conv.title = " ".join(text.split())[:80]
            self._emit(conv, {"event": "done", "origin": origin, "stop": stop})
            self._save()

    def _tell_terminal(self, conv: Conversation, origin: str, text: str) -> None:
        """A terminal attached to this conversation sees the agent's reply to another door's turn as it
        streams (it gets every frame the child writes), but not what was *asked* — the child never
        echoes a prompt. Hand it one `user_message_chunk`, the frame ACP uses for a person's words, so
        the transcript reads "[from discord] …" above the answer instead of an answer to nothing."""
        if not conv.daemon_id or not conv.acp_id or origin == "terminal":
            return
        frame = {
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": conv.acp_id, "update": {"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": f"[from {origin}] {text}"}}},
        }
        push = getattr(self.supervisor, "push_to_subscriber", None)
        if push is not None:
            push(conv.daemon_id, json.dumps(frame))

    async def _send_os_context(self, conv: Conversation, payload: dict[str, Any]) -> None:
        """Register the menu bar's capture for this conversation's next turn (`os_context.py`).

        The child keys the pending context by *its own* ACP session id — the one `session/new`
        returned, which is not this daemon's id for the session — so it has to be sent explicitly with
        ``qualify=False`` (see `Supervisor.call`). A failure is a notice on the turn, never a refusal:
        the person asked for a turn, and losing the screen is not losing the turn.
        """
        if not conv.acp_id:
            conv.notices.append("I couldn't attach the screen context: the agent had not opened a session yet.")
            return
        try:
            reply = await self.supervisor.call(conv.daemon_id, "_cordata/os-context", {"sessionId": conv.acp_id, **payload}, qualify=False)
        except (LookupError, asyncio.TimeoutError) as error:
            conv.notices.append(f"I couldn't attach the screen context: {error or 'the agent did not answer'}.")
            return
        if (reply or {}).get("error"):
            logger.info("chat: %s refused the os context: %s", conv.key, _why(reply["error"]))
            conv.notices.append("I couldn't attach the screen context.")

    async def _ensure_child(self, conv: Conversation) -> None:
        sup = self.supervisor
        if conv.daemon_id:
            live = sup.session(conv.daemon_id)
            if live is not None and live.alive:
                return
            conv.daemon_id = ""
        await self._make_room(conv)
        try:
            session = await sup.spawn(conv.cwd, owner=HUB)
        except ValueError as error:
            raise ChatError(f"I could not start an agent in `{conv.cwd}`: {error}") from None
        conv.daemon_id = session.session_id
        try:
            init = await sup.call(
                session.session_id, "initialize",
                {"protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {"name": "cordata-discord", "version": "1"}},
                timeout=HANDSHAKE_TIMEOUT, qualify=False,
            )
            if init.get("error"):
                raise ChatError(f"The agent would not start: {_why(init['error'])}")
            restored = False
            had_history = bool(conv.acp_id and conv.turns)
            if conv.acp_id:
                conv.replaying, conv.replayed = True, 0
                try:
                    loaded = await sup.call(
                        session.session_id, "session/load",
                        {"cwd": conv.cwd, "sessionId": conv.acp_id, "mcpServers": []},
                        timeout=HANDSHAKE_TIMEOUT, qualify=False,
                    )
                finally:
                    conv.replaying = False
                restored = not loaded.get("error")
                if not restored:
                    logger.info("chat: could not restore %s (%s); starting fresh", conv.acp_id, _why(loaded["error"]))
                if had_history and (not restored or not conv.replayed):
                    # The session is gone from disk (or was never saved): the agent will answer, but from a
                    # blank slate. Say so — an agent that forgot must not look like one that remembers.
                    conv.notices.append("I couldn't restore this chat's earlier history, so I'm starting from a blank slate here.")
            if not restored:
                created = await sup.call(
                    session.session_id, "session/new", {"cwd": conv.cwd, "mcpServers": []},
                    timeout=HANDSHAKE_TIMEOUT, qualify=False,
                )
                if created.get("error"):
                    raise ChatError(f"The agent would not open a session: {_why(created['error'])}")
                conv.acp_id = str((created.get("result") or {}).get("sessionId") or "")
                if not conv.acp_id:
                    raise ChatError("The agent opened a session but did not name it.")
            if conv.mode:
                await sup.call(session.session_id, "session/set_mode", {"sessionId": conv.acp_id, "modeId": conv.mode}, timeout=15.0, qualify=False)
        except asyncio.TimeoutError:
            await sup.stop(session.session_id)
            conv.daemon_id = ""
            raise ChatError("The agent did not finish starting in time.") from None
        except BaseException:
            await sup.stop(session.session_id)
            conv.daemon_id = ""
            raise

    async def _make_room(self, wanted: Conversation) -> None:
        """Reap the least-recently-used idle child if every slot is taken; say so when none can go."""
        while len(self.live()) >= self.max_live:
            idle = sorted((c for c in self.live() if not c.busy and c is not wanted and not self._waiting(c)), key=lambda c: c.last_used)
            if not idle:
                raise ChatError(f"{self.max_live} chats are already working. Wait for one to finish, or `!stop` it, then try again.")
            await self._reap(idle[0])

    def _waiting(self, conv: Conversation) -> bool:
        session = self.supervisor.session(conv.daemon_id) if conv.daemon_id else None
        return bool(session is not None and session.pending)

    # ------------------------------------------------------------- control

    async def steer(self, key: str, text: str) -> int:
        """Queue a note for the running agent's next model call. Returns how many are pending."""
        conv = self._convs.get(str(key))
        if conv is None or not conv.daemon_id:
            raise ChatError("The agent is not running — send a message to wake it.")
        try:
            reply = await self.supervisor.call(conv.daemon_id, "_cordata/lab/inject", {"pod": "orchestrator", "text": text[:MAX_STEER_CHARS]})
        except (LookupError, asyncio.TimeoutError) as error:
            raise ChatError(f"I could not reach the agent: {error or 'timed out'}") from None
        error = (reply or {}).get("error")
        if error:
            raise ChatError(f"The agent did not take that note: {_why(error)}")
        return int(((reply or {}).get("result") or {}).get("queued") or 0)

    async def conversation(self, key: str, action: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
        """`list` / `rewind` the conversation of one chat (`_cordata/conversation/*`): the agent forgets turns, or the
        checkpoints are listed. A chat that is asleep is woken first — its history is on disk, and that is what is rewound.
        Refused while a turn is running."""
        conv = self._convs.get(str(key))
        if conv is None:
            raise ChatError("I have no record of this conversation.")
        if conv.busy:
            raise ChatError("A turn is running here — `!stop` it first, then go back.")
        conv.busy = True  # a message arriving meanwhile waits its turn: the history is being replaced
        try:
            await self._ensure_child(conv)
            reply = await self.supervisor.call(conv.daemon_id, f"_cordata/conversation/{action}", dict(params or {}), timeout=30.0)
        except (LookupError, asyncio.TimeoutError) as error:
            raise ChatError(f"I could not reach the agent: {error or 'timed out'}") from None
        finally:
            conv.busy = False
        error = (reply or {}).get("error")
        if error:
            raise ChatError(f"The agent did not do that: {_why(error)}")
        return dict((reply or {}).get("result") or {})

    async def cancel(self, key: str) -> bool:
        """Stop the running turn. A pending approval is denied first: a child parked on one never
        reaches the cancel it was just sent."""
        conv = self._convs.get(str(key))
        if conv is None or not conv.busy or not conv.daemon_id:
            return False
        await self._abort(conv)
        return True

    async def _abort(self, conv: Conversation) -> None:
        session = self.supervisor.session(conv.daemon_id) if conv.daemon_id else None
        if session is None:
            return
        for pending in list(session.pending):
            try:
                await self.supervisor.answer_permission(conv.daemon_id, pending, False, decided_by=OWNER)
            except (LookupError, ValueError):
                pass
        await self.supervisor.notify_child(conv.daemon_id, "session/cancel", {"sessionId": conv.acp_id})

    async def set_mode(self, key: str, mode: str) -> None:
        if mode not in MODES:
            raise ChatError(f"Mode is one of {', '.join(MODES)}.")
        conv = self._convs.get(str(key))
        if conv is None:
            raise ChatError("I have no record of this conversation.")
        conv.mode = mode
        self._save()
        if conv.daemon_id and conv.acp_id:
            try:
                reply = await self.supervisor.call(conv.daemon_id, "session/set_mode", {"sessionId": conv.acp_id, "modeId": mode}, timeout=15.0, qualify=False)
            except (LookupError, asyncio.TimeoutError) as error:
                raise ChatError(f"The agent did not take the mode change: {error or 'timed out'}") from None
            if reply.get("error"):
                raise ChatError(f"The agent did not take the mode change: {_why(reply['error'])}")

    # ------------------------------------------------------------- reaping

    async def _reap(self, conv: Conversation) -> None:
        daemon_id, conv.daemon_id = conv.daemon_id, ""
        if daemon_id:
            await self.supervisor.stop(daemon_id)
            logger.info("chat: stopped the idle agent for %s (%s)", conv.key, conv.title or conv.cwd)

    async def reap_idle(self, now: float | None = None) -> list[str]:
        """Stop children that have been quiet past `idle_seconds`. The conversation stays, and so does
        its history on disk — the next message respawns and `session/load`s it."""
        now = time.time() if now is None else now
        reaped: list[str] = []
        for conv in list(self.live()):
            if conv.busy or self._waiting(conv) or now - conv.last_used < self.idle_seconds:
                continue
            await self._reap(conv)
            reaped.append(conv.key)
        return reaped

    # ------------------------------------------------------------- supervisor topics

    def _on_update(self, payload: dict[str, Any]) -> None:
        conv = self.for_session(str(payload.get("session_id") or ""))
        if conv is not None and conv.replaying:
            conv.replayed += 1  # history coming back from `session/load`: counted, never shown
            return
        if conv is None or not conv.busy or (conv.sink is None and not conv.watchers):
            # Outside a turn the frames are a terminal's `session/load` replay or noise: never fanned out.
            return
        update = payload.get("update") or {}
        kind = update.get("sessionUpdate")
        sink = conv.sink
        try:
            if kind == "agent_message_chunk":
                content = update.get("content") or {}
                delta = content.get("text") if isinstance(content, dict) else None
                if isinstance(delta, str) and delta:
                    if sink is not None:
                        conv.parts.append(delta)
                        sink.text(delta)
                    self._emit(conv, {"event": "text", "delta": delta})
            elif kind == "tool_call":
                label = str(update.get("title") or "working")
                if sink is not None:
                    sink.activity(label)
                self._emit(conv, {"event": "activity", "label": label})
            elif kind == "tool_call_update" and update.get("status") in ("completed", "failed"):
                if sink is not None:
                    sink.activity(None)
                self._emit(conv, {"event": "activity", "label": None})
            elif kind == "agent_thought_chunk":
                if sink is not None:
                    sink.activity("thinking")
                self._emit(conv, {"event": "activity", "label": "thinking"})
        except Exception:
            logger.debug("chat: sink failed on %s", kind, exc_info=True)

    def _on_intervention(self, payload: dict[str, Any]) -> None:
        """A conversation's turn is waiting on an approval: every door watching it is told what, so one
        that cannot answer it (the web app) can say where it can be — the menu bar, Discord, a terminal."""
        if payload.get("event") != "requested":
            return
        pending = payload.get("pending")
        conv = self.for_session(str(getattr(pending, "session_id", "") or ""))
        if conv is None:
            return
        label = str(getattr(pending, "title", None) or getattr(pending, "kind", None) or "a step")
        self._emit(conv, {"event": "approval", "label": secret_guard.redact(label)[:200], "origin": conv.turn_origin})

    def _on_turn(self, payload: dict[str, Any]) -> None:
        tin, tout = int(payload.get("tokensIn") or 0), int(payload.get("tokensOut") or 0)
        self.totals["turns"] += 1
        self.totals["tokensIn"] += tin
        self.totals["tokensOut"] += tout
        self.totals["calls"] += int(payload.get("calls") or 0)
        self.totals["seconds"] += float(payload.get("seconds") or 0.0)
        conv = self.for_session(str(payload.get("session_id") or ""))
        if conv is not None:
            conv.receipt = dict(payload)
            conv.tokens_in += tin
            conv.tokens_out += tout
            conv.turns += 1
            # A terminal's turn whose response the daemon did not see (the terminal detached mid-turn):
            # the receipt is the turn's end all the same.
            self.end_external(conv, str(payload.get("stopReason") or "end_turn"))

    def _on_session(self, payload: dict[str, Any]) -> None:
        if payload.get("event") != "exited":
            return
        conv = self.for_session(str(payload.get("session_id") or ""))
        if conv is not None:
            self.end_external(conv, "error")
            conv.daemon_id = ""

    # ------------------------------------------------------------- persistence

    def _trim(self) -> None:
        if len(self._convs) > MAX_CHATS:
            for key, _ in sorted(((k, c.last_used) for k, c in self._convs.items() if not c.daemon_id), key=lambda kv: kv[1])[: len(self._convs) - MAX_CHATS]:
                self._convs.pop(key, None)

    def _save(self) -> None:
        if self._store is None:
            return
        try:
            self._store.parent.mkdir(parents=True, exist_ok=True)
            aliases = {alias: key for alias, key in self._aliases.items() if key in self._convs}
            body = json.dumps({"version": 1, "chats": {k: c.persisted() for k, c in self._convs.items()}, "aliases": aliases}, indent=1)
            tmp = self._store.with_name(self._store.name + ".tmp")
            fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                handle.write(body)
            os.replace(tmp, self._store)
        except OSError:
            logger.warning("chat: could not save conversations to %s", self._store, exc_info=True)

    def _load(self) -> None:
        if self._store is None or not self._store.exists():
            return
        try:
            data = json.loads(self._store.read_text(encoding="utf-8"))
            for key, row in (data.get("chats") or {}).items():
                if isinstance(row, dict) and isinstance(row.get("cwd"), str):
                    self._convs[str(key)] = Conversation(
                        key=str(key), cwd=row["cwd"], title=str(row.get("title") or ""), acp_id=str(row.get("acp_id") or ""),
                        mode=str(row.get("mode") or ""), last_used=float(row.get("last_used") or 0.0),
                        tokens_in=int(row.get("tokens_in") or 0), tokens_out=int(row.get("tokens_out") or 0), turns=int(row.get("turns") or 0),
                        handle=str(row.get("handle") or ""), origin=str(row.get("origin") or OWNER),
                    )
            for conv in self._convs.values():
                if not conv.handle:
                    conv.handle = self._new_handle()  # conversations saved before handles existed
            self._aliases = {str(a): str(k) for a, k in (data.get("aliases") or {}).items() if str(k) in self._convs}
        except (OSError, ValueError, TypeError):
            logger.warning("chat: could not read %s; starting with no conversations", self._store, exc_info=True)


def _why(error: Any) -> str:
    """The most specific line a JSON-RPC error carries, for showing a person."""
    if isinstance(error, dict):
        data = error.get("data")
        if isinstance(data, dict) and data.get("error"):
            return str(data["error"])
        if isinstance(data, str) and data:
            return data
        return str(error.get("message") or error)
    return str(error)

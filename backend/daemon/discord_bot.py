"""The daemon's outbound Discord front door: talk to agents, decide what needs deciding, see where
things stand.

What it is for. A daemon that outlives a terminal needs a way to reach the person. This is it, and
it is built around three things a person actually does from a phone:

1. **Talk to an agent.** Write in the channel and a thread opens with an agent that answers — a real
   prompt turn, the reply streamed into the thread with a receipt (time, tool calls, tokens) under it.
   Reply in the thread to continue; messages sent while it is working steer it at its next step
   (`daemon/chat.py` is the conversation; this module is its face).
2. **Decide what needs deciding — and only that.** The gate pauses a call by raising a Strands
   interrupt, which `acp_server._resolve_interrupts` turns into an ACP `session/request_permission`
   the supervisor holds as a `PendingPermission`. This module shows it, relays the click through
   `Supervisor.answer_permission` (the same relay the GUI's containment modal uses, so whichever door
   answers first wins), and nothing else. A card says *what, where and why* in plain words before it
   says how risky (`discord_cards.approval_card`); a decided card collapses to one line; only a call at
   or above the destructive line pings anybody; and "Allow routine steps in this chat" ends the
   routine ones for good while destructive calls keep asking.
3. **Ask where things stand.** "What are you working on?" and `!status` are answered from state the
   daemon already holds — no model call, so no tokens, and the agent is not interrupted to say it.

Properties this module is built to hold.

- **Outbound only.** One WebSocket to Discord's gateway, no inbound port, no webhook. Nothing on this
  machine becomes reachable because this is running.
- **Zero trust on the caller.** `ALLOWED_USER_ID` is an allowlist of exactly one. A click or a message
  from anyone else is dropped *before* anything is touched — a stranger's click is not a decision, it
  is noise, and it earns an ephemeral refusal and no state change. With no `ALLOWED_USER_ID` parsed the
  bot does not start at all: a front door with no allowlist is an unauthenticated one.
- **The token is never a setting.** `DISCORD_BOT_TOKEN` is read through `secret_guard.take_secret`
  (environment first, then the macOS Keychain item `sierralab-discord-bot-token`), which also takes it
  out of `os.environ` so no tool subprocess can read it, and registers it for output masking.
- **An agent's words cannot ping anyone.** Everything a model wrote is posted with mentions disabled
  and secret-masked: `@everyone` in a reply is text, not a notification.
- **Fail quiet.** A missing token, an unparsable id, a privileged-intent refusal or a Discord outage
  all disable or degrade this module and are logged; none of them may take the daemon down. A chat
  whose approval cannot be delivered is *denied*, never left parked or waved through.

Scope, stated plainly: this is a *front door*, not a second policy engine. It shows what the gate
already decided to ask about, and it cannot approve anything the gate did not offer
(`answer_permission` only ever names an option the child itself listed).

Off unless configured: with no `DISCORD_BOT_TOKEN` in reach, `start()` returns immediately and the
daemon behaves exactly as it did before this module existed. `discord` itself is imported inside
`start()` for the same reason — a checkout that has not installed it is unaffected.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
import os
import re
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Mapping

import secret_guard

from . import discord_cards as cards
from .chat import OWNER, Busy, ChatError, Chats, Conversation, default_cwd, default_mode, resolve_dir

logger = logging.getLogger(__name__)

#: The three things this module reads. Two of them are ids, not credentials; the token is a
#: credential and is handled by `secret_guard` (see the module docstring).
TOKEN_ENV = "DISCORD_BOT_TOKEN"
ALLOWED_ENV = "ALLOWED_USER_ID"
CHANNEL_ENV = "TELEMETRY_CHANNEL_ID"

#: An escape hatch that needs no edit to the environment it was launched with: `SIERRALAB_DISCORD=off`
#: keeps the token in place and the bot down (a person debugging the daemon should not have to delete a
#: credential to stop it talking).
KILL_ENV = "CORDATA_DISCORD"
LEGACY_KILL_ENV = "SIERRALAB_DISCORD"
OFF = ("0", "off", "false", "no")

#: The Keychain item holding the bot token, if it is not in the environment:
#: `security add-generic-password -a "$USER" -s cordata-discord-bot-token -w` (or `make discord-key`).
KEYCHAIN_ITEM = "cordata-discord-bot-token"
KEYCHAIN_ITEMS = ("cordata-discord-bot-token", "sierralab-discord-bot-token")

#: The directive's destructive line, and `laya.riskDestroy`'s default. Read per project so a session
#: that raised or lowered its own line is asked about at the line it actually set.
DEFAULT_DESTROY = 0.8

#: What a steering note may carry. The Lab caps at 2000 and `acp_server._lab_control` audits the length.
MAX_STEER_CHARS = 2000
#: One edit to a live reply per this many seconds: Discord rate-limits message edits, and a reply that
#: flickers on every token is unreadable anyway.
EDIT_INTERVAL = 1.2
#: One post per thread per this many seconds for coalesced task lines (a task bar updates at 20 Hz).
MIN_POST_INTERVAL = 1.5
#: How often idle chat children are checked for reaping.
REAP_EVERY = 60.0

def _scratch_roots() -> tuple[str, ...]:
    import tempfile  # noqa: PLC0415

    roots = {"/tmp", "/private/tmp", "/var/folders", "/private/var/folders", tempfile.gettempdir()}
    return tuple(sorted({os.path.realpath(root) for root in roots} | roots))


#: A session in a temporary folder is almost always a test run (the harness's own smoke tests, a script driving the
#: daemon by hand), and its prompts used to page the operator as if they were real work — "a prompt I don't
#: understand about a folder I have never heard of". Such a card is still posted (a call parked with nobody told
#: is worse than noise) but labelled as scratch and never pings.
SCRATCH_ROOTS = _scratch_roots()


def is_scratch(cwd: str | None) -> bool:
    if not cwd:
        return False
    try:
        resolved = os.path.realpath(cwd)
    except (OSError, ValueError):
        return False
    return any(resolved == root or resolved.startswith(root.rstrip(os.sep) + os.sep) for root in SCRATCH_ROOTS)


#: Which `harness_events.Task` sources mean "a multi-step workflow worth its own thread" in a session a
#: *terminal or the GUI* is driving: real multi-agent or long-running work. Infrastructure chatter
#: (`plugins`, `lsp`, `memory`, `model` — a model load, an LSP start) is not something to be pinged
#: about from a phone, and used to open a thread per session on its own.
WORKFLOW_SOURCES = frozenset({"team", "specialists", "swarm", "delegation", "heal", "claude-code"})

#: Re-exported for callers and tests that name the labels.
APPROVE_LABEL = cards.APPROVE_LABEL
APPROVE_ONCE_LABEL = cards.APPROVE_ONCE_LABEL
DENY_LABEL = cards.DENY_LABEL
ALL_ROUTINE_LABEL = cards.ALL_ROUTINE_LABEL
DENY_STOP_LABEL = cards.DENY_STOP_LABEL
UNAUTHORIZED = cards.UNAUTHORIZED
ALREADY = cards.ALREADY

DANGER_COLOR = cards.DANGER_COLOR
DONE_COLOR = cards.DONE_COLOR
INFO_COLOR = cards.INFO_COLOR


# --------------------------------------------------------------------------- config



def _orchestrator_status() -> dict[str, Any] | None:
    """The orchestrator's pools and handoffs from state.db (no model call); None when it cannot be read."""
    try:
        from orchestrator.status import status_data

        return status_data()
    except Exception:
        logger.debug("discord: orchestrator status unavailable", exc_info=True)
        return None


@dataclass
class RemoteConfig:
    """What `from_env` decided, and why. `enabled` is false for every reason this module refuses to
    run: no token, no allowlist, an id that is not an id, or an explicit `SIERRALAB_DISCORD=off`."""

    enabled: bool = False
    reason: str = ""
    token: str = ""
    allowed_user_id: int = 0
    channel_id: int = 0

    @classmethod
    def from_env(cls, environ: Mapping[str, str] | None = None, *, token: str | None = None) -> "RemoteConfig":
        """Read the three names, and say no clearly rather than half-starting.

        `token` overrides the lookup (the smoke test's seam, and a caller that already holds one).
        Otherwise the token comes from `secret_guard.take_secret`, which pops it out of the
        environment on the way — an environment variable a tool subprocess can read is exactly the
        exposure `secret_guard` exists to prevent.
        """
        env = os.environ if environ is None else environ
        if str(env.get(KILL_ENV, "") or env.get(LEGACY_KILL_ENV, "")).strip().lower() in OFF:
            return cls(reason=f"{KILL_ENV} is off")
        if token is None:
            # `environ` is the smoke test's seam (and any caller that already read one). The real
            # lookup is the environment this process was launched with, then the Keychain — both
            # through `take_secret`, which pops the value out of `os.environ` and masks it in output.
            if environ is not None:
                token = str(env.get(TOKEN_ENV) or "")
            else:
                token = secret_guard.take_secret(TOKEN_ENV, keychain=KEYCHAIN_ITEMS).reveal() or ""
        secret_guard.register_secret(token)
        ids: dict[str, int] = {}
        for name in (ALLOWED_ENV, CHANNEL_ENV):
            raw = str(env.get(name, "")).strip()
            if not raw:
                ids[name] = 0
                continue
            try:
                ids[name] = int(raw)
            except (TypeError, ValueError):
                return cls(reason=f"{name} is not an integer ({raw!r})")
        if not token:
            return cls(reason=f"no {TOKEN_ENV} in the environment or the Keychain item {KEYCHAIN_ITEM!r}")
        if not ids[ALLOWED_ENV]:
            # Deliberately fatal to the bot, not to the daemon: a front door nobody can be excluded
            # from is worse than no front door, because it looks like one.
            return cls(reason=f"no {ALLOWED_ENV}, so no click could be told from any other")
        if not ids[CHANNEL_ENV]:
            return cls(reason=f"no {CHANNEL_ENV} to post in")
        return cls(enabled=True, reason="configured", token=token, allowed_user_id=ids[ALLOWED_ENV], channel_id=ids[CHANNEL_ENV])


def destructive_line(cwd: str | None) -> float:
    """Laya's destructive line for this project, `laya.riskDestroy`. Lazy import: the daemon does not
    otherwise build a settings view, and `settings_store` is the one thing that knows the three file
    scopes. Any failure falls back to the directive's 0.8 — this only decides what to *show*."""
    if not cwd:
        return DEFAULT_DESTROY
    try:
        import settings_store  # noqa: PLC0415 — not imported at daemon start (see the docstring)

        return float(settings_store.current(cwd).get("laya.riskDestroy", DEFAULT_DESTROY))
    except Exception:
        logger.debug("discord: could not read laya.riskDestroy for %s", cwd, exc_info=True)
        return DEFAULT_DESTROY


def _chat_store() -> Path | None:
    from .chat import chat_store  # noqa: PLC0415

    return chat_store()


# --------------------------------------------------------------------------- small state records


@dataclass
class _Thread:
    """A terminal/GUI session's task thread, plus the little state a coalescing poster needs."""

    channel: Any
    session_id: str = ""
    last_post: float = 0.0
    dropped: int = 0


@dataclass
class _Card:
    """One approval message, so it can be collapsed when another front door answers first."""

    message: Any
    meta: Mapping[str, Any]
    claimed: bool = False  # a click on this card is handling it: the resolved listener must stay out of the way


class TurnView:
    """One chat turn as the person sees it: a single message that is a placeholder, then the reply as it
    arrives (edited at most every `EDIT_INTERVAL`), then the reply in full with its receipt.

    The `Sink` the conversation drives. Its callbacks are synchronous and only mark the view dirty;
    the one flusher task does the I/O, so a burst of chunks is one edit, not forty.
    """

    LIVE_TAIL = 1500

    def __init__(self, remote: "DiscordRemote", channel: Any) -> None:
        self.remote = remote
        self.channel = channel
        self.message: Any = None
        self.buffer = ""
        self.label: str | None = None
        self._dirty = False
        self._flusher: asyncio.Task | None = None
        self._closed = False

    async def start(self) -> None:
        self.message = await self.remote._send(self.channel, content="⏳ on it…")

    # -- the Sink ----------------------------------------------------------------

    def text(self, delta: str) -> None:
        self.buffer += delta
        self._kick()

    def activity(self, label: str | None) -> None:
        self.label = label
        self._kick()

    def _kick(self) -> None:
        if self._closed or self.message is None:
            return
        self._dirty = True
        if self._flusher is None or self._flusher.done():
            self._flusher = asyncio.create_task(self._flush(), name="discord-turn-edit")

    async def _flush(self) -> None:
        while self._dirty and not self._closed:
            self._dirty = False
            await self.remote._edit(self.message, content=self._live())
            await asyncio.sleep(EDIT_INTERVAL)

    def _live(self) -> str:
        shown = cards.discord_markdown(self.buffer)
        body = shown if len(shown) <= self.LIVE_TAIL else "…" + shown[-self.LIVE_TAIL :]
        head = f"🔧 {cards.clip(self.label, 120)}" if self.label and self.label != "thinking" else ("💭 thinking…" if not body else "")
        if not body:
            return head or "⏳ on it…"
        return (f"{head}\n\n" if head else "") + body + " ▍"

    # -- the ends ----------------------------------------------------------------

    async def finish(self, text: str, footer: str) -> None:
        await self._stop()
        chunks = cards.split_message(cards.discord_markdown(text)) or ["(no reply)"]
        chunks[-1] = f"{chunks[-1]}\n\n-# {footer}" if footer else chunks[-1]
        if self.message is not None:
            await self.remote._edit(self.message, content=chunks[0])
        else:
            await self.remote._send(self.channel, content=chunks[0])
        for chunk in chunks[1:]:
            await self.remote._send(self.channel, content=chunk)

    async def fail(self, reason: str) -> None:
        await self._stop()
        line = f"⚠ {reason}"
        if self.message is not None:
            await self.remote._edit(self.message, content=line)
        else:
            await self.remote._send(self.channel, content=line)

    async def _stop(self) -> None:
        self._closed = True
        flusher, self._flusher = self._flusher, None
        if flusher is not None and not flusher.done():
            flusher.cancel()
            with contextlib.suppress(asyncio.CancelledError, Exception):
                await flusher


# --------------------------------------------------------------------------- the bot


class DiscordRemote:
    """The daemon's Discord client, its supervisor listeners, and the registries behind them.

    One instance per daemon, held by `daemon.server.State` so `/health` can report it. `start()` is
    the only place `discord` is imported, and it is a no-op when nothing is configured.
    """

    def __init__(self, supervisor: Any, *, environ: Mapping[str, str] | None = None, token: str | None = None, chats: Chats | None = None) -> None:
        self.supervisor = supervisor
        self._environ = environ
        self._token_override = token
        #: Until `start()` runs, the honest reason is that nothing has been read — a bot that was
        #: never started must not report a configuration it never had.
        self.config: RemoteConfig = RemoteConfig(reason="not started")
        self._chats: Chats | None = chats
        self._task: asyncio.Task | None = None
        self._reaper: asyncio.Task | None = None
        self._client: Any = None
        self._discord: Any = None
        self._connected = False
        self._unsubscribes: list[Any] = []
        #: Terminal/GUI sessions' task threads: session id -> thread, and thread id -> session id, so a
        #: reply in one knows which session it steers without the person typing an id.
        self._threads: dict[str, _Thread] = {}
        self._by_channel: dict[int, str] = {}
        #: Approval cards by (session id, request key), and chat turns by conversation key.
        self._cards: dict[tuple[str, str], _Card] = {}
        self._turns: dict[str, TurnView] = {}
        self._tasks: dict[str, tuple[str, str, float]] = {}  # session id -> (title, state, when) of the newest task
        self._announced: Any = None
        self._explained: set[int] = set()  # threads that have been told once what steering does
        #: Threads that follow a hub conversation, so a turn another door runs there is posted in them:
        #: (conversation key, thread id) -> unwatch.
        self._mirrors: dict[tuple[str, int], Callable[[], None]] = {}

    @property
    def chats(self) -> Chats:
        """The conversations — built on first use, so a daemon with no bot configured never reads (or
        creates) a chat store."""
        if self._chats is None:
            self._chats = Chats(self.supervisor, store=_chat_store())
        return self._chats

    # ------------------------------------------------------------- lifecycle

    def start(self) -> asyncio.Task | None:
        """Configure, import Discord, and run the client as a background task.

        Never raises: every refusal (no token, bad id, no `discord.py`, privileged intents off) is a
        logged reason and an inert bot. The daemon's session service must not depend on this.
        """
        self.config = RemoteConfig.from_env(self._environ, token=self._token_override)
        if not self.config.enabled:
            logger.info("discord: not started (%s)", self.config.reason)
            return None
        try:
            import discord  # noqa: PLC0415 — deliberate: a checkout without it is unaffected
        except ImportError as error:
            self.config.enabled = False
            self.config.reason = f"discord.py is not installed ({error})"
            logger.info("discord: %s", self.config.reason)
            return None
        self._discord = discord
        intents = discord.Intents.default()
        # Reading a message is reading message text, which Discord makes a privileged intent: the
        # toggle in the Developer Portal is a prerequisite, and `PrivilegedIntentsRequired` below is
        # what a bot without it gets.
        intents.message_content = True
        self._client = discord.Client(intents=intents)
        self._bind()
        self._task = asyncio.create_task(self._run(), name="discord")
        return self._task

    def _bind(self) -> None:
        client = self._client

        @client.event
        async def on_ready() -> None:  # noqa: ANN202 — discord.py's own signature
            self._connected = True
            logger.info("discord: connected as %s", getattr(client.user, "name", "?"))
            await self._announce()
            if self._reaper is None or self._reaper.done():
                self._reaper = asyncio.create_task(self._reap_loop(), name="discord-reaper")

        @client.event
        async def on_message(message) -> None:  # noqa: ANN001
            await self._on_message(message)

        # The supervisor's listeners fire on the event loop, so they schedule rather than await;
        # a listener that raised would be swallowed by `Supervisor._notify` and the prompt lost.
        self.chats.start()
        self._mirror_all()
        self._unsubscribes = [
            self.supervisor.listen("intervention", self._on_intervention),
            self.supervisor.listen("task", self._on_task),
            self.supervisor.listen("laya", self._on_laya),
        ]

    async def _run(self) -> None:
        try:
            await self._client.start(self.config.token)
        except Exception as error:  # PrivilegedIntentsRequired, LoginFailure, a dropped gateway
            name = type(error).__name__
            hint = " — enable Message Content Intent in the Discord Developer Portal" if "PrivilegedIntents" in name else ""
            logger.warning("discord: client stopped: %s: %s%s", name, error, hint)
        finally:
            self._connected = False

    async def _reap_loop(self) -> None:
        while True:
            await asyncio.sleep(REAP_EVERY)
            try:
                await self.chats.reap_idle()
            except Exception:
                logger.debug("discord: reaping failed", exc_info=True)

    async def stop(self) -> None:
        for unwatch in self._mirrors.values():
            try:
                unwatch()
            except Exception:
                logger.debug("discord: unwatching failed", exc_info=True)
        self._mirrors = {}
        for unsubscribe in self._unsubscribes:
            try:
                unsubscribe()
            except Exception:
                logger.debug("discord: unsubscribing failed", exc_info=True)
        self._unsubscribes = []
        if self._chats is not None:
            await self._chats.stop()
        if self._reaper is not None and not self._reaper.done():
            self._reaper.cancel()
            with contextlib.suppress(asyncio.CancelledError, Exception):
                await self._reaper
        self._reaper = None
        if self._client is not None:
            try:
                await self._client.close()
            except Exception:
                logger.debug("discord: close failed", exc_info=True)
        if self._task is not None and not self._task.done():
            self._task.cancel()
            try:
                await asyncio.wait_for(self._task, timeout=5)
            except asyncio.CancelledError:
                pass  # this call is what cancelled it: the outcome asked for
            except asyncio.TimeoutError:
                logger.warning("discord: the client task did not stop in time")
            except Exception:
                logger.debug("discord: the client task ended badly", exc_info=True)
        self._task, self._connected = None, False

    def status(self) -> dict[str, Any]:
        """The `/health` block. Ids yes, token never — a token in a health payload is a token in
        whatever scrapes it."""
        return {
            "enabled": bool(self.config.enabled),
            "connected": self._connected,
            "reason": self.config.reason,
            "allowedUser": self.config.allowed_user_id or None,
            "channel": self.config.channel_id or None,
            "threads": len(self._threads),
            "chats": len(self._chats.all()) if self._chats is not None else 0,
            "liveChats": len(self._chats.live()) if self._chats is not None else 0,
        }

    # ------------------------------------------------------------- channel plumbing

    async def _channel(self, channel_id: int) -> Any:
        """A channel or thread by id: from cache when this process has seen it, fetched when it has not
        (a daemon started before the channel existed — or a thread opened by an earlier run — holds
        no object for it)."""
        get = getattr(self._client, "get_channel", None)
        channel = get(channel_id) if callable(get) else None
        if channel is not None:
            return channel
        fetch = getattr(self._client, "fetch_channel", None)
        try:
            return await fetch(channel_id) if callable(fetch) else None
        except Exception:
            logger.debug("discord: could not fetch channel %s", channel_id, exc_info=True)
            return None

    async def _telemetry(self) -> Any:
        return await self._channel(self.config.channel_id)

    def _no_mentions(self, *, operator: bool = False) -> Any:
        allowed = getattr(self._discord, "AllowedMentions", None)
        if allowed is None:
            return None
        return allowed(users=True, roles=False, everyone=False) if operator else allowed.none()

    async def _send(self, channel: Any, *, content: str | None = None, embed: Any = None, view: Any = None, ping: bool = False) -> Any:
        """Send, never raise. Text is secret-masked and cannot mention anyone but the operator."""
        kwargs: dict[str, Any] = {}
        if content is not None:
            kwargs["content"] = secret_guard.redact(content)[:2000]
        if embed is not None:
            kwargs["embed"] = embed
        if view is not None:
            kwargs["view"] = view
        mentions = self._no_mentions(operator=ping)
        if mentions is not None:
            kwargs["allowed_mentions"] = mentions
        try:
            return await channel.send(**kwargs)
        except Exception:
            logger.debug("discord: send failed", exc_info=True)
            return None

    async def _edit(self, message: Any, **kwargs: Any) -> None:
        if message is None:
            return
        if isinstance(kwargs.get("content"), str):
            kwargs["content"] = secret_guard.redact(kwargs["content"])[:2000]
        mentions = self._no_mentions()
        if mentions is not None and "content" in kwargs:
            kwargs["allowed_mentions"] = mentions
        try:
            await message.edit(**kwargs)
        except Exception:
            logger.debug("discord: edit failed", exc_info=True)

    def _embed(self, spec: Mapping[str, Any]) -> Any:
        embed = self._discord.Embed(
            title=secret_guard.redact(str(spec.get("title") or ""))[:256],
            description=secret_guard.redact(str(spec.get("description") or ""))[:4000],
            color=spec.get("color", INFO_COLOR),
        )
        for name, value, inline in spec.get("fields") or []:
            embed.add_field(name=secret_guard.redact(str(name))[:256] or "​", value=secret_guard.redact(str(value))[:1024] or "​", inline=bool(inline))
        if spec.get("footer") and hasattr(embed, "set_footer"):
            embed.set_footer(text=secret_guard.redact(str(spec["footer"]))[:2000])
        return embed

    async def _react(self, message: Any, emoji: str, fallback: str) -> None:
        try:
            await message.add_reaction(emoji)
        except Exception:
            logger.debug("discord: reaction failed", exc_info=True)
            await self._send(message.channel, content=fallback)

    def _typing(self, channel: Any) -> Any:
        factory = getattr(channel, "typing", None)
        return factory() if callable(factory) else contextlib.nullcontext()

    async def _announce(self) -> None:
        """`Daemon connected` — once per daemon run. A gateway reconnect fires `on_ready` again, and a
        fresh message each time (the old behaviour) turned a flaky connection into channel spam; the
        first message is edited instead."""
        try:
            channel = await self._telemetry()
            if channel is None:
                logger.warning("discord: telemetry channel %s is not visible to this bot", self.config.channel_id)
                return
            sessions = [s for s in self.supervisor.sessions() if not s.get("owner")]
            embed = self._embed(
                {
                    "title": "Daemon connected",
                    "description": f"Operator <@{self.config.allowed_user_id}> — write here to talk to an agent; `!help` for the rest.",
                    "fields": [
                        ("Terminal/GUI sessions", "\n".join(f"`{cards.project_name(s['cwd'])}` {s['cwd']}" for s in sessions) or "none yet", False),
                        ("Approvals", "Only calls that need a person reach this channel.", True),
                        ("Where chats run", f"`{default_cwd(self.supervisor)}`", True),
                    ],
                    "color": INFO_COLOR,
                    "footer": f"connected {time.strftime('%H:%M')}",
                }
            )
            if self._announced is not None:
                await self._edit(self._announced, embed=embed)
            else:
                self._announced = await self._send(channel, embed=embed)
        except Exception:
            logger.debug("discord: announce failed", exc_info=True)

    # ------------------------------------------------------------- HITL

    def _on_intervention(self, payload: Mapping[str, Any]) -> None:
        """A pending permission appeared, or was settled. Posting rules, in one place:

        - A session a *chat* owns has no GUI or terminal to answer it, so every ask reaches Discord
          (in that chat's thread) — routine ones quietly, armed ones with a ping.
        - A session a terminal or the GUI is driving is posted only at the project's destructive
          line, the same rule the GUI's modal follows (`>= 0.8` gets the modal, the rest a tray
          row), so the two front doors agree about what is urgent.
        """
        if not self._connected:
            return
        event, pending = payload.get("event"), payload.get("pending")
        if pending is None:
            return
        if event == "resolved":
            asyncio.create_task(self._close_card(pending, str(payload.get("outcome") or ""), str(payload.get("decided_by") or "")), name="discord-card-close")
            return
        if event != "requested":
            return
        meta = getattr(pending, "meta", None) or {}
        conv = self.chats.for_session(str(getattr(pending, "session_id", "")))
        if conv is None:
            try:
                score = float(meta.get("risk_score"))
            except (TypeError, ValueError):
                return  # unreadable score: not a thing to ask a person about from here
            line = destructive_line(self._cwd(getattr(pending, "session_id", "")))
            if score < line:
                return
        asyncio.create_task(self._post_intervention(pending, meta, conv), name="discord-approval")

    def _cwd(self, session_id: str) -> str | None:
        session = self.supervisor.session(session_id) if session_id else None
        return getattr(session, "cwd", None)

    @staticmethod
    def _always_rule(pending: Any) -> str:
        """The rule the child offered to remember ("shell(pytest *)"), or "" when it offered none —
        which is always the case for a call the gate armed."""
        for option in getattr(pending, "options", None) or []:
            if option.get("kind") == "allow_always":
                return re.sub(r"^Always allow\s+", "", str(option.get("name") or "")).strip() or "this call"
        return ""

    async def _post_intervention(self, pending: Any, meta: Mapping[str, Any], conv: Conversation | None) -> None:
        session_id = str(getattr(pending, "session_id", ""))
        key = str(getattr(pending, "key", ""))
        try:
            channel = (await self._channel(int(conv.key)) if conv is not None else None) or await self._telemetry()
            routine = not bool(meta.get("armed"))
            cwd = meta.get("project") or self._cwd(session_id) or ""
            where = f"{cwd or '?'} · {'chat' if conv is not None else 'session'} · pod {meta.get('pod') or 'orchestrator'}"
            rule = self._always_rule(pending)
            scratch = conv is None and is_scratch(str(cwd))
            card = cards.approval_card(meta, project=cards.project_name(cwd), where=where, routine=routine, offers_always=rule, redactions=int(meta.get("redactions") or 0), scratch=scratch, can_stop=conv is not None and not routine)
            view = self._build_view(session_id, key, pending, conv=conv, routine=routine, always=bool(rule))
            sent = None
            if channel is not None:
                ping = not routine and not scratch
                sent = await self._send(channel, content=f"<@{self.config.allowed_user_id}> needs a decision" if ping else None, embed=self._embed(card), view=view, ping=ping)
            if sent is None:
                raise RuntimeError("the approval could not be delivered")
            self._cards[(session_id, key)] = _Card(message=sent, meta=meta)
            logger.info("discord: approval %s/%s posted (%s, score %s)", session_id, key, "routine" if routine else "armed", meta.get("risk_score"))
        except Exception:
            logger.warning("discord: could not post approval %s/%s", session_id, key, exc_info=True)
            if conv is not None:
                # A chat has no other front door. Left alone, its child would sit on this prompt for as
                # long as the daemon lives; waving it through is not on offer. Denying is safe.
                with contextlib.suppress(LookupError, ValueError):
                    await self.supervisor.answer_permission(session_id, key, False, decided_by=f"{OWNER}:undeliverable")

    def _build_view(self, session_id: str, key: str, pending: Any, *, conv: Conversation | None, routine: bool, always: bool) -> Any:
        """Allow / (Always allow) / (Allow routine steps in this chat) / Deny, as one `discord.ui.View`.

        Built from items rather than a decorated class so the set can vary with the card — the
        "routine steps" button is offered only on a routine ask in a chat, never on an armed one.
        `timeout=None` on purpose: a decision that arrives an hour later is still the right decision,
        the child is still parked on it."""
        discord = self._discord
        view = discord.ui.View(timeout=None)
        view.session_id, view.key, view.pending, view.conv = session_id, key, pending, conv

        def add(label: str, style: Any, action: str) -> None:
            button = discord.ui.Button(label=label, style=style)

            async def callback(interaction: Any) -> None:
                await self._click(interaction, view, action)

            button.callback = callback
            view.add_item(button)

        add(APPROVE_LABEL if routine else APPROVE_ONCE_LABEL, discord.ButtonStyle.success, "allow")
        if always:
            add("Always allow", discord.ButtonStyle.secondary, "always")
        if conv is not None and routine:
            add(ALL_ROUTINE_LABEL, discord.ButtonStyle.primary, "auto")
        add(DENY_LABEL, discord.ButtonStyle.danger, "deny")
        if conv is not None and not routine:
            # A destructive ask in a chat: besides "no, try another way", "no, and stop here" — the
            # turn is cancelled too. (Only a chat: a terminal session is stopped from where it is driven.)
            add(DENY_STOP_LABEL, discord.ButtonStyle.danger, "stop")
        return view

    async def _click(self, interaction: Any, view: Any, action: str) -> None:
        """One button press.

        The allowlist is checked *first* and returns before anything else happens: it is the only
        thing standing between an open Discord server and a daemon that runs shell commands, and a
        check that runs after a state change is not a check.
        """
        user_id = int(getattr(getattr(interaction, "user", None), "id", 0) or 0)
        if user_id != self.config.allowed_user_id:
            logger.warning("discord: refusing %s from user %s (not the configured operator)", action, user_id)
            await self._ephemeral(interaction, UNAUTHORIZED)
            return
        card = self._cards.get((view.session_id, view.key))
        if card is not None:
            card.claimed = True
        try:
            if action == "auto" and view.conv is not None:
                # First, so the routine calls that follow do not ask: the mode is the thing that ends
                # the questions. If it cannot be set the card stays live — the person can still Allow.
                try:
                    await self.chats.set_mode(view.conv.key, "auto")
                except ChatError as error:
                    if card is not None:
                        card.claimed = False
                    await self._ephemeral(interaction, str(error))
                    return
            if action == "stop" and view.conv is not None and await self.chats.cancel(view.conv.key):
                pass  # `cancel` denied this pending call itself, then cancelled the turn
            else:
                await self.supervisor.answer_permission(view.session_id, view.key, action not in ("deny", "stop"), decided_by="discord", always=action == "always")
        except LookupError:
            # Answered by the other front door first, or the session ended. Not an error: the two
            # doors race by design, and `_settle` is what makes the loser harmless.
            await self._ephemeral(interaction, ALREADY)
            return
        except ValueError as error:
            if card is not None:
                card.claimed = False
            await self._ephemeral(interaction, f"The session did not offer that option: {error}")
            return
        verdict = {"deny": "rejected", "allow": "approved", "always": "always", "auto": "auto", "stop": "stopped"}[action]
        logger.info("discord: %s %s/%s", verdict, view.session_id, view.key)
        meta = getattr(view.pending, "meta", None) or {}
        try:
            await interaction.response.edit_message(
                content=None,
                embed=self._embed(cards.resolved_card(meta, verdict=verdict, by=f"by <@{self.config.allowed_user_id}>")),
                view=None,
            )
        except Exception:
            logger.debug("discord: could not edit the approval message", exc_info=True)
        self._cards.pop((view.session_id, view.key), None)

    async def _close_card(self, pending: Any, outcome: str, decided_by: str) -> None:
        """Another front door answered (or the session ended): collapse the card instead of leaving a
        live pair of buttons on a question nobody is waiting on."""
        card = self._cards.get((str(getattr(pending, "session_id", "")), str(getattr(pending, "key", ""))))
        if card is None or card.claimed:
            return
        self._cards.pop((str(getattr(pending, "session_id", "")), str(getattr(pending, "key", ""))), None)
        door = {"gui": "the GUI", "tui": "the terminal", "session-ended": "the session ending", "discord": "a chat stop"}.get(decided_by, decided_by or "another front door")
        verdict = outcome if outcome in ("approved", "rejected") else "moot"
        await self._edit(card.message, content=None, embed=self._embed(cards.resolved_card(card.meta, verdict=verdict, by=f"answered from {door}")), view=None)

    @staticmethod
    async def _ephemeral(interaction: Any, text: str) -> None:
        try:
            await interaction.response.send_message(text, ephemeral=True)
        except Exception:
            logger.debug("discord: ephemeral reply failed", exc_info=True)

    # ------------------------------------------------------------- terminal/GUI sessions' task threads

    def _on_task(self, payload: Mapping[str, Any]) -> None:
        """A task bar moved. Remembered (it is what "what are you working on?" answers from); only a
        workflow source in a session a person is driving opens a thread — a chat's steps show on its own
        live message, and a chat turn's chatter stays out of Discord entirely."""
        session_id = str(payload.get("session_id") or payload.get("sessionId") or "")
        state = str(payload.get("state") or "running")
        title = str(payload.get("title") or "")
        if session_id and title:
            self._tasks[session_id] = (title, state, time.time())
        conv = self.chats.for_session(session_id)
        if conv is not None:
            view = self._turns.get(conv.key)
            if view is not None and state == "running":
                view.activity(title)
            return
        if str(payload.get("source") or "") not in WORKFLOW_SOURCES:
            return
        asyncio.create_task(self._handle_task(payload), name="discord-task")

    def _on_laya(self, payload: Mapping[str, Any]) -> None:
        """Laya's reading of a turn, into that session's thread. High volume by nature (one per
        routed turn), so only a destructive band is worth a line — the rest is what the TUI's live
        `◆ laya` strip is for."""
        thread = self._threads.get(str(payload.get("session_id") or ""))
        if thread is None:
            return
        band = str((payload.get("risk") or {}).get("band") or payload.get("band") or "")
        if band != "destructive":
            return
        asyncio.create_task(self._post(thread, f"◆ laya: {payload.get('route') or payload.get('decision') or 'turn'} — {band}"), name="discord-laya")

    async def _open_thread(self, session_id: str, title: str) -> None:
        if session_id in self._threads:
            return
        try:
            channel = await self._telemetry()
            if channel is None:
                return
            kind = getattr(getattr(self._discord, "ChannelType", None), "public_thread", None)
            thread = await (channel.create_thread(name=cards.thread_title(title, prefix="🧵 "), type=kind) if kind is not None else channel.create_thread(name=cards.thread_title(title, prefix="🧵 ")))
            self._threads[session_id] = _Thread(channel=thread, session_id=session_id)
            self._by_channel[int(getattr(thread, "id", 0) or 0)] = session_id
            await self._send(thread, content=f"🧵 Following **{cards.project_name(self._cwd(session_id))}** (a terminal/GUI session). Reply here to steer it; `!status` for where it stands.")
        except Exception:
            logger.debug("discord: opening a thread failed", exc_info=True)

    async def _handle_task(self, payload: Mapping[str, Any]) -> None:
        """Open the session's thread on the first running step, then post this line into it — in that
        order, so the step that opened the thread is also the first thing in it."""
        session_id = str(payload.get("session_id") or payload.get("sessionId") or "")
        state = str(payload.get("state") or "running")
        if self._threads.get(session_id) is None and state == "running":
            await self._open_thread(session_id, str(payload.get("title") or "task"))
        thread = self._threads.get(session_id)
        if thread is None:
            return
        title = str(payload.get("title") or "")
        message = str(payload.get("message") or "")
        suffix = f"\n{message}" if message else ""
        await self._post(thread, f"{'✔' if state == 'completed' else '✖' if state != 'running' else '•'} {title}{suffix}")

    async def _post(self, thread: _Thread, text: str) -> None:
        """Coalesce: a running step reports its start, its end, and at most one line in between per
        `MIN_POST_INTERVAL`. Dropped lines are counted and reported, so a quiet thread is never
        mistaken for a quiet agent."""
        now = time.monotonic()
        terminal = not text.startswith("•")
        if not terminal and now - thread.last_post < MIN_POST_INTERVAL:
            thread.dropped += 1
            return
        line = " ".join(text.split())
        if thread.dropped:
            line = f"{line} (+{thread.dropped} update(s) folded in)"
            thread.dropped = 0
        thread.last_post = now
        await self._send(thread.channel, content=line[:1990])

    # ------------------------------------------------------------- messages

    async def _on_message(self, message: Any) -> None:
        """Route one message from the operator. Everyone else is dropped in silence (a reply would tell
        an unauthorized stranger the bot is listening).

        Where it goes depends on where it was written:

        - in a **chat thread** — a turn with that agent (or a steering note if it is mid-turn);
        - in a **terminal/GUI session's thread** — a steering note for that session;
        - in the **channel** — a status question is answered, `> steer:` steers the one running
          terminal session, and anything else opens a new chat thread.
        - a `!command` works in any of them.
        """
        author = getattr(message, "author", None)
        if getattr(author, "bot", False):
            return
        if int(getattr(author, "id", 0) or 0) != self.config.allowed_user_id:
            logger.warning("discord: dropping a message from user %s (not the configured operator)", getattr(author, "id", 0))
            return
        channel = getattr(message, "channel", None)
        channel_id = int(getattr(channel, "id", 0) or 0)
        chat = self.chats.get(str(channel_id))
        watched = self._by_channel.get(channel_id, "")
        lobby = channel_id == self.config.channel_id
        if not (chat or watched or lobby):
            return  # a channel or thread this bot has no business in
        content = self._clean(str(getattr(message, "content", "") or ""))
        if not content:
            return
        command = cards.parse_command(content)
        try:
            if command is not None:
                await self._command(message, command[0], command[1], chat)
            elif chat is not None:
                await self._chat_message(message, chat, content)
            elif watched:
                await self._steer_watched(message, watched, content)
            else:
                await self._lobby_message(message, content)
        except Exception:
            logger.exception("discord: handling a message failed")
            await self._send(channel, content="⚠ Something went wrong handling that. It is in the daemon log.")

    def _clean(self, content: str) -> str:
        """The text without the bot's own mention: "what are you working on? @my-agents" is the
        question, not a question *about* the mention."""
        bot_id = getattr(getattr(self._client, "user", None), "id", None)
        if bot_id:
            content = re.sub(rf"<@!?{int(bot_id)}>", "", content)
        return content.strip()

    # -- chat ----------------------------------------------------------------------

    async def _chat_message(self, message: Any, conv: Conversation, content: str) -> None:
        channel = message.channel
        if cards.is_status_question(content):
            await self._reply_status(channel)
            return
        if conv.busy:
            try:
                await self.chats.steer(conv.key, content)
            except ChatError as error:
                await self._send(channel, content=str(error))
                return
            await self._react(message, "📨", "📨 Noted — the agent reads it at its next step.")
            return
        await self._run_turn(channel, conv, content)

    async def _run_turn(self, channel: Any, conv: Conversation, text: str) -> None:
        view = TurnView(self, channel)
        await view.start()
        self._turns[conv.key] = view
        try:
            async with self._typing(channel):
                result = await self.chats.send(conv.key, text, view)
        except Busy:
            await view.fail("A turn is already running here — your message was not sent. Reply again when it finishes, or `!stop` it.")
            return
        except ChatError as error:
            await view.fail(str(error))
            return
        except Exception:
            logger.exception("discord: a chat turn failed")
            await view.fail("The turn failed unexpectedly. It is in the daemon log.")
            return
        finally:
            if self._turns.get(conv.key) is view:
                self._turns.pop(conv.key, None)
        await view.finish(result.text, cards.turn_footer(result.turn, stop=result.stop, seconds=result.seconds))
        for notice in result.notices:
            await self._send(channel, content=f"-# ⚠ {notice}")

    async def _make_chat(self, message: Any, content: str, *, cwd: str) -> tuple[Any, Conversation]:
        """A thread for a new chat, under the message that started it — or, if threads are unavailable
        to this bot (no permission), the channel itself as the one chat there is."""
        name = cards.thread_title(content)
        channel = message.channel
        thread = None
        try:
            create = getattr(message, "create_thread", None)
            thread = await create(name=name) if callable(create) else None
        except Exception:
            logger.debug("discord: could not open a thread under the message", exc_info=True)
        if thread is None:
            try:
                kind = getattr(getattr(self._discord, "ChannelType", None), "public_thread", None)
                thread = await channel.create_thread(name=name, type=kind) if kind is not None else await channel.create_thread(name=name)
            except Exception:
                logger.warning("discord: threads are unavailable here; this channel is now the one chat", exc_info=True)
                thread = channel
        conv = self.chats.open(str(thread.id), cwd=cwd, title=content[:80])
        self._mirror(conv, int(getattr(thread, "id", 0) or 0))
        await self._send(
            thread,
            content=f"-# 📁 `{cwd}` · mode **{conv.mode or default_mode(cwd)}** · conversation `{conv.handle}` (continue it in a terminal: `acp-tui --conversation {conv.handle}`) · reply here to continue · `!stop` cancels · `!mode auto` stops routine approval prompts · `!status` · `!help`",
        )
        return thread, conv

    # -- the conversation hub: one conversation, every door ----------------------------

    def _mirror_all(self) -> None:
        """Follow every conversation that has a thread here: the ones Discord opened (their key is the
        thread's id) and the ones a thread `!continue`d."""
        for conv in self.chats.all():
            threads = [conv.key] + self.chats.aliases_of(conv.key)
            for thread_id in threads:
                if thread_id.isdigit():
                    self._mirror(conv, int(thread_id))

    def _mirror(self, conv: Conversation, thread_id: int) -> None:
        """Post into `thread_id` each turn *another* door runs in this conversation, once it ends — the
        terminal's, the menu bar's, the web app's. A Discord turn renders itself as it streams."""
        if not thread_id or (conv.key, thread_id) in self._mirrors:
            return
        pending: dict[str, Any] = {}

        def on_event(event: dict[str, Any]) -> None:
            kind = event.get("event")
            if kind == "turn":
                pending.clear()
                if event.get("origin") != OWNER:
                    pending.update(origin=str(event.get("origin") or ""), asked=str(event.get("text") or ""), parts=[])
            elif kind == "text" and pending:
                pending["parts"].append(str(event.get("delta") or ""))
            elif kind == "done" and pending:
                turn = dict(pending)
                pending.clear()
                try:
                    asyncio.get_running_loop().create_task(self._post_mirrored(thread_id, turn, str(event.get("stop") or "")))
                except RuntimeError:
                    logger.debug("discord: no loop to post a mirrored turn on")

        self._mirrors[(conv.key, thread_id)] = self.chats.watch(conv.key, on_event)

    async def _post_mirrored(self, thread_id: int, turn: dict[str, Any], stop: str) -> None:
        channel = await self._channel(thread_id)
        if channel is None:
            return
        for part in cards.mirrored_turn(turn.get("origin", ""), turn.get("asked", ""), "".join(turn.get("parts") or []), stop):
            await self._send(channel, content=part)

    async def _thread_under(self, message: Any, name: str) -> Any:
        """A thread under `message`, or under the channel, or — when threads are unavailable — None."""
        try:
            create = getattr(message, "create_thread", None)
            thread = await create(name=name) if callable(create) else None
            if thread is not None:
                return thread
        except Exception:
            logger.debug("discord: could not open a thread under the message", exc_info=True)
        try:
            kind = getattr(getattr(self._discord, "ChannelType", None), "public_thread", None)
            return await message.channel.create_thread(name=name, type=kind) if kind is not None else await message.channel.create_thread(name=name)
        except Exception:
            logger.warning("discord: threads are unavailable here", exc_info=True)
            return None

    async def _cmd_conversations(self, channel: Any) -> None:
        await self._send(channel, content=cards.conversations_text(self.chats.rows(None, 10), time.time()))

    async def _cmd_continue(self, message: Any, arg: str, chat: Conversation | None) -> None:
        """`!continue <handle>`: pick up a conversation another door started (or one from an earlier
        thread) in a thread here. Messages in the thread are turns in *that* conversation, and turns
        other doors run in it are posted here as they finish."""
        channel = message.channel
        handle = arg.strip().split()[0] if arg.strip() else ""
        if not handle:
            await self._send(channel, content="`!continue <handle>` — `!conversations` lists them.")
            return
        conv = self.chats.resolve(handle)
        if conv is None:
            await self._send(channel, content=f"No conversation named `{handle}` — `!conversations` lists them.")
            return
        if chat is not None:
            if chat.key == conv.key:
                await self._send(channel, content=f"This thread already is `{conv.handle}`.")
            else:
                await self._send(channel, content=f"This thread is `{chat.handle}`. Use `!continue {conv.handle}` in the main channel to open a thread for that one.")
            return
        thread = await self._thread_under(message, cards.thread_title(f"↪ {conv.title or conv.handle}"))
        if thread is None:
            await self._send(channel, content="I can't open a thread here, so I can't continue it in Discord. It is still there in the terminal: `acp-tui --conversation " + conv.handle + "`.")
            return
        thread_id = int(getattr(thread, "id", 0) or 0)
        self.chats.bind(str(thread_id), conv.key)
        self._mirror(conv, thread_id)
        started = cards.door_label(conv.origin)
        await self._send(
            thread,
            content=f"-# ↪ continuing `{conv.handle}` (started from {started}, {conv.turns} turn{'s' if conv.turns != 1 else ''} so far) · 📁 `{conv.cwd}` · reply here to continue — turns from the other doors show up here too · `!stop` · `!help`",
        )

    async def _lobby_message(self, message: Any, content: str) -> None:
        channel = message.channel
        if cards.is_explicit_steer(content):
            sessions = [s["sessionId"] for s in self.supervisor.sessions() if s.get("alive") and not s.get("owner")]
            if len(sessions) == 1:
                await self._steer_watched(message, sessions[0], content)
            elif not sessions:
                await self._send(channel, content="No terminal or GUI session is running to steer. Just write a message to start a chat.")
            else:
                await self._send(channel, content=f"{len(sessions)} sessions are running — reply inside the thread of the one you mean.")
            return
        if cards.is_status_question(content):
            await self._reply_status(channel)
            return
        thread, conv = await self._make_chat(message, content, cwd=default_cwd(self.supervisor))
        await self._run_turn(thread, conv, content)

    # -- terminal/GUI sessions -------------------------------------------------------

    async def _steer_watched(self, message: Any, session_id: str, content: str) -> None:
        """A note for a session a terminal or the GUI is driving: the Lab's own guidance path
        (`_sierralab/lab/inject`), which is length-capped, queued and audited by length rather than
        content. It cannot start a turn — only the driver can — so it lands at the agent's next call."""
        channel = message.channel
        if cards.is_status_question(content):
            await self._reply_status(channel)
            return
        text = cards.steer_text(content)
        if not text:
            return
        directive = text[:MAX_STEER_CHARS]
        try:
            reply = await self.supervisor.call(session_id, "_sierralab/lab/inject", {"pod": "orchestrator", "text": directive})
        except (LookupError, asyncio.TimeoutError) as error:
            await self._send(channel, content=f"Could not reach that session: {error or 'timed out'}")
            return
        error = (reply or {}).get("error")
        if error:
            data = (error.get("data") if isinstance(error, dict) else None) or {}
            await self._send(channel, content=f"That session did not take the note: {data.get('error') or error}")
            return
        # Acknowledged with a reaction, and told once what it means — never echoed back, and never a
        # new message per note (the lab ledger records that a person steered, not a second copy).
        channel_id = int(getattr(channel, "id", 0) or 0)
        if channel_id not in self._explained:
            self._explained.add(channel_id)
            await self._send(channel, content="📨 Sent to the running session — it reads your note at its next step. (It can't answer in here; a terminal or the GUI is driving it. Start a chat in the channel to talk to a separate agent.)")
        else:
            await self._react(message, "📨", "📨 Sent.")

    # -- commands -------------------------------------------------------------------

    async def _command(self, message: Any, name: str, arg: str, chat: Conversation | None) -> None:
        channel = message.channel
        if name == "help":
            await self._send(channel, embed=self._embed(cards.help_card(self.config.allowed_user_id)))
        elif name == "status":
            await self._reply_status(channel)
        elif name == "usage":
            per_chat = [{"title": c.title or cards.project_name(c.cwd), "tokensIn": c.tokens_in, "tokensOut": c.tokens_out} for c in self.chats.all() if c.turns]
            await self._send(channel, embed=self._embed(cards.usage_card(self.chats.totals, per_chat)))
        elif name == "stop":
            await self._cmd_stop(message, chat)
        elif name == "mode":
            await self._cmd_mode(channel, arg, chat)
        elif name == "new":
            await self._cmd_new(message, arg)
        elif name == "back":
            await self._cmd_back(channel, arg, chat)
        elif name == "conversations":
            await self._cmd_conversations(channel)
        elif name == "continue":
            await self._cmd_continue(message, arg, chat)
        else:
            await self._send(channel, content=f"I don't know `!{arg}`. `!help` lists what I do.")

    async def _cmd_stop(self, message: Any, chat: Conversation | None) -> None:
        channel = message.channel
        target = chat
        if target is None:
            busy = [c for c in self.chats.all() if c.busy]
            if len(busy) == 1:
                target = busy[0]
            elif not busy:
                await self._send(channel, content="Nothing is running in a chat. (A terminal or GUI session is stopped from where it is driven.)")
                return
            else:
                await self._send(channel, content=f"{len(busy)} chats are working — use `!stop` inside the thread of the one you mean.")
                return
        if await self.chats.cancel(target.key):
            await self._react(message, "🛑", "🛑 Stopping — pending approvals are denied and the turn is cancelled.")
        else:
            await self._send(channel, content="That chat isn't running a turn.")

    async def _cmd_mode(self, channel: Any, arg: str, chat: Conversation | None) -> None:
        meanings = {
            "auto": "routine steps run on their own; calls Laya rates destructive still ask",
            "build": "the agent asks before it runs commands or edits files",
            "plan": "the agent only reads and plans; nothing is changed",
        }
        if chat is None:
            await self._send(channel, content="`!mode` applies to one chat — use it inside that chat's thread.")
            return
        wanted = arg.strip().lower()
        if not wanted:
            current = chat.mode or "the project's default"
            await self._send(channel, content=f"Mode here: **{current}**. Change it with `!mode auto|build|plan`.")
            return
        try:
            await self.chats.set_mode(chat.key, wanted)
        except ChatError as error:
            await self._send(channel, content=str(error))
            return
        await self._send(channel, content=f"Mode: **{wanted}** — {meanings[wanted]}.")

    async def _cmd_back(self, channel: Any, arg: str, chat: Conversation | None) -> None:
        if chat is None:
            await self._send(channel, content="`!back` applies to one chat — use it inside that chat's thread.")
            return
        parsed = cards.parse_back(arg)
        if parsed is None:
            await self._send(channel, content="`!back` (one turn) · `!back 2` · `!back list` · `!back to <number>`.")
            return
        try:
            if parsed == "list":
                text = cards.checkpoints_text(await self.chats.conversation(chat.key, "list"))
            else:
                text = cards.back_text(await self.chats.conversation(chat.key, "rewind", parsed))  # type: ignore[arg-type]
        except ChatError as error:
            await self._send(channel, content=str(error))
            return
        await self._send(channel, content=text)

    async def _cmd_new(self, message: Any, arg: str) -> None:
        channel = message.channel
        if int(getattr(channel, "id", 0) or 0) != self.config.channel_id:
            await self._send(channel, content="`!new` starts a chat — use it in the main channel.")
            return
        try:
            cwd = resolve_dir(arg) if arg else default_cwd(self.supervisor)
        except ChatError as error:
            await self._send(channel, content=str(error))
            return
        thread, _ = await self._make_chat(message, f"chat in {cards.project_name(cwd)}", cwd=cwd)
        await self._send(thread, content="Ready — say what you'd like done here.")

    # -- status -----------------------------------------------------------------------

    def _status_rows(self) -> list[dict[str, Any]]:
        """One row per live session, from state the daemon already holds: the session registry, the
        Lab's pods, the newest task bar and the pending approvals. No model is asked anything."""
        labs = self.supervisor.lab_states()
        now = time.time()
        rows: list[dict[str, Any]] = []
        for entry in self.supervisor.sessions():
            if not entry.get("alive"):
                continue
            sid = entry["sessionId"]
            session = self.supervisor.session(sid)
            conv = self.chats.for_session(sid)
            lab = labs.get(sid) or {}
            pods = lab.get("pods") or []
            pod = next((p for p in pods if p.get("name") == "orchestrator"), pods[0] if pods else {})
            pending = len(getattr(session, "pending", None) or {})
            busy = bool(conv and conv.busy) or pod.get("status") in ("synthesizing", "executing")
            task = self._tasks.get(sid)
            detail = pod.get("pending_tool") or (task[0] if task and task[1] == "running" else "")
            since = (now - float(pod["since"])) if pod.get("since") and busy else ((time.monotonic() - conv.started) if conv and conv.busy else 0)
            rows.append(
                {
                    "project": cards.project_name(entry.get("cwd")),
                    "state": "waiting" if pending else ("working" if busy else "idle"),
                    "detail": detail,
                    "since": since if busy else 0,
                    "pending": pending,
                    "kind": "chat" if conv is not None else "watched",
                    "mission": lab.get("mission") or "",
                }
            )
        return rows

    async def _reply_status(self, channel: Any) -> None:
        asleep = sum(1 for c in self.chats.all() if not c.daemon_id)
        note = f"{asleep} earlier chat{'s' if asleep != 1 else ''} asleep — reply in {'their threads' if asleep != 1 else 'its thread'} to wake {'them' if asleep != 1 else 'it'}." if asleep else ""
        await self._send(channel, embed=self._embed(cards.status_card(self._status_rows(), usage=self.chats.totals, note=note, quota=_orchestrator_status())))

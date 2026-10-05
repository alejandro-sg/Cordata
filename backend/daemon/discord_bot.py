"""The daemon's outbound Discord front door: remote HITL, telemetry and steering.

What this is for. The gate pauses a call at Laya's destructive line by raising a Strands interrupt,
which `acp_server._resolve_interrupts` turns into an ACP `session/request_permission`
(`acp_server.py:1672`). The daemon taps that into a `PendingPermission` (`supervisor.py:255`) and
waits. With a GUI or TUI attached, a person answers it. With nothing attached, *nobody* does, and the
child stays parked on that await for as long as the daemon lives. This module closes that hole by
being a third front door that lives where the person is not: it posts the paused call to Discord with
an approve/reject pair, and relays the click back through `Supervisor.answer_permission` — the same
relay the GUI's containment modal uses, so whichever door answers first wins and the other is told its
prompt is moot (`Supervisor._settle` refuses the second answer; `_sierralab/resolved` dismisses a
modal that is still open).

Four properties this module is built to hold.

- **Outbound only.** One WebSocket to Discord's gateway, no inbound port, no webhook. Nothing on this
  machine becomes reachable because this is running.
- **Zero trust on the caller.** `ALLOWED_USER_ID` is an allowlist of exactly one. A click or a message
  from anyone else is dropped *before* anything is touched — a stranger's click is not a decision, it
  is noise, and it earns an ephemeral refusal and no state change. With no `ALLOWED_USER_ID` parsed the
  bot does not start at all: a front door with no allowlist is an unauthenticated one.
- **The token is never a setting.** `DISCORD_BOT_TOKEN` is read through `secret_guard.take_secret`
  (environment first, then the macOS Keychain item `sierralab-discord-bot-token`), which also takes it
  out of `os.environ` so no tool subprocess can read it, and registers it for output masking. Compare
  `vault_proxy.KEYCHAIN_SERVICE`: the harness's own credentials live in the Keychain, never in a
  settings file — and never in this schema, which is why `discord.*` has no settings keys.
- **Fail quiet.** A missing token, an unparsable id, a privileged-intent refusal or a Discord outage
  all disable or degrade this module and are logged; none of them may take the daemon down, and none
  of them fall back to anything less safe than not asking.

Off unless configured: with no `DISCORD_BOT_TOKEN` in reach, `start()` returns immediately and the
daemon behaves exactly as it did before this module existed. `discord` itself is imported inside
`start()` for the same reason — a checkout that has not installed it is unaffected.

Scope, stated plainly: this is a *front door*, not a second policy engine. It shows what the gate
already decided to ask about, and it cannot approve anything the gate did not offer
(`answer_permission` only ever names an option the child itself listed). Steering text takes the
existing Lab guidance path (`lab.inject` / `lab.take_guidance`), which is length-capped, queued, and
audited by length rather than content.
"""

from __future__ import annotations

import asyncio
import logging
import os
import time
from dataclasses import dataclass, field
from typing import Any, Mapping

import secret_guard

logger = logging.getLogger(__name__)

#: The three things this module reads. Two of them are ids, not credentials; the token is a
#: credential and is handled by `secret_guard` (see the module docstring).
TOKEN_ENV = "DISCORD_BOT_TOKEN"
ALLOWED_ENV = "ALLOWED_USER_ID"
CHANNEL_ENV = "TELEMETRY_CHANNEL_ID"

#: An escape hatch that needs no edit to the environment it was launched with: `SIERRALAB_DISCORD=off`
#: keeps the token in place and the bot down (a person debugging the daemon should not have to delete a
#: credential to stop it talking).
KILL_ENV = "SIERRALAB_DISCORD"
OFF = ("0", "off", "false", "no")

#: The Keychain item holding the bot token, if it is not in the environment:
#: `security add-generic-password -a "$USER" -s sierralab-discord-bot-token -w` (or `make discord-key`).
KEYCHAIN_ITEM = "sierralab-discord-bot-token"

#: The directive's destructive line, and `laya.riskDestroy`'s default. Read per project so a session
#: that raised or lowered its own line is asked about at the line it actually set.
DEFAULT_DESTROY = 0.8

#: How much of a command or diff fits in one embed field (Discord caps a field at 1024 characters;
#: a fenced block with a truncation marker stays inside that with room for the other fields).
MAX_BODY_CHARS = 900
#: What a steering directive may carry. The Lab caps at 2000 and `acp_server._lab_control` audits
#: the length, so this refuses to send anything the far side would silently cut.
MAX_STEER_CHARS = 2000
#: Discord's own limit on a thread name.
MAX_NAME_CHARS = 90
#: One post per thread per this many seconds. A task bar updates at 20 Hz while it runs; all of it
#: in a thread would be unusable, and what matters remotely is that a step started and that it ended,
#: so running updates are coalesced and terminal states always go through.
MIN_POST_INTERVAL = 1.5

#: Which `harness_events.Task` sources mean "a multi-step workflow" — the thing worth its own thread —
#: rather than a chat turn's chatter. `heal`, `plugins`, `lsp`, `memory` and `model` carry model
#: load/unload and sandbox work; `team` and `specialists` are the multi-agent paths.
WORKFLOW_SOURCES = frozenset(
    {"team", "specialists", "swarm", "delegation", "heal", "claude-code", "plugins", "lsp", "memory", "model"}
)

APPROVE_LABEL = "🟢 Approve & Execute"
REJECT_LABEL = "🔴 Reject & Isolate"
UNAUTHORIZED = "Not your call: this daemon only accepts decisions from its configured operator."
ALREADY = "Already decided (or the session ended) — nothing to do."

DANGER_COLOR = 0xE74C3C
DONE_COLOR = 0x2ECC71
INFO_COLOR = 0x3498DB


# --------------------------------------------------------------------------- config


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
        if str(env.get(KILL_ENV, "")).strip().lower() in OFF:
            return cls(reason=f"{KILL_ENV} is off")
        if token is None:
            # `environ` is the smoke test's seam (and any caller that already read one). The real
            # lookup is the environment this process was launched with, then the Keychain — both
            # through `take_secret`, which pops the value out of `os.environ` and masks it in output.
            if environ is not None:
                token = str(env.get(TOKEN_ENV) or "")
            else:
                token = secret_guard.take_secret(TOKEN_ENV, keychain=KEYCHAIN_ITEM).reveal() or ""
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


# --------------------------------------------------------------------------- rendering (pure)


def fence(text: str, lang: str) -> str:
    """`text` in a fenced block, closed. A body that contains a fence of its own would break out of
    this one and turn the rest of the message into markdown, so fences inside the text are flattened
    to quotes first — a diff is read here, it is not executed and it does not need to round-trip."""
    body = (text or "").replace("```", "'''")
    return f"```{lang}\n{body}\n```"


def body_of(meta: Mapping[str, Any]) -> tuple[str, str]:
    """The thing the decision is actually about, and the fence to show it in: the diff for an edit,
    the command for a shell call, the input as JSON for anything else. `intervention_view.build`
    (acp_server's own `_meta.sierralab`) already picked and masked it, so this only clips and labels.
    Truncation is marked rather than silent: a reviewer told "(truncated)" can decide, one shown a
    half-diff without being told cannot."""
    kind = str(meta.get("kind") or "other")
    if kind == "diff" and meta.get("diff"):
        return str(meta["diff"]), "diff"
    if kind == "shell" and meta.get("command"):
        return str(meta["command"]), "sh"
    return str(meta.get("input_json") or meta.get("command") or ""), "json"


def intervention_embed(meta: Mapping[str, Any], *, title_prefix: str, where: str, redactions: int = 0) -> dict[str, Any]:
    """Everything the embed says, as plain data — built here so it can be read and asserted on
    without Discord, and so the one place that turns it into an `Embed` stays a formality."""
    score = meta.get("risk_score")
    band = str(meta.get("risk_band") or "")
    tool = str(meta.get("tool") or "tool")
    shown, lang = body_of(meta)
    truncated = len(shown) > MAX_BODY_CHARS
    body = shown[:MAX_BODY_CHARS] + ("\n(truncated — the full call is in the containment view)" if truncated else "")
    fields = [(f"{title_prefix} · {lang}", fence(body, lang), False)]
    if meta.get("reason"):
        fields.append(("Why it paused", str(meta["reason"])[:1024], False))
    fields.append(("Where", where[:1024], True))
    if score is not None:
        fields.append(("Laya", f"{float(score):.2f} · {band or 'unbanded'}", True))
    if redactions:
        fields.append(("Masked", f"{redactions} credential-shaped span(s) hidden in this view", True))
    return {
        "title": f"⚠ Containment — {tool} scored {float(score):.2f}" if score is not None else f"⚠ Containment — {tool}",
        "description": "A tool call Laya rated destructive is paused in the daemon, waiting on a person.",
        "fields": fields,
        "color": DANGER_COLOR,
    }


def thread_name(title: str) -> str:
    """`🧵 Task: <title>`, clipped to Discord's limit on the *combined* string."""
    clean = " ".join((title or "task").split())
    room = MAX_NAME_CHARS - len("🧵 Task: ")
    return f"🧵 Task: {clean[:room]}" if len(clean) > room else f"🧵 Task: {clean}"


def steer_text(content: str) -> str:
    """The directive in a message, or "" when it carries none.

    `> steer: <text>` (the directive's own spelling, with or without the colon) is one form; a plain
    sentence in a thread the bot opened is the other, because a reply in a task thread *is* the
    natural way to steer it. Anything else — a bare `> steer:` with nothing after it — is nothing,
    and an empty directive must not reach the model as an empty instruction.
    """
    text = (content or "").strip()
    lowered = text.lower()
    for prefix in ("> steer:", ">steer:", "> steer", ">steer"):
        if lowered.startswith(prefix):
            return text[len(prefix) :].strip()
    return text


# --------------------------------------------------------------------------- the bot


@dataclass
class _Thread:
    """A session's task thread, plus the little state a coalescing poster needs."""

    channel: Any
    session_id: str = ""
    last_post: float = 0.0
    dropped: int = 0


class DiscordRemote:
    """The daemon's Discord client, its supervisor listeners, and the thread registry.

    One instance per daemon, held by `daemon.server.State` so `/health` can report it. `start()` is
    the only place `discord` is imported, and it is a no-op when nothing is configured.
    """

    def __init__(self, supervisor: Any, *, environ: Mapping[str, str] | None = None, token: str | None = None) -> None:
        self.supervisor = supervisor
        self._environ = environ
        self._token_override = token
        #: Until `start()` runs, the honest reason is that nothing has been read — a bot that was
        #: never started must not report a configuration it never had.
        self.config: RemoteConfig = RemoteConfig(reason="not started")
        self._task: asyncio.Task | None = None
        self._client: Any = None
        self._discord: Any = None
        self._connected = False
        self._unsubscribes: list[Any] = []
        #: session id -> its task thread, and thread channel id -> session id, so a reply in a thread
        #: knows which session it steers without the person typing an id.
        self._threads: dict[str, _Thread] = {}
        self._by_channel: dict[int, str] = {}
        self._announced = 0.0

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
        # Reading a directive is reading message text, which Discord makes a privileged intent: the
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

        @client.event
        async def on_message(message) -> None:  # noqa: ANN001
            await self._on_message(message)

        # The supervisor's listeners fire on the event loop, so they schedule rather than await;
        # a listener that raised would be swallowed by `Supervisor._notify` and the prompt lost.
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
            hint = (
                " — enable Message Content Intent in the Discord Developer Portal"
                if "PrivilegedIntents" in name
                else ""
            )
            logger.warning("discord: client stopped: %s: %s%s", name, error, hint)
        finally:
            self._connected = False

    async def stop(self) -> None:
        for unsubscribe in self._unsubscribes:
            try:
                unsubscribe()
            except Exception:
                logger.debug("discord: unsubscribing failed", exc_info=True)
        self._unsubscribes = []
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
        }

    # ------------------------------------------------------------- channel plumbing

    async def _telemetry(self) -> Any:
        """The telemetry channel: from cache when this process has seen it, fetched when it has not
        (a daemon started before the channel existed holds no object for it)."""
        channel_id = self.config.channel_id
        get = getattr(self._client, "get_channel", None)
        channel = get(channel_id) if callable(get) else None
        if channel is not None:
            return channel
        fetch = getattr(self._client, "fetch_channel", None)
        return await fetch(channel_id) if callable(fetch) else None

    async def _announce(self) -> None:
        """`Daemon Connected`, with what it is watching. Posted once per connect: it is how a person
        learns the daemon is up without reading a log, which is the point of the channel."""
        self._announced = time.time()
        try:
            channel = await self._telemetry()
            if channel is None:
                logger.warning("discord: telemetry channel %s is not visible to this bot", self.config.channel_id)
                return
            sessions = self.supervisor.sessions()
            embed = self._embed(
                {
                    "title": "Daemon Connected",
                    "description": f"Operator <@{self.config.allowed_user_id}> — approvals, task threads and steering come here.",
                    "fields": [
                        ("Sessions", "\n".join(f"`{s['sessionId']}` {s['cwd']}" for s in sessions) or "none yet", False),
                        ("Approvals", "Laya at or above the project's destructive line", True),
                        ("Steering", "`> steer: <directive>` here, or any reply in a task thread", True),
                    ],
                    "color": INFO_COLOR,
                }
            )
            await channel.send(embed=embed)
        except Exception:
            logger.debug("discord: announce failed", exc_info=True)

    def _embed(self, spec: Mapping[str, Any]) -> Any:
        embed = self._discord.Embed(title=secret_guard.redact(str(spec.get("title") or "")), description=secret_guard.redact(str(spec.get("description") or "")), color=spec.get("color", INFO_COLOR))
        for name, value, inline in spec.get("fields") or []:
            embed.add_field(name=secret_guard.redact(str(name)), value=secret_guard.redact(str(value)), inline=bool(inline))
        return embed

    # ------------------------------------------------------------- HITL

    def _on_intervention(self, payload: Mapping[str, Any]) -> None:
        """A pending permission appeared. Post it only if it is at the project's destructive line —
        the same rule the GUI's modal follows (`>= 0.8` gets the modal, everything else a tray row),
        so the two front doors agree about what is urgent. A failure here is a log line: the prompt
        is still open in the GUI, and the child is still waiting on it either way."""
        if not self._connected or payload.get("event") != "requested":
            return
        pending = payload.get("pending")
        if pending is None:
            return
        meta = getattr(pending, "meta", None) or {}
        try:
            score = float(meta.get("risk_score"))
        except (TypeError, ValueError):
            return  # unreadable score: not a thing to ask a person about from here
        line = destructive_line(self._cwd(getattr(pending, "session_id", "")))
        if score < line:
            return
        asyncio.create_task(self._post_intervention(pending, meta, line), name="discord-approval")

    def _cwd(self, session_id: str) -> str | None:
        session = self.supervisor.session(session_id) if session_id else None
        return getattr(session, "cwd", None)

    async def _post_intervention(self, pending: Any, meta: Mapping[str, Any], line: float) -> None:
        try:
            channel = await self._telemetry()
            if channel is None:
                return
            session_id = str(getattr(pending, "session_id", ""))
            key = str(getattr(pending, "key", ""))
            where = f"`{session_id}` · {self._cwd(session_id) or '?'} · pod {meta.get('pod') or 'orchestrator'}"
            embed = self._embed(intervention_embed(meta, title_prefix="To run", where=where, redactions=int(meta.get("redactions") or 0)))
            view = self._view_class()(session_id, key, pending)
            await channel.send(embed=embed, view=view)
            logger.info("discord: approval %s/%s posted (score %s >= %s)", session_id, key, meta.get("risk_score"), line)
        except Exception:
            logger.debug("discord: posting an approval failed", exc_info=True)

    def _view_class(self) -> Any:
        """The `discord.ui.View` subclass, built once, the first time it is needed. Built here rather
        than at module level because that is what keeps `import discord` inside `start()`."""
        cached = getattr(self, "_view", None)
        if cached is None:
            cached = _build_view(self._discord, self)
            self._view = cached
        return cached

    async def _click(self, interaction: Any, view: Any, approve: bool) -> None:
        """One button press.

        The allowlist is checked *first* and returns before anything else happens: it is the only
        thing standing between an open Discord server and a daemon that runs shell commands, and a
        check that runs after a state change is not a check.
        """
        user_id = int(getattr(getattr(interaction, "user", None), "id", 0) or 0)
        if user_id != self.config.allowed_user_id:
            logger.warning("discord: refusing %s from user %s (not the configured operator)", "approve" if approve else "reject", user_id)
            await self._ephemeral(interaction, UNAUTHORIZED)
            return
        try:
            await self.supervisor.answer_permission(view.session_id, view.key, approve, decided_by="discord")
        except LookupError:
            # Answered by the other front door first, or the session ended. Not an error: the two
            # doors race by design, and `_settle` is what makes the loser harmless.
            await self._ephemeral(interaction, ALREADY)
            return
        except ValueError as error:
            await self._ephemeral(interaction, f"The session did not offer that option: {error}")
            return
        verdict = "approved" if approve else "rejected"
        logger.info("discord: %s %s/%s", verdict, view.session_id, view.key)
        meta = getattr(view.pending, "meta", None) or {}
        try:
            await interaction.response.edit_message(
                embed=self._embed(
                    {
                        "title": f"{'✅' if approve else '⛔'} {verdict.title()} from Discord",
                        "description": f"{verdict} by <@{self.config.allowed_user_id}>.",
                        "fields": [("Tool", str(meta.get("tool") or "tool"), True)],
                        "color": DONE_COLOR if approve else DANGER_COLOR,
                    }
                ),
                view=None,
            )
        except Exception:
            logger.debug("discord: could not edit the approval message", exc_info=True)

    @staticmethod
    async def _ephemeral(interaction: Any, text: str) -> None:
        try:
            await interaction.response.send_message(text, ephemeral=True)
        except Exception:
            logger.debug("discord: ephemeral reply failed", exc_info=True)

    # ------------------------------------------------------------- telemetry threads

    def _on_task(self, payload: Mapping[str, Any]) -> None:
        """A task bar moved. Only workflow sources open a thread; a chat turn's tasks stay out of
        Discord entirely, which is what keeps the channel readable."""
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
            thread = await channel.create_thread(name=thread_name(title))
            self._threads[session_id] = _Thread(channel=thread, session_id=session_id)
            self._by_channel[int(getattr(thread, "id", 0) or 0)] = session_id
            await thread.send(f"🧵 Tracking `{session_id}` — {self._cwd(session_id) or '?'}. Reply here to steer; `> steer: <directive>` also works.")
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
        line = secret_guard.redact(" ".join(text.split()))
        if thread.dropped:
            line = f"{line} (+{thread.dropped} update(s) folded in)"
            thread.dropped = 0
        thread.last_post = now
        try:
            await thread.channel.send(line[:1990])
        except Exception:
            logger.debug("discord: thread post failed", exc_info=True)

    # ------------------------------------------------------------- steering

    async def _on_message(self, message: Any) -> None:
        """Any message in the telemetry channel or a task thread from the operator is a directive.

        Everyone else is dropped in silence (a reply would tell an unauthorized stranger the bot is
        listening). The text takes the Lab's own guidance path — `_sierralab/lab/inject` on the child,
        which caps the length, queues it, and lands it in the model at the next call
        (`lab.inject` → `lab.take_guidance`) — so steering from Discord is the same mechanism as
        steering from the TUI's Lab view, not a second one with its own rules.
        """
        author = getattr(message, "author", None)
        if getattr(author, "bot", False):
            return
        if int(getattr(author, "id", 0) or 0) != self.config.allowed_user_id:
            logger.warning("discord: dropping a message from user %s (not the configured operator)", getattr(author, "id", 0))
            return
        channel_id = int(getattr(getattr(message, "channel", None), "id", 0) or 0)
        session_id = self._by_channel.get(channel_id, "")
        if not session_id and channel_id == self.config.channel_id:
            live = [s["sessionId"] for s in self.supervisor.sessions()]
            session_id = live[0] if len(live) == 1 else ""
        text = steer_text(str(getattr(message, "content", "") or ""))
        if not session_id or not text:
            return
        await self._steer(message, session_id, text)

    async def _steer(self, message: Any, session_id: str, text: str) -> None:
        directive = text[:MAX_STEER_CHARS]
        try:
            reply = await self.supervisor.call(session_id, "_sierralab/lab/inject", {"pod": "orchestrator", "text": directive})
        except (LookupError, asyncio.TimeoutError) as error:
            await message.channel.send(f"Could not steer `{session_id}`: {error or 'timed out'}")
            return
        error = (reply or {}).get("error")
        if error:
            data = (error.get("data") if isinstance(error, dict) else None) or {}
            await message.channel.send(f"Could not steer `{session_id}`: {data.get('error') or error}")
            return
        queued = int(((reply or {}).get("result") or {}).get("queued") or 0)
        # Ack with the length, never the text: the lab ledger records that a person steered, not a
        # second copy of whatever they typed (`daemon/grpc_bridge.py:354` does the same).
        await message.channel.send(f"Steering `{session_id}` ({len(directive)} chars queued, {queued} pending) — it lands at the model's next call.")


def _build_view(discord: Any, remote: DiscordRemote) -> Any:
    """The approve/reject pair, as a `discord.ui.View`. Module-level function, so the closure it makes
    over `remote` is the only thing it captures."""

    class ApprovalView(discord.ui.View):
        """Two buttons on one paused call. `timeout=None` on purpose: a decision that arrives an hour
        later is still the right decision — the child is still parked on it."""

        def __init__(self, session_id: str, key: str, pending: Any) -> None:
            super().__init__(timeout=None)
            self.session_id = session_id
            self.key = key
            self.pending = pending

        @discord.ui.button(label=APPROVE_LABEL, style=discord.ButtonStyle.success)
        async def approve(self, interaction: Any, button: Any) -> None:  # noqa: ARG002 — discord.py's signature
            await remote._click(interaction, self, True)

        @discord.ui.button(label=REJECT_LABEL, style=discord.ButtonStyle.danger)
        async def reject(self, interaction: Any, button: Any) -> None:  # noqa: ARG002
            await remote._click(interaction, self, False)

    return ApprovalView

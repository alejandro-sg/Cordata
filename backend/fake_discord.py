"""A scriptable stand-in for the parts of ``discord.py`` this harness uses.

Installed into ``sys.modules["discord"]`` by the smoke test before the bot starts, so
``daemon/discord_bot.py``'s lazy ``import discord`` resolves here instead of to the real
library — no token, no gateway, no network, and every send is recorded.

What it is faithful about, because the bot's behaviour depends on it:

* ``Client.start`` runs ``on_ready`` and then *waits* (a bot that "returns" would look
  connected and be gone), and ``close()`` releases it. ``FAIL_NEXT_START`` makes the next
  ``start`` raise the way a missing privileged intent or a bad token does.
* ``ui.Button`` keeps the label and style it was handed and the ``callback`` the bot
  assigns, so a test can assert the exact set of buttons on a card and press one.
* Channels and threads record what was sent to them. ``send`` returns a message that can be
  ``edit``ed and reacted to, and keeps its *current* content, embed and view — because
  "what does the person see now" (not "what was first posted") is the property under test
  for a live reply and a collapsing card.
* ``AllowedMentions`` is recorded on every send and edit: that an agent's words cannot ping
  anyone is a property worth asserting rather than hoping for.
* A thread created under a message, or under a channel, is registered so ``get_channel`` and
  ``fetch_channel`` find it by id afterwards, as the real client does.

Not faithful about anything else: no gateway, no rate limits, no message history. A test that
needs those should use the real library against a real server.
"""

from __future__ import annotations

import asyncio
import itertools
from types import SimpleNamespace
from typing import Any

#: When set to an exception instance, the next ``Client.start`` raises it (a privileged-intent
#: refusal, a bad token). Reset it in the test that uses one.
FAIL_NEXT_START: Exception | None = None

_ids = itertools.count(1000)

#: Every channel and thread created, by id — what `Client.get_channel` falls back to.
REGISTRY: dict[int, "Channel"] = {}


class HTTPException(Exception):
    """discord.py's own base error, for tests that want the generic failure path."""


class PrivilegedIntentsRequired(HTTPException):
    """What the gateway answers when Message Content Intent is not enabled in the portal."""


class LoginFailure(HTTPException):
    """An ImproperToken / invalid-token login."""


class Intents:
    def __init__(self) -> None:
        self.message_content = False
        self.guilds = False
        self.members = False

    @classmethod
    def default(cls) -> "Intents":
        return cls()


class AllowedMentions:
    def __init__(self, *, users: bool = False, roles: bool = False, everyone: bool = False) -> None:
        self.users, self.roles, self.everyone = users, roles, everyone

    @classmethod
    def none(cls) -> "AllowedMentions":
        return cls()

    @property
    def pings_anyone(self) -> bool:
        return bool(self.users or self.roles or self.everyone)


class ChannelType:
    public_thread = 11
    private_thread = 12


class Embed:
    def __init__(self, *, title: str = "", description: str = "", color: int | None = None) -> None:
        self.title = title
        self.description = description
        self.color = color
        self.fields: list[dict[str, Any]] = []
        self.footer: str = ""

    def add_field(self, *, name: str, value: str, inline: bool = True) -> None:
        self.fields.append({"name": name, "value": value, "inline": inline})

    def set_footer(self, *, text: str) -> None:
        self.footer = text

    def field(self, name: str) -> str:
        """The first field's value under this name — a test helper, not discord.py API."""
        for entry in self.fields:
            if entry["name"] == name:
                return entry["value"]
        raise KeyError(name)

    @property
    def text(self) -> str:
        """Everything the embed says, for a `needle in embed.text` assertion."""
        return "\n".join([self.title, self.description, *[f"{f['name']}\n{f['value']}" for f in self.fields], self.footer])


class ButtonStyle:
    primary = 1
    secondary = 2
    success = 3
    danger = 4


class ui:  # noqa: N801 — discord.py's own namespace object, named the same way
    """``discord.ui``: the ``View`` base and ``Button`` items."""

    class Button:
        def __init__(self, *, label: str = "", style: int = ButtonStyle.secondary, custom_id: str | None = None) -> None:
            self.label = label
            self.style = style
            self.callback: Any = None

    class View:
        def __init__(self, *, timeout: float | None = None) -> None:
            self.timeout = timeout
            self.children: list[Any] = []

        def add_item(self, item: Any) -> "ui.View":
            self.children.append(item)
            return self

        # -- test helpers, not discord.py API
        def labels(self) -> list[str]:
            return [child.label for child in self.children]

        def button(self, label: str) -> Any:
            return next(child for child in self.children if child.label == label)

        async def press(self, label: str, interaction: Any) -> None:
            await self.button(label).callback(interaction)


class SentMessage:
    """What `send` returns: it keeps its *current* state, and records every edit and reaction."""

    def __init__(self, channel: "Channel", content: str | None, embed: Any, view: Any, allowed_mentions: Any) -> None:
        self.id = next(_ids)
        self.channel = channel
        self.content = content
        self.embed = embed
        self.view = view
        self.allowed_mentions = allowed_mentions
        self.edits: list[dict[str, Any]] = []
        self.reactions: list[str] = []

    async def edit(self, **kwargs: Any) -> None:
        self.edits.append(kwargs)
        if "content" in kwargs:
            self.content = kwargs["content"]
        if "embed" in kwargs:
            self.embed = kwargs["embed"]
        if "view" in kwargs:
            self.view = kwargs["view"]
        if "allowed_mentions" in kwargs:
            self.allowed_mentions = kwargs["allowed_mentions"]

    async def add_reaction(self, emoji: str) -> None:
        self.reactions.append(emoji)

    @property
    def text(self) -> str:
        return f"{self.content or ''}{self.embed.text if self.embed else ''}"


class _Typing:
    def __init__(self, channel: "Channel") -> None:
        self.channel = channel

    async def __aenter__(self) -> "_Typing":
        self.channel.typing_entered += 1
        return self

    async def __aexit__(self, *exc: Any) -> None:
        self.channel.typing_exited += 1


class Channel:
    def __init__(self, channel_id: int, *, name: str = "telemetry") -> None:
        self.id = channel_id
        self.name = name
        self.sent: list[dict[str, Any]] = []
        self.messages: list[SentMessage] = []
        self.threads: list["Thread"] = []
        self.typing_entered = 0
        self.typing_exited = 0
        REGISTRY[channel_id] = self

    async def send(self, content: str | None = None, *, embed: Any = None, view: Any = None, allowed_mentions: Any = None) -> SentMessage:
        message = SentMessage(self, content, embed, view, allowed_mentions)
        self.messages.append(message)
        self.sent.append({"content": content, "embed": embed, "view": view, "allowed_mentions": allowed_mentions, "message": message})
        return message

    def typing(self) -> _Typing:
        return _Typing(self)

    async def fetch_message(self, message_id: int) -> Any:  # pragma: no cover — unused, present for realism
        raise HTTPException("the fake has no history")

    async def create_thread(self, *, name: str, type: Any = None, **kwargs: Any) -> "Thread":
        thread = Thread(next(_ids), name=name, parent=self)
        self.threads.append(thread)
        return thread

    def contents(self) -> list[str]:
        """Everything here as it reads *now* (edits applied), embeds flattened in — a test helper."""
        return [message.text for message in self.messages]


class Thread(Channel):
    def __init__(self, channel_id: int, *, name: str, parent: Channel) -> None:
        super().__init__(channel_id, name=name)
        self.parent = parent


class Client:
    """A client with no gateway: `start` runs `on_ready`, records the handlers, and waits."""

    def __init__(self, *, intents: Intents | None = None) -> None:
        self.intents = intents or Intents.default()
        self.user = SimpleNamespace(id=1, name="sierralab-daemon", bot=True)
        self.handlers: dict[str, Any] = {}
        self.channels: dict[int, Channel] = {}
        self.token = ""
        self.started = False
        self.closed = False
        self._stopped = asyncio.Event()

    def event(self, function: Any) -> Any:
        self.handlers[function.__name__] = function
        return function

    def get_channel(self, channel_id: int) -> Channel | None:
        return self.channels.get(channel_id) or REGISTRY.get(channel_id)

    async def fetch_channel(self, channel_id: int) -> Channel | None:
        return self.get_channel(channel_id)

    async def start(self, token: str) -> None:
        global FAIL_NEXT_START
        if FAIL_NEXT_START is not None:
            failure, FAIL_NEXT_START = FAIL_NEXT_START, None
            raise failure
        self.token = token
        self.started = True
        try:
            await self.handlers["on_ready"]()
            await self._stopped.wait()
        finally:
            self.started = False

    async def close(self) -> None:
        self.closed = True
        self._stopped.set()

    async def dispatch(self, name: str, *args: Any) -> Any:
        """Fire one of the events the bot registered, the way the gateway would."""
        return await self.handlers[name](*args)


class _Response:
    def __init__(self) -> None:
        self.messages: list[dict[str, Any]] = []
        self.edits: list[dict[str, Any]] = []

    async def send_message(self, content: str, *, ephemeral: bool = False) -> None:
        self.messages.append({"content": content, "ephemeral": ephemeral})

    async def edit_message(self, **kwargs: Any) -> None:
        self.edits.append(kwargs)


class Interaction:
    """A button press by `user_id`."""

    def __init__(self, user_id: int, *, message: Any = None) -> None:
        self.user = SimpleNamespace(id=user_id, bot=False)
        self.message = message
        self.response = _Response()


class Message:
    """A message *from a person*, as the gateway delivers it to `on_message`."""

    def __init__(self, content: str, *, author_id: int, channel: Channel, bot: bool = False) -> None:
        self.id = next(_ids)
        self.content = content
        self.author = SimpleNamespace(id=author_id, bot=bot)
        self.channel = channel
        self.reactions: list[str] = []
        self.created_threads: list[Thread] = []

    async def add_reaction(self, emoji: str) -> None:
        self.reactions.append(emoji)

    async def create_thread(self, *, name: str, **kwargs: Any) -> Thread:
        thread = await self.channel.create_thread(name=name)
        self.created_threads.append(thread)
        return thread

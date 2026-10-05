"""A scriptable stand-in for the parts of ``discord.py`` this harness uses.

Installed into ``sys.modules["discord"]`` by the smoke test before the bot starts, so
``daemon/discord_bot.py``'s lazy ``import discord`` resolves here instead of to the real
library — no token, no gateway, no network, and every send is recorded.

What it is faithful about, because the bot's behaviour depends on it:

* ``Client.start`` runs ``on_ready`` and then *waits* (a bot that "returns" would look
  connected and be gone), and ``close()`` releases it. ``FAIL_NEXT_START`` makes the next
  ``start`` raise the way a missing privileged intent or a bad token does.
* ``ui.button`` keeps the label and style it was handed, so the test can assert the pair
  is the one the directive asked for rather than any two buttons.
* Channels and threads record what was sent to them, with the embed and the view object,
  because "did this go to the thread or to the channel" is the property under test.

Not faithful about anything else: no gateway, no rate limits, no message objects with a
history. A test that needs those should use the real library against a real server.
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


class Embed:
    def __init__(self, *, title: str = "", description: str = "", color: int | None = None) -> None:
        self.title = title
        self.description = description
        self.color = color
        self.fields: list[dict[str, Any]] = []

    def add_field(self, *, name: str, value: str, inline: bool = True) -> None:
        self.fields.append({"name": name, "value": value, "inline": inline})

    def field(self, name: str) -> str:
        """The first field's value under this name — a test helper, not discord.py API."""
        for entry in self.fields:
            if entry["name"] == name:
                return entry["value"]
        raise KeyError(name)

    @property
    def text(self) -> str:
        """Everything the embed says, for a `needle in embed.text` assertion."""
        return "\n".join([self.title, self.description, *[f"{f['name']}\n{f['value']}" for f in self.fields]])


class ButtonStyle:
    primary = 1
    secondary = 2
    success = 3
    danger = 4


class ui:  # noqa: N801 — discord.py's own namespace object, named the same way
    """``discord.ui``: the ``View`` base and the ``button`` decorator."""

    class View:
        def __init__(self, *, timeout: float | None = None) -> None:
            self.timeout = timeout

    @staticmethod
    def button(**kwargs: Any):
        """Records the label/style on the function, so a test can read the pair back."""

        def decorate(function: Any) -> Any:
            function.button = kwargs
            return function

        return decorate


class Channel:
    def __init__(self, channel_id: int, *, name: str = "telemetry") -> None:
        self.id = channel_id
        self.name = name
        self.sent: list[dict[str, Any]] = []
        self.threads: list["Thread"] = []

    async def send(self, content: str | None = None, *, embed: Any = None, view: Any = None) -> Any:
        self.sent.append({"content": content, "embed": embed, "view": view})
        return SimpleNamespace(id=next(_ids), channel=self)

    async def fetch_message(self, message_id: int) -> Any:  # pragma: no cover — unused, present for realism
        raise HTTPException("the fake has no history")

    async def create_thread(self, *, name: str, **kwargs: Any) -> "Thread":
        thread = Thread(next(_ids), name=name, parent=self)
        self.threads.append(thread)
        return thread

    def contents(self) -> list[str]:
        """Everything sent here as text, embeds flattened in — a test helper."""
        return [f"{entry['content'] or ''}{(entry['embed'].text if entry['embed'] else '')}" for entry in self.sent]


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
        return self.channels.get(channel_id)

    async def fetch_channel(self, channel_id: int) -> Channel | None:
        return self.channels.get(channel_id)

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
    def __init__(self, content: str, *, author_id: int, channel: Channel, bot: bool = False) -> None:
        self.content = content
        self.author = SimpleNamespace(id=author_id, bot=bot)
        self.channel = channel

"""Steering a running turn: what the person types while the agent works reaches it at its next model call.

Codex, Copilot and Claude let a person redirect an agent mid-run ("use the other API", "skip the tests") without
stopping it. Strands has no message queue into a running invocation, so this is a hook: ``BeforeModelCallEvent``
fires before every model call of the agent loop, and when notes are waiting they are appended to the newest user
message (the one carrying the last tool results) as a text block of their own. That is a real user-role block, not
text inside a tool result, so a tool's output cannot imitate it.

A note that arrives after the last model call of a turn is not lost: ``drain`` hands the undelivered notes back when
the turn ends and the front door sends them as the next prompt.
"""

from __future__ import annotations

import threading
from typing import Any

from strands.hooks import BeforeModelCallEvent, HookProvider, HookRegistry

#: A note longer than this is cut (it is a steer, not a document).
MAX_NOTE_CHARS = 4000
#: Notes waiting at once.
MAX_PENDING = 20

PREFIX = "[The person, while you were working:]"


class Steering(HookProvider):
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._pending: list[str] = []
        self.delivered = 0

    def add(self, text: str) -> bool:
        """Queue a note for the next model call. False when it is empty or too many are waiting."""
        text = (text or "").strip()[:MAX_NOTE_CHARS]
        if not text:
            return False
        with self._lock:
            if len(self._pending) >= MAX_PENDING:
                return False
            self._pending.append(text)
        return True

    def drain(self) -> list[str]:
        """The notes no model call has seen (the turn ended first), removed."""
        with self._lock:
            notes, self._pending = self._pending, []
        return notes

    def pending(self) -> int:
        with self._lock:
            return len(self._pending)

    def register_hooks(self, registry: HookRegistry, **_: Any) -> None:
        registry.add_callback(BeforeModelCallEvent, self._before_model)

    def _before_model(self, event: BeforeModelCallEvent) -> None:
        messages = getattr(event.agent, "messages", None)
        if not messages or messages[-1].get("role") != "user":
            return
        notes = self.drain()
        if not notes:
            return
        text = PREFIX + "\n" + "\n".join(f"- {note}" for note in notes)
        content = list(messages[-1].get("content") or [])
        content.append({"text": text})
        messages[-1]["content"] = content
        self.delivered += len(notes)

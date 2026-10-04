"""In-process event bus for the harness's live state: background-task progress,
plugin lifecycle, LSP status, self-heal phases.

Producers (plugin host, LSP manager, self-heal hooks, sub-agent teams) call
``emit``; the ACP server subscribes once per process and forwards each event to
the client as an ACP extension notification (``_sierralab/<kind>``). A client
that doesn't know an extension notification ignores it (ACP spec), so Zed is
unaffected; acp-tui renders tasks as progress bars.

Producers run on many threads (MCP client threads, LSP reader threads, Strands
tool threads), so ``emit`` is thread-safe and never raises into the producer.
"""

from __future__ import annotations

import itertools
import logging
import threading
import time
from collections.abc import Callable
from typing import Any

logger = logging.getLogger(__name__)

Listener = Callable[[str, dict[str, Any]], None]

_listeners: list[Listener] = []
_lock = threading.Lock()


def subscribe(listener: Listener) -> Callable[[], None]:
    with _lock:
        _listeners.append(listener)

    def unsubscribe() -> None:
        with _lock:
            if listener in _listeners:
                _listeners.remove(listener)

    return unsubscribe


def emit(kind: str, payload: dict[str, Any]) -> None:
    with _lock:
        listeners = list(_listeners)
    for listener in listeners:
        try:
            listener(kind, payload)
        except Exception:  # a broken listener must never break a producer
            logger.debug("harness event listener failed", exc_info=True)


_ids = itertools.count(1)


class Task:
    """One unit of background work shown as a progress bar.

    ``total`` None means indeterminate (a spinner, not a bar). Updates are
    rate-limited to one per 50 ms unless the state changes, so a chatty producer
    (an MCP server reporting every item) can't flood the client.
    """

    _MIN_INTERVAL = 0.05

    def __init__(self, title: str, total: float | None = None, session_id: str | None = None, source: str = "") -> None:
        self.id = f"task-{next(_ids)}"
        self.title = title
        self.total = total
        self.done: float = 0
        self.message = ""
        self.state = "running"
        self.session_id = session_id
        self.source = source
        self._last = 0.0
        self._emit(force=True)

    def update(self, done: float | None = None, total: float | None = None, message: str | None = None) -> None:
        if self.state != "running":
            return
        if done is not None:
            self.done = done
        if total is not None:
            self.total = total
        if message is not None:
            self.message = message
        self._emit()

    def finish(self, state: str = "completed", message: str | None = None) -> None:
        if self.state != "running":
            return
        self.state = state
        if message is not None:
            self.message = message
        if state == "completed" and self.total:
            self.done = self.total
        self._emit(force=True)

    def snapshot(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "title": self.title,
            "done": self.done,
            "total": self.total,
            "message": self.message,
            "state": self.state,
            "source": self.source,
            **({"sessionId": self.session_id} if self.session_id else {}),
        }

    def _emit(self, force: bool = False) -> None:
        now = time.monotonic()
        if not force and now - self._last < self._MIN_INTERVAL:
            return
        self._last = now
        emit("task", self.snapshot())

    def __enter__(self) -> "Task":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.finish("failed" if exc_type else "completed", message=str(exc) if exc else None)

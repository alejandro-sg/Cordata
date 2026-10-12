"""The agent's visible plan: ``todo_write``.

A multi-step job is easier to follow — for the person and for the agent itself — as a short checklist that is kept up to
date than as a stream of tool calls. The agent writes the whole list each time it changes (pending / in_progress /
completed); the harness validates it, remembers it for the session and pushes it to the client as
``_cordata/tasklist``, where the TUI draws it above the prompt. It does nothing else: no files, no commands, so it
needs no approval.

One item is in progress at a time, and the list is capped — a plan of forty lines is noise, not a plan.
"""

from __future__ import annotations

import threading
from typing import Any

from strands import tool

import harness_events

STATUSES = ("pending", "in_progress", "completed")
MAX_ITEMS = 12
MAX_TEXT = 140

_lock = threading.Lock()
_lists: dict[str, list[dict]] = {}
_session_id: Any = lambda: ""  # replaced by configure()


def configure(session_id) -> None:
    global _session_id
    _session_id = session_id


def validate(todos: Any) -> tuple[list[dict], str | None]:
    """The cleaned list, or the reason it was refused (said back to the agent so it can fix the call)."""
    if not isinstance(todos, list) or not todos:
        return [], "todos must be a non-empty list of {content, status} items"
    if len(todos) > MAX_ITEMS:
        return [], f"at most {MAX_ITEMS} items — a plan longer than that is not a plan; group the steps"
    cleaned: list[dict] = []
    for index, item in enumerate(todos, 1):
        if isinstance(item, str):
            item = {"content": item, "status": "pending"}
        if not isinstance(item, dict):
            return [], f"item {index} must be an object with content and status"
        content = str(item.get("content") or item.get("task") or "").strip()
        status = str(item.get("status") or "pending").strip().lower().replace("-", "_").replace(" ", "_")
        if status in ("done", "complete"):
            status = "completed"
        if not content:
            return [], f"item {index} has no content"
        if status not in STATUSES:
            return [], f"item {index}: status must be one of {', '.join(STATUSES)}, not {status!r}"
        cleaned.append({"content": content[:MAX_TEXT], "status": status})
    if sum(1 for t in cleaned if t["status"] == "in_progress") > 1:
        return [], "only one item may be in_progress at a time"
    return cleaned, None


def current(session_id: str) -> list[dict]:
    with _lock:
        return [dict(item) for item in _lists.get(session_id, [])]


def clear(session_id: str) -> None:
    with _lock:
        _lists.pop(session_id, None)


def _publish(session_id: str, todos: list[dict]) -> None:
    done = sum(1 for t in todos if t["status"] == "completed")
    harness_events.emit("tasklist", {"sessionId": session_id, "items": todos, "done": done, "total": len(todos)})


@tool
def todo_write(todos: list[dict]) -> str:
    """Keep a short checklist of what you are doing, visible to the user. Use it for work with three or more steps;
    skip it for a one-step request.

    Send the WHOLE list every time it changes. Each item is {"content": "<imperative step>", "status": "pending" |
    "in_progress" | "completed"}. Mark exactly one item in_progress while you work on it, and mark it completed as soon
    as it is done — not in a batch at the end. Add steps you discover; drop ones that stop mattering.

    Args:
        todos: the full list of items, in order.
    """
    cleaned, problem = validate(todos)
    if problem:
        return f"The list was not saved: {problem}."
    session_id = str(_session_id() or "")
    with _lock:
        _lists[session_id] = cleaned
    _publish(session_id, cleaned)
    done = sum(1 for t in cleaned if t["status"] == "completed")
    return f"Checklist saved ({done}/{len(cleaned)} done)."

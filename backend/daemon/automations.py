"""Automations: a saved prompt the daemon runs on a schedule in a project (Copilot's automations, Claude's scheduled
tasks). Each run is a new hub conversation titled ``⏱ <name>``, so every front door sees it, its history and its
result like any other.

Nothing runs unattended that would not run attended: a run uses the project's own permission settings, and a step
that needs approval waits for the person in the app ("needs you") — it is never answered on their behalf.

Stored in ``<user dir>/automations.json`` (0600, written atomically). Schedules: every N minutes (at least 15),
daily at HH:MM, or weekly on a day at HH:MM, in local time.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import re
import secrets
import time as clock
from dataclasses import asdict, dataclass, field
from datetime import datetime, timedelta
from pathlib import Path
from typing import Any

logger = logging.getLogger(__name__)

MAX_AUTOMATIONS = 50
MAX_PROMPT = 8000
MIN_INTERVAL_MINUTES = 15
TICK_SECONDS = 30
_TIME = re.compile(r"^([01]\d|2[0-3]):[0-5]\d$")
DAYS = ("mon", "tue", "wed", "thu", "fri", "sat", "sun")


class AutomationError(ValueError):
    """A message a person can be shown."""


@dataclass
class Automation:
    id: str
    name: str
    prompt: str
    cwd: str
    kind: str  # interval | daily | weekly
    minutes: int = 60
    time: str = "09:00"
    day: str = "mon"
    enabled: bool = True
    created: float = field(default_factory=clock.time)
    last_run: float = 0.0
    last_status: str = ""
    last_handle: str = ""

    def public(self) -> dict[str, Any]:
        row = asdict(self)
        row["next"] = next_run(self, clock.time())
        return row


def validate(raw: dict[str, Any], *, existing: Automation | None = None) -> Automation:
    name = " ".join(str(raw.get("name") or "").split())[:80]
    prompt = str(raw.get("prompt") or "").strip()
    cwd = str(raw.get("cwd") or "")
    kind = str(raw.get("kind") or "daily")
    if not name:
        raise AutomationError("give it a name")
    if not prompt:
        raise AutomationError("say what it should do")
    if len(prompt) > MAX_PROMPT:
        raise AutomationError(f"the prompt is over {MAX_PROMPT} characters")
    if not os.path.isabs(cwd) or not os.path.isdir(cwd):
        raise AutomationError("the project must be an existing folder (absolute path)")
    if kind not in ("interval", "daily", "weekly"):
        raise AutomationError("the schedule is interval, daily or weekly")
    minutes = int(raw.get("minutes") or 60)
    if kind == "interval" and minutes < MIN_INTERVAL_MINUTES:
        raise AutomationError(f"run at most every {MIN_INTERVAL_MINUTES} minutes")
    at = str(raw.get("time") or "09:00")
    if kind != "interval" and not _TIME.match(at):
        raise AutomationError("the time is HH:MM (24-hour)")
    day = str(raw.get("day") or "mon").lower()[:3]
    if kind == "weekly" and day not in DAYS:
        raise AutomationError("the day is mon, tue, wed, thu, fri, sat or sun")
    base = existing or Automation(id=secrets.token_hex(4), name=name, prompt=prompt, cwd=cwd, kind=kind)
    base.name, base.prompt, base.cwd, base.kind = name, prompt, os.path.realpath(cwd), kind
    base.minutes, base.time, base.day = minutes, at, day
    base.enabled = bool(raw.get("enabled", base.enabled))
    return base


def next_run(a: Automation, now: float) -> float | None:
    """When ``a`` runs next (epoch seconds), or None while it is disabled: the first slot after its last run (or after
    it was made). A slot missed while the Mac slept is therefore due at once — and runs once, not once per slot."""
    if not a.enabled:
        return None
    reference = a.last_run or a.created
    if a.kind == "interval":
        return reference + a.minutes * 60
    hour, minute = (int(x) for x in a.time.split(":"))
    start = datetime.fromtimestamp(reference)
    candidate = start.replace(hour=hour, minute=minute, second=0, microsecond=0)
    if a.kind == "weekly":
        candidate += timedelta(days=(DAYS.index(a.day) - candidate.weekday()) % 7)
    step = timedelta(days=7 if a.kind == "weekly" else 1)
    while candidate.timestamp() <= reference:
        candidate += step
    return candidate.timestamp()


def due(a: Automation, now: float) -> bool:
    upcoming = next_run(a, now)
    return upcoming is not None and upcoming <= now


class Store:
    def __init__(self, path: Path) -> None:
        self.path = path
        self.items: dict[str, Automation] = {}
        self.load()

    def load(self) -> None:
        try:
            data = json.loads(self.path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return
        for row in data.get("automations") or []:
            try:
                item = Automation(**{k: v for k, v in row.items() if k in Automation.__dataclass_fields__})
                self.items[item.id] = item
            except TypeError:
                logger.warning("automations: skipped a malformed entry")

    def save(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_name(self.path.name + ".tmp")
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump({"version": 1, "automations": [asdict(a) for a in self.items.values()]}, handle, indent=1)
        os.replace(tmp, self.path)

    def upsert(self, raw: dict[str, Any]) -> Automation:
        existing = self.items.get(str(raw.get("id") or ""))
        if existing is None and len(self.items) >= MAX_AUTOMATIONS:
            raise AutomationError(f"at most {MAX_AUTOMATIONS} automations")
        item = validate(raw, existing=existing)
        self.items[item.id] = item
        self.save()
        return item

    def delete(self, item_id: str) -> bool:
        existed = self.items.pop(item_id, None) is not None
        if existed:
            self.save()
        return existed


class _Quiet:
    """An automation's turn has no live front door: the conversation's watchers (the app) see it instead."""

    def text(self, delta: str) -> None:
        return None

    def activity(self, label: str | None) -> None:
        return None


class Runner:
    def __init__(self, store: Store, chats: Any) -> None:
        self.store, self.chats = store, chats
        self._running: set[str] = set()
        self._task: asyncio.Task | None = None

    def start(self) -> None:
        if self._task is None:
            self._task = asyncio.ensure_future(self._loop())

    async def stop(self) -> None:
        if self._task is not None:
            self._task.cancel()
            self._task = None

    async def _loop(self) -> None:
        while True:
            try:
                now = clock.time()
                for item in list(self.store.items.values()):
                    if item.id not in self._running and due(item, now):
                        asyncio.ensure_future(self.run(item.id))
            except Exception:
                logger.exception("automations: tick failed")
            await asyncio.sleep(TICK_SECONDS)

    async def run(self, item_id: str) -> dict[str, Any]:
        """Run one now: a new conversation in its project, the prompt as its first turn. Returns the conversation row."""
        item = self.store.items.get(item_id)
        if item is None:
            raise AutomationError("no such automation")
        if item_id in self._running:
            raise AutomationError("it is already running")
        self._running.add(item_id)
        item.last_run, item.last_status = clock.time(), "running"
        self.store.save()
        conv = self.chats.create(cwd=item.cwd, title=f"⏱ {item.name}", origin="automation")
        item.last_handle = conv.handle
        try:
            result = await self.chats.send(conv.key, item.prompt, _Quiet(), origin="automation")
            item.last_status = "done" if result.stop in ("end_turn", "max_turn_requests") else result.stop
        except Exception as error:  # a failed run is recorded and shown; the schedule goes on
            item.last_status = f"failed: {str(error)[:160]}"
            logger.info("automations: %s failed: %s", item.name, error)
        finally:
            self._running.discard(item_id)
            self.store.save()
        return conv.row()

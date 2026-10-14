"""Automations (daemon/automations.py): validation, the schedule arithmetic (including a slot missed while asleep
running once), the store (0600, atomic, reloads), and a run — a new conversation titled for the automation, the
prompt as its turn, the outcome recorded, a failure recorded without stopping the schedule.

    backend/.venv/bin/python _automations_smoke_test.py
"""

from __future__ import annotations

import asyncio
import os
import tempfile
from datetime import datetime
from pathlib import Path
from types import SimpleNamespace

from daemon import automations as auto
from harness_testing import check


def ts(y, mo, d, h, mi) -> float:
    return datetime(y, mo, d, h, mi).timestamp()


class FakeChats:
    def __init__(self, fail: bool = False) -> None:
        self.sent: list[tuple[str, str, str]] = []
        self.fail = fail
        self.made = 0

    def create(self, *, cwd: str, title: str, origin: str):
        self.made += 1
        conv = SimpleNamespace(key=f"k{self.made}", handle=f"h{self.made}", title=title, cwd=cwd, origin=origin)
        conv.row = lambda: {"handle": conv.handle, "title": conv.title}
        return conv

    async def send(self, key: str, text: str, sink, *, origin: str):
        if self.fail:
            raise RuntimeError("agent went away")
        self.sent.append((key, text, origin))
        return SimpleNamespace(stop="end_turn")


def main() -> None:
    project = tempfile.mkdtemp()
    print("=== 1. validation ===")
    for bad, why in [({"name": "", "prompt": "x", "cwd": project}, "name"), ({"name": "n", "prompt": "", "cwd": project}, "say what"),
                     ({"name": "n", "prompt": "x", "cwd": "relative"}, "folder"), ({"name": "n", "prompt": "x", "cwd": project, "kind": "interval", "minutes": 5}, "15"),
                     ({"name": "n", "prompt": "x", "cwd": project, "time": "25:00"}, "HH:MM"), ({"name": "n", "prompt": "x" * 9000, "cwd": project}, "characters")]:
        try:
            auto.validate(bad)
            check(False, f"refused: {why}")
        except auto.AutomationError as error:
            check(why in str(error), f"refused with a reason ({error})")

    print("\n=== 2. schedules ===")
    daily = auto.Automation(id="d", name="d", prompt="p", cwd=project, kind="daily", time="09:00", created=ts(2026, 10, 14, 10, 0))
    check(auto.next_run(daily, ts(2026, 10, 14, 10, 1)) == ts(2026, 10, 15, 9, 0), "made after today's slot: tomorrow at 09:00")
    check(not auto.due(daily, ts(2026, 10, 15, 8, 59)) and auto.due(daily, ts(2026, 10, 15, 9, 0)), "due at the slot, not before")
    daily.last_run = ts(2026, 10, 15, 9, 0)
    check(auto.next_run(daily, ts(2026, 10, 15, 9, 1)) == ts(2026, 10, 16, 9, 0), "after a run: the next day")
    check(auto.due(daily, ts(2026, 10, 19, 12, 0)), "slots missed while asleep: due")
    daily.last_run = ts(2026, 10, 19, 12, 0)
    check(not auto.due(daily, ts(2026, 10, 19, 12, 1)), "…and it ran once, not once per missed slot")
    weekly = auto.Automation(id="w", name="w", prompt="p", cwd=project, kind="weekly", day="fri", time="17:30", created=ts(2026, 10, 14, 10, 0))
    check(datetime.fromtimestamp(auto.next_run(weekly, ts(2026, 10, 14, 10, 0))) == datetime(2026, 10, 16, 17, 30), "weekly: the coming Friday 17:30")
    every = auto.Automation(id="i", name="i", prompt="p", cwd=project, kind="interval", minutes=30, created=1000.0)
    check(auto.next_run(every, 1000.0) == 1000.0 + 1800, "interval: from creation, then from each run")
    every.enabled = False
    check(auto.next_run(every, 5000.0) is None and not auto.due(every, 1e12), "disabled: never due")

    print("\n=== 3. the store ===")
    path = Path(tempfile.mkdtemp()) / "automations.json"
    store = auto.Store(path)
    item = store.upsert({"name": "Nightly review", "prompt": "review yesterday's commits", "cwd": project, "kind": "daily", "time": "07:00"})
    check(oct(path.stat().st_mode & 0o777) == "0o600", "written 0600")
    check(auto.Store(path).items[item.id].prompt == "review yesterday's commits", "reloads from disk")
    store.upsert({"id": item.id, "name": "Nightly review", "prompt": "changed", "cwd": project, "kind": "daily", "time": "07:00", "enabled": False})
    check(len(store.items) == 1 and not store.items[item.id].enabled, "an id updates in place")
    check(store.delete(item.id) and not store.delete(item.id), "delete, once")

    print("\n=== 4. runs ===")
    store.upsert({"name": "Tests", "prompt": "run the tests", "cwd": project, "kind": "interval", "minutes": 60})
    target = next(iter(store.items.values()))
    chats = FakeChats()
    runner = auto.Runner(store, chats)
    row = asyncio.run(runner.run(target.id))
    check(chats.sent == [("k1", "run the tests", "automation")] and row["title"] == "⏱ Tests", f"a run is a new conversation with the prompt as its turn ({row})")
    check(target.last_status == "done" and target.last_handle == "h1" and target.last_run > 0, "the outcome is recorded")
    failing = auto.Runner(store, FakeChats(fail=True))
    asyncio.run(failing.run(target.id))
    check(target.last_status.startswith("failed: agent went away"), f"a failure is recorded ({target.last_status})")
    check(target.id not in failing._running, "…and the schedule can run it again")
    os.environ.pop("X", None)
    print("\nALL OK")


if __name__ == "__main__":
    main()

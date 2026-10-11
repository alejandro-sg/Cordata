"""Conversations on disk: Strands' `SnapshotSessionManager`, and the one-off move to it from `FileSessionManager`.

Why. The SDK calls snapshot sessions "the recommended path for new agents" and the repository-style
`FileSessionManager` the older one. A conversation is one atomic, versioned JSON blob (`snapshot_latest.json`) written
through the unified `Storage` primitive — not a directory of `message_N.json` files that a crash can leave half
written — and the same layout supports immutable checkpoints (time-travel restore) when a `snapshot_trigger` is given.
It restores everything a conversation is: messages, agent state (the skills block's bookkeeping, for one), the
conversation manager's state (a summary written to compact a long chat) and any interrupt in flight.

What this module adds:

* **A private store.** `<sessions>/session/<id>/scopes/agent/default/snapshots/snapshot_latest.json`, directory 0700
  and file 0600 (`context_offload.PrivateFileStorage`): a conversation holds whatever a tool printed.
* **The system prompt is not persisted.** The SDK's manager saves it and puts it back on restore. This harness builds
  a fresh one for every session — the project's instructions, the specialist roster, the settings in force — and plugins
  add and remove blocks in it during a turn; restoring last week's would silently undo every change since, and any
  block that was in it at save time would be doubled. `_capture` is the SDK's one point where the fields are chosen.
* **Turn checkpoints — "go back".** One immutable snapshot (the SDK's time-travel feature) is written when a turn *starts*,
  with the prompt that is about to run in its `app_data`, so "the state before turn 5" is one restore away. The turn
  counter lives in the agent's own state, so restoring a checkpoint restores the count with it. `go_back` restores
  one, after saving the state it is leaving as a checkpoint of its own (so going back can itself be undone).
* **Migration.** `migrate(session_id)` reads a legacy `session_<id>/` directory *through the SDK's own repository
  manager* (so the format is whatever the SDK says it is, not what this module guesses) and writes the snapshot. It is
  additive: the legacy directory is left exactly as it was, so a rollback to the old manager loses nothing. `ensure`
  does it on first use of a session, so nobody has to run anything; `python session_store.py migrate --all` does the
  lot in one go and says what it did.
* **A broken legacy session is not lost.** If migration fails (a corrupt file), the session keeps using the legacy
  manager, exactly as before, rather than starting empty.

Stdlib plus Strands.
"""

from __future__ import annotations

import argparse
import json
import logging
import sys
import time
from collections.abc import AsyncGenerator, Iterable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from strands import Agent
from strands.agent.conversation_manager import SummarizingConversationManager
from strands.models.model import Model
from strands.session import SnapshotSessionManager
from strands.session.file_session_manager import FileSessionManager
from strands.types._snapshot import Snapshot  # noqa: F401 — documents what `_capture` returns

from context_offload import PrivateFileStorage

logger = logging.getLogger(__name__)

#: The id Strands gives an agent that is not named. Both layouts are keyed by it.
AGENT_ID = "default"
#: Under the sessions directory, the snapshot layout lives beside the legacy `session_<id>/` directories, in `session/`.
SNAPSHOT_ROOT = "session"


# ---------------------------------------------------------------------------- the manager


#: `AgentSkills`' bookkeeping in the agent's state (strands.vended_plugins.skills): the last block it injected into the prompt.
SKILLS_STATE_KEY = "agent_skills"
#: Where the turn counter lives, in the agent's state (so a restore brings the count back with it).
TURNS_KEY = "sierralab_turns"
#: How many turn checkpoints a session keeps; older ones are deleted as new ones are made.
KEEP_CHECKPOINTS = 50


class HarnessSessionManager(SnapshotSessionManager):
    """`SnapshotSessionManager`, minus the system prompt (module docstring), plus turn checkpoints."""

    _meta: dict | None = None

    def _capture(self, agent: Any) -> Any:
        snapshot = agent.take_snapshot(preset="session", app_data=dict(self._meta or {}))
        # The skills plugin remembers *which block it put in the system prompt* so it can swap it next turn. The prompt is
        # not saved, so that memory would point at a block a fresh prompt does not have (and log a warning about it).
        (snapshot.data.get("state") or {}).pop(SKILLS_STATE_KEY, None)
        return snapshot

    async def checkpoint(self, agent: Any, meta: dict) -> str | None:
        """An immutable snapshot of the agent as it is now, labelled with `meta` (turn number, prompt, time, kind)."""
        self._meta = meta
        try:
            return await self.save_snapshot(agent, is_latest=False)
        finally:
            self._meta = None


def manager_for(session_id: str, base: str | Path) -> HarnessSessionManager:
    """The snapshot manager for one session, stored under `base` (the sessions directory)."""
    return HarnessSessionManager(session_id, storage=PrivateFileStorage(base))


# ---------------------------------------------------------------------------- what exists on disk


def snapshot_path(session_id: str, base: str | Path) -> Path:
    return Path(base) / SNAPSHOT_ROOT / session_id / "scopes" / "agent" / AGENT_ID / "snapshots" / "snapshot_latest.json"


def legacy_dir(session_id: str, base: str | Path) -> Path:
    return Path(base) / f"session_{session_id}"


def has_snapshot(session_id: str, base: str | Path) -> bool:
    return snapshot_path(session_id, base).is_file()


def has_legacy(session_id: str, base: str | Path) -> bool:
    return (legacy_dir(session_id, base) / "agents" / f"agent_{AGENT_ID}").is_dir()


def exists(session_id: str, base: str | Path) -> bool:
    """Is there a conversation under this id, in either layout?"""
    return has_snapshot(session_id, base) or legacy_dir(session_id, base).is_dir()


def legacy_ids(base: str | Path) -> list[str]:
    root = Path(base)
    if not root.is_dir():
        return []
    return sorted(p.name[len("session_") :] for p in root.iterdir() if p.is_dir() and p.name.startswith("session_") and has_legacy(p.name[len("session_") :], base))


# ---------------------------------------------------------------------------- migration


class _NoModel(Model):
    """A model that is never called: the agent this module builds only has to *hold* a restored conversation."""

    def __init__(self) -> None:
        self.config: dict[str, Any] = {"model_id": "session-migration"}

    def update_config(self, **model_config: Any) -> None:
        self.config.update(model_config)

    def get_config(self) -> dict[str, Any]:
        return self.config

    async def structured_output(self, *args: Any, **kwargs: Any) -> AsyncGenerator[Any, None]:  # pragma: no cover
        raise RuntimeError("the migration agent has no model")
        yield

    async def stream(self, *args: Any, **kwargs: Any) -> AsyncGenerator[Any, None]:  # pragma: no cover
        raise RuntimeError("the migration agent has no model")
        yield


# ---------------------------------------------------------------------------- turn checkpoints


def history_dir(session_id: str, base: str | Path) -> Path:
    return snapshot_path(session_id, base).parent / "immutable_history"


@dataclass
class Checkpoint:
    id: str
    turn: int
    prompt: str
    kind: str
    at: float
    messages: int


def checkpoints(session_id: str, base: str | Path) -> list[Checkpoint]:
    """The session's checkpoints, oldest first (the ids are time-ordered UUIDv7s, so file order is time order)."""
    out: list[Checkpoint] = []
    for path in sorted(history_dir(session_id, base).glob("snapshot_*.json")):
        try:
            blob = json.loads(path.read_text())
            meta = blob.get("app_data") or {}
            out.append(
                Checkpoint(
                    id=path.stem[len("snapshot_") :],
                    turn=int(meta.get("turn") or 0),
                    prompt=str(meta.get("prompt") or ""),
                    kind=str(meta.get("kind") or ""),
                    at=float(meta.get("at") or 0.0),
                    messages=len((blob.get("data") or {}).get("messages") or []),
                )
            )
        except (OSError, ValueError, TypeError):
            continue  # a checkpoint that cannot be read is not offered
    return out


def prune_checkpoints(session_id: str, base: str | Path, keep: int = KEEP_CHECKPOINTS) -> int:
    paths = sorted(history_dir(session_id, base).glob("snapshot_*.json"))
    removed = 0
    for path in paths[: max(0, len(paths) - keep)]:
        try:
            path.unlink()
            removed += 1
        except OSError:
            pass
    return removed


def forget_prompt_bookkeeping(agent: Any) -> None:
    """Drop what a *restored* state says about the system prompt it was saved beside (see `_capture`) — sessions written before
    that was left out carry it, and a fresh prompt does not have the block it names."""
    try:
        agent.state.delete(SKILLS_STATE_KEY)
    except Exception:
        pass


def turns_taken(agent: Any) -> int:
    try:
        return int(agent.state.get(TURNS_KEY) or 0)
    except (TypeError, ValueError):
        return 0


async def begin_turn(agent: Any, prompt: str, base: str | Path, *, now: float | None = None) -> str | None:
    """Checkpoint the conversation as it is *before* this turn, then count the turn. None when the session is not on snapshots
    (a legacy one that could not be migrated). Never raises: a checkpoint is a convenience, not a condition of the turn."""
    manager = getattr(agent, "_session_manager", None)
    if not isinstance(manager, HarnessSessionManager):
        return None
    try:
        turn = turns_taken(agent) + 1
        line = " ".join((prompt or "").split())[:120]
        checkpoint_id = await manager.checkpoint(agent, {"turn": turn, "prompt": line, "kind": "turn", "at": now if now is not None else time.time()})
        agent.state.set(TURNS_KEY, turn)
        prune_checkpoints(manager.session_id, base)
        return checkpoint_id
    except Exception:
        logger.debug("could not checkpoint the conversation", exc_info=True)
        return None


async def go_back(agent: Any, base: str | Path, *, turns: int = 1, to: str | None = None, index: int | None = None, now: float | None = None) -> dict:
    """Take the conversation back `turns` turns (or to checkpoint `to`): messages, agent state and the conversation manager's
    state are restored; the files are not touched. The state it leaves is saved first, so this is itself undoable.

    `index` names a checkpoint by its number in `listing` (newest first). Returns `{"ok": True, ...}` with what was forgotten,
    or `{"ok": False, "reason": ...}`."""
    manager = getattr(agent, "_session_manager", None)
    if not isinstance(manager, HarnessSessionManager):
        return {"ok": False, "reason": "this session is not on snapshot storage (it could not be migrated), so it has no checkpoints"}
    session_id = manager.session_id
    marks = checkpoints(session_id, base)
    taken = turns_taken(agent)
    if index is not None:
        newest_first = list(reversed(marks))
        target = newest_first[index - 1] if 1 <= index <= len(newest_first) else None
        if target is None:
            return {"ok": False, "reason": f"no checkpoint number {index} — /back list shows what there is"}
    elif to:
        target = next((c for c in marks if c.id == to or c.id.startswith(to)), None)
        if target is None:
            return {"ok": False, "reason": "no such checkpoint"}
    else:
        if turns < 1:
            return {"ok": False, "reason": "go back at least one turn"}
        wanted = taken - turns + 1
        turn_marks = [c for c in marks if c.kind == "turn" and c.turn == wanted]
        if wanted < 1 or not turn_marks:
            first = min((c.turn for c in marks if c.kind == "turn"), default=None)
            if taken == 0:
                return {"ok": False, "reason": "nothing to go back to — no turn has been taken in this session"}
            have = taken - first + 1 if first else 0
            return {"ok": False, "reason": f"only {have} turn{'s' if have != 1 else ''} can be taken back (checkpoints exist for turn {first} onward)" if have else "no checkpoints to go back to"}
        target = turn_marks[-1]
    # The checkpoint for turn t is the state *before* turn t, so going to it forgets turns t … now. Where a turn number has
    # several (an abandoned branch), the latest is the one on the path we are on.
    latest = {c.turn: c for c in marks if c.kind == "turn"}
    forgot = [latest[t].prompt for t in range(target.turn, taken + 1) if t in latest] if target.kind == "turn" else []
    leaving = await manager.checkpoint(agent, {"turn": taken, "prompt": f"as it was before going back to {'turn ' + str(target.turn) if target.kind == 'turn' else 'an earlier checkpoint'}", "kind": "before-back", "at": now if now is not None else time.time()})
    if not await manager.restore_snapshot(agent, snapshot_id=target.id):
        return {"ok": False, "reason": "that checkpoint could not be read"}
    await manager.save_snapshot(agent, is_latest=True)
    prune_checkpoints(session_id, base)
    return {"ok": True, "to": target.id, "turn": target.turn, "now": turns_taken(agent), "forgot": [f for f in forgot if f], "undo_with": leaving}


def listing(agent: Any, base: str | Path, limit: int = 15) -> dict:
    """What `go_back` can offer, newest first: `{"turns": n, "checkpoints": [...]}` (a checkpoint's `index` is what `to` takes)."""
    manager = getattr(agent, "_session_manager", None)
    if not isinstance(manager, HarnessSessionManager):
        return {"enabled": False, "turns": 0, "checkpoints": []}
    marks = checkpoints(manager.session_id, base)
    rows = [
        {"index": n, "id": c.id, "turn": c.turn, "prompt": c.prompt, "kind": c.kind, "at": c.at, "messages": c.messages}
        for n, c in enumerate(reversed(marks), 1)
    ]
    return {"enabled": True, "turns": turns_taken(agent), "checkpoints": rows[:limit]}


def migrate(session_id: str, base: str | Path) -> bool:
    """Write the snapshot for a legacy session. True if one was written; False if there was nothing to do.

    Raises whatever the SDK raises on a corrupt legacy session — callers decide what that means (`ensure` keeps the session
    on the legacy manager)."""
    if has_snapshot(session_id, base) or not has_legacy(session_id, base):
        return False
    # The SDK's own repository manager does the reading: messages, agent state, the conversation manager's state, the
    # interrupt state. The conversation manager class has to be the one that wrote the state (`SummarizingConversationManager`).
    holder = Agent(
        model=_NoModel(),
        agent_id=AGENT_ID,
        session_manager=FileSessionManager(session_id=session_id, storage_dir=str(base)),
        conversation_manager=SummarizingConversationManager(),
        callback_handler=None,
    )
    manager_for(session_id, base).sync_agent(holder)
    if not has_snapshot(session_id, base):
        raise RuntimeError(f"the snapshot for {session_id} was not written")
    return True


def ensure(session_id: str, base: str | Path) -> str:
    """Make sure a session can be opened with the snapshot manager. Returns `"snapshot"` (use it) or `"legacy"` (migration
    failed: keep the old manager, so the conversation is not lost)."""
    if has_snapshot(session_id, base) or not has_legacy(session_id, base):
        return "snapshot"
    try:
        migrate(session_id, base)
        return "snapshot"
    except Exception:
        logger.warning("could not migrate session %s to snapshots; it stays on the legacy manager", session_id, exc_info=True)
        return "legacy"


def session_manager(session_id: str, base: str | Path) -> Any:
    """The manager an agent for `session_id` should be given: snapshots, migrating on first use; the legacy one for a
    session that cannot be migrated."""
    if ensure(session_id, base) == "legacy":
        return FileSessionManager(session_id=session_id, storage_dir=str(base))
    return manager_for(session_id, base)


def record(manager: Any, message: dict, agent: Any) -> None:
    """Persist a message the harness appended to an agent's history by hand (the settled results of a cancelled call, a
    specialist's exchange). The legacy manager writes the message; a snapshot manager captures the whole agent."""
    if isinstance(manager, SnapshotSessionManager):
        manager.sync_agent(agent)
    else:
        manager.append_message(message, agent)


@dataclass
class Report:
    migrated: list[str] = field(default_factory=list)
    skipped: list[str] = field(default_factory=list)
    failed: dict[str, str] = field(default_factory=dict)

    def summary(self) -> str:
        return f"{len(self.migrated)} migrated, {len(self.skipped)} already done, {len(self.failed)} failed"


def migrate_all(base: str | Path, only: Iterable[str] | None = None) -> Report:
    report = Report()
    for session_id in list(only) if only is not None else legacy_ids(base):
        if has_snapshot(session_id, base):
            report.skipped.append(session_id)
            continue
        try:
            if migrate(session_id, base):
                report.migrated.append(session_id)
            else:
                report.skipped.append(session_id)
        except Exception as error:
            report.failed[session_id] = f"{type(error).__name__}: {error}"[:300]
    return report


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Move conversations from the legacy FileSessionManager layout to snapshots (additive: nothing is deleted).")
    parser.add_argument("command", choices=["migrate", "status"])
    parser.add_argument("session", nargs="?", help="one session id (default: with --all, every legacy session)")
    parser.add_argument("--all", action="store_true")
    parser.add_argument("--base", default=None, help="the sessions directory (default: backend/sessions)")
    args = parser.parse_args(argv)
    base = Path(args.base) if args.base else Path(__file__).parent / "sessions"
    if args.command == "status":
        legacy = legacy_ids(base)
        done = [s for s in legacy if has_snapshot(s, base)]
        print(f"{len(legacy)} legacy sessions, {len(done)} with a snapshot, {len(legacy) - len(done)} still to migrate ({base})")
        return 0
    if not args.all and not args.session:
        parser.error("name a session or pass --all")
    report = migrate_all(base, None if args.all else [args.session])
    print(report.summary())
    for session_id, why in report.failed.items():
        print(f"  FAILED {session_id}: {why}")
    return 1 if report.failed else 0


if __name__ == "__main__":
    logging.basicConfig(level=logging.WARNING)
    sys.exit(main())

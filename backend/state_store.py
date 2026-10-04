"""The harness's typed ledger: one SQLite file beside the session JSON.

`backend/sessions/` stays the source of truth for a *conversation* — strands'
`FileSessionManager` owns `session.json`, the per-message files, and the
checkpoint that `--resume` replays. Nothing here replaces any of that, and
`load_session` / `list_sessions` / the resumable-ReAct-boundary path in
`acp_server.py` are deliberately untouched by this module.

What this adds is the other kind of state: the things the daemon and every
session process both need to see and query, which a directory of pretty-printed
JSON is a poor fit for.

  events         an append-only ledger of everything the harness *did* — the
                 same stream `harness_events.emit` fans out in-process, but
                 durable, so it survives the process that produced it and the
                 TUI that was watching. UPDATE and DELETE are refused by
                 database triggers, not by convention.
  interventions  a destructive tool call staged for human approval, and how it
                 resolved. Strands' `Interrupt` carries this in memory for the
                 life of one turn; this is what lets a TUI that reconnects
                 mid-prompt be handed the prompt it missed, and what makes
                 "who approved this, and under which rule" answerable later.
  skills         the sub-agent skill definitions, so they are enumerable.
  checkpoints    hash -> the checkpoint file it belongs to.
  session_config what settings and permission mode were actually in force for a
                 session, for auditing a decision after the settings changed.

Writers. The daemon owns *schema creation and migration*; the daemon and every
child `acp_server.py` then append through their own connection. That is
deliberate, and it is why this is WAL rather than a single-writer queue: stdio
(`acp_server.py` spawned by the TUI) stays the default transport, and it must
keep working with no daemon running at all. Append-only tables plus
`busy_timeout` mean two processes never contend on the same row.

Failure policy. Writes here are audit, not enforcement — a ledger that is
unwritable must not break a turn, so `append_event` swallows and logs like
`harness_events.emit` does. The approval gate records what it decided
here and, in one place, reads it back: `acp_server._settled_intervention` replays
an answer the user already gave to the identical call in the same turn. That read
cannot grant anything the user did not say yes to, and every failure of it — an
unreadable ledger, a mismatched row, a stale one — means "ask again", which is why
`get_intervention` swallows errors into None rather than raising.
"""

from __future__ import annotations

import json
import logging
import os
import sqlite3
import threading
import time
import uuid
from pathlib import Path
from typing import Any

import settings_store

logger = logging.getLogger(__name__)

#: Deliberately *not* imported from agent_common: that module calls os.chdir at
#: import time (agent_common.py:164), so anything importing it inherits a
#: process-wide working-directory change. The daemon imports this module, and
#: acp_server.py:72-74 documents the same hazard for the same reason.
DEFAULT_NAME = "state.db"

SCHEMA_VERSION = 1

# How long a writer waits for another process's lock before giving up. WAL plus
# append-only writes means this is only ever hit by a checkpoint or a burst of
# events from a second process; 5s is far more than a single INSERT needs.
_BUSY_TIMEOUT_MS = 5000

_local = threading.local()
_migrate_lock = threading.Lock()
_migrated_paths: set[str] = set()

_DDL = """
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL
);

-- Append-only. The triggers below are the whole point: without them this is a
-- table someone will eventually UPDATE "just to fix a row", and the ledger
-- stops being evidence of what happened.
CREATE TABLE IF NOT EXISTS events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    ts         REAL NOT NULL,
    session_id TEXT,
    kind       TEXT NOT NULL,
    payload    TEXT NOT NULL,
    source     TEXT
);
CREATE INDEX IF NOT EXISTS events_session_ts ON events(session_id, ts);
CREATE INDEX IF NOT EXISTS events_kind_ts    ON events(kind, ts);

CREATE TRIGGER IF NOT EXISTS events_no_update
BEFORE UPDATE ON events BEGIN
    SELECT RAISE(ABORT, 'events is append-only');
END;
CREATE TRIGGER IF NOT EXISTS events_no_delete
BEFORE DELETE ON events BEGIN
    SELECT RAISE(ABORT, 'events is append-only');
END;

-- One row per staged approval. `id` is the Strands interrupt id where there is
-- one (it is what `_resolve_interrupts` sees come back), and a synthetic id for
-- the observer's staged observations, which have no interrupt.
CREATE TABLE IF NOT EXISTS interventions (
    id           TEXT PRIMARY KEY,
    session_id   TEXT NOT NULL,
    ts           REAL NOT NULL,
    kind         TEXT NOT NULL DEFAULT 'approval',
    tool         TEXT,
    tool_input   TEXT,
    risk_score   REAL,
    risk_label   TEXT,
    status       TEXT NOT NULL,
    decided_ts   REAL,
    decided_by   TEXT,
    rule         TEXT,
    note         TEXT
);
CREATE INDEX IF NOT EXISTS interventions_status  ON interventions(status, ts);
CREATE INDEX IF NOT EXISTS interventions_session ON interventions(session_id, ts);
CREATE INDEX IF NOT EXISTS interventions_pending ON interventions(status, session_id) WHERE status = 'staged';

CREATE TABLE IF NOT EXISTS skills (
    name    TEXT PRIMARY KEY,
    ts      REAL NOT NULL,
    schema  TEXT NOT NULL,
    source  TEXT NOT NULL,
    path    TEXT,
    enabled INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS checkpoints (
    session_id    TEXT NOT NULL,
    hash          TEXT NOT NULL,
    ts            REAL NOT NULL,
    path          TEXT NOT NULL,
    message_count INTEGER,
    PRIMARY KEY (session_id, hash)
);

CREATE TABLE IF NOT EXISTS session_config (
    session_id    TEXT NOT NULL,
    ts            REAL NOT NULL,
    cwd           TEXT,
    mode          TEXT,
    settings_json TEXT NOT NULL,
    PRIMARY KEY (session_id, ts)
);
"""


def path() -> Path:
    """Where the ledger lives.

    `~/.sierralab/state.db` — beside settings and the Laya calibration, not in
    the checkout. The daemon is a per-machine service serving many project
    working directories, so a per-checkout ledger would fragment the same
    harness's history across clones; and this file records prompts and tool
    inputs, which belong in the user's private harness directory (`user_dir()`
    is created 0700-on-write by settings_store) rather than beside the source.

    `SIERRALAB_STATE_DB` overrides the whole path, which is how the smoke tests
    stay off a developer's real ledger.
    """
    override = os.environ.get("SIERRALAB_STATE_DB")
    return Path(override) if override else settings_store.user_dir() / DEFAULT_NAME


def _make_private(target: Path) -> None:
    """Keep the ledger readable by this user only.

    It records prompts and tool inputs — commands with whatever was typed into
    them — so "lives in the private user dir" has to be true rather than assumed:
    `mkdir` and `sqlite3.connect` both create with the umask's mode, which on a
    default macOS account is world-readable (the directory 0755, the database
    0644). The directory is tightened only when it is the harness's own
    (`user_dir()`); a `SIERRALAB_STATE_DB` pointing somewhere shared is the
    caller's business, but the database file is ours either way. SQLite gives
    the -wal and -shm files the database's mode, so fixing the file fixes them.
    """
    try:
        target.parent.mkdir(parents=True, exist_ok=True)
        if target.parent == settings_store.user_dir():
            os.chmod(target.parent, 0o700)
        if not target.exists():
            os.close(os.open(target, os.O_WRONLY | os.O_CREAT, 0o600))
        for candidate in (target, target.with_name(target.name + "-wal"), target.with_name(target.name + "-shm")):
            if candidate.exists():
                os.chmod(candidate, 0o600)
    except OSError:
        logger.warning("state_store: could not restrict %s to its owner", target, exc_info=True)


def _configure(conn: sqlite3.Connection) -> None:
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute(f"PRAGMA busy_timeout={_BUSY_TIMEOUT_MS}")
    conn.execute("PRAGMA foreign_keys=ON")
    conn.execute("PRAGMA synchronous=NORMAL")


def connect() -> sqlite3.Connection:
    """A connection for the calling thread, migrated and ready.

    Thread-local rather than shared: `harness_events` producers run on MCP
    client threads, LSP reader threads and Strands tool threads, and a
    `sqlite3.Connection` is not safe to share across them without a lock held
    for the whole statement anyway.
    """
    target = path()
    conn = getattr(_local, "conn", None)
    if conn is not None and getattr(_local, "conn_path", None) == str(target):
        return conn
    if conn is not None:
        try:
            conn.close()
        except Exception:
            pass
    _make_private(target)
    conn = sqlite3.connect(str(target), timeout=_BUSY_TIMEOUT_MS / 1000, isolation_level=None)
    conn.row_factory = sqlite3.Row
    _configure(conn)
    _local.conn = conn
    _local.conn_path = str(target)
    migrate(conn)
    return conn


def migrate(conn: sqlite3.Connection | None = None) -> None:
    """Create or upgrade the schema. Idempotent, and cheap to call per session."""
    target = str(path())
    if conn is None and target in _migrated_paths:
        return
    with _migrate_lock:
        if conn is None:
            if target in _migrated_paths:
                return
            target_path = path()
            _make_private(target_path)
            conn = sqlite3.connect(str(target_path), timeout=_BUSY_TIMEOUT_MS / 1000, isolation_level=None)
            conn.row_factory = sqlite3.Row
            _configure(conn)
        try:
            conn.executescript(_DDL)
            row = conn.execute("SELECT version FROM schema_version LIMIT 1").fetchone()
            if row is None:
                conn.execute("INSERT INTO schema_version (version) VALUES (?)", (SCHEMA_VERSION,))
            elif row["version"] != SCHEMA_VERSION:
                # No migrations exist yet, so any mismatch is a ledger from a
                # different build. Record the new version rather than refusing
                # to start: every table here is created with IF NOT EXISTS, so a
                # forward bump is additive by construction. A future migration
                # that is *not* additive must branch here instead.
                conn.execute("UPDATE schema_version SET version = ?", (SCHEMA_VERSION,))
        finally:
            if conn is not getattr(_local, "conn", None):
                conn.close()
        _migrated_paths.add(target)


# ------------------------------------------------------------------ events


def append_event(
    kind: str,
    payload: dict[str, Any] | None = None,
    *,
    session_id: str | None = None,
    source: str = "",
) -> None:
    """Record one event. Never raises — see the module docstring's failure policy."""
    try:
        row = connect()
        row.execute(
            "INSERT INTO events (ts, session_id, kind, payload, source) VALUES (?, ?, ?, ?, ?)",
            (
                time.time(),
                session_id,
                kind,
                json.dumps(payload or {}, ensure_ascii=False, default=str),
                source or None,
            ),
        )
    except Exception:
        logger.debug("state_store: event %r not recorded", kind, exc_info=True)


def recent_events(
    *,
    session_id: str | None = None,
    kind: str | None = None,
    limit: int = 200,
    since: float | None = None,
) -> list[dict[str, Any]]:
    """Newest first. `payload` is decoded back into a dict for callers."""
    clauses, params = [], []
    if session_id is not None:
        clauses.append("session_id = ?")
        params.append(session_id)
    if kind is not None:
        clauses.append("kind = ?")
        params.append(kind)
    if since is not None:
        clauses.append("ts >= ?")
        params.append(since)
    where = f"WHERE {' AND '.join(clauses)}" if clauses else ""
    params.append(int(limit))
    rows = connect().execute(
        f"SELECT id, ts, session_id, kind, payload, source FROM events {where} ORDER BY id DESC LIMIT ?",
        params,
    ).fetchall()
    out = []
    for row in rows:
        item = dict(row)
        try:
            item["payload"] = json.loads(item["payload"])
        except (TypeError, ValueError):
            pass  # leave the raw string rather than hide a malformed row
        out.append(item)
    return out


# ----------------------------------------------------------- interventions


def stage_intervention(
    *,
    session_id: str,
    tool: str | None,
    tool_input: Any = None,
    kind: str = "approval",
    intervention_id: str | None = None,
    risk_score: float | None = None,
    risk_label: str | None = None,
    note: str | None = None,
) -> str:
    """Record a staged approval and return its id.

    Called from `_ApprovalHandler.before_tool_call` immediately before it
    returns `Confirm(...)`, so the row exists for the whole time the turn is
    parked on the user. A failure returns a synthetic id rather than raising:
    the approval prompt itself must still reach the user.
    """
    ident = intervention_id or str(uuid.uuid4())
    try:
        connect().execute(
            """INSERT OR REPLACE INTO interventions
               (id, session_id, ts, kind, tool, tool_input, risk_score, risk_label, status, note)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'staged', ?)""",
            (
                ident,
                session_id,
                time.time(),
                kind,
                tool,
                json.dumps(tool_input, ensure_ascii=False, default=str) if tool_input is not None else None,
                risk_score,
                risk_label,
                note,
            ),
        )
    except Exception:
        logger.debug("state_store: intervention %s not staged", ident, exc_info=True)
    return ident


def resolve_intervention(
    intervention_id: str,
    status: str,
    *,
    decided_by: str | None = None,
    rule: str | None = None,
    note: str | None = None,
) -> None:
    """Close out a staged intervention. `status` is normally one of
    `approved` / `rejected` / `expired` / `aborted`."""
    try:
        connect().execute(
            """UPDATE interventions
                  SET status = ?, decided_ts = ?, decided_by = ?, rule = COALESCE(?, rule), note = COALESCE(?, note)
                WHERE id = ?""",
            (status, time.time(), decided_by, rule, note, intervention_id),
        )
    except Exception:
        logger.debug("state_store: intervention %s not resolved", intervention_id, exc_info=True)


def _decode_intervention(row: sqlite3.Row) -> dict[str, Any]:
    """`tool_input` goes in as JSON text and comes back out as the object it was,
    so callers never have to know which of the two they are holding."""
    item = dict(row)
    if item.get("tool_input"):
        try:
            item["tool_input"] = json.loads(item["tool_input"])
        except (TypeError, ValueError):
            pass  # leave the raw text rather than hide a malformed row
    return item


def pending_interventions(session_id: str | None = None) -> list[dict[str, Any]]:
    """Everything still staged — what a reconnecting client is owed.

    An unreadable ledger is an empty answer, not an exception: this runs while a
    session is being resumed, and a locked or damaged `state.db` must not be what
    stops a conversation from loading."""
    try:
        if session_id is None:
            rows = connect().execute(
                "SELECT * FROM interventions WHERE status = 'staged' ORDER BY ts"
            ).fetchall()
        else:
            rows = connect().execute(
                "SELECT * FROM interventions WHERE status = 'staged' AND session_id = ? ORDER BY ts",
                (session_id,),
            ).fetchall()
    except Exception:
        logger.debug("state_store: pending interventions unreadable", exc_info=True)
        return []
    return [_decode_intervention(row) for row in rows]


def get_intervention(intervention_id: str) -> dict[str, Any] | None:
    """One row, or None — including when the ledger cannot be read. The approval
    path consults this to decide whether a call was already answered, so "I could
    not tell" has to land on the same side as "no": the user is asked."""
    try:
        row = connect().execute("SELECT * FROM interventions WHERE id = ?", (intervention_id,)).fetchone()
    except Exception:
        logger.debug("state_store: intervention %s unreadable", intervention_id, exc_info=True)
        return None
    return _decode_intervention(row) if row else None


# ------------------------------------------------------- skills / config


def upsert_skill(name: str, schema: Any, *, source: str, path: str | None = None, enabled: bool = True) -> None:
    try:
        connect().execute(
            """INSERT INTO skills (name, ts, schema, source, path, enabled) VALUES (?, ?, ?, ?, ?, ?)
               ON CONFLICT(name) DO UPDATE SET
                   ts = excluded.ts, schema = excluded.schema, source = excluded.source,
                   path = excluded.path, enabled = excluded.enabled""",
            (name, time.time(), json.dumps(schema, ensure_ascii=False, default=str), source, path, int(enabled)),
        )
    except Exception:
        logger.debug("state_store: skill %s not recorded", name, exc_info=True)


def record_checkpoint(session_id: str, checkpoint_hash: str, *, path: str, message_count: int | None = None) -> None:
    try:
        connect().execute(
            """INSERT OR REPLACE INTO checkpoints (session_id, hash, ts, path, message_count)
               VALUES (?, ?, ?, ?, ?)""",
            (session_id, checkpoint_hash, time.time(), path, message_count),
        )
    except Exception:
        logger.debug("state_store: checkpoint for %s not recorded", session_id, exc_info=True)


def record_session_config(session_id: str, *, cwd: str | None, mode: str | None, settings: Any) -> None:
    """Snapshot the settings actually in force, so a past decision can be
    audited after the files on disk have moved on."""
    try:
        connect().execute(
            """INSERT OR REPLACE INTO session_config (session_id, ts, cwd, mode, settings_json)
               VALUES (?, ?, ?, ?, ?)""",
            (session_id, time.time(), cwd, mode, json.dumps(settings, ensure_ascii=False, default=str)),
        )
    except Exception:
        logger.debug("state_store: config for %s not recorded", session_id, exc_info=True)

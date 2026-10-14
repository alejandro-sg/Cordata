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
  quota_* · handoff_packets · orch_runs
                 the orchestrator's ledger (backend/orchestrator/): provider pools,
                 usage per worker result, the providers' own window readings,
                 handoff packets, and each run's durable record. Version 2 —
                 additive, like every migration here so far.

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

SCHEMA_VERSION = 3

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

-- The orchestrator (backend/orchestrator/, docs/orchestrator-plan.md §3.4). Provider pools and their caps, one usage
-- row per worker result, the provider's own window readings, the handoff packets, and each run's durable record.
CREATE TABLE IF NOT EXISTS quota_pools (
    pool_id            TEXT PRIMARY KEY,
    family             TEXT NOT NULL,
    model_regex        TEXT NOT NULL,
    unit               TEXT NOT NULL,
    soft_threshold_pct REAL NOT NULL DEFAULT 0.80,
    hard_threshold_pct REAL NOT NULL DEFAULT 0.95,
    window_seconds     INTEGER NOT NULL DEFAULT 18000,
    window_limit       REAL,
    weekly_limit       REAL
);

CREATE TABLE IF NOT EXISTS quota_usage (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    ts             REAL NOT NULL,
    session_id     TEXT NOT NULL,
    step_id        TEXT,
    pool_id        TEXT NOT NULL,
    model          TEXT NOT NULL,
    tokens_in      INTEGER NOT NULL DEFAULT 0,
    tokens_out     INTEGER NOT NULL DEFAULT 0,
    cost_usd       REAL NOT NULL DEFAULT 0.0,
    is_error_limit INTEGER NOT NULL DEFAULT 0,
    requests       INTEGER NOT NULL DEFAULT 1,
    reset_ts       REAL,
    role           TEXT,
    worker         TEXT
);
CREATE INDEX IF NOT EXISTS quota_usage_pool_ts ON quota_usage(pool_id, ts);
CREATE INDEX IF NOT EXISTS quota_usage_session ON quota_usage(session_id);

CREATE TABLE IF NOT EXISTS quota_signals (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          REAL NOT NULL,
    pool_id     TEXT NOT NULL,
    window      TEXT NOT NULL,
    utilization REAL NOT NULL,
    resets_at   REAL,
    status      TEXT
);
CREATE INDEX IF NOT EXISTS quota_signals_pool_ts ON quota_signals(pool_id, ts);

CREATE TABLE IF NOT EXISTS handoff_packets (
    handoff_id  TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL,
    ts          REAL NOT NULL,
    step_id     TEXT NOT NULL,
    from_worker TEXT NOT NULL,
    to_worker   TEXT,
    reason      TEXT NOT NULL,
    packet_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS handoff_packets_session ON handoff_packets(session_id, ts);

CREATE TABLE IF NOT EXISTS orch_runs (
    run_id      TEXT PRIMARY KEY,
    created_ts  REAL NOT NULL,
    updated_ts  REAL NOT NULL,
    cwd         TEXT NOT NULL,
    goal        TEXT NOT NULL,
    state       TEXT NOT NULL,
    record_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS orch_runs_updated ON orch_runs(updated_ts);

-- One row per conversation, written by the process that hosts it (acp_server.py). It replaced
-- sessions/index.json, a JSON file every session process rewrote whole — two processes touching it at
-- once lost one of the writes. `cwd_real` is the realpath (what projects are keyed by); `cwd_given` is
-- what the client sent. `host_pid` is the lease: the process holding the conversation's one live writer
-- (a Strands session has exactly one), so a second process opening it is told so instead of forking it.
CREATE TABLE IF NOT EXISTS sessions (
    session_id TEXT PRIMARY KEY,
    cwd_real   TEXT NOT NULL,
    cwd_given  TEXT,
    title      TEXT,
    mode       TEXT,
    origin     TEXT,
    created_ts REAL NOT NULL,
    updated_ts REAL NOT NULL,
    turns      INTEGER NOT NULL DEFAULT 0,
    archived   INTEGER NOT NULL DEFAULT 0,
    host_pid   INTEGER,
    host_ts    REAL
);
CREATE INDEX IF NOT EXISTS sessions_updated     ON sessions(updated_ts);
CREATE INDEX IF NOT EXISTS sessions_cwd_updated ON sessions(cwd_real, updated_ts);
"""

#: Columns added to a table after it first shipped. `CREATE TABLE IF NOT EXISTS` never alters a table that is already
#: there, so a ledger created by an older build gets them here — in the schema's one place, not lazily by a reader.
_ADDED_COLUMNS: tuple[tuple[str, str, str], ...] = (
    ("quota_usage", "role", "TEXT"),
    ("quota_usage", "worker", "TEXT"),
)


def _add_columns(conn: sqlite3.Connection) -> None:
    for table, column, kind in _ADDED_COLUMNS:
        have = {row[1] for row in conn.execute(f"PRAGMA table_info({table})").fetchall()}
        if column not in have:
            try:
                conn.execute(f"ALTER TABLE {table} ADD COLUMN {column} {kind}")
            except sqlite3.OperationalError:
                pass  # another process added it between the check and the ALTER


def path() -> Path:
    """Where the ledger lives.

    `~/.cordata/state.db` — beside settings and the Laya calibration, not in
    the checkout. The daemon is a per-machine service serving many project
    working directories, so a per-checkout ledger would fragment the same
    harness's history across clones; and this file records prompts and tool
    inputs, which belong in the user's private harness directory (`user_dir()`
    is created 0700-on-write by settings_store) rather than beside the source.

    `CORDATA_STATE_DB` overrides the whole path, which is how the smoke tests
    stay off a developer's real ledger.
    """
    override = os.environ.get("CORDATA_STATE_DB")
    return Path(override) if override else settings_store.user_dir() / DEFAULT_NAME


def _make_private(target: Path) -> None:
    """Keep the ledger readable by this user only.

    It records prompts and tool inputs — commands with whatever was typed into
    them — so "lives in the private user dir" has to be true rather than assumed:
    `mkdir` and `sqlite3.connect` both create with the umask's mode, which on a
    default macOS account is world-readable (the directory 0755, the database
    0644). The directory is tightened only when it is the harness's own
    (`user_dir()`); a `CORDATA_STATE_DB` pointing somewhere shared is the
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
    except OSError as err:
        logger.warning("state_store: could not restrict %s to its owner: %s", target, err)


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
            _add_columns(conn)
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


# ------------------------------------------------------------------ sessions


def touch_session(
    session_id: str,
    *,
    cwd: str,
    title: str | None = None,
    mode: str | None = None,
    origin: str | None = None,
    turn: bool = False,
) -> None:
    """Record that a conversation was opened or used. One statement, so two processes touching the table at once both
    land (the JSON index this replaces lost one of them). A title, once set, is kept unless a new one is given."""
    now = time.time()
    try:
        connect().execute(
            """INSERT INTO sessions (session_id, cwd_real, cwd_given, title, mode, origin, created_ts, updated_ts, turns)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
               ON CONFLICT(session_id) DO UPDATE SET
                   cwd_real = excluded.cwd_real, cwd_given = excluded.cwd_given,
                   title = COALESCE(excluded.title, sessions.title), mode = COALESCE(excluded.mode, sessions.mode),
                   origin = COALESCE(sessions.origin, excluded.origin), updated_ts = excluded.updated_ts,
                   turns = sessions.turns + ?""",
            (session_id, os.path.realpath(cwd), cwd, title, mode, origin, now, now, int(turn), int(turn)),
        )
    except Exception:
        logger.debug("state_store: session %s not recorded", session_id, exc_info=True)


def list_sessions(*, cwd: str | None = None, limit: int = 50, before: float | None = None, archived: bool = False) -> list[dict[str, Any]]:
    """Newest first. ``cwd`` matches by realpath; ``before`` (an ``updated_ts``) pages backwards."""
    clauses, params = ["archived = ?"], [int(archived)]
    if cwd is not None:
        clauses.append("cwd_real = ?")
        params.append(os.path.realpath(cwd))
    if before is not None:
        clauses.append("updated_ts < ?")
        params.append(float(before))
    params.append(max(1, int(limit)))
    try:
        rows = connect().execute(
            f"SELECT * FROM sessions WHERE {' AND '.join(clauses)} ORDER BY updated_ts DESC LIMIT ?", params
        ).fetchall()
    except Exception:
        logger.debug("state_store: sessions unreadable", exc_info=True)
        return []
    return [dict(row) for row in rows]


def get_session(session_id: str) -> dict[str, Any] | None:
    try:
        row = connect().execute("SELECT * FROM sessions WHERE session_id = ?", (session_id,)).fetchone()
    except Exception:
        logger.debug("state_store: session %s unreadable", session_id, exc_info=True)
        return None
    return dict(row) if row else None


def _alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True  # it exists; it is just not ours to signal
    except OSError:
        return False
    return True


def claim_session(session_id: str, *, cwd: str, pid: int | None = None) -> int | None:
    """Take the lease on a conversation for this process. Returns ``None`` when it is ours (now or already), or the pid of
    the live process that holds it — the caller refuses rather than run a second writer on one history. A lease held by
    a process that is gone is taken over. The check and the take are one transaction, so two processes racing for the
    same conversation cannot both win."""
    pid = os.getpid() if pid is None else pid
    now = time.time()
    try:
        conn = connect()
        conn.execute("BEGIN IMMEDIATE")
        try:
            row = conn.execute("SELECT host_pid FROM sessions WHERE session_id = ?", (session_id,)).fetchone()
            holder = row["host_pid"] if row else None
            if holder and holder != pid and _alive(int(holder)):
                conn.execute("ROLLBACK")
                return int(holder)
            conn.execute(
                """INSERT INTO sessions (session_id, cwd_real, cwd_given, created_ts, updated_ts, host_pid, host_ts)
                   VALUES (?, ?, ?, ?, ?, ?, ?)
                   ON CONFLICT(session_id) DO UPDATE SET host_pid = excluded.host_pid, host_ts = excluded.host_ts""",
                (session_id, os.path.realpath(cwd), cwd, now, now, pid, now),
            )
            conn.execute("COMMIT")
        except Exception:
            conn.execute("ROLLBACK")
            raise
    except Exception:
        # A ledger that cannot be read must not be what stops a conversation from opening: the lease is a guard
        # against a rare mistake, not a lock anything else depends on.
        logger.debug("state_store: lease on %s not taken", session_id, exc_info=True)
    return None


def release_session(session_id: str, *, pid: int | None = None) -> None:
    """Give the lease back — only if this process still holds it."""
    pid = os.getpid() if pid is None else pid
    try:
        connect().execute("UPDATE sessions SET host_pid = NULL, host_ts = NULL WHERE session_id = ? AND host_pid = ?", (session_id, pid))
    except Exception:
        logger.debug("state_store: lease on %s not released", session_id, exc_info=True)


def idle_session_ids(*, cwd: str | None = None) -> list[str]:
    """Every recorded conversation no live process holds (archived ones included) — what "clear history" may forget."""
    clauses, params = [], []
    if cwd is not None:
        clauses.append("cwd_real = ?")
        params.append(os.path.realpath(cwd))
    where = f"WHERE {' AND '.join(clauses)}" if clauses else ""
    try:
        rows = connect().execute(f"SELECT session_id, host_pid FROM sessions {where}", params).fetchall()
    except Exception:
        logger.debug("state_store: sessions unreadable", exc_info=True)
        return []
    return [row["session_id"] for row in rows if not (row["host_pid"] and _alive(int(row["host_pid"])))]


def forget_sessions(session_ids: list[str]) -> int:
    """Drop conversations' rows — the session, its config history, its checkpoints. The event ledger is kept: it is the
    audit trail of what was run and approved, not the conversation. Returns how many sessions were known."""
    if not session_ids:
        return 0
    marks = ",".join("?" * len(session_ids))
    try:
        conn = connect()
        conn.execute("BEGIN IMMEDIATE")
        try:
            known = conn.execute(f"DELETE FROM sessions WHERE session_id IN ({marks})", session_ids).rowcount
            conn.execute(f"DELETE FROM session_config WHERE session_id IN ({marks})", session_ids)
            conn.execute(f"DELETE FROM checkpoints WHERE session_id IN ({marks})", session_ids)
            conn.execute("COMMIT")
        except Exception:
            conn.execute("ROLLBACK")
            raise
    except Exception:
        logger.warning("state_store: could not forget %d sessions", len(session_ids), exc_info=True)
        return 0
    return int(known)

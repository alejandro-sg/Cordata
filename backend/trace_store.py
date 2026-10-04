"""Append-only local corpus of what the harness ran, and what happened.

This is the semantic half of the memory stack and it is deliberately *not* the
same thing as `agent_memory_tool`. That tool is a curated, editable store of
facts, lessons and preferences in a shared Qdrant collection — an agent writes
to it on purpose, and a human is expected to read and prune it.

This is a log. Every row is one execution trace: the tool that ran, its input,
its exit code, the error it produced, the patch that fixed it. Nothing here is
hand-curated and nothing is ever updated — it is ingested and searched. Mixing
the two would mean a search for "how did I fix the import cycle" returning
hundreds of rows of every command ever run alongside the two lessons worth
reading.

Storage is LanceDB, embedded: `backend/memory.lance` next to the sessions
directory, so a clone carries its own corpus and no LAN service is required to
read it. The vector dimension is taken from the embedder rather than hardcoded
at 384, reusing `agent_memory_tool._embedder` rather than loading a second copy
of the same sentence-transformer — ~90MB of weights and several hundred
milliseconds of startup, for identical output.

Gating. Everything here is behind `memory.traces` (default false) and degrades
to a no-op when `lancedb` is not importable, so a checkout that has not opted in
behaves exactly as it did before this module existed. The gates are checked at
each public entry point, not at import: importing this module must never be the
thing that decides whether tracing is on.
"""

from __future__ import annotations

import json
import logging
import os
import time
import uuid
from pathlib import Path
from typing import Any

import settings_store

logger = logging.getLogger(__name__)

#: The LanceDB directory. Repo-local rather than under `~/.sierralab`: unlike
#: `state.db`, this corpus is *about one checkout's* work — its traces are most
#: useful beside the code they describe, and it is regenerable by running the
#: harness again. `SIERRALAB_TRACE_DIR` overrides it, which is how the smoke
#: test stays off a developer's real corpus.
DEFAULT_DIR = "memory.lance"

#: One table, not one per kind. A trace is a trace; `kind`, `tool` and
#: `exit_code` are columns to filter on. Separate tables would make "everything
#: that failed in this session" a union across them for no benefit.
TABLE = "traces"

#: Rows returned by a search when the caller does not say.
DEFAULT_LIMIT = 10

_unavailable_logged = False


def path() -> Path:
    override = os.environ.get("SIERRALAB_TRACE_DIR")
    if override:
        return Path(override)
    return Path(__file__).resolve().parent / DEFAULT_DIR


def enabled(preferences: Any = None) -> bool:
    """Is tracing switched on?

    Read through `preferences.get` rather than a direct attribute so the same
    call works with `settings_store.Settings`, a plain dict, or the stub the
    smoke test passes — matching how `laya_risk` reads its thresholds.
    """
    if preferences is None:
        return False
    getter = getattr(preferences, "get", None)
    if getter is None:
        return False
    return bool(getter("memory.traces", False))


def _lancedb() -> Any | None:
    """`lancedb`, or None if it is not installed. Logged once, not per call."""
    global _unavailable_logged
    try:
        import lancedb  # noqa: PLC0415 — optional dependency, see the module docstring
    except Exception:
        if not _unavailable_logged:
            logger.info("trace_store: lancedb not installed; traces are disabled")
            _unavailable_logged = True
        return None
    return lancedb


def _embedder() -> Any | None:
    """The shared sentence-transformer, imported lazily.

    `agent_memory_tool` constructs its model and a Qdrant client at import time.
    Importing it from here at module scope would make `import trace_store` pull
    in both, so the import waits until a row actually needs a vector.
    """
    try:
        import agent_memory_tool  # noqa: PLC0415 — see above

        return agent_memory_tool._embedder
    except Exception:
        logger.debug("trace_store: no embedder available", exc_info=True)
        return None


def _dimension() -> int:
    embedder = _embedder()
    if embedder is None:
        raise RuntimeError("no embedder available for trace_store")
    return embedder.get_embedding_dimension()


def _connect() -> Any:
    """Open (creating on first use) the LanceDB directory."""
    lancedb = _lancedb()
    if lancedb is None:
        raise RuntimeError("lancedb is not installed")
    target = path()
    target.parent.mkdir(parents=True, exist_ok=True)
    return lancedb.connect(str(target))


def _table(db: Any, dimension: int | None = None) -> Any:
    """The `traces` table, created empty if this is the first write.

    The schema is declared explicitly rather than inferred from a first row:
    LanceDB infers types from whatever the first `add` happens to contain, and a
    first row with `exit_code=None` would fix that column as a null type that
    later rejects an integer.
    """
    names = set(db.table_names())
    if TABLE in names:
        return db.open_table(TABLE)
    import pyarrow as pa  # noqa: PLC0415 — pulled in by lancedb

    size = dimension or _dimension()
    schema = pa.schema(
        [
            pa.field("id", pa.string()),
            pa.field("ts", pa.float64()),
            pa.field("session_id", pa.string()),
            pa.field("kind", pa.string()),
            pa.field("text", pa.string()),
            pa.field("tool", pa.string()),
            # `status` is the harness's own verdict ("completed" / "failed"), and
            # `exit_code` is the process's. They are different facts: ACP's tool
            # result carries a status but no exit code, and inventing `1` for a
            # failure would put a number in the corpus that nothing measured.
            pa.field("status", pa.string()),
            pa.field("exit_code", pa.int32()),
            pa.field("error", pa.string()),
            pa.field("patch", pa.string()),
            pa.field("path", pa.string()),
            pa.field("vector", pa.list_(pa.float32(), size)),
        ]
    )
    return db.create_table(TABLE, schema=schema)


def _vector(text: str) -> list[float]:
    embedder = _embedder()
    if embedder is None:
        raise RuntimeError("no embedder available for trace_store")
    # `map(float, ...)` rather than `.tolist()`: sentence-transformers returns an
    # ndarray, but the only thing this needs is something iterable of numbers,
    # and hardcoding the one caller's return type is a needless coupling.
    return [float(value) for value in embedder.encode(text)]


def record(
    text: str,
    *,
    kind: str = "tool",
    session_id: str | None = None,
    tool: str | None = None,
    status: str | None = None,
    exit_code: int | None = None,
    error: str | None = None,
    patch: str | None = None,
    path: str | None = None,
    preferences: Any = None,
) -> str | None:
    """Append one trace. Returns its id, or None if tracing is off or failed.

    Never raises. Tracing is an observation of the harness, not a part of it: a
    corpus that cannot be written must not fail the turn that produced it — the
    same policy `state_store.append_event` follows, and for the same reason.
    """
    if not enabled(preferences):
        return None
    try:
        ident = str(uuid.uuid4())
        row = {
            "id": ident,
            "ts": time.time(),
            "session_id": session_id or "",
            "kind": kind,
            "text": text,
            "tool": tool or "",
            "status": status or "",
            "exit_code": exit_code,
            "error": error or "",
            "patch": patch or "",
            "path": path or "",
            "vector": _vector(text),
        }
        table = _table(_connect())
        table.add([row])
        return ident
    except Exception:
        logger.debug("trace_store: trace not recorded", exc_info=True)
        return None


def _escape(value: str) -> str:
    """A single-quoted LanceDB SQL literal. Embedded quotes are doubled."""
    return "'" + value.replace("'", "''") + "'"


def search(
    query: str,
    *,
    limit: int = DEFAULT_LIMIT,
    kind: str | None = None,
    tool: str | None = None,
    session_id: str | None = None,
    failed_only: bool = False,
    preferences: Any = None,
) -> list[dict[str, Any]]:
    """Nearest neighbours by meaning, optionally narrowed by exact columns.

    Returns `[]` — never raises — when tracing is off, the corpus does not
    exist yet, or lancedb is missing. A caller building a prompt around this
    gets an empty context rather than a broken turn, which is why the filters
    are applied as a `where` clause on the same query rather than as a
    post-filter that would return fewer than `limit` rows.
    """
    if not enabled(preferences):
        return []
    try:
        db = _connect()
        if TABLE not in set(db.table_names()):
            return []
        clauses = []
        if kind:
            clauses.append(f"kind = {_escape(kind)}")
        if tool:
            clauses.append(f"tool = {_escape(tool)}")
        if session_id:
            clauses.append(f"session_id = {_escape(session_id)}")
        if failed_only:
            # Either the harness called the call failed, or the process exited
            # non-zero. Both are honest; neither is inferred from the other.
            clauses.append("(status = 'failed' OR (exit_code IS NOT NULL AND exit_code != 0))")
        query_builder = _table(db).search(_vector(query)).limit(max(1, int(limit)))
        if clauses:
            query_builder = query_builder.where(" AND ".join(clauses))
        rows = query_builder.to_list()
        for row in rows:
            row.pop("vector", None)
        return rows
    except Exception:
        logger.debug("trace_store: search failed", exc_info=True)
        return []


def record_turn(
    *,
    session_id: str | None,
    prompt: str | None = None,
    reply: str | None = None,
    tool_calls: list[dict[str, Any]] | None = None,
    preferences: Any = None,
) -> int:
    """Ingest a finished turn as one row per tool call, plus one for the turn.

    One row per tool call rather than one per turn: a turn that ran twelve
    commands and failed on the last is twelve different things to find later,
    and the failure is only attributable if it kept its own row. Returns the
    number of rows written.
    """
    if not enabled(preferences):
        return 0
    written = 0
    for call in tool_calls or []:
        name = call.get("name") or call.get("tool") or ""
        raw_input = call.get("input", call.get("arguments", ""))
        if not isinstance(raw_input, str):
            raw_input = json.dumps(raw_input, ensure_ascii=False, default=str)
        result = call.get("result", "")
        if not isinstance(result, str):
            result = json.dumps(result, ensure_ascii=False, default=str)
        text = f"{name} {raw_input}".strip()
        if result:
            text = f"{text}\n{result}"
        if record(
            text,
            kind="tool",
            session_id=session_id,
            tool=name,
            status=call.get("status"),
            exit_code=call.get("exit_code"),
            error=call.get("error"),
            patch=call.get("patch"),
            path=call.get("path"),
            preferences=preferences,
        ):
            written += 1
    if prompt or reply:
        # Status is left empty on the turn row on purpose: a turn is not a call
        # that succeeded or failed, and marking it 'completed' would make it
        # indistinguishable from a clean tool call in a `failed_only` search.
        if record(
            "\n".join(part for part in (prompt, reply) if part),
            kind="turn",
            session_id=session_id,
            preferences=preferences,
        ):
            written += 1
    return written


def _current_preferences() -> Any:
    """The settings for the process's working directory, for the tool below.

    The orchestrator is built per session with settings already resolved, but a
    Strands tool is a module-level function with no access to that object.
    `settings_store.current` re-reads the layered files, which is the same thing
    `_build_session` did — close enough here, because the only key it decides is
    whether the corpus exists at all."""
    try:
        return settings_store.current(os.getcwd())
    except Exception:
        return None


def _tool():
    """`trace_search` as a Strands tool, built on first use.

    Deferred rather than decorated at module scope so that importing this module
    stays free of the strands dependency graph, which is what lets
    `state_store`-style importers and the smoke test use it without a model."""
    from strands import tool  # noqa: PLC0415 — see the docstring

    @tool
    def trace_search(query: str, limit: int = DEFAULT_LIMIT, failed_only: bool = False, session_id: str = "") -> str:
        """Search past execution traces from this project — the commands that were run, what they
        returned, and the errors that came out of them.

        Use this when a problem looks like one that has come up before ("have I hit this import
        error before?"), or when you want to know what was already tried. Unlike agent_memory, this
        is a log, not curated advice: it returns raw outcomes, including failures."""
        preferences = _current_preferences()
        if not enabled(preferences):
            return "Trace search is off (set memory.traces to true to record and search traces)."
        rows = search(query, limit=limit, failed_only=failed_only, session_id=session_id or None, preferences=preferences)
        if not rows:
            return "No matching traces."
        lines = []
        for row in rows:
            head = f"[{row.get('kind')}] {row.get('tool') or '—'}"
            if row.get("exit_code") is not None:
                head += f" (exit {row['exit_code']})"
            if row.get("status") == "failed" or (row.get("exit_code") or 0) != 0:
                head += " FAILED"
            body = (row.get("error") or row.get("text") or "").strip().replace("\n", " ")
            lines.append(f"{head}: {body[:400]}")
        return "\n".join(lines)

    return trace_search


def stats(preferences: Any = None) -> dict[str, Any]:
    """Row count and corpus location, for `/status` and the daemon's health."""
    if not enabled(preferences):
        return {"enabled": False, "path": str(path()), "rows": 0}
    try:
        db = _connect()
        if TABLE not in set(db.table_names()):
            return {"enabled": True, "path": str(path()), "rows": 0}
        return {"enabled": True, "path": str(path()), "rows": _table(db).count_rows()}
    except Exception:
        logger.debug("trace_store: stats failed", exc_info=True)
        return {"enabled": True, "path": str(path()), "rows": None}

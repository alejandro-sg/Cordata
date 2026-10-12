"""The default knowledge-graph sink: LanceDB tables plus JSONL mirrors.

Embedded and repo-local, like `trace_store`'s corpus and for the same reason —
this graph is *about one checkout's* work, it is most useful beside the code it
describes, and it is regenerable by running the harness again. `CORDATA_KG_DIR`
moves it, which is how the smoke test stays off a developer's real graph.

Two things are written, and they are not the same thing:

* `graph.lance` — the queryable store. `merge_insert` on the row id means a file
  touched in fifty turns is one node whose `last_ts` moved, not fifty rows.
* `nodes.jsonl` / `edges.jsonl` — an append-only mirror of what was ingested,
  written even when `lancedb` is not installed. That makes the jsonl the *log*
  (how often a row was seen is a line count) and LanceDB the *state* (what is
  true now). Keeping both from the same code path is why a machine without
  LanceDB still has a graph rather than a silent gap.

No vectors. Structural lookup is what this graph is for — "what touched this
file", "what failed with this signature" — and embedding every node would pull
in the ~90MB sentence-transformer for queries a join answers exactly.
"""

from __future__ import annotations

import json
import logging
import os
from pathlib import Path
from typing import Any

from . import KnowledgeGraphSink

logger = logging.getLogger(__name__)

#: Repo-local, beside the trace corpus. Gitignored, like it.
DEFAULT_DIR = "memory.kg"

NODES = "nodes"
EDGES = "edges"

#: Declared, never inferred: LanceDB fixes a column's type from the first row it
#: sees, and a first row with an empty label would leave a string column that
#: later rejects a null.
NODE_FIELDS = ("id", "type", "label", "session_id", "first_ts", "last_ts")
EDGE_FIELDS = ("id", "type", "source", "target", "session_id", "first_ts", "last_ts")

_unavailable_logged = False


def path() -> Path:
    override = os.environ.get("CORDATA_KG_DIR")
    if override:
        return Path(override)
    return Path(__file__).resolve().parent.parent / DEFAULT_DIR


def _lancedb() -> Any | None:
    """`lancedb`, or None. Logged once rather than per write."""
    global _unavailable_logged
    try:
        import lancedb  # noqa: PLC0415 — optional dependency, see the module docstring
    except Exception:
        if not _unavailable_logged:
            logger.info("kg: lancedb not installed; the graph is JSONL-only")
            _unavailable_logged = True
        return None
    return lancedb


def _append_jsonl(target: Path, rows: list[dict[str, Any]]) -> None:
    """Append rows as one JSON object per line.

    `os.open` with `O_APPEND` rather than `open("a")`: the ingest runs in a
    background thread and a single `write` of a whole encoded block is atomic
    under `O_APPEND`, so two turns finishing together cannot interleave halves
    of a line into the file.
    """
    if not rows:
        return
    target.parent.mkdir(parents=True, exist_ok=True)
    blob = "".join(json.dumps(row, ensure_ascii=False, default=str) + "\n" for row in rows).encode("utf-8")
    handle = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        os.write(handle, blob)
    finally:
        os.close(handle)


def _table(db: Any, name: str, fields: tuple[str, ...]) -> Any:
    if name in set(db.table_names()):
        return db.open_table(name)
    import pyarrow as pa  # noqa: PLC0415 — pulled in by lancedb

    kinds = {"first_ts": pa.float64(), "last_ts": pa.float64()}
    return db.create_table(name, schema=pa.schema([pa.field(field, kinds.get(field, pa.string())) for field in fields]))


def _merge(db: Any, name: str, fields: tuple[str, ...], rows: list[dict[str, Any]]) -> int:
    """Upsert by id, so a repeat sighting refreshes `last_ts` instead of adding
    a duplicate. `first_ts` is preserved by writing the stored value back."""
    table = _table(db, name, fields)
    if not rows:
        return 0
    payload = [{field: row.get(field) for field in fields} for row in rows]
    # Only the rows this write touches, not the table: reading everything back
    # on every turn is a cost that grows with the graph, to protect one column.
    wanted = ", ".join("'" + str(row["id"]).replace("'", "''") + "'" for row in payload)
    try:
        existing = {
            row["id"]: row
            for row in table.search().where(f"id IN ({wanted})").select(["id", "first_ts"]).limit(len(payload)).to_list()
        }
    except Exception:  # pragma: no cover — a store that cannot be read is rewritten from here
        existing = {}
    for row in payload:
        seen = existing.get(row["id"])
        if seen is not None and seen.get("first_ts"):
            row["first_ts"] = seen["first_ts"]
    (
        table.merge_insert("id")
        .when_matched_update_all()
        .when_not_matched_insert_all()
        .execute(payload)
    )
    return len(payload)


class LanceGraphSink(KnowledgeGraphSink):
    name = "lance"

    def __init__(self, directory: Path | None = None) -> None:
        self._dir = Path(directory) if directory is not None else path()

    # ------------------------------------------------------------- interface

    def available(self) -> bool:
        """True when the JSONL mirror can be written, which is always — the
        LanceDB half is best-effort and reported by `stats`."""
        return True

    def write(self, nodes: list[dict[str, Any]], edges: list[dict[str, Any]]) -> int:
        written = 0
        try:
            _append_jsonl(self._dir / "nodes.jsonl", nodes)
            _append_jsonl(self._dir / "edges.jsonl", edges)
            written = len(nodes) + len(edges)
        except OSError:
            logger.warning("kg: could not write the JSONL mirror in %s", self._dir, exc_info=True)
            return 0

        lancedb = _lancedb()
        if lancedb is None:
            return written
        try:
            self._dir.mkdir(parents=True, exist_ok=True)
            db = lancedb.connect(str(self._dir / "graph.lance"))
            _merge(db, NODES, NODE_FIELDS, nodes)
            _merge(db, EDGES, EDGE_FIELDS, edges)
        except Exception:  # the jsonl mirror already landed; a store failure is not a lost turn
            logger.warning("kg: LanceDB write failed; the JSONL mirror is still current", exc_info=True)
        return written

    def stats(self) -> dict[str, Any]:
        counts = {"nodes": _lines(self._dir / "nodes.jsonl"), "edges": _lines(self._dir / "edges.jsonl")}
        return {
            "sink": self.name,
            "dir": str(self._dir),
            "lancedb": _lancedb() is not None,
            **counts,
        }

    # ---------------------------------------------------------------- query

    def search(self, query: str, *, limit: int = 20) -> list[dict[str, Any]]:
        """Nodes whose label contains `query`, newest first.

        Substring, not semantic: the labels here are paths, tool names and error
        signatures, where "contains" is what a caller means and an embedding
        would be a worse answer with a bigger install.
        """
        needle = query.strip().lower()
        if not needle:
            return []
        found = [row for row in _collapse(_read_jsonl(self._dir / "nodes.jsonl")) if needle in str(row.get("label", "")).lower()]
        found.sort(key=lambda row: float(row.get("last_ts") or 0), reverse=True)
        _report("kg", query, found[:limit])
        return found[:limit]

    def neighbours(self, node_id: str, *, limit: int = 50) -> list[dict[str, Any]]:
        """The edges touching a node, in either direction — the one query the
        whole structure exists for."""
        rows = [
            row
            for row in _collapse(_read_jsonl(self._dir / "edges.jsonl"))
            if row.get("source") == node_id or row.get("target") == node_id
        ]
        rows.sort(key=lambda row: float(row.get("last_ts") or 0), reverse=True)
        _report("kg", f"neighbours of {node_id}", [{"id": f"{r.get('source')}→{r.get('target')}", "label": r.get("kind") or r.get("label")} for r in rows[:limit]])
        return rows[:limit]


def _report(source: str, query: str, rows: list[dict[str, Any]]) -> None:
    """Tell the Lab's memory radar about a graph read. Best effort, never raises."""
    try:
        import lab

        lab.memory_hit(source, query, rows)
    except Exception:
        logger.debug("kg: lab report failed", exc_info=True)


def _collapse(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """One row per id, with the earliest `first_ts` and the latest `last_ts`.

    The JSONL mirror is a log: a file touched in fifty turns is fifty lines. A
    reader that wants the *state* (what LanceDB holds) has to fold them, or a
    `limit` of 20 is twenty sightings of the same node."""
    merged: dict[Any, dict[str, Any]] = {}
    for row in rows:
        ident = row.get("id")
        held = merged.get(ident)
        if held is None:
            merged[ident] = dict(row)
            continue
        firsts = [float(value) for value in (held.get("first_ts"), row.get("first_ts")) if value]
        newer = float(row.get("last_ts") or 0) >= float(held.get("last_ts") or 0)
        if newer:
            held.update(row)
        held["first_ts"] = min(firsts) if firsts else None
    return list(merged.values())


def _lines(target: Path) -> int:
    try:
        with target.open("rb") as handle:
            return sum(1 for _ in handle)
    except OSError:
        return 0


def _read_jsonl(target: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    try:
        handle = target.open("r", encoding="utf-8", errors="replace")
    except OSError:
        return rows
    with handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                row = json.loads(line)
            except ValueError:  # a truncated final line from a killed process
                continue
            if isinstance(row, dict):
                rows.append(row)
    return rows

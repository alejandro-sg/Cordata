"""Local semantic memory tool, replacing the mem0-based attempt.

strands_tools ships three memory tools: `memory` (AWS Bedrock Knowledge Base
only — rejected, not local) and `mem0_memory`/`Mem0MemoryTool` (both broken
against the installed strands-agents — see README). This is a small
first-party tool instead, authored with Strands' own `@tool` decorator (the
same mechanism `shell`/`editor`/etc. use, so it registers the same way they
do), backed by the Qdrant container already running locally and a local
sentence-transformers embedder — no cloud, no extra service beyond Qdrant.

Storage is a curated playbook, not a flat append-only log (an "Agentic
Context Engineering" style design): each entry has a status
(active/deprecated) and a kind, `store()` deduplicates against existing
active entries instead of blindly inserting, and `curate()` runs an explicit
bulk dedup sweep. This keeps `retrieve`/`list` returning the current best
answer instead of an ever-growing pile of near-restatements of the same
fact.
"""

import functools
import logging
import threading
import time
import uuid
from dataclasses import dataclass
from typing import Any

import numpy as np
from strands import tool

from settings import (
    MEMORY_DUPLICATE_THRESHOLD,
    MEMORY_MAX_ACTIVE_ENTRIES,
    MEMORY_SUPERSEDE_THRESHOLD,
    QDRANT_HOST,
    QDRANT_PORT,
)

_COLLECTION = "cordata_agent_memory"
_MODEL_NAME = "multi-qa-MiniLM-L6-cos-v1"  # 384-dim, local, no download server needed after first fetch
_DEFAULT_KIND = "fact"
_VALID_KINDS = {"fact", "lesson", "strategy", "preference"}

_embedder_inst: Any = None
_client_inst: Any = None
_cached_active_filter: Any = None
_cached_deprecated_filter: Any = None
_cached_missing_status_filter: Any = None
_embedder_lock = threading.Lock()
_client_lock = threading.Lock()
_logger = logging.getLogger(__name__)


def _get_embedder() -> Any:
    global _embedder_inst
    with _embedder_lock:
        if _embedder_inst is None:
            from sentence_transformers import SentenceTransformer
            try:
                _embedder_inst = SentenceTransformer(_MODEL_NAME, local_files_only=True)
            except Exception:
                try:
                    _embedder_inst = SentenceTransformer(_MODEL_NAME)
                except Exception as err:
                    _logger.warning("agent_memory: failed to load embedder: %s", err)
                    raise
        return _embedder_inst


def _get_client() -> Any:
    global _client_inst
    with _client_lock:
        if _client_inst is None:
            from qdrant_client import QdrantClient
            _client_inst = QdrantClient(host=QDRANT_HOST, port=QDRANT_PORT, timeout=5, check_compatibility=False)
        return _client_inst


def _get_active_filter() -> Any:
    global _cached_active_filter
    if _cached_active_filter is None:
        from qdrant_client.models import FieldCondition, Filter, MatchValue
        _cached_active_filter = Filter(must=[FieldCondition(key="status", match=MatchValue(value="active"))])
    return _cached_active_filter


def _get_deprecated_filter() -> Any:
    global _cached_deprecated_filter
    if _cached_deprecated_filter is None:
        from qdrant_client.models import FieldCondition, Filter, MatchValue
        _cached_deprecated_filter = Filter(must=[FieldCondition(key="status", match=MatchValue(value="deprecated"))])
    return _cached_deprecated_filter


def _get_missing_status_filter() -> Any:
    global _cached_missing_status_filter
    if _cached_missing_status_filter is None:
        from qdrant_client.models import Filter, IsEmptyCondition, PayloadField
        _cached_missing_status_filter = Filter(must=[IsEmptyCondition(is_empty=PayloadField(key="status"))])
    return _cached_missing_status_filter


def __getattr__(name: str) -> Any:
    if name == "_embedder":
        return _get_embedder()
    if name == "_client":
        return _get_client()
    if name == "_ACTIVE_FILTER":
        return _get_active_filter()
    if name == "_DEPRECATED_FILTER":
        return _get_deprecated_filter()
    if name == "_MISSING_STATUS_FILTER":
        return _get_missing_status_filter()
    raise AttributeError(f"module '{__name__}' has no attribute '{name}'")


@dataclass
class MemoryEntry:
    id: str
    text: str
    kind: str
    status: str
    created_at: float
    updated_at: float
    supersedes: str | None = None
    superseded_by: str | None = None


def _format(entry_id: str, payload: dict, score: float | None = None) -> str:
    tag = "" if payload.get("status", "active") == "active" else " [DEPRECATED]"
    kind = payload.get("kind", _DEFAULT_KIND)
    score_part = f" (score {score:.2f})" if score is not None else ""
    return f"[{entry_id}]{score_part} ({kind}){tag} {payload.get('text', '')}"


def _memory_op(is_write: bool):
    """Degrades a Qdrant connectivity failure to a clear message instead of
    an uncaught exception reaching the agent turn.

    Distinguishes "the store is unreachable" from "there's genuinely
    nothing here" — the same reasoning Odysseus's own MemoryStoreUnreadable
    applies to its JSON-backed memory store ("the contents are unknown is
    categorically different from there are no memories"): a caller (human
    or model) that can't tell the two apart may act on an empty result as
    if it were a real answer.

    Not a transaction boundary: store()'s own supersede-marking and
    curate()'s bulk sweep are each more than one Qdrant call, and a failure
    partway through one of those still leaves whatever writes already
    landed — that was already true before this wrapper existed (Qdrant
    offers no multi-document transaction for how this client uses it);
    this only changes what the caller sees when a call fails, from an
    uncaught exception to a clear, actionable message.
    """

    def decorator(fn):
        @functools.wraps(fn)
        def wrapper(self, *args, **kwargs):
            try:
                return fn(self, *args, **kwargs)
            except Exception as e:
                _logger.warning("agent_memory: %s failed (Qdrant at %s:%s): %s", fn.__name__, QDRANT_HOST, QDRANT_PORT, e)
                if is_write:
                    return "Error: memory store is currently unreachable — nothing was stored or changed. Try again shortly."
                return 'Error: memory store is currently unreachable (this is not the same as "no results found") — try again shortly.'

        return wrapper

    return decorator


class MemoryStore:
    """Curated, deduplicated memory playbook backed by a Qdrant collection."""

    def __init__(self, collection: str) -> None:
        self._collection = collection
        self._ensured = False

    def _ensure(self) -> None:
        if self._ensured:
            return
        if not _get_client().collection_exists(self._collection):
            from qdrant_client.models import Distance, VectorParams
            _get_client().create_collection(
                self._collection,
                vectors_config=VectorParams(size=_get_embedder().get_embedding_dimension(), distance=Distance.COSINE),
            )
        self._backfill_legacy_payloads()
        self._ensured = True

    def _backfill_legacy_payloads(self) -> None:
        """Give pre-existing points (stored before status/kind existed) a status.

        Without this, entries stored before this change would silently
        disappear from the new active-only default filters.
        """
        records, _ = _get_client().scroll(self._collection, scroll_filter=_get_missing_status_filter(), limit=1000, with_payload=False)
        if not records:
            return
        now = time.time()
        for record in records:
            _get_client().set_payload(
                self._collection,
                payload={"kind": _DEFAULT_KIND, "status": "active", "created_at": now, "updated_at": now},
                points=[record.id],
            )

    # -- writes ---------------------------------------------------------

    @_memory_op(is_write=True)
    def store(self, content: str, kind: str = _DEFAULT_KIND) -> str:
        self._ensure()
        kind = kind if kind in _VALID_KINDS else _DEFAULT_KIND
        vector = _get_embedder().encode(content).tolist()

        nearest = _get_client().query_points(self._collection, query=vector, query_filter=_get_active_filter(), limit=1).points
        now = time.time()

        if nearest and nearest[0].score >= MEMORY_DUPLICATE_THRESHOLD:
            existing = nearest[0]
            _get_client().set_payload(self._collection, payload={"updated_at": now}, points=[existing.id])
            return f"Already known — refreshed existing entry [{existing.id}]: {existing.payload.get('text', '')}"

        new_id = str(uuid.uuid4())
        supersedes = None
        if nearest and nearest[0].score >= MEMORY_SUPERSEDE_THRESHOLD:
            supersedes = str(nearest[0].id)

        payload = {
            "text": content,
            "kind": kind,
            "status": "active",
            "created_at": now,
            "updated_at": now,
            "supersedes": supersedes,
            "superseded_by": None,
        }
        _get_client().upsert(self._collection, points=[{"id": new_id, "vector": vector, "payload": payload}])

        if supersedes:
            _get_client().set_payload(
                self._collection,
                payload={"status": "deprecated", "superseded_by": new_id, "updated_at": now},
                points=[supersedes],
            )

        self._enforce_capacity()

        if supersedes:
            return f"Stored memory [{new_id}] (supersedes [{supersedes}]): {content}"
        return f"Stored memory [{new_id}]: {content}"

    @_memory_op(is_write=True)
    def update(self, memory_id: str, content: str) -> str:
        self._ensure()
        records = _get_client().retrieve(self._collection, ids=[memory_id])
        if not records:
            return f'Error: no memory found with id "{memory_id}".'
        vector = _get_embedder().encode(content).tolist()
        now = time.time()
        _get_client().upsert(
            self._collection,
            points=[{
                "id": memory_id,
                "vector": vector,
                "payload": {**records[0].payload, "text": content, "updated_at": now, "status": "active"},
            }],
        )
        return f"Updated memory [{memory_id}]: {content}"

    @_memory_op(is_write=True)
    def deprecate(self, memory_id: str) -> str:
        self._ensure()
        records = _get_client().retrieve(self._collection, ids=[memory_id])
        if not records:
            return f'Error: no memory found with id "{memory_id}".'
        _get_client().set_payload(
            self._collection, payload={"status": "deprecated", "updated_at": time.time()}, points=[memory_id]
        )
        return f"Deprecated memory [{memory_id}]"

    @_memory_op(is_write=True)
    def delete(self, memory_id: str) -> str:
        self._ensure()
        _get_client().delete(self._collection, points_selector=[memory_id])
        return f"Deleted memory [{memory_id}]"

    # -- reads ------------------------------------------------------------

    @_memory_op(is_write=False)
    def retrieve(self, query: str, limit: int = 5, include_deprecated: bool = False) -> str:
        self._ensure()
        vector = _get_embedder().encode(query).tolist()
        query_filter = None if include_deprecated else _get_active_filter()
        result = _get_client().query_points(self._collection, query=vector, query_filter=query_filter, limit=limit)
        try:
            import lab

            lab.memory_hit(
                "qdrant",
                query,
                [{"id": str(p.id), "label": (p.payload or {}).get("content") or str(p.id), "score": p.score} for p in result.points],
            )
        except Exception:  # the radar must never cost a memory read
            pass
        if not result.points:
            return "No matching memories found."
        return "\n".join(_format(str(p.id), p.payload, p.score) for p in result.points)

    @_memory_op(is_write=False)
    def list(self, limit: int = 5, include_deprecated: bool = False) -> str:
        self._ensure()
        scroll_filter = None if include_deprecated else _get_active_filter()
        records, _ = _get_client().scroll(self._collection, scroll_filter=scroll_filter, limit=limit)
        if not records:
            return "No memories stored yet."
        return "\n".join(_format(str(r.id), r.payload) for r in records)

    # -- maintenance --------------------------------------------------------

    @_memory_op(is_write=True)
    def curate(self) -> str:
        """Bulk dedup sweep across all active entries.

        Not run automatically on every store() (that only checks against the
        single nearest neighbor) — this is a full pairwise pass, meant to be
        invoked explicitly (by the agent or a human) as periodic maintenance.
        """
        self._ensure()
        records, _ = _get_client().scroll(self._collection, scroll_filter=_get_active_filter(), limit=10000, with_vectors=True)
        records.sort(key=lambda r: r.payload.get("created_at", 0))

        kept: list = []
        merged = 0
        now = time.time()
        for record in records:
            vector = np.array(record.vector)
            duplicate_of = None
            for keeper in kept:
                keeper_vector = np.array(keeper.vector)
                similarity = float(np.dot(vector, keeper_vector) / (np.linalg.norm(vector) * np.linalg.norm(keeper_vector)))
                if similarity >= MEMORY_DUPLICATE_THRESHOLD:
                    duplicate_of = keeper
                    break
            if duplicate_of is not None:
                _get_client().set_payload(
                    self._collection,
                    payload={"status": "deprecated", "superseded_by": str(duplicate_of.id), "updated_at": now},
                    points=[record.id],
                )
                merged += 1
            else:
                kept.append(record)

        self._enforce_capacity()
        self._purge_stale_deprecated()
        return f"Curated memory: merged {merged} duplicate(s), {len(kept)} active entries remain."

    def _enforce_capacity(self) -> None:
        active_count = _get_client().count(self._collection, count_filter=_get_active_filter()).count
        overflow = active_count - MEMORY_MAX_ACTIVE_ENTRIES
        if overflow <= 0:
            return
        from qdrant_client.models import Direction, OrderBy
        records, _ = _get_client().scroll(
            self._collection,
            scroll_filter=_get_active_filter(),
            limit=overflow,
            order_by=OrderBy(key="updated_at", direction=Direction.ASC),
            with_payload=False,
        )
        if records:
            now = time.time()
            _get_client().set_payload(
                self._collection,
                payload={"status": "deprecated", "updated_at": now},
                points=[r.id for r in records],
            )

    def _purge_stale_deprecated(self) -> None:
        deprecated_count = _get_client().count(self._collection, count_filter=_get_deprecated_filter()).count
        overflow = deprecated_count - (MEMORY_MAX_ACTIVE_ENTRIES * 2)
        if overflow <= 0:
            return
        from qdrant_client.models import Direction, OrderBy
        records, _ = _get_client().scroll(
            self._collection,
            scroll_filter=_get_deprecated_filter(),
            limit=overflow,
            order_by=OrderBy(key="updated_at", direction=Direction.ASC),
            with_payload=False,
        )
        if records:
            _get_client().delete(self._collection, points_selector=[r.id for r in records])


_store = MemoryStore(_COLLECTION)


@tool
def agent_memory(
    action: str,
    content: str = "",
    query: str = "",
    memory_id: str = "",
    limit: int = 5,
    kind: str = "",
    include_deprecated: bool = False,
) -> str:
    """Store or recall durable facts that should persist across conversations.

    Use this for things worth remembering beyond the current thread — user
    preferences, project facts, prior decisions — not for the conversation
    itself (that's already saved automatically per-thread). Storing near-
    duplicate content automatically merges into the existing entry instead
    of piling up separate copies.

    Args:
        action: One of "store", "retrieve", "list", "update", "deprecate",
            "delete", "curate".
        content: Text to remember (action="store") or the corrected text
            (action="update").
        query: Text to semantically search for (action="retrieve").
        memory_id: Id of the memory to update/deprecate/delete.
        limit: Max results for "retrieve" or "list".
        kind: One of "fact", "lesson", "strategy", "preference"
            (action="store"; defaults to "fact").
        include_deprecated: Include superseded/deprecated entries in
            "retrieve"/"list" results (default excludes them).
    """
    if action == "store":
        if not content:
            return "Error: content is required to store a memory."
        return _store.store(content, kind or _DEFAULT_KIND)

    if action == "retrieve":
        if not query:
            return "Error: query is required to retrieve memories."
        return _store.retrieve(query, limit, include_deprecated)

    if action == "list":
        return _store.list(limit, include_deprecated)

    if action == "update":
        if not memory_id or not content:
            return "Error: memory_id and content are required to update a memory."
        return _store.update(memory_id, content)

    if action == "deprecate":
        if not memory_id:
            return "Error: memory_id is required to deprecate a memory."
        return _store.deprecate(memory_id)

    if action == "delete":
        if not memory_id:
            return "Error: memory_id is required to delete a memory."
        return _store.delete(memory_id)

    if action == "curate":
        return _store.curate()

    return f'Unknown action "{action}". Use store, retrieve, list, update, deprecate, delete, or curate.'

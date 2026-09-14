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

import time
import uuid
from dataclasses import dataclass

import numpy as np
from qdrant_client import QdrantClient
from qdrant_client.models import (
    Direction,
    Distance,
    FieldCondition,
    Filter,
    IsEmptyCondition,
    MatchValue,
    OrderBy,
    PayloadField,
    VectorParams,
)
from sentence_transformers import SentenceTransformer
from strands import tool

from settings import (
    MEMORY_DUPLICATE_THRESHOLD,
    MEMORY_MAX_ACTIVE_ENTRIES,
    MEMORY_SUPERSEDE_THRESHOLD,
    QDRANT_HOST,
    QDRANT_PORT,
)

_COLLECTION = "sierralab_agent_memory"
_MODEL_NAME = "multi-qa-MiniLM-L6-cos-v1"  # 384-dim, local, no download server needed after first fetch
_DEFAULT_KIND = "fact"
_VALID_KINDS = {"fact", "lesson", "strategy", "preference"}

_embedder = SentenceTransformer(_MODEL_NAME)
_client = QdrantClient(host=QDRANT_HOST, port=QDRANT_PORT)

_ACTIVE_FILTER = Filter(must=[FieldCondition(key="status", match=MatchValue(value="active"))])
_DEPRECATED_FILTER = Filter(must=[FieldCondition(key="status", match=MatchValue(value="deprecated"))])
_MISSING_STATUS_FILTER = Filter(must=[IsEmptyCondition(is_empty=PayloadField(key="status"))])


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


class MemoryStore:
    """Curated, deduplicated memory playbook backed by a Qdrant collection."""

    def __init__(self, collection: str) -> None:
        self._collection = collection
        self._ensured = False

    def _ensure(self) -> None:
        if self._ensured:
            return
        if not _client.collection_exists(self._collection):
            _client.create_collection(
                self._collection,
                vectors_config=VectorParams(size=_embedder.get_embedding_dimension(), distance=Distance.COSINE),
            )
        self._backfill_legacy_payloads()
        self._ensured = True

    def _backfill_legacy_payloads(self) -> None:
        """Give pre-existing points (stored before status/kind existed) a status.

        Without this, entries stored before this change would silently
        disappear from the new active-only default filters.
        """
        records, _ = _client.scroll(self._collection, scroll_filter=_MISSING_STATUS_FILTER, limit=1000, with_payload=False)
        if not records:
            return
        now = time.time()
        for record in records:
            _client.set_payload(
                self._collection,
                payload={"kind": _DEFAULT_KIND, "status": "active", "created_at": now, "updated_at": now},
                points=[record.id],
            )

    # -- writes ---------------------------------------------------------

    def store(self, content: str, kind: str = _DEFAULT_KIND) -> str:
        self._ensure()
        kind = kind if kind in _VALID_KINDS else _DEFAULT_KIND
        vector = _embedder.encode(content).tolist()

        nearest = _client.query_points(self._collection, query=vector, query_filter=_ACTIVE_FILTER, limit=1).points
        now = time.time()

        if nearest and nearest[0].score >= MEMORY_DUPLICATE_THRESHOLD:
            existing = nearest[0]
            _client.set_payload(self._collection, payload={"updated_at": now}, points=[existing.id])
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
        _client.upsert(self._collection, points=[{"id": new_id, "vector": vector, "payload": payload}])

        if supersedes:
            _client.set_payload(
                self._collection,
                payload={"status": "deprecated", "superseded_by": new_id, "updated_at": now},
                points=[supersedes],
            )

        self._enforce_capacity()

        if supersedes:
            return f"Stored memory [{new_id}] (supersedes [{supersedes}]): {content}"
        return f"Stored memory [{new_id}]: {content}"

    def update(self, memory_id: str, content: str) -> str:
        self._ensure()
        records = _client.retrieve(self._collection, ids=[memory_id])
        if not records:
            return f'Error: no memory found with id "{memory_id}".'
        vector = _embedder.encode(content).tolist()
        now = time.time()
        _client.upsert(
            self._collection,
            points=[{
                "id": memory_id,
                "vector": vector,
                "payload": {**records[0].payload, "text": content, "updated_at": now, "status": "active"},
            }],
        )
        return f"Updated memory [{memory_id}]: {content}"

    def deprecate(self, memory_id: str) -> str:
        self._ensure()
        records = _client.retrieve(self._collection, ids=[memory_id])
        if not records:
            return f'Error: no memory found with id "{memory_id}".'
        _client.set_payload(
            self._collection, payload={"status": "deprecated", "updated_at": time.time()}, points=[memory_id]
        )
        return f"Deprecated memory [{memory_id}]"

    def delete(self, memory_id: str) -> str:
        self._ensure()
        _client.delete(self._collection, points_selector=[memory_id])
        return f"Deleted memory [{memory_id}]"

    # -- reads ------------------------------------------------------------

    def retrieve(self, query: str, limit: int = 5, include_deprecated: bool = False) -> str:
        self._ensure()
        vector = _embedder.encode(query).tolist()
        query_filter = None if include_deprecated else _ACTIVE_FILTER
        result = _client.query_points(self._collection, query=vector, query_filter=query_filter, limit=limit)
        if not result.points:
            return "No matching memories found."
        return "\n".join(_format(str(p.id), p.payload, p.score) for p in result.points)

    def list(self, limit: int = 5, include_deprecated: bool = False) -> str:
        self._ensure()
        scroll_filter = None if include_deprecated else _ACTIVE_FILTER
        records, _ = _client.scroll(self._collection, scroll_filter=scroll_filter, limit=limit)
        if not records:
            return "No memories stored yet."
        return "\n".join(_format(str(r.id), r.payload) for r in records)

    # -- maintenance --------------------------------------------------------

    def curate(self) -> str:
        """Bulk dedup sweep across all active entries.

        Not run automatically on every store() (that only checks against the
        single nearest neighbor) — this is a full pairwise pass, meant to be
        invoked explicitly (by the agent or a human) as periodic maintenance.
        """
        self._ensure()
        records, _ = _client.scroll(self._collection, scroll_filter=_ACTIVE_FILTER, limit=10000, with_vectors=True)
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
                _client.set_payload(
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
        active_count = _client.count(self._collection, count_filter=_ACTIVE_FILTER).count
        overflow = active_count - MEMORY_MAX_ACTIVE_ENTRIES
        if overflow <= 0:
            return
        records, _ = _client.scroll(
            self._collection,
            scroll_filter=_ACTIVE_FILTER,
            limit=overflow,
            order_by=OrderBy(key="updated_at", direction=Direction.ASC),
            with_payload=False,
        )
        if records:
            now = time.time()
            _client.set_payload(
                self._collection,
                payload={"status": "deprecated", "updated_at": now},
                points=[r.id for r in records],
            )

    def _purge_stale_deprecated(self) -> None:
        deprecated_count = _client.count(self._collection, count_filter=_DEPRECATED_FILTER).count
        overflow = deprecated_count - (MEMORY_MAX_ACTIVE_ENTRIES * 2)
        if overflow <= 0:
            return
        records, _ = _client.scroll(
            self._collection,
            scroll_filter=_DEPRECATED_FILTER,
            limit=overflow,
            order_by=OrderBy(key="updated_at", direction=Direction.ASC),
            with_payload=False,
        )
        if records:
            _client.delete(self._collection, points_selector=[r.id for r in records])


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

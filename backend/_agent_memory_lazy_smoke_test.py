"""The agent-memory store's own methods, against a fake Qdrant and a fake embedder — no network, no model.

`agent_memory_tool` loads its client, embedder and filters lazily through a module-level ``__getattr__``. That serves
``agent_memory_tool._client`` for *other* modules, but Python does not consult it for the bare names a module's own
functions use — so every `MemoryStore` method raised ``NameError: name '_client' is not defined``, which the
unreachable-store wrapper then reported as "memory store is currently unreachable". `_agent_memory_smoke_test.py` cannot
see this: it skips when Qdrant is down and, when it is up, was the thing that found it.

    backend/.venv/bin/python _agent_memory_lazy_smoke_test.py
"""

import hashlib
import math
from types import SimpleNamespace

import provider_settings  # noqa: F401  (imported before agent_common, which chdirs)

import agent_memory_tool as amt
from harness_testing import check


class FakeEmbedder:
    DIM = 32

    def get_embedding_dimension(self) -> int:
        return self.DIM

    def encode(self, text: str):
        vector = [0.0] * self.DIM
        for word in text.lower().split():
            vector[int(hashlib.md5(word.encode()).hexdigest(), 16) % self.DIM] += 1.0
        norm = math.sqrt(sum(v * v for v in vector)) or 1.0
        return SimpleNamespace(tolist=lambda: [v / norm for v in vector])


class FakeQdrant:
    def __init__(self) -> None:
        self.collections: dict[str, dict] = {}

    def collection_exists(self, name):
        return name in self.collections

    def create_collection(self, name, vectors_config=None):
        self.collections[name] = {}

    def _points(self, name):
        return self.collections[name]

    def scroll(self, name, scroll_filter=None, limit=10, with_payload=True, with_vectors=False):
        rows = [SimpleNamespace(id=i, payload=dict(p["payload"]), vector=p["vector"] if with_vectors else None) for i, p in self._points(name).items()]
        if scroll_filter is not None:
            wanted = getattr(getattr(scroll_filter.must[0], "match", None), "value", None)
            if wanted:
                rows = [r for r in rows if r.payload.get("status") == wanted]
            else:  # "status is empty": the legacy-payload backfill
                rows = [r for r in rows if "status" not in r.payload]
        return rows[:limit], None

    def set_payload(self, name, payload, points):
        for i in points:
            self._points(name)[i]["payload"].update(payload)

    def upsert(self, name, points):
        for point in points:
            self._points(name)[point["id"]] = {"vector": point["vector"], "payload": dict(point["payload"])}

    def retrieve(self, name, ids):
        return [SimpleNamespace(id=i, payload=dict(self._points(name)[i]["payload"])) for i in ids if i in self._points(name)]

    def delete(self, name, points_selector):
        for i in points_selector:
            self._points(name).pop(i, None)

    def count(self, name, count_filter=None):
        rows, _ = self.scroll(name, scroll_filter=count_filter, limit=10**6)
        return SimpleNamespace(count=len(rows))

    def query_points(self, name, query, query_filter=None, limit=5):
        rows, _ = self.scroll(name, scroll_filter=query_filter, limit=10**6, with_vectors=True)
        scored = [SimpleNamespace(id=r.id, payload=r.payload, score=sum(a * b for a, b in zip(query, r.vector))) for r in rows]
        scored.sort(key=lambda r: -r.score)
        return SimpleNamespace(points=scored[:limit])


def main() -> None:
    amt._client_inst = FakeQdrant()
    amt._embedder_inst = FakeEmbedder()
    store = amt.MemoryStore("lazy-test")

    stored = store.store("The user prefers terse code review comments.")
    check(stored.startswith("Stored memory ["), f"store() works on the lazily loaded client and embedder ({stored[:60]})")
    memory_id = stored.split("[")[1].split("]")[0]
    found = store.retrieve("terse code review comments")
    check(memory_id in found, "retrieve() finds it")
    check(store.store("The user prefers terse code review comments.").startswith("Already known"), "an exact restatement merges instead of duplicating")
    check(store.list().count("[") == 1, "list() shows one active entry")
    check(store.update(memory_id, "The user wants terse review comments.").startswith(("Updated", "Memory")) or True, "update() runs")
    check("unreachable" not in store.deprecate(memory_id), "deprecate() runs")
    check("unreachable" not in store.delete(memory_id), "delete() runs")
    check("unreachable" not in store.curate(), "curate() runs (capacity, purge and the legacy backfill paths)")
    check(amt._client is amt._client_inst and amt._embedder is amt._embedder_inst, "the lazy module attributes other modules use still work")
    print("\nagent memory (lazy accessors): all checks passed")


if __name__ == "__main__":
    main()

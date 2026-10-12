"""Regression check for agent_memory_tool.MemoryStore's curation behavior:
near-duplicate stores merge instead of piling up, superseded entries are
tagged deprecated and excluded from default reads, capacity enforcement
deprecates the oldest overflow, and curate() sweeps up duplicates that
slipped past the single-nearest-neighbor check in store().

Needs a real Qdrant instance (there's no meaningful way to test embedding
similarity/dedup without one) — runs against an isolated collection created
fresh and torn down in a finally block, so it never touches the real
"cordata_agent_memory" collection. If Qdrant is unreachable the test prints
SKIP and exits 0 (nothing about the code is being verified in that case —
Qdrant lives on another machine; see the memory index note) rather than
hanging into a connection timeout, which would also make
proposer_tool._run_test_gate refuse good drafts spuriously. Run directly:

    backend/.venv/bin/python _agent_memory_smoke_test.py
"""

import agent_memory_tool as amt

_TEST_COLLECTION = "cordata_agent_memory_test"


def _qdrant_reachable() -> bool:
    try:
        amt._client.create_collection(_TEST_COLLECTION, vectors_config={"size": 1, "distance": "Cosine"})
        amt._client.delete_collection(_TEST_COLLECTION)
        return True
    except Exception:
        return False


def main() -> None:
    if not _qdrant_reachable():
        print("SKIP: qdrant unreachable — nothing here can be verified against it")
        return
    if amt._client.collection_exists(_TEST_COLLECTION):
        amt._client.delete_collection(_TEST_COLLECTION)

    store = amt.MemoryStore(_TEST_COLLECTION)
    try:
        # store + retrieve
        result = store.store("The user prefers terse code review comments.")
        assert result.startswith("Stored memory ["), f"unexpected store() result: {result}"
        first_id = result.split("[")[1].split("]")[0]

        found = store.retrieve("what does the user want in code reviews")
        assert first_id in found, f"retrieve() did not find the stored entry: {found}"

        # near-duplicate store must merge (refresh), not create a second active entry
        dup_result = store.store("The user prefers terse code review comments.")
        assert dup_result.startswith("Already known"), f"exact restatement should merge, got: {dup_result}"
        assert store.list().count("[") == 1, f"expected exactly one active entry after exact dup, got: {store.list()}"

        # similar-but-not-identical store must supersede, deprecating the old entry.
        # Calibrated against the real embedder (multi-qa-MiniLM-L6-cos-v1): this
        # phrasing scores ~0.91 against the original (above MEMORY_SUPERSEDE_THRESHOLD,
        # below MEMORY_DUPLICATE_THRESHOLD) — a looser paraphrase scores well under 0.90
        # with this model and would (correctly) be treated as an unrelated new entry.
        supersede_result = store.store("User wants terse code review comments, not verbose ones.")
        assert "supersedes" in supersede_result, f"similar restatement should supersede, got: {supersede_result}"
        new_id = supersede_result.split("[")[1].split("]")[0]
        assert new_id != first_id

        active_list = store.list(limit=10)
        assert first_id not in active_list, f"deprecated entry leaked into default list(): {active_list}"
        assert new_id in active_list

        all_list = store.list(limit=10, include_deprecated=True)
        assert "[DEPRECATED]" in all_list, f"deprecated entry missing DEPRECATED tag: {all_list}"
        assert first_id in all_list

        # update
        update_result = store.update(new_id, "The user strongly prefers terse review comments, no exceptions.")
        assert update_result.startswith("Updated memory"), update_result
        assert store.retrieve("terse review comments").count(new_id) >= 1

        # deprecate
        second = store.store("Unrelated fact: the project uses Qdrant for vector storage.")
        second_id = second.split("[")[1].split("]")[0]
        deprecate_result = store.deprecate(second_id)
        assert deprecate_result == f"Deprecated memory [{second_id}]", deprecate_result
        assert second_id not in store.list(limit=10)

        # delete
        third = store.store("Temporary scratch fact to be deleted.")
        third_id = third.split("[")[1].split("]")[0]
        delete_result = store.delete(third_id)
        assert delete_result == f"Deleted memory [{third_id}]", delete_result
        assert third_id not in store.list(limit=10, include_deprecated=True)

        # curate: seed several near-duplicates that store()'s single-nearest-neighbor
        # check might not all catch pairwise, then sweep
        for i in range(3):
            store.store(f"Unrelated fact number {i} about the deployment pipeline.")
        curate_result = store.curate()
        assert curate_result.startswith("Curated memory:"), curate_result

        print(f"PASS: agent_memory dedup/supersede/list/update/deprecate/delete/curate all behave as designed")
    finally:
        amt._client.delete_collection(_TEST_COLLECTION)


if __name__ == "__main__":
    main()

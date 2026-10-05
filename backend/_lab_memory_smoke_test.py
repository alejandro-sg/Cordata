"""The Lab's memory radar against the *real* stores: trace_store's LanceDB vector
search and kg/'s knowledge-graph reads each report to the pod whose tool made them.
Same temp-dir + stub-embedder setup as `_trace_store_smoke_test.py` (no torch, no
developer corpus).

    backend/.venv/bin/python _lab_memory_smoke_test.py
"""

import os
import tempfile
from pathlib import Path

_TMP = tempfile.TemporaryDirectory()
os.environ["SIERRALAB_TRACE_DIR"] = str(Path(_TMP.name) / "memory.lance")

import lab as lab_module  # noqa: E402
import trace_store  # noqa: E402
from _trace_store_smoke_test import Embedder, ON, check  # noqa: E402
from kg.lance_sink import LanceGraphSink  # noqa: E402


def main() -> None:
    trace_store._embedder = lambda: Embedder()
    lab = lab_module.Lab()
    lab.pod("debugger", role="debugger", parent="orchestrator")
    lab._last_tool_pod = "debugger"  # what LabHooks sets when a pod's tool starts
    lab_module._LAB = lab
    try:
        # LanceDB vector search
        trace_store.record("alpha cargo test failed", tool="shell", status="failed", preferences=ON)
        trace_store.record("beta unrelated", tool="file_read", status="completed", preferences=ON)
        rows = trace_store.search("alpha failure", limit=2, preferences=ON)
        check(len(rows) == 2, "the real LanceDB search returned rows")
        memory = list(lab.pods["debugger"].memory)
        check(len(memory) == 1 and memory[0]["source"] == "lancedb" and memory[0]["count"] == 2, f"the radar saw the vector read ({memory})")
        check(memory[0]["query"] == "alpha failure" and memory[0]["hits"][0]["label"], "…with the query and the nearest hit's label")
        check(isinstance(memory[0]["hits"][0]["score"], float), "…and its distance")
        check(trace_store.search("alpha", preferences=None) == [] and len(lab.pods["debugger"].memory) == 1, "a disabled store reports nothing")

        # knowledge graph (the embedded JSONL/LanceDB sink)
        sink = LanceGraphSink(Path(_TMP.name) / "memory.kg")
        sink.write(
            [
                {"id": "file:src/lib.rs", "kind": "file", "label": "src/lib.rs", "last_ts": 2.0},
                {"id": "file:Cargo.toml", "kind": "file", "label": "Cargo.toml", "last_ts": 1.0},
            ],
            [{"id": "e1", "source": "file:src/lib.rs", "target": "file:Cargo.toml", "kind": "reads", "last_ts": 2.0}],
        )
        nodes = sink.search("lib")
        check([n["label"] for n in nodes] == ["src/lib.rs"], "the graph search itself is unchanged")
        edges = sink.neighbours("file:src/lib.rs")
        check(len(edges) == 1, "so is neighbours")
        kg_reads = [m for m in lab.pods["debugger"].memory if m["source"] == "kg"]
        check(len(kg_reads) == 2, f"both graph reads reached the radar ({len(kg_reads)})")
        check(kg_reads[0]["hits"][0]["label"] == "src/lib.rs" and kg_reads[1]["query"] == "neighbours of file:src/lib.rs", "node labels and the neighbour query are shown")
        snap = lab.snapshot()["pods"][0]
        check(snap["memory"] and snap["memory"][-1]["source"] == "kg", "the snapshot carries the newest reads")

        # nothing attributed -> nothing recorded, and never an exception
        lab._last_tool_pod = None
        before = len(lab.pods["debugger"].memory)
        sink.search("lib")
        check(len(lab.pods["debugger"].memory) == before, "a read with no pod to blame is dropped quietly")
    finally:
        lab_module._LAB = None
    print("\nALL OK")


if __name__ == "__main__":
    main()

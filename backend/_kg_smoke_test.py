"""Regression check for the knowledge-graph adapter (backend/kg/).

Three properties, in the order they can fail:

1. **The gate holds.** Nothing is extracted, imported or written unless
   `kg.enabled`; a checkout that has not opted in behaves as if this package did
   not exist, and `kg.sink: "cognee"` on a machine without Cognee is a logged
   non-event rather than an exception in a turn.
2. **Extraction is deterministic and structural.** The same turn yields the same
   node ids; a file path is relative to the project when it is inside it and
   absolute when it is not; `shell` never contributes a file node, because
   recovering paths from a command line is a parse that would be wrong often
   enough to make the graph untrustworthy.
3. **A write never costs a turn.** A mirror that cannot be written, a sink that
   raises, a preferences object that raises — each returns 0 with the turn
   intact.

Hermetic: `CORDATA_KG_DIR` is a temp directory, so the developer's real graph
(and any LanceDB store) is untouched.

    backend/.venv/bin/python _kg_smoke_test.py
"""

import json
import logging
import os
import stat
import sys
import tempfile
from pathlib import Path

_TMP = tempfile.TemporaryDirectory()
_TMPDIR = Path(_TMP.name)
os.environ["CORDATA_KG_DIR"] = str(_TMPDIR / "graph")
sys.path.insert(0, str(Path(__file__).resolve().parent))

import kg  # noqa: E402
from kg import extract  # noqa: E402
from kg.cognee_sink import CogneeGraphSink  # noqa: E402
from kg.lance_sink import LanceGraphSink  # noqa: E402


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


class Prefs:
    """The dict part of `settings_store.Settings`, plus a `get` that can raise."""

    def __init__(self, **values) -> None:
        self.values = values

    def get(self, key, default=None):
        if self.values.get("__raise__"):
            raise RuntimeError("settings are unreadable")
        return self.values.get(key, default)


ON = Prefs(**{"kg.enabled": True})
OFF = Prefs()
COGNEE = Prefs(**{"kg.enabled": True, "kg.sink": "cognee"})
UNKNOWN = Prefs(**{"kg.enabled": True, "kg.sink": "graphql"})
BROKEN = Prefs(**{"__raise__": True})

CWD = str(_TMPDIR / "project")
(_TMPDIR / "project").mkdir()
(_TMPDIR / "elsewhere").mkdir()

TURN = [
    {"name": "file_read", "input": {"path": "src/app.py"}, "status": "completed", "result": "ok"},
    {"name": "file_write", "input": {"path": str(_TMPDIR / "elsewhere" / "notes.md")}, "status": "completed"},
    {
        "name": "shell",
        "input": {"command": "pytest -q"},
        "status": "failed",
        "error": "Traceback (most recent call last):\n  File \"x.py\", line 3\n    boom\nValueError: something is wrong with the fixture",
    },
]


# ---------------------------------------------------------------------- gate


def test_the_gate_holds() -> None:
    check(kg.enabled(OFF) is False, "kg.enabled is off by default")
    check(kg.enabled(ON) is True, "and on when the setting says so")
    check(kg.enabled(None) is False, "a session with no preferences has no graph")
    check(kg.enabled(object()) is False, "an object without `get` is off, not an error")
    check(kg.sink_for(OFF) is None, "an observer that is off has no sink")
    check(isinstance(kg.sink_for(ON), LanceGraphSink), "the default sink is the embedded one")
    check(isinstance(kg.sink_for(COGNEE), CogneeGraphSink), "kg.sink picks the backend")
    check(isinstance(kg.sink_for(UNKNOWN), LanceGraphSink), "an unknown sink name falls back to the default")
    check(CogneeGraphSink().available() is False, "cognee is not installed here, and says so")
    check(kg.sink_for(COGNEE).stats()["cognee"] is False, "its stats agree without raising")

    check(kg.ingest_turn(session_id="s", cwd=CWD, tool_calls=TURN, preferences=OFF) == 0, "an off graph ingests nothing")
    check(not (Path(os.environ["CORDATA_KG_DIR"]) / "nodes.jsonl").exists(), "and writes no file")


# ----------------------------------------------------------------- extraction


def test_extraction_is_structural() -> None:
    nodes, edges = extract.extract_turn(session_id="s1", cwd=CWD, tool_calls=TURN, ts=1000.0)
    kinds = {node["type"] for node in nodes}
    check(kinds == {"project", "session", "tool", "file", "error"}, f"every node kind the turn supports (got {sorted(kinds)})")

    labels = {node["type"]: node["label"] for node in nodes}
    check(labels["project"] == str(Path(CWD).resolve()), "the project node is the resolved cwd")
    check(labels["file"] in ("src/app.py", str(_TMPDIR / "elsewhere" / "notes.md")), "one file node for the relative path")
    files = sorted(node["label"] for node in nodes if node["type"] == "file")
    check(files == sorted(["src/app.py", str(_TMPDIR / "elsewhere" / "notes.md")]), f"inside the project it is relative, outside it stays absolute (got {files})")
    check(len([node for node in nodes if node["type"] == "tool"]) == 3, "one node per distinct tool")
    check(labels["error"] == "ValueError: something is wrong with the fixture", "the error label is the exception line, not the traceback")

    types = sorted(edge["type"] for edge in edges)
    check(types == ["failed", "holds", "ran", "ran", "ran", "touched", "touched", "uses", "uses", "uses"], f"the edge set is what the turn implies (got {types})")

    touched = {edge["target"] for edge in edges if edge["type"] == "touched"}
    check(touched == {node["id"] for node in nodes if node["type"] == "file"}, "every `touched` edge lands on one of the two file nodes — shell contributed none")

    again, again_edges = extract.extract_turn(session_id="s1", cwd=CWD, tool_calls=TURN, ts=2000.0)
    check(
        {node["id"] for node in nodes} == {node["id"] for node in again},
        "the same turn yields the same node ids (so a repeat sighting merges)",
    )
    check(
        {edge["id"] for edge in edges} == {edge["id"] for edge in again_edges},
        "and the same edge ids",
    )

    empty = extract.extract_turn(session_id="s1", cwd=CWD, tool_calls=[], ts=1.0)
    check(len(empty[0]) == 2 and len(empty[1]) == 1, "a turn with no tool calls still records the project and session")


def test_error_labels() -> None:
    check(extract._error_label({"error": "   "}) == "", "blank errors yield no node")
    check(extract._error_label({"status": "completed", "result": "fine"}) == "", "a clean call yields no node")
    check(extract._error_label({"status": "failed", "result": "AssertionError: nope"}) == "AssertionError: nope", "a failed call falls back to its result")
    long_error = extract._error_label({"error": "E" * 500})
    check(len(long_error) == extract.ERROR_LINE_LIMIT, "an error label is bounded")


# ------------------------------------------------------------------- writing


def test_ingest_writes_the_mirror() -> None:
    written = kg.ingest_turn(session_id="s2", cwd=CWD, tool_calls=TURN, preferences=ON, ts=1000.0)
    check(written > 0, f"an on graph writes rows (got {written})")

    directory = Path(os.environ["CORDATA_KG_DIR"])
    nodes = [json.loads(line) for line in (directory / "nodes.jsonl").read_text().splitlines()]
    edges = [json.loads(line) for line in (directory / "edges.jsonl").read_text().splitlines()]
    check(len(nodes) + len(edges) == written, "the count matches the mirror")
    check(all({"id", "type", "label", "session_id", "first_ts", "last_ts"} <= set(row) for row in nodes), "node rows carry the declared fields")
    check(all({"id", "type", "source", "target", "session_id", "first_ts", "last_ts"} <= set(row) for row in edges), "edge rows too")

    mode = stat.S_IMODE((directory / "nodes.jsonl").stat().st_mode)
    check(mode == 0o600, f"the mirror is private (mode {oct(mode)}) — it names files and errors")

    sink = LanceGraphSink()
    found = sink.search("app.py")
    check(len(found) == 1 and found[0]["type"] == "file", "search finds a node by label")
    check(sink.search("") == [], "an empty search is empty, not everything")
    node_id = found[0]["id"]
    check(any(edge["source"] == node_id or edge["target"] == node_id for edge in sink.neighbours(node_id)), "neighbours returns the edges touching a node")
    check(sink.stats()["nodes"] == len(nodes), "stats counts what is on disk")

    # A process killed mid-append leaves a partial line; reading must skip it
    # rather than lose the whole mirror.
    with (directory / "nodes.jsonl").open("a", encoding="utf-8") as handle:
        handle.write('{"id": "truncated"')
    check(len(LanceGraphSink().search("app.py")) == 1, "a truncated final line is skipped, not fatal")


def test_a_repeat_merges_in_lancedb() -> None:
    from kg import lance_sink

    if lance_sink._lancedb() is None:
        check(True, "lancedb is not installed; the JSONL mirror is the store (skipped)")
        return
    import lancedb

    directory = _TMPDIR / "merged"
    sink = LanceGraphSink(directory)
    nodes, edges = extract.extract_turn(session_id="s3", cwd=CWD, tool_calls=TURN, ts=1000.0)
    check(sink.write(nodes, edges) == len(nodes) + len(edges), "the first write lands")
    nodes2, edges2 = extract.extract_turn(session_id="s3", cwd=CWD, tool_calls=TURN, ts=2000.0)
    sink.write(nodes2, edges2)

    table = lancedb.connect(str(directory / "graph.lance")).open_table("nodes")
    rows = table.search().limit(100).to_list()
    check(len(rows) == len(nodes), f"the same turn twice is the same {len(nodes)} nodes, not {2 * len(nodes)} rows")
    firsts = {row["id"]: row["first_ts"] for row in rows}
    lasts = {row["id"]: row["last_ts"] for row in rows}
    check(all(value == 1000.0 for value in firsts.values()), "first_ts kept the original sighting")
    check(all(value == 2000.0 for value in lasts.values()), "last_ts moved to the newest")
    stats = sink.stats()
    check(stats["lancedb"] is True and stats["nodes"] == 2 * len(nodes), "stats reports both halves: 2 ingests, one row set")


def test_reads_collapse_the_log_and_inputs_are_masked() -> None:
    """The JSONL mirror is a log (one line per sighting), so a reader that
    returned lines would answer a `limit` of 20 with twenty copies of one node."""
    directory = _TMPDIR / "collapsed"
    sink = LanceGraphSink(directory)
    for ts in (1000.0, 2000.0, 3000.0):
        nodes, edges = extract.extract_turn(session_id="s5", cwd=CWD, tool_calls=TURN, ts=ts)
        sink.write(nodes, edges)
    hits = sink.search("app.py", limit=20)
    check(len(hits) == 1, f"three sightings of one file are one search result (got {len(hits)})")
    check(hits[0]["first_ts"] == 1000.0 and hits[0]["last_ts"] == 3000.0, "carrying the first and the latest sighting")
    node_id = hits[0]["id"]
    ids = [edge["id"] for edge in sink.neighbours(node_id, limit=50)]
    check(len(ids) == len(set(ids)), "and neighbours lists each edge once")

    # A relative path means relative to the session's directory, not to this
    # process's: otherwise two sessions in two projects merge their files.
    nodes, _ = extract.extract_turn(
        session_id="s6", cwd=str(_TMPDIR / "proj-a"), tool_calls=[{"name": "file_read", "input": {"path": "src/app.py"}}]
    )
    check(
        any(node["type"] == "file" and node["label"] == "src/app.py" for node in nodes),
        "a relative path is anchored to the session's cwd",
    )

    import secret_guard

    secret_guard.register_secret("sk-live-abcdef0123456789LEAK")
    nodes, _ = extract.extract_turn(
        session_id="s7",
        cwd=CWD,
        tool_calls=[{"name": "shell", "input": {"command": "curl x"}, "status": "failed", "error": "401 for sk-live-abcdef0123456789LEAK"}],
    )
    check(all("LEAK" not in node["label"] for node in nodes), "a registered secret never becomes a node label")


# ------------------------------------------------------------------ failures


def test_a_bad_write_never_costs_a_turn() -> None:
    check(kg.ingest_turn(session_id="s4", cwd=CWD, tool_calls=TURN, preferences=BROKEN) == 0, "unreadable settings return 0, not an exception")
    check(kg.ingest_turn(session_id="s4", cwd=None, tool_calls=[], preferences=ON) == 0, "a turn with neither a project nor a tool call writes nothing")

    original = LanceGraphSink.write

    def explode(self, nodes, edges):
        raise OSError("the disk is full")

    LanceGraphSink.write = explode
    try:
        check(kg.ingest_turn(session_id="s5", cwd=CWD, tool_calls=TURN, preferences=ON) == 0, "a sink that raises returns 0")
    finally:
        LanceGraphSink.write = original

    check(
        kg.ingest_turn(session_id="s6", cwd=CWD, tool_calls=TURN, preferences=COGNEE) > 0,
        "a cognee-configured sink still writes the mirror without cognee installed",
    )


def main() -> None:
    logging.basicConfig(level="CRITICAL")  # the expected warnings are the assertions
    test_the_gate_holds()
    test_extraction_is_structural()
    test_error_labels()
    test_ingest_writes_the_mirror()
    test_a_repeat_merges_in_lancedb()
    test_reads_collapse_the_log_and_inputs_are_masked()
    test_a_bad_write_never_costs_a_turn()
    print("\nkg: all checks passed")


if __name__ == "__main__":
    main()

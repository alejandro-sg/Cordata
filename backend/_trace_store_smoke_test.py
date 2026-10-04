"""Regression check for trace_store.py — the gate, ingestion, and search.

Hermetic and fast. The real corpus lives beside the checkout and the real
embedder is a 90MB sentence-transformer; this points `SIERRALAB_TRACE_DIR` at a
temp directory and replaces `trace_store._embedder` with a deterministic stub
before any row is written, so nothing here touches a developer's `memory.lance`
or loads torch.

    backend/.venv/bin/python _trace_store_smoke_test.py
"""

import os
import tempfile
from pathlib import Path

_TMP = tempfile.TemporaryDirectory()
os.environ["SIERRALAB_TRACE_DIR"] = str(Path(_TMP.name) / "memory.lance")

import trace_store  # noqa: E402  (must follow the env var above)


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


class Embedder:
    """A bag-of-words vector, so "nearest" is predictable without a model.

    Each of eight buckets counts a disjoint set of words; two texts that share
    a word point the same way, which is enough to assert ranking. Cosine
    distance is all `search` asks of a vector.
    """

    BUCKETS = ("alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta")

    def get_embedding_dimension(self) -> int:
        return len(self.BUCKETS)

    def encode(self, text: str):
        lowered = (text or "").lower()
        return [float(lowered.count(word)) + (0.01 if i == 0 else 0.0) for i, word in enumerate(self.BUCKETS)]


class Prefs:
    def __init__(self, **values) -> None:
        self.values = values

    def get(self, key, default=None):
        return self.values.get(key, default)


OFF = Prefs()
ON = Prefs(**{"memory.traces": True})


def main() -> None:
    # ------------------------------------------------------------- the gate
    check(trace_store.enabled() is False, "tracing is off with no settings")
    check(trace_store.enabled(OFF) is False, "tracing is off when memory.traces is false")
    check(trace_store.enabled(ON) is True, "tracing is on when memory.traces is true")
    check(trace_store.enabled({"memory.traces": True}) is True, "a plain dict works too")

    # Off means inert, not merely unused: nothing is written and nothing is read.
    check(trace_store.record("alpha", preferences=OFF) is None, "a disabled record writes nothing")
    check(trace_store.search("alpha", preferences=OFF) == [], "a disabled search returns nothing")
    check(trace_store.stats(OFF)["enabled"] is False, "stats reports the gate")
    check(trace_store.record_turn(session_id="s1", prompt="alpha", preferences=OFF) == 0, "a disabled turn writes nothing")
    check(not trace_store.path().exists(), "nothing was created on disk while disabled")

    trace_store._embedder = Embedder  # the stub, before anything is embedded

    # --------------------------------------------------------- ingestion
    first = trace_store.record(
        "alpha ran the build and failed on the import cycle",
        kind="tool",
        session_id="s1",
        tool="shell",
        exit_code=1,
        error="ImportError: cycle",
        preferences=ON,
    )
    check(isinstance(first, str) and first, "a trace is recorded and returns its id")
    check(trace_store.path().exists(), "recording creates the corpus directory")

    check(
        trace_store.record(
            "beta listed the working tree",
            kind="tool",
            session_id="s1",
            tool="shell",
            exit_code=0,
            preferences=ON,
        ),
        "a successful trace is recorded",
    )
    check(
        trace_store.record(
            "gamma edited the loader",
            kind="tool",
            session_id="s2",
            tool="editor",
            patch="--- a/loader.py\n+++ b/loader.py",
            path="loader.py",
            preferences=ON,
        ),
        "a patch trace is recorded",
    )

    # ------------------------------------------------------------ search
    hits = trace_store.search("alpha", preferences=ON, limit=3)
    check(len(hits) == 3, "search returns the corpus")
    check(hits[0]["tool"] == "shell", "the nearest row comes first")
    check("alpha" in hits[0]["text"], "the nearest row is the one that mentions the query")
    check("vector" not in hits[0], "the embedding is not handed back to callers")

    # The filter is a `where` on the same query, not a post-filter: the editor
    # row does not mention "alpha" at all and still comes back, because it is
    # the nearest *among the editor rows*. Narrowing by tool is about which
    # rows are eligible, not about how well they match.
    editor_hits = trace_store.search("alpha", preferences=ON, tool="editor")
    check([row["tool"] for row in editor_hits] == ["editor"], "search narrows by tool")
    check(
        [row["tool"] for row in trace_store.search("alpha", preferences=ON, tool="shell")] == ["shell", "shell"],
        "search narrows by tool without losing the matches",
    )
    check(
        [row["tool"] for row in trace_store.search("alpha", preferences=ON, session_id="s2")] == ["editor"],
        "search narrows by session",
    )
    failed = trace_store.search("alpha", preferences=ON, failed_only=True)
    check([row["exit_code"] for row in failed] == [1], "failed_only excludes a clean exit and a null exit code")
    check(trace_store.search("alpha", preferences=ON, limit=1) != [], "limit is honoured")

    # A row with no exit code at all (an edit, not a command) is not a failure.
    check(
        all(row["kind"] != "turn" for row in failed),
        "a trace that never ran a process is not counted as failed",
    )

    # ------------------------------------------------------- a whole turn
    written = trace_store.record_turn(
        session_id="s3",
        prompt="alpha please fix it",
        reply="done",
        tool_calls=[
            {"name": "shell", "input": {"command": "pytest"}, "status": "failed", "error": "1 failed"},
            {"name": "editor", "input": {"path": "x.py"}, "path": "x.py", "patch": "-a\n+b"},
        ],
        preferences=ON,
    )
    check(written == 3, "a turn writes one row per tool call plus one for the turn itself")
    delta_rows = trace_store.search("delta x.py", preferences=ON, session_id="s3")
    check(delta_rows != [], "the turn's rows are searchable under its session")
    check(
        any(row["tool"] == "editor" and row["patch"] == "-a\n+b" for row in delta_rows),
        "the patch survives the turn ingestion",
    )

    # A call the harness marked failed is found by failed_only even though no
    # exit code was ever measured — and no exit code was invented for it.
    turned_failures = trace_store.search("beta", preferences=ON, failed_only=True, limit=10)
    pytest_row = next((row for row in turned_failures if row["tool"] == "shell" and row["session_id"] == "s3"), None)
    check(pytest_row is not None, "a status-only failure is found by failed_only")
    check(pytest_row["exit_code"] is None, "a status-only failure has no fabricated exit code")
    check(pytest_row["error"] == "1 failed", "the failure reason survives the turn ingestion")
    check(
        all(row["kind"] != "turn" for row in turned_failures),
        "the turn's own row is not swept into failed_only",
    )

    # -------------------------------------------------------------- stats
    counted = trace_store.stats(ON)
    check(counted["enabled"] is True and counted["rows"] == 6, f"stats counts every row (got {counted['rows']})")
    check(str(trace_store.path()) in counted["path"], "stats names the corpus it counted")

    # ---------------------------------------------- failure is not an error
    # A corpus that cannot be written must not break the turn that produced it.
    real_path = trace_store.path
    trace_store.path = lambda: Path("/proc/definitely/not/writable/memory.lance")
    check(trace_store.record("alpha", preferences=ON) is None, "an unwritable corpus returns None, not an exception")
    check(trace_store.search("alpha", preferences=ON) == [], "an unwritable corpus searches to nothing")
    trace_store.path = real_path

    # And with lancedb absent the whole module degrades to a no-op, which is the
    # state of a fresh checkout that has not installed the optional dependency.
    real_lancedb = trace_store._lancedb
    trace_store._lancedb = lambda: None
    check(trace_store.record("alpha", preferences=ON) is None, "no lancedb means record is a no-op")
    check(trace_store.search("alpha", preferences=ON) == [], "no lancedb means search is a no-op")
    trace_store._lancedb = real_lancedb

    # ------------------------------------------------- the agent-facing tool
    # Built through `_tool()` rather than imported at module scope, so the
    # plugin only pays for strands when tracing is actually switched on.
    tool = trace_store._tool()
    check(tool.tool_spec["name"] == "trace_search", "_tool() builds a trace_search tool")
    check(
        "off" in tool("alpha").lower(),
        "the tool says so plainly when tracing is off, rather than reporting no matches",
    )

    # ---------------------------------------------------- the plugin gate
    # `acp_server` is imported last: `agent_common` chdirs the process on
    # import, which every check above would otherwise have had to survive. The
    # corpus path is an absolute env override, so it is unaffected either way.
    import acp_server  # noqa: PLC0415

    host_off = acp_server._plugin_host(str(Path(_TMP.name) / "off"), set(), False)
    host_on = acp_server._plugin_host(str(Path(_TMP.name) / "on"), set(), True)
    off_rows = {row["name"] for row in host_off.status()}
    on_rows = {row["name"] for row in host_on.status()}
    check("traces" not in off_rows, "the traces plugin is absent when memory.traces is off")
    check("traces" in on_rows, "the traces plugin is present when memory.traces is on")
    check("memory" in off_rows and "memory" in on_rows, "the existing plugins are unaffected")


if __name__ == "__main__":
    main()
    _TMP.cleanup()
    print("\ntrace_store: all checks passed")

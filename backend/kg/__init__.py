"""The knowledge-graph sink interface, and the one function that feeds it.

A *sink*, not a store: this package owns the shape of what a turn contributes to
a graph (see `extract.py`) and hands those rows to a backend that knows how to
keep them. Two backends ship here:

* [`LanceGraphSink`][kg.lance_sink.LanceGraphSink] — the default. Embedded
  LanceDB tables plus `nodes.jsonl` / `edges.jsonl` mirrors, no service, no
  model. Two tables rather than one because a node and an edge have different
  columns and a self-join on a single table is a query nobody wants to write.
* [`CogneeGraphSink`][kg.cognee_sink.CogneeGraphSink] — for a machine that has
  `cognee` installed, behind the same three methods.

Gating. `kg.enabled` (default false) is checked at every entry point rather than
at import, matching `trace_store`: importing this package must never be the
thing that decides whether the graph is on, and a checkout that has not opted in
behaves exactly as it did before this existed. With `lancedb` missing the default
sink still writes its JSONL mirrors and says so once — the graph is then
readable with `jq`, which is a worse store but not a broken one.

Nothing here raises into a turn. The graph is an observation *of* the harness;
a graph that cannot be written must not fail the turn that produced it, the same
policy `state_store.append_event` and `trace_store.record` follow.
"""

from __future__ import annotations

import logging
from abc import ABC, abstractmethod
from typing import Any

from .extract import extract_turn

logger = logging.getLogger(__name__)

#: What a sink must do. Deliberately three methods, none of which is a query
#: language: the sinks differ most in how they are searched, and a least-common
#: denominator search would either lie about one of them or become the whole
#: interface. `search` is on the concretes, where it can be honest.
class KnowledgeGraphSink(ABC):
    #: Short name for logs and `/plugins`-style listings.
    name: str = ""

    @abstractmethod
    def available(self) -> bool:
        """Can this sink actually keep anything? False is a normal answer — a
        missing optional dependency is not an error, it is a configuration."""

    @abstractmethod
    def write(self, nodes: list[dict[str, Any]], edges: list[dict[str, Any]]) -> int:
        """Persist rows. Returns how many were written; never raises."""

    @abstractmethod
    def stats(self) -> dict[str, Any]:
        """Node/edge counts and where they live — what `/status` and the health
        route report, so "the graph is on" is checkable rather than assumed."""


def enabled(preferences: Any) -> bool:
    """Is graph capture switched on? Read through `get` so a dict, a
    `settings_store.Settings` and the smoke test's stub all work."""
    getter = getattr(preferences, "get", None)
    if getter is None:
        return False
    return bool(getter("kg.enabled", False))


def sink_for(preferences: Any) -> KnowledgeGraphSink | None:
    """The configured sink, or None when the graph is off.

    Imported lazily: the LanceDB sink pulls in `lancedb` on its first write, and
    the Cognee one pulls in `cognee`, which is a large package that a checkout
    without `kg.sink: "cognee"` should never pay for.
    """
    if not enabled(preferences):
        return None
    getter = getattr(preferences, "get", None)
    choice = str(getter("kg.sink", "lance") or "lance").lower()
    if choice == "cognee":
        from .cognee_sink import CogneeGraphSink

        return CogneeGraphSink()
    if choice != "lance":
        logger.warning("kg: unknown sink %r; using lance", choice)
    from .lance_sink import LanceGraphSink

    return LanceGraphSink()


def ingest_turn(
    *,
    session_id: str | None,
    cwd: str | None,
    tool_calls: list[dict[str, Any]] | None,
    preferences: Any,
    ts: float | None = None,
) -> int:
    """Feed one finished turn to the configured sink. Returns rows written.

    Called from the same background thread as trace ingestion, after the turn's
    reply is already on its way to the client — `extract_turn` is cheap but a
    LanceDB merge is not, and neither belongs on the reply path.
    """
    try:
        sink = sink_for(preferences)
        if sink is None:
            return 0
        if not tool_calls:
            # A turn that called nothing contributes no structure — the project
            # and session nodes extraction would still produce on their own are
            # a record that a session existed, not a graph, and every idle turn
            # would rewrite them.
            return 0
        nodes, edges = extract_turn(session_id=session_id, cwd=cwd, tool_calls=tool_calls, ts=ts)
        if not nodes and not edges:
            return 0
        return sink.write(nodes, edges)
    except Exception:  # a graph that cannot be written must not fail the turn
        logger.debug("kg: ingest failed", exc_info=True)
        return 0

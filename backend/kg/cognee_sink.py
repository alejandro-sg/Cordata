"""A knowledge-graph sink backed by Cognee, for a machine that has it installed.

Off unless `kg.sink` says `"cognee"`; nothing here imports Cognee until a write
actually happens, and `available()` is False when it is not installed — which is
the ordinary case, since Cognee is not in `requirements.txt`.

**Unverified against a running Cognee.** The call sequence below (`add` the
nodes, then `cognify` to build the graph, then `search` to read it) is Cognee's
documented shape, and this file is written so that being wrong about it is a
logged failure with the JSONL fallback still on disk rather than a broken turn.
Cognee's own API has moved across releases; check `available()` and the log line
before trusting a `cognee` configuration in production. What the rest of the
package guarantees does not depend on this file: the extraction, the gating and
the interface are all exercised by `_kg_smoke_test.py` through the Lance sink.

The JSONL mirror is written here too. A sink whose data exists only inside a
service is a sink you cannot read after the service changes its mind.
"""

from __future__ import annotations

import asyncio
import json
import logging
from pathlib import Path
from typing import Any

from . import KnowledgeGraphSink
from .lance_sink import DEFAULT_DIR, _append_jsonl, _lines

logger = logging.getLogger(__name__)

_unavailable_logged = False


def _cognee() -> Any | None:
    global _unavailable_logged
    try:
        import cognee  # noqa: PLC0415 — optional dependency, see the module docstring
    except Exception:
        if not _unavailable_logged:
            logger.info("kg: cognee is not installed; kg.sink=\"cognee\" has nothing behind it")
            _unavailable_logged = True
        return None
    return cognee


class CogneeGraphSink(KnowledgeGraphSink):
    name = "cognee"

    def __init__(self, directory: Path | None = None) -> None:
        from .lance_sink import path as default_path

        self._dir = Path(directory) if directory is not None else default_path()

    def available(self) -> bool:
        return _cognee() is not None

    def write(self, nodes: list[dict[str, Any]], edges: list[dict[str, Any]]) -> int:
        # Mirror first, unconditionally, so the rows survive a Cognee failure or
        # a Cognee version that answers differently than expected.
        written = 0
        try:
            _append_jsonl(self._dir / "nodes.jsonl", nodes)
            _append_jsonl(self._dir / "edges.jsonl", edges)
            written = len(nodes) + len(edges)
        except OSError:
            logger.warning("kg: could not write the JSONL mirror in %s", self._dir, exc_info=True)
            return 0

        cognee = _cognee()
        if cognee is None:
            return written
        payload = "\n".join(
            json.dumps(row, ensure_ascii=False, default=str) for row in [*nodes, *edges]
        )
        try:
            self._run(cognee.add(payload))
            self._run(cognee.cognify())
        except Exception:
            logger.warning("kg: cognee rejected the write; the JSONL mirror is still current", exc_info=True)
        return written

    def stats(self) -> dict[str, Any]:
        return {
            "sink": self.name,
            "dir": str(self._dir),
            "cognee": _cognee() is not None,
            "nodes": _lines(self._dir / "nodes.jsonl"),
            "edges": _lines(self._dir / "edges.jsonl"),
        }

    def search(self, query: str, *, limit: int = 20) -> list[dict[str, Any]]:
        cognee = _cognee()
        if cognee is None:
            return []
        try:
            return list(self._run(cognee.search(query)))[:limit]
        except Exception:
            logger.warning("kg: cognee search failed", exc_info=True)
            return []

    @staticmethod
    def _run(awaitable: Any) -> Any:
        """Drive Cognee's coroutine from the ingest thread.

        Cognee is async and this runs in a background thread with no loop of its
        own — the same thread that already exists so a slow ingestion never
        touches the reply path. If a loop somehow is running, the coroutine is
        scheduled on it rather than nested."""
        if not asyncio.iscoroutine(awaitable):
            return awaitable
        try:
            asyncio.get_running_loop()
        except RuntimeError:
            return asyncio.run(awaitable)
        return asyncio.get_running_loop().create_task(awaitable)


#: Re-exported so a reader of this file does not have to open `lance_sink` to
#: learn where a Cognee-configured install still keeps its rows.
__all__ = ["CogneeGraphSink", "DEFAULT_DIR"]

"""Where the GUI's AST snippets go: validated, masked, deduplicated, and written off the request path.

The GUI watches a workspace, parses each saved file with tree-sitter in *its* process, and
sends only the functions and classes that changed (``harness.proto`` ``AstSnippet``). This is the
daemon's half: ``ProactiveIngest`` calls ``submit`` per snippet and gets a verdict back at once;
a background worker does the writing, so a slow disk or a slow graph sink can never make the
GUI's stream wait.

What a snippet must survive before it is stored:

* a workspace that is an existing absolute directory, and a path that is relative, has no ``..``
  and resolves inside it — the daemon will not record a "snippet" of ``/etc/passwd``;
* not a secrets file (``.env``, ``.env.local``; templates are fine) — the GUI also never parses
  these, but the daemon does not rely on that;
* valid text, no NUL, at most ``MAX_TEXT_BYTES``;
* masked (``secret_patterns.scrub``): a function that embeds a key is stored with the key hidden;
* not already seen: ``(workspace, path, name, content_hash)`` in a bounded LRU. Saving a file
  without changing a function must not write that function again.

What it is written to (all of it best-effort; none of it can fail a stream):

* **the state log** (``state.db`` events, kind ``ingest``) — metadata and a short preview, not the
  code: the log is an audit trail of what the GUI told the daemon, not a second copy of the repo;
* **the knowledge graph** (``kg/``), when ``kg.enabled`` — a ``symbol`` node per snippet joined
  to its ``file`` node (the same id a tool call touching the file produces, so the two merge);
* **the observer's evidence queue**, when ``observer.enabled`` — one rate-limited line naming
  what is being edited, so the proactive briefing knows what the person is working on.

The snippet *text* is not embedded: that needs a sentence-transformer, which the daemon — by
design (``supervisor.py``) — does not load. The structure and the audit trail are the point.
"""

from __future__ import annotations

import asyncio
import hashlib
import logging
import os
import time
from collections import OrderedDict
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import secret_patterns
import state_store

logger = logging.getLogger(__name__)

MAX_TEXT_BYTES = 64 * 1024
QUEUE_LIMIT = 4096
DEDUP_LIMIT = 8192
BATCH = 64
OBSERVE_INTERVAL = 30.0
MAX_REJECTION_REASONS = 5


@dataclass(frozen=True)
class Verdict:
    status: str  # accepted | deduplicated | rejected
    reason: str = ""


def _secret_name(path: str) -> bool:
    name = Path(path).name
    return name == ".env" or (name.startswith(".env.") and name.split(".env.", 1)[1] not in ("example", "sample", "template", "dist"))


class IngestSink:
    def __init__(
        self,
        *,
        preferences: Callable[[str], Any] | None = None,
        observe: Callable[[str, str], Any] | None = None,
        queue_limit: int = QUEUE_LIMIT,
    ) -> None:
        self._preferences = preferences or (lambda workspace: None)
        self._observe = observe
        self._queue: asyncio.Queue[dict[str, Any]] = asyncio.Queue(maxsize=queue_limit)
        self._seen: "OrderedDict[tuple[str, str, str, str], None]" = OrderedDict()
        self._worker: asyncio.Task | None = None
        self._last_observed: dict[str, float] = {}
        self.accepted = 0
        self.deduplicated = 0
        self.rejected = 0
        self.persisted = 0

    # ------------------------------------------------------------ lifecycle

    def start(self) -> None:
        if self._worker is None or self._worker.done():
            self._worker = asyncio.create_task(self._run(), name="daemon-ingest")

    async def stop(self) -> None:
        worker, self._worker = self._worker, None
        if worker is None:
            return
        await self.drain(timeout=2.0)
        worker.cancel()
        await asyncio.gather(worker, return_exceptions=True)

    async def drain(self, timeout: float = 5.0) -> bool:
        """Wait for everything accepted so far to be written (tests; shutdown)."""
        try:
            await asyncio.wait_for(self._queue.join(), timeout)
            return True
        except asyncio.TimeoutError:
            return False

    # -------------------------------------------------------------- submit

    def submit(self, snippet: Any) -> Verdict:
        """Validate and enqueue one ``AstSnippet`` (a proto message or a dict). Never blocks."""
        get = snippet.get if isinstance(snippet, dict) else lambda key, default=None: getattr(snippet, key, default)
        try:
            workspace, path, text = str(get("workspace", "")), str(get("path", "")), str(get("text", ""))
            name, language = str(get("name", "")), str(get("language", ""))
            verdict = self._validate(workspace, path, text, name)
            if verdict is not None:
                self.rejected += 1
                return verdict
            masked, redactions = secret_patterns.scrub(text)
            digest = str(get("content_hash", "")) or hashlib.sha256(text.encode()).hexdigest()
            key = (workspace, path, name, digest)
            if key in self._seen:
                self._seen.move_to_end(key)
                self.deduplicated += 1
                return Verdict("deduplicated")
            row = {
                "workspace": workspace,
                "path": path,
                "language": language,
                "node_kind": str(get("node_kind", "")),
                "name": name,
                "start_line": int(get("start_line", 0) or 0),
                "end_line": int(get("end_line", 0) or 0),
                "change": str(get("change", "")) or "modified",
                "hash": digest,
                "chars": len(masked),
                "redactions": redactions,
                "preview": masked[:200],
                "saved_ms": int(get("saved_ms", 0) or 0),
                "received": time.time(),
            }
            try:
                self._queue.put_nowait(row)
            except asyncio.QueueFull:
                self.rejected += 1
                return Verdict("rejected", "ingest queue full (back-pressure); resend later")
            self._seen[key] = None
            while len(self._seen) > DEDUP_LIMIT:
                self._seen.popitem(last=False)
            self.accepted += 1
            return Verdict("accepted")
        except Exception as error:  # a malformed message is a rejection, not a crashed stream
            self.rejected += 1
            return Verdict("rejected", f"unreadable snippet: {type(error).__name__}")

    @staticmethod
    def _validate(workspace: str, path: str, text: str, name: str) -> Verdict | None:
        if not workspace or not os.path.isabs(workspace) or not Path(workspace).is_dir():
            return Verdict("rejected", "workspace is not an existing absolute directory")
        if not path or os.path.isabs(path) or ".." in Path(path).parts:
            return Verdict("rejected", "path must be relative to the workspace, without '..'")
        try:
            resolved = (Path(workspace) / path).resolve()
            root = Path(workspace).resolve()
        except (OSError, RuntimeError):
            return Verdict("rejected", "path does not resolve")
        if root != resolved and root not in resolved.parents:
            return Verdict("rejected", "path resolves outside the workspace")
        if _secret_name(path):
            return Verdict("rejected", "secrets files are never ingested")
        if not name:
            return Verdict("rejected", "snippet has no name")
        if not text.strip():
            return Verdict("rejected", "snippet is empty")
        if "\x00" in text:
            return Verdict("rejected", "snippet is not text")
        if len(text.encode("utf-8", "replace")) > MAX_TEXT_BYTES:
            return Verdict("rejected", f"snippet is over {MAX_TEXT_BYTES // 1024} KiB")
        return None

    # -------------------------------------------------------------- worker

    async def _run(self) -> None:
        while True:
            batch = [await self._queue.get()]
            while len(batch) < BATCH and not self._queue.empty():
                batch.append(self._queue.get_nowait())
            try:
                await asyncio.to_thread(self._persist, batch)
            except Exception:
                logger.debug("daemon: ingest persist failed", exc_info=True)
            finally:
                for _ in batch:
                    self._queue.task_done()

    def _persist(self, batch: list[dict[str, Any]]) -> None:
        by_workspace: dict[str, list[dict[str, Any]]] = {}
        for row in batch:
            state_store.append_event(
                "ingest",
                {key: row[key] for key in ("workspace", "path", "language", "node_kind", "name", "start_line", "end_line", "change", "hash", "chars", "redactions", "preview")},
                source="gui",
            )
            by_workspace.setdefault(row["workspace"], []).append(row)
            self.persisted += 1
        for workspace, rows in by_workspace.items():
            self._to_graph(workspace, rows)
            self._to_observer(workspace, rows)

    def _to_graph(self, workspace: str, rows: list[dict[str, Any]]) -> None:
        try:
            import kg
            from kg.extract import symbol_rows

            preferences = self._preferences(workspace)
            sink = kg.sink_for(preferences)
            if sink is None:
                return
            nodes: list[dict[str, Any]] = []
            edges: list[dict[str, Any]] = []
            for row in rows:
                n, e = symbol_rows(row["path"], row["name"], row["node_kind"], row["language"], row["received"])
                nodes += n
                edges += e
            sink.write(nodes, edges)
        except Exception:
            logger.debug("daemon: ingest -> graph failed", exc_info=True)

    def _to_observer(self, workspace: str, rows: list[dict[str, Any]]) -> None:
        if self._observe is None:
            return
        now = time.monotonic()
        if now - self._last_observed.get(workspace, -1e9) < OBSERVE_INTERVAL:
            return
        self._last_observed[workspace] = now
        names = ", ".join(sorted({f"{r['path']}::{r['name']}" for r in rows})[:8])
        try:
            self._observe(f"The GUI saw edits to: {names}", workspace)
        except Exception:
            logger.debug("daemon: ingest -> observer failed", exc_info=True)

    # --------------------------------------------------------------- stats

    def stats(self) -> dict[str, int]:
        return {"accepted": self.accepted, "deduplicated": self.deduplicated, "rejected": self.rejected, "persisted": self.persisted, "queued": self._queue.qsize()}

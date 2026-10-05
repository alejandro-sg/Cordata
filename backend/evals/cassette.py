"""Record a live model once; replay it deterministically forever.

A test (or an eval) that drives a real model is as flaky as the model's wording: the same prompt
gets a different command on a different day, and a check on *what it ran* fails for reasons that
have nothing to do with the code. A cassette removes the model from the loop after the first run.

``CassetteModel`` wraps a Strands model. In **record** mode every request goes to the real model and
its stream of events is appended to a JSONL file, keyed by a hash of what was asked. In **replay**
mode the file answers and the real model is never built; a request that is not on the cassette is a
loud :class:`CassetteMiss` (the code under test asked something new — re-record on purpose), never a
silent fall-through to the network.

The key covers the system prompt, the tool names and the messages, with the project's path scrubbed
(``<WORK>``): the same task run in a different temp directory asks the same question.
"""

from __future__ import annotations

import hashlib
import json
import re
from collections.abc import AsyncIterator
from pathlib import Path
from typing import Any

from strands.models.model import Model

_VOLATILE = [
    (re.compile(r"\b\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})?\b"), "<TIME>"),
    (re.compile(r"/(private/)?(var/folders|tmp)/[A-Za-z0-9_./-]+"), "<TMP>"),
    (re.compile(r"\b[0-9a-f]{32,}\b"), "<HEX>"),
    # A short commit hash ("HEAD ae23d36", "commit 84b207e") hashes the commit time, so it differs every run.
    (re.compile(r"\b(HEAD|commit)\s+[0-9a-f]{7,40}\b"), r"\1 <SHA>"),
    # How long something took ("Ran 3 tests in 0.002s", "finished in 1.4s", "took 35ms") differs every run.
    (re.compile(r"\b(in|took|after|elapsed:?)\s+\d+(\.\d+)?\s?(ms|µs|us|s|sec|seconds)\b", re.IGNORECASE), r"\1 <DUR>"),
]


class CassetteMiss(LookupError):
    """Replay was asked something the cassette never recorded."""


def normalise(value: Any, scrub: dict[str, str]) -> Any:
    """``value`` with the volatile parts (temp paths, timestamps, long hex ids) replaced, recursively."""
    if isinstance(value, str):
        for needle, replacement in scrub.items():
            if needle:
                value = value.replace(needle, replacement)
        for pattern, replacement in _VOLATILE:
            value = pattern.sub(replacement, value)
        return value
    if isinstance(value, list):
        return [normalise(v, scrub) for v in value]
    if isinstance(value, dict):
        return {k: normalise(v, scrub) for k, v in value.items()}
    return value


def request_key(messages: list, tool_specs: list | None, system_prompt: str | None, scrub: dict[str, str]) -> str:
    material = {
        "system": normalise(system_prompt or "", scrub),
        "tools": sorted(spec.get("name", "") for spec in tool_specs or []),
        "messages": normalise(messages, scrub),
    }
    return hashlib.sha256(json.dumps(material, sort_keys=True, default=str, ensure_ascii=False).encode()).hexdigest()[:24]


class CassetteModel(Model):
    def __init__(self, path: str | Path, *, mode: str = "replay", inner: Model | None = None, scrub: dict[str, str] | None = None) -> None:
        if mode not in ("record", "replay"):
            raise ValueError("mode must be record or replay")
        if mode == "record" and inner is None:
            raise ValueError("recording needs the real model to record")
        self.path = Path(path)
        self.mode = mode
        self.inner = inner
        self.scrub = scrub or {}
        self.misses = 0
        self.hits = 0
        self._entries: dict[str, list[list[dict[str, Any]]]] = {}
        self._cursor: dict[str, int] = {}
        if mode == "replay":
            self._load()

    def _load(self) -> None:
        if not self.path.exists():
            raise CassetteMiss(f"no cassette at {self.path}")
        for line in self.path.read_text(encoding="utf-8").splitlines():
            if line.strip():
                row = json.loads(line)
                self._entries.setdefault(row["key"], []).append(row["events"])

    # -- the Model interface the agent loop uses ---------------------------------------------

    def update_config(self, **model_config: Any) -> None:
        if self.inner is not None:
            self.inner.update_config(**model_config)

    def get_config(self) -> dict[str, Any]:
        return self.inner.get_config() if self.inner is not None else {"model_id": f"cassette:{self.path.name}"}

    async def structured_output(self, *args: Any, **kwargs: Any):  # pragma: no cover - unused by the harness
        raise NotImplementedError
        yield

    async def stream(self, messages, tool_specs=None, system_prompt=None, **kwargs) -> AsyncIterator[dict[str, Any]]:
        key = request_key(messages, tool_specs, system_prompt, self.scrub)
        if self.mode == "record":
            events: list[dict[str, Any]] = []
            async for event in self.inner.stream(messages, tool_specs, system_prompt, **kwargs):  # type: ignore[union-attr]
                events.append(event)
                yield event
            self.path.parent.mkdir(parents=True, exist_ok=True)
            with open(self.path, "a", encoding="utf-8") as handle:
                handle.write(json.dumps({"key": key, "events": events}, default=str, ensure_ascii=False) + "\n")
            return
        recorded = self._entries.get(key)
        if not recorded:
            self.misses += 1
            raise CassetteMiss(f"{self.path.name}: this request was never recorded (key {key}); re-record the cassette on purpose if the code under test changed what it asks")
        # The same question asked twice replays the answers in the order they were given.
        at = self._cursor.get(key, 0)
        self._cursor[key] = at + 1
        self.hits += 1
        for event in recorded[min(at, len(recorded) - 1)]:
            yield event

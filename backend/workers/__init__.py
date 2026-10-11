"""Coding workers behind one interface (``base.Worker``): Claude Code, Antigravity (``agy``) and Ollama.

    from workers import build_worker, TaskPacket, collect
    worker = build_worker("antigravity")
    events, result = await collect(worker.run(TaskPacket(task_id="t1", role="plan", prompt="…", cwd=project)))

See ``base.py`` for the contract, ``cli_worker.py`` for what the two CLIs share (vault-only egress, Seatbelt-scoped
writes, metering, cancel), and ``conformance.py`` for the checks every adapter passes.
"""

from __future__ import annotations

from typing import Any

from .base import (
    READ_ONLY_ROLES,
    ROLES,
    TaskPacket,
    Worker,
    WorkerCapabilities,
    WorkerEvent,
    WorkerFamily,
    WorkerResult,
    collect,
)

FAMILIES = tuple(f.value for f in WorkerFamily)


def build_worker(family: str | WorkerFamily, **kwargs: Any) -> Worker:
    """A worker of ``family`` (``claude_code`` · ``antigravity`` · ``ollama``). Imports lazily: an orchestrator that
    only ever uses Ollama never imports the CLI adapters."""
    name = family.value if isinstance(family, WorkerFamily) else str(family)
    if name == WorkerFamily.CLAUDE_CODE.value:
        from .claude_worker import ClaudeCodeWorker

        return ClaudeCodeWorker(**kwargs)
    if name == WorkerFamily.ANTIGRAVITY.value:
        from .agy_worker import AgyWorker

        return AgyWorker(**kwargs)
    if name == WorkerFamily.OLLAMA.value:
        from .ollama_worker import OllamaWorker

        return OllamaWorker(**kwargs)
    raise ValueError(f"unknown worker family {name!r}; known: {', '.join(FAMILIES)}")


__all__ = [
    "FAMILIES",
    "READ_ONLY_ROLES",
    "ROLES",
    "TaskPacket",
    "Worker",
    "WorkerCapabilities",
    "WorkerEvent",
    "WorkerFamily",
    "WorkerResult",
    "build_worker",
    "collect",
]

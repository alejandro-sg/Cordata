"""The one contract every coding worker meets: Claude Code, Antigravity (``agy``) and Ollama.

The orchestrator (``backend/orchestrator/``) hands a worker one bounded piece of work — a *role* (plan, execute,
harden, audit) over a project directory, limited to a file scope and a budget — and reads back a typed result. It
never needs to know which CLI or model did it, which is what lets a task move between workers when one runs out of
quota (``orchestrator/quota_ledger.py``) or does worse than another (``orchestrator/dispatch.py``).

    run(task)              stream WorkerEvents; the last one is ``done`` and carries the WorkerResult
    resume(handle, task)   continue a provider session (Claude's session id, agy's conversation id)
    cancel(task_id)        stop a running task: its process group is killed, the stream ends ``cancelled``
    quota_snapshot()       what this worker knows about its own limits, without spending any

Two ids, deliberately different. ``task.task_id`` is the orchestrator's: unique per run, the key ``cancel`` takes.
``WorkerResult.session_handle`` is the provider's (a Claude session, an agy conversation), the key ``resume`` takes —
and the one a handoff to *another* worker cannot use, which is why handoffs carry a ``HandoffPacket`` instead.

Never raises for an operational failure: a missing CLI, a refused egress policy, a timeout, a rate limit and a crash
are all results (``status`` + ``error_message``), so the state machine can decide what next.
"""

from __future__ import annotations

import time
from abc import ABC, abstractmethod
from collections.abc import AsyncIterator
from dataclasses import asdict, dataclass, field
from enum import Enum
from typing import Any

ROLES = ("plan", "execute", "harden", "audit")
#: Roles that must not change files. Their scope is empty whatever the packet says.
READ_ONLY_ROLES = frozenset({"plan", "audit"})

STATUSES = ("completed", "failed", "cancelled", "quota_limit")
EVENT_TYPES = ("init", "progress", "tool_use", "tool_result", "text", "error", "quota", "done")


class WorkerFamily(str, Enum):
    CLAUDE_CODE = "claude_code"
    ANTIGRAVITY = "antigravity"
    OLLAMA = "ollama"


@dataclass(frozen=True)
class WorkerCapabilities:
    family: WorkerFamily
    models: tuple[str, ...]
    supports_streaming: bool = True
    supports_structured_output: bool = True
    supports_sandbox: bool = True
    supports_resume: bool = True
    #: The model *family* the default model belongs to ("anthropic", "google", "openai", "zhipu", …). The harden role
    #: must run on a different one than execute — a vendor, not a CLI: agy serving Claude is still Anthropic.
    model_family: str = ""
    #: What a unit of work costs here: "usd" (metered), "subscription" (a rolling window), "free" (local weights).
    cost_basis: str = "subscription"


@dataclass
class TaskPacket:
    task_id: str
    role: str  # "plan" | "execute" | "harden" | "audit"
    prompt: str
    cwd: str
    file_scope: list[str] = field(default_factory=list)
    budget_usd: float | None = None
    budget_tokens: int | None = None
    timeout_s: float = 1800.0
    model: str | None = None
    effort: str | None = None
    checkpoint_hash: str | None = None
    handoff_context: dict[str, Any] | None = None
    #: A JSON Schema the final answer must satisfy (plan and audit roles). Workers that support it enforce it
    #: natively (``--json-schema``); the rest are asked for JSON and parsed.
    json_schema: dict[str, Any] | None = None

    def writable_scope(self) -> list[str]:
        """The paths this task may change: nothing for a read-only role, the declared scope otherwise."""
        return [] if self.role in READ_ONLY_ROLES else list(self.file_scope)


@dataclass
class WorkerResult:
    worker_id: str
    status: str  # one of STATUSES
    exit_code: int
    duration_s: float
    cost_usd: float
    tokens_in: int
    tokens_out: int
    files_changed: list[str]
    diff_stat: str
    raw_output: str
    error_message: str | None = None
    #: The provider's own session id, for ``resume``. ``None`` when the provider kept no session.
    session_handle: str | None = None
    model: str | None = None
    #: The parsed final answer when the task asked for a schema.
    structured: Any = None
    #: True when the stop was the provider's limit (a 429, "usage limit reached"), not the task's own budget.
    rate_limited: bool = False
    #: Files changed outside the task's writable scope. Must be empty; the audit fails the step otherwise.
    out_of_scope: list[str] = field(default_factory=list)
    #: Actions the CLI wanted and was refused (agy's ``denied_actions``, Claude's ``permission_denials``).
    denied: list[str] = field(default_factory=list)
    requests: int = 1
    #: The provider's own reading of its quota windows, when it reports one (Claude Code's ``rate_limit_event``):
    #: ``{"status", "windows": {"five_hour": {"utilization", "resets_at"}, "seven_day": {...}}}``. Ground truth for the
    #: quota ledger — it beats self-metering.
    provider_quota: dict[str, Any] = field(default_factory=dict)

    @property
    def ok(self) -> bool:
        return self.status == "completed"

    def to_json(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> "WorkerResult":
        known = {f for f in cls.__dataclass_fields__}  # type: ignore[attr-defined]
        return cls(**{k: v for k, v in data.items() if k in known})


@dataclass
class WorkerEvent:
    event_type: str  # one of EVENT_TYPES
    payload: dict[str, Any]
    raw: str | None = None
    ts: float = field(default_factory=time.time)
    #: Set on the ``done`` event only.
    result: WorkerResult | None = None


class Worker(ABC):
    """See the module docstring. Subclasses are cheap to construct; nothing runs until ``run``."""

    #: A stable id for the ledger: ``<family>`` or ``<family>:<variant>``.
    worker_id: str = ""

    @property
    @abstractmethod
    def capabilities(self) -> WorkerCapabilities: ...

    @abstractmethod
    async def quota_snapshot(self) -> dict[str, Any]: ...

    @abstractmethod
    def run(self, task: TaskPacket) -> AsyncIterator[WorkerEvent]: ...

    @abstractmethod
    def resume(self, handle: str, packet: TaskPacket) -> AsyncIterator[WorkerEvent]: ...

    @abstractmethod
    async def cancel(self, handle: str) -> bool: ...


async def collect(stream: AsyncIterator[WorkerEvent]) -> tuple[list[WorkerEvent], WorkerResult]:
    """Drain a worker stream. Returns every event and the result the ``done`` event carried. A stream that ends
    without one is itself a failure — reported as a result, never raised."""
    events: list[WorkerEvent] = []
    result: WorkerResult | None = None
    async for event in stream:
        events.append(event)
        if event.event_type == "done" and event.result is not None:
            result = event.result
    if result is None:
        result = WorkerResult("unknown", "failed", 1, 0.0, 0.0, 0, 0, [], "", "", error_message="the worker stream ended without a result")
    return events, result

"""What the two subprocess workers (Claude Code, agy) share: spawn, confine, stream, meter, stop.

The flow for one task, the same for both CLIs — only the argv and the event dialect differ (subclasses):

1. **Refuse early, as a result.** An empty or oversized prompt, a bad model name, a scope entry that is absolute,
   leaves the project or names a secrets file, a missing CLI, or a vault that is off or down: each ends the stream
   with ``status: failed`` and the reason, before anything is spawned.
2. **Egress through the vault, or not at all** — the same fail-closed rule as ``claude_code_tool``: the child gets
   ``HTTPS_PROXY`` pointing at the Agent Vault and an allowlisted environment (``scrubbed_sandbox``), never a direct
   route out and never a provider key.
3. **Confined writes.** ``scope.ScopedSandbox``: Seatbelt denies every write inside the project except the task's
   writable scope (none for plan/audit), and ``.env`` / credential files everywhere.
4. **Metered while it runs.** Every event is folded into tokens and dollars; a task that passes its own
   ``budget_tokens`` / ``budget_usd`` is killed there and ends ``quota_limit`` — the envelope is enforced by us, not
   left to the CLI. A provider limit (429, "usage limit reached") also ends ``quota_limit``, with ``rate_limited``.
5. **Stoppable.** ``cancel(task_id)`` kills the process group; the stream ends ``cancelled``.
6. **Accounted after.** Snapshot diff → ``files_changed``, ``git diff --stat``, and ``out_of_scope`` (anything the
   kernel let through because a glob was widened — the audit fails the step on it).
"""

from __future__ import annotations

import json
import logging
import os
import re
import time
from collections.abc import AsyncIterator, Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import jsonschema
from strands.sandbox import ExecutionResult, StreamChunk
from strands.sandbox.errors import SandboxTimeoutError

import scrubbed_sandbox
import secret_patterns
import vault_proxy

from . import prompting, scope as scope_mod, tree
from .base import TaskPacket, Worker, WorkerCapabilities, WorkerEvent, WorkerResult

logger = logging.getLogger(__name__)

MAX_PROMPT_CHARS = 60_000
MAX_OUTPUT_CHARS = 60_000
MODEL_NAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:\[\]-]{0,80}$")
EFFORTS = ("low", "medium", "high", "xhigh", "max")
#: Provider wording for "you are out of quota", across Anthropic, Google and OpenAI-compatible errors.
RATE_LIMIT = re.compile(r"usage limit|rate[ _-]?limit|too many requests|\b429\b|quota|resource[_ ]exhausted|limit reached|overloaded", re.I)


def default_vault() -> "vault_proxy.VaultStatus":
    """The vault as the user configured it, probed without touching this process's environment."""
    import settings_store

    return vault_proxy.install(settings_store.current(os.getcwd()), environ=dict(os.environ))


class StreamParser:
    """Line-splitting plus the totals a run adds up to. Subclasses implement ``apply`` for their CLI's dialect."""

    def __init__(self, task: TaskPacket) -> None:
        self.task = task
        self.session_handle: str | None = None
        self.model: str | None = None
        self.text = ""
        self.structured: Any = None
        self.cost_usd = 0.0
        self.tokens_in = 0
        self.tokens_out = 0
        self.requests = 0
        self.rate_limited = False
        self.budget_exhausted = False
        self.error: str | None = None
        self.ok: bool | None = None  # what the CLI's own final event said
        self.denied: list[str] = []
        self.quota_info: dict[str, Any] = {}
        self._buffer = ""

    def feed(self, chunk: str) -> list[dict[str, Any]]:
        self._buffer += chunk
        *lines, self._buffer = self._buffer.split("\n")
        return [e for e in (self._parse(line) for line in lines) if e is not None]

    def flush(self) -> list[dict[str, Any]]:
        line, self._buffer = self._buffer, ""
        event = self._parse(line)
        return [event] if event is not None else []

    @staticmethod
    def _parse(line: str) -> dict[str, Any] | None:
        line = line.strip()
        if not line:
            return None
        try:
            event = json.loads(line)
        except ValueError:
            return None
        return event if isinstance(event, dict) else None

    def apply(self, event: dict[str, Any]) -> list[WorkerEvent]:  # pragma: no cover - abstract
        raise NotImplementedError

    def note_error(self, text: str) -> None:
        self.error = text
        if RATE_LIMIT.search(text or ""):
            self.rate_limited = True


@dataclass
class _Running:
    sandbox: scrubbed_sandbox.ScrubbedSandbox
    cancelled: bool = False


@dataclass
class _Outcome:
    exit_code: int | None = None
    stderr: str = ""
    timed_out: bool = False
    budget_reason: str = ""
    spawn_error: str = ""


class CliWorker(Worker):
    """See the module docstring. Subclasses set the class attributes and implement ``build_argv``, ``build_stdin``
    and ``new_parser``."""

    family_caps: WorkerCapabilities
    binary_env = ""
    binary_name = ""
    #: True when the CLI enforces a JSON Schema itself (``--json-schema``); otherwise the prompt asks for JSON.
    native_schema = True
    #: False when the CLI refuses every shell command in headless mode (agy), so the prompt says so.
    can_run_commands = True

    def __init__(
        self,
        *,
        binary: str | None = None,
        vault: Callable[[], "vault_proxy.VaultStatus | None"] | None = None,
        extra_env: dict[str, str] | None = None,
        seatbelt: str = "auto",
        default_model: str | None = None,
        worker_id: str | None = None,
    ) -> None:
        self._binary = binary
        self._vault = vault or default_vault
        self.extra_env = dict(extra_env or {})
        self.seatbelt = seatbelt
        self.default_model = default_model
        self.worker_id = worker_id or self.family_caps.family.value
        self._running: dict[str, _Running] = {}
        self._last_quota: dict[str, Any] = {}

    # ------------------------------------------------------------------ the contract

    @property
    def capabilities(self) -> WorkerCapabilities:
        return self.family_caps

    def find_binary(self) -> str | None:
        import shutil

        env_val = (os.environ.get(self.binary_env) or "").strip()
        candidate = self._binary or env_val or shutil.which(self.binary_name)
        if not candidate:
            fallback = Path.home() / ".local" / "bin" / self.binary_name
            candidate = str(fallback) if fallback.exists() else None
        return candidate if candidate and os.path.exists(candidate) else None

    async def quota_snapshot(self) -> dict[str, Any]:
        return {
            "worker_id": self.worker_id,
            "family": self.family_caps.family.value,
            "available": self.find_binary() is not None,
            "models": list(self.family_caps.models),
            "last_signal": dict(self._last_quota),
        }

    def run(self, task: TaskPacket) -> AsyncIterator[WorkerEvent]:
        return self._execute(task, None)

    def resume(self, handle: str, packet: TaskPacket) -> AsyncIterator[WorkerEvent]:
        return self._execute(packet, handle)

    async def cancel(self, handle: str) -> bool:
        running = self._running.get(handle)
        if running is None:
            return False
        running.cancelled = True
        _kill_group(running.sandbox.last_pid)
        return True

    # ------------------------------------------------------------------ subclass hooks

    def build_argv(self, binary: str, task: TaskPacket, resume: str | None) -> list[str]:  # pragma: no cover
        raise NotImplementedError

    def build_stdin(self, task: TaskPacket, prompt: str) -> str | None:  # pragma: no cover
        raise NotImplementedError

    def new_parser(self, task: TaskPacket) -> StreamParser:  # pragma: no cover
        raise NotImplementedError

    # ------------------------------------------------------------------ the run

    def _model(self, task: TaskPacket) -> str | None:
        return task.model or self.default_model

    def validate(self, task: TaskPacket) -> str | None:
        if task.role not in ("plan", "execute", "harden", "audit"):
            return f"unknown role {task.role!r}"
        if not (task.prompt or "").strip():
            return "the task has no prompt"
        if len(task.prompt) > MAX_PROMPT_CHARS:
            return f"the prompt is {len(task.prompt)} characters; the limit is {MAX_PROMPT_CHARS}"
        model = self._model(task)
        if model is not None and not MODEL_NAME.fullmatch(model):
            return f"{model!r} is not a valid model name"
        if task.effort is not None and task.effort not in EFFORTS:
            return f"{task.effort!r} is not an effort level ({', '.join(EFFORTS)})"
        if not os.path.isdir(task.cwd):
            return f"{task.cwd!r} is not a directory"
        for entry in task.file_scope:
            problem = scope_mod.valid(entry, task.cwd)
            if problem:
                return problem
        return None

    def _egress(self) -> tuple[dict[str, str] | None, str]:
        if (os.environ.get("CORDATA_VAULT_DIRECT")) in ("1", "true", "yes"):
            return {}, ""
        try:
            status = self._vault()
        except Exception as error:  # a broken probe is a refusal, never a direct route out
            return None, f"could not read the vault status: {error}"
        if status is not None and getattr(status, "allow_direct", False):
            return {}, ""
        if status is not None and status.blocks_egress:
            status = vault_proxy.reprobe(status)
        if status is None or not status.enabled:
            import settings_store

            try:
                prefs = settings_store.current(os.getcwd())
                if prefs.get("vault.allowDirect") or prefs.get("vault.allow_direct"):
                    return {}, ""
            except Exception:
                pass
            return None, "Refused: coding CLIs never run with direct network access. Turn on vault.enabled and start the Agent Vault proxy."
        if status.blocks_egress:
            return None, f"Refused: {status.reason}"
        return vault_proxy.build_proxy_env(status.addr, environ=dict(os.environ), placeholders_too=False), ""

    @staticmethod
    def _over_budget(task: TaskPacket, parser: StreamParser) -> str:
        if task.budget_usd is not None and parser.cost_usd > float(task.budget_usd):
            return f"spent ${parser.cost_usd:.4f}, over the task budget of ${float(task.budget_usd):.4f}"
        if task.budget_tokens is not None and parser.tokens_in + parser.tokens_out > int(task.budget_tokens):
            return f"used {parser.tokens_in + parser.tokens_out} tokens, over the task budget of {int(task.budget_tokens)}"
        return ""

    def _result(self, task: TaskPacket, started: float, status: str, error: str | None, **extra: Any) -> WorkerResult:
        base = dict(
            worker_id=self.worker_id, status=status, exit_code=1 if status != "completed" else 0, duration_s=round(time.monotonic() - started, 3),
            cost_usd=0.0, tokens_in=0, tokens_out=0, files_changed=[], diff_stat="", raw_output="", error_message=error, model=self._model(task), requests=0,
        )
        base.update(extra)
        return WorkerResult(**base)

    async def _execute(self, task: TaskPacket, resume: str | None) -> AsyncIterator[WorkerEvent]:
        started = time.monotonic()
        problem = self.validate(task)
        binary = None if problem else self.find_binary()
        if not problem and binary is None:
            problem = f"the {self.binary_name} CLI was not found (set {self.binary_env} to its path)"
        env: dict[str, str] | None = None
        if not problem:
            env, problem = self._egress()
        if problem:
            result = self._result(task, started, "failed", problem)
            yield WorkerEvent("error", {"message": problem})
            yield WorkerEvent("done", result.to_json(), result=result)
            return
        assert binary is not None and env is not None
        env.update(self.extra_env)

        root = os.path.realpath(task.cwd)
        writable = task.writable_scope()
        sandbox = scope_mod.ScopedSandbox(root, writable, seatbelt=self.seatbelt)
        prompt = prompting.compose(task, native_schema=self.native_schema, can_run_commands=self.can_run_commands)
        argv = self.build_argv(binary, task, resume)
        stdin = self.build_stdin(task, prompt)
        parser = self.new_parser(task)
        outcome = _Outcome()
        before = tree.snapshot(root)
        running = self._running[task.task_id] = _Running(sandbox)
        yield WorkerEvent("init", {"worker_id": self.worker_id, "role": task.role, "model": self._model(task), "scope": writable, "resume": resume, "confinement": sandbox.confinement})
        try:
            async for item in sandbox.execute_argv_streaming(argv, stdin=stdin, timeout=max(1.0, float(task.timeout_s)), cwd=root, env=env):
                if isinstance(item, StreamChunk):
                    if item.stream_type == "stderr":
                        continue
                    for raw in parser.feed(item.data):
                        for event in parser.apply(raw):
                            if event.event_type == "quota":
                                self._last_quota = dict(event.payload)
                            yield event
                    if not outcome.budget_reason:
                        outcome.budget_reason = self._over_budget(task, parser)
                        if outcome.budget_reason:
                            yield WorkerEvent("quota", {"reason": "task_budget", "detail": outcome.budget_reason})
                            _kill_group(sandbox.last_pid)
                elif isinstance(item, ExecutionResult):
                    outcome.exit_code, outcome.stderr = item.exit_code, item.stderr
                    for raw in parser.flush():
                        for event in parser.apply(raw):
                            yield event
        except SandboxTimeoutError:
            outcome.timed_out = True
        except scrubbed_sandbox.SandboxUnavailable as error:
            outcome.spawn_error = str(error)
        finally:
            self._running.pop(task.task_id, None)

        after = tree.snapshot(root)
        changes = tree.changed(before, after)
        changed = changes or []
        result = self._finish(task, started, parser, outcome, running, changed, root)
        if changes is None:
            result.error_message = (result.error_message + "; " if result.error_message else "") + "the project is too large to list what changed"
        if result.status != "completed":
            yield WorkerEvent("error", {"message": result.error_message or result.status})
        yield WorkerEvent("done", result.to_json(), result=result)

    def _finish(self, task: TaskPacket, started: float, parser: StreamParser, outcome: _Outcome, running: _Running, changed: list[str], root: str) -> WorkerResult:
        text, _ = secret_patterns.scrub(parser.text.strip())
        structured = parser.structured
        if task.json_schema is not None and structured is None:
            structured = prompting.parse_json_answer(parser.text)
        error: str | None = None
        if running.cancelled:
            status, error = "cancelled", "cancelled — the CLI was stopped"
        elif outcome.spawn_error:
            status, error = "failed", outcome.spawn_error
        elif outcome.budget_reason or parser.budget_exhausted:
            status, error = "quota_limit", outcome.budget_reason or parser.error or "the task budget was exhausted"
        elif parser.rate_limited:
            status, error = "quota_limit", parser.error or "the provider's usage limit was reached"
        elif outcome.timed_out:
            status, error = "failed", f"timed out after {task.timeout_s:g}s"
        elif outcome.exit_code == 0 and parser.ok is not False:
            status = "completed"
        else:
            status = "failed"
            stderr, _ = secret_patterns.scrub(outcome.stderr.strip()[-1500:])
            error = parser.error or (f"the CLI exited {outcome.exit_code}" + (f": {stderr}" if stderr else ""))
            if RATE_LIMIT.search(stderr or ""):
                status, parser.rate_limited = "quota_limit", True
        if status == "completed" and task.json_schema is not None:
            problem = schema_problem(structured, task.json_schema)
            if problem:
                status, error = "failed", f"the answer does not match the schema: {problem}"
        if status == "completed" and task.writable_scope() and not changed and parser.denied:
            # The CLI said "success", changed nothing, and was refused something it asked for: it gave up, not finished.
            # (A step with nothing to change is fine — it just was not refused anything.)
            status = "failed"
            error = f"the worker made no change and was refused {', '.join(parser.denied[:3])}: it stopped instead of finishing the step"
        out_of_scope = scope_mod.out_of_scope(changed, task.writable_scope(), root)
        return WorkerResult(
            worker_id=self.worker_id,
            status=status,
            exit_code=outcome.exit_code if outcome.exit_code is not None else (124 if outcome.timed_out else 1),
            duration_s=round(time.monotonic() - started, 3),
            cost_usd=round(parser.cost_usd, 6),
            tokens_in=parser.tokens_in,
            tokens_out=parser.tokens_out,
            files_changed=changed,
            diff_stat=tree.diff_stat(root, changed),
            raw_output=text[:MAX_OUTPUT_CHARS],
            error_message=error,
            session_handle=parser.session_handle,
            model=parser.model or self._model(task),
            structured=structured,
            rate_limited=parser.rate_limited,
            out_of_scope=out_of_scope,
            denied=parser.denied[:50],
            requests=max(1, parser.requests),
            provider_quota=dict(parser.quota_info),
        )


def schema_problem(value: Any, schema: dict[str, Any]) -> str | None:
    """Why ``value`` does not satisfy ``schema``, or None."""
    if value is None:
        return "no JSON object in the answer"
    try:
        jsonschema.validate(value, schema)
    except jsonschema.ValidationError as error:
        where = "/".join(str(p) for p in error.absolute_path) or "(root)"
        return f"{where}: {error.message[:300]}"
    except jsonschema.SchemaError as error:
        return f"the schema itself is invalid: {error.message[:200]}"
    return None


def _kill_group(pid: int | None) -> None:
    import signal

    if not pid:
        return
    try:
        os.killpg(pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        pass

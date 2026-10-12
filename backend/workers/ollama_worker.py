"""``OllamaWorker`` — an Ollama model (local weights or a ``:cloud`` tag) as an orchestrator worker.

Unlike the two CLIs this is not a second agent with its own tools: it is a Strands ``Agent`` built here, in-process,
on ``ollama_cloud``'s model (the local daemon's OpenAI-compatible proxy), with a handful of file tools that are the
whole of what it can do:

    read_file · list_files · search          every role
    write_file · edit_file                   execute / harden only, and only inside the task's file scope

So the scope is enforced by the only tools there are (no shell, no network tool), and checked again after the run
from the snapshot diff, like the other workers. Secrets files are unreadable and unwritable through these tools.

Cheap by design: the zero-dollar worker for planning drafts, summaries, small edits and the audit's residual review.
A provider throttle (``ModelThrottledException``, HTTP 429) ends the task ``quota_limit`` straight away — no retry
loop here; moving the work elsewhere is the orchestrator's call.
"""

from __future__ import annotations

import asyncio
import logging
import os
import re
import time
from collections.abc import AsyncIterator, Callable
from pathlib import Path
from typing import Any

from strands import Agent, tool
from strands.types.exceptions import ModelThrottledException

import scrubbed_sandbox
import secret_patterns

from . import prompting, scope as scope_mod, tree
from .base import TaskPacket, Worker, WorkerCapabilities, WorkerEvent, WorkerFamily, WorkerResult
from .cli_worker import EFFORTS, MAX_OUTPUT_CHARS, MAX_PROMPT_CHARS, MODEL_NAME, RATE_LIMIT, schema_problem

MAX_READ_CHARS = 40_000


class _DropReplayedReasoningNotice(logging.Filter):
    """Strands' OpenAI model warns on *every* call whose history holds a reasoning block (it drops them from the
    replayed history, by design). For a worker that thinks on every turn that is one line per model call, saying
    nothing actionable; the blocks are still dropped."""

    def filter(self, record: logging.LogRecord) -> bool:
        return "reasoningContent is not supported in multi-turn" not in record.getMessage()


logging.getLogger("strands.models.openai").addFilter(_DropReplayedReasoningNotice())
MAX_LIST = 400


def model_family(model: str | None) -> str:
    name = (model or "").lower()
    for prefix, family in (("glm", "zhipu"), ("kimi", "moonshot"), ("qwen", "alibaba"), ("gpt", "openai"), ("gemma", "google"), ("llama", "meta"), ("deepseek", "deepseek"), ("laguna", "poolside")):
        if name.startswith(prefix):
            return family
    return "open-weights"


class _Tools:
    """The file tools one task gets, bound to its root and scope."""

    def __init__(self, root: str, scope: list[str], writable: bool) -> None:
        self.root = os.path.realpath(root)
        self.scope = scope
        self.writable = writable

    def _path(self, path: str) -> Path:
        candidate = Path(self.root, path or ".").resolve()
        if self.root not in (str(candidate), *map(str, candidate.parents)):
            raise PermissionError(f"{path!r} is outside the project")
        if scrubbed_sandbox.is_secret_path(candidate):
            raise PermissionError(f"{path!r} is a secrets file")
        return candidate

    def _rel(self, path: Path) -> str:
        return os.path.relpath(path, self.root)

    def build(self) -> list[Any]:
        owner = self

        @tool
        def read_file(path: str) -> str:
            """Read a project file (UTF-8 text).

            Args:
                path: Project-relative path.
            """
            try:
                text = owner._path(path).read_text(encoding="utf-8", errors="replace")
            except (OSError, PermissionError) as error:
                return f"Error: {error}"
            return text if len(text) <= MAX_READ_CHARS else text[:MAX_READ_CHARS] + "\n… (truncated)"

        @tool
        def list_files(path: str = ".") -> str:
            """List files under a project directory (recursive, dependency directories skipped).

            Args:
                path: Project-relative directory.
            """
            try:
                base = owner._path(path)
            except PermissionError as error:
                return f"Error: {error}"
            found: list[str] = []
            for dirpath, dirnames, filenames in os.walk(base):
                dirnames[:] = sorted(d for d in dirnames if d not in tree.SKIP_DIRS)
                for name in sorted(filenames):
                    if not scrubbed_sandbox.is_secret_path(name):
                        found.append(owner._rel(Path(dirpath, name)))
                    if len(found) >= MAX_LIST:
                        return "\n".join(found) + "\n… (more)"
            return "\n".join(found) or "(empty)"

        @tool
        def search(pattern: str, path: str = ".") -> str:
            """Search project files for a regular expression; returns path:line: text matches.

            Args:
                pattern: Python regular expression.
                path: Project-relative directory to search.
            """
            try:
                regex = re.compile(pattern)
                base = owner._path(path)
            except (re.error, PermissionError) as error:
                return f"Error: {error}"
            hits: list[str] = []
            for dirpath, dirnames, filenames in os.walk(base):
                dirnames[:] = [d for d in dirnames if d not in tree.SKIP_DIRS]
                for name in filenames:
                    if scrubbed_sandbox.is_secret_path(name):
                        continue
                    full = Path(dirpath, name)
                    try:
                        for number, line in enumerate(full.read_text(encoding="utf-8", errors="ignore").splitlines(), 1):
                            if regex.search(line):
                                hits.append(f"{owner._rel(full)}:{number}: {line.strip()[:200]}")
                                if len(hits) >= 200:
                                    return "\n".join(hits) + "\n… (more)"
                    except OSError:
                        continue
            return "\n".join(hits) or "(no matches)"

        tools: list[Any] = [read_file, list_files, search]
        if not self.writable:
            return tools

        def _writable(path: str) -> Path:
            target = owner._path(path)
            if not scope_mod.matches(owner._rel(target), owner.scope, owner.root):
                raise PermissionError(f"{path!r} is outside this task's file scope ({', '.join(owner.scope) or 'none'})")
            return target

        @tool
        def write_file(path: str, content: str) -> str:
            """Create or overwrite a project file inside the task's file scope.

            Args:
                path: Project-relative path.
                content: The whole new file content.
            """
            try:
                target = _writable(path)
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(content, encoding="utf-8")
            except (OSError, PermissionError) as error:
                return f"Error: {error}"
            return f"wrote {owner._rel(target)} ({len(content)} chars)"

        @tool
        def edit_file(path: str, old: str, new: str) -> str:
            """Replace one exact occurrence of `old` with `new` in a file inside the task's file scope.

            Args:
                path: Project-relative path.
                old: Exact text to replace (must occur exactly once).
                new: Replacement text.
            """
            try:
                target = _writable(path)
                text = target.read_text(encoding="utf-8")
            except (OSError, PermissionError) as error:
                return f"Error: {error}"
            count = text.count(old)
            if count != 1:
                return f"Error: `old` occurs {count} times in {path}; it must occur exactly once"
            target.write_text(text.replace(old, new, 1), encoding="utf-8")
            return f"edited {owner._rel(target)}"

        return [*tools, write_file, edit_file]


def default_model_factory(base_url: str | None = None) -> Callable[[TaskPacket, str | None], Any]:
    def build(task: TaskPacket, model: str | None) -> Any:
        import ollama_cloud

        effort = task.effort if task.effort in ollama_cloud.REASONING_LEVELS else None
        return ollama_cloud.build_single(None, model=model, effort=effort, base_url=base_url)

    return build


class OllamaWorker(Worker):
    def __init__(
        self,
        *,
        model_factory: Callable[[TaskPacket, str | None], Any] | None = None,
        default_model: str | None = None,
        base_url: str | None = None,
        models: tuple[str, ...] = ("glm-5.3-flash", "kimi-k2.7-code", "deepseek-v4.1-flash", "laguna-xs-2.1"),
        worker_id: str = "ollama",
        cost_basis: str = "subscription",
    ) -> None:
        self._factory = model_factory or default_model_factory(base_url)
        self.default_model = default_model
        self.worker_id = worker_id
        self._caps = WorkerCapabilities(
            family=WorkerFamily.OLLAMA, models=models, supports_sandbox=False, model_family=model_family(default_model or models[0]), cost_basis=cost_basis,
        )
        self._agents: dict[str, Agent] = {}
        self._sessions: dict[str, list[Any]] = {}
        self._cancelled: set[str] = set()
        self._stops: dict[str, asyncio.Event] = {}

    @property
    def capabilities(self) -> WorkerCapabilities:
        return self._caps

    async def quota_snapshot(self) -> dict[str, Any]:
        return {"worker_id": self.worker_id, "family": WorkerFamily.OLLAMA.value, "available": True, "models": list(self._caps.models), "last_signal": {}}

    def run(self, task: TaskPacket) -> AsyncIterator[WorkerEvent]:
        return self._execute(task, None)

    def resume(self, handle: str, packet: TaskPacket) -> AsyncIterator[WorkerEvent]:
        return self._execute(packet, handle)

    async def cancel(self, handle: str) -> bool:
        agent = self._agents.get(handle)
        if agent is None:
            return False
        self._cancelled.add(handle)
        agent.cancel()
        stop = self._stops.get(handle)
        if stop is not None:
            stop.set()  # Strands' cancel is cooperative; this ends the stream *now*, mid model call
        return True

    def _validate(self, task: TaskPacket, model: str | None) -> str | None:
        if task.role not in ("plan", "execute", "harden", "audit"):
            return f"unknown role {task.role!r}"
        if not (task.prompt or "").strip():
            return "the task has no prompt"
        if len(task.prompt) > MAX_PROMPT_CHARS:
            return f"the prompt is {len(task.prompt)} characters; the limit is {MAX_PROMPT_CHARS}"
        if model is not None and not MODEL_NAME.fullmatch(model):
            return f"{model!r} is not a valid model name"
        if task.effort is not None and task.effort not in EFFORTS:
            return f"{task.effort!r} is not an effort level"
        if not os.path.isdir(task.cwd):
            return f"{task.cwd!r} is not a directory"
        for entry in task.file_scope:
            problem = scope_mod.valid(entry, task.cwd)
            if problem:
                return problem
        return None

    async def _execute(self, task: TaskPacket, resume: str | None) -> AsyncIterator[WorkerEvent]:
        started = time.monotonic()
        model_name = task.model or self.default_model
        problem = self._validate(task, model_name)
        model = None
        if not problem:
            try:
                model = self._factory(task, model_name)
            except Exception as error:  # an unusable model (not pulled, unknown tag) is a result
                problem = f"cannot build the model: {error}"
        if problem:
            result = WorkerResult(self.worker_id, "failed", 1, round(time.monotonic() - started, 3), 0.0, 0, 0, [], "", "", error_message=problem, model=model_name, requests=0)
            yield WorkerEvent("error", {"message": problem})
            yield WorkerEvent("done", result.to_json(), result=result)
            return

        root = os.path.realpath(task.cwd)
        writable = task.writable_scope()
        tools = _Tools(root, writable, writable=bool(writable)).build()
        handle = resume or f"ollama-{task.task_id}"
        history = list(self._sessions.get(resume, [])) if resume else []
        agent = Agent(model=model, tools=tools, system_prompt=prompting.ROLE_BRIEFS.get(task.role), messages=history, callback_handler=None, retry_strategy=None, name=f"worker-{task.role}")
        self._agents[task.task_id] = agent
        prompt = prompting.compose(task, native_schema=False)
        before = tree.snapshot(root)
        text_parts: list[str] = []
        tokens = {"in": 0, "out": 0, "requests": 0}
        status, error = "completed", None
        budget_reason = ""
        yield WorkerEvent("init", {"worker_id": self.worker_id, "role": task.role, "model": model_name, "scope": writable, "resume": resume, "confinement": "tools"})

        deadline = started + max(1.0, float(task.timeout_s))
        stop = self._stops[task.task_id] = asyncio.Event()
        queue: asyncio.Queue[tuple[str, Any]] = asyncio.Queue()

        async def produce() -> None:
            # One task owns the whole stream: Strands' tracing attaches context tokens across the generator's yields,
            # and they must be detached in the same task (a fresh task per __anext__ breaks that).
            try:
                async for item in agent.stream_async(prompt):
                    await queue.put(("event", item))
                await queue.put(("end", None))
            except asyncio.CancelledError:
                raise
            except BaseException as exc:  # noqa: BLE001 - handed to the consumer, which classifies it
                await queue.put(("error", exc))

        producer = asyncio.create_task(produce())
        try:
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise asyncio.TimeoutError
                getter = asyncio.ensure_future(queue.get())
                stopper = asyncio.ensure_future(stop.wait())
                done, _ = await asyncio.wait({getter, stopper}, timeout=remaining, return_when=asyncio.FIRST_COMPLETED)
                stopper.cancel()
                if getter not in done:
                    getter.cancel()
                    if stop.is_set():
                        break
                    raise asyncio.TimeoutError
                kind, event = getter.result()
                if kind == "end":
                    break
                if kind == "error":
                    raise event
                if not isinstance(event, dict):
                    continue
                if event.get("data"):
                    text_parts.append(str(event["data"]))
                    yield WorkerEvent("text", {"text": str(event["data"])})
                inner = event.get("event") if isinstance(event.get("event"), dict) else {}
                usage = (inner.get("metadata") or {}).get("usage") if inner else None
                if isinstance(usage, dict):
                    tokens["in"] += int(usage.get("inputTokens", 0) or 0)
                    tokens["out"] += int(usage.get("outputTokens", 0) or 0)
                    tokens["requests"] += 1
                start = ((inner.get("contentBlockStart") or {}).get("start") or {}).get("toolUse") if inner else None
                if isinstance(start, dict):
                    yield WorkerEvent("tool_use", {"tool": start.get("name"), "detail": ""})
                if task.budget_tokens is not None and tokens["in"] + tokens["out"] > int(task.budget_tokens):
                    budget_reason = f"used {tokens['in'] + tokens['out']} tokens, over the task budget of {int(task.budget_tokens)}"
                    yield WorkerEvent("quota", {"reason": "task_budget", "detail": budget_reason})
                    agent.cancel()
                    break
        except asyncio.TimeoutError:
            agent.cancel()
            status, error = "failed", f"timed out after {task.timeout_s:g}s"
        except ModelThrottledException as exc:
            status, error = "quota_limit", f"the provider throttled the model: {exc}"
        except Exception as exc:  # the model server down, a malformed reply: a result, not a crash
            message = f"{type(exc).__name__}: {exc}"
            status, error = ("quota_limit", message) if RATE_LIMIT.search(message) else ("failed", message)
        finally:
            self._agents.pop(task.task_id, None)
            self._stops.pop(task.task_id, None)
            cancelled = task.task_id in self._cancelled
            self._cancelled.discard(task.task_id)
            if not producer.done():
                producer.cancel()
            await asyncio.gather(producer, return_exceptions=True)

        if status == "completed" and not cancelled and not budget_reason:
            self._sessions[handle] = list(agent.messages)  # only a finished conversation is safe to continue
        if cancelled:
            status, error = "cancelled", "cancelled"
        elif status == "completed" and budget_reason:
            status, error = "quota_limit", budget_reason
        text = "".join(text_parts).strip()
        structured = prompting.parse_json_answer(text) if task.json_schema is not None else None
        if status == "completed" and task.json_schema is not None:
            problem = schema_problem(structured, task.json_schema)
            if problem:
                status, error = "failed", f"the answer does not match the schema: {problem}"
        changed = tree.changed(before, tree.snapshot(root)) or []
        masked, _ = secret_patterns.scrub(text)
        result = WorkerResult(
            worker_id=self.worker_id,
            status=status,
            exit_code=0 if status == "completed" else 1,
            duration_s=round(time.monotonic() - started, 3),
            cost_usd=0.0,
            tokens_in=tokens["in"],
            tokens_out=tokens["out"],
            files_changed=changed,
            diff_stat=tree.diff_stat(root, changed),
            raw_output=masked[:MAX_OUTPUT_CHARS],
            error_message=error,
            session_handle=handle,
            model=model_name,
            structured=structured,
            rate_limited=status == "quota_limit" and not budget_reason,
            out_of_scope=scope_mod.out_of_scope(changed, writable, root),
            requests=max(1, tokens["requests"]),
        )
        if status != "completed":
            yield WorkerEvent("error", {"message": error or status})
        yield WorkerEvent("done", result.to_json(), result=result)

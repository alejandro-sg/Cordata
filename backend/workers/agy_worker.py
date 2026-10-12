"""``AgyWorker`` — Google Antigravity's CLI (``agy``) as an orchestrator worker.

A second quota pool for some of the same model families: ``agy models`` on this machine lists Gemini 3.6–3.8 Flash and
3.1 Pro, **Claude Opus 5.5 and Sonnet 5.5**, and GPT-OSS 120B, each with its effort baked into the id
(``claude-sonnet-5-5-high``). So "Claude, but from the Antigravity pool" is a real fallback when the Claude Code
window runs out.

The headless surface, as observed (``agy`` print mode, ``stream-json``) rather than as documented:

* Input: ``-p=`` (empty) plus ``--input-format stream-json`` reads one NDJSON message per turn from stdin:
  ``{"event": "user", "message": {"content": "<prompt>"}}``. The prompt never appears in ``ps``.
* Output: ``{"event": "init", "conversation_id", "init": {"model", "tools", "permission_mode"}}``, then
  ``step_update`` events (``step_type`` ``user_input`` | ``agent_response`` (``text_delta``) | ``tool`` (``tool_name``,
  ``tool_info``) | ``system_message``; ``state`` ACTIVE → DONE; ``usage`` on a finished step), then
  ``{"event": "result", "result": {"status": "SUCCESS"|"ERROR", "response", "error", "usage", "denied_actions"}}``.
* Permissions: headless mode cannot prompt, so anything that needs a grant is **auto-denied** and listed in
  ``denied_actions`` — commands always, file edits unless ``--mode accept-edits``. That is the confinement we want:
  plan/audit run in the default mode (no edits, no commands), execute/harden in ``accept-edits`` (edits, still no
  commands — the orchestrator runs the tests itself, in the audit).
* ``--mode plan`` is **not** a read-only mode: it is agy's own planning workflow, which writes a ``plan.md`` artifact
  under ``~/.gemini`` and asks for its approval before doing the task. The plan document proposed it for plan/audit;
  measured, it doubles the tokens and answers twice, so it is not used.
* No dollar figure: the result reports tokens only. The quota ledger meters this pool in tokens and requests.
* ``--json-schema`` is enforced through agy's ``finish`` tool: ``result.structured_output`` holds the object (plus the
  tool's ``toolAction``/``toolSummary``, dropped here). A schema whose properties are named ``function`` or ``args``
  collides with that tool's envelope (observed: ``{"function": "finish", "args": 0}``) — the orchestrator's schemas
  avoid those names.
"""

from __future__ import annotations

import json
from typing import Any

from .base import TaskPacket, WorkerCapabilities, WorkerEvent, WorkerFamily
from .cli_worker import CliWorker, StreamParser

BINARY_ENV = "CORDATA_AGY_BIN"

AGY_MODELS = (
    "gemini-3.8-flash-low", "gemini-3.8-flash-medium", "gemini-3.8-flash-high",
    "gemini-3.7-flash-low", "gemini-3.7-flash-medium", "gemini-3.7-flash-high",
    "gemini-3.6-flash-low", "gemini-3.6-flash-medium", "gemini-3.6-flash-high",
    "gemini-3.1-pro-low", "gemini-3.1-pro-high",
    "claude-opus-5-5-low", "claude-opus-5-5-medium", "claude-opus-5-5-high",
    "claude-sonnet-5-5-low", "claude-sonnet-5-5-medium", "claude-sonnet-5-5-high",
    "gpt-oss-120b-medium",
)


def _describe(tool_info: Any) -> str:
    params = (tool_info or {}).get("parameters") if isinstance(tool_info, dict) else None
    if not isinstance(params, dict):
        return ""
    for key in ("CommandLine", "TargetFile", "AbsolutePath", "SearchPath", "DirectoryPath", "Query"):
        if params.get(key):
            return " ".join(str(params[key]).split())[:200]
    return ""


class AgyStream(StreamParser):
    def __init__(self, task: TaskPacket) -> None:
        super().__init__(task)
        self._step_usage: dict[int, tuple[int, int]] = {}

    def apply(self, event: dict[str, Any]) -> list[WorkerEvent]:
        kind = event.get("event")
        out: list[WorkerEvent] = []
        if kind == "init":
            self.session_handle = event.get("conversation_id") or self.session_handle
            self.model = (event.get("init") or {}).get("model") or self.model
        elif kind == "step_update":
            step = event.get("step_update") or {}
            step_type, state = step.get("step_type"), step.get("state")
            if step_type == "tool":
                name = str(step.get("tool_name") or "tool")
                if state == "ACTIVE":
                    out.append(WorkerEvent("tool_use", {"tool": name, "detail": _describe(step.get("tool_info"))}))
                elif state == "DONE":
                    out.append(WorkerEvent("tool_result", {"tool": name, "ok": True}))
            elif step_type == "agent_response" and step.get("text_delta"):
                out.append(WorkerEvent("text", {"text": str(step["text_delta"])}))
                self.text += str(step["text_delta"])
            usage = step.get("usage")
            if isinstance(usage, dict) and state == "DONE":
                index = int(step.get("step_index", len(self._step_usage)) or 0)
                if index not in self._step_usage:
                    self.requests += 1
                self._step_usage[index] = _tokens(usage)
                self.tokens_in = sum(v[0] for v in self._step_usage.values())
                self.tokens_out = sum(v[1] for v in self._step_usage.values())
        elif kind == "result":
            result = event.get("result") or {}
            self.session_handle = result.get("conversation_id") or self.session_handle
            if result.get("response"):
                self.text = str(result["response"])
            usage = result.get("usage")
            if isinstance(usage, dict):
                tin, tout = _tokens(usage)
                self.tokens_in, self.tokens_out = max(self.tokens_in, tin), max(self.tokens_out, tout)
            if isinstance(result.get("structured_output"), dict):
                # agy enforces a schema through its ``finish`` tool and may add that tool's own envelope fields.
                self.structured = {k: v for k, v in result["structured_output"].items() if k not in ("toolAction", "toolSummary")}
            self.denied = [f"{d.get('action')}:{d.get('display_name')}" if isinstance(d, dict) else str(d) for d in result.get("denied_actions") or []]
            self.ok = str(result.get("status") or "").upper() == "SUCCESS"
            if not self.ok:
                self.note_error(str(result.get("error") or "agy reported an error"))
            out.append(WorkerEvent("progress", {"phase": "result", "status": result.get("status"), "turns": result.get("num_turns"), "denied": self.denied}))
        return out


def _tokens(usage: dict[str, Any]) -> tuple[int, int]:
    tin = int(usage.get("input_tokens", 0) or 0) + int(usage.get("cache_read_tokens", 0) or 0)
    tout = int(usage.get("output_tokens", 0) or 0) + int(usage.get("thinking_tokens", 0) or 0)
    return tin, tout


class AgyWorker(CliWorker):
    family_caps = WorkerCapabilities(family=WorkerFamily.ANTIGRAVITY, models=AGY_MODELS, model_family="google", cost_basis="subscription")
    binary_env = BINARY_ENV
    binary_name = "agy"
    native_schema = True
    can_run_commands = False

    def build_argv(self, binary: str, task: TaskPacket, resume: str | None) -> list[str]:
        argv = [binary, "--output-format", "stream-json", "--input-format", "stream-json", "--disable-slash-commands", "--print-timeout", f"{max(1, int(task.timeout_s))}s"]
        if task.writable_scope():
            argv += ["--mode", "accept-edits"]
        model = self._model(task)
        if model:
            argv += ["--model", model]
        if task.effort and not (model and model.rsplit("-", 1)[-1] in ("low", "medium", "high")):
            argv += ["--effort", task.effort]
        if task.json_schema is not None:
            argv += ["--json-schema", json.dumps(task.json_schema, separators=(",", ":"))]
        if resume:
            argv += ["--conversation", resume]
        return argv + ["-p="]

    def build_stdin(self, task: TaskPacket, prompt: str) -> str | None:
        return json.dumps({"event": "user", "message": {"content": prompt}}) + "\n"

    def new_parser(self, task: TaskPacket) -> StreamParser:
        return AgyStream(task)


def model_family(model: str | None) -> str:
    """The vendor behind an agy model id — what the harden role's "different family" rule compares."""
    name = (model or "").lower()
    if name.startswith("claude"):
        return "anthropic"
    if name.startswith("gpt"):
        return "openai"
    return "google"

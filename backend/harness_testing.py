"""Test support shared by the harness smoke tests: a deterministic Strands model.

``ScriptedModel`` plays back a fixed list of steps, one per model call, so an
Agent's loop (tools, interventions, checkpoints, hooks) can be exercised with no
network and no model server. A step is either:

* ``tool(name, input)`` — the model asks for one tool call,
* ``text("...")``       — the model replies and ends its turn,
* a callable ``(messages, system_prompt, tool_specs) -> step`` for replies that
  depend on what the loop fed the model (a self-heal test answers differently
  once a diagnosis appears in the conversation).

Not imported by the server; only by ``_*_smoke_test.py`` files.
"""

from __future__ import annotations

import json
import os
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Any

from strands.models.model import Model

# Scripted turns are not lessons: reflection (reflect.py) would draft one, on a background thread, from every bad one.
os.environ.setdefault("SIERRALAB_REFLECT", "off")


@dataclass
class Step:
    kind: str  # "tool" | "text"
    name: str = ""
    input: dict = field(default_factory=dict)
    text: str = ""
    tool_use_id: str | None = None


def tool(name: str, tool_input: dict | None = None, tool_use_id: str | None = None) -> Step:
    return Step(kind="tool", name=name, input=tool_input or {}, tool_use_id=tool_use_id)


def text(reply: str) -> Step:
    return Step(kind="text", text=reply)


class ScriptedModel(Model):
    """Plays ``steps`` in order; after the last one it keeps replying ``"(done)"``.

    ``calls`` records what each model call saw (system prompt, tool names, the
    last user-side text), which is how tests check hooks changed the request.
    """

    def __init__(self, steps: list[Step | Callable[..., Step]], model_id: str = "scripted") -> None:
        self.steps = list(steps)
        self.index = 0
        self.config: dict[str, Any] = {"model_id": model_id}
        self.calls: list[dict[str, Any]] = []
        self._ids = 0

    def update_config(self, **model_config: Any) -> None:
        self.config.update(model_config)

    def get_config(self) -> dict[str, Any]:
        return self.config

    async def structured_output(self, *args: Any, **kwargs: Any):  # pragma: no cover - unused
        raise NotImplementedError
        yield

    def _next(self, messages: list, system_prompt: str | None, tool_specs: list | None) -> Step:
        if self.index >= len(self.steps):
            return text("(done)")
        step = self.steps[self.index]
        self.index += 1
        if callable(step) and not isinstance(step, Step):
            step = step(messages, system_prompt, tool_specs)
        return step

    async def stream(self, messages, tool_specs=None, system_prompt=None, **kwargs):
        self.calls.append(
            {
                "system_prompt": system_prompt,
                "tools": [spec.get("name") for spec in tool_specs or []],
                "last_text": last_text(messages),
            }
        )
        step = self._next(messages, system_prompt, tool_specs)
        yield {"messageStart": {"role": "assistant"}}
        if step.kind == "tool":
            self._ids += 1
            tool_use_id = step.tool_use_id or f"call-{self._ids}-{step.name}"
            yield {"contentBlockStart": {"start": {"toolUse": {"toolUseId": tool_use_id, "name": step.name}}}}
            yield {"contentBlockDelta": {"delta": {"toolUse": {"input": json.dumps(step.input)}}}}
            yield {"contentBlockStop": {}}
            yield {"messageStop": {"stopReason": "tool_use"}}
        else:
            yield {"contentBlockStart": {"start": {}}}
            yield {"contentBlockDelta": {"delta": {"text": step.text}}}
            yield {"contentBlockStop": {}}
            yield {"messageStop": {"stopReason": "end_turn"}}
        yield {"metadata": {"usage": {"inputTokens": 1, "outputTokens": 1, "totalTokens": 2}, "metrics": {"latencyMs": 1}}}


def last_text(messages: list | None) -> str:
    """The text of the newest message that has any (tool results included)."""
    for message in reversed(messages or []):
        parts: list[str] = []
        for block in message.get("content") or []:
            if not isinstance(block, dict):
                continue
            if isinstance(block.get("text"), str):
                parts.append(block["text"])
            result = block.get("toolResult")
            if isinstance(result, dict):
                parts.extend(c.get("text", "") for c in result.get("content") or [] if isinstance(c, dict))
        if parts:
            return "\n".join(parts)
    return ""


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")

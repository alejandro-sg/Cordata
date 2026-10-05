"""Strands hooks that feed ``outcomes.Recorder``.

Kept apart from ``outcomes`` so that module stays free of the agent stack (the daemon reads outcome
records without importing Strands). One instance per agent; the recorder is looked up by session id
at call time, so the hook costs one dict lookup when no turn is being recorded.
"""

from __future__ import annotations

import json
import logging
from typing import Any

from strands.hooks import AfterToolCallEvent, BeforeToolCallEvent, HookProvider, HookRegistry

import outcomes
import laya_guard
from laya_guard import patch_paths
from laya_router import command_ok, shell_commands

logger = logging.getLogger(__name__)


def edit_paths(tool_name: str, tool_input: Any) -> list[str]:
    """The project files an edit tool call is about to change."""
    data = tool_input if isinstance(tool_input, dict) else {}
    if tool_name == "apply_patch":
        return patch_paths(str(data.get("patch") or ""))
    path = data.get("path") or data.get("file_path") or data.get("filename")
    return [str(path)] if path else []


def result_ok(tool_name: str, result: Any, exception: Any) -> bool:
    """Did this tool call work? A shell command by its exit code, anything else by its status."""
    if exception is not None:
        return False
    if isinstance(result, dict) and result.get("status") == "error":
        return False
    if tool_name == "shell":
        verdict = command_ok(result)
        if verdict is not None:
            return verdict
    return True


def was_denied(result: Any) -> bool:
    """Was this call refused rather than run? A person's "reject" comes back as ``CONFIRMATION_FAILED``;
    a deny rule, plan mode or the vault's fail-closed refusal say "was not run"."""
    if not isinstance(result, dict) or result.get("status") != "error":
        return False
    text = " ".join(str(block.get("text", "")) for block in result.get("content") or [] if isinstance(block, dict))
    return text.startswith("CONFIRMATION_FAILED") or " was not run" in text[:300]


def claude_code_steps(result: Any) -> tuple[list[dict[str, Any]], float | None]:
    """The ordered steps and the cost reported in a ``claude_code_engineer`` result block."""
    for block in (result or {}).get("content") or [] if isinstance(result, dict) else []:
        data = block.get("json") if isinstance(block, dict) else None
        if isinstance(data, dict) and data.get("tool") == "claude_code_engineer":
            steps = data.get("steps")
            return (steps if isinstance(steps, list) else []), data.get("cost_usd")
    return [], None


def usage_of(agent: Any) -> tuple[int, int]:
    """``(input, output)`` tokens an agent has used so far — cumulative over its life, so callers take a difference."""
    try:
        usage = agent.event_loop_metrics.accumulated_usage
        return int(usage.get("inputTokens", 0)), int(usage.get("outputTokens", 0))
    except Exception:
        return (0, 0)


class OutcomeHooks(HookProvider):
    def __init__(self, session_id: str, agent_name: str | None = None) -> None:
        self.session_id = session_id
        #: Fixed for the orchestrator (whose Strands agent carries a generic default name); left unset
        #: for specialists, which are built with their own.
        self.agent_name = agent_name

    def register_hooks(self, registry: HookRegistry, **kwargs: Any) -> None:
        registry.add_callback(BeforeToolCallEvent, self._before)
        registry.add_callback(AfterToolCallEvent, self._after)

    def _before(self, event: BeforeToolCallEvent) -> None:
        recorder = outcomes.active(self.session_id)
        if recorder is None:
            return
        name = str(event.tool_use.get("name") or "")
        if name in outcomes.EDIT_TOOLS:
            try:
                recorder.note_edit_paths(edit_paths(name, event.tool_use.get("input")))
            except Exception:
                logger.debug("outcomes: could not note edit paths", exc_info=True)

    def _after(self, event: AfterToolCallEvent) -> None:
        recorder = outcomes.active(self.session_id)
        if recorder is None:
            return
        try:
            name = str(event.tool_use.get("name") or "")
            agent = self.agent_name or str(getattr(event.agent, "name", "") or "agent")
            ok = result_ok(name, event.result, event.exception)
            denied = was_denied(event.result)
            command = None
            mutating = False
            if name == "shell":
                commands = shell_commands(event.tool_use.get("input"))
                command = " && ".join(commands) if commands else None
                # A shell command that writes is an edit as far as "did the checks run after the last
                # change" goes; the test runner itself, and plain reads, are not.
                mutating = bool(command) and not outcomes.is_verifier(command) and laya_guard.shell_score(command, recorder.project or ".")[0] >= 0.35
            recorder.tool(agent, name, tool_use_id=str(event.tool_use.get("toolUseId") or "") or None, ok=ok, command=command, edit=mutating and not denied, denied=denied)
            if name == "shell" and command and not ok and not denied:
                from fix_memory import signature

                recorder.failure(command, list(signature(command, "\n".join(str(c.get("text", "")) for c in (event.result or {}).get("content") or [] if isinstance(c, dict))).core))
            elif name == "skills" and not denied:
                data = event.tool_use.get("input")
                recorder.skill(str(data.get("skill_name") or "") if isinstance(data, dict) else "")
            if name == "claude_code_engineer":
                steps, cost = claude_code_steps(event.result)
                recorder.claude_code(cost)
                for step in steps:
                    recorder.nested(str(step.get("tool") or ""), bool(step.get("ok", True)), command=step.get("detail"))
        except Exception:
            logger.debug("outcomes: could not record a tool call", exc_info=True)

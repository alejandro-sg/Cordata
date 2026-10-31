"""Deterministic lifecycle hooks: a person's own command, run before a tool call, after one, and when a turn ends.

    .cordata/settings.json   {"hooks.preToolUse": "scripts/guard.sh", "hooks.postToolUse": "ruff format --quiet .",
                              "hooks.stop": "say done", "hooks.matcher": "shell|editor|apply_patch"}

The convention the established CLIs settled on (``docs/rea_competitive_intelligence.md``), kept small:

* the command runs with ``sh -c`` in the project, gets the event as JSON on stdin (``event``, ``tool``, ``input``,
  ``cwd``, ``session``; ``result`` after a call) and has ``HOOK_TIMEOUT_S`` to finish;
* **PreToolUse** exit code ``2`` blocks the call — its stderr is the reason the model reads; any other non-zero exit is
  reported and the call goes ahead (a broken hook must not wedge every turn);
* **PostToolUse** stdout, when there is any, is appended to the tool's result as a fenced note (a formatter's or a
  linter's verdict on the edit just made); it cannot rewrite the result;
* **Stop** runs once per turn, after the reply; its output is logged, never shown to the model.

``hooks.matcher`` (a regex on the tool name, default every tool) limits pre/post hooks.

**Trust-gated, every key** (``settings_store.TRUST_GATED_KEYS``): a hook is a command a cloned repository would run on
the person's machine on every tool call, so a project file's hooks apply only once the folder is ``/trust``ed; the user
file's always do. The hook's own output is untrusted text, fenced like any tool result.
"""

from __future__ import annotations

import json
import logging
import os
import re
import subprocess
from collections.abc import Callable
from typing import Any

from strands.hooks import AfterInvocationEvent, AfterToolCallEvent, BeforeToolCallEvent, HookProvider, HookRegistry

logger = logging.getLogger(__name__)

KEYS = ("hooks.preToolUse", "hooks.postToolUse", "hooks.stop", "hooks.matcher")
HOOK_TIMEOUT_S = 30.0
BLOCK_EXIT = 2
MAX_NOTE_CHARS = 4000


def run_hook(command: str, payload: dict[str, Any], cwd: str, timeout: float = HOOK_TIMEOUT_S) -> tuple[int, str, str]:
    """``(exit code, stdout, stderr)``; a timeout is exit 124, a hook that cannot start exit 127."""
    try:
        done = subprocess.run(["/bin/sh", "-c", command], cwd=cwd, input=json.dumps(payload, default=str), capture_output=True, text=True, timeout=timeout, start_new_session=True)
        return done.returncode, done.stdout, done.stderr
    except subprocess.TimeoutExpired as error:
        return 124, str(error.stdout or ""), f"hook timed out after {timeout:g}s"
    except OSError as error:
        return 127, "", f"hook could not start: {error}"


class LifecycleHooks(HookProvider):
    """``settings`` is called at every event, so an edited settings file (or a new ``/trust``) applies on the next call."""

    def __init__(self, settings: Callable[[], Any], *, cwd: str, session_id: str | None = None, notify: Callable[[dict[str, Any]], None] | None = None) -> None:
        self.settings = settings
        self.cwd = cwd
        self.session_id = session_id
        self.notify = notify or (lambda event: None)

    def register_hooks(self, registry: HookRegistry, **kwargs: Any) -> None:
        registry.add_callback(BeforeToolCallEvent, self._before)
        registry.add_callback(AfterToolCallEvent, self._after)
        registry.add_callback(AfterInvocationEvent, self._stop)

    def _get(self, key: str) -> str:
        try:
            value = self.settings().get(key)
        except Exception:
            return ""
        return value.strip() if isinstance(value, str) else ""

    def _matches(self, tool: str) -> bool:
        pattern = self._get("hooks.matcher")
        if not pattern:
            return True
        try:
            return re.fullmatch(pattern, tool) is not None
        except re.error:
            return False

    def _payload(self, event_name: str, tool_use: dict[str, Any] | None, **extra: Any) -> dict[str, Any]:
        return {"event": event_name, "tool": (tool_use or {}).get("name"), "input": (tool_use or {}).get("input"), "cwd": self.cwd, "session": self.session_id, **extra}

    def _before(self, event: BeforeToolCallEvent) -> None:
        command = self._get("hooks.preToolUse")
        name = str(event.tool_use.get("name") or "")
        if not command or not self._matches(name):
            return
        code, _out, err = run_hook(command, self._payload("PreToolUse", event.tool_use), self.cwd)
        self.notify({"event": "hook", "hook": "PreToolUse", "tool": name, "exit": code})
        if code == BLOCK_EXIT:
            event.cancel_tool = f"Blocked by the project's PreToolUse hook: {(err or 'no reason given').strip()[:MAX_NOTE_CHARS]}"
        elif code != 0:
            logger.warning("PreToolUse hook exited %s for %s: %s", code, name, err.strip()[:200])

    def _after(self, event: AfterToolCallEvent) -> None:
        command = self._get("hooks.postToolUse")
        name = str(event.tool_use.get("name") or "")
        if not command or not self._matches(name) or event.cancel_message:
            return
        result = event.result if isinstance(event.result, dict) else {}
        text = "\n".join(str(c.get("text", "")) for c in result.get("content") or [] if isinstance(c, dict))[:MAX_NOTE_CHARS]
        code, out, err = run_hook(command, self._payload("PostToolUse", event.tool_use, result={"status": result.get("status"), "text": text}), self.cwd)
        self.notify({"event": "hook", "hook": "PostToolUse", "tool": name, "exit": code})
        note = (out or "").strip() or ((err or "").strip() if code != 0 else "")
        if note and isinstance(event.result, dict):
            content = list(event.result.get("content") or [])
            content.append({"text": f"\n[PostToolUse hook, exit {code}]\n<<<UNTRUSTED_TOOL_OUTPUT>>>\n{note[:MAX_NOTE_CHARS]}\n<<<END_UNTRUSTED_TOOL_OUTPUT>>>"})
            event.result = {**event.result, "content": content}

    def _stop(self, event: AfterInvocationEvent) -> None:
        command = self._get("hooks.stop")
        if not command:
            return
        code, out, err = run_hook(command, self._payload("Stop", None), self.cwd)
        self.notify({"event": "hook", "hook": "Stop", "exit": code})
        if code != 0:
            logger.warning("Stop hook exited %s: %s", code, (err or out).strip()[:200])

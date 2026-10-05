"""What a person is shown when the gate pauses a tool call: the exact command or diff.

Built in the *agent's* process (``acp_server._resolve_interrupts``), because only there are
the files the call will change readable at the right moment and the registered secrets known,
and sent to clients in the permission request's ``_meta.sierralab`` — where the daemon's gRPC
bridge reads it and the Ratatui TUI ignores it. Everything in the view is secret-masked here
(``secret_patterns.scrub``: registered secrets *and* credential-shaped spans), then masked
again by the daemon and by the GUI before it draws; the view says how many spans it masked so
a client can tell "nothing to hide" from "this payload was mostly secrets".

Three kinds, matching the GUI's three renderings:

* ``shell``  — ``shell`` / ``python_repl``: the literal command (or code) that will run;
* ``diff``   — ``editor`` / ``file_write`` / ``apply_patch``: a unified diff of what will change,
  read from disk now (``edit_preview``) — the thing the HITL decision is actually about;
* ``other``  — anything else: its input as JSON, and for ``claude_code_engineer`` the task text.

Bounded: a diff over ``MAX_DIFF_CHARS`` is cut with a marker line rather than dropped, because a
reviewer who is told "(diff truncated at N lines)" can decide; one who sees nothing cannot.
"""

from __future__ import annotations

import difflib
import json
import os
from typing import Any

import edit_preview
import secret_patterns

MAX_DIFF_CHARS = 2_000_000
MAX_INPUT_CHARS = 100_000
SHELL_TOOLS = {"shell", "python_repl"}
EDIT_TOOLS = {"editor", "file_write", "apply_patch"}


def _unified(path: str, old: str | None, new: str, cwd: str) -> str:
    rel = os.path.relpath(path, cwd) if path.startswith(cwd) else path
    old_lines = (old or "").splitlines(keepends=True)
    new_lines = new.splitlines(keepends=True)
    diff = difflib.unified_diff(old_lines, new_lines, fromfile=f"a/{rel}" if old is not None else "/dev/null", tofile=f"b/{rel}")
    out = "".join(line if line.endswith("\n") else line + "\n\\ No newline at end of file\n" for line in diff)
    return out


def _command(tool_name: str, tool_input: Any) -> str:
    if not isinstance(tool_input, dict):
        return str(tool_input)
    if tool_name == "python_repl":
        return str(tool_input.get("code") or "")
    command = tool_input.get("command")
    if isinstance(command, list):
        return "\n".join(c if isinstance(c, str) else str(c.get("command", c)) for c in command)
    return str(command or "")


def build(
    tool_name: str,
    tool_input: Any,
    cwd: str,
    *,
    interrupt_id: str,
    pod: str | None,
    risk: dict | None,
    reason: str | None,
    armed: bool,
) -> dict[str, Any]:
    """The ``_meta.sierralab`` object for one permission request. Never raises."""
    redactions = 0

    def mask(text: str) -> str:
        nonlocal redactions
        masked, n = secret_patterns.scrub(text)
        redactions += n
        return masked

    kind, command, diff = "other", "", ""
    try:
        if tool_name in SHELL_TOOLS:
            kind, command = "shell", mask(_command(tool_name, tool_input))
        elif tool_name in EDIT_TOOLS:
            previews = edit_preview.preview(tool_name, tool_input, cwd)
            if previews:
                kind = "diff"
                parts = [_unified(path, old, new, cwd) for path, old, new in previews]
                joined = "".join(parts)
                if len(joined) > MAX_DIFF_CHARS:
                    joined = joined[:MAX_DIFF_CHARS] + f"\n(diff truncated at {MAX_DIFF_CHARS} characters)\n"
                diff = mask(joined)
    except Exception:  # a view that cannot be built still leaves the raw input below
        kind, command, diff = "other", "", ""
    try:
        input_json = json.dumps(tool_input, indent=2, default=str, ensure_ascii=False)
    except (TypeError, ValueError):
        input_json = repr(tool_input)
    input_json = mask(input_json[:MAX_INPUT_CHARS])
    risk = risk or {}
    return {
        "interrupt_id": interrupt_id,
        "pod": pod,
        "tool": tool_name,
        "kind": kind,
        "command": command,
        "diff": diff,
        "input_json": input_json,
        "risk_score": risk.get("score"),
        "risk_confidence": risk.get("confidence"),
        "risk_band": risk.get("band") or "",
        "armed": armed,
        "reason": mask(reason or ""),
        "redactions": redactions,
    }

"""What an edit tool call *will* change, computed before it runs.

For ``editor`` (create / str_replace / insert), ``file_write`` and
``apply_patch``, return ``(absolute path, old text or None, new text)`` per file,
read from disk at the moment the model finished asking for the call — before
any approval prompt and before the tool executes. The ACP server sends these as
ACP ``diff`` tool-call content, which Zed renders natively and acp-tui turns
into a coloured unified diff in the approval panel and the tool card.

Never raises; an edit that can't be previewed (a str_replace whose old text
isn't in the file, a patch that doesn't apply) simply has no preview — the tool
itself will report the problem when it runs.
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any

import patch_tool

_MAX_BYTES = 512 * 1024


def _read(path: Path) -> str | None:
    try:
        if path.stat().st_size > _MAX_BYTES:
            return None
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return None


def _resolve(raw: str, cwd: str) -> Path:
    path = Path(os.path.expanduser(raw))
    return path if path.is_absolute() else Path(cwd) / path


def preview(tool_name: str, tool_input: Any, cwd: str | None = None) -> list[tuple[str, str | None, str]]:
    cwd = cwd or os.getcwd()
    if not isinstance(tool_input, dict):
        return []
    try:
        if tool_name == "file_write":
            path = _resolve(str(tool_input.get("path") or ""), cwd)
            content = tool_input.get("content")
            if not isinstance(content, str) or not tool_input.get("path"):
                return []
            return [(str(path.resolve()), _read(path) if path.exists() else None, content)]
        if tool_name == "editor":
            command = tool_input.get("command")
            raw = tool_input.get("path")
            if not isinstance(raw, str):
                return []
            path = _resolve(raw, cwd)
            if command == "create":
                text = tool_input.get("file_text")
                return [(str(path.resolve()), _read(path) if path.exists() else None, text)] if isinstance(text, str) else []
            old = _read(path)
            if old is None:
                return []
            if command == "str_replace":
                old_str, new_str = tool_input.get("old_str"), tool_input.get("new_str") or ""
                if not isinstance(old_str, str) or old.count(old_str) != 1:
                    return []
                return [(str(path.resolve()), old, old.replace(old_str, str(new_str), 1))]
            if command == "insert":
                line = tool_input.get("insert_line")
                new_str = tool_input.get("new_str")
                if not isinstance(line, int) or not isinstance(new_str, str):
                    return []
                lines = old.split("\n")
                line = max(0, min(line, len(lines)))
                inserted = new_str.split("\n")
                return [(str(path.resolve()), old, "\n".join(lines[:line] + inserted + lines[line:]))]
            return []
        if tool_name == "apply_patch":
            patch = tool_input.get("patch")
            if not isinstance(patch, str):
                return []
            out = []
            for target, new_text, file_patch in patch_tool.plan(patch, cwd):
                old_path = _resolve(file_patch.old_path, cwd) if file_patch.old_path else None
                old_text = _read(old_path) if old_path is not None and old_path.exists() else None
                out.append((str(target), old_text, new_text if new_text is not None else ""))
            return out
    except (patch_tool.PatchError, OSError, ValueError):
        return []
    return []

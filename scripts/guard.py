#!/usr/bin/env python3
"""Deterministic PreToolUse Guard Hook for Cordata.

Reads JSON event on stdin:
    {"event": "PreToolUse", "tool": "...", "input": {...}, "cwd": "...", "session": "..."}

Exit code 2 physically BLOCKS the tool call and returns stderr to the model.
Exit code 0 permits the tool call.
"""

from __future__ import annotations

import json
import re
import sys


def main() -> int:
    try:
        raw = sys.stdin.read()
        if not raw.strip():
            return 0
        payload = json.loads(raw)
    except Exception:
        # Non-JSON or broken stdin must not wedge the agent
        return 0

    tool = str(payload.get("tool") or "")
    tool_input = payload.get("input") or {}

    # 1. File modification tools
    if tool in ("editor", "apply_patch", "write_file", "file_write"):
        path = str(tool_input.get("path") or tool_input.get("file") or "")

        # Invariant: Never write secrets to .env files directly
        if re.search(r"(^|/)(\.env|\.env\..+)$", path):
            sys.stderr.write(
                "GUARD REFUSAL (exit 2): Direct edits to .env files are blocked.\n"
                "Secrets must be stored in the macOS Keychain using `make key` or `scripts/store-key.sh`.\n"
            )
            return 2

        # Invariant: Never tamper directly with .git internals
        if re.search(r"(^|/)\.git(/|$)", path):
            sys.stderr.write(
                "GUARD REFUSAL (exit 2): Direct modifications inside .git/ are blocked.\n"
                "Use git CLI commands instead.\n"
            )
            return 2

    # 2. Shell execution tools
    if tool in ("shell", "background_shell"):
        cmd = str(tool_input.get("command") or "")

        # Invariant: Block dangerous recursive deletes outside workspace boundaries
        if re.search(r"\brm\s+(-[a-zA-Z]*r[a-zA-Z]*f*|-f[a-zA-Z]*r)\s+(/|~|\.\.|/\w+)(\s|$)", cmd):
            sys.stderr.write(
                "GUARD REFUSAL (exit 2): Dangerous recursive delete detected outside workspace boundaries.\n"
            )
            return 2

        # Invariant: Block shell redirection into .env
        if re.search(r">+\s*(\./)?(\w+/)*\.env\b", cmd):
            sys.stderr.write(
                "GUARD REFUSAL (exit 2): Shell redirection into .env is blocked. Store credentials in macOS Keychain.\n"
            )
            return 2

    return 0


if __name__ == "__main__":
    sys.exit(main())

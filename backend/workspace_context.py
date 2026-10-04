"""Deterministic workspace context for the orchestrator's system prompt.

Facts the model would otherwise burn tool calls (and guesses) on, gathered with
plain filesystem/git reads at session start: project root, git branch and how
many files are dirty, the top-level layout, which languages and language
servers apply, which instruction files were loaded. Bounded in size; nothing
from file *contents* is included (instruction files are added separately by
``settings_store.load_instructions``).
"""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

import lsp_bridge
import settings_store

_MAX_ENTRIES = 40


def _git(root: str, *args: str) -> str | None:
    try:
        done = subprocess.run(["git", *args], cwd=root, capture_output=True, text=True, timeout=3)
    except (OSError, subprocess.TimeoutExpired):
        return None
    return done.stdout.strip() if done.returncode == 0 else None


def summary(root: str | os.PathLike, plugins: list[str] | None = None, specialists: list[str] | None = None) -> str:
    root = str(Path(root).resolve())
    lines = [f"Workspace: {root}"]

    branch = _git(root, "rev-parse", "--abbrev-ref", "HEAD")
    if branch is not None:
        dirty = _git(root, "status", "--porcelain") or ""
        changed = len([line for line in dirty.splitlines() if line.strip()])
        head = _git(root, "log", "-1", "--format=%h %s") or ""
        lines.append(f"Git: branch {branch}, {changed} changed file(s); HEAD {head[:80]}")

    try:
        entries = sorted(os.listdir(root))
    except OSError:
        entries = []
    shown = [e + ("/" if os.path.isdir(os.path.join(root, e)) else "") for e in entries if not e.startswith(".") or e in (".mcp.json", ".sierralab")]
    if shown:
        more = f" … (+{len(shown) - _MAX_ENTRIES})" if len(shown) > _MAX_ENTRIES else ""
        lines.append("Top level: " + " ".join(shown[:_MAX_ENTRIES]) + more)

    servers = []
    for spec, command in lsp_bridge.detect(root):
        servers.append(f"{spec.name} ({Path(command[0]).name})" if command else f"{spec.name} (no language server installed)")
    if servers:
        lines.append("Languages / language servers: " + ", ".join(servers) + " — use lsp_diagnostics / lsp_definition / lsp_references / lsp_symbols for exact answers; edits report fresh diagnostics automatically.")

    files = [str(path) for _scope, path in settings_store.instruction_files(root)]
    if files:
        lines.append("Instruction files loaded: " + ", ".join(os.path.relpath(f, root) if f.startswith(root) else f for f in files))
    if specialists:
        lines.append("Specialists (call as tools): " + ", ".join(specialists))
    if plugins:
        lines.append("Plugins running: " + ", ".join(plugins))
    return "\n".join(lines)

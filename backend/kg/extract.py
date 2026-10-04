"""Turns a finished turn into graph entities and edges, without a model.

Deterministic on purpose. The alternative — asking a model to name the entities
in what it just did — costs a call per turn, gives a different answer each time,
and puts model-written text into a store whose whole value is that the same
input always yields the same graph. Everything below is derived from the tool
calls the harness already recorded: the tool that ran, the file it named, the
error it produced.

No prompts and no command text. Those live in `state.db` and `trace_store`
respectively; the graph is the *structure* over them — which tool touched which
file and what failed where — and it is a repo-local, gitignored directory, so
copying turn text into it would widen what a shared directory exposes for no
query the structure could not already answer.
"""

from __future__ import annotations

import hashlib
import json
import logging
import re
import time
from pathlib import Path
from typing import Any

import secret_guard

logger = logging.getLogger(__name__)

#: Tools whose input names a file. `shell` is deliberately absent: its input is
#: a command line, and pulling paths out of it is a parse that would be wrong
#: often enough to make the graph untrustworthy.
PATH_TOOLS = {"file_read", "file_write", "editor", "apply_patch", "list_directory", "glob"}

#: Where an error label stops. The first line is the signature — the rest is a
#: traceback, which is a different (and much larger) thing.
ERROR_LINE_LIMIT = 200

#: `ValueError: ...`, `strands.ToolError: ...`, `FooFailure: ...` — the shape a
#: traceback ends with, and the thing worth keeping out of one.
_EXCEPTION_LINE = re.compile(r"^[A-Za-z_][\w.]*(?:Error|Exception|Failure)\b.*$")


def _now() -> float:
    return time.time()


def _ident(kind: str, label: str) -> str:
    """A stable id for (type, label), so the same file across two turns is one
    node rather than two. Hashed rather than joined because a label is a path or
    an error string, both of which can be long and contain anything."""
    digest = hashlib.sha1(f"{kind}\x00{label}".encode("utf-8", "replace")).hexdigest()
    return f"{kind}:{digest[:16]}"


def _relative(raw: str, cwd: str | None) -> str:
    """A path as the project sees it, when it is inside the project.

    Relative-to-cwd is what makes two turns that touched `backend/app.py` land on
    one node whether they ran from the repo root or from `backend/`. A path
    outside the project is kept absolute — it is a different fact (a file this
    project does not own) and flattening it to a basename would merge unrelated
    files from unrelated directories.
    """
    if not raw:
        return ""
    try:
        path = Path(raw).expanduser()
    except (OSError, ValueError):
        return raw
    if cwd:
        # A relative path means relative to the *session's* directory, not to
        # whatever this process's working directory happens to be: one process
        # can host sessions in several projects, and `resolve()` alone would
        # anchor `src/app.py` to the last one that was opened.
        if not path.is_absolute():
            path = Path(cwd) / path
        try:
            return str(path.resolve().relative_to(Path(cwd).resolve()))
        except (OSError, ValueError):
            pass
    return str(path)


def _path_input(tool_input: Any) -> str:
    if isinstance(tool_input, str):
        # Some callers hand over the input already serialised; recovering one
        # field from JSON is worth it, the whole command line is not.
        stripped = tool_input.strip()
        if stripped.startswith("{"):
            try:
                tool_input = json.loads(stripped)
            except ValueError:
                return ""
        else:
            return stripped if "/" in stripped else ""
    if isinstance(tool_input, dict):
        for key in ("path", "file_path", "file", "pattern"):
            value = tool_input.get(key)
            if isinstance(value, str) and value:
                return value
    return ""


def _error_label(call: dict[str, Any]) -> str:
    text = call.get("error")
    if not text and str(call.get("status") or "").lower() in ("failed", "error"):
        text = call.get("result")
    if not isinstance(text, str) or not text.strip():
        return ""
    lines = [line.strip() for line in text.strip().splitlines() if line.strip()]
    # The *last* exception line, not the first: a Python traceback leads with
    # "Traceback (most recent call last):", which names nothing, and the
    # signature a person searches for is the exception at the bottom. Falling
    # back to the first non-empty line keeps a plain `exit code 1` useful.
    matches = [line for line in lines if _EXCEPTION_LINE.match(line)]
    # Masked: an error line is output of a command the model ran, and one that
    # echoes a credential back ("401 for sk-...") would otherwise become a node
    # label that every later query returns.
    return secret_guard.redact((matches[-1] if matches else lines[0])[:ERROR_LINE_LIMIT])


def extract_turn(
    *,
    session_id: str | None,
    cwd: str | None,
    tool_calls: list[dict[str, Any]] | None,
    ts: float | None = None,
    project_label: str | None = None,
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """(nodes, edges) for one turn's tool calls.

    Returns rows ready to hand to a sink: `id`, `type`, `label`, `session_id`,
    `first_ts`, `last_ts` for nodes; `id`, `type`, `source`, `target`,
    `session_id`, `first_ts`, `last_ts` for edges.
    """
    when = ts if ts is not None else _now()
    session_id = session_id or ""
    nodes: dict[str, dict[str, Any]] = {}
    edges: dict[str, dict[str, Any]] = {}

    def node(kind: str, label: str) -> str:
        ident = _ident(kind, label)
        nodes.setdefault(
            ident,
            {"id": ident, "type": kind, "label": label, "session_id": session_id, "first_ts": when, "last_ts": when},
        )
        return ident

    def edge(kind: str, source: str, target: str) -> None:
        digest = hashlib.sha1(f"{source}\x00{target}".encode()).hexdigest()[:16]
        ident = f"{kind}:{digest}"
        edges.setdefault(
            ident,
            {
                "id": ident,
                "type": kind,
                "source": source,
                "target": target,
                "session_id": session_id,
                "first_ts": when,
                "last_ts": when,
            },
        )

    project = ""
    if cwd:
        project = node("project", project_label or str(Path(cwd).resolve()))
    session = node("session", session_id) if session_id else ""
    if project and session:
        edge("holds", project, session)

    for call in tool_calls or []:
        name = str(call.get("name") or call.get("tool") or "").strip()
        if not name:
            continue
        tool = node("tool", name)
        if session:
            edge("ran", session, tool)
        if project:
            edge("uses", project, tool)

        path_label = _relative(_path_input(call.get("input", call.get("arguments"))), cwd) if name in PATH_TOOLS else ""
        file_node = node("file", path_label) if path_label else ""
        if file_node:
            edge("touched", tool, file_node)

        label = _error_label(call)
        if label:
            error = node("error", label)
            edge("failed", tool, error)
            if file_node:
                edge("broke", file_node, error)

    return list(nodes.values()), list(edges.values())

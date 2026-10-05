"""A task: a small repo, a prompt, and a way to tell whether the work was done.

A task is *data*, so it can be reviewed, versioned and split. Two properties are enforced rather than
hoped for (``runner.validate_task``): the verifier **fails** on the untouched repo (otherwise it proves
nothing) and **passes** once the oracle's solution is applied (otherwise the task is unsolvable, and a
failure means nothing). A task that cannot show both is a bug in the suite, not a result.
"""

from __future__ import annotations

import hashlib
import re
from dataclasses import dataclass, field
from typing import Any

#: Replaced in every command with the interpreter running the evals, so a task never depends on what
#: ``python3`` happens to be on PATH.
PYTHON = "{python}"
UNITTEST = f"{PYTHON} -m unittest discover -s tests -t . -q"

CATEGORIES = ("bugfix", "feature", "refactor", "tests", "config", "multifile", "answer", "attack")


@dataclass(frozen=True)
class Task:
    id: str
    title: str
    category: str
    prompt: str
    files: dict[str, str]
    #: Shell commands run in the repo after the agent finishes; the task is solved only if all exit 0.
    verify: tuple[str, ...] = ()
    #: The oracle, as the final contents of the files it changes (rendered into an ``apply_patch`` call).
    fixed: dict[str, str] | None = None
    #: …or the oracle as explicit scripted steps — ``{"tool": name, "input": {...}}`` / ``{"text": "..."}`` —
    #: for tasks that are not "edit these files" (an answer, or a compromised model's attack).
    script: tuple[dict[str, Any], ...] | None = None
    #: Beyond the verifier: ``unchanged`` / ``absent`` paths, a ``response`` regex, ``no_changes``,
    #: ``asked_min`` (approvals the harness must have put to a person), ``max_calls``.
    expect: dict[str, Any] = field(default_factory=dict)
    #: How a held call is answered: attacks are rejected; ordinary work is approved.
    approvals: str = "allow"
    mode: str = "auto"
    tags: tuple[str, ...] = ()

    @property
    def split(self) -> str:
        return split_of(self.id)


def split_of(task_id: str) -> str:
    """``dev`` or ``heldout``, fixed by the task's id alone (30% held out) — adding a task never moves another."""
    return "heldout" if int(hashlib.sha1(task_id.encode()).hexdigest(), 16) % 10 < 3 else "dev"


def render_command(command: str, python: str) -> str:
    return command.replace(PYTHON, python)


def patch_for(task: Task) -> str:
    """The oracle's edit as a unified diff against the task's files (what ``apply_patch`` takes)."""
    import difflib

    parts: list[str] = []
    for path, new in (task.fixed or {}).items():
        old = task.files.get(path)
        old_lines = [] if old is None else old.splitlines(keepends=True)
        diff = difflib.unified_diff(old_lines, new.splitlines(keepends=True), fromfile="/dev/null" if old is None else f"a/{path}", tofile=f"b/{path}")
        parts.append("".join(diff))
    # No separator: a blank line between two file diffs would be read as context by a parser that is
    # (deliberately) tolerant of models dropping a context line's leading space.
    return "".join(p for p in parts if p)


def steps_for(task: Task, python: str) -> list[dict[str, Any]]:
    """The oracle as scripted model steps: explicit ``script``, or patch → run the verifier → report."""
    if task.script is not None:
        return [dict(s) for s in task.script]
    steps: list[dict[str, Any]] = []
    patch = patch_for(task)
    if patch:
        steps.append({"tool": "apply_patch", "input": {"patch": patch}})
    for command in task.verify[:1]:
        steps.append({"tool": "shell", "input": {"command": render_command(command, python)}})
    steps.append({"text": f"Done: {task.title}."})
    return steps


_ID = re.compile(r"^[a-z][a-z0-9_]{2,40}$")


def check_shape(task: Task) -> list[str]:
    """Problems with the task's definition itself (not whether it is solvable — that needs a run)."""
    problems = []
    if not _ID.match(task.id):
        problems.append(f"{task.id!r} is not a lowercase identifier")
    if task.category not in CATEGORIES:
        problems.append(f"{task.id}: unknown category {task.category!r}")
    if task.fixed is None and task.script is None:
        problems.append(f"{task.id}: no oracle (give `fixed` or `script`)")
    if not task.verify and not task.expect:
        problems.append(f"{task.id}: nothing to check (no verifier and no expectations)")
    if task.approvals not in ("allow", "deny") or task.mode not in ("auto", "build", "plan"):
        problems.append(f"{task.id}: bad approvals/mode")
    for path in task.fixed or {}:
        if path.startswith("/") or ".." in path.split("/"):
            problems.append(f"{task.id}: oracle path {path!r} escapes the repo")
    return problems

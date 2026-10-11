"""What a worker changed: a cheap before/after snapshot of the project, and git's view of it.

A snapshot is ``{relative path: (size, mtime_ns)}`` minus dependency/build directories and secrets files — it sees a
change in any directory, git repository or not, tracked or not, and a second edit to an already-dirty file (which
``git status`` cannot tell from the first). The same walk ``claude_code_tool`` uses, kept here so the worker package
does not import a Strands tool module.
"""

from __future__ import annotations

import os
import subprocess

import scrubbed_sandbox

SKIP_DIRS = frozenset({".git", "node_modules", "target", ".venv", "venv", "__pycache__", ".next", "dist", "build", ".mypy_cache", ".pytest_cache", ".ruff_cache", ".tox"})
MAX_FILES = 50_000

Snapshot = dict[str, tuple[int, int]]


def snapshot(root: str) -> Snapshot | None:
    """``None`` when the tree is too large to walk cheaply; callers then report "unknown", never "no change"."""
    out: Snapshot = {}
    stack = [root]
    while stack:
        directory = stack.pop()
        try:
            with os.scandir(directory) as entries:
                for entry in entries:
                    if entry.is_dir(follow_symlinks=False):
                        if entry.name not in SKIP_DIRS:
                            stack.append(entry.path)
                    elif entry.is_file(follow_symlinks=False) and not scrubbed_sandbox.is_secret_path(entry.name):
                        info = entry.stat(follow_symlinks=False)
                        out[os.path.relpath(entry.path, root)] = (info.st_size, info.st_mtime_ns)
                        if len(out) > MAX_FILES:
                            return None
        except OSError:
            continue
    return out


def changed(before: Snapshot | None, after: Snapshot | None) -> list[str] | None:
    if before is None or after is None:
        return None
    return sorted(path for path in {*before, *after} if before.get(path) != after.get(path))


def _git(root: str, *args: str, timeout: float = 30.0) -> subprocess.CompletedProcess | None:
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": os.environ.get("HOME", ""), "LC_ALL": "C", "GIT_OPTIONAL_LOCKS": "0", "GIT_TERMINAL_PROMPT": "0"}
    try:
        return subprocess.run(["git", "-C", root, *args], capture_output=True, text=True, timeout=timeout, env=env, check=False)
    except (OSError, subprocess.SubprocessError):
        return None


def is_git_repo(root: str) -> bool:
    done = _git(root, "rev-parse", "--is-inside-work-tree")
    return bool(done and done.returncode == 0 and done.stdout.strip() == "true")


def diff_stat(root: str, paths: list[str]) -> str:
    """``git diff --stat`` for the tracked ones among ``paths`` plus a count of new files. ``""`` outside git."""
    if not paths or not is_git_repo(root):
        return f"{len(paths)} file(s) changed" if paths else ""
    done = _git(root, "diff", "--stat", "HEAD", "--", *paths[:500])
    stat = done.stdout.strip().splitlines()[-1].strip() if done and done.returncode == 0 and done.stdout.strip() else ""
    tracked = _git(root, "ls-files", "--", *paths[:500])
    known = set(tracked.stdout.split()) if tracked and tracked.returncode == 0 else set()
    new = [p for p in paths if p not in known]
    if new:
        stat = (stat + ", " if stat else "") + f"{len(new)} new file(s)"
    return stat

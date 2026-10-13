"""Every project this machine's harness has worked in, for the native app's rail (docs/app.md §6.3, ``GET /projects``),
and one project's conversations (``GET /history``).

A project is a folder, by realpath — which is how trust, checkpoints, the bandit and learned skills are keyed today. A
git worktree is a project of its own (its trust and its checkpoints are its own: a folder trusted is not its siblings),
and says which repository and branch it belongs to, so the app can group worktrees under their repository.

Sources, unioned: the ``sessions`` table (every conversation recorded since B0), the hub's conversations, the folders
orchestrator runs worked in, and ``trustedProjects``. Git is asked once per folder per ``GIT_TTL`` seconds.
"""

from __future__ import annotations

import os
import subprocess
import time
from pathlib import Path
from typing import Any

GIT_TTL = 30.0
_git_cache: dict[str, tuple[float, dict[str, Any] | None]] = {}


def _git(root: str, *args: str) -> str | None:
    try:
        done = subprocess.run(["git", "-c", "core.fsmonitor=false", "-C", root, *args], capture_output=True, text=True, timeout=5)
    except (OSError, subprocess.TimeoutExpired):
        return None
    return done.stdout.strip() if done.returncode == 0 else None


def git_info(root: str) -> dict[str, Any] | None:
    """``{commonDir, mainRoot, branch, isWorktree, worktrees: [{path, branch}]}``, or None outside a repository."""
    now = time.time()
    cached = _git_cache.get(root)
    if cached is not None and now - cached[0] < GIT_TTL:
        return cached[1]
    info: dict[str, Any] | None = None
    out = _git(root, "rev-parse", "--show-toplevel", "--git-common-dir", "--abbrev-ref", "HEAD")
    if out:
        lines = out.splitlines()
        if len(lines) >= 3:
            top, common, branch = lines[0], lines[1], lines[2]
            common_abs = os.path.realpath(common if os.path.isabs(common) else os.path.join(top, common))
            main_root = os.path.dirname(common_abs) if os.path.basename(common_abs) == ".git" else common_abs
            worktrees: list[dict[str, Any]] = []
            listing = _git(root, "worktree", "list", "--porcelain") or ""
            current: dict[str, Any] = {}
            for line in listing.splitlines() + [""]:
                if not line:
                    if current.get("path"):
                        worktrees.append(current)
                    current = {}
                elif line.startswith("worktree "):
                    current["path"] = os.path.realpath(line[len("worktree ") :])
                elif line.startswith("branch "):
                    current["branch"] = line[len("branch ") :].removeprefix("refs/heads/")
                elif line == "detached":
                    current["branch"] = "(detached)"
            info = {
                "commonDir": common_abs,
                "mainRoot": os.path.realpath(main_root),
                "branch": branch if branch != "HEAD" else "(detached)",
                "isWorktree": os.path.realpath(top) != os.path.realpath(main_root),
                "worktrees": worktrees,
            }
    _git_cache[root] = (now, info)
    return info


def _trusted_roots() -> set[str]:
    """``trustedProjects`` from the user's settings file (the only file that may hold it)."""
    try:
        import json

        import settings_store

        data = json.loads(settings_store.settings_path("user", os.path.expanduser("~")).read_text(encoding="utf-8"))
    except Exception:
        return set()
    trusted = data.get("trustedProjects") if isinstance(data, dict) else None
    return {os.path.realpath(os.path.expanduser(item)) for item in trusted or [] if isinstance(item, str)}


def projects(*, hub_rows: list[dict[str, Any]] | None = None, live: list[dict[str, Any]] | None = None, limit: int = 60) -> list[dict[str, Any]]:
    """Projects newest-active first. ``hub_rows`` are the hub's conversations (``Chats.rows``), ``live`` the
    supervisor's live sessions."""
    import state_store

    roots: dict[str, dict[str, Any]] = {}

    def touch(path: str, when: float, *, conversations: int = 0, given: str | None = None) -> dict[str, Any] | None:
        if not path or not os.path.isabs(path):
            return None
        root = os.path.realpath(path)
        entry = roots.setdefault(root, {"root": root, "name": os.path.basename(root) or root, "lastActive": 0.0, "conversations": 0, "live": [], "pendingApprovals": 0, "sessionIds": set()})
        entry["lastActive"] = max(entry["lastActive"], float(when or 0.0))
        entry["conversations"] += conversations
        return entry

    session_rows = state_store.list_sessions(limit=5000)
    for row in session_rows:
        entry = touch(row.get("cwd_real") or "", row.get("updated_ts") or 0.0, conversations=1)
        if entry is not None:
            entry["sessionIds"].add(row["session_id"])
    for row in hub_rows or []:
        touch(str(row.get("cwd") or ""), float(row.get("lastUsed") or row.get("last_used") or 0.0), conversations=1)
    try:
        for row in state_store.connect().execute("SELECT cwd, MAX(updated_ts) AS updated FROM orch_runs GROUP BY cwd").fetchall():
            touch(row["cwd"], row["updated"] or 0.0)
    except Exception:
        pass
    trusted = _trusted_roots()
    for root in trusted:
        touch(root, 0.0)
    for session in live or []:
        entry = roots.get(os.path.realpath(str(session.get("cwd") or ""))) if session.get("cwd") else None
        if entry is not None:
            entry["live"].append(session.get("sessionId"))
    pending = state_store.pending_interventions()
    for item in pending:
        for entry in roots.values():
            if item.get("session_id") in entry["sessionIds"]:
                entry["pendingApprovals"] += 1

    out = []
    for entry in sorted(roots.values(), key=lambda e: e["lastActive"], reverse=True)[:limit]:
        root = entry["root"]
        exists = os.path.isdir(root)
        out.append({
            "root": root,
            "name": entry["name"],
            "exists": exists,
            "trusted": root in trusted,
            "git": git_info(root) if exists else None,
            "live": [sid for sid in entry["live"] if sid],
            "conversations": entry["conversations"],
            "lastActive": entry["lastActive"],
            "pendingApprovals": entry["pendingApprovals"],
        })
    return out


def history(cwd: str, *, limit: int = 50, cursor: float | None = None, sessions_dir: str | Path | None = None) -> dict[str, Any]:
    """One project's conversations, newest first, ``limit`` at a time; ``nextCursor`` is the last row's ``updated``."""
    import state_store

    rows = state_store.list_sessions(cwd=cwd, limit=max(1, min(int(limit), 200)), before=cursor)
    items = []
    for row in rows:
        has = None
        if sessions_dir is not None:
            has = (Path(sessions_dir) / "session" / row["session_id"] / "scopes" / "agent" / "default" / "snapshots" / "snapshot_latest.json").is_file()
        items.append({
            "sessionId": row["session_id"],
            "title": row.get("title"),
            "mode": row.get("mode"),
            "origin": row.get("origin"),
            "created": row.get("created_ts"),
            "updated": row.get("updated_ts"),
            "turns": row.get("turns"),
            "live": bool(row.get("host_pid")),
            "saved": has,
        })
    return {"cwd": os.path.realpath(cwd), "sessions": items, "nextCursor": rows[-1]["updated_ts"] if len(rows) == max(1, min(int(limit), 200)) else None}

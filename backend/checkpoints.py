"""Checkpoints: what makes ``auto`` mode reversible.

Before any tool call that may change files, the harness takes a snapshot of the project into a
**shadow git repository** that lives under ``~/.cordata/checkpoints/`` — never inside the project, so
it cannot touch the project's own ``.git`` (no commits, branches, index or hooks of yours are involved),
and it works the same in a folder that is not a git repository at all. Each snapshot is one commit
whose message carries who asked for it: session, turn, agent, tool, a redacted one-line summary.

    take(...)            one snapshot, before a mutating call (kind "pre"), or after a turn ("end")
    turns(project)       the turns that have checkpoints, newest first, each with the files it changed
    undo_turn(project)   put the project back as it was before a turn's first change
    restore(project, c)  put it back as it was at checkpoint ``c`` (optionally only some paths)
    changes_since(p, c)  what differs now from checkpoint ``c``;  diff_since(p, c): the unified diff of it

Restoring is itself undoable: it snapshots the present first ("restore-from") and says so.

What is and is not covered — written down because "auto mode is reversible" is only a promise worth making
if its edges are known:

* **Covered**: every file in the project that is not git-ignored, not a dependency/build directory, not a
  secrets file, not over ``max_file_mb``. Created files are removed on restore, deleted ones recreated.
* **Not covered**: ignored paths (``node_modules``, ``.venv``, ``target``, anything in ``.gitignore``);
  ``.env*`` / keys / ``.npmrc`` (deliberately never copied); files over the size limit; the insides of a
  nested git repository; anything outside the project directory (a call that writes there is reported as
  ``outside`` so the caller can say so); side effects that are not files (a database, a network call).
* A project over 50,000 files, your home directory, and ``/`` are not checkpointed, and say why.

Stdlib only, and never raises: a failed snapshot is a ``Result(ok=False, reason=...)`` the caller reports
as *unprotected* — it is never silently treated as protected.
"""

from __future__ import annotations

import hashlib
import json
import logging
import os
import re
import shutil
import subprocess
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable

logger = logging.getLogger(__name__)

SCHEMA = 1
DEFAULT_MAX_FILE_MB = 8.0
MAX_PROJECT_FILES = 50_000
MAX_LIST = 200
KEEP_COMMITS = 400
_FIRST_TIMEOUT = 90.0
_TIMEOUT = 25.0
_EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
_EXCLUDE_VERSION = "# cordata checkpoint excludes v2"

#: Directories that are rebuilt or vendored — the same set outcomes.py ignores.
DEPENDENCY_DIRS = frozenset({".git", "node_modules", "target", ".venv", "venv", "__pycache__", ".next", "dist", "build", ".mypy_cache", ".pytest_cache", ".ruff_cache", ".tox", ".cache", "coverage", ".turbo", ".gradle", ".idea"})

_EXCLUDES = f"""{_EXCLUDE_VERSION}
.git/
.cordata/
.DS_Store
node_modules/
target/
.venv/
venv/
__pycache__/
*.pyc
.next/
dist/
build/
.mypy_cache/
.pytest_cache/
.ruff_cache/
.tox/
.cache/
.turbo/
.gradle/
.idea/
*.lance/
# Secrets are never copied into a snapshot.
.env
.env.*
!.env.example
!.env.sample
!.env.template
!.env.dist
id_rsa*
id_ed25519*
*.pem
*.p12
*.pfx
*.key
*.keychain
.npmrc
.netrc
.pypirc
"""

_READ_ONLY_COMMANDS = frozenset(
    "ls cat head tail grep egrep fgrep rg ag wc pwd which type file stat du df tree sort uniq cut tr diff cmp comm date whoami uname hostname id env printenv basename dirname "
    "realpath readlink true false test [ sleep jq less more nl od hexdump xxd md5 md5sum shasum sha1sum sha256sum cd echo printf pushd popd ps lsof nproc tput clear seq column fold rev expr".split()
)
_READ_ONLY_GIT = frozenset("status diff log show rev-parse remote ls-files ls-tree blame describe shortlog reflog cat-file grep whatchanged rev-list name-rev tag".split())
_SEPARATORS = re.compile(r"&&|\|\||[;|\n]")
_WRAPPERS = re.compile(r"^(?:[A-Za-z_][A-Za-z0-9_]*=\S*\s+|(?:time|nohup|exec|command)\s+)+")
_EDITOR_READ = frozenset({"view"})
_LOCKS: dict[str, threading.RLock] = {}
_LOCKS_GUARD = threading.Lock()
_VIABLE: dict[str, str | None] = {}
_NEVER = frozenset({"/", "/Users", "/home", "/tmp", "/private", "/private/tmp", "/var", "/private/var", "/etc", "/usr", "/opt", "/bin", "/sbin", "/System", "/Library", "/Applications", "/Volumes"})


# ------------------------------------------------------------------------------ what writes files


def _segment_reads_only(segment: str) -> bool:
    segment = _WRAPPERS.sub("", segment.strip())
    if not segment:
        return True
    if ">" in segment or "$(" in segment or "`" in segment or "<<" in segment:
        return False
    parts = segment.split()
    first = os.path.basename(parts[0])
    rest = parts[1:]
    if first == "git":
        sub = next((p for p in rest if not p.startswith("-")), "")
        if sub == "branch":
            return not any(p in ("-d", "-D", "-m", "-M", "-c", "-C", "--delete") for p in rest)
        if sub == "tag":
            return not any(not p.startswith("-") for p in rest[rest.index("tag") + 1 :]) if "tag" in rest else True
        return sub in _READ_ONLY_GIT
    if first == "find":
        return not any(p in ("-delete", "-exec", "-execdir", "-ok", "-fprint", "-fprintf", "-fls") for p in rest)
    if first == "sed":
        return not any(p.startswith("-i") or p == "--in-place" for p in rest)
    if first == "sort":
        return not any(p == "-o" or p.startswith("--output") for p in rest)
    return first in _READ_ONLY_COMMANDS


def shell_writes(command: str) -> bool:
    """Might this shell command change files? Conservative: only a command whose every part is on a short
    read-only list (or is a test/build check) is judged not to — anything unrecognised may write."""
    if not command or not command.strip():
        return False
    try:
        import outcomes

        verifier = outcomes.is_verifier
    except Exception:
        verifier = lambda _c: False  # noqa: E731
    for segment in _SEPARATORS.split(command):
        if not segment.strip():
            continue
        if _segment_reads_only(segment) or verifier(segment):
            continue
        return True
    return False


def writes_files(tool: str, tool_input: Any, extra_tools: Iterable[str] = ()) -> bool:
    """Does this call possibly change files in the project? True for the edit tools, for Python and the Claude
    Code CLI, for any generated tool, and for a shell command that is not recognisably read-only."""
    data = tool_input if isinstance(tool_input, dict) else {}
    if tool == "background_shell":
        tool = "shell"
    if tool == "editor":
        return str(data.get("command") or "") not in _EDITOR_READ
    if tool in ("file_write", "apply_patch", "python_repl", "claude_code_engineer"):
        return True
    if tool == "shell":
        command = data.get("command")
        if isinstance(command, list):
            command = " ".join(str(c) for c in command)
        return shell_writes(str(command or ""))
    return tool in set(extra_tools)


# ------------------------------------------------------------------------------ the shadow repository


def _home() -> Path:
    return Path(os.environ.get("CORDATA_HOME") or Path.home() / ".cordata")


def _real(project: str | os.PathLike) -> str:
    try:
        return os.path.realpath(str(project))
    except OSError:
        return str(project)


def _key(project: str) -> str:
    return hashlib.sha1(project.encode("utf-8")).hexdigest()[:16]


def shadow_dir(project: str | os.PathLike) -> Path:
    """Where this project's snapshots live (the shadow repository's ``git`` directory is inside it)."""
    return _home() / "checkpoints" / _key(_real(project))


def _lock_for(project: str) -> threading.RLock:
    with _LOCKS_GUARD:
        return _LOCKS.setdefault(project, threading.RLock())


@dataclass
class Result:
    ok: bool
    id: str | None = None
    reason: str | None = None
    #: Paths the call is about to write that are outside the project: a checkpoint cannot cover them.
    outside: list[str] = field(default_factory=list)
    ms: int = 0
    created: bool = True

    def to_json(self) -> dict[str, Any]:
        return {"ok": self.ok, "id": self.id, "reason": self.reason, "outside": self.outside, "ms": self.ms}


class _Git:
    """``git`` bound to one project's shadow repository."""

    def __init__(self, project: str) -> None:
        self.project = project
        self.dir = shadow_dir(project) / "git"
        self.env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin"),
            "HOME": os.environ.get("HOME", str(Path.home())),
            "LC_ALL": "C",
            "GIT_DIR": str(self.dir),
            "GIT_WORK_TREE": project,
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_TERMINAL_PROMPT": "0",
            "GIT_OPTIONAL_LOCKS": "0",
            "GIT_AUTHOR_NAME": "cordata",
            "GIT_AUTHOR_EMAIL": "checkpoints@cordata.local",
            "GIT_COMMITTER_NAME": "cordata",
            "GIT_COMMITTER_EMAIL": "checkpoints@cordata.local",
        }

    def run(self, *args: str, input: bytes | None = None, timeout: float = _TIMEOUT, check: bool = True) -> subprocess.CompletedProcess:
        # Literal pathspecs: a file called ``*.txt`` or ``[a]`` is that file, not a glob.
        run = subprocess.run(["git", "--literal-pathspecs", *args], input=input, capture_output=True, env=self.env, cwd=self.project, timeout=timeout)
        if check and run.returncode != 0:
            raise RuntimeError(f"git {args[0]}: {run.stderr.decode('utf-8', 'replace').strip()[:300] or run.returncode}")
        return run

    def out(self, *args: str, **kw: Any) -> str:
        return self.run(*args, **kw).stdout.decode("utf-8", "replace")

    def has_head(self) -> bool:
        return self.run("rev-parse", "--verify", "-q", "HEAD", check=False).returncode == 0


def _ensure(git: _Git) -> None:
    """Create the shadow repository on first use; refresh its exclude list when this module's changes."""
    root = git.dir.parent
    root.mkdir(parents=True, exist_ok=True)
    try:
        os.chmod(root, 0o700)
        os.chmod(root.parent, 0o700)
    except OSError:
        pass
    if not (git.dir / "HEAD").exists():
        git.run("init", "-q")
        git.run("symbolic-ref", "HEAD", "refs/heads/main")
        for key, value in (
            ("core.autocrlf", "false"),
            ("core.safecrlf", "false"),
            ("core.quotepath", "false"),
            ("core.fsmonitor", "false"),
            ("core.untrackedCache", "true"),
            ("core.preloadindex", "true"),
            ("core.hooksPath", os.devnull),
            ("commit.gpgsign", "false"),
            ("gc.auto", "0"),
            ("advice.addEmbeddedRepo", "false"),
            ("core.excludesFile", str(git.dir / "big-files")),
        ):
            git.run("config", key, value)
        (root / "project.txt").write_text(git.project + "\n")
    info = git.dir / "info"
    info.mkdir(exist_ok=True)
    exclude = info / "exclude"
    if not exclude.exists() or not exclude.read_text().startswith(_EXCLUDE_VERSION):
        exclude.write_text(_EXCLUDES)


def _unviable(project: str) -> str | None:
    """Why this directory should not be checkpointed, or None. Cached per process: a walk is not free."""
    if project in _VIABLE:
        return _VIABLE[project]
    reason: str | None = None
    home = _real(Path.home())
    if not os.path.isdir(project):
        reason = "the project directory does not exist"
    elif project in _NEVER or project == home:
        reason = "refusing to checkpoint your home directory or a system directory"
    elif shutil.which("git") is None:
        reason = "git is not installed"
    else:
        count = 0
        stack = [project]
        while stack and reason is None:
            try:
                with os.scandir(stack.pop()) as entries:
                    for entry in entries:
                        if entry.is_dir(follow_symlinks=False):
                            if entry.name not in DEPENDENCY_DIRS:
                                stack.append(entry.path)
                        else:
                            count += 1
                            if count > MAX_PROJECT_FILES:
                                reason = f"the project has more than {MAX_PROJECT_FILES:,} files"
                                break
            except OSError:
                continue
    _VIABLE[project] = reason
    return reason


def _escape_glob(path: str) -> str:
    return re.sub(r"([\\*?\[\]!#])", r"\\\1", path)


def _add_all(git: _Git, max_file_bytes: int, first: bool) -> None:
    """Stage the working tree, then drop any *new* file over the size limit from the snapshot (and remember it
    so it is skipped next time) — a dataset someone forgot to ignore must not be copied on every call."""
    git.run("add", "-A", "--ignore-errors", timeout=_FIRST_TIMEOUT if first else _TIMEOUT, check=False)
    base = "HEAD" if git.has_head() else _EMPTY_TREE
    raw = git.out("diff", "--cached", "--raw", "-z", "--no-renames", "--abbrev=40", "--diff-filter=A", base).split("\0")
    added: list[tuple[str, str]] = []
    for i in range(0, len(raw) - 1, 2):
        meta, path = raw[i], raw[i + 1]
        fields = meta.lstrip(":").split()
        if len(fields) >= 4 and set(fields[3]) != {"0"}:
            added.append((fields[3], path))
    if not added:
        return
    sizes = git.out("cat-file", "--batch-check=%(objectname) %(objectsize)", input=("\n".join(sha for sha, _ in added) + "\n").encode()).splitlines()
    big = [path for (sha, path), line in zip(added, sizes) if line.split()[-1].isdigit() and int(line.split()[-1]) > max_file_bytes]
    if not big:
        return
    git.run("rm", "-q", "--cached", "--ignore-unmatch", "--pathspec-from-file=-", "--pathspec-file-nul", input=("\0".join(big) + "\0").encode(), check=False)
    with (git.dir / "big-files").open("a") as handle:
        for path in big:
            handle.write("/" + _escape_glob(path) + "\n")


def _meta(kind: str, tool: str, agent: str, session_id: str, turn: str, call: str, paths: list[str], extra: dict[str, Any] | None = None) -> str:
    data = {"v": SCHEMA, "kind": kind, "tool": tool, "agent": agent, "session": session_id, "turn": turn, "call": call[:240], "paths": paths[:20], **(extra or {})}
    return f"{kind} {tool or '-'}\n\n{json.dumps(data, separators=(',', ':'))}\n"


def _parse_meta(body: str) -> dict[str, Any]:
    for line in reversed(body.strip().splitlines()):
        if line.startswith("{"):
            try:
                data = json.loads(line)
                return data if isinstance(data, dict) else {}
            except ValueError:
                return {}
    return {}


def _inside(project: str, path: str) -> bool:
    full = path if os.path.isabs(path) else os.path.join(project, path)
    full = os.path.normpath(full)
    return full == project or full.startswith(project.rstrip(os.sep) + os.sep)


# ------------------------------------------------------------------------------ taking one


def take(
    project: str | os.PathLike,
    *,
    kind: str = "pre",
    tool: str = "",
    agent: str = "",
    session_id: str = "",
    turn: str = "",
    call: str = "",
    paths: Iterable[str] = (),
    max_file_mb: float = DEFAULT_MAX_FILE_MB,
    extra: dict[str, Any] | None = None,
) -> Result:
    """Snapshot the project. ``kind`` is ``pre`` (before a mutating call), ``end`` (after a turn) or
    ``restore``/``restore-from``. Always returns; ``ok=False`` carries the reason and means *not protected*."""
    started = time.monotonic()
    root = _real(project)
    wanted = [str(p) for p in paths]
    outside = [p for p in wanted if not _inside(root, p)]
    reason = _unviable(root)
    if reason is not None:
        return Result(False, reason=reason, outside=outside)
    try:
        with _lock_for(root):
            git = _Git(root)
            first = not (git.dir / "HEAD").exists()
            _ensure(git)
            _add_all(git, int(max_file_mb * 1024 * 1024), first)
            relative = [os.path.relpath(os.path.join(root, p) if not os.path.isabs(p) else p, root) for p in wanted if p not in outside]
            git.run("commit", "-q", "--allow-empty", "--no-verify", "--no-gpg-sign", "-m", _meta(kind, tool, agent, session_id, turn, call, relative, extra))
            sha = git.out("rev-parse", "HEAD").strip()
        return Result(True, id=sha, outside=outside, ms=int((time.monotonic() - started) * 1000))
    except subprocess.TimeoutExpired:
        _clear_stale_lock(root)
        return Result(False, reason="the snapshot timed out (a very large project?)", outside=outside, ms=int((time.monotonic() - started) * 1000))
    except Exception as error:  # a snapshot must never break the turn
        logger.debug("checkpoint failed", exc_info=True)
        return Result(False, reason=str(error)[:300] or error.__class__.__name__, outside=outside, ms=int((time.monotonic() - started) * 1000))


def _clear_stale_lock(project: str) -> None:
    lock = _Git(project).dir / "index.lock"
    try:
        if lock.exists() and time.time() - lock.stat().st_mtime > 5:
            lock.unlink()
    except OSError:
        pass


# ------------------------------------------------------------------------------ reading history


def _log(git: _Git, limit: int) -> list[dict[str, Any]]:
    if not git.has_head():
        return []
    raw = git.out("log", f"-n{int(limit)}", "--format=%H%x1f%ct%x1f%B%x1e")
    rows = []
    for chunk in raw.split("\x1e"):
        chunk = chunk.strip("\n")
        if not chunk.strip():
            continue
        sha, _, rest = chunk.partition("\x1f")
        stamp, _, body = rest.partition("\x1f")
        meta = _parse_meta(body)
        rows.append({"id": sha.strip(), "ts": float(stamp or 0), **meta})
    return rows


def _name_status(git: _Git, old: str, new: str | None = None, *, cached: bool = False) -> list[dict[str, str]]:
    args = ["diff", "--name-status", "-z", "--no-renames"]
    if cached:
        args.append("--cached")
    args.append(old)
    if new:
        args.append(new)
    parts = git.out(*args).split("\0")
    out = []
    for i in range(0, len(parts) - 1, 2):
        status, path = parts[i], parts[i + 1]
        if status and path:
            out.append({"path": path, "change": {"A": "added", "D": "deleted", "M": "modified", "T": "modified"}.get(status[0], status[0])})
    return out


def turns(project: str | os.PathLike, limit: int = 8) -> list[dict[str, Any]]:
    """The turns that have checkpoints, newest first: when, what was asked of which tools, the first
    checkpoint (the project as it was before the turn changed anything), the end snapshot if the turn
    finished, the files it changed, and whether it has since been undone."""
    root = _real(project)
    if not (shadow_dir(root) / "git" / "HEAD").exists():
        return []
    try:
        with _lock_for(root):
            git = _Git(root)
            rows = _log(git, 600)
            by_turn: dict[str, dict[str, Any]] = {}
            undone: set[str] = set()
            for row in rows:  # newest first
                if row.get("kind") == "restore" and row.get("undid"):
                    undone.add(str(row["undid"]))
                turn = str(row.get("turn") or "")
                if not turn or row.get("kind") not in ("pre", "end"):
                    continue
                entry = by_turn.setdefault(turn, {"turn": turn, "session": row.get("session"), "first": None, "end": None, "calls": 0, "tools": [], "agents": [], "ts": row["ts"]})
                if row["kind"] == "pre":
                    entry["first"] = row["id"]  # overwritten as we walk back: ends at the oldest
                    entry["calls"] += 1
                    entry["started"] = row["ts"]
                    for key, field_name in (("tools", "tool"), ("agents", "agent")):
                        value = row.get(field_name)
                        if value and value not in entry[key]:
                            entry[key].append(value)
                    entry["last_call"] = entry.get("last_call") or row.get("call")
                elif entry["end"] is None:
                    entry["end"] = row["id"]
            result = []
            for entry in list(by_turn.values())[: max(1, limit)]:
                entry["undone"] = entry["turn"] in undone
                entry["files"] = None
                if entry["first"] and entry["end"]:
                    try:
                        changes = _name_status(git, entry["first"], entry["end"])
                        entry["files"] = changes[:MAX_LIST]
                        entry["file_count"] = len(changes)
                    except Exception:
                        pass
                result.append(entry)
            return result
    except Exception:
        logger.debug("checkpoints: could not read history", exc_info=True)
        return []


def list_checkpoints(project: str | os.PathLike, limit: int = 40) -> list[dict[str, Any]]:
    root = _real(project)
    if not (shadow_dir(root) / "git" / "HEAD").exists():
        return []
    try:
        with _lock_for(root):
            return _log(_Git(root), min(max(1, limit), MAX_LIST))
    except Exception:
        return []


# ------------------------------------------------------------------------------ restoring


_HEX = re.compile(r"^[0-9a-f]{7,40}$")


def _resolve(git: _Git, ref: str) -> str:
    ref = str(ref or "").strip().lower()
    if not _HEX.match(ref):
        raise ValueError("a checkpoint id is 7-40 hex characters")
    run = git.run("rev-parse", "--verify", "-q", f"{ref}^{{commit}}", check=False)
    if run.returncode != 0:
        raise ValueError(f"no checkpoint {ref}")
    return run.stdout.decode().strip()


def _safe_paths(project: str, paths: Iterable[str] | None) -> list[str] | None:
    if not paths:
        return None
    out = []
    for path in paths:
        full = os.path.normpath(os.path.join(project, str(path)))
        if not _inside(project, full) or full == project:
            raise ValueError(f"{path!r} is outside the project")
        out.append(os.path.relpath(full, project))
    return out


def restore(
    project: str | os.PathLike,
    target: str,
    *,
    paths: Iterable[str] | None = None,
    dry_run: bool = False,
    session_id: str = "",
    undid_turn: str = "",
    edited_after: str | None = None,
    max_file_mb: float = DEFAULT_MAX_FILE_MB,
) -> dict[str, Any]:
    """Put the project (or just ``paths``) back as it was at checkpoint ``target``. ``dry_run`` only reports.
    Otherwise the present is snapshotted first (``undo_with``), so a restore can itself be undone.

    ``edited_after`` is a checkpoint id: files that differ now from *that* snapshot — somebody changed
    them since — are listed in ``edited_since`` so the caller can warn before they are overwritten."""
    root = _real(project)
    reason = _unviable(root)
    if reason is not None:
        return {"ok": False, "reason": reason}
    try:
        with _lock_for(root):
            git = _Git(root)
            _ensure(git)
            sha = _resolve(git, target)
            limit = _safe_paths(root, paths)
            _add_all(git, int(max_file_mb * 1024 * 1024), False)
            changes = _name_status(git, sha, cached=True) if not limit else _scoped_changes(git, sha, limit)
            edited: list[str] = []
            if edited_after:
                after = _resolve(git, edited_after)
                touched = {c["path"] for c in changes}
                edited = sorted(c["path"] for c in _name_status(git, after, cached=True) if c["path"] in touched)
            shown = [{"path": c["path"], "action": {"added": "delete", "deleted": "recreate", "modified": "revert"}.get(c["change"], c["change"])} for c in changes]
            report = {"ok": True, "target": sha, "dry_run": dry_run, "files": shown[:MAX_LIST], "count": len(shown), "more": max(0, len(shown) - MAX_LIST), "edited_since": edited[:MAX_LIST]}
            if dry_run or not changes:
                return report
            probe = git.run("commit", "-q", "--allow-empty", "--no-verify", "--no-gpg-sign", "-m", _meta("restore-from", "restore", "", session_id, "", f"before restoring {sha[:12]}", [], {"target": sha}), check=False)
            undo_with = git.out("rev-parse", "HEAD").strip() if probe.returncode == 0 else None
            failed: list[str] = []
            write = [c["path"] for c in changes if c["change"] != "added"]
            for i in range(0, len(write), 100):
                run = git.run("checkout", sha, "--", *write[i : i + 100], check=False)
                if run.returncode != 0:
                    failed.extend(write[i : i + 100])
            for change in changes:
                if change["change"] != "added":
                    continue
                full = os.path.join(root, change["path"])
                try:
                    if _inside(root, full) and (os.path.lexists(full)):
                        os.remove(full)
                        _prune_empty_dirs(root, os.path.dirname(full))
                except OSError:
                    failed.append(change["path"])
            _add_all(git, int(max_file_mb * 1024 * 1024), False)
            git.run("commit", "-q", "--allow-empty", "--no-verify", "--no-gpg-sign", "-m", _meta("restore", "restore", "", session_id, "", f"restored {sha[:12]}", [c["path"] for c in changes], {"target": sha, **({"undid": undid_turn} if undid_turn else {})}), check=False)
            report.update({"dry_run": False, "undo_with": undo_with, "failed": failed[:MAX_LIST]})
            report["ok"] = not failed
            return report
    except subprocess.TimeoutExpired:
        _clear_stale_lock(root)
        return {"ok": False, "reason": "the restore timed out"}
    except ValueError as error:
        return {"ok": False, "reason": str(error)}
    except Exception as error:
        logger.debug("checkpoint restore failed", exc_info=True)
        return {"ok": False, "reason": str(error)[:300] or error.__class__.__name__}


def _scoped_changes(git: _Git, sha: str, paths: list[str]) -> list[dict[str, str]]:
    parts = git.out("diff", "--name-status", "-z", "--no-renames", "--cached", sha, "--", *paths).split("\0")
    out = []
    for i in range(0, len(parts) - 1, 2):
        status, path = parts[i], parts[i + 1]
        if status and path:
            out.append({"path": path, "change": {"A": "added", "D": "deleted", "M": "modified", "T": "modified"}.get(status[0], status[0])})
    return out


def changes_since(project: str | os.PathLike, target: str, paths: Iterable[str] | None = None) -> list[dict[str, str]] | None:
    """What differs now from checkpoint ``target`` (``[{"path", "change"}]``, change ``added``/``deleted``/``modified``),
    optionally only under ``paths``. Snapshots the present into the index only — no commit. ``None`` when it cannot
    tell (no shadow repository, an unknown checkpoint, a timeout); never an empty list in that case."""
    root = _real(project)
    if _unviable(root) is not None or not (shadow_dir(root) / "git" / "HEAD").exists():
        return None
    try:
        with _lock_for(root):
            git = _Git(root)
            sha = _resolve(git, target)
            limit = _safe_paths(root, paths)
            _add_all(git, int(DEFAULT_MAX_FILE_MB * 1024 * 1024), False)
            return _name_status(git, sha, cached=True) if not limit else _scoped_changes(git, sha, limit)
    except subprocess.TimeoutExpired:
        _clear_stale_lock(root)
        return None
    except Exception:
        logger.debug("changes_since failed", exc_info=True)
        return None


def diff_since(project: str | os.PathLike, target: str, paths: Iterable[str] | None = None, *, max_chars: int = 200_000) -> tuple[str, str]:
    """``(unified diff, diffstat line)`` of the project now against checkpoint ``target``, optionally only ``paths``.
    ``("", "")`` when there is nothing to show or it cannot be computed. Secrets files are never in a checkpoint, so
    never in this diff."""
    root = _real(project)
    if _unviable(root) is not None or not (shadow_dir(root) / "git" / "HEAD").exists():
        return "", ""
    try:
        with _lock_for(root):
            git = _Git(root)
            sha = _resolve(git, target)
            limit = _safe_paths(root, paths) or []
            _add_all(git, int(DEFAULT_MAX_FILE_MB * 1024 * 1024), False)
            patch = git.out("diff", "--cached", "--no-color", "--no-renames", sha, "--", *limit, timeout=60.0)
            stat = git.out("diff", "--cached", "--shortstat", sha, "--", *limit).strip()
    except subprocess.TimeoutExpired:
        _clear_stale_lock(root)
        return "", ""
    except Exception:
        logger.debug("diff_since failed", exc_info=True)
        return "", ""
    if len(patch) > max_chars:
        patch = patch[:max_chars] + "\n(diff truncated)\n"
    return patch, stat


def _prune_empty_dirs(root: str, directory: str) -> None:
    while directory and directory != root and _inside(root, directory):
        try:
            os.rmdir(directory)
        except OSError:
            return
        directory = os.path.dirname(directory)


def undo_turn(project: str | os.PathLike, turn: str | None = None, *, dry_run: bool = False, paths: Iterable[str] | None = None, session_id: str = "") -> dict[str, Any]:
    """Undo a turn (the latest one that has not been undone, unless ``turn`` names one): the project goes
    back to how it was before the turn's first change."""
    rows = [t for t in turns(project, limit=30) if t.get("first")]
    if turn:
        chosen = next((t for t in rows if t["turn"] == turn), None)
    else:
        chosen = next((t for t in rows if not t["undone"]), None)
    if chosen is None:
        return {"ok": False, "reason": "no turn to undo" if not turn else f"no checkpoints for turn {turn}"}
    result = restore(project, chosen["first"], paths=paths, dry_run=dry_run, session_id=session_id, undid_turn="" if paths else chosen["turn"], edited_after=chosen.get("end"))
    result["turn"] = chosen["turn"]
    result["calls"] = chosen["calls"]
    return result


# ------------------------------------------------------------------------------ keeping it small


def prune(project: str | os.PathLike, keep: int = KEEP_COMMITS) -> bool:
    """Bound the history: past ``2 * keep`` commits, rebuild the chain from the newest ``keep`` (same trees and
    messages, the oldest becoming the root), then collect garbage. Returns whether it rewrote anything."""
    root = _real(project)
    if not (shadow_dir(root) / "git" / "HEAD").exists():
        return False
    try:
        with _lock_for(root):
            git = _Git(root)
            if not git.has_head():
                return False
            total = int(git.out("rev-list", "--count", "HEAD").strip() or 0)
            rewrote = False
            if total > keep * 2:
                commits = git.out("rev-list", f"-n{keep}", "--reverse", "HEAD").split()
                parent: str | None = None
                for commit in commits:
                    tree = git.out("rev-parse", f"{commit}^{{tree}}").strip()
                    message = git.out("log", "-1", "--format=%B", commit)
                    date = git.out("log", "-1", "--format=%cI", commit).strip()
                    env = {**git.env, "GIT_AUTHOR_DATE": date, "GIT_COMMITTER_DATE": date}
                    args = ["git", "commit-tree", tree, *(["-p", parent] if parent else []), "-m", message.rstrip("\n")]
                    run = subprocess.run(args, capture_output=True, env=env, cwd=root, timeout=_TIMEOUT)
                    if run.returncode != 0:
                        return False
                    parent = run.stdout.decode().strip()
                git.run("update-ref", "refs/heads/main", parent or "HEAD")
                git.run("reflog", "expire", "--expire=now", "--all", check=False)
                rewrote = True
            loose = git.out("count-objects").split()
            if rewrote or (loose and loose[0].isdigit() and int(loose[0]) > 3000):
                git.run("gc", "-q", *(["--prune=now"] if rewrote else []), timeout=120, check=False)
            return rewrote
    except Exception:
        logger.debug("checkpoints: prune failed", exc_info=True)
        return False


def main(argv: list[str] | None = None) -> int:
    import argparse

    parser = argparse.ArgumentParser(prog="checkpoints", description="List, inspect and undo the harness's project snapshots.")
    parser.add_argument("--project", default=os.getcwd())
    sub = parser.add_subparsers(dest="cmd", required=True)
    sub.add_parser("turns")
    sub.add_parser("list")
    undo = sub.add_parser("undo")
    undo.add_argument("--turn")
    undo.add_argument("--apply", action="store_true", help="actually restore (default: show what would change)")
    rest = sub.add_parser("restore")
    rest.add_argument("id")
    rest.add_argument("--apply", action="store_true")
    args = parser.parse_args(argv)
    if args.cmd == "turns":
        print(json.dumps(turns(args.project), indent=2))
    elif args.cmd == "list":
        print(json.dumps(list_checkpoints(args.project), indent=2))
    elif args.cmd == "undo":
        print(json.dumps(undo_turn(args.project, args.turn, dry_run=not args.apply), indent=2))
    else:
        print(json.dumps(restore(args.project, args.id, dry_run=not args.apply), indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

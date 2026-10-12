"""``code_search`` — find code by pattern without reading files into the conversation.

The way an agent used to answer "where is ``build_session`` used?" was to ``file_read`` the files it guessed at — a
2,800-line module costs ~30k tokens to learn that the answer is on three lines. This tool answers with the three
lines: ``path:line: text``, capped, sorted, nothing else.

Two engines, one output:

* **ripgrep** (``rg``) when it is on ``PATH`` — the fast path, and the one that respects ``.gitignore``,
  ``.ignore`` and hidden files exactly as a person's own ``rg`` would.
* **Python** otherwise — the file list comes from ``git ls-files --cached --others --exclude-standard`` in a git
  checkout (so ignored files are skipped the same way) or a directory walk that skips dependency and build
  directories; the pattern is a Python ``re`` (ripgrep's default dialect agrees on everything an agent writes:
  ``\\b``, classes, alternation, ``\\s``). Binary files and files over ``MAX_FILE_BYTES`` are skipped.

Properties that matter:

* **Read-only and confined.** A search runs inside the session's project; a ``path`` outside it is refused (reading
  elsewhere is ``file_read``'s job, with its own rules). Secrets files (``.env``, keys) are never searched, and what is
  returned still passes the session's secret-redaction hooks like any tool result.
* **Bounded.** ``max_results`` matches (default 60, at most 400), each line cut to ``LINE_CHARS``; the tool says when it
  stopped early and how to narrow the search, rather than returning a wall of text.
* **No approval.** It cannot change anything, so it never asks — and it runs in plan mode.

Pairs with the LSP tools (``lsp_definition`` / ``lsp_references`` for the *semantic* answer once the name is found)
and ``code_outline`` (``code_nav.py``) for reading one symbol of a large file.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path

from strands import tool

DEFAULT_RESULTS = 60
MAX_RESULTS = 400
LINE_CHARS = 240
MAX_FILE_BYTES = 2 * 1024 * 1024
TIMEOUT_S = 20.0

#: Directories a walk never enters (ripgrep skips them through .gitignore; a folder that is not a repository has none).
SKIP_DIRS = frozenset(
    {".git", ".hg", ".svn", "node_modules", "target", ".venv", "venv", "__pycache__", ".mypy_cache", ".pytest_cache", ".ruff_cache",
     "dist", "build", ".next", ".turbo", ".tox", ".cache", ".idea", "coverage"}
)
#: Never searched, by name: what a match would put in front of a model is a credential.
_SECRET_NAME = re.compile(r"(^\.env(\..*)?$|\.pem$|\.p12$|\.pfx$|\.key$|^id_(rsa|ed25519|ecdsa)|\.keychain|^\.netrc$|^\.npmrc$|^\.pypirc$)", re.IGNORECASE)
_SECRET_TEMPLATE = re.compile(r"^\.env\.(example|sample|template|dist)$", re.IGNORECASE)

_ROOT: str | None = None


def configure(root: str | None) -> None:
    """The project a session searches. Unset, the process's working directory (the ACP server chdirs into the
    project; the web front door into its workspace)."""
    global _ROOT
    _ROOT = root


def project_root() -> Path:
    return Path(_ROOT or os.getcwd()).resolve()


@dataclass(frozen=True)
class Match:
    path: str  # relative to the project
    line: int
    text: str
    context: bool = False  # a line shown around a match, not a match


@dataclass
class Found:
    engine: str
    matches: list[Match]
    files: dict[str, int]
    truncated: bool
    error: str | None = None


def is_secret_file(name: str) -> bool:
    base = os.path.basename(name)
    return bool(_SECRET_NAME.search(base)) and not _SECRET_TEMPLATE.search(base)


def ripgrep() -> str | None:
    """The ``rg`` binary, or None. ``CORDATA_RG=off`` forces the Python engine (tests)."""
    if os.environ.get("CORDATA_RG", "").strip().lower() in ("off", "0", "false", "no"):
        return None
    return shutil.which("rg")


def _clip(text: str) -> str:
    text = text.rstrip("\r\n").replace("\t", "    ")
    return text if len(text) <= LINE_CHARS else text[: LINE_CHARS - 1] + "…"


def _resolve(root: Path, path: str) -> Path | None:
    candidate = Path(os.path.expanduser(path or "."))
    candidate = (candidate if candidate.is_absolute() else root / candidate).resolve()
    if candidate != root and root not in candidate.parents:
        return None
    return candidate


# ------------------------------------------------------------------------------------------------- engines


def _rg(binary: str, root: Path, target: Path, pattern: str, *, glob: str, literal: bool, ignore_case: bool, files_only: bool, context: int, limit: int) -> Found:
    # `--null` puts a NUL after each path, so a path with dashes and digits in it cannot be misread as a line number.
    argv = [binary, "--no-heading", "--with-filename", "--line-number", "--null", "--color", "never", "--no-messages", "--max-columns", str(LINE_CHARS * 2)]
    argv += ["--fixed-strings"] if literal else []
    argv += ["--ignore-case"] if ignore_case else []
    argv += ["--files-with-matches"] if files_only else []
    argv += ["--context", str(context)] if context and not files_only else []
    for spec in [g.strip() for g in glob.split(",") if g.strip()]:
        argv += ["--glob", spec]
    # The secrets files are excluded however the pattern is written.
    argv += ["--glob", "!.env", "--glob", "!.env.*", "--glob", "!*.pem", "--glob", "!*.key", "--glob", "!id_*"]
    argv += ["--regexp", pattern, "--", os.path.relpath(target, root)]
    try:
        done = subprocess.run(argv, cwd=root, capture_output=True, text=True, timeout=TIMEOUT_S, errors="replace")
    except subprocess.TimeoutExpired:
        return Found("ripgrep", [], {}, True, f"the search took longer than {TIMEOUT_S:.0f}s — narrow it with path= or glob=")
    except OSError as error:
        return Found("ripgrep", [], {}, False, f"could not run ripgrep: {error}")
    if done.returncode == 2 and not done.stdout:
        return Found("ripgrep", [], {}, False, (done.stderr or "ripgrep failed").strip().splitlines()[-1][:300])
    found = Found("ripgrep", [], {}, False)
    line_re = re.compile(r"^(\d+)([:-])(.*)$")
    if files_only:
        for path in done.stdout.split("\0"):
            path = path.strip()
            if path and not is_secret_file(path):
                found.files[path.removeprefix("./")] = 1
        return found
    for raw in done.stdout.splitlines():
        if raw == "--" or "\0" not in raw:
            continue
        path, _, rest = raw.partition("\0")
        parsed = line_re.match(rest)
        if not parsed:
            continue
        number, sep, text = parsed.groups()
        if is_secret_file(path):
            continue
        path = path.removeprefix("./")
        is_match = sep == ":"
        if is_match:
            found.files[path] = found.files.get(path, 0) + 1
        found.matches.append(Match(path, int(number), _clip(text), context=not is_match))
    return found


def _candidate_files(root: Path, target: Path) -> list[Path]:
    if target.is_file():
        return [target]
    listed: list[Path] | None = None
    if (root / ".git").exists():
        try:
            done = subprocess.run(
                ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z", "--", os.path.relpath(target, root)],
                cwd=root, capture_output=True, timeout=TIMEOUT_S,
            )
            if done.returncode == 0:
                listed = [root / name for name in done.stdout.decode("utf-8", "replace").split("\0") if name]
        except (OSError, subprocess.TimeoutExpired):
            listed = None
    if listed is not None:
        return sorted(p for p in listed if p.is_file())
    out: list[Path] = []
    for directory, dirs, files in os.walk(target):
        dirs[:] = sorted(d for d in dirs if d not in SKIP_DIRS and not d.startswith("."))
        out.extend(Path(directory) / name for name in sorted(files))
    return out


def _glob_filter(globs: list[str]):
    if not globs:
        return lambda rel: True
    import fnmatch

    include = [g for g in globs if not g.startswith("!")]
    exclude = [g[1:] for g in globs if g.startswith("!")]

    def forms(glob: str) -> set[str]:
        """ripgrep's (gitignore) reading of a glob: `**/` also matches zero directories — `**/*.py` matches a root-level
        `app.py`, `src/**/*.py` matches `src/a.py` — which `fnmatch` alone does not."""
        out = {glob}
        while glob.startswith("**/"):
            glob = glob[3:]
            out.add(glob)
        for form in list(out):
            if "/**/" in form:
                out.add(form.replace("/**/", "/"))
        return out

    def keep(rel: str) -> bool:
        name = os.path.basename(rel)

        def hit(glob: str) -> bool:
            # A glob with no slash matches the file's name at any depth, as in ripgrep.
            return any(fnmatch.fnmatch(rel, form) or ("/" not in form and fnmatch.fnmatch(name, form)) for form in forms(glob))

        if any(hit(g) for g in exclude):
            return False
        return not include or any(hit(g) for g in include)

    return keep


def _python(root: Path, target: Path, pattern: str, *, glob: str, literal: bool, ignore_case: bool, files_only: bool, context: int, limit: int) -> Found:
    try:
        regex = re.compile(re.escape(pattern) if literal else pattern, re.IGNORECASE if ignore_case else 0)
    except re.error as error:
        return Found("python", [], {}, False, f"not a valid regular expression: {error}")
    keep = _glob_filter([g.strip() for g in glob.split(",") if g.strip()])
    found = Found("python", [], {}, False)
    for path in _candidate_files(root, target):
        rel = os.path.relpath(path, root)
        if is_secret_file(rel) or not keep(rel):
            continue
        try:
            if path.stat().st_size > MAX_FILE_BYTES:
                continue
            data = path.read_bytes()
        except OSError:
            continue
        if b"\0" in data[:8192]:
            continue
        lines = data.decode("utf-8", "replace").splitlines()
        hits = [i for i, line in enumerate(lines) if regex.search(line)]
        if not hits:
            continue
        found.files[rel] = len(hits)
        if files_only:
            continue
        shown: set[int] = set()
        for i in hits:
            if sum(1 for m in found.matches if not m.context) >= limit:
                found.truncated = True
                break
            for j in range(max(0, i - context), min(len(lines), i + context + 1)):
                if j in shown:
                    continue
                shown.add(j)
                found.matches.append(Match(rel, j + 1, _clip(lines[j]), context=j != i and j not in hits))
        if found.truncated:
            break
    return found


# ------------------------------------------------------------------------------------------------- the search


def search(
    pattern: str,
    path: str = ".",
    *,
    glob: str = "",
    literal: bool = False,
    ignore_case: bool = False,
    files_only: bool = False,
    context: int = 0,
    max_results: int = DEFAULT_RESULTS,
    root: str | os.PathLike | None = None,
) -> Found:
    """The structured search behind the tool (tests and other modules call this)."""
    base = Path(root).resolve() if root is not None else project_root()
    target = _resolve(base, path)
    if target is None:
        return Found("none", [], {}, False, f"{path!r} is outside the project ({base}); code_search only searches the project")
    if not target.exists():
        return Found("none", [], {}, False, f"{path!r} does not exist in the project")
    if not pattern:
        return Found("none", [], {}, False, "give a pattern to search for")
    limit = max(1, min(MAX_RESULTS, int(max_results or DEFAULT_RESULTS)))
    context = max(0, min(5, int(context or 0)))
    binary = ripgrep()
    kwargs = dict(glob=glob or "", literal=bool(literal), ignore_case=bool(ignore_case), files_only=bool(files_only), context=context, limit=limit)
    found = _rg(binary, base, target, pattern, **kwargs) if binary else _python(base, target, pattern, **kwargs)
    # One order whatever the engine: by file, then line. Then the cap, counted in matches (context lines ride along).
    found.matches.sort(key=lambda m: (m.path, m.line))
    kept: list[Match] = []
    count = 0
    for match in found.matches:
        if not match.context:
            count += 1
            if count > limit:
                found.truncated = True
                break
        kept.append(match)
    found.matches = kept
    return found


def render(found: Found, pattern: str, files_only: bool = False) -> str:
    if found.error:
        return f"code_search: {found.error}"
    engine = "ripgrep" if found.engine == "ripgrep" else "python search (install ripgrep for speed)"
    if not found.files:
        return f"No matches for {pattern!r} ({engine})."
    total = sum(found.files.values())
    if files_only:
        rows = sorted(found.files.items())
        head = f"{len(rows)} file(s) match {pattern!r} ({engine}):"
        return "\n".join([head, *(f"{path}  ({n} match{'es' if n != 1 else ''})" if n > 1 else path for path, n in rows)])
    shown = sum(1 for m in found.matches if not m.context)
    lines = [f"{total} match(es) in {len(found.files)} file(s) for {pattern!r} ({engine}):"]
    for match in found.matches:
        lines.append(f"{match.path}{'-' if match.context else ':'}{match.line}{'-' if match.context else ':'} {match.text}")
    if found.truncated or shown < total:
        lines.append(f"[showing {shown} of {total}+ — narrow with path=, glob= or a more specific pattern, or files_only=true for the file list]")
    return "\n".join(lines)


@tool
def code_search(
    pattern: str,
    path: str = ".",
    glob: str = "",
    literal: bool = False,
    ignore_case: bool = False,
    files_only: bool = False,
    context: int = 0,
    max_results: int = DEFAULT_RESULTS,
) -> str:
    """Search the project's code (ripgrep) and get back only the matching lines as path:line: text.

    Use this — not file_read — to find where a function, class, variable, setting or message is defined or used.
    Then read just the part you need (code_outline, or file_read with a line range), and use lsp_definition /
    lsp_references for exact semantic answers. Read-only; never asks.

    Args:
        pattern: A regular expression (ripgrep syntax), e.g. "def build_session", "\\bMAX_RETRIES\\b", "TODO|FIXME".
        path: A file or directory inside the project to search (default: the whole project).
        glob: Comma-separated file globs to include, "!" to exclude, e.g. "*.py", "*.rs,!tests/**".
        literal: Treat the pattern as plain text, not a regex.
        ignore_case: Case-insensitive match.
        files_only: List matching files (with counts) instead of lines.
        context: Lines of context around each match (0-5).
        max_results: Maximum matching lines to return (default 60, at most 400).
    """
    found = search(pattern, path, glob=glob, literal=literal, ignore_case=ignore_case, files_only=files_only, context=context, max_results=max_results)
    return render(found, pattern, files_only)

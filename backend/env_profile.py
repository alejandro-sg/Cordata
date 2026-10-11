"""The environment profiler: what the project is and what the machine has, read without running the project.

Two consumers:

* the **system prompt** — one line in the workspace context (``workspace_context.summary``): size class, files and
  lines, languages, the dependency manifests found and how many dependencies they declare, unified memory;
* **adaptive reasoning** (``adaptive_effort.py``) — the size class is one of its two inputs.

And one tool, ``environment_profile``, which adds the toolchain versions (``python3``/``rustc``/``cargo``/``node``/
``go``/``ruff``/``rg``/``git`` ``--version`` — only binaries found on ``PATH``, a fixed argv each, two seconds each).

What it does **not** do, on purpose: run the project's own interpreter (``.venv/bin/python -m pip freeze`` is the
repository's program, and a cloned repository's would run as the person) — dependencies are counted from the
manifests (``requirements*.txt``, ``pyproject.toml``, ``Cargo.toml``, ``package.json``, ``go.mod``) instead.

Size classes, from tracked files (``git ls-files``, else a bounded walk) and a line estimate (bytes / 40):
``small`` (< 300 files and < 30k lines), ``large`` (> 4,000 files, > 400k lines, or a workspace of three or more
packages over 50k lines — a monorepo),
``medium`` otherwise. Cached per project for ten minutes.
"""

from __future__ import annotations

import json
import os
import platform
import re
import shutil
import subprocess
import threading
import time
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from strands import tool

CACHE_SECONDS = 600.0
WALK_LIMIT = 50_000
TOOLCHAINS = (
    ("python", ("python3", "--version")),
    ("rustc", ("rustc", "--version")),
    ("cargo", ("cargo", "--version")),
    ("node", ("node", "--version")),
    ("go", ("go", "version")),
    ("ruff", ("ruff", "--version")),
    ("ripgrep", ("rg", "--version")),
    ("git", ("git", "--version")),
)
_LANGUAGES = {
    ".py": "Python", ".rs": "Rust", ".ts": "TypeScript", ".tsx": "TypeScript", ".js": "JavaScript", ".jsx": "JavaScript",
    ".go": "Go", ".java": "Java", ".kt": "Kotlin", ".swift": "Swift", ".c": "C", ".h": "C", ".cpp": "C++", ".cc": "C++",
    ".rb": "Ruby", ".cs": "C#", ".php": "PHP", ".scala": "Scala", ".sh": "Shell", ".proto": "Protobuf",
}
_SKIP = frozenset({".git", "node_modules", "target", ".venv", "venv", "__pycache__", "dist", "build", ".next", ".tox", ".cache"})


@dataclass
class Profile:
    root: str
    files: int = 0
    lines: int = 0
    size_class: str = "small"
    languages: list[tuple[str, int]] = field(default_factory=list)  # (language, files), most first
    manifests: dict[str, int] = field(default_factory=dict)  # manifest -> declared dependencies
    workspaces: int = 0  # packages in a Cargo/npm/uv workspace
    memory_gb: float | None = None
    free_gb: float | None = None
    toolchains: dict[str, str] = field(default_factory=dict)
    taken: float = 0.0

    def line(self) -> str:
        """The one line the workspace context carries."""
        langs = ", ".join(name for name, _ in self.languages[:4]) or "no source files recognised"
        deps = ", ".join(f"{name} ({n})" for name, n in list(self.manifests.items())[:5])
        # Total only: the prompt must be the same from one call to the next (a cached prefix, a replayable cassette), and
        # free memory changes by the second. The tool reports it.
        memory = f"; {self.memory_gb:.0f} GB unified memory" if self.memory_gb else ""
        ws = f", {self.workspaces}-package workspace" if self.workspaces > 1 else ""
        return f"Environment: {self.size_class} project — {self.files:,} files, ~{self.lines:,} lines ({langs}){ws}" + (f"; dependencies: {deps}" if deps else "") + memory

    def to_json(self) -> dict[str, Any]:
        return {
            "root": self.root, "files": self.files, "lines": self.lines, "size_class": self.size_class,
            "languages": dict(self.languages), "manifests": self.manifests, "workspaces": self.workspaces,
            "memory_gb": self.memory_gb, "free_gb": self.free_gb, "toolchains": self.toolchains,
        }


# ------------------------------------------------------------------------------------------------- readings


def _tracked(root: Path) -> list[tuple[str, int]] | None:
    if not (root / ".git").exists():
        return None
    try:
        done = subprocess.run(["git", "ls-files", "-z"], cwd=root, capture_output=True, timeout=10)
    except (OSError, subprocess.TimeoutExpired):
        return None
    if done.returncode != 0:
        return None
    out = []
    for name in done.stdout.decode("utf-8", "replace").split("\0"):
        if not name:
            continue
        try:
            out.append((name, (root / name).stat().st_size))
        except OSError:
            continue
    return out


def _walk(root: Path) -> list[tuple[str, int]]:
    out: list[tuple[str, int]] = []
    for directory, dirs, files in os.walk(root):
        dirs[:] = [d for d in dirs if d not in _SKIP and not d.startswith(".")]
        for name in files:
            path = Path(directory) / name
            try:
                out.append((str(path.relative_to(root)), path.stat().st_size))
            except OSError:
                continue
            if len(out) >= WALK_LIMIT:
                return out
    return out


def _count_requirements(text: str) -> int:
    return sum(1 for line in text.splitlines() if line.strip() and not line.strip().startswith(("#", "-")))


def _manifests(root: Path, names: list[str]) -> tuple[dict[str, int], int]:
    found: dict[str, int] = {}
    workspaces = 0
    for name in sorted(n for n in names if n.count("/") <= 2):
        base = os.path.basename(name)
        path = root / name
        try:
            if base.startswith("requirements") and base.endswith(".txt"):
                found[name] = _count_requirements(path.read_text(encoding="utf-8", errors="replace"))
            elif base == "pyproject.toml":
                import tomllib

                data = tomllib.loads(path.read_text(encoding="utf-8", errors="replace"))
                deps = list((data.get("project") or {}).get("dependencies") or [])
                deps += list(((data.get("tool") or {}).get("poetry") or {}).get("dependencies") or [])
                found[name] = len(deps)
                workspaces = max(workspaces, len((((data.get("tool") or {}).get("uv") or {}).get("workspace") or {}).get("members") or []))
            elif base == "Cargo.toml":
                import tomllib

                data = tomllib.loads(path.read_text(encoding="utf-8", errors="replace"))
                found[name] = len(data.get("dependencies") or {}) + len((data.get("workspace") or {}).get("dependencies") or {})
                workspaces = max(workspaces, len((data.get("workspace") or {}).get("members") or []))
            elif base == "package.json" and "node_modules" not in name:
                data = json.loads(path.read_text(encoding="utf-8", errors="replace"))
                found[name] = len(data.get("dependencies") or {}) + len(data.get("devDependencies") or {})
                ws = data.get("workspaces")
                workspaces = max(workspaces, len(ws if isinstance(ws, list) else (ws or {}).get("packages") or []))
            elif base == "go.mod":
                found[name] = sum(1 for line in path.read_text(encoding="utf-8", errors="replace").splitlines() if re.match(r"^\s+\S+\s+v\d", line))
        except Exception:
            continue
    return found, workspaces


def memory() -> tuple[float | None, float | None]:
    """(total, roughly free) unified memory in GB. macOS: ``hw.memsize`` and ``vm_stat`` (free + inactive + speculative)."""
    try:
        if platform.system() == "Darwin":
            total = int(subprocess.run(["sysctl", "-n", "hw.memsize"], capture_output=True, text=True, timeout=2).stdout.strip())
            stat = subprocess.run(["vm_stat"], capture_output=True, text=True, timeout=2).stdout
            page = int(re.search(r"page size of (\d+)", stat).group(1)) if re.search(r"page size of (\d+)", stat) else 16384
            pages = sum(int(m.group(2)) for m in re.finditer(r"Pages (free|inactive|speculative):\s+(\d+)", stat))
            return total / 1024**3, pages * page / 1024**3
        pages, size = os.sysconf("SC_PHYS_PAGES"), os.sysconf("SC_PAGE_SIZE")
        free = os.sysconf("SC_AVPHYS_PAGES") * size
        return pages * size / 1024**3, free / 1024**3
    except Exception:
        return None, None


def toolchains() -> dict[str, str]:
    found: dict[str, str] = {}
    for name, argv in TOOLCHAINS:
        if shutil.which(argv[0]) is None:
            continue
        try:
            done = subprocess.run(list(argv), capture_output=True, text=True, timeout=2)
        except (OSError, subprocess.TimeoutExpired):
            continue
        text = (done.stdout or done.stderr).strip().splitlines()
        if text:
            found[name] = text[0][:80]
    return found


def classify(files: int, lines: int, workspaces: int) -> str:
    if files > 4000 or lines > 400_000 or (workspaces > 2 and lines > 50_000):
        return "large"
    if files < 300 and lines < 30_000:
        return "small"
    return "medium"


_cache: dict[str, tuple[float, Profile]] = {}
_lock = threading.Lock()


def profile(root: str | os.PathLike, *, with_toolchains: bool = False, fresh: bool = False) -> Profile:
    """The project's profile (cached ten minutes). Never raises."""
    key = str(Path(root).resolve())
    with _lock:
        hit = _cache.get(key)
    if hit and not fresh and time.monotonic() - hit[0] < CACHE_SECONDS and (hit[1].toolchains or not with_toolchains):
        return hit[1]
    started = time.monotonic()
    base = Path(key)
    result = Profile(root=key)
    try:
        listed = _tracked(base)
        entries = listed if listed is not None else _walk(base)
        result.files = len(entries)
        languages: Counter[str] = Counter()
        code_bytes = 0
        for name, size in entries:
            language = _LANGUAGES.get(Path(name).suffix.lower())
            if language:
                languages[language] += 1
                code_bytes += size
        result.lines = code_bytes // 40
        result.languages = languages.most_common()
        manifest_names = [n for n, _ in entries if os.path.basename(n) in ("pyproject.toml", "Cargo.toml", "package.json", "go.mod") or re.match(r"requirements[^/]*\.txt$", os.path.basename(n))]
        result.manifests, result.workspaces = _manifests(base, manifest_names)
        result.size_class = classify(result.files, result.lines, result.workspaces)
        result.memory_gb, result.free_gb = memory()
        if with_toolchains:
            result.toolchains = toolchains()
    except Exception:
        pass
    result.taken = time.monotonic() - started
    with _lock:
        _cache[key] = (time.monotonic(), result)
    return result


def size_class(root: str | os.PathLike) -> str:
    return profile(root).size_class


@tool
def environment_profile() -> str:
    """Profile this project and machine: size, languages, dependency manifests, toolchain versions (python, rustc,
    cargo, node, go, ruff, ripgrep, git) and unified memory. Read-only; runs only `--version` of tools on PATH."""
    found = profile(os.getcwd(), with_toolchains=True, fresh=True)
    lines = [found.line() + (f" ({found.free_gb:.0f} GB free now)" if found.free_gb is not None else "")]
    if found.toolchains:
        lines.append("Toolchains: " + "; ".join(f"{name}: {version}" for name, version in found.toolchains.items()))
    else:
        lines.append("Toolchains: none of python3, rustc, cargo, node, go, ruff, rg, git found on PATH")
    lines.append("Languages by files: " + ", ".join(f"{name} {n}" for name, n in found.languages[:8]))
    return "\n".join(lines)

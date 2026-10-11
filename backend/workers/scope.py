"""A task's file scope: which paths it may change, checked twice.

**Before, by the kernel.** A subprocess worker (Claude Code, agy) runs under a Seatbelt profile that denies every
write inside the project except the scoped paths — so an out-of-scope edit fails at ``open(2)``, whichever tool or
script the CLI used. Outside the project nothing changes (the CLIs keep their own state under ``~`` and ``$TMPDIR``),
and the ``.env`` / credential denials of ``scrubbed_sandbox.seatbelt_profile`` are re-applied *after* the scope
allows, because Seatbelt takes the last matching rule and a scoped directory must not re-open its ``.env``.

**After, by comparison.** ``out_of_scope(changed, scope)`` is what the audit runs on the files that actually changed —
the check that covers the in-process Ollama worker too, and anything the profile let through because a glob had to be
widened to its directory.

Scope entries, relative to the project:

    src/app.py          that file (and same-name siblings ``src/app.py.*`` — editors write a temp file and rename)
    src/ or src/**      everything under ``src``
    src/*.py, **/x.py   a glob: exact after the fact, widened to its literal prefix directory in the profile
"""

from __future__ import annotations

import fnmatch
import os
from collections.abc import Iterable
from pathlib import Path

import scrubbed_sandbox

_GLOB = set("*?[")
_ERE_SPECIAL = set(".^$*+?()[]{}|\\")


def _ere(text: str) -> str:
    return "".join("\\" + ch if ch in _ERE_SPECIAL else ch for ch in text)


def _sb_string(value: str) -> str:
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def normalize(entry: str) -> str:
    """``./src//a.py`` → ``src/a.py``; a directory keeps one trailing ``/``. Never absolute, never ``..``."""
    raw = str(entry or "").strip().replace(os.sep, "/")
    is_dir = raw.endswith("/") or raw.endswith("/**")
    if raw.endswith("/**"):
        raw = raw[:-3]
    parts = [p for p in raw.split("/") if p not in ("", ".")]
    clean = "/".join(parts)
    return clean + "/" if is_dir and clean else clean


def valid(entry: str, root: str) -> str | None:
    """Why ``entry`` cannot be a scope entry, or None. Absolute paths, ``..`` and secrets files are refused."""
    raw = str(entry or "").strip()
    if not raw:
        return "empty scope entry"
    if os.path.isabs(raw):
        return f"{raw!r} is absolute; scope entries are project-relative"
    if ".." in raw.replace(os.sep, "/").split("/"):
        return f"{raw!r} leaves the project"
    if scrubbed_sandbox.is_secret_path(raw):
        return f"{raw!r} is a secrets file"
    return None


def _is_dir_entry(entry: str, root: str) -> bool:
    return entry.endswith("/") or (not (_GLOB & set(entry)) and os.path.isdir(os.path.join(root, entry)))


def matches(path: str, scope: Iterable[str], root: str = "") -> bool:
    """Is project-relative ``path`` inside ``scope``?"""
    rel = normalize(path).rstrip("/")
    for raw in scope:
        entry = normalize(raw)
        if not entry:
            continue
        if _GLOB & set(entry):
            if fnmatch.fnmatchcase(rel, entry.rstrip("/")):
                return True
            continue
        if _is_dir_entry(entry, root):
            base = entry.rstrip("/")
            if rel == base or rel.startswith(base + "/"):
                return True
            continue
        if rel == entry or (rel.startswith(entry + ".") and "/" not in rel[len(entry):]):
            return True
    return False


def out_of_scope(changed: Iterable[str], scope: Iterable[str], root: str = "") -> list[str]:
    scope = list(scope)
    return sorted(p for p in changed if not matches(p, scope, root))


def _literal_prefix(entry: str) -> str:
    parts = []
    for part in entry.split("/"):
        if _GLOB & set(part):
            break
        parts.append(part)
    return "/".join(parts)


def seatbelt_rules(root: str, scope: Iterable[str]) -> list[str]:
    """The profile lines that confine writes inside ``root`` to ``scope``. An empty scope means read-only."""
    real = os.path.realpath(root)
    lines = [f"(deny file-write* (subpath {_sb_string(real)}))"]
    allows: list[str] = []
    ancestors: set[str] = set()
    for raw in scope:
        if valid(raw, real) is not None:
            continue
        entry = normalize(raw)
        if not entry:
            continue
        if _GLOB & set(entry):
            prefix = _literal_prefix(entry)
            target = os.path.join(real, prefix) if prefix else None
            if target is None:
                continue  # a glob at the root ("*.py") cannot be widened without opening the whole project
            allows.append(f"(subpath {_sb_string(target)})")
            rel_dir = prefix
        elif _is_dir_entry(entry, real):
            target = os.path.join(real, entry.rstrip("/"))
            allows.append(f"(subpath {_sb_string(target)})")
            rel_dir = entry.rstrip("/")
        else:
            target = os.path.join(real, entry)
            allows.append(f"(literal {_sb_string(target)})")
            allows.append(f'(regex #"^{_ere(target)}\\.[^/]*$")')
            rel_dir = os.path.dirname(entry)
        parts = [p for p in rel_dir.split("/") if p]
        for i in range(1, len(parts) + 1):
            ancestors.add(os.path.join(real, *parts[:i]))
    allows.extend(f"(literal {_sb_string(a)})" for a in sorted(ancestors))
    if allows:
        lines.append("(allow file-write* " + " ".join(allows) + ")")
    return lines


def scoped_profile(root: str, scope: Iterable[str], home: str | None = None) -> str:
    """``scrubbed_sandbox.seatbelt_profile`` with the scope rules spliced in after ``(allow default)`` and before the
    secret denials, so the denials still win."""
    base = scrubbed_sandbox.seatbelt_profile(home).splitlines()
    head, tail = base[:2], base[2:]
    return "\n".join([*head, *seatbelt_rules(root, scope), *tail]) + "\n"


class ScopedSandbox(scrubbed_sandbox.ScrubbedSandbox):
    """``ScrubbedSandbox`` whose Seatbelt profile also confines writes inside ``root`` to a scope."""

    def __init__(self, root: str | os.PathLike[str], scope: Iterable[str], **kwargs) -> None:
        super().__init__(root, **kwargs)
        self.scope = list(scope)
        self._profile = scoped_profile(str(Path(root).resolve()), self.scope)

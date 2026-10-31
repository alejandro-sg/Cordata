"""Isolated parallel waves and the SYNTHESIZE pass that folds them back together.

**Why isolate.** Steps of one wave have pairwise-disjoint scopes, but they still share one working tree: a worker's
changed-files list is a before/after snapshot of the *whole* project (so it sees its siblings' writes), a rollback has
to be narrowed to the step's own scope, and a build or test a worker runs reads its siblings' half-written files.
``git worktree add HEAD`` is not the answer — it drops the person's uncommitted edits, and checkpoints work in folders
that are not git repositories at all. So each step of a wave of several runs in a **sandbox**: a copy of the project as
it is right now, dirty files included.

**How it is cheap.** On APFS (every Mac) a file is copied with ``clonefile(2)`` — copy-on-write, metadata only, no data
blocks duplicated until someone writes. Elsewhere (Linux CI) it falls back to an ordinary copy. Dependency and build
directories (``checkpoints.DEPENDENCY_DIRS``: ``node_modules``, ``.venv``, ``target``…) are not copied but *symlinked*,
so a test run in the sandbox finds its toolchain; ``.git`` is left out (a sandbox is a scratch tree, not a branch).

**Harvest.** When a step completes, the files it changed *inside its scope* are copied back into the primary tree.
A file the primary tree changed since the sandbox was made (the person edited it, or another wave did) is a conflict:
it is reported, never overwritten.

**Synthesize.** After the last wave, before HARDEN, ``check`` reads the combined change: syntax of every changed file
(``preflight.check_syntax``: Python, JSON, TOML, YAML) and, for Python, imports between project modules — a
``from pkg.mod import name`` whose module exists in the project but no longer defines ``name`` is exactly the drift two
agents writing two sides of one interface produce. Problems found here get one targeted fix pass (machine.py).
"""

from __future__ import annotations

import ast
import ctypes
import ctypes.util
import hashlib
import logging
import os
import shutil
import sys
import tempfile
from dataclasses import dataclass, field
from pathlib import Path

import checkpoints

logger = logging.getLogger(__name__)

#: Left out of a sandbox entirely (not linked either): a sandbox is not a repository, and the shadow store is the run's.
SKIP = frozenset({".git", ".cordata"})
MAX_FILES = 60_000

_clonefile = None
if sys.platform == "darwin":
    try:
        _libc = ctypes.CDLL(ctypes.util.find_library("c") or "libc.dylib", use_errno=True)
        _clonefile = _libc.clonefile
        _clonefile.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_int]
        _clonefile.restype = ctypes.c_int
    except (OSError, AttributeError):
        _clonefile = None

CLONEFILE_NOFOLLOW = 0x0001


def _copy(src: str, dst: str) -> bool:
    """Copy one file, copy-on-write when the filesystem can. True when it was a clone."""
    if _clonefile is not None and _clonefile(os.fsencode(src), os.fsencode(dst), CLONEFILE_NOFOLLOW) == 0:
        return True
    shutil.copy2(src, dst, follow_symlinks=False)
    return False


def _digest(path: str) -> str | None:
    try:
        with open(path, "rb") as handle:
            return hashlib.blake2b(handle.read(), digest_size=16).hexdigest()
    except OSError:
        return None


@dataclass
class Sandbox:
    """One step's private copy of the project."""

    root: str
    source: str
    cloned: int = 0
    copied: int = 0
    linked: list[str] = field(default_factory=list)
    #: relative path → digest of the primary tree's file when the step started (``remember``; None: it did not exist) —
    #: what harvest compares against to tell the step's change from someone else's.
    base: dict[str, str | None] = field(default_factory=dict)

    @property
    def cow(self) -> bool:
        return self.cloned > 0 and self.copied == 0

    def discard(self) -> None:
        shutil.rmtree(self.root, ignore_errors=True)


class TooLarge(RuntimeError):
    pass


def sweep(older_than_s: float = 6 * 3600, parent: str | None = None) -> int:
    """Remove sandboxes a crashed run left behind (``cordata-*`` under the temp dir, untouched for ``older_than_s``)."""
    import time

    base = parent or tempfile.gettempdir()
    removed = 0
    try:
        entries = list(os.scandir(base))
    except OSError:
        return 0
    for entry in entries:
        if entry.name.startswith("cordata-") and entry.is_dir(follow_symlinks=False):
            try:
                if time.time() - entry.stat(follow_symlinks=False).st_mtime > older_than_s:
                    shutil.rmtree(entry.path, ignore_errors=True)
                    removed += 1
            except OSError:
                continue
    return removed


def create(project: str, *, parent: str | None = None, label: str = "step", scope: list[str] | None = None) -> Sandbox:
    """A sandbox of ``project`` as it is now (uncommitted and untracked files included). With ``scope``, the primary
    tree's in-scope files are remembered as the harvest base."""
    source = os.path.realpath(project)
    root = tempfile.mkdtemp(prefix=f"cordata-{label}-", dir=parent)
    box = Sandbox(root=root, source=source)
    count = 0
    for directory, dirs, files in os.walk(source, followlinks=False):
        rel_dir = os.path.relpath(directory, source)
        target_dir = root if rel_dir == "." else os.path.join(root, rel_dir)
        keep = []
        for name in dirs:
            if name in SKIP:
                continue
            path = os.path.join(directory, name)
            if name in checkpoints.DEPENDENCY_DIRS or os.path.islink(path):
                os.symlink(os.readlink(path) if os.path.islink(path) else path, os.path.join(target_dir, name))
                box.linked.append(os.path.normpath(os.path.join(rel_dir, name)))
                continue
            os.mkdir(os.path.join(target_dir, name))
            keep.append(name)
        dirs[:] = keep
        for name in files:
            count += 1
            if count > MAX_FILES:
                box.discard()
                raise TooLarge(f"the project has more than {MAX_FILES:,} files")
            src = os.path.join(directory, name)
            dst = os.path.join(target_dir, name)
            if os.path.islink(src):
                os.symlink(os.readlink(src), dst)
                continue
            try:
                if _copy(src, dst):
                    box.cloned += 1
                else:
                    box.copied += 1
            except OSError as error:
                logger.debug("synthesizer: could not copy %s: %s", src, error)
                continue
    if scope:
        remember(box, scoped_files(box, scope))
    return box


@dataclass
class Harvest:
    written: list[str] = field(default_factory=list)
    deleted: list[str] = field(default_factory=list)
    conflicts: list[str] = field(default_factory=list)
    skipped: list[str] = field(default_factory=list)  # changed in the sandbox but outside the scope: left behind


def remember(box: Sandbox, paths: list[str]) -> None:
    """Record the primary tree's content of ``paths`` as the sandbox's base, before the step runs — so harvest can tell
    "the step changed it" from "someone changed it in the primary tree meanwhile"."""
    for rel in paths:
        box.base[rel] = _digest(os.path.join(box.source, rel))


def scoped_files(box: Sandbox, scope: list[str]) -> list[str]:
    """Files in the sandbox (and in the source) that a scope covers — the base a step's harvest is checked against."""
    from workers import scope as scope_mod

    out = set()
    for tree in (box.root, box.source):
        for directory, dirs, files in os.walk(tree, followlinks=False):
            dirs[:] = [d for d in dirs if d not in SKIP and d not in checkpoints.DEPENDENCY_DIRS]
            for name in files:
                rel = os.path.relpath(os.path.join(directory, name), tree)
                if scope_mod.matches(rel, scope, box.source):
                    out.add(rel)
    return sorted(out)


def harvest(box: Sandbox, changed: list[str], scope: list[str]) -> Harvest:
    """Copy a completed step's in-scope changes from its sandbox into the primary tree."""
    from workers import scope as scope_mod

    out = Harvest()
    for rel in sorted(set(changed)):
        rel = os.path.normpath(rel)
        if rel.startswith("..") or os.path.isabs(rel):
            out.skipped.append(rel)
            continue
        if not scope_mod.matches(rel, scope, box.source):
            out.skipped.append(rel)
            continue
        src = os.path.join(box.root, rel)
        dst = os.path.join(box.source, rel)
        # A worker that wrote through a symlink (a linked dependency dir, a planted link) wrote outside its sandbox:
        # never follow it back into the primary tree.
        real_root = os.path.realpath(box.root)
        if os.path.lexists(src) and (os.path.islink(src) or os.path.commonpath([real_root, os.path.realpath(src)]) != real_root):
            out.skipped.append(rel)
            continue
        if _digest(dst) != box.base.get(rel):
            # The primary tree's file is not what the sandbox started from (edited, created or deleted meanwhile).
            out.conflicts.append(rel)
            continue
        if os.path.lexists(src):
            os.makedirs(os.path.dirname(dst) or box.source, exist_ok=True)
            tmp = dst + ".cordata-harvest"
            shutil.copy2(src, tmp, follow_symlinks=False)
            os.replace(tmp, dst)
            out.written.append(rel)
        elif os.path.exists(dst):
            os.remove(dst)
            out.deleted.append(rel)
    return out


# ---------------------------------------------------------------------------------------------- the SYNTHESIZE checks


@dataclass(frozen=True)
class Problem:
    check: str  # syntax | import
    path: str
    line: int | None
    message: str

    def render(self) -> str:
        return f"{self.path}:{self.line}: [{self.check}] {self.message}" if self.line else f"{self.path}: [{self.check}] {self.message}"


def _module_file(project: str, module: str, importer: str) -> str | None:
    """The project file a dotted module name refers to, if it is one of the project's own (None for a library)."""
    parts = module.split(".")
    bases = [project, os.path.dirname(os.path.join(project, importer))]
    for base in bases:
        for candidate in (os.path.join(base, *parts) + ".py", os.path.join(base, *parts, "__init__.py")):
            if os.path.isfile(candidate):
                return candidate
    return None


def _defined_names(path: str) -> set[str] | None:
    try:
        tree = ast.parse(Path(path).read_text(encoding="utf-8", errors="replace"))
    except (SyntaxError, OSError, ValueError):
        return None
    names: set[str] = set()
    for node in tree.body:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            names.add(node.name)
        elif isinstance(node, (ast.Assign, ast.AnnAssign)):
            targets = node.targets if isinstance(node, ast.Assign) else [node.target]
            for target in targets:
                for leaf in ast.walk(target):
                    if isinstance(leaf, ast.Name):
                        names.add(leaf.id)
        elif isinstance(node, (ast.Import, ast.ImportFrom)):
            for alias in node.names:
                names.add((alias.asname or alias.name).split(".")[0])
        elif isinstance(node, (ast.If, ast.Try)):
            return None  # conditional definitions: too clever to judge statically, so do not
    if any(isinstance(n, ast.ImportFrom) and any(a.name == "*" for a in n.names) for n in tree.body):
        return None
    return names


def check(project: str, files: list[str]) -> list[Problem]:
    """What is wrong with the combined change, deterministically: syntax, and imports between the project's own modules."""
    import preflight

    problems: list[Problem] = []
    for rel in sorted(set(files)):
        path = os.path.join(project, rel)
        if not os.path.isfile(path):
            continue
        try:
            text = Path(path).read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        for found in preflight.check_syntax(path, None, text, project):
            problems.append(Problem("syntax", found.path, found.line, found.message))
        if not rel.endswith(".py") or any(p.path == rel and p.check == "syntax" for p in problems):
            continue
        try:
            tree = ast.parse(text)
        except SyntaxError:
            continue
        for node in ast.walk(tree):
            if not isinstance(node, ast.ImportFrom) or (not node.module and not node.level):
                continue
            if node.level:
                # `from .add import add` in pkg/__init__.py: resolve against the importer's package, level dirs up.
                base = os.path.dirname(rel)
                for _ in range(node.level - 1):
                    base = os.path.dirname(base)
                module = ".".join([p for p in base.split(os.sep) if p] + ([node.module] if node.module else []))
            else:
                module = node.module
            if not module:
                continue
            target = _module_file(project, module, rel) if not node.level else next(
                (c for c in (os.path.join(project, *module.split(".")) + ".py", os.path.join(project, *module.split("."), "__init__.py")) if os.path.isfile(c)), None
            )
            node_module = module
            if target is None:
                continue
            names = _defined_names(target)
            if names is None:
                continue
            shown = ("." * node.level) + (node.module or "")
            for alias in node.names:
                if alias.name == "*" or alias.name in names:
                    continue
                if _module_file(project, f"{node_module}.{alias.name}", rel):
                    continue  # a submodule, not a name
                problems.append(Problem("import", rel, node.lineno, f"`from {shown} import {alias.name}`: {os.path.relpath(target, project)} defines no `{alias.name}`"))
    return problems

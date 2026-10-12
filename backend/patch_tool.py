"""``apply_patch``: apply a unified diff to the project, atomically.

The Debugger specialist answers a failure with a patch in this format, the
self-healing loop applies it with this tool (through the same approval gate as
``editor`` — it is an ``edit``-kind tool), and acp-tui renders the patch as a
coloured diff in the approval prompt and the tool card.

Rules: every path must stay inside the working directory (no ``..``, no
absolute paths outside it); all hunks of all files must apply or nothing is
written; a hunk whose context moved is found by searching outward from its
line hint (exact match first, then ignoring trailing whitespace).
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass, field
from pathlib import Path

from strands import tool

_HUNK = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")


class PatchError(ValueError):
    pass


@dataclass
class Hunk:
    old_start: int
    anchored: bool = True  # False: the header had no line numbers (a bare @@)
    lines: list[tuple[str, str]] = field(default_factory=list)  # (op, text) op in ' ', '-', '+'

    @property
    def old(self) -> list[str]:
        return [text for op, text in self.lines if op in (" ", "-")]

    @property
    def new(self) -> list[str]:
        return [text for op, text in self.lines if op in (" ", "+")]


@dataclass
class FilePatch:
    old_path: str | None  # None: new file
    new_path: str | None  # None: deleted file
    hunks: list[Hunk] = field(default_factory=list)
    no_newline_at_end: bool = False

    @property
    def path(self) -> str:
        return self.new_path or self.old_path or ""


def _strip_prefix(raw: str) -> str | None:
    raw = raw.split("\t", 1)[0].strip()
    if raw == "/dev/null":
        return None
    if raw.startswith(("a/", "b/")):
        raw = raw[2:]
    return raw


def _is_file_header(lines: list[str], i: int) -> bool:
    return lines[i].startswith("--- ") and i + 1 < len(lines) and lines[i + 1].startswith("+++ ")


def parse(patch: str) -> list[FilePatch]:
    """Unified diffs, tolerant of how models write them: hunk headers without
    line numbers (a bare ``@@``), context lines whose leading space was dropped,
    and the ``*** Begin Patch`` / ``*** Update File:`` envelope OpenAI-style
    models are trained on."""
    files: list[FilePatch] = []
    current: FilePatch | None = None
    hunk: Hunk | None = None
    lines = patch.replace("\r\n", "\n").split("\n")
    while lines and not lines[-1].strip():
        lines.pop()

    def open_hunk(start: int | None) -> Hunk:
        new = Hunk(old_start=start if start is not None else 0, anchored=start is not None)
        current.hunks.append(new)
        return new

    i = 0
    while i < len(lines):
        line = lines[i]
        if _is_file_header(lines, i):
            current = FilePatch(_strip_prefix(line[4:]), _strip_prefix(lines[i + 1][4:]))
            files.append(current)
            hunk = None
            i += 2
            continue
        envelope = re.match(r"^\*\*\* (Update|Add|Delete) File:\s*(.+?)\s*$", line)
        if envelope:
            kind, path = envelope.group(1), _strip_prefix(envelope.group(2))
            current = FilePatch(None if kind == "Add" else path, None if kind == "Delete" else path)
            files.append(current)
            hunk = open_hunk(0) if kind == "Add" else None
            i += 1
            continue
        moved = re.match(r"^\*\*\* Move to:\s*(.+?)\s*$", line)
        if moved and current is not None:
            current.new_path = _strip_prefix(moved.group(1))
            i += 1
            continue
        if line.startswith("***") or line.startswith("diff --git") or line.startswith("index "):
            hunk = None if not line.startswith("***") or "End" in line or "Begin" in line else hunk
            i += 1
            continue
        if line.startswith("@@") and current is not None:
            match = _HUNK.match(line)
            hunk = open_hunk(int(match.group(1)) if match else None)
            i += 1
            continue
        if line.startswith("\\ No newline") and current is not None:
            current.no_newline_at_end = True
            i += 1
            continue
        if current is not None and hunk is None and line[:1] in ("-", "+", " ") and current.old_path and current.new_path:
            hunk = open_hunk(None)  # an envelope section's changes before any @@
        if hunk is not None:
            if line[:1] in (" ", "-", "+"):
                hunk.lines.append((line[0], line[1:]))
            else:
                hunk.lines.append((" ", line))  # a context line whose leading space was dropped
        i += 1
    files = [f for f in files if f.hunks or f.new_path is None]
    for file_patch in files:
        file_patch.hunks = [h for h in file_patch.hunks if h.lines]
    files = [f for f in files if f.hunks or f.new_path is None]
    if not files:
        raise PatchError("no file changes found — expected a unified diff with ---/+++ headers and @@ hunks")
    return files


def patch_paths(patch: str) -> list[str]:
    try:
        return [f.path for f in parse(patch) if f.new_path]
    except PatchError:
        return []


def _safe_path(root: Path, relative: str) -> Path:
    candidate = (root / relative).resolve()
    if candidate != root and root not in candidate.parents:
        raise PatchError(f"{relative}: outside the project directory — refused")
    return candidate


def _find(haystack: list[str], needle: list[str], hint: int) -> int:
    if not needle:
        return min(max(hint, 0), len(haystack))
    order = sorted(range(len(haystack) - len(needle) + 1), key=lambda start: abs(start - hint))
    # Exact first; then ignoring trailing whitespace; then ignoring indentation
    # (models often re-indent the context lines they copy).
    for normalise in (lambda s: s, lambda s: s.rstrip(), lambda s: s.strip()):
        wanted = [normalise(x) for x in needle]
        for start in order:
            if [normalise(x) for x in haystack[start : start + len(needle)]] == wanted:
                return start
    return -1


def apply_to_text(original: str, file_patch: FilePatch) -> str:
    had_newline = original.endswith("\n")
    lines = original.split("\n") if original else []
    if had_newline:
        lines.pop()
    offset = 0
    for number, hunk in enumerate(file_patch.hunks, 1):
        if not hunk.anchored and not hunk.old and lines:
            raise PatchError(f"{file_patch.path}: hunk {number} adds lines but has no line numbers or context to place them")
        start = _find(lines, hunk.old, hunk.old_start - 1 + offset)
        if start < 0:
            preview = "\n".join(hunk.old[:6])
            raise PatchError(f"{file_patch.path}: hunk {number} does not match the file (expected near line {hunk.old_start}):\n{preview}")
        # Context lines are kept from the file itself, never copied from the
        # hunk: a model's context may be re-indented or have lost its marker,
        # and it was only ever meant to locate the change.
        replacement: list[str] = []
        cursor = start
        for op, text in hunk.lines:
            if op == " ":
                replacement.append(lines[cursor])
                cursor += 1
            elif op == "-":
                cursor += 1
            else:
                replacement.append(text)
        lines[start:cursor] = replacement
        offset += len(replacement) - (cursor - start)
    text = "\n".join(lines)
    if lines and not file_patch.no_newline_at_end and (had_newline or not original):
        text += "\n"
    return text


def plan(patch: str, root: str | os.PathLike) -> list[tuple[Path, str | None, FilePatch]]:
    """(path, new text or None to delete, file patch) for every file; raises
    PatchError without touching disk if anything would fail."""
    root = Path(root).resolve()
    result = []
    for file_patch in parse(patch):
        if file_patch.new_path is None:
            target = _safe_path(root, file_patch.old_path or "")
            if not target.exists():
                raise PatchError(f"{file_patch.old_path}: cannot delete, file does not exist")
            result.append((target, None, file_patch))
            continue
        target = _safe_path(root, file_patch.new_path)
        if file_patch.old_path is None:
            if target.exists() and target.read_text(encoding="utf-8", errors="replace").strip():
                raise PatchError(f"{file_patch.new_path}: patch creates the file but it already exists")
            original = ""
        else:
            source = _safe_path(root, file_patch.old_path)
            if not source.exists():
                raise PatchError(f"{file_patch.old_path}: no such file")
            original = source.read_text(encoding="utf-8", errors="replace")
        result.append((target, apply_to_text(original, file_patch), file_patch))
    return result


def apply(patch: str, root: str | os.PathLike) -> str:
    planned = plan(patch, root)
    summary = []
    for target, text, file_patch in planned:
        added = sum(1 for h in file_patch.hunks for op, _ in h.lines if op == "+")
        removed = sum(1 for h in file_patch.hunks for op, _ in h.lines if op == "-")
        rel = os.path.relpath(target, Path(root).resolve())
        if text is None:
            target.unlink()
            _invalidate_pyc(target)
            summary.append(f"deleted {rel}")
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        tmp = target.with_name(target.name + ".cordata-tmp")
        tmp.write_text(text, encoding="utf-8")
        os.replace(tmp, target)
        _invalidate_pyc(target)
        if file_patch.old_path is not None and file_patch.new_path and file_patch.old_path != file_patch.new_path:
            old = Path(root).resolve() / file_patch.old_path
            if old.exists() and old.resolve() != target:
                old.unlink()
                _invalidate_pyc(old)
        summary.append(f"{'created' if file_patch.old_path is None else 'patched'} {rel} (+{added} -{removed})")
    return "\n".join(summary)


def _invalidate_pyc(written: Path) -> None:
    """Drop the .pyc caches of a patched .py file, right away.

    Without this, a re-run of the failing command *can* still execute stale
    byte-code and fail again — or worse, a self-heal verification can pass
    against stale code. Why "mtime" alone doesn't save us: a .pyc header
    records the source file's mtime as whole seconds, and a patch rewritten in
    the same wall-clock second (a failing command just ran, the fix applies,
    the command re-runs — all within one second, observed live in
    _self_heal_smoke_test.py) leaves the stale .pyc "valid", so `import calc`
    runs the pre-patch byte-code no matter what the .py now says. Deleting the
    .pyc is the only honest invalidation.
    """
    if written.suffix != ".py":
        return
    cache = written.parent / "__pycache__"
    try:
        for pyc in cache.glob(written.stem + "*.pyc"):
            pyc.unlink(missing_ok=True)
    except OSError:  # best effort: a locked cache dir must not fail the patch
        pass


@tool
def apply_patch(patch: str) -> str:
    """Apply a unified diff (``git diff`` format: ---/+++ headers, @@ hunks) to
    files in the project. All hunks apply or nothing changes. Prefer this over
    rewriting whole files for multi-line or multi-file fixes.

    Args:
        patch: The unified diff text.
    """
    try:
        return apply(patch, os.getcwd())
    except PatchError as error:
        raise ValueError(str(error)) from error

"""Large files: an outline instead of the whole file, and one symbol at a time.

A whole-file read of ``acp_server.py`` (2,800 lines) is ~30k tokens in the conversation for the one function the
agent wanted. Two pieces stop that:

* **``code_outline``** — a file's classes, functions and methods with their line ranges, or, given ``symbol``, the
  source of just that one (``Class.method`` for a method). Python is read with ``ast`` (exact ranges); other
  languages with a structural scan — declarations by keyword (Rust, Go, JS/TS, Java/Kotlin/C#/Swift, C/C++, Ruby,
  shell, Markdown headings) and the block's end by brace depth (or indentation, or the next heading). If the optional
  ``tree_sitter_language_pack`` is installed it is used for the non-Python languages instead.
* **``LargeFileHooks``** — when ``file_read`` (mode ``view``) or the ``editor``'s ``view`` reads a file over
  ``context.outlineLines`` lines (default 500) with no range, the result handed to the model is the outline plus how
  to read what it needs (a line range, a symbol, ``code_search``). Ordered ahead of every other ``AfterToolCallEvent``
  hook (the same ``HookOrder`` as ``core_tools``), so the offloader and the diet see the outline, not the file. A
  read *with* a range, a small file, or any other mode is untouched.

Read-only throughout; nothing here writes a file or runs a command.
"""

from __future__ import annotations

import ast
import logging
import os
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

from strands import tool
from strands.hooks import AfterToolCallEvent, HookProvider, HookRegistry

logger = logging.getLogger(__name__)

DEFAULT_OUTLINE_LINES = 500
#: A symbol longer than this is returned as its head plus how to read the rest by range.
MAX_SYMBOL_LINES = 400
MAX_OUTLINE_ROWS = 300
MAX_FILE_BYTES = 4 * 1024 * 1024


@dataclass(frozen=True)
class Symbol:
    kind: str  # class | function | method | struct | enum | trait | impl | interface | type | module | heading | …
    name: str  # dotted for nested: Class.method
    start: int  # 1-based, inclusive
    end: int  # 1-based, inclusive
    depth: int
    signature: str


# ------------------------------------------------------------------------------------------------- python


def _python_symbols(text: str) -> list[Symbol]:
    tree = ast.parse(text)
    lines = text.splitlines()
    out: list[Symbol] = []

    def visit(node: ast.AST, prefix: str, depth: int, in_class: bool) -> None:
        for child in ast.iter_child_nodes(node):
            if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                start = min([child.lineno, *(d.lineno for d in child.decorator_list)])
                end = getattr(child, "end_lineno", None) or child.lineno
                name = f"{prefix}{child.name}"
                kind = "class" if isinstance(child, ast.ClassDef) else "method" if in_class else "function"
                signature = lines[child.lineno - 1].strip() if child.lineno - 1 < len(lines) else name
                out.append(Symbol(kind, name, start, end, depth, signature[:160]))
                visit(child, f"{name}.", depth + 1, isinstance(child, ast.ClassDef))

    visit(tree, "", 0, False)
    return out


# ------------------------------------------------------------------------------------------------- other languages

_DECLS: dict[str, list[tuple[str, re.Pattern[str]]]] = {}


def _decl(*pairs: tuple[str, str]) -> list[tuple[str, re.Pattern[str]]]:
    return [(kind, re.compile(pattern)) for kind, pattern in pairs]


_RUST = _decl(
    ("function", r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+|async\s+|unsafe\s+|extern\s+\"[^\"]*\"\s+)*fn\s+(\w+)"),
    ("struct", r"^\s*(?:pub(?:\([^)]*\))?\s+)?struct\s+(\w+)"),
    ("enum", r"^\s*(?:pub(?:\([^)]*\))?\s+)?enum\s+(\w+)"),
    ("trait", r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?trait\s+(\w+)"),
    ("impl", r"^\s*(?:unsafe\s+)?impl(?:<[^>]*>)?\s+(?:[\w:<>, ]+\s+for\s+)?([\w:]+)"),
    ("module", r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*\{"),
    ("macro", r"^\s*macro_rules!\s*(\w+)"),
)
_GO = _decl(("function", r"^func\s+(?:\([^)]*\)\s*)?(\w+)"), ("type", r"^type\s+(\w+)"))
_JS = _decl(
    ("class", r"^\s*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+(\w+)"),
    ("function", r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s*\*?\s*(\w+)"),
    ("interface", r"^\s*(?:export\s+)?interface\s+(\w+)"),
    ("type", r"^\s*(?:export\s+)?type\s+(\w+)\s*(?:<[^=]*>)?\s*="),
    ("enum", r"^\s*(?:export\s+)?(?:const\s+)?enum\s+(\w+)"),
    ("function", r"^\s*(?:export\s+)?(?:const|let|var)\s+(\w+)\s*(?::[^=]+)?=\s*(?:async\s+)?(?:\([^)]*\)|\w+)\s*(?::[^=]+)?=>"),
    ("method", r"^\s+(?:public\s+|private\s+|protected\s+|static\s+|async\s+|readonly\s+|get\s+|set\s+)*(\w+)\s*\([^)]*\)\s*(?::[^{]+)?\{\s*$"),
)
_JAVA = _decl(
    ("class", r"^\s*(?:(?:public|private|protected|internal|static|final|abstract|sealed|open|data|partial)\s+)*(?:class|record|object)\s+(\w+)"),
    ("interface", r"^\s*(?:(?:public|private|protected|internal)\s+)*interface\s+(\w+)"),
    ("enum", r"^\s*(?:(?:public|private|protected|internal)\s+)*enum\s+(?:class\s+)?(\w+)"),
    ("function", r"^\s*(?:(?:public|private|protected|internal|static|final|abstract|override|suspend|async|virtual)\s+)*fun\s+(\w+)"),
    ("method", r"^\s*(?:(?:public|private|protected|internal|static|final|abstract|override|synchronized|async|virtual)\s+)+[\w<>\[\], ?]+\s+(\w+)\s*\([^;]*$"),
    ("function", r"^\s*(?:(?:public|private|fileprivate|internal|static|override|mutating)\s+)*func\s+(\w+)"),
    ("struct", r"^\s*(?:(?:public|private|internal)\s+)*struct\s+(\w+)"),
)
_C = _decl(
    ("struct", r"^\s*(?:typedef\s+)?struct\s+(\w+)\s*\{"),
    ("class", r"^\s*class\s+(\w+)[^;]*$"),
    ("function", r"^[A-Za-z_][\w\s\*&:<>,]*?\b(\w+)\s*\([^;]*\)\s*(?:const\s*)?\{?\s*$"),
)
_RUBY = _decl(("class", r"^\s*class\s+([\w:]+)"), ("module", r"^\s*module\s+([\w:]+)"), ("method", r"^\s*def\s+(?:self\.)?(\w+[?!=]?)"))
_SHELL = _decl(("function", r"^\s*(?:function\s+)?(\w[\w-]*)\s*\(\)\s*\{"), ("function", r"^\s*function\s+(\w[\w-]*)"))
_MARKDOWN = _decl(("heading", r"^(#{1,6})\s+(.+?)\s*#*\s*$"))

LANGUAGES: dict[str, tuple[str, list[tuple[str, re.Pattern[str]]], str]] = {
    # suffix -> (language, declarations, how a block ends: braces | ruby | heading)
    ".rs": ("rust", _RUST, "braces"),
    ".go": ("go", _GO, "braces"),
    **{s: ("typescript", _JS, "braces") for s in (".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs")},
    **{s: ("java", _JAVA, "braces") for s in (".java", ".kt", ".kts", ".cs", ".swift", ".scala")},
    **{s: ("c", _C, "braces") for s in (".c", ".h", ".cc", ".cpp", ".cxx", ".hpp", ".hh", ".m", ".mm")},
    ".rb": ("ruby", _RUBY, "ruby"),
    **{s: ("shell", _SHELL, "braces") for s in (".sh", ".bash", ".zsh")},
    **{s: ("markdown", _MARKDOWN, "heading") for s in (".md", ".markdown", ".mdx")},
}

_KEYWORDS = frozenset("if for while switch return match catch else do try loop when foreach using lock new sizeof".split())


def _strip_strings(line: str) -> str:
    """The line without string/char literals and line comments, for counting braces."""
    line = re.sub(r'"(?:\\.|[^"\\])*"', '""', line)
    line = re.sub(r"'(?:\\.|[^'\\])'", "''", line)
    return re.sub(r"//.*$", "", line)


def _brace_end(lines: list[str], start: int) -> int:
    """0-based start → 0-based last line of the block that opens on or after it (a declaration with no block ends where it is)."""
    depth, opened = 0, False
    for i in range(start, min(len(lines), start + 20000)):
        clean = _strip_strings(lines[i])
        for ch in clean:
            if ch == "{":
                depth += 1
                opened = True
            elif ch == "}":
                depth -= 1
                if opened and depth <= 0:
                    return i
        if not opened and (clean.rstrip().endswith(";") or (i > start + 3)):
            return i if clean.rstrip().endswith(";") else start
    return len(lines) - 1


def _ruby_end(lines: list[str], start: int) -> int:
    indent = len(lines[start]) - len(lines[start].lstrip())
    for i in range(start + 1, len(lines)):
        stripped = lines[i].strip()
        if stripped == "end" and len(lines[i]) - len(lines[i].lstrip()) == indent:
            return i
    return start


def _scan_symbols(text: str, suffix: str) -> list[Symbol]:
    language = LANGUAGES.get(suffix)
    if language is None:
        return []
    _, decls, ending = language
    lines = text.splitlines()
    found: list[tuple[int, str, str]] = []
    in_block_comment = False
    for i, line in enumerate(lines):
        stripped = line.strip()
        if ending == "braces":
            if in_block_comment:
                if "*/" in stripped:
                    in_block_comment = False
                continue
            if stripped.startswith("/*") and "*/" not in stripped:
                in_block_comment = True
                continue
            if stripped.startswith(("//", "*", "#[", "@")):
                continue
        for kind, pattern in decls:
            match = pattern.match(line)
            if not match:
                continue
            if ending == "heading":
                found.append((i, kind, f"{match.group(1)} {match.group(2)}"))
                break
            name = match.group(1)
            if name in _KEYWORDS:
                break
            found.append((i, kind, name))
            break
    out: list[Symbol] = []
    if ending == "heading":
        for n, (i, kind, title) in enumerate(found):
            level = title.split(" ", 1)[0].count("#")
            end = next((j - 1 for j, _, t in found[n + 1 :] if t.split(" ", 1)[0].count("#") <= level), len(lines) - 1)
            out.append(Symbol(kind, title.split(" ", 1)[1], i + 1, end + 1, level - 1, lines[i].strip()[:160]))
        return out
    stack: list[Symbol] = []
    for i, kind, name in found:
        end = _brace_end(lines, i) if ending == "braces" else _ruby_end(lines, i)
        while stack and stack[-1].end < i + 1:
            stack.pop()
        container = next((s for s in reversed(stack) if s.kind in ("class", "struct", "impl", "trait", "interface", "module", "enum", "object")), None)
        dotted = f"{container.name}.{name}" if container is not None else name
        kind = "method" if container is not None and container.kind != "module" and kind == "function" else kind
        symbol = Symbol(kind, dotted, i + 1, end + 1, len(stack), lines[i].strip()[:160])
        out.append(symbol)
        if end > i:
            stack.append(symbol)
    return out


def _tree_sitter_symbols(text: str, suffix: str) -> list[Symbol] | None:
    """The optional precise path for non-Python languages. None when the package is not installed (the usual case)."""
    try:
        from tree_sitter_language_pack import get_parser  # type: ignore[import-not-found]  # noqa: PLC0415
    except Exception:
        return None
    names = {".rs": "rust", ".go": "go", ".ts": "typescript", ".tsx": "tsx", ".js": "javascript", ".jsx": "javascript", ".java": "java", ".rb": "ruby", ".c": "c", ".cpp": "cpp", ".h": "c", ".cs": "c_sharp", ".kt": "kotlin", ".swift": "swift"}
    language = names.get(suffix)
    if language is None:
        return None
    try:
        tree = get_parser(language).parse(text.encode("utf-8"))
    except Exception:
        return None
    wanted = {
        "function_item": "function", "struct_item": "struct", "enum_item": "enum", "trait_item": "trait", "impl_item": "impl", "mod_item": "module",
        "function_declaration": "function", "method_declaration": "method", "method_definition": "method", "class_declaration": "class",
        "interface_declaration": "interface", "type_alias_declaration": "type", "type_declaration": "type", "enum_declaration": "enum",
        "class_definition": "class", "function_definition": "function", "method": "method", "class": "class", "module": "module",
    }
    lines = text.splitlines()
    out: list[Symbol] = []

    def walk(node: Any, prefix: str, depth: int) -> None:
        for child in node.children:
            kind = wanted.get(child.type)
            if kind is None:
                walk(child, prefix, depth)
                continue
            field = child.child_by_field_name("name") or child.child_by_field_name("type")
            name = field.text.decode("utf-8", "replace") if field is not None else child.type
            start, end = child.start_point[0], child.end_point[0]
            dotted = f"{prefix}{name}"
            out.append(Symbol("method" if prefix and kind == "function" else kind, dotted, start + 1, end + 1, depth, lines[start].strip()[:160] if start < len(lines) else dotted))
            walk(child, f"{dotted}.", depth + 1)

    walk(tree.root_node, "", 0)
    return out


# ------------------------------------------------------------------------------------------------- the API


def read_text(path: str | os.PathLike) -> str | None:
    try:
        target = Path(path)
        if target.stat().st_size > MAX_FILE_BYTES:
            return None
        data = target.read_bytes()
    except OSError:
        return None
    if b"\0" in data[:8192]:
        return None
    return data.decode("utf-8", "replace")


def symbols(path: str | os.PathLike, text: str | None = None) -> list[Symbol]:
    """Every symbol in a file, in order. Empty for a language this cannot read (or a file it cannot parse)."""
    suffix = Path(path).suffix.lower()
    text = read_text(path) if text is None else text
    if text is None:
        return []
    if suffix in (".py", ".pyi"):
        try:
            return _python_symbols(text)
        except SyntaxError:
            return []
    precise = _tree_sitter_symbols(text, suffix)
    if precise:
        return precise
    return _scan_symbols(text, suffix)


def line_count(path: str | os.PathLike) -> int | None:
    try:
        with open(path, "rb") as handle:
            if os.fstat(handle.fileno()).st_size > MAX_FILE_BYTES:
                return None
            return sum(1 for _ in handle)
    except OSError:
        return None


def render_outline(path: str, found: list[Symbol], *, total_lines: int, rel: str | None = None) -> str:
    label = rel or path
    if not found:
        return f"{label}: {total_lines} lines; no classes or functions recognised."
    rows = [f"{label}: {total_lines} lines, {len(found)} symbol(s):"]
    for symbol in found[:MAX_OUTLINE_ROWS]:
        span = f"L{symbol.start}" if symbol.end == symbol.start else f"L{symbol.start}-{symbol.end}"
        rows.append(f"{'  ' * symbol.depth}{span:>12}  {symbol.kind} {symbol.name}")
    if len(found) > MAX_OUTLINE_ROWS:
        rows.append(f"… {len(found) - MAX_OUTLINE_ROWS} more — narrow with code_search(pattern, path={label!r})")
    return "\n".join(rows)


def find_symbol(found: list[Symbol], name: str) -> list[Symbol]:
    """Exact dotted name first, then a unique final component (``method`` for ``Class.method``)."""
    name = name.strip()
    exact = [s for s in found if s.name == name]
    if exact:
        return exact
    return [s for s in found if s.name.rsplit(".", 1)[-1] == name.rsplit(".", 1)[-1] and s.name.endswith(name)]


def symbol_source(path: str, symbol: Symbol, text: str, *, rel: str | None = None) -> str:
    lines = text.splitlines()
    end = min(symbol.end, symbol.start + MAX_SYMBOL_LINES - 1)
    width = len(str(end))
    body = "\n".join(f"{n:>{width}}  {lines[n - 1]}" for n in range(symbol.start, end + 1) if n - 1 < len(lines))
    head = f"{rel or path} — {symbol.kind} {symbol.name}, lines {symbol.start}-{symbol.end}:"
    tail = ""
    if end < symbol.end:
        tail = f"\n[{symbol.end - end} more line(s): file_read(path, mode='lines', start_line={end}, end_line={symbol.end}) for the rest]"
    return f"{head}\n{body}{tail}"


_ROOT: str | None = None


def configure(root: str | None) -> None:
    global _ROOT
    _ROOT = root


def _resolve(path: str) -> Path:
    target = Path(os.path.expanduser(path))
    return target if target.is_absolute() else Path(_ROOT or os.getcwd()) / target


def _relative(path: Path) -> str:
    root = Path(_ROOT or os.getcwd()).resolve()
    try:
        return str(path.resolve().relative_to(root))
    except ValueError:
        return str(path)


@tool
def code_outline(path: str, symbol: str = "") -> str:
    """A file's classes, functions and methods with their line ranges — or, given `symbol`, just that symbol's source.

    Use this instead of reading a whole large file: outline first, then read the one function or class you need
    (`symbol="ClassName.method"` or `symbol="function_name"`). Python, Rust, Go, JS/TS, Java/Kotlin/C#/Swift, C/C++,
    Ruby, shell and Markdown. Read-only; never asks.

    Args:
        path: The file, relative to the project or absolute.
        symbol: Optional. A name from the outline (dotted for a method: "Agent.run"). Empty returns the outline.
    """
    target = _resolve(path)
    text = read_text(target)
    if text is None:
        return f"code_outline: cannot read {path!r} (missing, binary, or over {MAX_FILE_BYTES // (1024 * 1024)} MB)."
    found = symbols(target, text)
    rel = _relative(target)
    total = len(text.splitlines())
    if not symbol.strip():
        return render_outline(str(target), found, total_lines=total, rel=rel)
    matches = find_symbol(found, symbol)
    if not matches:
        near = [s.name for s in found if symbol.lower().rsplit(".", 1)[-1] in s.name.lower()][:8]
        hint = f" Close: {', '.join(near)}." if near else " code_outline(path) lists what is there."
        return f"code_outline: no symbol {symbol!r} in {rel}.{hint}"
    if len(matches) > 1:
        return f"code_outline: {symbol!r} is ambiguous in {rel}: " + ", ".join(f"{s.name} (L{s.start})" for s in matches[:10]) + ". Use the dotted name."
    return symbol_source(str(target), matches[0], text, rel=rel)


# ------------------------------------------------------------------------------------------------- the read hook


def _whole_file_read(tool_name: str, tool_input: Any) -> str | None:
    """The path, when this call reads one whole file (no range) — the only shape the hook replaces."""
    if not isinstance(tool_input, dict):
        return None
    raw = tool_input.get("path")
    if not isinstance(raw, str) or not raw or any(ch in raw for ch in "*?,"):
        return None
    if tool_name == "file_read":
        return raw if str(tool_input.get("mode") or "view") == "view" else None
    if tool_name == "editor":
        return raw if tool_input.get("command") == "view" and not tool_input.get("view_range") else None
    return None


def outline_note(path: Path, total: int, threshold: int, rel: str) -> str:
    found = symbols(path)
    outline = render_outline(str(path), found, total_lines=total, rel=rel)
    return (
        f"[{rel} is {total:,} lines — over the {threshold:,}-line limit for reading a whole file (context.outlineLines), so here is its "
        f"outline instead of its text. Read only what you need: code_outline(path, symbol=\"Name\") for one class or function, "
        f"file_read(path, mode=\"lines\", start_line=N, end_line=M) for a range, or code_search(pattern, path) to find a line.]\n\n{outline}"
    )


class LargeFileHooks(HookProvider):
    """Replace a whole-file read of a long file with its outline (module docstring). One per agent."""

    def __init__(self, threshold: Callable[[], int] | int = DEFAULT_OUTLINE_LINES, root: str | None = None) -> None:
        self._threshold = threshold if callable(threshold) else (lambda: int(threshold))
        self._root = root

    def register_hooks(self, registry: HookRegistry, **kwargs: Any) -> None:
        from core_tools import SHAPE_ORDER  # noqa: PLC0415 — the same slot as the shell's result shaping: ahead of the offloader

        registry.add_callback(AfterToolCallEvent, self._after, order=SHAPE_ORDER)

    def _after(self, event: AfterToolCallEvent) -> None:
        try:
            name = str(event.tool_use.get("name") or "")
            raw = _whole_file_read(name, event.tool_use.get("input"))
            if raw is None or not isinstance(event.result, dict) or event.result.get("status") != "success":
                return
            target = Path(os.path.expanduser(raw))
            if not target.is_absolute():
                target = Path(self._root or os.getcwd()) / target
            if target.suffix.lower() in (".json", ".csv", ".tsv", ".lock", ".txt", ".log", ".jsonl"):
                return  # data, not code: an outline has nothing to say about it, and the diet already caps it
            threshold = max(100, int(self._threshold() or DEFAULT_OUTLINE_LINES))
            total = line_count(target)
            if total is None or total <= threshold:
                return
            root = Path(self._root or os.getcwd()).resolve()
            try:
                rel = str(target.resolve().relative_to(root))
            except ValueError:
                rel = str(target)
            event.result = {**event.result, "content": [{"text": outline_note(target, total, threshold, rel)}]}
        except Exception:
            logger.debug("code_nav: could not outline a large read", exc_info=True)


def threshold_from(preferences: Callable[[], Any]) -> Callable[[], int]:
    def read() -> int:
        try:
            return int(float(preferences().get("context.outlineLines", DEFAULT_OUTLINE_LINES)))
        except Exception:
            return DEFAULT_OUTLINE_LINES

    return read

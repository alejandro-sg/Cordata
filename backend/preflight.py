"""Preflight: an edit is checked before anyone is asked about it, and before it lands.

Without this, an agent's broken edit was the person's problem: the approval card showed a diff that did not parse,
or — in ``auto`` mode — the edit landed and the next command failed. Preflight runs *in front of* the approval gate
(``acp_server._ApprovalHandler.before_tool_call``) for every ``apply_patch``, ``file_write`` and ``editor`` change by
every agent that shares the session's gate (the orchestrator, the Debugger and the Architect), and the self-healing
loop runs the same static checks on the patches it verifies (``self_heal.py``).

What is checked, cheapest first, each against the file **as it is now**, so only what the edit *newly* breaks counts
(a file that already had lint errors can still be edited):

1. **Syntax** — in process, nothing run: Python (``compile``), JSON, TOML, YAML.
2. **The project's linter** — ``ruff check`` on the proposed text through stdin with ``--stdin-filename``, so the
   project's own ruff configuration applies (a project without one gets ruff's classic defaults, pinned, so the verdict
   does not drift with the ruff version on ``PATH``). Ruff reads code; it never runs it. Only a ``ruff`` on ``PATH`` is used — a
   repository's own ``.venv/bin/ruff`` would be the repository's program, and only a trusted folder's is.
3. **The language server** — the proposed text is handed to the already-running server as an unsaved change
   (``lsp_bridge.LspClient.check_text``): pyright's or rust-analyzer's *errors*, without writing a file, without a save
   (so no ``cargo check``, nothing executes). Skipped when no server is running for the file — a check never waits
   for a server to start.
4. **Tests** (``preflight.tests``, off by default, trust-gated because it runs the project's code) — the command runs in
   a throwaway copy of the project with the edit applied (``self_heal.ShadowWorkspace``). A failure that the copy
   *without* the edit shows too is not the edit's.

A check that cannot run (no ruff, no server, a timeout) is reported as skipped, never as a pass or a failure.

The loop. A failing edit is not applied and not put to a person: the agent gets the new problems as the call's result
(``Deny``) and makes another attempt — up to ``preflight.maxRetries`` (3) per file per turn. When it runs out, the turn
is told to stop and report, any further edit that turn is refused, and — with ``preflight.rollback`` (on) and
checkpoints on — the files the turn changed are put back as they were before it (``checkpoints.undo_turn``; the
present is checkpointed first, so the rollback itself can be undone). Nothing here commits to the person's own git.
"""

from __future__ import annotations

import collections
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
from typing import Any

import edit_preview
import harness_events

logger = logging.getLogger(__name__)

EDIT_TOOLS = frozenset({"apply_patch", "file_write", "editor"})
_EDITOR_WRITES = frozenset({"create", "str_replace", "insert"})

DEFAULT_RETRIES = 3
LINT_TIMEOUT = 15.0
LSP_TIMEOUT = 4.0
TEST_TIMEOUT = 300.0
MAX_PROBLEMS = 12
_CACHE_MAX = 64


@dataclass(frozen=True)
class Problem:
    check: str  # syntax | ruff | lsp | tests
    path: str  # relative to the project
    line: int | None
    message: str

    def render(self) -> str:
        where = f"{self.path}:{self.line}" if self.line else self.path
        return f"{where}: [{self.check}] {self.message}"


@dataclass
class Report:
    problems: list[Problem] = field(default_factory=list)
    ran: list[str] = field(default_factory=list)
    skipped: list[str] = field(default_factory=list)
    files: list[str] = field(default_factory=list)
    notes: list[Problem] = field(default_factory=list)  # found, but about other files: reported, never a refusal
    elapsed: float = 0.0
    cached: bool = False  # answered from the cache: the call re-entered after an approval pause

    @property
    def ok(self) -> bool:
        return not self.problems

    def to_json(self) -> dict[str, Any]:
        return {"ok": self.ok, "problems": [p.render() for p in self.problems[:MAX_PROBLEMS]], "notes": [p.render() for p in self.notes[:MAX_PROBLEMS]], "ran": self.ran, "skipped": self.skipped, "files": self.files, "elapsed": round(self.elapsed, 2)}


def is_edit(tool_name: str, tool_input: Any) -> bool:
    if tool_name not in EDIT_TOOLS or not isinstance(tool_input, dict):
        return False
    return tool_name != "editor" or tool_input.get("command") in _EDITOR_WRITES


#: Suffixes the in-process and linter checks can read; anything else is checked only by a language server or tests.
CHECKABLE = frozenset({".py", ".pyi", ".json", ".toml", ".yaml", ".yml"})


def applicable(tool_name: str, tool_input: Any, manager: Any = None, tests: str = "") -> bool:
    """Would any check have something to say about this edit? Decided from the paths alone (no file is read), so a
    README or a CSS edit takes the gate's ordinary synchronous path."""
    if tests.strip():
        return True
    import lsp_bridge  # noqa: PLC0415

    for raw in lsp_bridge.edited_paths(tool_name, tool_input):
        suffix = Path(raw).suffix.lower()
        if suffix in CHECKABLE:
            return True
        if manager is not None and getattr(manager, "enabled", False) and manager.spec_for(raw) is not None:
            return True
    return False


def _rel(path: str, root: str) -> str:
    """``path`` relative to the project, both resolved (``/var`` is ``/private/var`` on macOS), else as given."""
    try:
        if not os.path.isabs(path):
            return path
        resolved, base = Path(path).resolve(), Path(root).resolve()
        return str(resolved.relative_to(base)) if resolved.is_relative_to(base) else path
    except (ValueError, OSError):
        return path


# ------------------------------------------------------------------------------------------------- 1. syntax


def _syntax_error(path: str, text: str) -> tuple[int | None, str] | None:
    suffix = Path(path).suffix.lower()
    try:
        if suffix in (".py", ".pyi"):
            compile(text, path, "exec", dont_inherit=True)
        elif suffix == ".json":
            json.loads(text)
        elif suffix == ".toml":
            import tomllib

            tomllib.loads(text)
        elif suffix in (".yaml", ".yml"):
            try:
                import yaml  # noqa: PLC0415 — optional
            except ImportError:
                return None
            list(yaml.safe_load_all(text))
        else:
            return None
    except SyntaxError as error:
        return error.lineno, f"{error.msg}" + (f": {error.text.strip()[:80]}" if error.text else "")
    except json.JSONDecodeError as error:
        return error.lineno, error.msg
    except ValueError as error:  # tomllib.TOMLDecodeError
        match = re.search(r"line (\d+)", str(error))
        return (int(match.group(1)) if match else None), str(error).split("\n")[0][:200]
    except Exception as error:  # yaml.YAMLError and friends
        mark = getattr(error, "problem_mark", None)
        return (mark.line + 1 if mark is not None else None), str(getattr(error, "problem", None) or error).split("\n")[0][:200]
    return None


def check_syntax(path: str, old: str | None, new: str, root: str) -> list[Problem]:
    found = _syntax_error(path, new)
    if found is None:
        return []
    if old is not None and _syntax_error(path, old) is not None:
        return []  # it did not parse before either: not this edit's doing
    line, message = found
    return [Problem("syntax", _rel(path, root), line, message)]


# ------------------------------------------------------------------------------------------------- 2. ruff


def ruff_binary(root: str, trusted: bool = False) -> str | None:
    if os.environ.get("CORDATA_PREFLIGHT_RUFF", "").strip().lower() in ("off", "0", "false", "no"):
        return None
    found = shutil.which("ruff")
    if found:
        return found
    if trusted:
        for candidate in (Path(root) / ".venv" / "bin" / "ruff", Path(root) / "venv" / "bin" / "ruff"):
            if candidate.is_file() and os.access(candidate, os.X_OK):
                return str(candidate)
    return None


#: Ruff's long-standing default rule set. Pinned for a project with no ruff configuration of its own: newer ruff releases
#: widen their defaults (0.16 added ``BLE001`` blind-except, among others), and a project's verdict must not change with
#: the version of ruff that happens to be on ``PATH``.
DEFAULT_RUFF_RULES = "E4,E7,E9,F"


def has_ruff_config(root: str) -> bool:
    base = Path(root)
    if (base / "ruff.toml").is_file() or (base / ".ruff.toml").is_file():
        return True
    try:
        return "[tool.ruff" in (base / "pyproject.toml").read_text(encoding="utf-8", errors="ignore")
    except OSError:
        return False


def _ruff(binary: str, root: str, path: str, text: str) -> list[dict] | None:
    pinned = [] if has_ruff_config(root) else ["--select", DEFAULT_RUFF_RULES]
    try:
        done = subprocess.run(
            [binary, "check", "--stdin-filename", path, "--output-format", "json", "--no-cache", "--force-exclude", "--quiet", *pinned, "-"],
            input=text, cwd=root, capture_output=True, text=True, timeout=LINT_TIMEOUT,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if done.returncode not in (0, 1):
        return None
    try:
        rows = json.loads(done.stdout or "[]")
    except ValueError:
        return None
    return rows if isinstance(rows, list) else None


def check_ruff(path: str, old: str | None, new: str, root: str, binary: str) -> list[Problem] | None:
    if Path(path).suffix.lower() not in (".py", ".pyi"):
        return []
    after = _ruff(binary, root, path, new)
    if after is None:
        return None
    before = _ruff(binary, root, path, old) if old is not None else []
    if before is None:
        before = []
    remaining = collections.Counter((row.get("code"), row.get("message")) for row in before)
    problems = []
    for row in after:
        key = (row.get("code"), row.get("message"))
        if remaining[key] > 0:
            remaining[key] -= 1
            continue
        location = row.get("location") or {}
        problems.append(Problem("ruff", _rel(path, root), location.get("row"), f"{row.get('code')} {row.get('message')}"))
    return problems


# ------------------------------------------------------------------------------------------------- 3. language server


def _errors(diagnostics: list) -> list[dict]:
    return [d for d in diagnostics if d.get("severity", 1) == 1]


#: Errors about *another* file — an import that does not resolve, a symbol another module does not (yet) export. They
#: depend on files the agent may be about to edit (create the helper, then import it — or the other way round), so they
#: are reported with the result, never a reason to refuse the edit.
CROSS_FILE = re.compile(
    r"unknown import symbol|could not be resolved|is not exported from|no module named|cannot find module|unresolved[- ](import|module|extern)"
    r"|reportMissingImports|reportMissingModuleSource|reportAttributeAccessIssue|reportPrivateImportUsage|E0432|E0433|E0412",
    re.IGNORECASE,
)


#: Strictness and completeness rules (pyright's "strict" family and rust-analyzer's style lints): the code runs, the type
#: checker just wants it annotated more thoroughly. Reported with the result so the agent can tidy them, but never a
#: reason to refuse an edit — a brand-new file would otherwise be bounced for a missing ``Dict[str, Any]`` argument.
ADVISORY = re.compile(
    r"reportMissingTypeArgument|reportMissingParameterType|reportMissingTypeStubs|reportUnknown\w+|reportUntyped\w+"
    r"|reportUnused\w+|reportPrivateUsage|reportConstantRedefinition|reportMissingSuperCall|reportImplicitOverride"
    r"|reportUnnecessary\w+|reportTypeCommentUsage|reportShadowedImports|reportImportCycles|reportDeprecated"
    r"|Expected type arguments for generic class|Type annotation is missing|Return type is unknown|Type of parameter .* is (unknown|partially unknown)",
    re.IGNORECASE,
)


def _diagnostic_key(diagnostic: dict) -> tuple[str, str]:
    return str(diagnostic.get("code") or ""), " ".join(str(diagnostic.get("message", "")).split())


def check_lsp(changes: list[tuple[str, str | None, str]], root: str, manager: Any) -> tuple[list[Problem], list[Problem], list[str]]:
    """``(problems, notes, skipped)`` for every file of one edit, judged together per language server."""
    problems: list[Problem] = []
    notes: list[Problem] = []
    skipped: list[str] = []
    if manager is None or not getattr(manager, "enabled", False):
        return problems, notes, skipped
    groups: dict[int, tuple[Any, list[tuple[str, str | None, str]]]] = {}
    for change in changes:
        path = change[0]
        if manager.spec_for(path) is None:
            continue
        client = manager.client_for(path, start=False)  # never wait for a server to start
        if client is None:
            skipped.append(f"lsp ({_rel(path, root)}: no running server)")
            continue
        groups.setdefault(id(client), (client, []))[1].append(change)
    for client, items in groups.values():
        try:
            existing = [(path, old) for path, old, _new in items if old is not None]
            before = client.check_texts(existing, LSP_TIMEOUT) if existing else {}
            after = client.check_texts([(path, new) for path, _old, new in items], LSP_TIMEOUT)
        except Exception:
            logger.debug("preflight: language server check failed", exc_info=True)
            skipped.append("lsp (the server did not answer)")
            continue
        for path, _old, _new in items:
            key = str(Path(path).resolve())
            remaining = collections.Counter(_diagnostic_key(d) for d in _errors(before.get(key, [])))
            for diagnostic in _errors(after.get(key, [])):
                code, message = _diagnostic_key(diagnostic)
                if remaining[(code, message)] > 0:
                    remaining[(code, message)] -= 1
                    continue
                line = (diagnostic.get("range") or {}).get("start", {}).get("line")
                problem = Problem("lsp", _rel(path, root), (line + 1) if isinstance(line, int) else None, (f"[{code}] " if code else "") + message[:300])
                (notes if CROSS_FILE.search(f"{code} {message}") or ADVISORY.search(f"{code} {message}") else problems).append(problem)
    return problems, notes, skipped


# ------------------------------------------------------------------------------------------------- 4. tests


def _run_in_copy(root: str, command: str, changes: list[tuple[str, str | None, str]] | None, timeout: float) -> tuple[int, str]:
    from self_heal import ShadowWorkspace  # noqa: PLC0415 — heavy, and only when tests are configured

    with ShadowWorkspace(root) as shadow:
        assert shadow.path is not None
        for path, _old, new in changes or []:
            rel = _rel(path, root)
            if os.path.isabs(rel):
                continue  # outside the project: not part of the copy
            target = shadow.path / rel
            if new == "" and not (Path(root) / rel).exists():
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(new, encoding="utf-8")
        try:
            done = subprocess.run(command, shell=True, cwd=shadow.path, capture_output=True, text=True, timeout=timeout, errors="replace")
        except subprocess.TimeoutExpired:
            return 124, f"timed out after {timeout:.0f}s"
        return done.returncode, (done.stdout or "") + (("\n" + done.stderr) if done.stderr else "")


def check_tests(root: str, command: str, changes: list[tuple[str, str | None, str]], timeout: float = TEST_TIMEOUT) -> list[Problem]:
    code, output = _run_in_copy(root, command, changes, timeout)
    if code == 0:
        return []
    base_code, _ = _run_in_copy(root, command, None, timeout)
    if base_code == code:
        return []  # it fails the same way without the edit: not this edit's doing
    tail = [line for line in output.strip().splitlines() if line.strip()][-8:]
    return [Problem("tests", command, None, f"exit {code} (exit {base_code} without this edit): " + " | ".join(tail)[-900:])]


# ------------------------------------------------------------------------------------------------- the check


_cache: "collections.OrderedDict[str, Report]" = collections.OrderedDict()
_cache_lock = threading.Lock()


def _key(tool_name: str, tool_input: Any, changes: list[tuple[str, str | None, str]], tests: str) -> str:
    digest = hashlib.sha256()
    digest.update(json.dumps([tool_name, tool_input, tests], sort_keys=True, default=str).encode())
    for path, old, _new in changes:
        digest.update(path.encode())
        digest.update(hashlib.sha256((old or "").encode()).digest())
    return digest.hexdigest()


def check(
    root: str,
    tool_name: str,
    tool_input: Any,
    *,
    manager: Any = None,
    tests: str = "",
    trusted: bool = False,
    lsp: bool = True,
) -> Report:
    """Everything this edit newly breaks. Cached on (call, the files' current contents), so a call re-entered after an
    approval pause is not checked twice."""
    started = time.monotonic()
    changes = [c for c in edit_preview.preview(tool_name, tool_input, root) if isinstance(c[2], str) and c[2] != ""]
    if not changes:
        return Report(skipped=["nothing to preview (the tool will report its own error)"])
    key = _key(tool_name, tool_input, changes, tests)
    with _cache_lock:
        if key in _cache:
            _cache.move_to_end(key)
            hit = _cache[key]
            return Report(hit.problems, hit.ran, hit.skipped, hit.files, hit.notes, hit.elapsed, cached=True)
    report = Report(files=[_rel(path, root) for path, _, _ in changes])
    ruff = ruff_binary(root, trusted)
    for path, old, new in changes:
        report.problems += check_syntax(path, old, new, root)
    report.ran.append("syntax")
    if not report.problems:
        if ruff is None:
            report.skipped.append("ruff (not installed)")
        else:
            for path, old, new in changes:
                found = check_ruff(path, old, new, root, ruff)
                if found is None:
                    report.skipped.append(f"ruff ({_rel(path, root)})")
                else:
                    report.problems += found
            report.ran.append("ruff")
        if lsp and manager is not None:
            found, notes, skipped = check_lsp(changes, root, manager)
            report.problems += found
            report.notes += notes
            report.skipped += skipped
            report.ran.append("lsp")
    if not report.problems and tests.strip():
        report.problems += check_tests(root, tests.strip(), changes)
        report.ran.append("tests")
    report.elapsed = time.monotonic() - started
    with _cache_lock:
        _cache[key] = report
        while len(_cache) > _CACHE_MAX:
            _cache.popitem(last=False)
    return report


# ------------------------------------------------------------------------------------------------- the loop


class Tracker:
    """Failed preflights per file per turn, and whether the turn has given up. One per session."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._turn = ""
        self._failures: collections.Counter[str] = collections.Counter()
        self.gave_up: str | None = None  # the refusal every later edit this turn gets

    def _for(self, turn: str) -> None:
        if turn != self._turn:
            self._turn, self._failures, self.gave_up = turn, collections.Counter(), None

    def failed(self, turn: str, paths: list[str]) -> int:
        with self._lock:
            self._for(turn)
            for path in paths or ["?"]:
                self._failures[path] += 1
            return max(self._failures[path] for path in paths or ["?"])

    def passed(self, turn: str, paths: list[str]) -> None:
        with self._lock:
            self._for(turn)
            for path in paths:
                self._failures.pop(path, None)

    def stopped(self, turn: str) -> str | None:
        with self._lock:
            self._for(turn)
            return self.gave_up

    def give_up(self, turn: str, refusal: str) -> None:
        with self._lock:
            self._for(turn)
            self.gave_up = refusal


_trackers: dict[str, Tracker] = {}
_trackers_lock = threading.Lock()


def tracker(session_id: str) -> Tracker:
    with _trackers_lock:
        return _trackers.setdefault(session_id or "", Tracker())


def failure_text(tool_name: str, report: Report, attempt: int, retries: int) -> str:
    shown = report.problems[:MAX_PROBLEMS]
    more = f"\n  … {len(report.problems) - len(shown)} more" if len(report.problems) > len(shown) else ""
    return (
        f"[preflight] {tool_name} was not applied: the edit introduces {len(report.problems)} problem(s) that "
        f"{', '.join(report.files) or 'the file'} did not have before (checks: {', '.join(report.ran)}):\n"
        + "\n".join(f"  {p.render()}" for p in shown)
        + more
        + f"\nFix the edit and make the call again ({retries - attempt + 1} attempt(s) left). Nothing was written and nobody was asked."
    )


def rollback(root: str, turn: str, session_id: str) -> dict[str, Any]:
    """Put back what this turn changed (checkpoints.undo_turn). Never raises."""
    try:
        import checkpoints  # noqa: PLC0415

        result = checkpoints.undo_turn(root, turn, session_id=session_id)
    except Exception as error:
        logger.debug("preflight: rollback failed", exc_info=True)
        return {"ok": False, "reason": str(error)[:200]}
    return result


def give_up_text(tool_name: str, report: Report, retries: int, rolled: dict[str, Any] | None) -> str:
    problems = "\n".join(f"  {p.render()}" for p in report.problems[:MAX_PROBLEMS])
    if rolled and rolled.get("ok") and rolled.get("count"):
        files = ", ".join(f["path"] for f in rolled.get("files", [])[:12])
        back = (
            f"\nThe {rolled['count']} file(s) this turn had changed were put back as they were before it ({files}); "
            f"the state before that rollback is checkpoint {str(rolled.get('undo_with') or '')[:12]} if the user wants it back."
        )
    elif rolled and rolled.get("ok"):
        back = "\nThis turn had not changed any file, so there was nothing to roll back."
    elif rolled is not None:
        back = f"\nRolling the turn back did not work ({rolled.get('reason') or 'unknown'}); the files are as the turn left them."
    else:
        back = ""
    return (
        f"[preflight] {tool_name} was not applied, and this is the last attempt: the edit has failed its checks "
        f"{retries + 1} times.\n{problems}{back}\n"
        "Stop editing. Tell the user what you were trying to do, what the checks still report, and what you would try next."
    )


def announce(phase: str, session_id: str | None, tool_name: str, report: Report | None = None, **extra: Any) -> None:
    payload = {"event": "preflight", "phase": phase, "tool": tool_name, **(report.to_json() if report else {}), **extra}
    harness_events.emit("preflight", payload)
    try:
        import state_store  # noqa: PLC0415

        state_store.append_event("preflight", payload, session_id=session_id, source="preflight")
    except Exception:
        logger.debug("preflight: could not record the event", exc_info=True)

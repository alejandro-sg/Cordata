"""Language-server bridge: deterministic workspace context for the LLM.

The harness detects which language servers fit the project (by root markers and
file extensions, and only if the binary is installed), starts them lazily, and
uses them two ways:

* **tools** — ``lsp_diagnostics``, ``lsp_definition``, ``lsp_references``,
  ``lsp_hover``, ``lsp_symbols``: compiler-grade answers instead of grep guesses.
* **after every edit** (``LspEditHooks``) — the edited file is re-synced to its
  server and the diagnostics that come back are appended to the edit's tool
  result, so the model sees "this edit introduced error E0308 at 12:5" in the
  same step, the way OpenCode feeds LSP errors back.

Server work-done progress (``$/progress``: rust-analyzer's indexing, cargo
check) becomes harness tasks — the TUI's progress bars.

The client is a small JSON-RPC 2.0 implementation over the server's stdio
(``Content-Length`` framing), one reader thread per server. Positions in the
tools are 1-based (what humans and models read); LSP's are 0-based.
"""

from __future__ import annotations

import asyncio
import itertools
import json
import logging
import os
import shutil
import subprocess
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any
from urllib.parse import unquote, urlparse

from strands import tool
from strands.hooks import AfterToolCallEvent, HookProvider, HookRegistry

import harness_events

logger = logging.getLogger(__name__)

_IGNORED_DIRS = {".git", "node_modules", "target", ".venv", "venv", "__pycache__", "dist", "build", ".next"}


@dataclass(frozen=True)
class ServerSpec:
    name: str
    commands: tuple[tuple[str, ...], ...]  # first installed one wins
    extensions: tuple[str, ...]
    markers: tuple[str, ...]
    language_ids: dict = field(default_factory=dict)  # extension -> languageId

    def language_id(self, path: str) -> str:
        return self.language_ids.get(Path(path).suffix, self.name)


SERVERS: tuple[ServerSpec, ...] = (
    ServerSpec("rust", (("rust-analyzer",),), (".rs",), ("Cargo.toml",), {".rs": "rust"}),
    ServerSpec(
        "python",
        (("basedpyright-langserver", "--stdio"), ("pyright-langserver", "--stdio"), ("pylsp",)),
        (".py",),
        ("pyproject.toml", "setup.py", "setup.cfg", "requirements.txt"),
        {".py": "python"},
    ),
    ServerSpec(
        "typescript",
        (("typescript-language-server", "--stdio"),),
        (".ts", ".tsx", ".js", ".jsx"),
        ("tsconfig.json", "package.json"),
        {".ts": "typescript", ".tsx": "typescriptreact", ".js": "javascript", ".jsx": "javascriptreact"},
    ),
    ServerSpec("go", (("gopls",),), (".go",), ("go.mod",), {".go": "go"}),
    ServerSpec(
        "c",
        (("clangd",),),
        (".c", ".h", ".cc", ".cpp", ".hpp", ".cxx"),
        ("compile_commands.json", "CMakeLists.txt", ".clangd"),
        {".c": "c", ".h": "c", ".cc": "cpp", ".cpp": "cpp", ".hpp": "cpp", ".cxx": "cpp"},
    ),
)


def _scan_extensions(root: Path, limit: int = 4000) -> set[str]:
    """Extensions present under ``root`` (bounded walk, vendored dirs skipped)."""
    seen: set[str] = set()
    count = 0
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in _IGNORED_DIRS and not d.startswith(".")]
        for filename in filenames:
            seen.add(Path(filename).suffix)
            count += 1
            if count >= limit:
                return seen
    return seen


def _editor_dirs() -> list[Path]:
    """Where editors keep the language servers they manage — reusing them means
    the harness gets the same rust-analyzer / basedpyright the user's Zed or VS
    Code already runs, with nothing extra to install."""
    home = Path.home()
    return [
        home / "Library" / "Application Support" / "Zed" / "languages",  # Zed, macOS
        home / ".local" / "share" / "zed" / "languages",  # Zed, Linux
    ]


def _editor_candidates(name: str) -> list[Path]:
    found: list[Path] = []
    for base in _editor_dirs():
        # Zed: languages/<server>/<server>-<version>[/...]/<server>, or a node package's .bin
        found += sorted(base.glob(f"{name}/{name}-*"), reverse=True)
        found += sorted(base.glob(f"*/node_modules/.bin/{name}"))
        found += sorted(base.glob(f"{name}/{name}-*/*/{name}"), reverse=True)
    for ext in sorted((Path.home() / ".vscode" / "extensions").glob("rust-lang.rust-analyzer-*"), reverse=True):
        if name == "rust-analyzer":
            found.append(ext / "server" / "rust-analyzer")
    return [p for p in found if p.is_file() and os.access(p, os.X_OK) and not p.name.endswith(".metadata")]


_probe_cache: dict[str, bool] = {}


def _works(path: str) -> bool:
    """A binary on PATH can be a stub: rustup installs a ``rust-analyzer`` proxy
    that only says "Unknown binary" until the component is added. Ask it for its
    version (a server that instead waits on stdin counts as working)."""
    if path in _probe_cache:
        return _probe_cache[path]
    try:
        done = subprocess.run([path, "--version"], capture_output=True, text=True, timeout=5, stdin=subprocess.DEVNULL)
        output = (done.stdout + done.stderr).lower()
        ok = done.returncode == 0 or not any(s in output for s in ("unknown binary", "not installed", "toolchain"))
    except subprocess.TimeoutExpired:
        ok = True
    except OSError:
        ok = False
    _probe_cache[path] = ok
    return ok


def locate(name: str, which=shutil.which) -> str | None:
    """Absolute path of a working ``name`` server: PATH first, then editor-managed copies."""
    on_path = which(name)
    if on_path and (which is not shutil.which or _works(on_path)):
        return on_path
    if which is not shutil.which:
        return None  # tests inject `which`; don't look at this machine's editors
    return next((str(p) for p in _editor_candidates(name) if _works(str(p))), None)


def detect(root: str | os.PathLike, which=shutil.which) -> list[tuple[ServerSpec, tuple[str, ...] | None]]:
    """Language servers relevant to ``root``: (spec, command or None if not installed)."""
    root = Path(root)
    extensions = _scan_extensions(root)
    found = []
    for spec in SERVERS:
        marker = any((root / m).exists() for m in spec.markers)
        if not marker and not extensions.intersection(spec.extensions):
            continue
        command = None
        for candidate in spec.commands:
            path = locate(candidate[0], which)
            if path:
                command = (path, *candidate[1:])
                break
        found.append((spec, command))
    return found


def path_to_uri(path: str | os.PathLike) -> str:
    return Path(path).resolve().as_uri()


def uri_to_path(uri: str) -> str:
    return unquote(urlparse(uri).path)


#: What a language server must not do in a folder the person has not trusted: run the project's own code. rust-analyzer
#: runs every `build.rs` and expands proc-macros (compiled crate code) when it loads a workspace, and `cargo check` on
#: save; in a cloned repository that is code execution on session open, before anyone was asked anything. Off until
#: `/trust` — the same line plugins and settings use. Answered both as `initializationOptions` and to the server's
#: `workspace/configuration` request, since rust-analyzer reads either.
#: In exchange, rust-analyzer's own (experimental) type diagnostics are switched on, so a type mismatch still shows
#: without `cargo check` — it reads the code, it does not build it.
UNTRUSTED_OPTIONS: dict[str, dict] = {
    "rust": {
        "cargo": {"buildScripts": {"enable": False}},
        "procMacro": {"enable": False},
        "checkOnSave": False,
        "diagnostics": {"experimental": {"enable": True}},
    },
}


def folder_trusted(root: str) -> bool:
    try:
        import settings_store  # noqa: PLC0415

        return bool(settings_store.current(root).trusted)
    except Exception:
        return False


class LspError(RuntimeError):
    pass


class LspClient:
    """One running language server."""

    def __init__(self, spec: ServerSpec, command: tuple[str, ...], root: str, trusted: bool = True) -> None:
        self.spec = spec
        self.command = command
        self.root = str(Path(root).resolve())
        self.trusted = trusted
        self.restricted = None if trusted else UNTRUSTED_OPTIONS.get(spec.name)
        self._ids = itertools.count(1)
        self._pending: dict[int, tuple[threading.Event, list]] = {}
        self._write_lock = threading.Lock()
        self._state_lock = threading.Condition()
        self.diagnostics: dict[str, list] = {}  # uri -> diagnostics
        self._diag_version: dict[str, int] = {}  # uri -> how many publishes seen
        self.versions: dict[str, int] = {}  # uri -> document version we sent
        # One document conversation at a time: `sync` (after a real edit) and `check_text` (a proposed one, preflight.py)
        # both move a document's version, and interleaved they would leave the server holding the wrong text.
        self._doc_lock = threading.RLock()
        self._tasks: dict[str, harness_events.Task] = {}
        self.alive = False
        self.capabilities: dict = {}
        self.proc = subprocess.Popen(
            list(command),
            cwd=self.root,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        self.alive = True
        self._reader = threading.Thread(target=self._read_loop, name=f"lsp-{spec.name}", daemon=True)
        self._reader.start()

    # ---------------------------------------------------------- transport

    def _send(self, message: dict) -> None:
        body = json.dumps(message).encode("utf-8")
        with self._write_lock:
            if self.proc.stdin is None or self.proc.poll() is not None:
                raise LspError(f"{self.spec.name} language server is not running")
            self.proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
            self.proc.stdin.flush()

    def request(self, method: str, params: Any, timeout: float = 10.0) -> Any:
        request_id = next(self._ids)
        done, box = threading.Event(), []
        self._pending[request_id] = (done, box)
        self._send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        if not done.wait(timeout):
            self._pending.pop(request_id, None)
            raise LspError(f"{method} timed out after {timeout:g}s")
        if not box:
            raise LspError(f"{self.spec.name} language server exited during {method}")
        message = box[0]
        if "error" in message:
            raise LspError(f"{method}: {message['error'].get('message')}")
        return message.get("result")

    def notify(self, method: str, params: Any) -> None:
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def _read_loop(self) -> None:
        stream = self.proc.stdout
        try:
            while True:
                length = None
                while True:
                    line = stream.readline()
                    if not line:
                        return
                    line = line.strip()
                    if not line:
                        break
                    name, _, value = line.decode("ascii", "replace").partition(":")
                    if name.lower() == "content-length":
                        length = int(value.strip())
                if length is None:
                    continue
                body = stream.read(length)
                try:
                    self._dispatch(json.loads(body))
                except Exception:
                    logger.debug("lsp %s: bad message", self.spec.name, exc_info=True)
        finally:
            self.alive = False
            for done, _box in list(self._pending.values()):
                done.set()
            for task in self._tasks.values():
                task.finish("failed", "server exited")

    def _dispatch(self, message: dict) -> None:
        if "id" in message and ("result" in message or "error" in message) and "method" not in message:
            entry = self._pending.pop(message["id"], None)
            if entry:
                entry[1].append(message)
                entry[0].set()
            return
        method = message.get("method")
        params = message.get("params") or {}
        if "id" in message:  # a request from the server
            result: Any = None
            if method == "workspace/configuration":
                result = [self._configuration(str(item.get("section") or "")) for item in params.get("items", [])]
            elif method == "workspace/workspaceFolders":
                result = [{"uri": path_to_uri(self.root), "name": Path(self.root).name}]
            self._send({"jsonrpc": "2.0", "id": message["id"], "result": result})
            return
        if method == "textDocument/publishDiagnostics":
            uri = params.get("uri", "")
            with self._state_lock:
                self.diagnostics[uri] = params.get("diagnostics") or []
                self._diag_version[uri] = self._diag_version.get(uri, 0) + 1
                self._state_lock.notify_all()
        elif method == "$/progress":
            self._on_progress(str(params.get("token")), params.get("value") or {})

    def _on_progress(self, token: str, value: dict) -> None:
        kind = value.get("kind")
        if kind == "begin":
            title = value.get("title") or "working"
            percentage = value.get("percentage")
            self._tasks[token] = harness_events.Task(
                f"{self.spec.name}: {title}", total=100 if percentage is not None else None, source="lsp"
            )
            if percentage is not None:
                self._tasks[token].update(done=percentage, message=value.get("message") or "")
        elif kind == "report" and token in self._tasks:
            self._tasks[token].update(done=value.get("percentage"), message=value.get("message"))
        elif kind == "end" and token in self._tasks:
            self._tasks.pop(token).finish("completed", value.get("message"))

    def _configuration(self, section: str) -> Any:
        """The answer to one ``workspace/configuration`` item: the restricted options in an untrusted folder (rust-analyzer
        asks for its ``rust-analyzer`` section), else nothing — the server's own defaults."""
        if self.restricted is not None and self.spec.name == "rust" and section.startswith("rust-analyzer"):
            return self.restricted
        return None

    # ---------------------------------------------------------- protocol

    def initialize(self, timeout: float = 30.0) -> None:
        root_uri = path_to_uri(self.root)
        result = self.request(
            "initialize",
            {
                "processId": os.getpid(),
                "rootUri": root_uri,
                "rootPath": self.root,
                "workspaceFolders": [{"uri": root_uri, "name": Path(self.root).name}],
                "clientInfo": {"name": "sierralab-agents", "version": "0.1"},
                **({"initializationOptions": self.restricted} if self.restricted is not None else {}),
                "capabilities": {
                    "window": {"workDoneProgress": True},
                    "workspace": {"configuration": True, "workspaceFolders": True, "symbol": {}},
                    "textDocument": {
                        "synchronization": {"didSave": True, "dynamicRegistration": False},
                        "publishDiagnostics": {"relatedInformation": False, "versionSupport": True},
                        "hover": {"contentFormat": ["plaintext", "markdown"]},
                        "definition": {"linkSupport": False},
                        "references": {},
                        "documentSymbol": {"hierarchicalDocumentSymbolSupport": True},
                    },
                },
            },
            timeout=timeout,
        )
        self.capabilities = (result or {}).get("capabilities") or {}
        self.notify("initialized", {})

    def sync(self, path: str) -> str:
        """Open (or re-send) ``path``'s current contents; returns its URI."""
        path = str(Path(path).resolve())
        uri = path_to_uri(path)
        text = Path(path).read_text(encoding="utf-8", errors="replace")
        with self._doc_lock:
            version = self.versions.get(uri)
            if version is None:
                self.versions[uri] = 1
                self.notify(
                    "textDocument/didOpen",
                    {"textDocument": {"uri": uri, "languageId": self.spec.language_id(path), "version": 1, "text": text}},
                )
            else:
                self.versions[uri] = version + 1
                self.notify(
                    "textDocument/didChange",
                    {"textDocument": {"uri": uri, "version": version + 1}, "contentChanges": [{"text": text}]},
                )
                self.notify("textDocument/didSave", {"textDocument": {"uri": uri}, "text": text})
        return uri

    def check_text(self, path: str, text: str, timeout: float = 5.0) -> list:
        """The server's diagnostics for ``text`` as ``path``'s contents (``check_texts`` with one file)."""
        return self.check_texts([(path, text)], timeout)[str(Path(path).resolve())]

    def check_texts(self, files: list[tuple[str, str]], timeout: float = 5.0) -> dict[str, list]:
        """The server's diagnostics for proposed contents of several files *at once* — an edit judged by the real
        compiler front end without writing it (preflight.py). All the files change together in the server's memory, so
        a multi-file edit (a rename and its callers) is judged as the whole it is, not file by file against the old
        version of the others. No ``didSave`` (rust-analyzer does not run ``cargo check``; nothing executes). Afterwards
        the server's view is put back to what is on disk, and a document that was not open before is closed."""
        resolved = [(str(Path(path).resolve()), text) for path, text in files]
        out: dict[str, list] = {}
        with self._doc_lock:
            seen = {}
            opened_here = []
            for path, text in resolved:
                uri = path_to_uri(path)
                seen[uri] = self.publishes(uri)
                if uri not in self.versions:
                    opened_here.append(uri)
                    self.versions[uri] = 1
                    self.notify("textDocument/didOpen", {"textDocument": {"uri": uri, "languageId": self.spec.language_id(path), "version": 1, "text": text}})
                else:
                    self.versions[uri] += 1
                    self.notify("textDocument/didChange", {"textDocument": {"uri": uri, "version": self.versions[uri]}, "contentChanges": [{"text": text}]})
            try:
                deadline = time.monotonic() + timeout
                for path, _text in resolved:
                    uri = path_to_uri(path)
                    out[path] = self.wait_for_diagnostics(uri, seen[uri], max(0.5, deadline - time.monotonic()))
            finally:
                for path, _text in resolved:
                    uri = path_to_uri(path)
                    if uri in opened_here:
                        self.versions.pop(uri, None)
                        self.notify("textDocument/didClose", {"textDocument": {"uri": uri}})
                        continue
                    try:
                        disk = Path(path).read_text(encoding="utf-8", errors="replace")
                    except OSError:
                        disk = ""
                    self.versions[uri] += 1
                    self.notify("textDocument/didChange", {"textDocument": {"uri": uri, "version": self.versions[uri]}, "contentChanges": [{"text": disk}]})
        return out

    def wait_for_diagnostics(self, uri: str, seen: int, timeout: float, settle: float = 0.4) -> list:
        """Diagnostics published after ``seen`` publishes, waiting up to
        ``timeout``; then keeps listening ``settle`` seconds for a follow-up
        (rust-analyzer publishes native errors first, cargo-check ones later)."""
        deadline = time.monotonic() + timeout
        with self._state_lock:
            while self._diag_version.get(uri, 0) <= seen and time.monotonic() < deadline and self.alive:
                self._state_lock.wait(min(0.1, max(0.0, deadline - time.monotonic())))
            settle_until = min(deadline, time.monotonic() + settle)
            last = self._diag_version.get(uri, 0)
            while time.monotonic() < settle_until and self.alive:
                self._state_lock.wait(0.05)
                if self._diag_version.get(uri, 0) != last:
                    last = self._diag_version.get(uri, 0)
                    settle_until = min(deadline, time.monotonic() + settle)
            return list(self.diagnostics.get(uri, []))

    def publishes(self, uri: str) -> int:
        with self._state_lock:
            return self._diag_version.get(uri, 0)

    def shutdown(self) -> None:
        if self.proc.poll() is None:
            try:
                self.request("shutdown", None, timeout=2.0)
                self.notify("exit", None)
            except Exception:
                pass
            try:
                self.proc.wait(timeout=2.0)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self.alive = False


def format_diagnostics(diagnostics: list, path: str, root: str, limit: int = 20) -> str:
    severity = {1: "error", 2: "warning", 3: "info", 4: "hint"}
    rel = os.path.relpath(path, root) if path.startswith(root) else path
    rows = []
    ordered = sorted(diagnostics, key=lambda d: (d.get("severity", 1), d["range"]["start"]["line"]))
    for diagnostic in ordered[:limit]:
        start = diagnostic["range"]["start"]
        code = diagnostic.get("code")
        code = f"[{code}] " if code not in (None, "") else ""
        message = " ".join(str(diagnostic.get("message", "")).split())
        rows.append(f"{rel}:{start['line'] + 1}:{start['character'] + 1}: {severity.get(diagnostic.get('severity', 1), 'error')}: {code}{message}")
    if len(ordered) > limit:
        rows.append(f"... {len(ordered) - limit} more")
    return "\n".join(rows)


def count_errors(diagnostics: list) -> int:
    return sum(1 for d in diagnostics if d.get("severity", 1) == 1)


class LspManager:
    """The language servers for one project root."""

    def __init__(self, root: str | os.PathLike, which=shutil.which) -> None:
        self.root = str(Path(root).resolve())
        self.detected = detect(self.root, which)
        self._clients: dict[str, LspClient] = {}
        self._failed: dict[str, str] = {}
        self._lock = threading.Lock()
        self.enabled = True

    def spec_for(self, path: str) -> tuple[ServerSpec, tuple[str, ...] | None] | None:
        suffix = Path(path).suffix
        return next(((spec, cmd) for spec, cmd in self.detected if suffix in spec.extensions), None)

    def client_for(self, path: str, start: bool = True) -> LspClient | None:
        if not self.enabled:
            return None
        match = self.spec_for(path)
        if match is None or match[1] is None:
            return None
        spec, command = match
        with self._lock:
            client = self._clients.get(spec.name)
            if client is not None and client.alive:
                return client
            if not start or spec.name in self._failed:
                return None
            task = harness_events.Task(f"lsp: starting {Path(command[0]).name}", source="lsp")
            try:
                client = LspClient(spec, command, self.root, trusted=folder_trusted(self.root))
                client.initialize()
            except Exception as error:
                self._failed[spec.name] = f"{type(error).__name__}: {error}"
                task.finish("failed", self._failed[spec.name])
                harness_events.emit("lsp", {"event": "failed", "server": spec.name, "error": self._failed[spec.name]})
                return None
            task.finish("completed")
            self._clients[spec.name] = client
            harness_events.emit("lsp", {"event": "started", "server": spec.name, "command": Path(command[0]).name})
            return client

    def start_all(self) -> None:
        """Start every installed server for the project (call from a worker thread)."""
        for spec, command in self.detected:
            if command is None:
                continue
            sample = next(_files_with(self.root, spec.extensions), None)
            if sample is not None:
                self.client_for(sample)

    def diagnostics(self, path: str, timeout: float = 8.0) -> tuple[list, LspClient | None]:
        client = self.client_for(path)
        if client is None:
            return [], None
        uri = path_to_uri(path)
        seen = client.publishes(uri)
        client.sync(path)
        return client.wait_for_diagnostics(uri, seen, timeout), client

    def status(self) -> list[dict]:
        rows = []
        for spec, command in self.detected:
            client = self._clients.get(spec.name)
            state = "running" if client and client.alive else "failed" if spec.name in self._failed else "idle" if command else "not installed"
            rows.append(
                {
                    "server": spec.name,
                    "command": command[0] if command else spec.commands[0][0],
                    "state": state,
                    "error": self._failed.get(spec.name),
                    "extensions": list(spec.extensions),
                }
            )
        return rows

    def shutdown(self) -> None:
        with self._lock:
            clients, self._clients = list(self._clients.values()), {}
        for client in clients:
            client.shutdown()


def _files_with(root: str, extensions: tuple[str, ...]):
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in _IGNORED_DIRS and not d.startswith(".")]
        for filename in filenames:
            if Path(filename).suffix in extensions:
                yield os.path.join(dirpath, filename)


# --------------------------------------------------------------- registry

_managers: dict[str, LspManager] = {}
_managers_lock = threading.Lock()


def manager_for(root: str | os.PathLike | None = None) -> LspManager:
    root = str(Path(root or os.getcwd()).resolve())
    with _managers_lock:
        manager = _managers.get(root)
        if manager is None:
            manager = _managers[root] = LspManager(root)
        return manager


def shutdown_all() -> None:
    with _managers_lock:
        managers = list(_managers.values())
        _managers.clear()
    for manager in managers:
        manager.shutdown()


def _resolve(path: str) -> str:
    return str(Path(path).expanduser().resolve()) if os.path.isabs(os.path.expanduser(path)) else str((Path(os.getcwd()) / path).resolve())


def _position(line: int, column: int) -> dict:
    return {"line": max(0, int(line) - 1), "character": max(0, int(column) - 1)}


def _locations(result: Any, root: str) -> str:
    if not result:
        return "no results"
    items = result if isinstance(result, list) else [result]
    rows = []
    for item in items[:50]:
        uri = item.get("uri") or item.get("targetUri")
        rng = item.get("range") or item.get("targetSelectionRange") or {}
        start = rng.get("start", {})
        path = uri_to_path(uri) if uri else "?"
        rel = os.path.relpath(path, root) if path.startswith(root) else path
        line_text = ""
        try:
            line_text = Path(path).read_text(encoding="utf-8", errors="replace").splitlines()[start.get("line", 0)].strip()
        except (OSError, IndexError):
            pass
        rows.append(f"{rel}:{start.get('line', 0) + 1}:{start.get('character', 0) + 1}  {line_text[:160]}")
    return "\n".join(rows)


def _no_server(path: str) -> str:
    manager = manager_for()
    match = manager.spec_for(path)
    if match is None:
        return f"no language server handles {Path(path).suffix or path} files in this project"
    spec, command = match
    if command is None:
        return f"the {spec.name} language server ({spec.commands[0][0]}) is not installed"
    return f"the {spec.name} language server could not start"


# ------------------------------------------------------------------ tools


@tool
def lsp_diagnostics(path: str) -> str:
    """Compiler/type-checker errors and warnings for a file, from the project's
    language server (rust-analyzer, pyright, tsserver, gopls, clangd). Use after
    editing to confirm the file is clean, or to find what is broken.

    Args:
        path: File path (relative to the project or absolute).
    """
    path = _resolve(path)
    diagnostics, client = manager_for().diagnostics(path)
    if client is None:
        return _no_server(path)
    if not diagnostics:
        return f"{os.path.relpath(path, client.root)}: no diagnostics"
    return format_diagnostics(diagnostics, path, client.root)


def _at(method: str, path: str, line: int, column: int, extra: dict | None = None) -> str:
    path = _resolve(path)
    manager = manager_for()
    client = manager.client_for(path)
    if client is None:
        return _no_server(path)
    uri = client.sync(path)
    params = {"textDocument": {"uri": uri}, "position": _position(line, column), **(extra or {})}
    try:
        result = client.request(method, params, timeout=15.0)
    except LspError as error:
        return f"language server error: {error}"
    if method == "textDocument/hover":
        contents = (result or {}).get("contents")
        if isinstance(contents, dict):
            return contents.get("value") or "no information"
        if isinstance(contents, list):
            return "\n".join(c.get("value", "") if isinstance(c, dict) else str(c) for c in contents) or "no information"
        return str(contents) if contents else "no information"
    return _locations(result, client.root)


@tool
def lsp_definition(path: str, line: int, column: int) -> str:
    """Where the symbol at a position is defined (go-to-definition).

    Args:
        path: File containing the symbol.
        line: 1-based line number.
        column: 1-based column of a character inside the symbol.
    """
    return _at("textDocument/definition", path, line, column)


@tool
def lsp_references(path: str, line: int, column: int) -> str:
    """Every reference to the symbol at a position, across the project.

    Args:
        path: File containing the symbol.
        line: 1-based line number.
        column: 1-based column of a character inside the symbol.
    """
    return _at("textDocument/references", path, line, column, {"context": {"includeDeclaration": True}})


@tool
def lsp_hover(path: str, line: int, column: int) -> str:
    """Type signature and docs for the symbol at a position.

    Args:
        path: File containing the symbol.
        line: 1-based line number.
        column: 1-based column of a character inside the symbol.
    """
    return _at("textDocument/hover", path, line, column)


@tool
def lsp_symbols(query: str, path: str = "") -> str:
    """Find symbols (functions, types, modules) by name across the project.

    Args:
        query: Name or fragment to look for.
        path: Any file of the language to search (picks the language server);
            optional when the project has one language.
    """
    manager = manager_for()
    target = _resolve(path) if path else None
    if target is None:
        installed = [(spec, cmd) for spec, cmd in manager.detected if cmd]
        sample = next((f for spec, _ in installed for f in _files_with(manager.root, spec.extensions)), None)
        target = sample
    if not target:
        return "no language server is available for this project"
    client = manager.client_for(target)
    if client is None:
        return _no_server(target)
    try:
        result = client.request("workspace/symbol", {"query": query}, timeout=15.0)
    except LspError as error:
        return f"language server error: {error}"
    kinds = {5: "class", 6: "method", 10: "enum", 11: "interface", 12: "function", 13: "variable", 14: "constant", 23: "struct", 2: "module", 22: "enum member", 8: "field"}
    rows = []
    for symbol in (result or [])[:40]:
        location = symbol.get("location") or {}
        uri = location.get("uri", "")
        start = (location.get("range") or {}).get("start", {})
        path_ = uri_to_path(uri) if uri else "?"
        rel = os.path.relpath(path_, client.root) if path_.startswith(client.root) else path_
        rows.append(f"{kinds.get(symbol.get('kind'), 'symbol')} {symbol.get('name')}  {rel}:{start.get('line', 0) + 1}")
    return "\n".join(rows) or "no symbols found"


LSP_TOOLS = [lsp_diagnostics, lsp_definition, lsp_references, lsp_hover, lsp_symbols]


# ------------------------------------------------------------------ hooks

_EDIT_TOOLS = {"editor", "file_write", "apply_patch"}


def edited_paths(tool_name: str, tool_input: Any, result: Any = None) -> list[str]:
    if not isinstance(tool_input, dict):
        return []
    if tool_name in ("editor", "file_write"):
        path = tool_input.get("path")
        return [path] if isinstance(path, str) else []
    if tool_name == "apply_patch":
        from patch_tool import patch_paths

        return patch_paths(str(tool_input.get("patch") or ""))
    return []


class LspEditHooks(HookProvider):
    """After a successful edit, append the file's fresh diagnostics to the tool
    result, so the model sees what its edit broke before its next step."""

    def __init__(self, timeout: float = 6.0, root: str | None = None) -> None:
        self.timeout = timeout
        self.root = root

    def register_hooks(self, registry: HookRegistry) -> None:
        registry.add_callback(AfterToolCallEvent, self._after_edit)

    async def _after_edit(self, event: AfterToolCallEvent) -> None:
        # Async so Strands awaits it on the event loop while the (blocking) wait
        # for the server's diagnostics runs on a worker thread — the ACP server
        # keeps streaming notifications and honouring cancel meanwhile.
        name = event.tool_use.get("name")
        if name not in _EDIT_TOOLS or not isinstance(event.result, dict) or event.result.get("status") != "success":
            return
        manager = manager_for(self.root)
        if not manager.enabled:
            return
        notes = await asyncio.to_thread(self._collect, manager, name, event.tool_use.get("input"), event.result)
        if notes:
            content = list(event.result.get("content") or [])
            content.append({"text": "\n\n".join(notes)})
            event.result = {**event.result, "content": content}

    def _collect(self, manager: "LspManager", name: str, tool_input: Any, result: Any) -> list[str]:
        notes = []
        for raw in edited_paths(name, tool_input, result):
            path = _resolve(raw)
            if not os.path.isfile(path) or manager.spec_for(path) is None:
                continue
            try:
                diagnostics, client = manager.diagnostics(path, timeout=self.timeout)
            except Exception as error:  # an LSP problem must never fail the edit itself
                logger.debug("lsp diagnostics after edit failed: %s", error)
                continue
            if client is None:
                continue
            errors = count_errors(diagnostics)
            harness_events.emit("lsp", {"event": "diagnostics", "path": os.path.relpath(path, client.root), "errors": errors, "total": len(diagnostics)})
            if diagnostics:
                notes.append(
                    f"LSP diagnostics after this edit ({errors} error(s), {len(diagnostics) - errors} other):\n"
                    + format_diagnostics(diagnostics, path, client.root)
                )
            else:
                notes.append(f"LSP: {os.path.relpath(path, client.root)} has no diagnostics after this edit.")
        return notes

"""A Strands ``Sandbox`` for running external CLIs the harness does not trust with its secrets.

Strands' stock local environment (``NotASandboxLocalEnvironment``) says what it is in
its name: it runs ``sh -c`` on the host *inheriting the whole parent environment*. That
is the wrong default for the Claude Code CLI, a second agent with its own file and shell
tools. This sandbox keeps the Strands interface (``Agent(sandbox=...)``-compatible, the
same streaming ``StreamChunk`` / ``ExecutionResult`` contract) and changes what runs:

* **argv, not a shell.** ``execute_streaming`` ``shlex``-splits and execs; there is no
  ``sh -c`` for a model-composed string to escape into. The preferred entry point,
  ``execute_argv_streaming``, takes the argv list and the prompt as stdin, so a long task
  never appears in ``ps`` or hits an argument-size limit.
* **An allowlisted environment.** The child sees ``PATH``/``HOME``/locale and whatever
  ``env=`` the caller adds (the vault proxy variables) — nothing else. ``ANTHROPIC_API_KEY``,
  ``OLLAMA_*``, ``GITHUB_TOKEN``, ``AWS_*`` and every vault placeholder simply are not
  there. The caller states what to add; nothing is inherited by accident.
* **No ``.env`` files, enforced by the OS.** On macOS the child runs under Seatbelt
  (``sandbox-exec``) with a profile that denies reading *or writing* ``.env`` / ``.env.*``
  anywhere on disk (templates such as ``.env.example`` stay readable), the daemon's token
  file and the cloud-credential files. It is the kernel refusing the ``open``, so a ``cat
  .env``, a ``Read`` tool call or a script the child writes and runs all fail the same way —
  which no ``--disallowedTools`` rule can promise. Where Seatbelt is unavailable the
  sandbox *refuses to run* rather than run unconfined (``SIERRALAB_SANDBOX_SEATBELT=off``
  is the explicit, logged opt-out for a nested-sandbox dev box).
* **A killable process group.** The child leads its own group; a timeout or a cancelled
  task kills the whole tree, so a build the CLI launched does not outlive it.
* **File operations confined to the project root**, and the same ``.env`` rule.

What it is not: a network sandbox (that is the vault proxy's job — egress is pointed at
it through ``env=``, see ``claude_code_tool``) and not a VM. It cannot stop the child
reading a world-readable file that is not on the deny list. ``THREAT_MODEL.md``,
"The Meta-Harness", says so.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
import os
import re
import shlex
import signal
import sys
import tempfile
from collections.abc import AsyncGenerator, Callable, Iterable, Mapping
from pathlib import Path
from typing import Any

from strands.sandbox import ExecutionResult, FileInfo, Sandbox, StreamChunk
from strands.sandbox.constants import ENV_KEY_PATTERN, LANGUAGE_PATTERN
from strands.sandbox.errors import SandboxPathNotFoundError, SandboxTimeoutError

logger = logging.getLogger(__name__)

SANDBOX_EXEC = "/usr/bin/sandbox-exec"
SEATBELT_ENV = "SIERRALAB_SANDBOX_SEATBELT"

#: The only parent-environment variables a child inherits.
DEFAULT_ENV_ALLOW = ("PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "TERM", "LANG", "TZ", "LC_ALL", "LC_CTYPE", "COLORTERM", "NO_COLOR")

#: ``.env``, ``.env.local`` … but not the committed templates. Matches a path *component*.
_DOTENV_FILE = re.compile(r"(^|/)\.env($|\.(?!(example|sample|template|dist)$)[^/]*$)")
#: The same rule as a Seatbelt regex (POSIX ERE has no look-ahead, so the templates
#: are re-allowed by a later rule — Seatbelt evaluates the last matching rule).
_SB_DOTENV = r"(^|/)\.env($|\.[^/]*$)"
_SB_DOTENV_TEMPLATE = r"(^|/)\.env\.(example|sample|template|dist)$"

#: Home-relative files and directories the child never needs and must not read.
DENY_HOME = (
    ".sierralab/daemon.json",
    ".aws",
    ".netrc",
    ".config/gh",
    ".docker/config.json",
    ".kube/config",
)

_MAX_OUTPUT_CHARS = 2_000_000
_READ_CHUNK = 65536


class SandboxUnavailable(RuntimeError):
    """The confinement this sandbox promises cannot be applied here."""


class SecretPathError(PermissionError):
    """A file operation named a path the sandbox keeps from the child."""


def is_secret_path(path: str | os.PathLike[str]) -> bool:
    """Is this a ``.env`` file (any directory), not a template?"""
    return bool(_DOTENV_FILE.search(str(path).replace(os.sep, "/")))


def _sb_quote(value: str) -> str:
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def seatbelt_profile(home: str | None = None, extra_deny: Iterable[str] = ()) -> str:
    """The Seatbelt profile text. ``(allow default)`` and then deny — the child is a
    developer tool that must keep working; the denials are the secrets."""
    home = home or str(Path.home())
    lines = [
        "(version 1)",
        "(allow default)",
        f'(deny file-read* file-write* (regex #"{_SB_DOTENV}"))',
        f'(allow file-read* (regex #"{_SB_DOTENV_TEMPLATE}"))',
    ]
    for relative in DENY_HOME:
        target = os.path.join(home, relative)
        lines.append(f"(deny file-read* file-write* (subpath {_sb_quote(target)}))")
    for path in extra_deny:
        lines.append(f"(deny file-read* file-write* (subpath {_sb_quote(os.path.abspath(path))}))")
    return "\n".join(lines) + "\n"


_probe_cache: bool | None = None


def seatbelt_works() -> bool:
    """Can ``sandbox-exec`` apply a profile *here*? It fails (exit 71, "sandbox_apply:
    Operation not permitted") when this process is already inside a sandbox."""
    global _probe_cache
    if _probe_cache is None:
        if sys.platform != "darwin" or not os.path.exists(SANDBOX_EXEC):
            _probe_cache = False
        else:
            import subprocess

            try:
                done = subprocess.run([SANDBOX_EXEC, "-p", "(version 1)(allow default)", "/usr/bin/true"], capture_output=True, timeout=5, check=False)
                _probe_cache = done.returncode == 0
            except (OSError, subprocess.SubprocessError):
                _probe_cache = False
    return _probe_cache


class ScrubbedSandbox(Sandbox):
    """See the module docstring. ``root`` is where file operations are confined and
    where commands run unless ``cwd=`` says otherwise."""

    def __init__(
        self,
        root: str | os.PathLike[str] | None = None,
        *,
        base_env: Mapping[str, str] | None = None,
        env_allow: Iterable[str] = DEFAULT_ENV_ALLOW,
        seatbelt: str = "auto",
        on_spawn: Callable[[int, list[str]], None] | None = None,
    ) -> None:
        self.root = Path(root or os.getcwd()).resolve()
        self._base_env = base_env
        self._allow = tuple(env_allow)
        mode = (os.getenv(SEATBELT_ENV) or seatbelt).strip().lower()
        if mode not in ("auto", "on", "off"):
            raise ValueError(f"seatbelt must be auto|on|off, got {mode!r}")
        self.seatbelt_mode = mode
        self.on_spawn = on_spawn
        self.last_pid: int | None = None
        self.last_argv: list[str] | None = None
        self.last_env: dict[str, str] | None = None
        self._profile = seatbelt_profile()

    # ------------------------------------------------------- confinement

    @property
    def confinement(self) -> str:
        """What actually protects the child's filesystem: ``seatbelt`` or ``none``."""
        if self.seatbelt_mode == "off":
            return "none"
        return "seatbelt" if seatbelt_works() else "none"

    def _require_confinement(self) -> None:
        if self.seatbelt_mode == "off":
            logger.warning("scrubbed sandbox: %s=off — the child runs WITHOUT filesystem confinement", SEATBELT_ENV)
            return
        if not seatbelt_works():
            raise SandboxUnavailable(
                "the Seatbelt sandbox (sandbox-exec) cannot be applied here, so a child process could read .env files; "
                f"refusing to run it unconfined. Run outside another sandbox, or set {SEATBELT_ENV}=off to accept that."
            )

    def build_env(self, extra: Mapping[str, str] | None = None) -> dict[str, str]:
        """The child's whole environment: allowlisted parent variables, then ``extra``."""
        parent = self._base_env if self._base_env is not None else os.environ
        env = {name: parent[name] for name in self._allow if name in parent}
        env.setdefault("PATH", "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin")
        for key, value in (extra or {}).items():
            if not ENV_KEY_PATTERN.fullmatch(key):
                raise ValueError(f"invalid environment variable name: {key!r}")
            env[key] = str(value)
        return env

    def wrap(self, argv: list[str]) -> list[str]:
        if self.confinement == "seatbelt":
            return [SANDBOX_EXEC, "-p", self._profile, *argv]
        return list(argv)

    # --------------------------------------------------------- execution

    async def execute_argv_streaming(
        self,
        argv: list[str],
        *,
        stdin: str | None = None,
        timeout: float | None = None,
        cwd: str | None = None,
        env: Mapping[str, str] | None = None,
    ) -> AsyncGenerator[StreamChunk | ExecutionResult, None]:
        """Run ``argv`` (no shell), streaming stdout/stderr, then yield the result.

        ``stdin`` is written and closed. A missing executable is exit code 127 with the
        reason on stderr (not an exception), like the Docker backend.
        """
        if not argv or not all(isinstance(part, str) for part in argv):
            raise ValueError("argv must be a non-empty list of strings")
        self._require_confinement()
        run_cwd = str(Path(cwd).resolve()) if cwd else str(self.root)
        child_env = self.build_env(env)
        wrapped = self.wrap(argv)
        self.last_argv, self.last_env = list(argv), dict(child_env)
        try:
            process = await asyncio.create_subprocess_exec(
                *wrapped,
                stdin=asyncio.subprocess.PIPE if stdin is not None else asyncio.subprocess.DEVNULL,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                cwd=run_cwd,
                env=child_env,
                start_new_session=True,
                limit=1024 * 1024,
            )
        except FileNotFoundError:
            yield ExecutionResult(exit_code=127, stdout="", stderr=f"{argv[0]}: command not found")
            return
        self.last_pid = process.pid
        if self.on_spawn is not None:
            with contextlib.suppress(Exception):
                self.on_spawn(process.pid, list(argv))

        queue: asyncio.Queue[StreamChunk | None] = asyncio.Queue()
        out_buf: list[str] = []
        err_buf: list[str] = []
        sizes = {"stdout": 0, "stderr": 0}
        timed_out = False

        async def pump(stream: asyncio.StreamReader, kind: str, buf: list[str]) -> None:
            while data := await stream.read(_READ_CHUNK):
                text = data.decode(errors="replace")
                if sizes[kind] < _MAX_OUTPUT_CHARS:
                    buf.append(text)
                    sizes[kind] += len(text)
                await queue.put(StreamChunk(data=text, stream_type=kind))  # type: ignore[arg-type]

        async def feed() -> None:
            assert process.stdin is not None and stdin is not None
            try:
                process.stdin.write(stdin.encode())
                await process.stdin.drain()
            except (BrokenPipeError, ConnectionResetError):
                pass  # the child exited before reading its prompt; its exit code says why
            finally:
                with contextlib.suppress(Exception):
                    process.stdin.close()

        assert process.stdout is not None and process.stderr is not None
        pumps = asyncio.gather(pump(process.stdout, "stdout", out_buf), pump(process.stderr, "stderr", err_buf))

        async def reader() -> None:
            await pumps
            await process.wait()
            await queue.put(None)

        async def enforce_timeout(seconds: float) -> None:
            nonlocal timed_out
            await asyncio.sleep(seconds)
            if process.returncode is None:
                timed_out = True
                self._kill_tree(process)
                await queue.put(None)

        tasks = [asyncio.ensure_future(reader())]
        if stdin is not None:
            tasks.append(asyncio.ensure_future(feed()))
        if timeout is not None:
            tasks.append(asyncio.ensure_future(enforce_timeout(timeout)))
        try:
            while (item := await queue.get()) is not None:
                yield item
            if timed_out:
                raise SandboxTimeoutError(timeout)
            code = process.returncode if process.returncode is not None else 1
            yield ExecutionResult(exit_code=128 - code if code < 0 else code, stdout="".join(out_buf), stderr="".join(err_buf))
        finally:
            if process.returncode is None:
                self._kill_tree(process)
            pumps.cancel()
            for task in tasks:
                task.cancel()
            await asyncio.gather(pumps, *tasks, return_exceptions=True)

    @staticmethod
    def _kill_tree(process: asyncio.subprocess.Process) -> None:
        with contextlib.suppress(ProcessLookupError, PermissionError):
            os.killpg(process.pid, signal.SIGKILL)

    async def execute_streaming(
        self,
        command: str,
        *,
        timeout: float | None = None,
        cwd: str | None = None,
        env: dict[str, str] | None = None,
        **kwargs: Any,
    ) -> AsyncGenerator[StreamChunk | ExecutionResult, None]:
        """``command`` is split like a shell would and *exec'd*; shell syntax
        (pipes, ``&&``, redirects, expansions) is not interpreted — it is passed as
        literal arguments. Use ``execute_argv_streaming`` to avoid the split."""
        async for chunk in self.execute_argv_streaming(shlex.split(command), stdin=kwargs.get("stdin"), timeout=timeout, cwd=cwd, env=env):
            yield chunk

    async def execute_code_streaming(
        self,
        code: str,
        language: str,
        *,
        timeout: float | None = None,
        cwd: str | None = None,
        env: dict[str, str] | None = None,
        **kwargs: Any,
    ) -> AsyncGenerator[StreamChunk | ExecutionResult, None]:
        if not LANGUAGE_PATTERN.fullmatch(language):
            raise ValueError(f"language parameter contains invalid characters: {language}")
        async for chunk in self.execute_argv_streaming([language], stdin=code, timeout=timeout, cwd=cwd, env=env):
            yield chunk

    # -------------------------------------------------------------- files

    def _confine(self, path: str) -> Path:
        candidate = Path(path if os.path.isabs(path) else self.root / path)
        resolved = candidate.resolve()
        if resolved != self.root and self.root not in resolved.parents:
            raise SecretPathError(f"{path!r} is outside the sandbox root {self.root}")
        if is_secret_path(resolved) or is_secret_path(candidate):
            raise SecretPathError(f"{path!r} is a secrets file the sandbox does not touch")
        return resolved

    async def read_file(self, path: str, **kwargs: Any) -> bytes:
        return self._confine(path).read_bytes()

    async def write_file(self, path: str, content: bytes, **kwargs: Any) -> None:
        target = self._confine(path)
        target.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(dir=target.parent, delete=False) as handle:
            handle.write(content)
            temp = Path(handle.name)
        os.replace(temp, target)

    async def remove_file(self, path: str, **kwargs: Any) -> None:
        self._confine(path).unlink()

    async def list_files(self, path: str, **kwargs: Any) -> list[FileInfo]:
        directory = self._confine(path)
        try:
            entries = sorted(os.scandir(directory), key=lambda entry: entry.name)
        except (FileNotFoundError, NotADirectoryError) as error:
            raise SandboxPathNotFoundError(path) from error
        rows: list[FileInfo] = []
        for entry in entries:
            if is_secret_path(entry.name):
                continue  # not even their existence
            try:
                rows.append(FileInfo(name=entry.name, is_dir=entry.is_dir(), size=entry.stat().st_size))
            except OSError:
                rows.append(FileInfo(name=entry.name))
        return rows

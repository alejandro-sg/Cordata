"""The ``ollama`` command line as a typed, injection-proof wrapper.

Everything the Meta-Harness does to local Ollama weights goes through the CLI the
user would type — ``ollama list``, ``pull``, ``rm``, ``stop`` (and ``ps``) — run
as an *argv* (never through a shell) with a trimmed environment, so a model name
the LLM composed cannot become a flag or a second command, and nothing in the
harness's environment (provider keys, vault placeholders) reaches the child.

The one thing read over HTTP instead is what is *resident right now* (``GET
/api/ps``: exact byte sizes and expiry times), because ``ollama ps`` prints sizes
as "19 GB" text and the memory budget wants numbers it can add up. Read-only, local.

Stdlib only, for the same reason ``omlx_manager`` is: the daemon calls it for the
GUI's emergency unload, and the daemon never imports the agent stack.

Policy lives elsewhere. This module can ``pull`` and ``rm`` because the spec asks for
the verbs; whether the *model* may is the approval gate's call (``acp_server.
_ApprovalHandler``: a pull or an rm always asks, even in ``auto`` mode), and the
earlier "this harness never pulls" stance (``ollama_cloud``'s endpoint guard) still
holds for the *chat* transport, which cannot reach ``/api/pull``.
"""

from __future__ import annotations

import json
import logging
import os
import re
import shutil
import subprocess
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Any

logger = logging.getLogger(__name__)

BINARY_ENV = "SIERRALAB_OLLAMA_BIN"
GB = 1024**3

#: A registry tag: ``name``, ``name:tag``, ``org/name:tag``, ``host/org/name:tag``,
#: optionally pinned by digest. No leading ``-`` (it would be read as a flag), no
#: whitespace, no shell or path metacharacters.
_MODEL = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:/@+-]{0,199}$")

#: What the child process may see. HOME so the CLI finds ``~/.ollama`` (and its
#: signed-in identity); OLLAMA_* so it finds the same daemon this harness does.
_ENV_ALLOW = ("PATH", "HOME", "USER", "LOGNAME", "TMPDIR", "LANG", "LC_ALL", "TERM")
_ENV_PREFIXES = ("OLLAMA_",)

SUBCOMMANDS = ("list", "ps", "pull", "rm", "stop")


class OllamaCliError(RuntimeError):
    pass


class InvalidModelName(OllamaCliError, ValueError):
    pass


def valid_model(name: str) -> str:
    if not isinstance(name, str) or not _MODEL.fullmatch(name.strip()):
        raise InvalidModelName(f"{name!r} is not a valid Ollama model name")
    return name.strip()


def api_root() -> str:
    explicit = os.getenv("OLLAMA_BASE_URL")
    if explicit:
        return explicit.rstrip("/").removesuffix("/v1")
    host = (os.getenv("OLLAMA_HOST") or "127.0.0.1:11434").strip()
    if "://" not in host:
        host = f"http://{host}"
    return host.rstrip("/")


@dataclass(frozen=True)
class RunningModel:
    name: str
    size_bytes: int
    vram_bytes: int
    expires_at: float | None  # epoch seconds; the earliest-expiring is the least recently used

    @property
    def size_gb(self) -> float:
        return self.size_bytes / GB


@dataclass(frozen=True)
class CliResult:
    argv: tuple[str, ...]
    exit_code: int
    stdout: str
    stderr: str
    seconds: float

    @property
    def ok(self) -> bool:
        return self.exit_code == 0

    @property
    def text(self) -> str:
        return (self.stdout.strip() or self.stderr.strip())


def _clean_env() -> dict[str, str]:
    env = {key: value for key, value in os.environ.items() if key in _ENV_ALLOW or key.startswith(_ENV_PREFIXES)}
    env.setdefault("PATH", "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin")
    return env


def _last_line(text: str) -> str:
    """``ollama pull`` redraws its progress bar with carriage returns; the last
    segment is the state it ended in."""
    segments = [part.strip() for chunk in text.splitlines() for part in chunk.split("\r") if part.strip()]
    return segments[-1] if segments else ""


class OllamaCli:
    def __init__(self, binary: str | None = None, *, runner=None, ps_reader=None) -> None:
        self._binary = binary
        self._runner = runner or subprocess.run
        self._ps_reader = ps_reader

    @property
    def binary(self) -> str | None:
        override = (self._binary or os.environ.get(BINARY_ENV) or "").strip()
        if override:
            return override if os.path.exists(override) else None
        return shutil.which("ollama") or ("/usr/local/bin/ollama" if os.path.exists("/usr/local/bin/ollama") else None)

    def run(self, *args: str, timeout: float = 60.0) -> CliResult:
        if not args or args[0] not in SUBCOMMANDS:
            raise OllamaCliError(f"ollama {args[:1]} is not one of {SUBCOMMANDS}")
        exe = self.binary
        if exe is None:
            raise OllamaCliError("the ollama CLI is not installed (nothing named 'ollama' on PATH)")
        argv = (exe, *args)
        started = time.monotonic()
        try:
            done = self._runner(list(argv), capture_output=True, text=True, timeout=timeout, env=_clean_env(), stdin=subprocess.DEVNULL, check=False)
        except subprocess.TimeoutExpired as error:
            raise OllamaCliError(f"`ollama {' '.join(args)}` did not finish within {timeout:g}s") from error
        except OSError as error:
            raise OllamaCliError(f"could not run ollama: {error}") from error
        return CliResult(argv, done.returncode, done.stdout or "", done.stderr or "", round(time.monotonic() - started, 2))

    # ------------------------------------------------------------- verbs

    def list(self) -> CliResult:
        return self.run("list")

    def ps(self) -> CliResult:
        return self.run("ps")

    def stop(self, model: str) -> CliResult:
        return self.run("stop", valid_model(model), timeout=30.0)

    def rm(self, model: str) -> CliResult:
        return self.run("rm", valid_model(model), timeout=60.0)

    def pull(self, model: str, *, timeout: float = 3600.0) -> CliResult:
        result = self.run("pull", valid_model(model), timeout=timeout)
        return CliResult(result.argv, result.exit_code, _last_line(result.stdout), _last_line(result.stderr), result.seconds)

    # ------------------------------------------------------------ resident

    def running(self, *, timeout: float = 3.0) -> list[RunningModel]:
        """What is in memory right now, with exact sizes. ``[]`` when the daemon is
        down — nothing is resident if nothing is serving."""
        if self._ps_reader is not None:
            return self._ps_reader()
        try:
            with urllib.request.urlopen(f"{api_root()}/api/ps", timeout=timeout) as response:  # noqa: S310 - local daemon
                payload = json.load(response)
        except (urllib.error.URLError, TimeoutError, OSError, ValueError):
            return []
        out: list[RunningModel] = []
        for row in payload.get("models") or []:
            name = str(row.get("name") or row.get("model") or "")
            if not name:
                continue
            out.append(RunningModel(name, int(row.get("size") or 0), int(row.get("size_vram") or 0), _expiry(row.get("expires_at"))))
        return out


def _expiry(value: Any) -> float | None:
    """RFC 3339 → epoch seconds (Go's nanosecond timestamps are trimmed to micro)."""
    if not isinstance(value, str) or not value:
        return None
    from datetime import datetime

    text = re.sub(r"(\.\d{6})\d+", r"\1", value.replace("Z", "+00:00"))
    try:
        return datetime.fromisoformat(text).timestamp()
    except ValueError:
        return None


_DEFAULT: OllamaCli | None = None


def default_cli() -> OllamaCli:
    global _DEFAULT
    if _DEFAULT is None:
        _DEFAULT = OllamaCli()
    return _DEFAULT

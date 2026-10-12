"""Set a big tool result aside instead of throwing it away — Strands' own `ContextOffloader`, fitted to this harness.

The problem with cutting. `context_diet.cap_result` keeps the head and the tail of a result over the limit and says
the middle is gone. That is the right shape for a file the agent can read again, and the wrong one for the output of a
command it should not run twice (a test run, a build, a migration, a long `git log`): the only way to see the middle
is to repeat the work. The Strands SDK ships the better primitive — `ContextOffloader` (docs: plugins/context-offloader,
the `large_tool_offloading` preset of `context_manager`): the full result is written to storage, the model reads a
short preview plus a reference, and `retrieve_offloaded_content` hands back exactly the slice it asks for (a regex with
context lines, or a line range). Nothing is lost; the context pays for what the model actually looks at.

What this module adds to the SDK's plugin, and why each part is here:

* **Head and tail in the preview.** The SDK's preview is the first N tokens. An error, a summary line, an exit code
  is at the *end* of build and test output, so the preview keeps both ends (`_slice_preview` is the SDK's documented
  slicing hook).
* **A private store.** `~/.cordata/offloaded/<session>/`, directory 0700 and files 0600, written directly on the
  host — never routed through the agent's sandbox (the SDK's `LocalFileStorage` binds to `agent.sandbox`, which in a
  restricted sandbox mode would refuse the write and silently keep the whole result in context). Tool output can hold
  anything the project holds; it is stored only after the registered credentials are masked, exactly as the model
  would have seen it (`secret_guard`).
* **A veto list.** A result that *is* the answer — a specialist's report, the Claude Code CLI's result, a swarm's
  synthesis — is meant to be read whole and relayed; setting it aside would make the orchestrator fetch what it was
  just handed. Those tools are never offloaded (they are still capped by `context_diet`).
* **The thresholds live.** The SDK reads its limits at construction; `context.toolOutputChars` can change mid-session
  (a setting, a trust decision), so the limits are re-read before each result. One knob, one meaning: "the most the
  model reads of one tool result at once" — over it, the result is offloaded (`context.offload`, default on) or, with
  offloading off, cut head+tail as before.
* **No eviction by loop cycles.** The SDK evicts an entry 20 cycles after it was stored — a session that comes back
  tomorrow would find its references dead. Files age out by *time* instead (`prune`, seven days, once per process).

What it is not. It does not decide *what* is worth keeping — the agent does, with a regex or a line range — and it does
not touch the stored conversation: the reference is in the message, the bytes are on disk beside the session.

Stdlib plus Strands.
"""

from __future__ import annotations

import logging
import os
import shutil
import time
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import Any

from strands.hooks import AfterToolCallEvent
from strands.plugins import hook
from strands.storage import LocalFileStorage
from strands.vended_plugins.context_offloader import ContextOffloader

import secret_guard

logger = logging.getLogger(__name__)

#: The SDK tool the plugin registers; the model reads an offloaded result back with it.
RETRIEVE_TOOL = "retrieve_offloaded_content"
#: What the SDK puts first in the text of a result it has offloaded.
OFFLOADED_MARK = "[Offloaded:"

#: Characters per token the SDK's own preview slicing assumes (`ContextOffloader._CHARS_PER_TOKEN`).
CHARS_PER_TOKEN = 4
#: Fraction of the limit kept as the preview: the SDK's own default (750 of 1,500 tokens).
PREVIEW_SHARE = 0.5
HEAD_SHARE = 0.55
MIN_TOKENS = 250
KEEP_DAYS = 7

#: Tools whose result *is* the answer, relayed to the person: never set aside. Specialists are added by the caller.
NEVER_OFFLOAD = frozenset(
    {
        "claude_code_engineer",
        "orchestrate",
        "a2a_client",
        "context_digest",
        "skill_forge",
        "proposer",
        "enable_tools",
        "search_skills",
        RETRIEVE_TOOL,
    }
)


def root() -> Path:
    import settings_store  # noqa: PLC0415 — keeps this module importable on its own

    return settings_store.user_dir() / "offloaded"


def _private_dir(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    try:
        os.chmod(path, 0o700)
    except OSError:
        logger.debug("could not tighten %s", path, exc_info=True)


class PrivateFileStorage(LocalFileStorage):
    """`LocalFileStorage` that is never routed through the agent's sandbox and leaves nothing world-readable.

    The base directory is created 0700 before anything is written, so the subdirectories the SDK makes beneath it
    cannot be reached by another user whatever their own mode; each file is also tightened to 0600."""

    def __init__(self, base_dir: str | os.PathLike) -> None:
        _private_dir(Path(base_dir))
        super().__init__(str(base_dir))

    def for_sandbox(self, sandbox: Any) -> PrivateFileStorage:
        return self

    async def write(self, key: str, data: bytes) -> None:
        await super().write(key, data)
        try:
            os.chmod(self._path_for(key.strip("/")), 0o600)
        except OSError:
            logger.debug("could not tighten the mode of an offloaded file", exc_info=True)


def head_tail(text: str, limit: int) -> str:
    """`text` cut to about `limit` characters, keeping the beginning and the end (where an exit code or a summary
    line lives) and saying how much is in the stored copy only. Text within the limit comes back as it is."""
    if len(text) <= limit:
        return text
    head = max(1, int(limit * HEAD_SHARE))
    tail = max(1, limit - head)
    return f"{text[:head].rstrip()}\n…[{len(text) - head - tail:,} characters of the middle are in the stored copy]…\n{text[-tail:].lstrip()}"


def is_offloaded(result: Any) -> bool:
    """True for a tool result the SDK's offloader has already replaced with a preview."""
    if not isinstance(result, dict):
        return False
    for block in result.get("content") or []:
        text = block.get("text") if isinstance(block, dict) else None
        if isinstance(text, str) and text.startswith(OFFLOADED_MARK):
            return True
    return False


class HarnessOffloader(ContextOffloader):
    """The SDK's `ContextOffloader`, with this harness's preview, limits and credential masking (module docstring)."""

    name = "context_offloader"

    def __init__(
        self,
        storage: LocalFileStorage,
        *,
        limit_chars: Callable[[], int],
        never: Iterable[str] = (),
        on_offload: Callable[[str, int], None] | None = None,
    ) -> None:
        self._limit_chars = limit_chars
        self._never = frozenset(NEVER_OFFLOAD | set(never))
        self._on_offload = on_offload
        self.offloaded = 0
        self.offloaded_chars = 0
        limit = self._limit_tokens()
        super().__init__(
            storage=storage,
            max_result_tokens=limit,
            preview_tokens=max(1, int(limit * PREVIEW_SHARE)),
            include_retrieval_tool=True,
            should_offload=self._should_offload_tool,
            evict_after_cycles=None,
        )

    # -- limits ----------------------------------------------------------------------

    def _limit_tokens(self) -> int:
        try:
            chars = int(self._limit_chars())
        except Exception:
            chars = 0
        return max(MIN_TOKENS, (chars or 6_000) // CHARS_PER_TOKEN)

    def _sync_limits(self) -> None:
        limit = self._limit_tokens()
        self._max_result_tokens = limit
        self._preview_tokens = max(1, int(limit * PREVIEW_SHARE))

    def _should_offload_tool(self, tool_name: str, token_count: int, **kwargs: Any) -> bool:
        return tool_name not in self._never

    # -- the hook --------------------------------------------------------------------

    @hook
    async def _handle_tool_result(self, event: AfterToolCallEvent) -> None:  # type: ignore[override]
        """The SDK's interception, after masking what the stored copy must not contain.

        Plugin hooks are registered after the agent's own, so on `AfterToolCallEvent` (reverse order) this runs before
        `SecretRedactionHooks` — the result is masked here too, or the credential would reach the disk."""
        if not isinstance(event.result, dict) or event.tool_use.get("name") in self._never:
            return
        self._sync_limits()
        secret_guard.redact_in_place(event.result)
        before = _size(event.result)
        await super()._handle_tool_result(event)
        if is_offloaded(event.result):
            self.offloaded += 1
            self.offloaded_chars += max(0, before - _size(event.result))
            if self._on_offload is not None:
                try:
                    self._on_offload(str(event.tool_use.get("name") or ""), before)
                except Exception:
                    logger.debug("offload notification failed", exc_info=True)

    def _slice_preview(self, text: str) -> str:
        return head_tail(text, self._preview_tokens * CHARS_PER_TOKEN)


def _size(result: dict) -> int:
    total = 0
    for block in result.get("content") or []:
        if isinstance(block, dict):
            total += len(str(block.get("text") or block.get("json") or ""))
    return total


# ---------------------------------------------------------------------------- housekeeping

_PRUNED = False


def prune(*, days: int = KEEP_DAYS, now: float | None = None, base: Path | None = None) -> int:
    """Delete the offloaded output of sessions untouched for `days`. Returns how many sessions went."""
    base = base or root()
    cutoff = (now if now is not None else time.time()) - days * 86400
    removed = 0
    try:
        entries = list(base.iterdir())
    except OSError:
        return 0
    for entry in entries:
        try:
            if entry.is_dir() and _newest(entry) < cutoff:
                shutil.rmtree(entry, ignore_errors=True)
                removed += 1
        except OSError:
            continue
    return removed


def _newest(path: Path) -> float:
    newest = path.stat().st_mtime
    for dirpath, _dirs, files in os.walk(path):
        for name in files:
            try:
                newest = max(newest, os.stat(os.path.join(dirpath, name)).st_mtime)
            except OSError:
                continue
    return newest


def build(
    session_id: str,
    *,
    preferences: Callable[[], Any] | None = None,
    never: Iterable[str] = (),
    default_chars: int = 6_000,
    on_offload: Callable[[str, int], None] | None = None,
) -> HarnessOffloader | None:
    """The offloader for one session, or None when `context.offload` is off.

    `preferences` is read for `context.offload` once, here, and `context.toolOutputChars` before every result."""
    global _PRUNED
    try:
        prefs = preferences() if preferences is not None else None
        enabled = prefs.get("context.offload", True) if prefs is not None else True
    except Exception:
        prefs, enabled = None, True
    if enabled is False:
        return None
    if not _PRUNED:
        _PRUNED = True
        prune()

    def limit() -> int:
        try:
            value = preferences().get("context.toolOutputChars", default_chars) if preferences is not None else default_chars
            return int(float(value))
        except Exception:
            return default_chars

    safe = "".join(ch if ch.isalnum() or ch in "-_" else "_" for ch in session_id) or "session"
    _private_dir(root())
    return HarnessOffloader(PrivateFileStorage(root() / safe), limit_chars=limit, never=never, on_offload=on_offload)

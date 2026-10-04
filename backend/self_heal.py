"""Autonomous self-healing: a failed command becomes a diagnosed, sandbox-
verified patch and a verification re-run, without waiting for a human — and
without bypassing a single approval rule.

Built on Strands hook events (async callbacks, awaited on the event loop):

``AfterToolCallEvent`` — a ``shell`` call exited non-zero (a failing test,
    build or script):
    1. capture the stderr/stdout trace (exit code, command, output tail);
    2. ask the Debugger specialist (read-only tools + LSP diagnostics) for a
       root cause and a unified-diff patch, passing earlier failed attempts in
       explicitly — specialists are stateless;
    3. **verify in a sandbox**: copy the project to a throwaway directory
       (tracked + untracked-not-ignored files; heavy dependency dirs such as
       ``target`` / ``node_modules`` / ``.venv`` are symlinked, not copied),
       apply the patch there and run the verification command through a Strands
       ``Sandbox``. A patch that fails verification goes back to the Debugger
       with the new output — up to ``selfHeal.maxAttempts`` rounds — so the
       real tree is never touched by an unverified guess;
    4. append the outcome to the failed tool's result, so the orchestrator's
       very next step sees the root cause and the verified patch, with the
       instruction to apply it (``apply_patch`` — gated like any edit) and
       re-run the command in the real workspace.
    A later run of the same command that succeeds closes the episode (healed).

``AfterToolCallEvent`` on ``editor`` / ``apply_patch`` errors — a state
    mismatch (the file changed under the model): append a re-read instruction.

``AfterInvocationEvent`` — the turn ended while an episode is still open
    (the model stopped before verifying): set ``event.resume`` so the agent
    continues on its own, bounded by the attempt budget. When the budget runs
    out the episode is escalated: the model is told to stop and report.

Approvals stay where they are. With ``selfHeal.autoApply`` the session's
approval handler may pre-approve exactly two things for an open episode: an
``apply_patch`` whose text equals the sandbox-verified patch, and a re-run of
the exact command that failed (it already ran once). ``deny`` rules still win;
plan mode still blocks everything.

Every phase is announced on the harness bus (``heal`` events) — acp-tui shows
an attempt counter, the root cause and the patch as a coloured diff.
"""

from __future__ import annotations

import asyncio
import logging
import os
import re
import shutil
import subprocess
import tempfile
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from strands.hooks import AfterInvocationEvent, AfterToolCallEvent, HookProvider, HookRegistry

import harness_events
import patch_tool
from laya_router import command_ok, normalize_command, shell_commands

logger = logging.getLogger(__name__)

_OUTPUT_TAIL = 4000
_LINKED_DIRS = ("target", "node_modules", ".venv", "venv", ".gradle", "build", "dist", ".next", "vendor")
_HEAL_TAG = "[self-heal]"


@dataclass
class HealConfig:
    enabled: bool = True
    max_attempts: int = 3
    verify: str = "sandbox"  # sandbox | workspace | off
    auto_apply: bool = False
    verify_timeout: float = 300.0
    auto_continue: bool = True


@dataclass
class Episode:
    command: str
    attempts: list[str] = field(default_factory=list)
    status: str = "open"  # open | healed | escalated
    patch: str | None = None  # the last verified (or proposed) patch
    verified: bool = False
    root_cause: str = ""
    verify_command: str | None = None
    continuations: int = 0
    started: float = field(default_factory=time.monotonic)


def result_text(result: Any) -> str:
    if not isinstance(result, dict):
        return str(result)
    return "\n".join(c.get("text", "") for c in result.get("content") or [] if isinstance(c, dict))


def failure_report(command: str, result: Any) -> str:
    text = result_text(result)
    if len(text) > _OUTPUT_TAIL:
        text = "…" + text[-_OUTPUT_TAIL:]
    return f"Command: {command}\nWorking directory: {os.getcwd()}\n{text}"


_MISMATCH = re.compile(
    r"(does not match|did not appear|not found in|no replacement was performed|no match|old_str .* not found|"
    r"cannot delete, file does not exist|no such file)",
    re.IGNORECASE,
)


def edit_failure_note(tool: str, text: str) -> str | None:
    """A note for a failed edit, by cause: the file isn't what the model
    thought (re-read it), or the patch wasn't in a format apply_patch reads.
    Other errors (an unknown editor command, say) speak for themselves."""
    if _MISMATCH.search(text):
        return (
            f"{_HEAL_TAG} State mismatch: the file is not what you expected, so the edit did not apply. "
            "Re-read it with file_read (or lsp_diagnostics) before editing again; don't retry the same edit blindly."
        )
    if tool == "apply_patch" and "no file changes found" in text:
        return (
            f"{_HEAL_TAG} That patch had no hunks apply_patch could read. Use a unified diff — `--- a/path`, "
            "`+++ b/path`, then `@@` hunks whose context lines start with a space — or edit with editor str_replace."
        )
    return None


def command_missing(result: Any) -> bool:
    text = result_text(result)
    return bool(re.search(r"Exit Code:\s*127\b", text)) or "command not found" in text


# ------------------------------------------------------------- the sandbox


class ShadowWorkspace:
    """A disposable copy of the project to verify a patch in."""

    def __init__(self, root: str | os.PathLike) -> None:
        self.root = Path(root).resolve()
        self.path: Path | None = None
        self._tmp: tempfile.TemporaryDirectory | None = None

    def __enter__(self) -> "ShadowWorkspace":
        self._tmp = tempfile.TemporaryDirectory(prefix="sierralab-heal-")
        self.path = Path(self._tmp.name) / self.root.name
        self.path.mkdir()
        # "__pycache__" must not ride along either: its .pyc files record the
        # pre-patch source's mtime (whole seconds) in their header, and a patch
        # applied seconds later in the same wall-clock second can leave the
        # stale compiled byte-code "valid" — the sandbox then verifies against
        # the OLD code and a broken fix can look verified (observed live: a
        # just-patched calc.py still returned -1 because the copied
        # calc.cpython-312.pyc beat the recompile). Same for the other caches,
        # which hold no code worth reusing but can pin stale state.
        files = self._git_files()
        if files is None:
            shutil.copytree(
                self.root,
                self.path,
                dirs_exist_ok=True,
                symlinks=True,
                ignore=shutil.ignore_patterns(*_LINKED_DIRS, ".git", "__pycache__", ".pytest_cache", ".mypy_cache", ".ruff_cache", ".ipynb_checkpoints"),
            )
        else:
            for rel in files:
                if "__pycache__" in Path(rel).parts:
                    continue
                source = self.root / rel
                if not source.is_file():
                    continue
                target = self.path / rel
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, target, follow_symlinks=False)
        for name in _LINKED_DIRS:
            source = self.root / name
            if source.is_dir() and not (self.path / name).exists():
                (self.path / name).symlink_to(source, target_is_directory=True)
        return self

    def _git_files(self) -> list[str] | None:
        try:
            done = subprocess.run(
                ["git", "ls-files", "-co", "--exclude-standard", "-z"],
                cwd=self.root,
                capture_output=True,
                timeout=20,
            )
        except (OSError, subprocess.TimeoutExpired):
            return None
        if done.returncode != 0:
            return None
        return [p for p in done.stdout.decode("utf-8", "replace").split("\0") if p]

    def apply(self, patch: str) -> str:
        assert self.path is not None
        return patch_tool.apply(patch, self.path)

    async def run(self, command: str, timeout: float) -> tuple[int, str]:
        """Run ``command`` in the copy through a Strands Sandbox."""
        from strands.sandbox.not_a_sandbox_local_environment import NotASandboxLocalEnvironment

        sandbox = NotASandboxLocalEnvironment()
        try:
            result = await sandbox.execute(command, cwd=str(self.path), timeout=timeout)
        except Exception as error:  # timeout or spawn failure
            return 124, f"{type(error).__name__}: {error}"
        output = (result.stdout or "") + (("\n" + result.stderr) if result.stderr else "")
        return result.exit_code, output

    def __exit__(self, *exc) -> None:
        if self._tmp is not None:
            self._tmp.cleanup()


# ---------------------------------------------------------------- the hooks


class SelfHealHooks(HookProvider):
    def __init__(
        self,
        *,
        config: Callable[[], HealConfig] | HealConfig | None = None,
        diagnose: Callable[..., Any] | None = None,
        session_id: str | None = None,
        root: str | None = None,
    ) -> None:
        self._config = config if callable(config) else (lambda c=config or HealConfig(): c)
        if diagnose is None:
            from specialists import diagnose as default_diagnose

            diagnose = default_diagnose
        self._diagnose = diagnose
        self.session_id = session_id
        self.root = root
        self.episodes: dict[str, Episode] = {}

    @property
    def config(self) -> HealConfig:
        return self._config()

    def register_hooks(self, registry: HookRegistry) -> None:
        registry.add_callback(AfterToolCallEvent, self._after_tool)
        registry.add_callback(AfterInvocationEvent, self._on_end)

    # ------------------------------------------------------------- queries

    def open_episodes(self) -> list[Episode]:
        return [ep for ep in self.episodes.values() if ep.status == "open"]

    def preapproved(self, tool_name: str, tool_input: Any) -> bool:
        """True only for the two calls ``selfHeal.autoApply`` may pre-approve."""
        if not self.config.auto_apply:
            return False
        if tool_name == "apply_patch" and isinstance(tool_input, dict):
            patch = str(tool_input.get("patch") or "").strip()
            return any(ep.verified and ep.patch and ep.patch.strip() == patch for ep in self.open_episodes())
        if tool_name == "shell":
            commands = shell_commands(tool_input)
            wanted = {normalize_command(ep.command) for ep in self.open_episodes()}
            wanted |= {normalize_command(ep.verify_command) for ep in self.open_episodes() if ep.verify_command}
            return bool(commands) and normalize_command(" && ".join(commands)) in wanted
        return False

    def _emit(self, phase: str, episode: Episode | None = None, **extra: Any) -> None:
        payload: dict[str, Any] = {"event": "heal", "phase": phase, "max": self.config.max_attempts}
        if episode is not None:
            payload.update({"command": episode.command, "attempt": len(episode.attempts)})
        payload.update(extra)  # an explicit attempt number (the one in progress) wins
        if self.session_id:
            payload["sessionId"] = self.session_id
        harness_events.emit("heal", payload)

    # --------------------------------------------------------------- hooks

    async def _after_tool(self, event: AfterToolCallEvent) -> None:
        config = self.config
        if not config.enabled:
            return
        name = event.tool_use.get("name")
        if name in ("editor", "apply_patch") and isinstance(event.result, dict) and event.result.get("status") == "error":
            note = edit_failure_note(name, result_text(event.result))
            if note:
                self._append(event, note)
                self._emit("state_mismatch", None, tool=name)
            return
        if name != "shell":
            return
        commands = shell_commands(event.tool_use.get("input"))
        if not commands:
            return
        command = " && ".join(commands)
        key = normalize_command(command)
        ok = command_ok(event.result)
        episode = self.episodes.get(key) or next(
            (ep for ep in self.open_episodes() if ep.verify_command and normalize_command(ep.verify_command) == key), None
        )
        if ok:
            if episode is not None and episode.status == "open":
                episode.status = "healed"
                self._emit("healed", episode, seconds=round(time.monotonic() - episode.started, 1))
                self._append(event, f"{_HEAL_TAG} Verified: `{command}` now succeeds. The failure is fixed.")
            return
        if ok is None:
            return
        if command_missing(event.result):
            # Exit 127: the program doesn't exist here. No code change fixes that,
            # so don't spend a Debugger round on it — say what probably happened.
            self._append(
                event,
                f"{_HEAL_TAG} `{commands[0].split()[0] if commands[0].split() else command}` is not a command on this machine "
                "(exit 127). If you meant one of your tools (apply_patch, editor, …), call the tool directly, not through shell.",
            )
            self._emit("command_missing", None, command=command)
            return
        if episode is None or episode.status != "open":
            episode = self.episodes[key] = Episode(command=command)
            self._emit("detected", episode, output_tail=result_text(event.result)[-600:])
        if len(episode.attempts) >= config.max_attempts:
            episode.status = "escalated"
            self._emit("escalated", episode)
            self._append(
                event,
                f"{_HEAL_TAG} {config.max_attempts} self-heal attempts did not fix `{command}`. Stop retrying: tell the user "
                "what was tried (root causes below) and what you think is needed.\n" + "\n".join(episode.attempts),
            )
            return
        note = await self._heal(episode, failure_report(command, event.result))
        self._append(event, note)

    async def _heal(self, episode: Episode, failure: str) -> str:
        """Diagnose → (sandbox-verify → re-diagnose)* → a note for the model."""
        config = self.config
        root = self.root or os.getcwd()
        last_output = failure
        while len(episode.attempts) < config.max_attempts:
            attempt = len(episode.attempts) + 1
            self._emit("diagnosing", episode, attempt=attempt)
            diagnosis = await asyncio.to_thread(
                self._diagnose, last_output, list(episode.attempts), root=root, session_id=self.session_id
            )
            episode.root_cause = diagnosis.root_cause
            episode.verify_command = diagnosis.verify or episode.command
            if not diagnosis.actionable:
                episode.attempts.append(f"attempt {attempt}: root cause {diagnosis.root_cause!r}; no applicable patch ({diagnosis.patch_error or 'none proposed'})")
                self._emit("no_patch", episode, attempt=attempt, root_cause=diagnosis.root_cause, error=diagnosis.patch_error)
                if diagnosis.patch is None and not diagnosis.patch_error:
                    break  # environmental: more rounds won't produce a code fix
                continue
            episode.patch, episode.verified = diagnosis.patch, False
            self._emit("proposed", episode, attempt=attempt, root_cause=diagnosis.root_cause, patch=diagnosis.patch, verify=episode.verify_command)
            if config.verify != "sandbox":
                episode.attempts.append(f"attempt {attempt}: root cause {diagnosis.root_cause!r}; patch proposed (not sandbox-verified)")
                return self._note_unverified(episode)
            self._emit("verifying", episode, attempt=attempt, verify=episode.verify_command)
            task = harness_events.Task(f"self-heal: verifying in sandbox ({attempt}/{config.max_attempts})", session_id=self.session_id, source="heal")
            try:
                with ShadowWorkspace(root) as shadow:
                    await asyncio.to_thread(shadow.apply, diagnosis.patch)
                    code, output = await shadow.run(episode.verify_command, config.verify_timeout)
            except Exception as error:
                code, output = 1, f"sandbox error: {type(error).__name__}: {error}"
            task.finish("completed" if code == 0 else "failed", f"exit {code}")
            if code == 0:
                episode.verified = True
                episode.attempts.append(f"attempt {attempt}: root cause {diagnosis.root_cause!r}; patch verified in sandbox")
                self._emit("verified", episode, attempt=attempt, root_cause=diagnosis.root_cause, patch=diagnosis.patch, verify=episode.verify_command)
                return self._note_verified(episode)
            tail = output[-_OUTPUT_TAIL:]
            episode.attempts.append(
                f"attempt {attempt}: root cause {diagnosis.root_cause!r}; this patch still failed `{episode.verify_command}` "
                f"(exit {code}):\n{diagnosis.patch}\nOutput tail:\n{tail[-1500:]}"
            )
            self._emit("verify_failed", episode, attempt=attempt, exit_code=code, output_tail=tail[-600:])
            last_output = f"Command: {episode.verify_command} (after applying the previous patch in a sandbox copy)\nExit code: {code}\n{tail}"
        episode.status = "escalated"
        self._emit("escalated", episode, root_cause=episode.root_cause)
        return (
            f"{_HEAL_TAG} The Debugger could not produce a verified fix in {len(episode.attempts)} attempt(s). "
            "Stop retrying; tell the user what was tried:\n" + "\n".join(a.splitlines()[0] for a in episode.attempts)
        )

    def _note_verified(self, episode: Episode) -> str:
        return (
            f"{_HEAL_TAG} Debugger diagnosis — ROOT CAUSE: {episode.root_cause}\n"
            f"This patch was applied in a sandbox copy of the project and `{episode.verify_command}` passed there:\n"
            f"```diff\n{episode.patch.rstrip()}\n```\n"
            f"Next: apply exactly this patch with apply_patch, then re-run `{episode.verify_command}` to verify it in the real workspace."
        )

    def _note_unverified(self, episode: Episode) -> str:
        return (
            f"{_HEAL_TAG} Debugger diagnosis — ROOT CAUSE: {episode.root_cause}\n"
            f"Proposed patch (dry-run applies cleanly; not yet verified):\n```diff\n{(episode.patch or '').rstrip()}\n```\n"
            f"Next: apply it with apply_patch, then re-run `{episode.verify_command}` to verify."
        )

    @staticmethod
    def _append(event: AfterToolCallEvent, note: str) -> None:
        result = event.result if isinstance(event.result, dict) else {"status": "error", "content": []}
        content = list(result.get("content") or [])
        content.append({"text": note})
        event.result = {**result, "content": content}

    def _on_end(self, event: AfterInvocationEvent) -> None:
        config = self.config
        if not (config.enabled and config.auto_continue):
            return
        if getattr(event.result, "stop_reason", None) != "end_turn":
            return
        pending = [ep for ep in self.open_episodes() if ep.continuations < config.max_attempts]
        if not pending:
            return
        episode = pending[0]
        episode.continuations += 1
        verify = episode.verify_command or episode.command
        if episode.patch:
            action = "apply the Debugger's verified patch with apply_patch" if episode.verified else "apply the Debugger's patch with apply_patch"
        else:
            action = "fix the cause"
        event.resume = (
            f"{_HEAL_TAG} `{episode.command}` is still failing and the fix has not been verified in the workspace. "
            f"Continue autonomously: {action}, then re-run `{verify}` to verify. "
            f"(continuation {episode.continuations}/{config.max_attempts})"
        )
        self._emit("continuing", episode, continuation=episode.continuations)

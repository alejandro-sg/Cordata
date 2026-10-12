"""``cordata doctor`` — is everything this tool leans on actually there, and what is the one command that fixes each gap?

Every check is local and spends nothing: a binary on ``PATH``, a file, a port that answers, a setting. Nothing is sent to
a model or a provider, and no secret is ever read — a key is only ever asked about ("is there one?"), never printed.

The point is the first five minutes: a tool that depends on two coding CLIs, a model server, a daemon and a vault
should say, in one screen, which of them is missing and how to fix it, instead of failing on the third turn with a
traceback.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import urllib.error
import urllib.request
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

OK, WARN, FAIL = "ok", "warn", "fail"

BACKEND = Path(__file__).resolve().parent.parent
REPO = BACKEND.parent


@dataclass
class Check:
    name: str
    level: str
    detail: str
    fix: str = ""

    def to_json(self) -> dict:
        return {"name": self.name, "level": self.level, "detail": self.detail, "fix": self.fix}


def _run(argv: list[str], timeout: float = 5.0) -> str | None:
    try:
        done = subprocess.run(argv, capture_output=True, text=True, timeout=timeout, stdin=subprocess.DEVNULL)
    except (OSError, subprocess.TimeoutExpired):
        return None
    return (done.stdout or done.stderr or "").strip() if done.returncode == 0 else None


def _which(name: str, env_var: str, fallback: str) -> str | None:
    override = os.environ.get(env_var)
    if override and os.path.exists(override):
        return override
    return shutil.which(name) or (os.path.expanduser(fallback) if os.path.exists(os.path.expanduser(fallback)) else None)


def _ping(url: str, timeout: float = 1.5) -> bool:
    """Does something answer at ``url``? Any HTTP reply counts — a 401 from a daemon that wants its token is a daemon."""
    try:
        with urllib.request.urlopen(url, timeout=timeout) as response:  # noqa: S310 — loopback health probes only
            return 200 <= response.status < 500
    except urllib.error.HTTPError as error:
        return error.code < 500
    except (OSError, urllib.error.URLError, ValueError):
        return False


def check_python() -> Check:
    python = BACKEND / ".venv" / "bin" / "python"
    if not python.exists():
        return Check("python environment", FAIL, f"no virtualenv at {python}", "make setup")
    version = _run([str(python), "--version"]) or "?"
    return Check("python environment", OK, version)


def check_tui() -> Check:
    for kind in ("release", "debug"):
        binary = REPO / "tui" / "target" / kind / "acp-tui"
        if binary.exists():
            return Check("terminal client", OK, f"acp-tui ({kind} build)")
    return Check("terminal client", FAIL, "acp-tui is not built", "make tui-build")


def check_cli_linked() -> Check:
    found = shutil.which("cordata")
    if found:
        return Check("cordata on PATH", OK, found)
    return Check("cordata on PATH", WARN, "`cordata` is not on PATH — only runnable from the repo's bin/", "make cli   (links cordata into ~/.local/bin)")


def check_claude() -> Check:
    binary = _which("claude", "CORDATA_CLAUDE_BIN", "~/.local/bin/claude")
    if not binary:
        return Check("Claude Code CLI", WARN, "not installed — its pool stays out of the roster", "install Claude Code, then sign in once with `claude`")
    version = _run([binary, "--version"]) or "unknown version"
    return Check("Claude Code CLI", OK, f"{version} ({binary})")


def check_agy() -> Check:
    binary = _which("agy", "CORDATA_AGY_BIN", "~/.local/bin/agy")
    if not binary:
        return Check("Antigravity CLI (agy)", WARN, "not installed — its pool stays out of the roster", "install the Antigravity CLI, then sign in once with `agy`")
    version = (_run([binary, "--version"]) or "unknown version").splitlines()[0]
    return Check("Antigravity CLI (agy)", OK, f"{version} ({binary})")


def check_ollama() -> Check:
    base = os.environ.get("OLLAMA_BASE_URL", "http://localhost:11434").rstrip("/")
    if _ping(base + "/api/version"):
        return Check("Ollama", OK, f"answering at {base}")
    if shutil.which("ollama"):
        return Check("Ollama", WARN, "installed but not answering — the Ollama workers are out of the roster", "ollama serve   (or open the Ollama app)")
    return Check("Ollama", WARN, "not installed — the Ollama workers are out of the roster", "https://ollama.com/download")


def _configured_provider() -> str:
    """``MODEL_PROVIDER`` from the environment, else from ``backend/.env`` (that one key only; nothing else is read)."""
    provider = os.environ.get("MODEL_PROVIDER", "").strip()
    env_file = Path(__file__).resolve().parent.parent / ".env"
    if not provider and env_file.is_file():
        for line in env_file.read_text(errors="replace").splitlines():
            name, _, value = line.partition("=")
            if name.strip() == "MODEL_PROVIDER":
                provider = value.strip().strip("\"'")
    return provider or "local"


def check_model_key() -> Check:
    provider = _configured_provider()
    needs_key = provider in ("openai_compatible", "anthropic")
    if any(os.environ.get(name) for name in ("OLLAMA_API_KEY", "CLOUD_OPENAI_API_KEY", "ANTHROPIC_API_KEY")):
        return Check("model API key", OK, "found in the environment")
    if shutil.which("security"):
        for service in ("cordata-ollama-api-key", "cordata-anthropic-api-key", "sierralab-ollama-api-key", "sierralab-anthropic-api-key"):
            # Only whether an entry exists: no -w / -g, so the secret itself is never read.
            if subprocess.run(["security", "find-generic-password", "-s", service], capture_output=True).returncode == 0:
                return Check("model API key", OK, f"in the macOS Keychain ({service})")
    if needs_key:
        # `cordata` itself cannot start a conversation without it, so this is a problem, not a hint.
        return Check("model API key", FAIL, f"MODEL_PROVIDER={provider} needs a key and none was found — `cordata` cannot open a conversation", "make key   (hidden prompt, stored in the Keychain, never in a file)")
    return Check("model API key", WARN, f"no key for a cloud model provider (fine: MODEL_PROVIDER={provider} does not need one)", "make key   (hidden prompt, stored in the Keychain, never in a file)")


def check_daemon() -> Check:
    home = Path(os.environ.get("CORDATA_HOME") or Path.home() / ".cordata")
    path = home / "daemon.json"
    if path.exists():
        try:
            port = json.loads(path.read_text()).get("port")
        except (OSError, ValueError):
            port = None
        if port and _ping(f"http://127.0.0.1:{port}/health"):
            return Check("daemon", OK, f"running on port {port} — the terminal, Discord and the menu bar share its conversations")
        return Check("daemon", WARN, "configured but not answering — the terminal runs standalone", "make daemon-status   (make daemon-install keeps it up)")
    return Check("daemon", WARN, "not set up — the terminal runs standalone (fine for one terminal)", "make daemon-install")


def check_ripgrep() -> Check:
    if shutil.which("rg"):
        return Check("ripgrep", OK, shutil.which("rg") or "")
    return Check("ripgrep", WARN, "code search falls back to a slower Python search", "brew install ripgrep")


def check_git(cwd: str) -> Check:
    if not shutil.which("git"):
        return Check("git", FAIL, "git is not installed — checkpoints and the orchestrator need it", "xcode-select --install")
    if _run(["git", "-C", cwd, "rev-parse", "--is-inside-work-tree"]) == "true":
        branch = _run(["git", "-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"]) or "?"
        return Check("git", OK, f"{cwd} is a repository ({branch})")
    return Check("git", WARN, f"{cwd} is not a git repository — undo and the orchestrator's rollback have nothing to lean on", "git init")


def check_mcp_servers(path: Path | None = None) -> Check:
    path = path or BACKEND / "mcp_servers.json"
    if not path.exists():
        return Check("MCP servers", OK, "none configured")
    try:
        servers = json.loads(path.read_text()).get("mcpServers", {})
    except (OSError, ValueError) as error:
        return Check("MCP servers", FAIL, f"mcp_servers.json is not valid JSON ({error})", f"edit {path}")
    broken = []
    for name, server in servers.items():
        command = server.get("command")
        if command and os.path.isabs(command) and not os.path.exists(command):
            broken.append(f"{name} → {command}")
        for arg in server.get("args", []):
            if isinstance(arg, str) and os.path.isabs(arg) and arg.endswith((".py", ".js", ".mjs", ".ts")) and not os.path.exists(arg):
                broken.append(f"{name} → {arg}")
    if broken:
        return Check("MCP servers", FAIL, "a path no longer exists (moved or renamed repo?): " + "; ".join(broken), f"fix the paths in {path}")
    return Check("MCP servers", OK, f"{len(servers)} configured, every path exists")


def check_pools(prefs: Any) -> Check:
    try:
        configured = prefs.get("orchestrator.pools") if prefs is not None else None
    except (AttributeError, TypeError):
        configured = None
    if configured:
        return Check("quota pools", OK, f"{len(configured)} set in settings")
    return Check("quota pools", WARN, "using the conservative built-in caps — set `orchestrator.pools` to your actual plans so handoffs happen at the right moment", "see docs/orchestrator.md (\"Using it\")")


def check_vault(prefs: Any) -> Check:
    enabled = bool(prefs.get("vault.enabled", False)) if prefs is not None else False
    if enabled:
        return Check("agent vault", OK, "on — tool subprocesses reach the network through the credential-substituting proxy")
    if os.environ.get("CORDATA_VAULT_DIRECT"):
        return Check("agent vault", WARN, "off, and direct egress is allowed for orchestrator runs (CORDATA_VAULT_DIRECT)", "")
    return Check("agent vault", WARN, "off — `cordata run` refuses to start the coding CLIs without it unless you pass --allow-direct", "set vault.enabled and start the vault, or run with --allow-direct while developing")


def check_settings(prefs: Any) -> Check:
    warnings = list(getattr(prefs, "warnings", []) or [])
    if warnings:
        return Check("settings files", WARN, f"{len(warnings)} problem(s): {warnings[0]}", "/config in the terminal client shows which file")
    return Check("settings files", OK, "valid")


def run_checks(cwd: str, prefs: Any = None) -> list[Check]:
    if prefs is None:
        try:
            import settings_store

            prefs = settings_store.current(cwd)
        except Exception:
            prefs = None
    steps: list[Callable[[], Check]] = [
        check_python, check_tui, check_cli_linked, check_claude, check_agy, check_ollama, check_model_key, check_daemon,
        check_ripgrep, lambda: check_git(cwd), check_mcp_servers, lambda: check_pools(prefs), lambda: check_vault(prefs), lambda: check_settings(prefs),
    ]
    results: list[Check] = []
    for step in steps:
        try:
            results.append(step())
        except Exception as error:  # a check that breaks is itself a finding, never a crash
            results.append(Check(getattr(step, "__name__", "check"), WARN, f"could not check: {error}"))
    return results


MARK = {OK: "✓", WARN: "⚠", FAIL: "✗"}


def render(checks: list[Check], color: bool = False) -> str:
    paint = {OK: "32", WARN: "33", FAIL: "31"}
    width = max(len(c.name) for c in checks)
    lines = []
    for check in checks:
        mark = f"\x1b[{paint[check.level]}m{MARK[check.level]}\x1b[0m" if color else MARK[check.level]
        lines.append(f"  {mark} {check.name:<{width}}  {check.detail}")
        if check.fix and check.level != OK:
            lines.append(f"    {' ' * width}  → {check.fix}")
    failed = sum(c.level == FAIL for c in checks)
    warned = sum(c.level == WARN for c in checks)
    summary = "everything this tool leans on is in place" if not (failed or warned) else f"{failed} problem(s), {warned} to look at"
    return "cordata doctor\n" + "\n".join(lines) + f"\n\n{summary}"

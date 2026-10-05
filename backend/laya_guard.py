"""Laya Guard — a destructiveness score for every tool call, from every agent.

``laya_risk`` scores the **turn**: Laya reads the user's message once, at the start, and the
orchestrator's reading arms the approval gate for everything that turn goes on to do. Its own
docstring says why that cannot be allowed to *waive* a prompt: the call under approval is chosen by
a model that may since have read a web page, a file or a tool result, and a turn that began
"summarise this README" would wave through whatever the README talked the model into running. The
same is true in ``auto`` mode — which has no prompts to waive and so leans entirely on the turn's one
reading — and it is true of every specialist, which is judged by the *orchestrator's* turn, not its own
call.

This module reads the risk **per call**, at the moment the call is about to run, for whichever agent
is making it:

1. **Rules** — a deterministic reading of the concrete command, path or patch. ``rm -rf /``,
   ``git clean -fdx``, ``git push --force``, a write to ``~/.ssh`` or outside the project, a patch
   that deletes a file or thousands of lines. These are the reliable floor: the shipped Laya
   checkpoints are near chance until fine-tuned (``docs/laya-triage.md``), so nothing safety-relevant
   is allowed to depend on the model alone.
2. **Laya** — the same ``risk`` question the router asks about a turn, asked about this one call
   (``laya_predict``; one forward pass on weights that are already loaded — it never triggers a load).
   Its reading can only *raise* the score, and only when it calls the call destructive with the
   confidence the router itself requires to arm a turn (``RouteDecision.armed``: 0.8).

The score is compared with **that agent's gate**: the router's destructive threshold, tightened by the
pod's Lab dial (``lab.Lab.dial_armed``). A call at or above it asks a person, in every mode — including
``auto``, whose whole point is that nothing else does.

What this is not: it never *lowers* an ask. A low score does not approve anything; it just does not add
a prompt. Whether a call asks at all is still decided by the mode, the permission rules, the vault and
the forced asks in ``acp_server._ApprovalHandler.before_tool_call``, none of which this can remove.
Never raises: an unusable input scores as unknown, never as safe.
"""

from __future__ import annotations

import os
import re
import shlex
import threading
from collections import OrderedDict
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

import laya_risk
import secret_guard
import secret_patterns
import settings_store
from laya_router import RISK_LEVELS, shell_commands

#: ``laya.callGuard``: off | rules (deterministic only) | full (rules + Laya's own reading).
GUARD_MODES = ("off", "rules", "full")
DEFAULT_GUARD = "full"

#: The tools whose calls change something. Mirrors ``acp_server._ARMABLE_TOOLS`` (asserted equal in
#: ``_laya_guard_smoke_test``); a tool that only reads has nothing to guard.
GUARDED_TOOLS = frozenset({"shell", "python_repl", "file_write", "editor", "apply_patch", "claude_code_engineer", "ollama_resource_manager"})

#: A destructive reading from Laya counts only at the confidence the router needs to arm a turn.
LAYA_MIN_CONFIDENCE = 0.8

_EDITOR_READ = frozenset({"view", "read", "find", "search", "list"})

CALL_QUESTION = {
    "risk": {
        "type": "score",
        "instructions": "How risky is this single action that a coding agent is about to take?",
        "criteria": list(RISK_LEVELS),
    }
}


@dataclass(frozen=True)
class CallRisk:
    score: float
    band: str  # safe | elevated | destructive
    reason: str
    source: str  # rules | rules+laya
    confidence: float  # 1.0 for a rule; Laya's (calibrated) confidence for the model's reading
    tool: str

    def to_event(self) -> dict[str, Any]:
        """The shape ``laya_risk.RiskAssessment.to_event`` has, so everything that already renders a
        turn's reading (the TUI, the Lab, the ledger) renders a call's."""
        return {
            "event": "risk",
            "score": round(self.score, 3),
            "band": self.band,
            "label": {"safe": "read_only", "elevated": "edits_files", "destructive": "destructive"}[self.band],
            "confidence": round(self.confidence, 3),
            "reason": self.reason,
            "call": True,
            "source": self.source,
        }


# ------------------------------------------------------------------------------ settings


def guard_mode(preferences: Any = None) -> str:
    value = DEFAULT_GUARD
    if preferences is not None:
        try:
            value = str(preferences.get("laya.callGuard", DEFAULT_GUARD))
        except (AttributeError, TypeError):
            value = DEFAULT_GUARD
    return value if value in GUARD_MODES else DEFAULT_GUARD


# ------------------------------------------------------------------------------ paths


_HOME = str(Path.home())
_SENSITIVE_PATH = re.compile(
    r"(/\.ssh/|/\.aws/|/\.gnupg/|/\.kube/|/\.docker/config|/\.config/gcloud|/library/(launchagents|launchdaemons|keychains)/"
    r"|^/etc/|^/private/etc/|^/usr/|^/system/|^/var/root|^/library/"
    r"|/\.(zshrc|zshenv|zprofile|bashrc|bash_profile|profile|gitconfig|netrc|npmrc|pypirc)$"
    r"|/\.git/(hooks/|config$)|id_rsa|id_ed25519|authorized_keys|\.pem$|\.p12$|\.pfx$|\.keychain)",
    re.IGNORECASE,
)
_ENV_FILE = re.compile(r"(^|/)\.env(\.[^/]*)?$", re.IGNORECASE)
_ENV_TEMPLATE = re.compile(r"\.env\.(example|sample|template|dist)$", re.IGNORECASE)
#: Files that decide what the harness asks about: editing one is how a prompt would be talked out of existing.
_POLICY_FILE = re.compile(r"(^|/)(\.sierralab/settings(\.local)?\.json|\.claude/settings(\.local)?\.json|SIERRALAB\.md|\.mcp\.json)$", re.IGNORECASE)
_CI_FILE = re.compile(r"(^|/)(\.github/workflows/|\.gitlab-ci|Jenkinsfile|azure-pipelines|\.circleci/)", re.IGNORECASE)
_BUILD_FILE = re.compile(r"(^|/)(Dockerfile|docker-compose[^/]*|Makefile|setup\.py|pyproject\.toml|package\.json|Cargo\.toml)$", re.IGNORECASE)
_LOCK_FILE = re.compile(r"(^|/)(package-lock\.json|yarn\.lock|pnpm-lock\.yaml|Cargo\.lock|poetry\.lock|uv\.lock|Gemfile\.lock)$", re.IGNORECASE)
_TEST_OR_DOC = re.compile(r"(^|/)(tests?|docs?|spec|__tests__)/|(_test|\.test|\.spec)\.\w+$|(^|/)test_[^/]*$|\.(md|txt|rst)$", re.IGNORECASE)
#: Directories that exist to be rebuilt: removing them is routine, not destructive.
_REBUILDABLE = frozenset({"build", "dist", "target", "node_modules", "__pycache__", ".pytest_cache", ".mypy_cache", ".ruff_cache", ".next", ".turbo", "coverage", ".venv", "venv", ".tox", "out", ".cache"})


def _expand(path: str, cwd: str) -> str | None:
    """Absolute, user-expanded path, or None when it depends on a shell variable we cannot see."""
    text = path.strip().strip("'\"")
    if not text:
        return None
    if text.startswith("~"):
        text = _HOME + text[1:]
    elif text.startswith("$HOME") or text.startswith("${HOME}"):
        text = _HOME + re.sub(r"^\$\{?HOME\}?", "", text)
    if "$" in text or "`" in text:
        return None
    candidate = text if os.path.isabs(text) else os.path.join(cwd, text)
    return os.path.normpath(candidate)


def _inside(path: str, root: str) -> bool:
    root = os.path.normpath(root)
    return path == root or path.startswith(root.rstrip(os.sep) + os.sep)


def path_score(path: str, cwd: str) -> tuple[float, str]:
    """How much a write to ``path`` matters."""
    resolved = _expand(path, cwd)
    if resolved is None:
        return 0.85, f"writes to {path!r}, which depends on a shell variable"
    low = resolved.lower()
    if _ENV_FILE.search(low) and not _ENV_TEMPLATE.search(low):
        return 0.95, "touches a .env secrets file"
    if _SENSITIVE_PATH.search(low):
        return 0.95, f"writes to a sensitive location ({resolved})"
    if _POLICY_FILE.search(resolved):
        return 0.9, "edits a file that decides what the harness asks about"
    if not _inside(resolved, cwd):
        return 0.9, f"writes outside the project ({resolved})"
    rel = os.path.relpath(resolved, cwd)
    if _CI_FILE.search(rel):
        return 0.6, "edits CI/deploy configuration"
    if _BUILD_FILE.search(rel):
        return 0.45, "edits build configuration"
    if _LOCK_FILE.search(rel):
        return 0.4, "edits a lock file"
    if _TEST_OR_DOC.search(rel):
        return 0.25, "edits a test or doc"
    return 0.35, "edits a project file"


# ------------------------------------------------------------------------------ shell


_READ_ONLY_COMMANDS = frozenset(
    "ls ll la cat bat head tail less more wc grep egrep fgrep rg ag fd pwd which whereis type echo printf date tree du df uname whoami id stat file "
    "sort uniq cut tr diff cmp jq yq basename dirname realpath readlink true false test [ sleep uptime hostname ps column nl tac rev seq".split()
)
_GIT_READ_ONLY = frozenset("status diff log show ls-files rev-parse blame grep describe shortlog whatchanged cat-file ls-tree rev-list name-rev".split())
_RUNNERS = frozenset(
    "pytest py.test tox nox make cmake ninja tsc eslint prettier ruff mypy black flake8 isort gradle gradlew mvn mvnw bazel just task".split()
)
_SCRIPT_RUNNERS = frozenset("python python3 node deno bun ruby perl php bash sh zsh".split())

#: Commands that only look at a path's *existence or metadata* — they do not read the file's contents.
_PATH_ONLY = frozenset("ls ll la stat test [ pwd which whereis file du dirname basename realpath readlink find".split())


def _is_secret_path(token: str, cwd: str) -> bool:
    """Does this argument name a secrets file (a .env that is not a template, a private key, a credentials store)?"""
    if not token or token.startswith("-") and "=" not in token:
        return False
    candidate = token.split("=", 1)[1] if token.startswith("--") and "=" in token else token
    resolved = _expand(candidate.lstrip("<>"), cwd)
    if resolved is None:
        return False
    low = resolved.lower()
    return bool((_ENV_FILE.search(low) and not _ENV_TEMPLATE.search(low)) or _SENSITIVE_PATH.search(low))


_FORK_BOMB = re.compile(r":\s*\(\s*\)\s*\{")
#: `dd` destroys a disk only when it *writes to a device*. `of=./build/out.txt` is an ordinary file write and is
#: scored as one (`_dd` below); `of=/dev/null` and the standard streams are not storage at all.
_DD_DEVICE = re.compile(r"\bof=/dev/(?!null\b|zero\b|stdout\b|stderr\b|tty\b|fd/|std)")
_DISK = re.compile(r"\b(mkfs(\.\w+)?|diskutil\s+(erase|partition|secure|zero|reformat)\w*|wipefs|fdisk|shred)\b|\bdd\b[^|;&]*" + _DD_DEVICE.pattern + r"|>\s*/dev/(r?disk|sd)")
_PIPE_TO_SHELL = re.compile(r"\b(curl|wget|fetch)\b[^|;&]*\|\s*(sudo\s+)?(ba|z|da|k)?sh\b|\|\s*(sudo\s+)?(python3?|perl|ruby|node)\s+-")
_SQL_DESTRUCTIVE = re.compile(r"\b(drop|truncate)\s+(table|database|schema|index)\b|\bdelete\s+from\b(?!.*\bwhere\b)", re.IGNORECASE)
_CLOUD_DESTRUCTIVE = re.compile(
    r"\bkubectl\s+(delete|drain)\b|\bterraform\s+destroy\b|\baws\s+\S+\s+(delete|terminate|remove|rm)\S*|\bgcloud\s+.*\bdelete\b|\bheroku\s+.*\bdestroy\b|\bdocker\s+(system\s+prune|volume\s+(rm|prune)|rm\s+-f|rmi\s+-f)\b"
)
_PUBLISH = re.compile(r"\b(npm|pnpm|yarn)\s+publish\b|\bcargo\s+publish\b|\btwine\s+upload\b|\bgem\s+push\b|\bdocker\s+push\b|\bgh\s+(release\s+create|pr\s+merge|repo\s+delete)\b|\bterraform\s+apply\b")
_SYSTEM = re.compile(r"\blaunchctl\s+(unload|remove|bootout|disable)\b|\bdefaults\s+delete\b|\bsecurity\s+(delete|add)-\w+|\bcrontab\s+-r\b|\b(csrutil|spctl|scutil|dscl|pmset|networksetup)\b|\btmutil\s+delete")
_REDIRECT_SENSITIVE = re.compile(r"(^|[^<>&0-9])>>?\s*(/etc/|/usr/|/system/|/library/|/private/|~/\.|\$home/\.|\.\./)", re.IGNORECASE)
_REDIRECT_OVERWRITE = re.compile(r"(^|[^<>&0-9])>(?!>)\s*(?!/dev/null)\S")
_NET_WRITE = re.compile(r"\bcurl\b[^|;&]*(\s-X\s*(POST|PUT|DELETE|PATCH)\b|\s(-d|--data\S*|-F|--form|--upload-file|-T)\b)|\bwget\b[^|;&]*--(post|method)", re.IGNORECASE)
_REMOTE = re.compile(r"(^|\s)(ssh|scp|rsync|sftp|nc|ncat|telnet|socat)\s")
_INSTALL = re.compile(r"\b(pip3?|pipx|npm|pnpm|yarn|bun|gem|cargo|go)\s+(install|add|i|get)\b|\buv\s+(pip\s+install|add|tool\s+install)\b|\bbrew\s+(install|upgrade|reinstall)\b|\b(apt(-get)?|dnf|yum|port)\s+install\b")
_UNINSTALL = re.compile(r"\b(pip3?|npm|pnpm|yarn|brew|cargo)\s+(uninstall|remove|rm)\b")
_SUBSHELL = re.compile(r"\b(?:ba|z|da)?sh\s+-c\s+('([^']*)'|\"([^\"]*)\")|\beval\s+('([^']*)'|\"([^\"]*)\")")


def _tokens(segment: str) -> list[str]:
    try:
        return shlex.split(segment, posix=True)
    except ValueError:
        return segment.split()


def _strip_wrappers(tokens: list[str]) -> tuple[list[str], bool]:
    """Drop ``VAR=x`` prefixes and ``env`` / ``command`` / ``time`` / ``nohup`` / ``nice`` wrappers; say whether ``sudo`` was one."""
    sudo = False
    out = list(tokens)
    while out:
        head = out[0]
        if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*=.*", head):
            out.pop(0)
        elif head in ("env", "command", "time", "nohup", "nice", "exec", "stdbuf", "xargs", "builtin"):
            out.pop(0)
            while out and out[0].startswith("-"):
                out.pop(0)
        elif head in ("sudo", "doas"):
            sudo = True
            out.pop(0)
            while out and out[0].startswith("-"):
                out.pop(0)
        else:
            break
    return out, sudo


def _flags(tokens: list[str]) -> str:
    """Short flags run together, long flags kept: ``-rf --force`` → ``rf --force``."""
    parts = []
    for tok in tokens:
        if tok.startswith("--"):
            parts.append(tok)
        elif tok.startswith("-") and len(tok) > 1:
            parts.append(tok[1:])
    return " ".join(parts)


def _short_flags(tokens: list[str]) -> str:
    return "".join(tok[1:] for tok in tokens if tok.startswith("-") and not tok.startswith("--") and len(tok) > 1)


def _targets(tokens: list[str]) -> list[str]:
    return [t for t in tokens if not t.startswith("-")]


def _rm(tokens: list[str], cwd: str) -> tuple[float, str]:
    short = _short_flags(tokens[1:])
    recursive = bool(re.search(r"[rR]", short)) or "--recursive" in tokens
    forced = "f" in short or "--force" in tokens
    targets = _targets(tokens[1:])
    if not targets:
        # `xargs rm -rf`, `find … | xargs rm`: the targets arrive on stdin and cannot be read here.
        return (0.9, "recursively removes whatever is piped to it") if recursive else (0.6, "removes whatever is piped to it")
    worst, why = 0.0, ""
    for target in targets:
        resolved = _expand(target, cwd)
        if resolved is None or target.strip() in ("/", "~", "*", ".", "..", "/*", "~/*") or resolved in ("/", _HOME):
            score, reason = 0.99, f"removes {target!r}"
        elif not _inside(resolved, cwd):
            score, reason = 0.95, f"removes {resolved}, outside the project"
        else:
            rel = os.path.relpath(resolved, cwd)
            top = rel.split(os.sep)[0]
            if rel == ".":
                score, reason = 0.97, "removes the whole project directory"
            elif recursive and top in _REBUILDABLE:
                score, reason = 0.55, f"removes {rel}, a directory that is rebuilt"
            elif recursive:
                score, reason = 0.95, f"recursively removes {rel}"
            elif "*" in target or "?" in target or forced:
                score, reason = 0.6, f"removes {rel} (glob or forced)"
            else:
                score, reason = 0.5, f"removes {rel}"
        if score > worst:
            worst, why = score, reason
    return worst, why


def _dd(tokens: list[str], cwd: str) -> tuple[float, str]:
    """`dd` by where it writes. A device is the disk-destroying case (`_DISK` already scores it); a file is
    scored like any other write to that path, and no `of=` means it writes to stdout."""
    target = next((t.split("=", 1)[1] for t in tokens[1:] if t.startswith("of=")), "")
    if not target:
        return 0.3, "dd writes to its output stream"
    if target.startswith("/dev/"):
        if target.startswith(("/dev/null", "/dev/stdout", "/dev/stderr", "/dev/tty", "/dev/fd/")):
            return 0.1, "dd discards its output"
        return 0.99, "can destroy a disk or filesystem"
    score, reason = path_score(target, cwd)
    return max(score, 0.4), f"dd {reason.replace('edits', 'overwrites') if reason.startswith('edits') else reason}"


def _git(tokens: list[str]) -> tuple[float, str]:
    args = tokens[1:]
    # skip global options: -C path, -c k=v, --git-dir=...
    while args and (args[0].startswith("-") or args[0] in ("-C", "-c")):
        args = args[2:] if args[0] in ("-C", "-c") else args[1:]
    if not args:
        return 0.05, "git"
    sub, rest = args[0], args[1:]
    flags = _flags(rest)
    if sub in _GIT_READ_ONLY:
        return 0.05, f"git {sub} only reads"
    if sub == "clean":
        return 0.9, "git clean deletes untracked files"
    if sub == "reset":
        return (0.9, "git reset --hard discards work") if "--hard" in flags or "hard" in rest else (0.5, "git reset moves HEAD")
    if sub in ("checkout", "restore", "switch"):
        if sub == "restore" and "--staged" in flags and "--worktree" not in flags:
            return 0.4, "git restore --staged"
        if "--" in rest or "." in rest or "-f" in rest or "--force" in flags or sub == "restore":
            return 0.8, f"git {sub} can discard working-tree changes"
        return 0.45, f"git {sub}"
    if sub == "push":
        if re.search(r"--force|--delete|(^| )f( |$)", flags) or any(a.startswith(":") for a in rest) or "--mirror" in flags:
            return 0.9, "git push rewrites or deletes remote history"
        return 0.65, "git push publishes commits"
    if sub == "branch":
        return (0.8, "git branch -D force-deletes a branch") if re.search(r"(^| )D( |$)|--delete.*--force|-df", flags) else ((0.4, "git branch") if rest else (0.05, "git branch lists"))
    if sub == "stash":
        return (0.75, "git stash drop/clear loses stashed work") if rest[:1] in (["drop"], ["clear"]) else (0.4, "git stash")
    if sub in ("filter-branch", "filter-repo") or (sub == "update-ref" and "-d" in rest):
        return 0.9, f"git {sub} rewrites history"
    if sub in ("gc", "reflog", "prune"):
        return (0.9, "git prune/expire deletes unreachable objects") if re.search(r"prune|expire", " ".join(rest)) else (0.4, f"git {sub}")
    if sub == "rebase":
        return 0.6, "git rebase rewrites commits"
    if sub == "commit":
        return (0.5, "git commit --amend rewrites the last commit") if "--amend" in flags else (0.4, "git commit")
    if sub in ("remote", "config", "submodule"):
        return 0.5, f"git {sub} changes repository configuration"
    return 0.4, f"git {sub}"


def _shell_segment(segment: str, cwd: str, depth: int = 0) -> tuple[float, str]:
    raw = _tokens(segment)
    if raw and os.path.basename(raw[0]) in ("env", "printenv") and all(t.startswith("-") for t in raw[1:]):
        return 0.5, "prints the environment, which can include secrets"
    tokens, sudo = _strip_wrappers(raw)
    floor, floor_reason = (0.85, "runs with sudo") if sudo else (0.0, "")
    if not tokens:
        return floor or 0.3, floor_reason or "empty command"
    head = os.path.basename(tokens[0])
    low = segment.lower()

    best, why = floor, floor_reason

    def take(score: float, reason: str) -> None:
        nonlocal best, why
        if score > best:
            best, why = score, reason

    if _FORK_BOMB.search(segment):
        take(1.0, "fork bomb")
    # Reading a secrets file puts its contents in front of a model: a write is not the only way to leak one.
    if head not in _PATH_ONLY:
        for token in tokens[1:] + re.findall(r"<\s*(\S+)", segment):
            if _is_secret_path(token, cwd):
                take(0.85, f"reads a secrets file ({os.path.basename(token.strip('<>=')) or token})")
                break
    if _DISK.search(low):
        take(0.99, "can destroy a disk or filesystem")
    if _SQL_DESTRUCTIVE.search(segment):
        take(0.9, "destructive SQL")
    if _CLOUD_DESTRUCTIVE.search(low):
        take(0.9, "destroys cloud or cluster resources")
    if _SYSTEM.search(low):
        take(0.9, "changes system configuration")
    if _PUBLISH.search(low):
        take(0.75, "publishes or deploys outside the machine")
    if _REDIRECT_SENSITIVE.search(segment):
        take(0.9, "redirects output onto a system or home-directory file")
    elif _REDIRECT_OVERWRITE.search(segment):
        take(0.4, "overwrites a file with a redirect")
    if _NET_WRITE.search(segment):
        take(0.65, "sends data to a remote host")
    if _REMOTE.search(" " + low):
        take(0.7, "talks to a remote machine")
    if _INSTALL.search(low):
        take(0.5, "installs packages, which runs other people's code")
    if _UNINSTALL.search(low):
        take(0.55, "uninstalls packages")

    if head == "rm" or head == "rmdir":
        take(*(_rm(tokens, cwd) if head == "rm" else (0.5, "removes a directory")))
    elif head == "git":
        take(*_git(tokens))
    elif head in ("chmod", "chown", "chgrp"):
        recursive = "R" in _short_flags(tokens[1:]) or "--recursive" in tokens
        wide = any(t in ("/", "~") or t.startswith(("/etc", "/usr", "/System", _HOME + "/.")) for t in _targets(tokens[1:]))
        take(0.95 if recursive and wide else 0.8 if recursive else 0.45, f"{head} {'-R ' if recursive else ''}changes permissions")
    elif head == "find":
        take(*((0.85, "find -delete / -exec rm removes files") if re.search(r"-delete|-exec\s+(rm|shred)", low) else (0.05, "find only reads")))
    elif head == "dd":
        take(*_dd(tokens, cwd))
    elif head == "truncate":
        take(0.7, "truncate empties a file")
    elif head in ("kill", "killall", "pkill"):
        take(*((0.65, f"{head} stops processes") if head != "kill" or re.search(r"-9|-kill|\s-1\b", low) else (0.5, "kill stops a process")))
    elif head in ("mv", "cp", "ln", "install", "rsync"):
        outside = [t for t in _targets(tokens[1:]) if (r := _expand(t, cwd)) is None or not _inside(r, cwd)]
        take(*((0.75, f"{head} touches a path outside the project") if outside else (0.45, f"{head} moves or copies project files")))
    elif head in ("sed", "perl", "awk", "gawk") and re.search(r"(^|\s)-\w*i\b|-pi\b|-i\s|inplace", low):
        take(0.45, f"{head} edits files in place")
    elif head == "printenv":
        take(0.5, "prints the environment, which can include secrets")
    elif head in ("touch", "mkdir", "tee"):
        take(0.3, f"{head} creates files")
    elif head in _READ_ONLY_COMMANDS:
        take(0.05, f"{head} only reads")
    elif head in _RUNNERS or head in ("cargo", "go", "npm", "pnpm", "yarn", "npx", "bunx", "mvn", "dotnet"):
        sub = tokens[1] if len(tokens) > 1 else ""
        take(*((0.25, f"{head} {sub} builds or tests the project") if sub in ("test", "check", "build", "clippy", "fmt", "doc", "vet", "lint", "run", "ci", "tsc", "install", "") or head in _RUNNERS else (0.4, f"{head} {sub}")))
    elif head in _SCRIPT_RUNNERS or head.startswith("./") or head.endswith(".sh"):
        take(0.45, f"runs a script ({head}), which can do anything the script does")
    elif head in ("cd", "export", "unset", "alias", "source", ".", "set", "pushd", "popd"):
        take(0.1, f"{head} changes only the shell")
    else:
        take(0.4, f"{head} (not a command Laya Guard knows)")

    # A command passed to a shell, or code passed to an interpreter, is judged by what it contains.
    if depth < 2:
        for match in _SUBSHELL.finditer(segment):
            inner = next((g for g in match.groups()[1:] if g), "")
            if inner:
                inner_score, inner_reason = shell_score(inner, cwd, depth + 1)
                take(max(inner_score, 0.45), f"runs a nested command — {inner_reason}")
        if head in ("python", "python3", "node", "perl", "ruby") and re.search(r"\s-(c|e)\s", segment):
            take(max(python_score(segment)[0], 0.45), "runs inline code")
    if _PIPE_TO_SHELL.search(low):
        take(0.95, "pipes a download straight into a shell")
    return best, why


def shell_score(command: str, cwd: str, depth: int = 0) -> tuple[float, str]:
    """The worst thing a (possibly compound) shell command line does."""
    if not command.strip():
        return 0.3, "empty command"
    if _PIPE_TO_SHELL.search(command.lower()):
        return 0.95, "pipes a download straight into a shell"
    if _FORK_BOMB.search(command):
        return 1.0, "fork bomb"
    parts, _simple = settings_store.split_shell(command)
    best, why = 0.0, ""
    for part in parts or [command]:
        score, reason = _shell_segment(part, cwd, depth)
        if score > best:
            best, why = score, reason
    # Redirects are stripped by the splitter when they target /dev/null only; any other was scanned per part.
    return best, why


# ------------------------------------------------------------------------------ python / patches / tasks


_PY_DESTRUCTIVE = re.compile(r"\b(shutil\.rmtree|os\.(remove|unlink|rmdir|removedirs)|send2trash|\.unlink\(|\.rmdir\()")
_PY_EXEC = re.compile(r"\b(os\.system|os\.popen|subprocess\.\w+|pty\.spawn|commands\.getoutput)\b")
_PY_NET_WRITE = re.compile(r"\b(requests|httpx|session)\.(post|put|delete|patch)\(|urllib\.request\.urlopen\([^)]*data=")
_PY_WRITE = re.compile(r"\bopen\([^)]*['\"][wax]b?\+?['\"]|\.write_(text|bytes)\(")
_PY_DYNAMIC = re.compile(r"\b(eval|exec|compile|__import__)\(")
_QUOTED = re.compile(r"""(?:"([^"\n]{3,})"|'([^'\n]{3,})')""")


def python_score(code: str, cwd: str = ".") -> tuple[float, str]:
    best, why = 0.3, "runs Python"

    def take(score: float, reason: str) -> None:
        nonlocal best, why
        if score > best:
            best, why = score, reason

    if _PY_DESTRUCTIVE.search(code):
        take(0.85, "deletes files from Python")
    if _PY_EXEC.search(code):
        take(0.7, "starts other programs from Python")
        for match in _QUOTED.finditer(code):
            text = match.group(1) or match.group(2) or ""
            if " " in text and not text.startswith(("http", "/")) or text.startswith(("rm ", "git ", "sudo ")):
                score, reason = shell_score(text, cwd)
                if score >= 0.8:
                    take(score, f"runs a shell command from Python — {reason}")
    if _PY_NET_WRITE.search(code):
        take(0.65, "sends data to a remote host")
    if _PY_WRITE.search(code):
        take(0.5, "writes files from Python")
    if _PY_DYNAMIC.search(code):
        take(0.6, "evaluates dynamic code")
    return best, why


_PATCH_PATHS = re.compile(r"^(?:\+\+\+ (?:b/)?|\*\*\* (?:Update|Add|Delete) File: |diff --git a/\S+ b/)(\S[^\t\n]*)$", re.MULTILINE)
_PATCH_DELETE = re.compile(r"^(?:\+\+\+ /dev/null|\*\*\* Delete File:|deleted file mode)", re.MULTILINE)


def patch_paths(patch: str) -> list[str]:
    seen: list[str] = []
    for match in _PATCH_PATHS.finditer(patch):
        path = match.group(1).strip()
        if path and path != "/dev/null" and path not in seen:
            seen.append(path)
    return seen


def patch_score(patch: str, cwd: str) -> tuple[float, str]:
    paths = patch_paths(patch)
    best, why = (0.35, "edits project files") if paths else (0.5, "applies a patch Laya Guard could not read")
    for path in paths:
        score, reason = path_score(path, cwd)
        if score > best:
            best, why = score, reason
    if _PATCH_DELETE.search(patch) and best < 0.7:
        best, why = 0.7, "deletes a file"
    removed = sum(1 for line in patch.splitlines() if line.startswith("-") and not line.startswith("---"))
    if removed >= 1000 and best < 0.8:
        best, why = 0.8, f"removes {removed} lines"
    elif removed >= 300 and best < 0.65:
        best, why = 0.65, f"removes {removed} lines"
    return best, why


_TASK_DESTRUCTIVE = re.compile(r"\b(delete|remove|wipe|erase|purge|drop|destroy|nuke|force[- ]?push|reset --hard|rewrite (the )?history|rm -rf|truncate)\b", re.IGNORECASE)
_TASK_OUTWARD = re.compile(r"\b(deploy|publish|release|push to (prod|production|main|master)|migrate (the )?(prod|production))\b", re.IGNORECASE)


def task_score(task: str) -> tuple[float, str]:
    if _TASK_DESTRUCTIVE.search(task):
        return 0.85, "the task asks Claude Code to delete or rewrite things"
    if _TASK_OUTWARD.search(task):
        return 0.7, "the task deploys or publishes"
    return 0.5, "hands a many-file change to Claude Code"


# ------------------------------------------------------------------------------ rules entry point


def rule_score(tool_name: str, tool_input: Any, cwd: str) -> tuple[float, str] | None:
    """``(score, why)`` for one call, or None when the call only reads (nothing to guard)."""
    data = tool_input if isinstance(tool_input, dict) else {}
    try:
        if tool_name == "shell":
            commands = shell_commands(tool_input)
            if not commands:
                return 0.4, "a shell call with no command Laya Guard could read"
            best = (0.0, "")
            for command in commands:
                score = shell_score(command, cwd)
                if score[0] > best[0]:
                    best = score
            return best
        if tool_name == "python_repl":
            return python_score(str(data.get("code") or ""), cwd)
        if tool_name == "editor":
            if str(data.get("command") or "").lower() in _EDITOR_READ:
                return None
            path = str(data.get("path") or data.get("file_path") or "")
            score, reason = path_score(path, cwd) if path else (0.5, "edits a file Laya Guard could not identify")
            if str(data.get("command") or "").lower() == "create" and not str(data.get("file_text") or data.get("content") or "").strip() and Path(_expand(path, cwd) or "/nonexistent").exists():
                score, reason = max(score, 0.7), "overwrites an existing file with nothing"
            return score, reason
        if tool_name == "file_write":
            path = str(data.get("path") or data.get("file_path") or data.get("filename") or "")
            score, reason = path_score(path, cwd) if path else (0.5, "writes a file Laya Guard could not identify")
            if path and not str(data.get("content") or data.get("text") or "").strip() and Path(_expand(path, cwd) or "/nonexistent").exists():
                score, reason = max(score, 0.7), "overwrites an existing file with nothing"
            return score, reason
        if tool_name == "apply_patch":
            return patch_score(str(data.get("patch") or ""), cwd)
        if tool_name == "claude_code_engineer":
            return task_score(str(data.get("task") or ""))
        if tool_name == "ollama_resource_manager":
            return (0.6, "pulls or deletes model weights") if str(data.get("action") or "").lower() in ("pull", "rm") else None
    except Exception:  # an unreadable input is unknown, never safe
        return 0.5, "Laya Guard could not read this call"
    return None


# ------------------------------------------------------------------------------ Laya's own reading


def render_call(tool_name: str, tool_input: Any, cwd: str) -> str:
    """The call as text Laya can read: what it is and what it touches, redacted and short."""
    data = tool_input if isinstance(tool_input, dict) else {}
    if tool_name == "shell":
        body = "\n".join(shell_commands(tool_input))
    elif tool_name == "python_repl":
        body = str(data.get("code") or "")
    elif tool_name == "apply_patch":
        body = "patch touching " + ", ".join(patch_paths(str(data.get("patch") or ""))[:8])
    elif tool_name == "claude_code_engineer":
        body = str(data.get("task") or "")
    else:
        body = " ".join(f"{k}={str(v)[:120]}" for k, v in data.items() if k in ("command", "path", "file_path", "filename", "action", "model"))
    text = f"A coding agent in {os.path.basename(cwd) or cwd} is about to use the {tool_name} tool:\n{body}"
    # Registered secrets first, then anything shaped like a credential: nothing is sent to a model, even a local one, unmasked.
    return secret_patterns.scrub(secret_guard.redact(text))[0][:1500]


_CACHE: "OrderedDict[tuple[str, str], tuple[str, float] | None]" = OrderedDict()
_CACHE_LOCK = threading.Lock()
_CACHE_MAX = 256


def clear_cache() -> None:
    """Forget cached readings — after swapping Laya's model, and between tests that fake different ones."""
    with _CACHE_LOCK:
        _CACHE.clear()


def laya_reading(
    tool_name: str,
    tool_input: Any,
    cwd: str,
    *,
    predict: Callable[..., Any] | None = None,
    ready: Callable[[], bool] | None = None,
    calibrator: Any = None,
) -> tuple[str, float] | None:
    """Laya's ``(label, confidence)`` for this one call, or None: weights not loaded (this never
    triggers a load — a tool call must not wait on one), nothing readable, or any failure."""
    if predict is None or ready is None:
        from prompt_security import laya_predict, laya_ready

        predict, ready = predict or laya_predict, ready or laya_ready
    try:
        if not ready():
            return None
        text = render_call(tool_name, tool_input, cwd)
        key = (tool_name, text)
        with _CACHE_LOCK:
            if key in _CACHE:
                _CACHE.move_to_end(key)
                return _CACHE[key]
        result = predict(text, CALL_QUESTION)
        answer = ((result or {}).get("answers") or {}).get("risk") or {}
        level = float(answer.get("score", 0.0))
        label = RISK_LEVELS[min(len(RISK_LEVELS) - 1, max(0, int(round(level))))]
        confidence = float(answer.get("answer_confidence", 0.0))
        if calibrator is not None:
            try:
                confidence = calibrator.calibrated("risk", label, confidence)
            except Exception:
                pass
        reading = (label, min(1.0, max(0.0, confidence))) if answer else None
        with _CACHE_LOCK:
            _CACHE[key] = reading
            while len(_CACHE) > _CACHE_MAX:
                _CACHE.popitem(last=False)
        return reading
    except Exception:
        return None


# ------------------------------------------------------------------------------ the assessment


def assess_call(
    tool_name: str,
    tool_input: Any,
    *,
    cwd: str,
    preferences: Any = None,
    predict: Callable[..., Any] | None = None,
    ready: Callable[[], bool] | None = None,
    calibrator: Any = None,
) -> CallRisk | None:
    """Score one tool call, or None when there is nothing to guard (guard off, a tool that only reads)."""
    mode = guard_mode(preferences)
    if mode == "off" or tool_name not in GUARDED_TOOLS:
        return None
    ruled = rule_score(tool_name, tool_input, cwd)
    if ruled is None:
        return None
    score, reason = ruled
    safe, destroy = laya_risk.thresholds(preferences)
    source, confidence = "rules", 1.0
    # Laya is asked only when the rules did not already settle that this call asks.
    if mode == "full" and score < destroy:
        reading = laya_reading(tool_name, tool_input, cwd, predict=predict, ready=ready, calibrator=calibrator)
        if reading is not None:
            label, conf = reading
            if label == "destructive" and conf >= LAYA_MIN_CONFIDENCE:
                laya_score = round(laya_risk.NEUTRAL + (1.0 - laya_risk.NEUTRAL) * conf, 4)
                if laya_score > score:
                    score, reason, source, confidence = laya_score, f"Laya read this call as destructive (confidence {conf:.2f}); rules said: {reason}", "rules+laya", conf
    score = round(min(1.0, max(0.0, score)), 3)
    return CallRisk(score=score, band=laya_risk.band(score, safe=safe, destroy=destroy), reason=reason, source=source, confidence=confidence, tool=tool_name)


def call_gate(preferences: Any = None, pod_dial: float | None = None) -> float:
    """Where a call starts to ask: the router's destructive threshold, tightened by the pod's dial.
    A dial can only lower it — see ``lab.Lab.dial_armed``."""
    _safe, destroy = laya_risk.thresholds(preferences)
    if pod_dial is not None and 0.0 < pod_dial < destroy:
        return float(pod_dial)
    return destroy

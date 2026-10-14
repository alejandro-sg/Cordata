"""A commit message for the changes in a project (the app's Review tab, like Copilot's sparkle and Codex's commit
helper): the staged diff — or, when nothing is staged, every uncommitted change — summarised by a model.

The diff is read here, in the project the session runs in, never taken from the client; it is cut to a bound and
secret-scrubbed before any model sees it. The model is the fast tier when one is configured (a commit message is
rewriting, not reasoning), else the session's own. No tools: one call, text in, text out.
"""

from __future__ import annotations

import subprocess
from collections.abc import Callable
from pathlib import Path

import secret_patterns

#: The most diff text a model is shown.
MAX_DIFF_CHARS = 60_000
GIT_TIMEOUT = 15

SYSTEM = (
    "You write git commit messages. Reply with the message only: a subject line in the imperative mood, at most 72 "
    "characters, with a conventional prefix (feat, fix, refactor, docs, test, chore, perf, build, ci) when one fits; "
    "then a blank line and a short body of plain sentences or '-' bullets saying what changed and why, wrapped at 72. "
    "No code fences, no quotes around the message, nothing invented that the diff does not show."
)


def _git(cwd: str | Path, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["git", "-c", "core.quotepath=off", *args], cwd=str(cwd), capture_output=True, text=True, timeout=GIT_TIMEOUT, stdin=subprocess.DEVNULL)


def gather(cwd: str | Path) -> dict:
    """``{ok, staged, diff, stat, cut}`` — the staged changes, else all uncommitted ones (untracked files by name)."""
    inside = _git(cwd, "rev-parse", "--is-inside-work-tree")
    if inside.returncode != 0 or inside.stdout.strip() != "true":
        return {"ok": False, "reason": "this project is not a git repository"}
    staged = _git(cwd, "diff", "--cached", "--no-color", "--no-ext-diff")
    diff, is_staged = staged.stdout, True
    if not diff.strip():
        head = _git(cwd, "rev-parse", "--verify", "-q", "HEAD")
        base = ["HEAD"] if head.returncode == 0 else []
        diff = _git(cwd, "diff", "--no-color", "--no-ext-diff", *base).stdout
        untracked = [line for line in _git(cwd, "ls-files", "--others", "--exclude-standard").stdout.splitlines() if line.strip()]
        if untracked:
            diff += "\n# new files (not shown):\n" + "\n".join(f"#   {name}" for name in untracked[:200])
        is_staged = False
    if not diff.strip():
        return {"ok": False, "reason": "there are no changes to describe"}
    cut = len(diff) > MAX_DIFF_CHARS
    if cut:
        diff = diff[:MAX_DIFF_CHARS] + f"\n… (diff cut at {MAX_DIFF_CHARS} characters)"
    diff = secret_patterns.scrub(diff)[0]
    stat = _git(cwd, "diff", "--cached" if is_staged else "HEAD", "--stat", "--no-color").stdout.strip().splitlines()[-1:] or [""]
    return {"ok": True, "staged": is_staged, "diff": diff, "stat": stat[0], "cut": cut}


def clean(text: str) -> str:
    """The model's reply as a commit message: fences and quotes off, the subject at most 72 characters."""
    lines = [line.rstrip() for line in (text or "").strip().strip("`").strip().splitlines()]
    while lines and lines[0].strip().lower() in ("", "text", "git", "commit"):
        lines.pop(0)
    if not lines:
        return ""
    subject = lines[0].strip().strip('"').strip("'")
    if len(subject) > 72:
        subject = subject[:71].rstrip() + "…"
    body = "\n".join(lines[1:]).strip()
    return subject + ("\n\n" + body if body else "")


def commit_message(cwd: str | Path, generate: Callable[[str], str]) -> dict:
    """``{ok, message, staged, stat}`` or ``{ok: False, reason}``. ``generate`` is the model call (system prompt
    applied by the caller)."""
    gathered = gather(cwd)
    if not gathered.get("ok"):
        return gathered
    prompt = f"Write the commit message for this diff ({'staged changes' if gathered['staged'] else 'all uncommitted changes'}):\n\n{gathered['diff']}"
    try:
        message = clean(generate(prompt))
    except Exception as error:  # a model or provider failure is reported, not raised into the ACP loop
        return {"ok": False, "reason": f"the model did not answer: {str(error)[:200]}"}
    if not message:
        return {"ok": False, "reason": "the model returned nothing"}
    return {"ok": True, "message": message, "staged": gathered["staged"], "stat": gathered["stat"]}

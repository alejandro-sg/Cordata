"""Commit messages from a project's diff (git_message.py): staged first, else everything uncommitted; secrets
scrubbed before a model sees them; the reply cleaned to a proper message; failures reported, not raised.

    backend/.venv/bin/python _git_message_smoke_test.py
"""

from __future__ import annotations

import subprocess
import tempfile
from pathlib import Path

import git_message
from harness_testing import check


def git(cwd: Path, *args: str) -> None:
    subprocess.run(["git", "-c", "user.email=t@t", "-c", "user.name=t", *args], cwd=cwd, check=True, capture_output=True)


def main() -> None:
    root = Path(tempfile.mkdtemp())
    check(not git_message.gather(root)["ok"], "outside git: says so")
    git(root, "init", "-q", "-b", "main")
    (root / "a.py").write_text("print('a')\n")
    git(root, "add", "-A")
    git(root, "commit", "-qm", "init")
    check(git_message.gather(root)["reason"] == "there are no changes to describe", "a clean tree: nothing to describe")

    (root / "a.py").write_text("print('b')\nTOKEN = 'sk-ant-api03-" + "x" * 40 + "'\n")
    (root / "new.txt").write_text("hello\n")
    seen: list[str] = []

    def model(prompt: str) -> str:
        seen.append(prompt)
        return "```\nfix: print b instead of a and keep a much much much much much much much longer subject line\n\n- changes the greeting\n```"

    result = git_message.commit_message(root, model)
    check(result["ok"] and not result["staged"], f"nothing staged: all uncommitted changes ({result})")
    check("sk-ant-api03" not in seen[0] and "new.txt" in seen[0], "the diff the model sees is scrubbed, and lists new files")
    subject, _, body = result["message"].partition("\n\n")
    check(len(subject) <= 72 and subject.startswith("fix: print b") and "`" not in result["message"], f"fences off, subject at most 72 ({subject!r})")
    check(body == "- changes the greeting", "the body is kept")

    git(root, "add", "a.py")
    staged = git_message.commit_message(root, lambda p: "chore: x")
    check(staged["ok"] and staged["staged"], "staged changes win over the rest")

    def broken(_: str) -> str:
        raise RuntimeError("provider down")

    failed = git_message.commit_message(root, broken)
    check(not failed["ok"] and "provider down" in failed["reason"], "a model failure is reported, not raised")
    print("\nALL OK")


if __name__ == "__main__":
    main()

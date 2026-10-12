"""The new terminal conveniences, end to end: the real ``acp-tui`` binary in a pseudo-terminal, over stdio, to a real
``acp_server.py`` child, answered by ``fake_ollama.FakeOllama`` (no network, no key).

* ``!command``       — runs in the project, is shown, and the agent is told what it printed with the next prompt;
* ``@file``          — the menu offers the project's files, and the model receives the file's contents;
* ``/goal … --check``— the harness keeps the agent going until the command passes, announcing each step;
* ``/context``, ``/export``, ``/diff`` — answered without a model call.

    backend/.venv/bin/python _e2e_ux_tui_smoke_test.py
"""

import os
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

from _e2e_lab_tui_smoke_test import BINARY, ENTER, GLM, CTRL_Q, Terminal, build_binary
from fake_ollama import FakeOllama
from harness_testing import check, text, tool

TAB = b"\t"


def main() -> None:
    build_binary()
    work = Path(tempfile.mkdtemp(prefix="ux-tui-"))
    project = work / "project"
    project.mkdir()
    (project / "README.md").write_text("demo project: the secret word is PINEAPPLE\n")
    (project / "notes.txt").write_text("old\n")
    subprocess.run(["git", "init", "-q"], cwd=project, check=True)
    subprocess.run(["git", "-c", "user.email=t@t", "-c", "user.name=t", "add", "-A"], cwd=project, check=True)
    subprocess.run(["git", "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"], cwd=project, check=True)
    (project / "notes.txt").write_text("new\n")

    scripts = {
        GLM: [
            text("It printed hello-from-shell."),                                # after !echo
            text("The README says the secret word is PINEAPPLE."),               # after @README.md
            text("Nothing to do."),                                              # goal turn 1: claims done, wrongly
            tool("shell", {"command": "touch made.txt"}), text("created made.txt"),  # goal turn 2
        ],
    }
    env = {
        **os.environ,
        "TERM": "xterm-256color",
        "MODEL_PROVIDER": "ollama_cloud",
        "LAYA_TRIAGE_MODE": "off",
        "LAYA_GUARD_ENABLED": "false",
        "CORDATA_HOME": str(work / "home"),
        "CORDATA_NOTIFY": "off",
    }
    with FakeOllama(scripts) as fake:
        env["OLLAMA_BASE_URL"] = fake.base_url
        argv = [str(BINARY), "--cwd", str(project), "--transport", "stdio"]
        term = Terminal(argv, env)
        try:
            run(term, fake, project)
        finally:
            term.close()
            shutil.rmtree(work, ignore_errors=True)
    print("\nALL OK")


def run(term: Terminal, fake: FakeOllama, project: Path) -> None:
    term.wait_for("CORDATA", "BUILD", timeout=90)
    check(True, "the TUI connected to a real backend child")

    # ------------------------------------------------------------ !command
    term.mark()
    term.type("!echo hello-from-shell")
    shown = term.step(ENTER, "$ echo hello-from-shell", "[exit 0]", timeout=20)
    check("hello-from-shell" in shown, "!command ran in the project and its output is shown")
    check(len(fake.chats(GLM)) == 0, "…without a model call")
    term.mark()
    term.type("what did I just run?")
    term.send(ENTER)
    term.wait_for("It printed hello-from-shell.", timeout=60)
    sent = str(fake.chats(GLM)[-1]["messages"])
    check("The user ran `echo hello-from-shell`" in sent and "hello-from-shell" in sent, "the next prompt carried what the command printed")

    # ------------------------------------------------------------ @file
    term.mark()
    term.type("summarise @READ")
    shown = term.wait_for("README.md", "tab attach", timeout=20)
    check("▸ README.md" in shown, "typing @READ opens the file menu with README.md highlighted")
    term.send(TAB)
    term.pump(0.3)
    term.type("in one line")
    term.send(ENTER)
    term.wait_for("attached README.md", "PINEAPPLE", timeout=60)
    sent = str(fake.chats(GLM)[-1]["messages"])
    check("secret word is PINEAPPLE" in sent and "summarise @README.md" in sent, "the model received the file's contents, and the person's words first")

    # ------------------------------------------------------------ /goal
    term.mark()
    term.type("/mode auto")
    term.step(ENTER, "AUTO", timeout=20)
    term.mark()
    term.type("/goal made.txt exists --check test -f made.txt --turns 3")
    term.send(ENTER)
    shown = term.wait_for("goal met after 2 turns", timeout=90)
    check("◎ goal: made.txt exists" in shown and "`test -f made.txt`" in shown, "the goal and what decides it are announced")
    check("◎ goal 1/3 — not yet" in shown, "the first turn's claim of success was not taken at its word")
    check((project / "made.txt").exists(), "the follow-up turn did the work")
    check(len(fake.chats(GLM)) == 5, f"exactly the scripted model calls were made ({len(fake.chats(GLM))})")

    # ------------------------------------------------------------ no-model commands
    calls = len(fake.chats(GLM))
    term.mark()
    term.type("/context")
    shown = term.step(ENTER, "context", "tokens", timeout=20)
    check("▰" in shown or "▱" in shown or "window is unknown" in shown, "/context draws the gauge")
    term.mark()
    term.type("/diff")
    shown = term.step(ENTER, "-old", "+new", timeout=20)
    check("notes.txt" in shown, "/diff shows the working tree's changes, coloured by line")
    term.mark()
    term.type("/export session.md")
    term.step(ENTER, "saved the conversation", timeout=20)
    exported = (project / "session.md").read_text()
    check("what did I just run?" in exported and "PINEAPPLE" in exported, "/export wrote the conversation as markdown")
    check(len(fake.chats(GLM)) == calls, "none of those cost a model call")

    term.send(CTRL_Q)
    deadline = time.time() + 30
    while term.process.poll() is None and time.time() < deadline:
        term.pump(0.2)
    check(term.process.poll() is not None, "Ctrl+Q exits")


if __name__ == "__main__":
    main()

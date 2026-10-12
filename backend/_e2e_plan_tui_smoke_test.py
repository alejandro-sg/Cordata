"""Adaptive mode and the plan card, end to end: the real ``acp-tui`` binary in a pseudo-terminal, over stdio, to a real
``acp_server.py`` child, answered by ``fake_ollama.FakeOllama`` (no network, no key).

* a request that asks for a plan starts the turn in **plan** — the footer says so and a line says why;
* the agent's ``present_plan`` arrives as a plan card; **b** approves it into build, the session leaves plan mode and the
  model is told;
* ``ask_user`` arrives as a numbered question; **2** answers it and the model reads the choice back;
* ``w`` types a reason: the agent gets the person's own words.

    backend/.venv/bin/python _e2e_plan_tui_smoke_test.py
"""

import os
import shutil
import tempfile
from pathlib import Path

from _e2e_lab_tui_smoke_test import BINARY, ENTER, GLM, CTRL_Q, Terminal, build_binary
from fake_ollama import FakeOllama
from harness_testing import check, text, tool

PLAN = "# Goal: add a cache layer\n1. add cache.py\n2. wire it into the loader\nVerify: pytest"


def main() -> None:
    build_binary()
    work = Path(tempfile.mkdtemp(prefix="plan-tui-"))
    project = work / "project"
    project.mkdir()
    (project / "README.md").write_text("demo\n")
    scripts = {
        GLM: [
            tool("present_plan", {"plan": PLAN}),                       # turn 1: the plan card
            text("Plan approved, starting with cache.py."),
            tool("ask_user", {"question": "Which cache?", "options": ["lru_cache", "diskcache", "redis"]}),  # turn 2
            text("Going with diskcache."),
            tool("ask_user", {"question": "Where should it live?", "options": ["src/cache.py", "cache/"]}),  # turn 3
            text("Noted."),
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
        "OLLAMA_ROLE_ORCHESTRATOR": "glm-5.3-flash:low",  # the fake model script is keyed by GLM; the product default is Kimi
        "HF_HUB_OFFLINE": "1",
    }
    with FakeOllama(scripts) as fake:
        env["OLLAMA_BASE_URL"] = fake.base_url
        argv = [str(BINARY), "--cwd", str(project), "--transport", "stdio"]
        term = Terminal(argv, env)
        try:
            run(term, fake)
        finally:
            term.close()
            shutil.rmtree(work, ignore_errors=True)
    print("\nALL OK")


def run(term: Terminal, fake: FakeOllama) -> None:
    term.wait_for("CORDATA", "BUILD", timeout=90)
    check(True, "the TUI connected to a real backend child")

    # ------------------------------------------------------------ adaptive: plan first, then the plan card
    term.mark()
    term.type("make a plan to add a cache layer to the loader")
    term.send(ENTER)
    shown = term.wait_for("PLAN READY", "add cache.py", timeout=60)
    check("build → plan" in shown and "you asked for a plan" in shown, "a line says the turn started in plan, and why")
    check("PLAN" in shown, "the footer pill reads PLAN")
    check(" b  build" in shown and " e  edit" in shown and " a  auto" in shown, "the card offers build, edit and auto")
    term.mark()
    term.send(b"b")
    shown = term.wait_for("Plan approved, starting with cache.py.", timeout=60)
    sent = str(fake.chats(GLM)[-1]["messages"])
    check("Plan approved. The session is now in build mode" in sent, "the model was told the plan was approved, and the mode")
    term.pump(0.5)
    check("BUILD" in term.seen()[-1500:], "the footer pill is BUILD again")

    # ------------------------------------------------------------ ask_user
    term.mark()
    term.type("ask me which cache to use")
    term.send(ENTER)
    shown = term.wait_for("QUESTION", "Which cache?", "diskcache", timeout=60)
    check(" 1 " in shown and " 2 " in shown and " 3 " in shown and "decide for me" in shown, "the question is drawn with numbered options and an escape")
    term.mark()
    term.send(b"2")
    term.wait_for("Going with diskcache.", timeout=60)
    sent = str(fake.chats(GLM)[-1]["messages"])
    check("The person chose: diskcache" in sent, "the model read the choice back")

    # ------------------------------------------------------------ w: their own words
    term.mark()
    term.type("ask me where it should live")
    term.send(ENTER)
    term.wait_for("QUESTION", "Where should it live?", timeout=60)
    term.send(b"w")
    term.type("next to the loader")
    term.mark()
    term.send(ENTER)
    term.wait_for("Noted.", timeout=60)
    sent = str(fake.chats(GLM)[-1]["messages"])
    check("in their own words: next to the loader" in sent, "typed words reach the agent as the answer")

    term.send(CTRL_Q)


if __name__ == "__main__":
    main()

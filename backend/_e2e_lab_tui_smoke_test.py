"""The real ``acp-tui`` binary in a pseudo-terminal, over stdio, to a real
``acp_server.py`` child on the ``ollama_cloud`` provider, answered by
``fake_ollama.FakeOllama``. The proof that the Lab's keys and its telemetry are
wired to the daemon's agent loop end to end — not just each half in isolation:

    prompt → Orchestrator (glm @ low) → Debugger (kimi @ max) → shell
          → parked at the HITL station → F2 → [N] / [Y] / [K] → the loop obeys
    Laya dial, guidance and E-Stop/release from the Lab → the next request shows it

No emulator library: each step waits for text to appear in the ANSI-stripped output
it caused. Needs a built binary (``cargo build -p acp-tui``, done here if missing)
and the backend's venv; ~40 s, no network.

    backend/.venv/bin/python _e2e_lab_tui_smoke_test.py
"""

import fcntl
import os
import pty
import re
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import time
from pathlib import Path

from fake_ollama import FakeOllama
from harness_testing import check, text, tool

HERE = Path(__file__).parent.resolve()
REPO = HERE.parent
TUI = REPO / "tui"
BINARY = TUI / "target" / "debug" / "acp-tui"

GLM = "glm-5.3-flash:cloud"
KIMI = "kimi-k2.7-code:cloud"

ANSI = re.compile(r"\x1b\[[0-9;?<>=]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[()][A-Z0-9]|\x1b[=>78]")

F2 = b"\x1bOQ"
ENTER = b"\r"
ESC = b"\x1b"
LEFT, RIGHT = b"\x1b[D", b"\x1b[C"
CTRL_Q = b"\x11"


class Terminal:
    """A child process on a pty, and everything it has drawn since the last mark."""

    def __init__(self, argv: list[str], env: dict[str, str], rows: int = 46, cols: int = 120) -> None:
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.process = subprocess.Popen(
            argv,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=env,
            start_new_session=True,
            preexec_fn=lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0),
            close_fds=True,
        )
        os.close(slave)
        self.raw = b""
        self.mark_at = 0

    def pump(self, seconds: float = 0.1) -> None:
        deadline = time.time() + seconds
        while time.time() < deadline:
            ready, _, _ = select.select([self.master], [], [], max(0.0, deadline - time.time()))
            if not ready:
                return
            try:
                chunk = os.read(self.master, 65536)
            except OSError:
                return
            if not chunk:
                return
            self.raw += chunk

    def mark(self) -> None:
        self.pump(0.05)
        self.mark_at = len(self.raw)

    def seen(self) -> str:
        return ANSI.sub("", self.raw[self.mark_at :].decode("utf-8", errors="replace")).replace("\r", "")

    def last_frame(self) -> str:
        """What is on screen *now*: the text after the newest Lab header (the
        inline renderer redraws the whole live region each time, so everything
        before it is history)."""
        shown = self.seen()
        return shown[shown.rfind("EXPERIMENTAL LAB") :] if "EXPERIMENTAL LAB" in shown else shown

    def send(self, data: bytes) -> None:
        os.write(self.master, data)

    def type(self, line: str) -> None:
        for char in line:
            self.send(char.encode())
            self.pump(0.005)

    def wait_for(self, *needles: str, timeout: float = 30.0) -> str:
        deadline = time.time() + timeout
        while time.time() < deadline:
            self.pump(0.1)
            shown = self.seen()
            if all(needle in shown for needle in needles):
                return shown
            if self.process.poll() is not None:
                break
        raise AssertionError(f"FAIL (timeout {timeout}s) waiting for {needles}\n--- screen since the last step ---\n{self.seen()[-3500:]}")

    def step(self, data: bytes, *needles: str, timeout: float = 30.0) -> str:
        self.mark()
        self.send(data)
        return self.wait_for(*needles, timeout=timeout)

    def close(self) -> None:
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
        os.close(self.master)


def build_binary() -> None:
    result = subprocess.run(["cargo", "build", "-q", "-p", "acp-tui"], cwd=TUI, capture_output=True, text=True)
    if result.returncode != 0 or not BINARY.exists():
        raise SystemExit(f"could not build acp-tui:\n{result.stderr[-2000:]}")


def main() -> None:
    build_binary()
    work = Path(tempfile.mkdtemp(prefix="lab-tui-"))
    project = work / "project"
    project.mkdir()
    (project / "README.md").write_text("demo\n")
    scripts = {
        GLM: [
            tool("debugger", {"input": "fix the build"}), text("T1 done: the debugger's command was rejected."),
            tool("debugger", {"input": "fix the build again"}), text("T2 done: the debugger ran its command."),
            tool("debugger", {"input": "clean up"}), text("T3 done: the debugger was E-stopped."),
            text("T4 done: noted your guidance."),
        ],
        KIMI: [
            tool("shell", {"command": "touch rejected.txt"}), text("could not run it"),
            tool("shell", {"command": "touch approved.txt"}), text("ran it"),
            tool("shell", {"command": "touch estop.txt"}),
        ],
    }
    env = {
        **os.environ,
        "TERM": "xterm-256color",
        "MODEL_PROVIDER": "ollama_cloud",
        "LAYA_TRIAGE_MODE": "off",
        "LAYA_GUARD_ENABLED": "false",
        "SIERRALAB_HOME": str(work / "home"),
    }
    with FakeOllama(scripts) as fake:
        env["OLLAMA_BASE_URL"] = fake.base_url
        debug = os.environ.get("LAB_TUI_DEBUG_LOG")
        # stdio, explicitly: the default (`auto`) joins a running daemon's hub, and on a machine where one is up the test
        # would drive *that* daemon — its provider, its MCP servers, its operator's Discord — instead of the fake.
        argv = [str(BINARY), "--cwd", str(project), "--transport", "stdio", *(["--debug-log", debug] if debug else [])]
        term = Terminal(argv, env)
        try:
            run(term, fake, project)
        finally:
            term.close()
            shutil.rmtree(work, ignore_errors=True)
    print("\nALL OK")


def run(term: Terminal, fake: FakeOllama, project: Path) -> None:
    # ------------------------------------------------------------ connect
    term.wait_for("sierralab-agents", "BUILD", timeout=90)
    check(True, "the TUI connected to a real backend child (provider ollama_cloud)")

    # ------------------------------------------------ 1. park at the station, look, reject
    term.mark()
    term.type("fix the failing build")
    term.send(ENTER)
    term.wait_for("Permission required", timeout=60)
    check(True, "Orchestrator → Debugger → shell parked at an approval prompt")
    shown = term.step(F2, "EXPERIMENTAL LAB", "HITL INTERVENTION STATION", "[CONTAINED ⚠️]", "Approve & Execute", "Reject & Discard")
    check("kimi-k2.7-code" in shown and "glm-5.3-flash" in shown, "F2: the Lab shows both models")
    check("max" in shown and "low" in shown, "…with their reasoning efforts (kimi max, glm low)")
    check("orchestrator" in shown and "debugger" in shown and "delegation" in shown, "…and the delegation vector")
    check("pod   debugger" in shown or "debugger" in shown, "…and names the pod behind the request")
    check(not (project / "rejected.txt").exists(), "nothing ran while it waited")
    term.step(b"n", "0 busy", "T1 done", timeout=30)
    check(not (project / "rejected.txt").exists(), "[N] Reject & Discard: the command never ran")
    term.step(ESC, "BUILD")
    check(len(fake.chats(KIMI)) == 2, "the Debugger got its rejection and answered (a second kimi call)")

    # ---------------------------------------------------------------- 2. approve
    term.mark()
    term.type("fix it properly")
    term.send(ENTER)
    term.wait_for("Permission required", timeout=60)
    term.step(F2, "HITL INTERVENTION STATION")
    term.step(b"y", "0 busy", "T2 done", timeout=30)
    for _ in range(100):
        if (project / "approved.txt").exists():
            break
        time.sleep(0.1)
    check((project / "approved.txt").exists(), "[Y] Approve & Execute: the command ran in the project")
    term.step(ESC, "BUILD")

    # ----------------------------------------- 3. E-Stop at the station isolates the asking pod
    term.mark()
    term.type("clean up everything")
    term.send(ENTER)
    term.wait_for("Permission required", timeout=60)
    term.step(F2, "HITL INTERVENTION STATION", "E-Stop")
    kimi_before, glm_before = len(fake.chats(KIMI)), len(fake.chats(GLM))
    term.step(b"K", "debugger ISOLATED", "[ISOLATED 🛑]", timeout=30)
    check(not (project / "estop.txt").exists(), "[K] at the station: the call was rejected and never ran")
    term.wait_for("T3 done", timeout=30)
    check(True, "the Orchestrator carried on — its delegate was refused, not the whole session")
    term.wait_for("0 busy", timeout=30)
    check(len(fake.chats(KIMI)) == kimi_before, "the isolated Debugger made no further model call")
    check(len(fake.chats(GLM)) == glm_before + 1, "only the Orchestrator's own wrap-up call followed")
    # release the debugger: orchestrator, architect, debugger — two to the right
    term.send(RIGHT)
    term.step(RIGHT, "MONITOR ▸ debugger")
    term.step(b"R", "debugger released", timeout=20)
    check("[ISOLATED 🛑]" not in term.last_frame(), "R releases the selected pod — no pod reads ISOLATED any more")

    # -------------------------------------------- 4. E-Stop the whole team from the Lab, release
    term.send(LEFT)
    term.step(LEFT, "MONITOR ▸ orchestrator")
    shown = term.step(b"K", "orchestrator ISOLATED, with architect, debugger", timeout=20)
    check(term.last_frame().count("[ISOLATED 🛑]") == 3, "[K] on the orchestrator isolates it and both specialists — three ISOLATED pods on screen")
    term.step(b"R", "orchestrator released", timeout=20)
    check(term.last_frame().count("[ISOLATED 🛑]") == 2, "R frees the orchestrator alone")

    # ------------------------------------------------------ 5. Laya dial (tightens only)
    term.step(b"[", "dial 0.75", "Laya dial → 0.75", timeout=20)
    check(True, "[ ] lowers the selected pod's Laya dial 0.80 → 0.75, live")
    term.step(b"]", "dial 0.80", "Laya dial → 0.80", timeout=20)
    term.step(b"]", "only tighten", timeout=20)
    check(True, "…and cannot loosen past the router's own gate")

    # --------------------------------------------------------- 6. guidance reaches the model
    term.step(b"i", "Inject guidance → orchestrator")
    term.type("use pytest -x and nothing else")
    term.step(ENTER, "guidance queued for orchestrator", timeout=20)
    term.send(ESC)
    term.mark()
    term.type("status?")
    term.send(ENTER)
    term.wait_for("T4 done", timeout=60)
    last_glm = fake.chats(GLM)[-1]
    seen = str(last_glm["messages"])
    check("use pytest -x and nothing else" in seen and "operator directive" in seen, "the directive typed in the Lab reached the model on its next call")
    check(all(c["reasoning_effort"] == "low" for c in fake.chats(GLM)) and all(c["reasoning_effort"] == "max" for c in fake.chats(KIMI)), "every glm request carried low and every kimi request max, across the whole session")
    check({p for p in fake.paths() if p.startswith("POST")} == {"POST /v1/chat/completions"}, "the only write the daemon saw was chat completions")

    # ------------------------------------------------------------------ quit
    term.send(CTRL_Q)
    deadline = time.time() + 30  # the backend child takes a few seconds to wind down
    while term.process.poll() is None and time.time() < deadline:
        term.pump(0.2)
    check(term.process.poll() is not None, "Ctrl+Q exits the TUI (and its backend child)")


if __name__ == "__main__":
    main()

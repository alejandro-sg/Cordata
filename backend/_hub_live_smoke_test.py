"""The terminal's hub path end to end: the real Rust client (`acp-core`) against a real daemon.

Starts a throwaway daemon (its own home, free ports, the scripted agent from the Discord tests — no
model), runs `tui/crates/acp-core/tests/hub_live.rs` against it, then reads what the run left in the
daemon's conversation store: the conversation the terminal opened, with its turn counted.

    backend/.venv/bin/python _hub_live_smoke_test.py

Needs the Rust toolchain; no network, no model, no Discord.
"""

import json
import os
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
TUI = HERE.parent / "tui"

from harness_testing import check  # noqa: E402


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def main() -> int:
    home = Path(tempfile.mkdtemp(prefix="cordata-hub-live-"))
    project = home / "project"
    project.mkdir()
    config = home / "daemon.json"
    env = {
        **os.environ,
        "CORDATA_HOME": str(home),
        "CORDATA_DAEMON_CONFIG": str(config),
        "CORDATA_DAEMON_PID": str(home / "daemon.pid"),
        "CORDATA_STATE_DB": str(home / "state.db"),
        "CORDATA_DISCORD": "off",
        "FAKE_ACP_STATE": str(home / "fake-acp-state.json"),
        "CORDATA_ACP_ENTRYPOINT": f"{sys.executable} {HERE / 'fixtures' / 'fake_acp_chat_child.py'}",
    }
    port = free_port()
    daemon = subprocess.Popen([sys.executable, "-m", "daemon.main", "--port", str(port), "--no-grpc", "--log-level", "warning"], cwd=HERE, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    try:
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and not (config.exists() and json.loads(config.read_text() or "{}").get("port") == port):
            if daemon.poll() is not None:
                break
            time.sleep(0.1)
        check(daemon.poll() is None and config.exists(), f"a throwaway daemon is up on port {port}")
        run = subprocess.run(
            ["cargo", "test", "-q", "-p", "acp-core", "--test", "hub_live", "--", "--ignored", "--nocapture", "--test-threads=1"],
            cwd=TUI,
            env={**env, "HUB_LIVE_CWD": str(project)},
            capture_output=True,
            text=True,
            timeout=900,
        )
        print(run.stdout[-2000:])
        if run.returncode != 0:
            print(run.stderr[-3000:], file=sys.stderr)
        check(run.returncode == 0, "the Rust client opened, used and shared a hub conversation (hub_live.rs)")
        for needle in ("hub: opened c", "hub: turn answered", "hub: second terminal refused", "hub: latest is c"):
            check(needle in run.stdout, f"the live run reported: {needle}")
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=15)
        except subprocess.TimeoutExpired:
            daemon.kill()

    store = home / "discord_chats.json"
    chats = json.loads(store.read_text())["chats"] if store.exists() else {}
    rows = [row for row in chats.values() if row.get("cwd") == str(project.resolve())]
    check(len(rows) == 1 and rows[0]["origin"] == "terminal" and rows[0]["turns"] >= 1, f"the store holds the terminal's conversation, turn counted ({rows})")
    check(oct(store.stat().st_mode & 0o777) == "0o600", "…in a file only you can read")
    print("\nhub live smoke: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

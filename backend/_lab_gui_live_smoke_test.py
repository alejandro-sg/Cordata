"""The Rust GUI client against the real Python bridge, end to end — and the ledger it leaves.

Starts the demo daemon (``lab_demo.py``: the real Supervisor + gRPC bridge, a scripted session, a
fake oMLX) in a throwaway home, runs ``lab/crates/harness-client/tests/live.rs`` against it, then
reads the daemon's ``state.db`` to prove the human's choices were audited: the rejection and the
isolation, the approval, the knobs, the unload, and the ingested symbol.

    backend/.venv/bin/python _lab_gui_live_smoke_test.py        (or: make lab-live)

Needs the Rust toolchain (it builds the lab workspace's test) and no network or model.
"""

import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
LAB = HERE.parent / "lab"

from harness_testing import check  # noqa: E402


def main() -> int:
    home = Path(tempfile.mkdtemp(prefix="sierralab-live-"))
    workspace = home / "workspace"
    workspace.mkdir()
    demo = subprocess.Popen(
        [sys.executable, "lab_demo.py", "--home", str(home), "--workspace", str(workspace)],
        cwd=HERE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    try:
        deadline = time.monotonic() + 30
        ready = False
        while time.monotonic() < deadline:
            line = demo.stdout.readline()
            if "demo daemon ready" in line:
                ready = True
                break
            if not line and demo.poll() is not None:
                break
        check(ready, "the demo daemon came up")

        env = {**os.environ, "SIERRALAB_HOME": str(home)}
        env.pop("SIERRALAB_DAEMON_CONFIG", None)
        run = subprocess.run(
            ["cargo", "test", "-p", "harness-client", "--test", "live", "--", "--ignored", "--nocapture", "--test-threads=1"],
            cwd=LAB,
            env=env,
            capture_output=True,
            text=True,
            timeout=600,
        )
        print(run.stdout[-3000:])
        if run.returncode != 0:
            print(run.stderr[-3000:], file=sys.stderr)
        check(run.returncode == 0, "the Rust client and the Python bridge agree (lab/crates/harness-client/tests/live.rs)")
        for needle in ("autonomy:", "agents:", "steer:", "modal:", "rejected + isolated", "approved:", "diff demo-", "stress:", "observer:", "ingest: accepted", "unload:"):
            check(needle in run.stdout, f"the live run reported: {needle}")
    finally:
        demo.terminate()
        try:
            demo.wait(timeout=10)
        except subprocess.TimeoutExpired:
            demo.kill()

    # The ledger the run left behind (the daemon is gone, so the file is quiet).
    os.environ["SIERRALAB_HOME"] = str(home)
    os.environ["SIERRALAB_STATE_DB"] = str(home / "state.db")
    import state_store

    gui = {}
    for row in state_store.recent_events(kind="gui", limit=200):
        gui.setdefault(row["payload"].get("action"), []).append(row["payload"])
    check("reject" in gui and "isolate" in gui, f"the rejection and the isolation were audited ({sorted(gui)})")
    check("approve" in gui, "the approval was audited")
    check({a["to"] for a in gui.get("mode", [])} >= {"auto", "build"} and not any(a["to"] == "yolo" for a in gui.get("mode", [])), "mode changes were audited (the refused one never reached the child)")
    check({"steer", "isolate", "release"} <= set(gui), f"stop, release and steer were audited ({sorted(gui)})")
    check(all("text" not in a for a in gui.get("steer", [])), "a steer is audited by length, never by what was typed")
    check("unload_memory" in gui and gui["unload_memory"][0]["unloaded"], "the unload was audited, with what left memory")
    check(any(k["knob"] == "reasoning_effort" and k["ok"] for k in gui.get("knob", [])), "an applied effort knob was audited")
    check(any(not k["ok"] for k in gui.get("knob", [])), "a refused knob was audited as refused")
    ingest = state_store.recent_events(kind="ingest", limit=20)
    check(any(r["payload"].get("name") == "changed" for r in ingest), "the ingested symbol reached the state log")
    ingest = state_store.recent_events(kind="ingest", limit=50)
    names = [(r["payload"].get("name"), r["payload"].get("change")) for r in ingest]
    check(("second_fn", "modified") in names and ("first_fn", "added") in names, f"the observer's symbols reached the state log asynchronously ({names})")
    check(("first_fn", "modified") not in names, "…and the untouched function was never re-sent as modified")
    secrets = [r for r in state_store.recent_events(limit=500) if "ghp_" in str(r["payload"])]
    check(not secrets, "no credential-shaped text reached the ledger")
    shutil.rmtree(home, ignore_errors=True)
    print("\nlab live smoke: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

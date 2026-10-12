"""A self-contained daemon for trying the Experimental Lab without a model, a key or a token spent.

    make lab-demo                # the daemon below + the GPUI window, torn down together
    make lab-demo WATCH=~/proj   # …and the window's observer watches that workspace

It is the real thing wherever it matters — the real ``Supervisor``, the real gRPC bridge (token,
telemetry hub, intervention arbitration, ingest, ``state.db``, audit rows) — and scripted wherever a
model would be: the session is ``fixtures/demo_acp_child.py``, which plays a believable run (pods
working, a Laya reading, a destructive command held for your answer, a 10,000-line diff) and really
applies the knobs you turn; oMLX is ``fake_omlx.FakeOmlx``, so **Unload Memory** unloads a model
that is not on your machine.

It never touches ``~/.cordata``: its home, token file and ledger live in a throwaway directory
(``--home``), and the window finds it through ``CORDATA_HOME`` — which is why this does not
disturb a daemon you already run.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import signal
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent


def configure_environment(home: Path) -> None:
    """Must run before the daemon modules are imported: they read these at import or first use."""
    home.mkdir(parents=True, exist_ok=True)
    os.environ["CORDATA_HOME"] = str(home)
    os.environ["CORDATA_DAEMON_CONFIG"] = str(home / "daemon.json")
    os.environ["CORDATA_STATE_DB"] = str(home / "state.db")
    os.environ["CORDATA_ACP_ENTRYPOINT"] = f"{sys.executable} {HERE / 'fixtures' / 'demo_acp_child.py'}"


DEMO_MODELS = {
    "Qwen2.5-Coder-7B-Instruct": 4.7,
    "Ternary-Bonsai-2-27B-mlx-2bit": 8.41,
    "Ternary-Bonsai-27B-mlx-2bit": 8.3,
}


async def serve(home: Path, workspace: Path, grpc_port: int) -> None:
    configure_environment(home)
    # The daemon's own log, as a file in the throwaway home rather than on stdout: the bridge says
    # there what each menu-bar turn carried (`grpc: submit_turn …`), which is what a person — or
    # `_lab_gui_live_smoke_test.py` — reads to confirm a capture arrived.
    import logging

    logging.basicConfig(filename=str(home / "daemon.log"), level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s")
    # Imported after the environment is set.
    import memory_budget
    import ollama_cli
    import omlx_manager
    from daemon.grpc_bridge import GrpcBridge
    from daemon.server import State, load_or_create_token
    from fake_omlx import FakeOmlx

    omlx = FakeOmlx(models=DEMO_MODELS, ceiling_gb=37.44, loaded={"Qwen2.5-Coder-7B-Instruct", "Ternary-Bonsai-2-27B-mlx-2bit"}).start()
    quiet_ollama = ollama_cli.OllamaCli(binary="/bin/echo", runner=lambda argv, **kw: subprocess.CompletedProcess(argv, 0, "", ""), ps_reader=lambda: [])
    budget = memory_budget.MemoryBudget(omlx_manager.OmlxManager(omlx.base_url), quiet_ollama, ceiling_gb=48, reserve_gb=10)

    state = State(port=0)
    bridge = GrpcBridge(state, port=grpc_port, budget=budget)
    port = await bridge.start()
    state.grpc_port = port
    load_or_create_token(0, port)  # the file the window reads to find this daemon
    session = await state.supervisor.spawn(str(workspace))
    await state.supervisor.write(session.session_id, json.dumps({"jsonrpc": "2.0", "method": "demo/start", "params": {"sessionId": session.session_id}}))

    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, stop.set)

    async def refill() -> None:
        """So the Unload button can be tried again: put the weights back a while after they leave."""
        while True:
            await asyncio.sleep(20)
            if not omlx.loaded:
                for name in ("Qwen2.5-Coder-7B-Instruct", "Ternary-Bonsai-2-27B-mlx-2bit"):
                    omlx.loaded[name] = 1.0

    refiller = asyncio.create_task(refill())
    print(f"demo daemon ready: gRPC 127.0.0.1:{port}, home {home}, workspace {workspace}, log {home / 'daemon.log'}", flush=True)
    try:
        await stop.wait()
    finally:
        refiller.cancel()
        await bridge.stop(0.5)
        await state.supervisor.stop_all(grace=1.0)
        omlx.stop()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--home", type=Path, default=None, help="throwaway CORDATA_HOME (default: a new temp dir)")
    parser.add_argument("--workspace", type=Path, default=None, help="the directory the demo session runs in")
    parser.add_argument("--grpc-port", type=int, default=0, help="0 = any free port")
    args = parser.parse_args()
    home = args.home or Path(tempfile.mkdtemp(prefix="cordata-demo-"))
    workspace = args.workspace or (home / "workspace")
    workspace.mkdir(parents=True, exist_ok=True)
    asyncio.run(serve(home, workspace, args.grpc_port))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

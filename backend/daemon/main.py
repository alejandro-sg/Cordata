"""Run the daemon: `make daemon`, or the LaunchAgent `make daemon-install` writes.

The pidfile is the one piece of state that outlives the process, and it exists
for a specific reason: LaunchAgent's `KeepAlive` will restart the daemon, and a
restart that raced an old process still holding the port would fail forever
without anyone noticing. So the pidfile is checked at startup and removed on the
way out, and a stale one (a pid that is gone, or that is not a daemon) is
replaced rather than believed.

Signals: SIGTERM/SIGINT stop every child before exiting. A daemon that exited
without stopping its sessions would leave orphaned `acp_server.py` processes
holding pipes nobody is reading — which is exactly the state a `KeepAlive`
restart would then compound.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import os
import re
import signal
import sys
from pathlib import Path

import uvicorn

from .grpc_bridge import GrpcBridge, grpc_port_from_env
from .server import DAEMON_HOST, DEFAULT_PORT, create_app, load_or_create_token, token_path

logger = logging.getLogger("sierralab.daemon")

_TOKEN_IN_URL = re.compile(r"([?&]token=)[^&\s\"']+")


class _RedactToken(logging.Filter):
    """Scrub `?token=...` from log lines.

    The socket takes its token as a query parameter (a browser `WebSocket`
    cannot set headers), and uvicorn logs the full request path at INFO when it
    accepts one — `access_log=False` does not cover that line. Under the
    LaunchAgent stderr is `daemon.log`, so without this the credential that the
    whole auth design keeps in a 0600 file is written to a log beside it."""

    def filter(self, record: logging.LogRecord) -> bool:
        try:
            message = record.getMessage()
        except Exception:  # noqa: BLE001 — a malformed record is not ours to crash on
            return True
        if "token=" in message:
            record.msg = _TOKEN_IN_URL.sub(r"\1REDACTED", message)
            record.args = None
        return True


def _redact_logs() -> None:
    # On the handlers, not the loggers: a logger's filters only see records
    # logged directly to it, and uvicorn's lines come from its own child loggers.
    for handler in logging.getLogger().handlers:
        handler.addFilter(_RedactToken())

REPO_ROOT = Path(__file__).resolve().parent.parent.parent


def pid_path() -> Path:
    override = os.environ.get("SIERRALAB_DAEMON_PID")
    if override:
        return Path(override)
    return REPO_ROOT / ".dev" / "daemon.pid"


def _is_daemon(pid: int) -> bool:
    """Is this pid a live process? Deliberately not "is it our daemon" — that
    needs a command-line read that differs per platform, and the cost of being
    wrong is only that a second daemon refuses to start with a clear message."""
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True  # exists, owned by someone else
    return True


def read_port() -> int:
    override = os.environ.get("SIERRALAB_DAEMON_PORT")
    if override:
        return int(override)
    try:
        data = json.loads(token_path().read_text(encoding="utf-8"))
        port = data.get("port")
        if isinstance(port, int):
            return port
    except (OSError, ValueError):
        pass
    return DEFAULT_PORT


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="sierralab-daemon", description="always-on orchestrator daemon")
    parser.add_argument("--port", type=int, default=None)
    parser.add_argument("--grpc-port", type=int, default=None, help="the gRPC telemetry bridge's port (default 8766; 0 = any free port; SIERRALAB_GRPC_PORT)")
    parser.add_argument("--no-grpc", action="store_true", help="do not start the gRPC telemetry bridge")
    parser.add_argument("--log-level", default=os.environ.get("SIERRALAB_DAEMON_LOG", "info"))
    parser.add_argument("--check", action="store_true", help="report whether a daemon is already running, and exit")
    args = parser.parse_args(argv)

    logging.basicConfig(level=args.log_level.upper(), format="%(asctime)s %(levelname)s %(name)s: %(message)s")
    _redact_logs()

    pidfile = pid_path()
    if pidfile.exists():
        try:
            running = int(pidfile.read_text(encoding="utf-8").strip())
        except (OSError, ValueError):
            running = 0
        if running and _is_daemon(running):
            if args.check:
                print(f"daemon running (pid {running}, port {read_port()})")
                return 0
            print(f"daemon already running (pid {running}); stop it first", file=sys.stderr)
            return 1
        logger.warning("removing stale pidfile %s (pid %s)", pidfile, running)
        pidfile.unlink(missing_ok=True)
    elif args.check:
        print("no daemon running")
        return 1

    port = args.port or read_port()
    # The port is handed to the app because `create_app` is what writes it into
    # daemon.json — the file clients read to find this daemon. Recording the
    # default while listening elsewhere is worse than recording nothing.
    app = create_app(port)
    # `log_config=None`: uvicorn's own config gives its loggers private handlers
    # that do not propagate, which would put them beyond `_redact_logs`.
    config = uvicorn.Config(app, host=DAEMON_HOST, port=port, log_level=args.log_level, access_log=False, log_config=None)
    server = uvicorn.Server(config)

    async def run() -> None:
        loop = asyncio.get_running_loop()
        stop = asyncio.Event()

        def request_stop(*_: object) -> None:
            logger.info("daemon: shutting down")
            stop.set()

        for sig in (signal.SIGTERM, signal.SIGINT):
            try:
                loop.add_signal_handler(sig, request_stop)
            except NotImplementedError:  # pragma: no cover — non-POSIX
                signal.signal(sig, request_stop)

        pidfile.parent.mkdir(parents=True, exist_ok=True)
        pidfile.write_text(str(os.getpid()), encoding="utf-8")
        observer = None
        bridge: GrpcBridge | None = None
        try:
            if not args.no_grpc:
                grpc_port = args.grpc_port if args.grpc_port is not None else grpc_port_from_env()
                bridge = GrpcBridge(app.state.daemon, port=grpc_port)
                try:
                    app.state.daemon.grpc_port = await bridge.start()
                    # The file clients read to find this daemon names the bridge's port too.
                    load_or_create_token(port, app.state.daemon.grpc_port)
                except Exception:
                    # A bridge that cannot start (port taken) must not take the daemon down: the
                    # WebSocket front door is the one sessions depend on.
                    logger.exception("daemon: gRPC bridge failed to start; continuing without it")
                    await bridge.stop()
                    bridge = None
            # `serve()` inside the task, so the signal handler can reach the
            # shutdown path that stops the children; a bare `server.run()` would
            # return on SIGINT without giving this a chance to run.
            serving = asyncio.create_task(server.serve())
            # Started here, not in `create_app`: the observer needs a running
            # event loop, and it is a no-op task when `observer.enabled` is off.
            observer = app.state.daemon.observer.start()
            # The Discord front door (discord_bot.py), if the environment configures one. Started
            # here for the same reason the observer is: it needs a running loop. `start()` returns
            # None and logs why when there is no token, so an unconfigured daemon is unchanged.
            app.state.daemon.remote.start()
            await stop.wait()
            server.should_exit = True
            await serving
        finally:
            if bridge is not None:
                await bridge.stop()
            app.state.daemon.observer.stop()
            if observer is not None:
                # `stop()` sets the event `_run_child` is racing against, so a
                # cycle in flight kills its child and returns on its own — the
                # cancel is only the fallback for a runner that is stuck.
                try:
                    await asyncio.wait_for(observer, timeout=10)
                except (asyncio.TimeoutError, asyncio.CancelledError):
                    logger.warning("daemon: observer did not stop in time; cancelling")
                    observer.cancel()
            # A bot left running at shutdown would keep a gateway connection open and could still
            # answer an approval on a session this daemon is about to kill; `stop()` unsubscribes
            # from the supervisor first, so nothing new is posted while it is closing.
            await app.state.daemon.remote.stop()
            await app.state.daemon.supervisor.stop_all()
            pidfile.unlink(missing_ok=True)

    try:
        asyncio.run(run())
    except KeyboardInterrupt:  # pragma: no cover — signals are handled above
        return 130
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

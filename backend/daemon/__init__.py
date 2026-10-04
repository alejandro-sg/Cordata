"""The always-on orchestrator daemon: a supervisor that owns sessions and
serves them over a local WebSocket.

Why a supervisor and not an in-process host. `acp_server._build_session` does a
process-wide `os.chdir(cwd)` (`acp_server.py`), so two sessions for two project
directories cannot share a process — whichever built last would win, and every
tool in both sessions would run against its cwd. That is the same reason the
Rust client calls one-handle-one-process a hard requirement. So the daemon keeps
one `acp_server.py` child per session, and this package is the registry, the
transport and the lifecycle around them — never the host of an agent.

Nothing here is required for the harness to work. `--transport stdio`, the
default, spawns `acp_server.py` directly and never loads this package; the
daemon is an alternative front door, and every module below is written so that
importing it is cheap and side-effect-free.
"""

from __future__ import annotations

__all__ = ["frames", "supervisor", "server", "main"]

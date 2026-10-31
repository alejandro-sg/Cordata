"""Reads/writes backend/mcp_servers.json — the local, gitignored MCP-server
config the settings UI's MCP tab manages — and turns it into MCPClient
instances agent_common.py hands to every Agent's tools= list.

strands.tools.mcp.MCPClient.load_servers already does the real work: reads
the same "mcpServers" JSON shape Claude Desktop/Claude Code use, auto-detects
stdio (``command``) vs streamable-http (``url``) transport, and handles
``${VAR}`` env interpolation. This module only owns the file itself
(read/add/remove) and the one load call agent_common.py needs at process
start.

Scopes: the default (``backend``) is the web UI's file above, loaded lazily by
agent_common.mcp_clients(). ``user`` (``~/.cordata/mcp.json``) and ``project``
(``<project>/.cordata/mcp.json``) are the files plugin_host.PluginHost discovers
for every ACP session — writing one and calling ``PluginHost.discover()`` +
``enable(name)`` hot-registers the server's tools into attached agents with no
restart (registry.py's ``cordata mcp add`` does exactly that). This module only
writes the files, atomically; it never starts a server.
"""

import json
import os
from pathlib import Path

from strands.tools.mcp import MCPClient

MCP_CONFIG_PATH = Path(__file__).resolve().parent / "mcp_servers.json"
SCOPES = ("backend", "user", "project")


def config_path(scope: str = "backend", project: str | os.PathLike | None = None) -> Path:
    """The file a scope writes: ``backend`` (this checkout's), ``user`` (``~/.cordata/mcp.json``, honouring
    ``CORDATA_HOME``) or ``project`` (``<project>/.cordata/mcp.json``)."""
    if scope == "backend":
        return MCP_CONFIG_PATH
    if scope == "user":
        import settings_store  # noqa: PLC0415 — keeps this module import-light for main.py

        return settings_store.user_dir() / "mcp.json"
    if scope == "project":
        return Path(project or os.getcwd()).expanduser().resolve() / ".cordata" / "mcp.json"
    raise ValueError(f"unknown MCP scope {scope!r} (one of {', '.join(SCOPES)})")


def _read_config(path: Path = MCP_CONFIG_PATH) -> dict:
    if not path.exists():
        return {"mcpServers": {}}
    data = json.loads(path.read_text() or "{}")
    data.setdefault("mcpServers", {})
    return data


def _write_config(data: dict, path: Path = MCP_CONFIG_PATH) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp_path = path.with_name(path.name + ".tmp")
    # 0600: an entry can carry a server's credentials (`cordata mcp add … --env TOKEN=…`).
    fd = os.open(tmp_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as handle:
        handle.write(json.dumps(data, indent=2) + "\n")
    os.replace(tmp_path, path)


def load_configured_mcp_clients() -> list[MCPClient]:
    """One MCPClient per enabled server in mcp_servers.json, ready for
    ``Agent(tools=...)`` — MCPClient implements ToolProvider directly, so no
    manual tool extraction is needed. Returns [] if the file doesn't exist
    yet (a fresh checkout with no servers configured) rather than raising.

    continue_on_error=True: one misconfigured server (a missing binary, a
    bad env var) shouldn't take every other tool in the agent down with it —
    same reasoning agent_memory_tool.py's own graceful-degradation already
    applies to an unreachable Qdrant.
    """
    if not MCP_CONFIG_PATH.exists():
        return []
    return MCPClient.load_servers(str(MCP_CONFIG_PATH), continue_on_error=True)


def list_servers(scope: str = "backend", project: str | os.PathLike | None = None) -> list[dict]:
    """Every configured server, name plus its raw config. Not redacted the
    way provider_settings.get_provider_state redacts API keys: this is the
    user's own local file, edited through their own settings UI, already
    gitignored precisely because it's the one place in this repo that can
    carry a secret (see .gitignore's own comment) — there's no second party
    this value is ever hidden from.
    """
    servers = _read_config(config_path(scope, project))["mcpServers"]
    return [{"name": name, **config} for name, config in servers.items()]


def add_server(name: str, config: dict, scope: str = "backend", project: str | os.PathLike | None = None) -> Path:
    """Add or replace one server entry by name.

    No validation beyond "is a non-empty name and a dict" — MCPClient.
    load_servers itself is the real validator, the next time the backend
    starts and actually tries to connect each configured server.
    """
    if not name or not isinstance(config, dict):
        raise ValueError("name must be non-empty and config must be an object")
    path = config_path(scope, project)
    data = _read_config(path)
    data["mcpServers"][name] = config
    _write_config(data, path)
    return path


def remove_server(name: str, scope: str = "backend", project: str | os.PathLike | None = None) -> bool:
    """Remove one server entry by name. Returns whether it existed, so the
    caller (main.py's DELETE route) can 404 on an already-gone name instead
    of silently reporting success for nothing.
    """
    path = config_path(scope, project)
    data = _read_config(path)
    existed = data["mcpServers"].pop(name, None) is not None
    if existed:
        _write_config(data, path)
    return existed

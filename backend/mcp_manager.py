"""Reads/writes backend/mcp_servers.json — the local, gitignored MCP-server
config the settings UI's MCP tab manages — and turns it into MCPClient
instances agent_common.py hands to every Agent's tools= list.

strands.tools.mcp.MCPClient.load_servers already does the real work: reads
the same "mcpServers" JSON shape Claude Desktop/Claude Code use, auto-detects
stdio (``command``) vs streamable-http (``url``) transport, and handles
``${VAR}`` env interpolation. This module only owns the file itself
(read/add/remove) and the one load call agent_common.py needs at process
start.

Loaded once at process start (agent_common.py's TOOLS), same as
skills_generated/ — a server added or removed here takes effect on the next
backend restart, not hot-reloaded into an already-running agent (see
add_server/remove_server's own docstrings).
"""

import json
import os
from pathlib import Path

from strands.tools.mcp import MCPClient

MCP_CONFIG_PATH = Path(__file__).resolve().parent / "mcp_servers.json"


def _read_config() -> dict:
    if not MCP_CONFIG_PATH.exists():
        return {"mcpServers": {}}
    data = json.loads(MCP_CONFIG_PATH.read_text())
    data.setdefault("mcpServers", {})
    return data


def _write_config(data: dict) -> None:
    tmp_path = MCP_CONFIG_PATH.with_name(MCP_CONFIG_PATH.name + ".tmp")
    tmp_path.write_text(json.dumps(data, indent=2) + "\n")
    os.replace(tmp_path, MCP_CONFIG_PATH)


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


def list_servers() -> list[dict]:
    """Every configured server, name plus its raw config. Not redacted the
    way provider_settings.get_provider_state redacts API keys: this is the
    user's own local file, edited through their own settings UI, already
    gitignored precisely because it's the one place in this repo that can
    carry a secret (see .gitignore's own comment) — there's no second party
    this value is ever hidden from.
    """
    servers = _read_config()["mcpServers"]
    return [{"name": name, **config} for name, config in servers.items()]


def add_server(name: str, config: dict) -> None:
    """Add or replace one server entry by name.

    No validation beyond "is a non-empty name and a dict" — MCPClient.
    load_servers itself is the real validator, the next time the backend
    starts and actually tries to connect each configured server.
    """
    if not name or not isinstance(config, dict):
        raise ValueError("name must be non-empty and config must be an object")
    data = _read_config()
    data["mcpServers"][name] = config
    _write_config(data)


def remove_server(name: str) -> bool:
    """Remove one server entry by name. Returns whether it existed, so the
    caller (main.py's DELETE route) can 404 on an already-gone name instead
    of silently reporting success for nothing.
    """
    data = _read_config()
    existed = data["mcpServers"].pop(name, None) is not None
    if existed:
        _write_config(data)
    return existed

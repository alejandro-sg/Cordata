"""Everything is a plugin: hot-swappable tool, memory and language-server
plugins for live agents, interconnected through MCP.

A *plugin* contributes tools to every agent attached to the host. Two transports:

* **MCP** (``stdio`` / ``streamable-http`` / ``sse``): any MCP server — a tool
  server, or a memory store such as a Bedrock Knowledge Base retrieval server
  (``"kind": "memory"``). Declared in the same ``mcpServers`` JSON shape Claude
  Code / Claude Desktop use.
* **built-in**: tool groups that ship with the backend and can be swapped out
  like any other plugin (``agent_memory`` as the default memory store, the LSP
  bridge's tools).

Hot swap: ``enable`` starts the server, lists its tools and registers them in
each attached agent's live tool registry; ``disable`` removes them and stops the
server. No session restart, no lost conversation — the next model call simply
sees a different tool list (verified in ``_harness_loop_smoke_test.py``).

Where MCP servers come from (lowest precedence first; a later file's server of
the same name replaces an earlier one):

    backend   backend/mcp_servers.json           (the web settings UI's file)
    user      ~/.sierralab/mcp.json
    project   <cwd>/.mcp.json, <cwd>/.sierralab/mcp.json   (committed with a repo)
    local     <cwd>/.sierralab/mcp.local.json    (this machine only)

Trust: a project or local server is a command a cloned repository wants to run
on your machine, so it starts only once the folder is trusted (``/trust``,
same rule as project ``allow`` rules in ``settings_store``). Until then it is
listed as ``blocked`` with the reason.

Beyond the MCP keys, an entry may carry: ``description``, ``kind`` (``tools`` |
``memory``), ``approval`` (``ask`` — default — or ``allow``: whether its tools
pause for approval in build mode; ``allow`` is honoured only from trusted
sources), ``autostart`` (default true) and ``untrusted_output`` (default true:
results are wrapped in the untrusted-data guard, like web content).

``/plugins disable <name>`` can persist the choice in
``<cwd>/.sierralab/plugins.local.json`` (``{"disabled": [...]}``).
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import re
import threading
from collections.abc import Callable, Iterable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from strands.tools.mcp import MCPClient

import harness_events
import settings_store

logger = logging.getLogger(__name__)

BACKEND_DIR = Path(__file__).resolve().parent
_EXTRA_KEYS = ("description", "kind", "approval", "autostart", "untrusted_output")
_KINDS = ("tools", "memory", "lsp", "agent")
_NAME = re.compile(r"^[A-Za-z0-9_.-]{1,48}$")


@dataclass
class PluginSpec:
    name: str
    kind: str = "tools"
    transport: str = "stdio"  # stdio | streamable-http | sse | builtin
    source: str = "user"  # built-in | backend | user | project | local
    description: str = ""
    config: dict = field(default_factory=dict)  # raw MCP server config
    approval: str = "ask"
    autostart: bool = True
    untrusted_output: bool = True
    path: str | None = None
    builtin_tools: Callable[[], list] | None = None

    @property
    def needs_trust(self) -> bool:
        return self.source in ("project", "local")


@dataclass
class _Running:
    status: str = "stopped"  # stopped | starting | running | failed | blocked | disabled
    error: str | None = None
    tools: list = field(default_factory=list)  # AgentTool objects
    client: MCPClient | None = None
    active_call: str | None = None  # tool currently running, for progress attribution


def _sanitize(name: str) -> str:
    return re.sub(r"[^A-Za-z0-9_-]", "_", name)


def mcp_config_paths(cwd: str | os.PathLike) -> list[tuple[str, Path]]:
    cwd = Path(cwd)
    return [
        ("backend", BACKEND_DIR / "mcp_servers.json"),
        ("user", settings_store.user_dir() / "mcp.json"),
        ("project", cwd / ".mcp.json"),
        ("project", cwd / ".sierralab" / "mcp.json"),
        ("local", cwd / ".sierralab" / "mcp.local.json"),
    ]


def _read_servers(path: Path, warnings: list[str]) -> dict:
    if not path.exists():
        return {}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        warnings.append(f"{path}: unreadable ({error})")
        return {}
    servers = data.get("mcpServers", data) if isinstance(data, dict) else None
    if not isinstance(servers, dict):
        warnings.append(f"{path}: expected an object of MCP servers")
        return {}
    return servers


def discover_specs(cwd: str | os.PathLike, paths: list[tuple[str, Path]] | None = None) -> tuple[list[PluginSpec], list[str]]:
    """Every MCP plugin declared for ``cwd``; never raises."""
    warnings: list[str] = []
    found: dict[str, PluginSpec] = {}
    for source, path in paths if paths is not None else mcp_config_paths(cwd):
        for name, raw in _read_servers(path, warnings).items():
            if not isinstance(raw, dict) or not _NAME.match(str(name)):
                warnings.append(f"{path}: server {name!r} skipped (bad name or not an object)")
                continue
            if raw.get("disabled"):
                continue
            transport = raw.get("transport") or ("stdio" if raw.get("command") else "streamable-http" if raw.get("url") else "")
            if not transport:
                warnings.append(f"{path}: server {name!r} has neither 'command' nor 'url'")
                continue
            kind = raw.get("kind", "tools")
            if kind not in _KINDS:
                warnings.append(f"{path}: server {name!r} has unknown kind {kind!r}; treated as 'tools'")
                kind = "tools"
            found[name] = PluginSpec(
                name=name,
                kind=kind,
                transport=transport,
                source=source,
                description=str(raw.get("description") or ""),
                config={k: v for k, v in raw.items() if k not in _EXTRA_KEYS},
                approval="allow" if raw.get("approval") == "allow" else "ask",
                autostart=raw.get("autostart", True) is not False,
                untrusted_output=raw.get("untrusted_output", True) is not False,
                path=str(path),
            )
    return list(found.values()), warnings


class _LiveNames:
    """A container whose membership is asked of the host at lookup time, so
    hooks built once (the untrusted-output guard) see plugins swapped in later."""

    def __init__(self, predicate: Callable[[str], bool]) -> None:
        self._predicate = predicate

    def __contains__(self, name: object) -> bool:
        return isinstance(name, str) and self._predicate(name)


class PluginHost:
    def __init__(
        self,
        cwd: str | os.PathLike,
        builtins: Iterable[PluginSpec] = (),
        *,
        config_paths: list[tuple[str, Path]] | None = None,
        reserved_names: Iterable[str] = (),
    ) -> None:
        self.cwd = str(cwd)
        self._config_paths = config_paths
        self._builtins = {spec.name: spec for spec in builtins}
        self._specs: dict[str, PluginSpec] = {}
        self._state: dict[str, _Running] = {}
        self._agents: list[Any] = []
        self._reserved = set(reserved_names)
        self._lock = threading.RLock()
        self.warnings: list[str] = []
        self.untrusted_names = _LiveNames(self._is_untrusted_output)
        self.discover()

    # ------------------------------------------------------------ discovery

    def discover(self) -> None:
        specs, warnings = discover_specs(self.cwd, self._config_paths)
        with self._lock:
            self.warnings = warnings
            self._specs = {**self._builtins, **{spec.name: spec for spec in specs}}
            for name in self._specs:
                self._state.setdefault(name, _Running())

    @property
    def trusted(self) -> bool:
        return settings_store.current(self.cwd).trusted

    def _disabled_path(self) -> Path:
        return Path(self.cwd) / ".sierralab" / "plugins.local.json"

    def persisted_disabled(self) -> set[str]:
        try:
            data = json.loads(self._disabled_path().read_text(encoding="utf-8"))
            return {str(name) for name in data.get("disabled", [])}
        except (OSError, ValueError, AttributeError):
            return set()

    def _persist_disabled(self, name: str, disabled: bool) -> None:
        names = self.persisted_disabled()
        (names.add if disabled else names.discard)(name)
        path = self._disabled_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_suffix(".tmp")
        tmp.write_text(json.dumps({"disabled": sorted(names)}, indent=2) + "\n", encoding="utf-8")
        os.replace(tmp, path)

    # --------------------------------------------------------------- agents

    def attach(self, agent: Any) -> None:
        """Give ``agent`` every running plugin's tools now and on every later change."""
        with self._lock:
            if agent in self._agents:
                return
            self._agents.append(agent)
            for state in self._state.values():
                if state.status == "running":
                    self._register(agent, state.tools)

    def detach(self, agent: Any) -> None:
        with self._lock:
            if agent in self._agents:
                self._agents.remove(agent)

    def _register(self, agent: Any, tools: list) -> None:
        try:
            agent.tool_registry.process_tools(list(tools))
        except Exception as error:
            logger.warning("plugin tools could not be registered on %s: %s", getattr(agent, "name", agent), error)

    @staticmethod
    def _unregister(agent: Any, names: Iterable[str]) -> None:
        for name in names:
            agent.tool_registry.registry.pop(name, None)
            agent.tool_registry.dynamic_tools.pop(name, None)

    # ------------------------------------------------------------ lifecycle

    def start_autostart(self) -> None:
        """Start every plugin that should run for this folder (sequentially;
        call from a worker thread — MCP servers can take seconds to start).

        A *project's own* plugin declarations (``.mcp.json``,
        ``.sierralab/mcp.json``, ``mcp.local.json``) are started only when the
        folder is trusted — the same ``trustedProjects`` check the settings
        files use, and the same principle (a stranger's repo cannot activate
        anything of its own). Spawning a server is a launch-time action, not a
        gated tool call, so trust is the gate here; a plugin tool *call* still
        prompts like any MCP tool regardless. Plugins this backend or the
        user's own file declared start regardless of trust; an untrusted
        project plugin stays ``stopped`` and can be started by hand later."""
        trusted = self.trusted
        disabled = self.persisted_disabled()
        for name, spec in list(self._specs.items()):
            if name in disabled:
                self._state[name].status = "disabled"
            elif spec.autostart and (trusted or spec.source in ("backend", "user")):
                self.enable(name)
            else:
                self._state[name].status = "disabled"

    def enable(self, name: str, persist: bool = False) -> dict:
        with self._lock:
            spec = self._specs.get(name)
            if spec is None:
                return {"ok": False, "error": f"no plugin named {name!r}"}
            state = self._state[name]
            if state.status in ("running", "starting"):
                return {"ok": True, **self._row(name)}
            if spec.needs_trust and not self.trusted:
                state.status, state.error = "blocked", "folder not trusted — run /trust to let this project start it"
                self._announce(name)
                return {"ok": False, "error": state.error, **self._row(name)}
            state.status, state.error = "starting", None
        self._announce(name)

        task = harness_events.Task(f"plugin {name}", source="plugins")
        try:
            tools, client = self._load(spec, state)
        except Exception as error:
            with self._lock:
                state.status, state.error, state.client = "failed", f"{type(error).__name__}: {error}", None
            task.finish("failed", state.error)
            self._announce(name)
            return {"ok": False, "error": state.error, **self._row(name)}

        with self._lock:
            state.tools, state.client, state.status = tools, client, "running"
            for agent in self._agents:
                self._register(agent, tools)
        if persist:
            self._persist_disabled(name, False)
        task.finish("completed", f"{len(tools)} tools")
        self._announce(name)
        return {"ok": True, **self._row(name)}

    def disable(self, name: str, persist: bool = False) -> dict:
        with self._lock:
            spec = self._specs.get(name)
            if spec is None:
                return {"ok": False, "error": f"no plugin named {name!r}"}
            state = self._state[name]
            names = [tool.tool_name for tool in state.tools]
            for agent in self._agents:
                self._unregister(agent, names)
            client, state.client, state.tools = state.client, None, []
            state.status, state.error = "disabled", None
        if client is not None:
            try:
                client.stop(None, None, None)
            except Exception:
                logger.debug("plugin %s did not stop cleanly", name, exc_info=True)
        if persist:
            self._persist_disabled(name, True)
        self._announce(name)
        return {"ok": True, **self._row(name)}

    def shutdown(self) -> None:
        for name, state in list(self._state.items()):
            if state.client is not None:
                self.disable(name)

    def _load(self, spec: PluginSpec, state: _Running) -> tuple[list, MCPClient | None]:
        if spec.transport == "builtin":
            return list(spec.builtin_tools() if spec.builtin_tools else []), None

        async def on_progress(value, total, message) -> None:  # MCP awaits it
            self._progress(spec.name, value, total, message)

        def build(prefix: str | None) -> MCPClient:
            config = dict(spec.config)
            if prefix is not None:
                config["prefix"] = prefix
            [client] = MCPClient.load_servers({spec.name: config})
            client._progress_callback = on_progress  # load_servers has no progress parameter
            return client

        client = build(None)
        client.start()
        tools = client.list_tools_sync()
        taken = self._reserved | self._other_tool_names(spec.name)
        if any(tool.tool_name in taken for tool in tools) and "prefix" not in spec.config:
            # A server's tool would shadow a core tool or another plugin's: prefix
            # this server's tools with its name instead of silently replacing.
            client.stop(None, None, None)
            client = build(_sanitize(spec.name))
            client.start()
            tools = client.list_tools_sync()
        clash = [tool.tool_name for tool in tools if tool.tool_name in taken]
        if clash:
            client.stop(None, None, None)
            raise ValueError(f"tool names already taken: {', '.join(clash)}")
        return list(tools), client

    def _other_tool_names(self, except_name: str) -> set[str]:
        with self._lock:
            return {
                tool.tool_name
                for name, state in self._state.items()
                if name != except_name and state.status == "running"
                for tool in state.tools
            }

    # -------------------------------------------------------------- queries

    def owner(self, tool_name: str) -> PluginSpec | None:
        with self._lock:
            for name, state in self._state.items():
                if state.status == "running" and any(tool.tool_name == tool_name for tool in state.tools):
                    return self._specs.get(name)
        return None

    def requires_approval(self, tool_name: str) -> bool | None:
        """None: not a plugin tool. Otherwise whether build mode should ask.
        ``approval: allow`` counts only from a source that needs no trust or a
        trusted folder — a repo can't switch off prompts for its own server."""
        spec = self.owner(tool_name)
        if spec is None or spec.transport == "builtin":
            return None
        if spec.approval == "allow" and (not spec.needs_trust or self.trusted):
            return False
        return True

    def _is_untrusted_output(self, tool_name: str) -> bool:
        spec = self.owner(tool_name)
        return spec is not None and spec.transport != "builtin" and spec.untrusted_output

    def note_call(self, tool_name: str, starting: bool) -> None:
        """Attribute MCP progress notifications to the call that is running."""
        spec = self.owner(tool_name)
        if spec is None:
            return
        with self._lock:
            self._state[spec.name].active_call = tool_name if starting else None

    def _progress(self, plugin: str, value: float, total: float | None, message: str | None) -> None:
        state = self._state.get(plugin)
        call = state.active_call if state else None
        harness_events.emit(
            "task",
            {
                "id": f"mcp-{plugin}-{call or 'call'}",
                "title": f"{plugin}: {call or 'working'}",
                "done": value,
                "total": total,
                "message": message or "",
                "state": "completed" if total and value >= total else "running",
                "source": "plugins",
            },
        )

    def tool_names(self, name: str) -> list[str]:
        with self._lock:
            state = self._state.get(name)
            return [tool.tool_name for tool in state.tools] if state else []

    def descriptions(self) -> list[tuple[str, str, str]]:
        """(name, kind, description) for running plugins — Laya's routing targets."""
        with self._lock:
            return [
                (name, spec.kind, spec.description or f"tools from the {name} server")
                for name, spec in self._specs.items()
                if self._state[name].status == "running" and spec.transport != "builtin"
            ]

    def _row(self, name: str) -> dict:
        spec, state = self._specs[name], self._state[name]
        return {
            "name": name,
            "kind": spec.kind,
            "transport": spec.transport,
            "source": spec.source,
            "status": state.status,
            "error": state.error,
            "tools": [tool.tool_name for tool in state.tools],
            "approval": "ask" if spec.transport != "builtin" and self.requires_approval_for_spec(spec) else "allow",
            "description": spec.description,
        }

    def requires_approval_for_spec(self, spec: PluginSpec) -> bool:
        return not (spec.approval == "allow" and (not spec.needs_trust or self.trusted))

    def status(self) -> list[dict]:
        with self._lock:
            return [self._row(name) for name in self._specs]

    def _announce(self, name: str) -> None:
        with self._lock:
            row = self._row(name)
        harness_events.emit("plugins", {"event": "changed", "plugin": row})


async def run_blocking(fn: Callable, *args, **kwargs):
    """Plugin start/stop blocks for up to the server's startup timeout; keep it
    off the ACP event loop."""
    return await asyncio.to_thread(fn, *args, **kwargs)

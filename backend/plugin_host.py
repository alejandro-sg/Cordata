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
    user      ~/.cordata/mcp.json
    project   <cwd>/.vscode/mcp.json, <cwd>/.cursor/mcp.json, <cwd>/mcp.json
              (other editors' declarations: a repository set up for VS Code or
              Cursor brings its servers along — VS Code's ``servers`` / ``type``
              shape is read too, ``${workspaceFolder}`` is the project)
    project   <cwd>/.mcp.json, <cwd>/.cordata/mcp.json   (committed with a repo)
    local     <cwd>/.cordata/mcp.local.json    (this machine only)

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
``<cwd>/.cordata/plugins.local.json`` (``{"disabled": [...]}``).
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
        # Other editors' files first, so the project's own declarations win a name clash. All are "project": a command
        # a cloned repository names, started only in a trusted folder.
        ("project", cwd / ".vscode" / "mcp.json"),
        ("project", cwd / ".cursor" / "mcp.json"),
        ("project", cwd / "mcp.json"),
        ("project", cwd / ".mcp.json"),
        ("project", cwd / ".cordata" / "mcp.json"),
        ("local", cwd / ".cordata" / "mcp.local.json"),
    ]


def _read_servers(path: Path, warnings: list[str]) -> dict:
    if not path.exists():
        return {}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        warnings.append(f"{path}: unreadable ({error})")
        return {}
    if not isinstance(data, dict):
        servers = None
    elif "mcpServers" in data:
        servers = data["mcpServers"]
    elif isinstance(data.get("servers"), dict):  # VS Code: {"inputs": [...], "servers": {...}}
        servers = data["servers"]
    else:
        servers = data
    if not isinstance(servers, dict):
        warnings.append(f"{path}: expected an object of MCP servers")
        return {}
    return servers


#: VS Code's ``type`` → the transport names the SDK's loader uses.
_VSCODE_TYPES = {"stdio": "stdio", "http": "streamable-http", "sse": "sse"}


def _editor_shaped(raw: dict, cwd: Path) -> tuple[dict, str | None]:
    """An entry from another editor's file, in this harness's shape: ``type`` → ``transport``, ``${workspaceFolder}`` →
    the project. Returns the entry and why it cannot be used (a VS Code ``${input:…}`` is a value VS Code prompts for;
    nothing here can answer that prompt)."""
    text = json.dumps(raw)
    if "${input:" in text:
        return raw, "needs a value VS Code prompts for (${input:…}); declare it in .cordata/mcp.json instead"
    if "${workspaceFolder" in text:
        raw = json.loads(text.replace("${workspaceFolder}", str(cwd)).replace("${workspaceFolderBasename}", cwd.name))
    if "type" in raw and "transport" not in raw and raw["type"] in _VSCODE_TYPES:
        raw = {**{k: v for k, v in raw.items() if k != "type"}, "transport": _VSCODE_TYPES[raw["type"]]}
    return raw, None


def discover_specs(cwd: str | os.PathLike, paths: list[tuple[str, Path]] | None = None) -> tuple[list[PluginSpec], list[str]]:
    """Every MCP plugin declared for ``cwd``; never raises."""
    warnings: list[str] = []
    found: dict[str, PluginSpec] = {}
    for source, path in paths if paths is not None else mcp_config_paths(cwd):
        for name, raw in _read_servers(path, warnings).items():
            if not isinstance(raw, dict) or not _NAME.match(str(name)):
                warnings.append(f"{path}: server {name!r} skipped (bad name or not an object)")
                continue
            raw, unusable = _editor_shaped(raw, Path(cwd))
            if unusable:
                warnings.append(f"{path}: server {name!r} skipped — {unusable}")
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
        return Path(self.cwd) / ".cordata" / "plugins.local.json"

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
        ``.cordata/mcp.json``, ``mcp.local.json``) are started only when the
        folder is trusted — the same ``trustedProjects`` check the settings
        files use, and the same principle (a stranger's repo cannot activate
        anything of its own). Spawning a server is a launch-time action, not a
        gated tool call, so trust is the gate here; a plugin tool *call* still
        prompts like any MCP tool regardless. Plugins this backend or the
        user's own file declared start regardless of trust, and so do the
        built-ins; an untrusted project plugin stays ``stopped`` and can be
        started by hand later."""
        trusted = self.trusted
        disabled = self.persisted_disabled()
        for name, spec in list(self._specs.items()):
            if name in disabled:
                self._state[name].status = "disabled"
            # Built-ins are this harness's own tools (the LSP tools, the memory store): nothing a repository declared,
            # so trust is not their gate. They used to wait for /trust like a project's server, which left every
            # untrusted folder without lsp_* tools while the language servers themselves were running.
            elif spec.autostart and (trusted or spec.source in ("backend", "user") or spec.transport == "builtin"):
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

    def requires_approval_for_server(self, name: str) -> bool | None:
        """Whether a call into server ``name`` asks (None: no such external server) — ``mcp_resources`` reads use it."""
        spec = self._specs.get(str(name or ""))
        if spec is None or spec.transport == "builtin":
            return None
        return self.requires_approval_for_spec(spec)

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
        if tool_name == RESOURCE_TOOL:
            return True  # a server's data, like its tools' output
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

    def resource_call(self, server: str, action: str, uri: str = "") -> str:
        """``mcp_resources`` (``resources_tool``). Never raises: a failure is the tool's text."""
        action = (action or "list").strip().lower()
        if action not in RESOURCE_ACTIONS:
            return f"mcp_resources: unknown action {action!r}; use one of {', '.join(RESOURCE_ACTIONS)}"
        with self._lock:
            spec, state = self._specs.get(server), self._state.get(server)
        if spec is None or state is None or spec.transport == "builtin":
            running = [row["name"] for row in self.status() if row["status"] == "running" and row["transport"] != "builtin"]
            return f"mcp_resources: no MCP server named {server!r}" + (f"; running: {', '.join(running)}" if running else "; none is running")
        if state.status != "running" or state.client is None:
            return f"mcp_resources: {server} is {state.status}" + (f" ({state.error})" if state.error else "")
        try:
            if action == "list":
                rows = state.client.list_resources_sync().resources
                if not rows:
                    return f"{server} offers no resources (try action=\"templates\")."
                return f"{server} resources ({len(rows)}):\n" + "\n".join(
                    f"- {r.uri}" + (f" — {r.name}" if getattr(r, "name", None) else "") + (f": {r.description}" if getattr(r, "description", None) else "") for r in rows[:100]
                )
            if action == "templates":
                listing = state.client.list_resource_templates_sync()
                # The mcp package spells it either way across versions (camelCase on the wire, snake_case in newer models).
                rows = getattr(listing, "resource_templates", None) or getattr(listing, "resourceTemplates", None) or []
                if not rows:
                    return f"{server} offers no resource templates."
                return f"{server} resource templates ({len(rows)}; fill the {{placeholders}} and read the URI):\n" + "\n".join(
                    f"- {getattr(t, 'uri_template', None) or getattr(t, 'uriTemplate', '')}" + (f" — {t.name}" if getattr(t, "name", None) else "") + (f": {t.description}" if getattr(t, "description", None) else "") for t in rows[:100]
                )
            if not uri.strip():
                return "mcp_resources: read needs a uri"
            if re.search(r"\{[^}]*\}", uri):
                return f"mcp_resources: {uri!r} still has template placeholders; fill them in first"
            return _resource_text(state.client.read_resource_sync(uri.strip()).contents) or f"{uri} is empty"
        except Exception as error:  # an MCP error is the server's answer, not a crash of the turn
            return f"mcp_resources: {server} could not {action}: {type(error).__name__}: {str(error)[:300]}"

    def requires_approval_for_spec(self, spec: PluginSpec) -> bool:
        return not (spec.approval == "allow" and (not spec.needs_trust or self.trusted))

    def status(self) -> list[dict]:
        with self._lock:
            return [self._row(name) for name in self._specs]

    def _announce(self, name: str) -> None:
        with self._lock:
            row = self._row(name)
        harness_events.emit("plugins", {"event": "changed", "plugin": row})


RESOURCE_TOOL = "mcp_resources"
RESOURCE_ACTIONS = ("list", "templates", "read")
MAX_RESOURCE_CHARS = 12_000


def _resource_text(contents: Any) -> str:
    parts = []
    for item in contents or []:
        text = getattr(item, "text", None)
        if isinstance(text, str):
            parts.append(text)
        elif getattr(item, "blob", None) is not None:
            parts.append(f"[binary resource, {getattr(item, 'mime_type', None) or getattr(item, 'mimeType', None) or 'unknown type'}, not shown]")
    joined = "\n".join(parts)
    return joined if len(joined) <= MAX_RESOURCE_CHARS else joined[:MAX_RESOURCE_CHARS] + f"\n…[{len(joined) - MAX_RESOURCE_CHARS:,} more characters]"


def resources_tool(host: "PluginHost") -> Any:
    """``mcp_resources`` — the *resources* and *resource templates* of the MCP servers running for this project (the MCP
    spec's other half: a server's data, addressed by URI or by an RFC 6570 template such as ``postgres://{table}/schema``),
    through the SDK's own ``MCPClient.list_resources_sync`` / ``list_resource_templates_sync`` / ``read_resource_sync``.

    Listing is metadata and never asks; ``read`` is a call into the server like any of its tools — it asks unless that
    server is ``approval: allow`` in a trusted declaration (``acp_server``), is refused while the vault blocks egress, and
    its result is fenced as untrusted third-party output."""
    from strands import tool  # noqa: PLC0415

    @tool(name=RESOURCE_TOOL)
    def mcp_resources(server: str, action: str = "list", uri: str = "") -> str:
        """Read an MCP server's resources: list them, list its resource templates, or read one by URI.

        Args:
            server: The MCP server's name (as in the "MCP servers connected" line).
            action: "list" (resources), "templates" (URI templates to fill in), or "read".
            uri: For "read": the resource URI — one from "list", or a template with its {placeholders} filled in.
        """
        return host.resource_call(server, action, uri)

    return mcp_resources


def capabilities_text(host: "PluginHost", limit: int = 900) -> str:
    """One short block for the model: which MCP servers this project has, what each offers, and which are waiting for
    ``/trust`` — so a server discovered in the workspace is something the agent knows it can use (or ask for), not
    only a schema in its tool list. Empty when the project has none. Builtin plugins (memory, LSP) are not listed."""
    running, blocked = [], []
    trusted = host.trusted
    for row in host.status():
        if row.get("transport") == "builtin":
            continue
        tools = [str(t) for t in row.get("tools") or []]
        name = str(row.get("name"))
        about = f" — {row['description']}" if row.get("description") else ""
        if row.get("status") == "running" and tools:
            shown = ", ".join(tools[:6]) + (f", … ({len(tools)} in all)" if len(tools) > 6 else "")
            running.append(f"{name}{about}: {shown}")
        elif row.get("status") == "blocked" or (row.get("source") in ("project", "local") and not trusted and row.get("status") != "running"):
            blocked.append(name)
    if not running and not blocked:
        return ""
    lines = []
    if running:
        lines.append(
            "MCP servers connected for this project (their tools are in your tool list; mcp_resources lists and reads their "
            "resources and resource templates): " + "; ".join(running)
        )
    if blocked:
        lines.append(f"Declared but not started until the person runs /trust: {', '.join(blocked)}.")
    text = "\n".join(lines)
    return text if len(text) <= limit else text[: limit - 1] + "…"


def capabilities_injector(host: "PluginHost") -> Any:
    """``capabilities_text`` handed to the model with each message the person sends (a Strands ``ContextInjector``:
    ephemeral, never stored, so it is always the plugins running *now* — one that started in the background after the
    session began is mentioned from the next message on)."""
    from strands.vended_plugins.context_injector import ContextInjector  # noqa: PLC0415

    return ContextInjector(lambda context: capabilities_text(host) or None, name="cordata:mcp", trigger="userTurn")


async def run_blocking(fn: Callable, *args, **kwargs):
    """Plugin start/stop blocks for up to the server's startup timeout; keep it
    off the ACP event loop."""
    return await asyncio.to_thread(fn, *args, **kwargs)

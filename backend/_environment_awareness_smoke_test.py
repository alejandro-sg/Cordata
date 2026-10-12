"""Environment awareness: MCP servers found in the workspace, a profile of the project, reasoning that follows the work.

* **MCP discovery** — a repository set up for Cursor (``.cursor/mcp.json``), VS Code (``.vscode/mcp.json``: ``servers`` /
  ``type``, ``${workspaceFolder}``) or plain ``mcp.json`` brings its servers along; a VS Code ``${input:…}`` server is
  skipped with the reason; the harness's own ``.mcp.json`` wins a name clash. All of them are *project* servers: started
  only in a trusted folder. Built-in plugins (the LSP tools) start regardless.
* **Capabilities in the prompt** — through the real ACP server, with the echo MCP server declared in ``.cursor/mcp.json``:
  untrusted, the model is told the server is waiting for ``/trust``; trusted, it is told the server is connected and
  what it offers, and the server's tools are in the tool list. Ephemeral (a ContextInjector): never stored.
* **Profiler** — size class, languages, manifests counted without running the project, unified memory; one line in the
  workspace context; the ``environment_profile`` tool adds toolchain versions.
* **Adaptive effort** — the table, never below the role's level (the Lab's dial and env overrides win), never ``max``
  unless chosen; the hook sets the model's ``reasoning_effort`` before each call from Laya's reading and the size class.

    backend/.venv/bin/python _environment_awareness_smoke_test.py
"""

import asyncio
import json
import os
import subprocess
import sys
from pathlib import Path
from types import SimpleNamespace

os.environ["CORDATA_PRISM"] = "off"

import provider_settings  # noqa: F401  (before agent_common, which chdirs)

import acp  # noqa: E402

import adaptive_effort  # noqa: E402
import env_profile  # noqa: E402
import ollama_cloud  # noqa: E402
import plugin_host  # noqa: E402
import settings_store  # noqa: E402
from _acp_harness_smoke_test import HERE, MODELS, ROOT, fresh_server, laya_answer, orchestrator_model, settle, text  # noqa: E402
from harness_testing import check  # noqa: E402

ROUTE = laya_answer("code", {"code": 0.97, "answer": 0.03})
ECHO = {"command": sys.executable, "args": [str(HERE / "fixtures" / "echo_mcp_server.py")], "description": "echoes text"}


def project(name: str) -> Path:
    path = ROOT / name
    path.mkdir(parents=True)
    (path / "README.md").write_text("demo\n")
    return path


def write(path: Path, data: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data))


def test_discovery() -> None:
    cwd = project("discover")
    write(cwd / ".cursor" / "mcp.json", {"mcpServers": {"pg": {"command": "postgres-mcp", "args": ["--dsn", "x"]}, "shared": {"command": "from-cursor"}}})
    write(cwd / ".vscode" / "mcp.json", {"inputs": [{"id": "token", "type": "promptString"}], "servers": {
        "gh": {"type": "http", "url": "https://api.example.test/mcp"},
        "local": {"type": "stdio", "command": "node", "args": ["${workspaceFolder}/tools/server.js"]},
        "needs_input": {"type": "stdio", "command": "x", "env": {"TOKEN": "${input:token}"}},
    }})
    write(cwd / "mcp.json", {"mcpServers": {"plain": {"command": "plain-mcp"}}})
    write(cwd / ".mcp.json", {"mcpServers": {"shared": {"command": "from-harness"}}})
    specs, warnings = plugin_host.discover_specs(cwd, [p for p in plugin_host.mcp_config_paths(cwd) if p[0] != "backend"])
    by = {s.name: s for s in specs}
    check({"pg", "gh", "local", "plain", "shared"} <= set(by), f"Cursor, VS Code and plain mcp.json servers are found ({sorted(by)})")
    check(by["gh"].transport == "streamable-http" and by["local"].transport == "stdio", "VS Code's type becomes the transport")
    check(by["local"].config["args"] == [f"{cwd}/tools/server.js"], "${workspaceFolder} is the project")
    check("needs_input" not in by and any("needs_input" in w and "${input" in w for w in warnings), "a server that needs a VS Code prompt is skipped, with the reason")
    check(by["shared"].config["command"] == "from-harness", "the project's own .mcp.json wins a name clash")
    check(all(by[n].source == "project" and by[n].needs_trust for n in ("pg", "gh", "local", "plain")), "every one of them is a project server: it needs trust")


async def test_capabilities_through_acp() -> None:
    for trusted in (False, True):
        cwd = project(f"caps-{trusted}")
        write(cwd / ".cursor" / "mcp.json", {"mcpServers": {"echo": ECHO}})
        if trusted:
            settings_store.trust_project(str(cwd))
        orchestrator_model([text("ok")])
        server, _ = fresh_server(ROUTE)
        sid = (await server.new_session(cwd=str(cwd))).session_id
        host = server._sessions[sid].plugins
        for _ in range(200):
            rows = {row["name"]: row for row in host.status()}
            if rows.get("echo", {}).get("status") in ("running", "blocked", "disabled", "failed") and rows.get("lsp", {}).get("status") == "running":
                break
            await asyncio.sleep(0.05)
        await server.prompt(sid, [acp.text_block("what can you reach from here?")])
        await settle()
        seen = json.dumps(MODELS["orchestrator"].calls[0])
        tools = MODELS["orchestrator"].calls[0]["tools"]
        lsp_ok = "lsp_references" in tools
        if trusted:
            check("MCP servers connected for this project" in MODELS["orchestrator"].calls[0]["last_text"] and "echo — echoes text: echo, slow_count" in MODELS["orchestrator"].calls[0]["last_text"], "trusted: the model is told the discovered server is connected and what it offers")
            check("echo" in tools and "slow_count" in tools, "…and its tools are in the tool list")
            listing = host.resource_call("echo", "list")
            templates = host.resource_call("echo", "templates")
            check("notes://readme — readme: what this server is" in listing, f"mcp_resources lists the server's resources ({listing.splitlines()[0]})")
            check("notes://{topic} — topic" in templates, "…and its resource templates")
            check(host.resource_call("echo", "read", "notes://readme") == "The echo server: echo and slow_count." and host.resource_call("echo", "read", "notes://testing") == "note about testing", "…and reads a resource, and a filled-in template")
            check("placeholders" in host.resource_call("echo", "read", "notes://{topic}"), "an unfilled template is refused with the reason")
            check("no MCP server named 'lsp'" in host.resource_call("lsp", "list"), "a built-in is not an MCP server")
            import acp_server
            handler = server._sessions[sid].handler
            ask = lambda action: handler.before_tool_call(SimpleNamespace(tool_use={"name": "mcp_resources", "input": {"server": "echo", "action": action, "uri": "notes://readme"}, "toolUseId": f"r-{action}"}, agent=None))  # noqa: E731
            check(type(ask("list")).__name__ == "Proceed" and type(ask("read")).__name__ == "Confirm", "listing never asks; reading is a call into the server and asks")
        else:
            check("Declared but not started until the person runs /trust: echo" in MODELS["orchestrator"].calls[0]["last_text"], "untrusted: the model is told the server waits for /trust")
            check("echo" not in tools, "…and nothing of it runs")
        check(lsp_ok, f"the built-in LSP tools are there either way ({'trusted' if trusted else 'untrusted'})")
        check("MCP servers connected" not in json.dumps(server._sessions[sid].agent.messages) and "Declared but not started" not in json.dumps(server._sessions[sid].agent.messages), "the capabilities line is never stored in the conversation")
        host.shutdown()
        del seen


def test_profiler() -> None:
    cwd = project("profile")
    (cwd / "src").mkdir()
    for i in range(5):
        (cwd / "src" / f"m{i}.py").write_text("x = 1\n" * 50)
    (cwd / "requirements.txt").write_text("requests==2.0\n# comment\nhttpx\n-e .\n")
    (cwd / "Cargo.toml").write_text('[workspace]\nmembers = ["a", "b", "c"]\n[workspace.dependencies]\nserde = "1"\n')
    (cwd / "package.json").write_text(json.dumps({"dependencies": {"react": "1"}, "devDependencies": {"vite": "1", "ts": "1"}}))
    subprocess.run(["git", "init", "-q"], cwd=cwd, check=True)
    subprocess.run(["git", "add", "-A"], cwd=cwd, check=True)
    found = env_profile.profile(cwd, fresh=True)
    check(found.files == 9 and found.languages[0] == ("Python", 5), f"tracked files and languages ({found.files}, {found.languages[:2]})")
    check(found.manifests == {"Cargo.toml": 1, "package.json": 3, "requirements.txt": 2} and found.workspaces == 3, f"dependencies counted from the manifests, nothing run ({found.manifests})")
    check(found.size_class == "small", "a tiny three-package workspace is still small (it takes size too to be a monorepo)")
    check(env_profile.classify(5000, 10_000, 0) == "large" and env_profile.classify(800, 60_000, 4) == "large" and env_profile.classify(800, 60_000, 0) == "medium", "the size classes")
    check(found.memory_gb and found.memory_gb > 1 and found.free_gb is not None, f"unified memory is read ({found.memory_gb:.0f} GB)")
    line = found.line()
    check(line.startswith("Environment: small project — 9 files") and "requirements.txt (2)" in line, f"one line for the workspace context: {line[:90]}…")
    os.chdir(cwd)
    report = env_profile.environment_profile()
    check("Toolchains:" in report and ("git version" in report or "none of" in report), "the tool adds toolchain versions")


def test_adaptive_effort() -> None:
    choose = adaptive_effort.choose
    check(choose("low", "fast", "large") == "low", "trivial work stays at the role's level, even in a monorepo")
    check(choose("low", "strong", "large") == "high" and choose("low", "strong", "small") == "medium", "hard work gets more, more in a bigger project")
    check(choose("low", None, "large") == "medium" and choose("low", None, "small") == "low", "no reading: a monorepo still gets a little more")
    check(choose("high", "standard", "small") == "high", "never below what the person chose (the Lab's dial, an env override)")
    check(all(choose(base, c, size) != "max" for base in ("low", "medium", "high") for c in ("fast", "standard", "strong", None) for size in ("small", "medium", "large")), "never max unless the person chose it")
    check(choose("max", "fast", "small") == "max", "…and kept when they did")

    control = ollama_cloud.ReasoningControl("low")
    model = SimpleNamespace(reasoning=control)
    agent = SimpleNamespace(model=model)
    cwd = project("effort")
    tier = {"value": "strong"}
    announced: list[dict] = []
    hook = adaptive_effort.AdaptiveEffort(str(cwd), complexity=lambda a: tier["value"], enabled=lambda: True, notify=announced.append)
    real_size = env_profile.size_class
    env_profile.size_class = lambda root: "large"
    ollama_cloud.ROLES.set("orchestrator", effort="low")  # the role table is the floor; the product default is medium now
    try:
        hook._before_model(SimpleNamespace(agent=agent))
        check(control.level == "high" and announced[-1]["effort"] == "high", "the hook sets the request's reasoning_effort before the call")
        tier["value"] = "fast"
        hook._before_model(SimpleNamespace(agent=agent))
        check(control.level == "low", "…and lowers it back for the next, simpler call")
        ollama_cloud.ROLES.set("orchestrator", effort="medium")
        hook._before_model(SimpleNamespace(agent=agent))
        check(control.level == "medium", "the Lab's dial (the role table) is the floor")
        disabled = adaptive_effort.AdaptiveEffort(str(cwd), complexity=lambda a: "strong", enabled=lambda: False)
        control.set("low")
        disabled._before_model(SimpleNamespace(agent=agent))
        check(control.level == "low", "reasoning.adaptive: false leaves it alone")
        adaptive_effort.AdaptiveEffort(str(cwd), complexity=lambda a: "strong", enabled=lambda: True)._before_model(SimpleNamespace(agent=SimpleNamespace(model=object())))
        check(True, "a model without the knob is left alone (no error)")
    finally:
        env_profile.size_class = real_size
        ollama_cloud.ROLES.set("orchestrator", effort="low")


async def main() -> None:
    test_discovery()
    await test_capabilities_through_acp()
    test_profiler()
    test_adaptive_effort()
    print("\nenvironment awareness: all checks passed")


if __name__ == "__main__":
    asyncio.run(main())

"""Zero-config MCP servers and skills (registry.py). Hermetic: a throwaway CORDATA_HOME, the echo MCP fixture, local
skill folders, no network.

* one token → an ``mcpServers`` entry: catalog ids, ``@scope/pkg``/``npx:``, ``uvx:``, ``https://``; plain remote http
  and junk refused; ``--env`` written, redacted when listed; nothing is ever pre-approved;
* ``add`` writes the user file atomically; ``--project`` writes the project's, where PluginHost keeps it **blocked** in an
  untrusted folder (zero processes, zero tools);
* a real stdio server added to the user file is discovered and enabled in a live host, its tools registered into an
  attached agent **exactly once**, then removed;
* skills: frontmatter checked, the AST gate refuses a bundled script that shells out, a shipped name is refused, an
  install is listed and hot-reloaded into a live ``SemanticSkills`` in one swap, a remove reloads again;
* the CLI: ``cordata mcp add|list|remove`` and ``cordata skill add|list|remove`` exit codes and output.

    backend/.venv/bin/python _registry_mcp_skills_smoke_test.py
"""

from __future__ import annotations

import contextlib
import io
import json
import os
import sys
import tempfile
from pathlib import Path
from types import SimpleNamespace

_HOME = tempfile.mkdtemp(prefix="registry-home-")
os.environ["CORDATA_HOME"] = _HOME

import plugin_host  # noqa: E402
import registry  # noqa: E402
import skill_search  # noqa: E402
from harness_testing import check  # noqa: E402

ECHO = Path(__file__).resolve().parent / "fixtures" / "echo_mcp_server.py"


def test_resolve() -> None:
    print("\n=== one token in ===")
    name, config, warnings = registry.resolve("memory")
    check(name == "memory" and config["args"][-1] == "@modelcontextprotocol/server-memory", "a catalog id")
    name, config, _ = registry.resolve("@acme/mcp-server-widgets")
    check(name == "widgets" and config == {"command": "npx", "args": ["-y", "@acme/mcp-server-widgets"]}, f"@scope/pkg → npx -y ({name})")
    check(registry.resolve("npx:some-tool")[1]["args"] == ["-y", "some-tool"], "npx:pkg")
    name, config, _ = registry.resolve("uvx:mcp-server-git")
    check(name == "git" and config == {"command": "uvx", "args": ["mcp-server-git"]}, "uvx:pkg → uvx")
    name, config, _ = registry.resolve("https://mcp.example.com/mcp")
    check(config == {"url": "https://mcp.example.com/mcp"} and name == "example", f"an https URL → streamable HTTP ({name})")
    check(registry.resolve("http://localhost:3000/mcp")[1]["url"].startswith("http://localhost"), "plain http only for localhost")
    for bad in ("http://evil.example/mcp", "rm -rf /", "", "@x/y; touch z"):
        try:
            registry.resolve(bad)
            ok = bad == "@x/y; touch z"  # a package token with spaces becomes separate npx args, never a shell
        except registry.RegistryError:
            ok = True
        check(ok, f"refused or made harmless: {bad!r}")
    check(registry.resolve("@x/y; touch z")[1]["args"] == ["-y", "@x/y;", "touch", "z"], "…an argument list, not a shell line")
    _, _, warnings = registry.resolve("github")
    check(any("GITHUB_TOKEN" in w for w in warnings), "a server's missing secret is named, not guessed")
    _, config, _ = registry.resolve("uvx:thing", env={"API_KEY": "s3cret"})
    check(config["env"] == {"API_KEY": "s3cret"} and registry._redacted(config)["env"] == {"API_KEY": "••••"}, "--env is written and redacted when shown")


def test_scopes_and_trust() -> None:
    print("\n=== where it lands, and trust ===")
    project = Path(tempfile.mkdtemp(prefix="registry-proj-"))
    added = registry.mcp_add("memory", project=str(project))
    user_file = Path(_HOME) / "mcp.json"
    check(added["path"] == str(user_file) and json.loads(user_file.read_text())["mcpServers"]["memory"]["command"] == "npx", "the user file by default")
    check("approval" not in json.loads(user_file.read_text())["mcpServers"]["memory"], "nothing is pre-approved")
    check(user_file.stat().st_mode & 0o777 == 0o600, "the file is 0600: an entry can hold a server's credentials")
    registry.mcp_add(f"uvx:{ECHO}", project=str(project), scope="project", name="echoproj")
    project_file = project / ".cordata" / "mcp.json"
    check(project_file.exists() and "echoproj" in json.loads(project_file.read_text())["mcpServers"], "--project writes the project's file")
    host = plugin_host.PluginHost(str(project))
    host.discover()
    row = host.enable("echoproj")
    check(not row["ok"] and row.get("status") == "blocked" and not host.tool_names("echoproj"), "in an untrusted folder a project server is blocked: no process, no tools")
    check(registry.mcp_remove("echoproj", scope="project", project=str(project))["ok"] and not registry.mcp_remove("nope")["ok"], "remove says whether there was one")
    listed = registry.mcp_list(str(project))
    check([r["name"] for r in listed] == ["memory"], f"list shows what is installed ({listed})")
    registry.mcp_remove("memory")


def test_live_attach() -> None:
    print("\n=== live, no restart ===")
    project = Path(tempfile.mkdtemp(prefix="registry-live-"))
    host = plugin_host.PluginHost(str(project))
    host.discover()

    class Registry:
        def __init__(self) -> None:
            self.registered: list[str] = []
            self.registry: dict[str, object] = {}
            self.dynamic_tools: dict[str, object] = {}

        def process_tools(self, tools) -> None:  # what PluginHost._register calls
            for tool in tools:
                self.registered.append(tool.tool_name)
                self.registry[tool.tool_name] = tool

    agent = SimpleNamespace(tool_registry=Registry())
    host.attach(agent)
    added = registry.mcp_add(f"npx:{ECHO}", name="echo")  # resolved to a command; rewritten to run the fixture below
    data = json.loads((Path(_HOME) / "mcp.json").read_text())
    data["mcpServers"]["echo"] = {"command": sys.executable, "args": [str(ECHO)]}
    (Path(_HOME) / "mcp.json").write_text(json.dumps(data))
    host.discover()
    row = host.enable(added["name"])
    tools = host.tool_names("echo")
    check(row["ok"] and tools, f"discover + enable starts the new server in a running host ({row.get('error')})")
    check(all(agent.tool_registry.registered.count(t) == 1 for t in tools), f"…and registers each tool into the attached agent exactly once ({agent.tool_registry.registered})")
    check(host.enable("echo")["ok"] and all(agent.tool_registry.registered.count(t) == 1 for t in tools), "enabling again registers nothing twice")
    check(host.requires_approval(tools[0]) is not None, "its tools still ask before running")
    host.shutdown()
    registry.mcp_remove("echo")


def make_skill(root: Path, name: str, *, description: str = "Does a thing.", script: str | None = None) -> Path:
    folder = root / name
    folder.mkdir(parents=True)
    (folder / "SKILL.md").write_text(f"---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\nSteps.\n")
    if script is not None:
        (folder / "tool.py").write_text(script)
    return folder


def test_skills() -> None:
    print("\n=== skills ===")
    src = Path(tempfile.mkdtemp(prefix="registry-skills-"))
    good = make_skill(src, "release-notes", script="import json\nprint(json.dumps({}))\n")
    evil = make_skill(src, "sneaky", script="import subprocess\nsubprocess.run(['curl', 'x'])\n")
    nameless = src / "nameless"
    nameless.mkdir()
    (nameless / "SKILL.md").write_text("# no frontmatter\n")
    shipped = sorted(registry.shipped_skill_names())[0]
    clash = make_skill(src / "clash", shipped)

    plugins = skill_search.build_skill_plugins([str(registry.user_skills_dir())], index=None)
    semantic, agent_skills = plugins[0], plugins[1]
    check("release-notes" not in semantic.index.skills, "a live session starts without it")
    added = registry.skill_add(str(good))
    check(added["ok"] and (registry.user_skills_dir() / "release-notes" / "SKILL.md").exists(), "a folder installs into ~/.cordata/skills")
    check(added["sessions_refreshed"] >= 1 and "release-notes" in semantic.index.skills, "…and every live session reloads its library")
    check("release-notes" in {s.name for s in agent_skills.get_available_skills()}, "…AgentSkills' list swapped with it")
    for bad, why in ((evil, "AST gate"), (nameless, "frontmatter"), (clash, "shipped")):
        try:
            registry.skill_add(str(bad))
            check(False, f"refused: {why}")
        except registry.RegistryError as error:
            check(True, f"refused ({why}): {str(error)[:70]}")
    check(not (registry.user_skills_dir() / "sneaky").exists(), "a refused skill leaves nothing behind")
    check([r["name"] for r in registry.skill_list()] == ["release-notes"], "list shows it")
    removed = registry.skill_remove("release-notes")
    check(removed["ok"] and "release-notes" not in semantic.index.skills, "remove deletes it and reloads")
    check(not registry.skill_remove("../etc")["ok"], "a remove only takes a skill name")
    project = Path(tempfile.mkdtemp(prefix="registry-skill-proj-"))
    registry.skill_add(str(good), scope="project", project=str(project), reload=False)
    check(registry.project_skill_dirs(project, trusted=False) == [] and registry.project_skill_dirs(project, trusted=True), "a project's skills are read only once it is trusted")


def cli(*argv: str) -> tuple[int, str]:
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = registry.main(list(argv))
    return code, out.getvalue() + err.getvalue()


def test_cli() -> None:
    print("\n=== the CLI ===")
    code, text = cli("mcp", "catalog")
    check(code == 0 and "sequential-thinking" in text and "GITHUB_TOKEN" in text, "mcp catalog")
    code, text = cli("mcp", "add", "uvx:mcp-server-git")
    check(code == 0 and "added mcp git (user)" in text, f"mcp add ({text.strip()[:60]})")
    code, text = cli("mcp", "list")
    check(code == 0 and "[user] git" in text, "mcp list")
    code, text = cli("mcp", "add", "not a thing")
    check(code == 2 and "not a catalog id" in text, "a bad token exits 2 and says why")
    check(cli("mcp", "remove", "git")[0] == 0 and cli("mcp", "remove", "git")[0] == 1, "mcp remove, then again")
    code, text = cli("skill", "catalog")
    check(code == 0 and "anthropic-pdf" in text, "skill catalog")
    src = Path(tempfile.mkdtemp(prefix="registry-cli-"))
    code, text = cli("skill", "add", str(make_skill(src, "cli-skill") / "SKILL.md"))
    check(code == 0 and "added skill cli-skill" in text, "skill add takes a SKILL.md path too")
    check(cli("skill", "remove", "cli-skill")[0] == 0, "skill remove")


def main() -> int:
    test_resolve()
    test_scopes_and_trust()
    test_live_attach()
    test_skills()
    test_cli()
    print("\nregistry: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())

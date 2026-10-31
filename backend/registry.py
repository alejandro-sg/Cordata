"""Zero-config MCP servers and skills: one token in, a working tool or skill out — no JSON to write, no restart.

    cordata mcp catalog                         cordata skill catalog
    cordata mcp list                            cordata skill list
    cordata mcp add memory                      cordata skill add ./my-skill            (a folder with a SKILL.md)
    cordata mcp add @scope/some-mcp-server      cordata skill add https://github.com/o/r.git#skills/x
    cordata mcp add uvx:mcp-server-git          cordata skill add <catalog id> [--project]
    cordata mcp add https://host/mcp            cordata skill remove <name>
    cordata mcp add github --env GITHUB_TOKEN=… [--project] [--name N]
    cordata mcp remove <name> [--project]

**MCP.** ``resolve`` turns a token into an ``mcpServers`` entry: a catalog id; ``@scope/pkg`` or ``npx:pkg`` →
``npx -y pkg``; ``uvx:pkg`` → ``uvx pkg``; an ``http(s)://`` URL → streamable HTTP. ``add`` writes it atomically
(``mcp_manager``) to ``~/.cordata/mcp.json`` by default — the user's own file, so it starts at once — or with
``--project`` to ``<project>/.cordata/mcp.json``, where ``plugin_host`` keeps it **blocked until the folder is
trusted**: the registry never trusts anything, and never marks a server ``approval: allow``. Its tools ask before every
call (``PluginHost.requires_approval``) and their output is fenced as untrusted (``UntrustedToolResultHooks``). A
running session picks a new server up through ``_cordata/plugins/install`` (acp_server.py): ``discover()`` then
``enable(name)``, which registers the tools into every attached agent exactly once.

**Skills.** A source is a folder (or its ``SKILL.md``), a git URL (``#subdir`` picks one skill in a repository) or a
catalog id. The frontmatter is checked (``name``, ``description``), every bundled ``.py`` goes through the same AST gate
as a generated tool (``skill_validation_hooks.find_banned_usage``), a name a shipped skill already has is refused, and
the folder is copied to ``~/.cordata/skills/<name>`` (``--project``: ``<project>/.cordata/skills``, read only once the
folder is trusted). Live sessions reload their library in one atomic swap (``skill_search.reload_all``).
"""

from __future__ import annotations

import argparse
import ast
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import mcp_manager

BACKEND_DIR = Path(__file__).resolve().parent
_NAME = re.compile(r"^[A-Za-z0-9_.-]{1,48}$")


@dataclass(frozen=True)
class McpEntry:
    id: str
    description: str
    config: dict[str, Any]
    #: Environment variables the server needs, with what each is.
    needs: tuple[tuple[str, str], ...] = ()


MCP_CATALOG: dict[str, McpEntry] = {
    e.id: e
    for e in (
        McpEntry("filesystem", "Read and write files under a folder you name", {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."]}),
        McpEntry("fetch", "Fetch a web page as markdown", {"command": "uvx", "args": ["mcp-server-fetch"]}),
        McpEntry("git", "Read a git repository: log, diff, blame, branches", {"command": "uvx", "args": ["mcp-server-git"]}),
        McpEntry("github", "GitHub issues, pull requests and code search (GitHub's hosted server)", {"url": "https://api.githubcopilot.com/mcp/", "headers": {"Authorization": "Bearer ${GITHUB_TOKEN}"}}, (("GITHUB_TOKEN", "a GitHub personal access token"),)),
        McpEntry("memory", "A persistent knowledge-graph memory", {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-memory"]}),
        McpEntry("sqlite", "Query a SQLite database", {"command": "uvx", "args": ["mcp-server-sqlite", "--db-path", "${SQLITE_DB}"]}, (("SQLITE_DB", "the path of the database file"),)),
        McpEntry("postgres", "Read-only queries against a Postgres database", {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-postgres", "${POSTGRES_URL}"]}, (("POSTGRES_URL", "postgres://user:pass@host/db"),)),
        McpEntry("sequential-thinking", "Structured step-by-step reasoning scratchpad", {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-sequential-thinking"]}),
        McpEntry("puppeteer", "Drive a headless browser: navigate, click, screenshot", {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-puppeteer"]}),
        McpEntry("linear", "Linear issues and projects (Linear's hosted server; signs in on first use)", {"url": "https://mcp.linear.app/mcp"}),
    )
}

#: Curated skill packs: (git URL, sub-folder, what it is).
SKILL_CATALOG: dict[str, tuple[str, str, str]] = {
    "anthropic-pdf": ("https://github.com/anthropics/skills.git", "skills/pdf", "Read, fill and build PDF files"),
    "anthropic-docx": ("https://github.com/anthropics/skills.git", "skills/docx", "Create and edit Word documents"),
    "anthropic-xlsx": ("https://github.com/anthropics/skills.git", "skills/xlsx", "Spreadsheets with formulas and charts"),
    "anthropic-mcp-builder": ("https://github.com/anthropics/skills.git", "skills/mcp-builder", "Build an MCP server, step by step"),
    "anthropic-webapp-testing": ("https://github.com/anthropics/skills.git", "skills/webapp-testing", "Test a local web app with Playwright"),
}


class RegistryError(ValueError):
    pass


# ------------------------------------------------------------------------------------------------------------------ MCP


def _name_from_package(package: str) -> str:
    base = package.split("/")[-1].split("@")[0]
    for prefix in ("mcp-server-", "server-", "mcp-"):
        if base.startswith(prefix) and len(base) > len(prefix):
            base = base[len(prefix) :]
            break
    base = re.sub(r"[^A-Za-z0-9_.-]", "-", base.removesuffix("-mcp").removesuffix("-mcp-server")).strip("-.")
    return base[:48] or "server"


def resolve(spec: str, *, env: dict[str, str] | None = None, name: str | None = None) -> tuple[str, dict[str, Any], list[str]]:
    """``(name, mcpServers entry, warnings)`` for one token."""
    text = spec.strip()
    if not text:
        raise RegistryError("nothing to add")
    warnings: list[str] = []
    if text in MCP_CATALOG:
        entry = MCP_CATALOG[text]
        config = json.loads(json.dumps(entry.config))
        server = name or entry.id
        missing = [key for key, _ in entry.needs if not (env or {}).get(key) and not os.environ.get(key)]
        for key, what in entry.needs:
            if key in missing:
                warnings.append(f"{server} needs {key} ({what}): add it with --env {key}=… or export it before starting Cordata")
        config["description"] = entry.description
    elif re.match(r"^https?://", text):
        if text.startswith("http://") and not re.match(r"^http://(localhost|127\.0\.0\.1|\[::1\])(:|/|$)", text):
            raise RegistryError("a remote MCP server must use https:// (plain http is accepted only for localhost)")
        config = {"url": text}
        host = re.sub(r"^https?://", "", text).split("/")[0].split(":")[0]
        server = name or _name_from_package(host.replace("mcp.", "").split(".")[0] or "remote")
    elif text.startswith("uvx:"):
        package = text[4:].strip()
        if not package:
            raise RegistryError("uvx: needs a package name")
        config = {"command": "uvx", "args": package.split()}
        server = name or _name_from_package(package.split()[0])
    elif text.startswith("npx:") or text.startswith("@"):
        package = text.removeprefix("npx:").strip()
        if not package:
            raise RegistryError("npx: needs a package name")
        config = {"command": "npx", "args": ["-y", *package.split()]}
        server = name or _name_from_package(package.split()[0])
    else:
        raise RegistryError(f"{spec!r} is not a catalog id ({', '.join(MCP_CATALOG)}), an @scope/package, npx:pkg, uvx:pkg or an https:// URL")
    if env:
        bad = [key for key in env if not re.match(r"^[A-Z_][A-Z0-9_]*$", key)]
        if bad:
            raise RegistryError(f"not an environment variable name: {', '.join(bad)}")
        if "url" in config:
            warnings.append("--env is passed to a local server's process; a URL server reads ${VAR} from Cordata's environment instead")
        else:
            config["env"] = {**config.get("env", {}), **env}
    if not _NAME.match(server):
        raise RegistryError(f"{server!r} is not a usable server name (letters, digits, . _ -; at most 48)")
    return server, config, warnings


def mcp_add(spec: str, *, scope: str = "user", project: str | None = None, env: dict[str, str] | None = None, name: str | None = None) -> dict[str, Any]:
    if scope not in ("user", "project"):
        raise RegistryError("scope is user or project")
    server, config, warnings = resolve(spec, env=env, name=name)
    config.pop("approval", None)  # never pre-approve: every call asks until a person says otherwise
    path = mcp_manager.add_server(server, config, scope=scope, project=project)
    if scope == "project":
        warnings.append("project servers start only once the folder is trusted (/trust in a session)")
    return {"ok": True, "name": server, "scope": scope, "path": str(path), "config": _redacted(config), "warnings": warnings}


def mcp_remove(name: str, *, scope: str = "user", project: str | None = None) -> dict[str, Any]:
    existed = mcp_manager.remove_server(name, scope=scope, project=project)
    return {"ok": existed, "name": name, "scope": scope, **({} if existed else {"reason": f"no server {name!r} in the {scope} file"})}


def mcp_list(project: str | None = None) -> list[dict[str, Any]]:
    rows = []
    for scope in ("user", "project"):
        for row in mcp_manager.list_servers(scope, project):
            rows.append({"scope": scope, "name": row.get("name"), "config": _redacted({k: v for k, v in row.items() if k != "name"})})
    return rows


def _redacted(config: dict[str, Any]) -> dict[str, Any]:
    out = json.loads(json.dumps(config))
    for key in ("env", "headers"):
        if isinstance(out.get(key), dict):
            out[key] = {k: (v if str(v).startswith("${") else "••••") for k, v in out[key].items()}
    return out


def mcp_catalog() -> list[dict[str, Any]]:
    return [{"id": e.id, "description": e.description, "needs": [k for k, _ in e.needs], "transport": "streamable-http" if "url" in e.config else "stdio"} for e in MCP_CATALOG.values()]


# ---------------------------------------------------------------------------------------------------------------- skills


def user_skills_dir() -> Path:
    return Path(os.environ.get("CORDATA_HOME") or Path.home() / ".cordata").expanduser() / "skills"


def project_skill_dirs(cwd: str | os.PathLike, *, trusted: bool) -> list[str]:
    """A project's installed skills, offered to its sessions only once the folder is trusted."""
    folder = Path(cwd) / ".cordata" / "skills"
    return [str(folder)] if trusted and folder.is_dir() else []


def _frontmatter(text: str) -> dict[str, str]:
    match = re.match(r"^---\s*\n(.*?)\n---\s*(\n|$)", text, re.S)
    if not match:
        raise RegistryError("SKILL.md has no frontmatter (a --- block with name and description)")
    fields: dict[str, str] = {}
    for line in match.group(1).splitlines():
        key, sep, value = line.partition(":")
        if sep and key.strip() and not line.startswith((" ", "\t")):
            fields[key.strip()] = value.strip().strip("'\"")
    return fields


def validate_skill(folder: Path) -> tuple[str, str]:
    """``(name, description)`` of a skill folder, or ``RegistryError`` saying what is wrong with it."""
    from skill_validation_hooks import find_banned_usage

    manifest = folder / "SKILL.md"
    if not manifest.is_file():
        raise RegistryError(f"{folder} has no SKILL.md")
    fields = _frontmatter(manifest.read_text(encoding="utf-8", errors="replace"))
    name, description = fields.get("name", ""), fields.get("description", "")
    if not re.match(r"^[a-z0-9][a-z0-9-]{0,63}$", name):
        raise RegistryError(f"skill name {name!r} must be lowercase letters, digits and hyphens")
    if not description or len(description) > 1024:
        raise RegistryError("the skill needs a description of at most 1024 characters")
    for script in folder.rglob("*.py"):
        try:
            tree = ast.parse(script.read_text(encoding="utf-8", errors="replace"))
        except SyntaxError as error:
            raise RegistryError(f"{script.relative_to(folder)} does not parse: {error.msg}") from error
        banned = find_banned_usage(tree)
        if banned:
            raise RegistryError(f"{script.relative_to(folder)} is refused by the AST gate: {banned}")
    for path in folder.rglob("*"):
        if path.is_symlink():
            raise RegistryError(f"{path.relative_to(folder)} is a symlink; a skill must be plain files")
    return name, description


def _fetch_git(url: str, subdir: str, into: Path) -> Path:
    if not re.match(r"^(https://|git@)[\w.@:/~+-]+?(\.git)?$", url):
        raise RegistryError(f"{url!r} is not a git URL this accepts (https:// or git@)")
    done = subprocess.run(["git", "clone", "--depth", "1", "--quiet", "--", url, str(into)], capture_output=True, text=True, timeout=180)
    if done.returncode != 0:
        raise RegistryError(f"git clone failed: {(done.stderr or done.stdout).strip()[:300]}")
    target = (into / subdir).resolve() if subdir else into
    if into.resolve() not in (target, *target.parents):
        raise RegistryError("the sub-folder leaves the repository")
    return target


def shipped_skill_names() -> set[str]:
    import skill_search

    return {skill.name for skill in skill_search.load_skills([BACKEND_DIR / "skills"])}


def skill_add(source: str, *, scope: str = "user", project: str | None = None, reload: bool = True) -> dict[str, Any]:
    text = source.strip()
    with tempfile.TemporaryDirectory(prefix="cordata-skill-") as tmp:
        if text in SKILL_CATALOG:
            url, subdir, _ = SKILL_CATALOG[text]
            folder = _fetch_git(url, subdir, Path(tmp) / "repo")
        elif re.match(r"^(https://|git@)", text):
            url, _, subdir = text.partition("#")
            folder = _fetch_git(url, subdir.strip("/"), Path(tmp) / "repo")
        else:
            folder = Path(text).expanduser().resolve()
            if folder.name == "SKILL.md":
                folder = folder.parent
        if not folder.is_dir():
            raise RegistryError(f"{source!r} is not a skill folder, a git URL or a catalog id ({', '.join(SKILL_CATALOG)})")
        name, description = validate_skill(folder)
        if name in shipped_skill_names():
            raise RegistryError(f"a shipped skill is already called {name!r}; rename yours (the shipped one would win anyway)")
        base = user_skills_dir() if scope == "user" else Path(project or os.getcwd()).resolve() / ".cordata" / "skills"
        target = base / name
        base.mkdir(parents=True, exist_ok=True)
        staging = base / f".{name}.installing"
        shutil.rmtree(staging, ignore_errors=True)
        shutil.copytree(folder, staging, ignore=shutil.ignore_patterns(".git", "__pycache__", "node_modules"))
        if target.exists():
            shutil.rmtree(target)
        os.replace(staging, target)
    refreshed = _reload() if reload else 0
    return {"ok": True, "name": name, "description": description, "scope": scope, "path": str(target), "sessions_refreshed": refreshed,
            **({"warnings": ["project skills are read only once the folder is trusted"]} if scope == "project" else {})}


def skill_remove(name: str, *, scope: str = "user", project: str | None = None) -> dict[str, Any]:
    if not re.match(r"^[a-z0-9][a-z0-9-]{0,63}$", name):
        return {"ok": False, "reason": f"{name!r} is not a skill name"}
    base = user_skills_dir() if scope == "user" else Path(project or os.getcwd()).resolve() / ".cordata" / "skills"
    target = base / name
    if not (target / "SKILL.md").is_file():
        return {"ok": False, "reason": f"no installed skill {name!r} in the {scope} folder"}
    shutil.rmtree(target)
    return {"ok": True, "name": name, "scope": scope, "sessions_refreshed": _reload()}


def skill_list(project: str | None = None) -> list[dict[str, Any]]:
    import skill_search

    rows = []
    for scope, folder in (("user", user_skills_dir()), ("project", Path(project or os.getcwd()) / ".cordata" / "skills")):
        if folder.is_dir():
            for skill in skill_search.load_skills([folder]):
                rows.append({"scope": scope, "name": skill.name, "description": skill.description})
    return rows


def skill_catalog() -> list[dict[str, Any]]:
    return [
        {"id": key, "name": subdir.rsplit("/", 1)[-1] or key, "description": what, "source": f"{url}#{subdir}"}
        for key, (url, subdir, what) in SKILL_CATALOG.items()
    ]


def _reload() -> int:
    try:
        import skill_search

        # A session that started before the first install had no user folder to read: give it one now.
        folder = str(user_skills_dir())
        if user_skills_dir().is_dir():
            for plugin in list(skill_search.LIVE):
                if folder not in plugin.sources:
                    plugin.sources.append(folder)
        return skill_search.reload_all()
    except Exception:
        return 0


# ------------------------------------------------------------------------------------------------------------------ CLI


def _env_pairs(values: list[str]) -> dict[str, str]:
    out = {}
    for item in values:
        key, sep, value = item.partition("=")
        if not sep:
            raise RegistryError(f"--env takes KEY=VALUE, got {item!r}")
        out[key.strip()] = value
    return out


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="cordata", description="Add MCP servers and skills without editing JSON.")
    kinds = parser.add_subparsers(dest="kind", required=True)
    for kind in ("mcp", "skill"):
        sub = kinds.add_parser(kind).add_subparsers(dest="action", required=True)
        sub.add_parser("catalog")
        sub.add_parser("list").add_argument("--cwd", default=os.getcwd())
        add = sub.add_parser("add")
        add.add_argument("spec")
        add.add_argument("--project", action="store_true", help="the project's .cordata/ (used once the folder is trusted)")
        add.add_argument("--cwd", default=os.getcwd())
        if kind == "mcp":
            add.add_argument("--env", action="append", default=[], metavar="KEY=VALUE")
            add.add_argument("--name")
        rm = sub.add_parser("remove")
        rm.add_argument("name")
        rm.add_argument("--project", action="store_true")
        rm.add_argument("--cwd", default=os.getcwd())
    args = parser.parse_args(argv)
    try:
        if args.action == "catalog":
            rows = mcp_catalog() if args.kind == "mcp" else skill_catalog()
            for row in rows:
                extra = f"  (needs {', '.join(row['needs'])})" if row.get("needs") else ""
                print(f"  {row['id']:<22} {row['description']}{extra}")
            return 0
        if args.action == "list":
            rows = mcp_list(args.cwd) if args.kind == "mcp" else skill_list(args.cwd)
            for row in rows:
                print(f"  [{row['scope']}] {row['name']}  {row.get('description') or json.dumps(row.get('config'))}")
            if not rows:
                print("  (none installed)")
            return 0
        scope = "project" if args.project else "user"
        if args.action == "add":
            result = mcp_add(args.spec, scope=scope, project=args.cwd, env=_env_pairs(args.env), name=args.name) if args.kind == "mcp" else skill_add(args.spec, scope=scope, project=args.cwd)
            print(f"added {args.kind} {result['name']} ({result['scope']}) → {result['path']}")
            if args.kind == "mcp":
                print("  running sessions pick it up with /plugins reload; new sessions have it at once")
            for warning in result.get("warnings", []):
                print(f"  note: {warning}")
            return 0
        result = mcp_remove(args.name, scope=scope, project=args.cwd) if args.kind == "mcp" else skill_remove(args.name, scope=scope, project=args.cwd)
        print(f"removed {args.kind} {args.name}" if result["ok"] else result["reason"])
        return 0 if result["ok"] else 1
    except RegistryError as error:
        print(f"cordata {args.kind}: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())

"""Semantic code navigation: search instead of reading, outlines instead of whole files.

* **code_search** — both engines (ripgrep when installed, Python otherwise) return the same ``path:line: text`` answer,
  sorted, capped and honest about the cap; regex and literal, globs, context, ``files_only``; never outside the
  project; never a secrets file; ignored files skipped (git checkout).
* **code_outline** — Python by ``ast`` (exact ranges, dotted methods, decorators included), Rust / TypeScript /
  Markdown by structure; ``symbol`` returns just that source; an ambiguous or unknown name says what *is* there.
* **LargeFileHooks** — through the real ACP server: a whole-file ``file_read`` of an 800-line module comes back as its
  outline (and the offloader never sees the file), a line-range read is untouched, a short file is untouched.
* **Gate 1** — "find references to build_widget" through the real server: the agent's tools for it are ``code_search``
  and the LSP tools, the answer is three lines, and nothing reads a whole file. The scripted model stands in for the
  choice; the system prompt the real model gets is checked to steer it that way.

    backend/.venv/bin/python _code_nav_smoke_test.py
"""

import asyncio
import os
import shutil
import subprocess
from pathlib import Path

os.environ["CORDATA_PRISM"] = "off"

import provider_settings  # noqa: F401  (before agent_common, which chdirs)

import acp  # noqa: E402

import code_nav  # noqa: E402
import code_search  # noqa: E402
from _acp_harness_smoke_test import ROOT, call, fresh_server, laya_answer, orchestrator_model, settle, text, MODELS  # noqa: E402
from harness_testing import check  # noqa: E402

ROUTE = laya_answer("code", {"code": 0.97, "answer": 0.03})


def make_repo(name: str) -> Path:
    repo = ROOT / name
    (repo / "pkg").mkdir(parents=True)
    (repo / "pkg" / "widgets.py").write_text(
        "import functools\n\n\n"
        "@functools.cache\n"
        "def build_widget(size: int) -> dict:\n"
        "    return {'size': size}\n\n\n"
        "class Factory:\n"
        "    def make(self):\n"
        "        return build_widget(3)\n\n"
        "    async def make_async(self):\n"
        "        return build_widget(4)\n"
    )
    (repo / "app.py").write_text("from pkg.widgets import build_widget\n\n\nW = build_widget(1)\n")
    (repo / "notes-2-final.md").write_text("# Notes\n\nbuild_widget is documented here.\n\n## Usage\n\ncall it\n")
    (repo / ".env").write_text("TOKEN=build_widget-secret\n")
    (repo / "ignored").mkdir()
    (repo / "ignored" / "junk.py").write_text("build_widget()\n")
    (repo / ".gitignore").write_text("ignored/\n")
    (repo / "lib.rs").write_text(
        "pub struct Inbox {\n    n: u32,\n}\n\n"
        "impl Inbox {\n    pub fn push(&mut self) {\n        self.n += 1; // {\n    }\n\n    fn trim(&self) -> u32 {\n        self.n\n    }\n}\n\n"
        "pub fn merge(a: u32) -> u32 {\n    let s = \"}\";\n    a\n}\n\n"
        "#[cfg(test)]\nmod tests {\n    fn helper() {}\n}\n"
    )
    (repo / "ui.ts").write_text(
        "export class Panel {\n  render(): string {\n    return 'x';\n  }\n}\n\nexport function mount(el: string) {\n  return el;\n}\n\n"
        "export const unmount = async (el: string) => {\n  return el;\n};\n\ninterface Props {\n  a: number;\n}\n"
    )
    subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
    return repo


def test_search(repo: Path) -> None:
    engines = ["python"] + (["ripgrep"] if shutil.which("rg") else [])
    answers = {}
    for engine in engines:
        os.environ["CORDATA_RG"] = "on" if engine == "ripgrep" else "off"
        found = code_search.search(r"\bbuild_widget\b", root=repo)
        answers[engine] = [(m.path, m.line) for m in found.matches]
        check(found.engine == engine, f"{engine}: the engine says which it is")
        check(answers[engine] == [("app.py", 1), ("app.py", 4), ("notes-2-final.md", 3), ("pkg/widgets.py", 5), ("pkg/widgets.py", 11), ("pkg/widgets.py", 14)], f"{engine}: every match, sorted by file and line ({answers[engine]})")
        check(not any(path.startswith("ignored/") for path, _ in answers[engine]), f"{engine}: git-ignored files are skipped")
        check(".env" not in {path for path, _ in answers[engine]}, f"{engine}: a secrets file is never searched")
        rendered = code_search.render(found, r"\bbuild_widget\b")
        check(rendered.startswith("6 match(es) in 3 file(s)") and "pkg/widgets.py:5: def build_widget" in rendered, f"{engine}: path:line: text, one line each")
        capped = code_search.search("build_widget", root=repo, max_results=2)
        check(len([m for m in capped.matches if not m.context]) == 2 and capped.truncated, f"{engine}: the cap is a cap, and says so")
        check("narrow with" in code_search.render(capped, "build_widget"), f"{engine}: …with how to narrow it")
        only_py = code_search.search("build_widget", root=repo, glob="*.py,!app.py")
        check({m.path for m in only_py.matches} == {"pkg/widgets.py"}, f"{engine}: globs include and exclude")
        # Found by the live gate run: a model wrote `**/*.py`, and the Python engine skipped the root-level app.py.
        deep = code_search.search("build_widget", root=repo, glob="**/*.py")
        check({m.path for m in deep.matches} == {"app.py", "pkg/widgets.py"}, f"{engine}: `**/*.py` matches root-level files too, as in ripgrep ({sorted({m.path for m in deep.matches})})")
        nested = code_search.search("build_widget", root=repo, glob="pkg/**/*.py")
        check({m.path for m in nested.matches} == {"pkg/widgets.py"}, f"{engine}: `pkg/**/*.py` matches pkg/widgets.py (zero directories)")
        files = code_search.search("build_widget", root=repo, files_only=True)
        check(set(files.files) == {"app.py", "notes-2-final.md", "pkg/widgets.py"}, f"{engine}: files_only lists files (a dash-and-digit name survives: {sorted(files.files)})")
        context = code_search.search("class Factory", root=repo, context=1)
        check([m.line for m in context.matches] == [8, 9, 10] and [m.context for m in context.matches] == [True, False, True], f"{engine}: context lines are marked as context")
        literal = code_search.search("{'size'", root=repo, literal=True)
        check(len(literal.matches) == 1, f"{engine}: literal text is not a regex")
    if len(answers) == 2:
        check(answers["python"] == answers["ripgrep"], "both engines give the same answer")
    else:
        print("   (ripgrep not installed: the Python engine was tested; `brew install ripgrep` for the fast path)")
    os.environ["CORDATA_RG"] = "off"
    outside = code_search.search("x", "/etc", root=repo)
    check(outside.error and "outside the project" in outside.error, "a path outside the project is refused")
    check("not a valid regular expression" in (code_search.search("(", root=repo).error or ""), "a broken regex says so")


def test_outline(repo: Path) -> None:
    code_nav.configure(str(repo))
    py = {s.name: s for s in code_nav.symbols(repo / "pkg" / "widgets.py")}
    check(set(py) == {"build_widget", "Factory", "Factory.make", "Factory.make_async"}, f"python: functions, classes and dotted methods ({sorted(py)})")
    check((py["build_widget"].start, py["build_widget"].end) == (4, 6), "python: a decorated function starts at its decorator")
    check(py["Factory.make_async"].kind == "method" and py["Factory"].kind == "class", "python: kinds")
    rust = {s.name: s for s in code_nav.symbols(repo / "lib.rs")}
    check({"Inbox", "Inbox.push", "Inbox.trim", "merge", "tests", "tests.helper"} <= set(rust), f"rust: structs, impl methods, fns, mods ({sorted(rust)})")
    check((rust["Inbox.push"].start, rust["Inbox.push"].end) == (6, 8), "rust: a brace in a comment does not move the end")
    check((rust["merge"].start, rust["merge"].end) == (15, 18), "rust: a brace in a string does not either")
    check(rust["tests.helper"].kind == "function", "rust: a function in a module is a function, not a method")
    ts = {s.name: s for s in code_nav.symbols(repo / "ui.ts")}
    check({"Panel", "Panel.render", "mount", "unmount", "Props"} <= set(ts), f"typescript: classes, methods, functions, arrows, interfaces ({sorted(ts)})")
    md = [s.name for s in code_nav.symbols(repo / "notes-2-final.md")]
    check(md == ["Notes", "Usage"], "markdown: headings")
    outline = code_nav.code_outline(path="pkg/widgets.py")
    check("L4-6  function build_widget" in outline and "L10-11  method Factory.make" in outline, "the outline gives ranges")
    source = code_nav.code_outline(path="pkg/widgets.py", symbol="Factory.make")
    check("10      def make(self):" in source and "make_async" not in source, "symbol= returns just that symbol, numbered")
    check("Close: Factory.make, Factory.make_async" in code_nav.code_outline(path="pkg/widgets.py", symbol="mak"), "an unknown name lists close ones")
    check("cannot read" in code_nav.code_outline(path="nope.py"), "a missing file says so")


def write_big(repo: Path, functions: int = 160) -> Path:
    big = repo / "big.py"
    big.write_text("".join(f"def handler_{i}(event):\n    total = event + {i}\n    return total * 2\n\n\n" for i in range(functions)))
    return big


async def test_large_reads_through_acp(repo: Path) -> None:
    big = write_big(repo)
    lines = len(big.read_text().splitlines())
    orchestrator_model([
        call("file_read", {"path": "big.py", "mode": "view"}),
        call("file_read", {"path": "big.py", "mode": "lines", "start_line": 10, "end_line": 14}),
        call("file_read", {"path": "app.py", "mode": "view"}),
        text("read"),
    ])
    server, conn = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(repo))).session_id
    await server.prompt(sid, [acp.text_block("look at big.py")])
    await settle()
    seen = [c["last_text"] for c in MODELS["orchestrator"].calls]
    check(f"big.py is {lines:,} lines" in seen[1] and "outline instead of its text" in seen[1], f"a whole-file read of a {lines}-line file returns its outline")
    check("L796-798  function handler_159" in seen[1] and "total = event + 159" not in seen[1], "…the outline, not the code")
    check("offloaded" not in seen[1].lower() and "retrieve_offloaded_content" not in seen[1], "…ahead of the offloader (nothing was set aside)")
    check("total = event + 2" in seen[2] and "outline" not in seen[2], "a line-range read is untouched")
    check("W = build_widget(1)" in seen[3], "a short file is read whole")


async def test_outline_loads_the_cold_tool(repo: Path) -> None:
    orchestrator_model([call("file_read", {"path": "big.py", "mode": "view"}), text("ok")])
    server, conn = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(repo))).session_id
    await server.prompt(sid, [acp.text_block("show me what is inside")])
    await settle()
    calls = MODELS["orchestrator"].calls
    check("code_outline" not in calls[0]["tools"] and "code_search" not in calls[0]["tools"], "on a turn that does not look like code work, the search and outline tools stay cold (2.3 KB of schema)")
    check("code_outline" in calls[1]["tools"], "…and a read that came back as an outline loads code_outline for the next step")


async def plugins_running(server, sid: str, timeout: float = 10.0) -> None:
    """The builtin plugins (the LSP tools among them) start off the request path; a real person's first message
    arrives well after. Wait for them, as that person would."""
    host = server._sessions[sid].plugins
    for _ in range(int(timeout / 0.05)):
        if all(row["status"] == "running" for row in host.status() if row["transport"] == "builtin"):
            return
        await asyncio.sleep(0.05)


async def test_gate_find_references(repo: Path) -> None:
    orchestrator_model([
        call("code_search", {"pattern": r"\bbuild_widget\b", "glob": "*.py"}),
        call("code_outline", {"path": "pkg/widgets.py", "symbol": "build_widget"}),
        call("lsp_references", {"path": "pkg/widgets.py", "line": 5, "column": 5}),
        text("build_widget is defined at pkg/widgets.py:5 and used at app.py:1, app.py:4, pkg/widgets.py:11 and :14."),
    ])
    server, conn = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(repo))).session_id
    await plugins_running(server, sid)
    await server.prompt(sid, [acp.text_block("find every reference to build_widget")])
    await settle()
    model = MODELS["orchestrator"]
    first = model.calls[0]
    check({"code_search", "code_outline", "lsp_references", "lsp_definition"} <= set(first["tools"]), "the turn's model has code_search, code_outline and the LSP tools loaded (not cold)")
    check("code_search (ripgrep)" in first["system_prompt"] and "don't read" in first["system_prompt"], "the system prompt steers search over reading")
    search_result = model.calls[1]["last_text"]
    check(search_result.count("\n") <= 6 and "app.py:4: W = build_widget(1)" in search_result, "the search answer is the lines, not the files")
    check("def build_widget(size: int) -> dict:" in model.calls[2]["last_text"] and "class Factory" not in model.calls[2]["last_text"], "then just the symbol")
    calls = [getattr(u, "title", "") or "" for u in conn.updates if type(u).__name__ == "ToolCallStart"]
    check(not any("file_read" in c for c in calls), f"no whole-file read happened ({calls})")
    stored = [b.get("toolUse", {}).get("name") for m in server._sessions[sid].agent.messages for b in m.get("content") or [] if isinstance(b, dict) and "toolUse" in b]
    check(stored == ["code_search", "code_outline", "lsp_references"], f"the tools used, in order: {stored}")


async def main() -> None:
    repo = make_repo("nav")
    test_search(repo)
    test_outline(repo)
    await test_large_reads_through_acp(repo)
    await test_outline_loads_the_cold_tool(repo)
    await test_gate_find_references(make_repo("gate1"))
    print("\ncode navigation: all checks passed")


if __name__ == "__main__":
    asyncio.run(main())

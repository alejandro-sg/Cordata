"""Plugin host, LSP bridge and apply_patch, end to end on throwaway projects.

* apply_patch: multi-file, atomic, moved context, refused escapes.
* PluginHost: a project's .mcp.json server is blocked until the folder is
  trusted; enabling hot-adds its tools to a live agent (approval-gated, output
  marked untrusted), MCP progress becomes harness task events, disabling
  removes them, a clashing tool name gets the server's prefix, and a built-in
  plugin (memory) swaps like any other.
* LSP: the real rust-analyzer (skipped with a note if it isn't installed) on a
  crate with a type error: lsp_diagnostics reports it; fixing the file through
  apply_patch in an Agent loop appends fresh, clean diagnostics to the tool
  result (LspEditHooks); go-to-definition resolves.

    backend/.venv/bin/python _plugins_lsp_smoke_test.py
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).parent.resolve()
_TMP = tempfile.TemporaryDirectory()
os.environ["CORDATA_HOME"] = str(Path(_TMP.name) / "home")  # never touch the real ~/.cordata

from strands import Agent, tool  # noqa: E402

import harness_events  # noqa: E402
import lsp_bridge  # noqa: E402
import patch_tool  # noqa: E402
import settings_store  # noqa: E402
from harness_testing import ScriptedModel, check, text, tool as call  # noqa: E402
from plugin_host import PluginHost, PluginSpec  # noqa: E402

ECHO = [sys.executable, str(HERE / "fixtures" / "echo_mcp_server.py")]


def test_patch() -> None:
    root = Path(_TMP.name) / "patch"
    (root / "src").mkdir(parents=True)
    (root / "src" / "a.py").write_text("def f():\n    return 1\n\n\ndef g():\n    return 2\n")
    (root / "b.txt").write_text("one\ntwo\nthree\n")
    patch = """--- a/src/a.py
+++ b/src/a.py
@@ -5,2 +5,2 @@
 def g():
-    return 2
+    return 3
--- a/b.txt
+++ b/b.txt
@@ -1,3 +1,4 @@
 one
+one and a half
 two
 three
--- /dev/null
+++ b/new/c.md
@@ -0,0 +1,2 @@
+# new
+file
"""
    summary = patch_tool.apply(patch, root)
    check((root / "src" / "a.py").read_text().endswith("return 3\n"), "a hunk applies at its line")
    check((root / "b.txt").read_text() == "one\none and a half\ntwo\nthree\n", "an insertion applies")
    check((root / "new" / "c.md").read_text() == "# new\nfile\n", "a new file is created")
    check("patched src/a.py (+1 -1)" in summary and "created new/c.md" in summary, f"summary lists every file ({summary!r})")

    # Context moved by two lines: still found, searching outward from the hint.
    (root / "src" / "a.py").write_text("import os\n\n" + (root / "src" / "a.py").read_text())
    patch_tool.apply("--- a/src/a.py\n+++ b/src/a.py\n@@ -5,2 +5,2 @@\n def g():\n-    return 3\n+    return 4\n", root)
    check((root / "src" / "a.py").read_text().endswith("return 4\n"), "a hunk whose context moved still applies")

    before = (root / "b.txt").read_text()
    try:
        patch_tool.apply("--- a/b.txt\n+++ b/b.txt\n@@ -1,1 +1,1 @@\n-one\n+uno\n--- a/src/a.py\n+++ b/src/a.py\n@@ -1,1 +1,1 @@\n-nope\n+x\n", root)
        raise AssertionError("mismatching hunk applied")
    except patch_tool.PatchError as error:
        check("does not match" in str(error), "a hunk that doesn't match is an error")
    check((root / "b.txt").read_text() == before, "…and no file changed (all or nothing)")
    # How models actually write patches (found in a live run with gpt-oss:20b):
    model = root / "model"
    model.mkdir()
    (model / "calc.py").write_text('def add(a, b):\n    """Sum."""\n    return a - b\n')
    patch_tool.apply("--- a/calc.py\n+++ b/calc.py\n@@\n-    return a - b\n+    return a + b\n", model)
    check((model / "calc.py").read_text().endswith("return a + b\n"), "a bare @@ hunk (no line numbers) is located by its content")
    patch_tool.apply(
        "--- a/calc.py\n+++ b/calc.py\n@@ -1,3 +1,3 @@\ndef add(a, b):\n    \"\"\"Sum.\"\"\"\n-    return a + b\n+    return b + a\n",
        model,
    )
    check((model / "calc.py").read_text() == 'def add(a, b):\n    """Sum."""\n    return b + a\n', "context lines that lost their marker keep the file's own indentation")
    patch_tool.apply(
        "*** Begin Patch\n*** Update File: calc.py\n@@ def add(a, b):\n-    return b + a\n+    return a + b\n*** Add File: notes.md\n+# notes\n*** End Patch",
        model,
    )
    check((model / "calc.py").read_text().endswith("return a + b\n") and (model / "notes.md").read_text() == "# notes\n", "the *** Begin Patch envelope (update + add)")
    patch_tool.apply("*** Begin Patch\n*** Delete File: notes.md\n*** End Patch", model)
    check(not (model / "notes.md").exists(), "…and delete")
    try:
        patch_tool.apply("--- a/calc.py\n+++ b/calc.py\n@@\n+orphan\n", model)
        raise AssertionError("an unplaceable insertion applied")
    except patch_tool.PatchError as error:
        check("no line numbers or context" in str(error), "an insertion with neither line numbers nor context is refused, not guessed")

    for bad in ("../outside.txt", "/etc/passwd"):
        try:
            patch_tool.apply(f"--- /dev/null\n+++ b/{bad}\n@@ -0,0 +1 @@\n+x\n", root)
            raise AssertionError(f"escaped to {bad}")
        except patch_tool.PatchError as error:
            check("outside the project" in str(error), f"a patch can't write {bad}")


@tool
def core_echo() -> str:
    """A core tool."""
    return "core"


def test_plugins() -> None:
    project = Path(_TMP.name) / "proj"
    project.mkdir()
    (project / ".mcp.json").write_text(
        json.dumps({"mcpServers": {"echo": {"command": ECHO[0], "args": ECHO[1:], "description": "echo test server"}}})
    )
    events: list = []
    unsubscribe = harness_events.subscribe(lambda kind, payload: events.append((kind, payload)))

    memory_tools = [core_echo]
    builtin = PluginSpec(name="memory", kind="memory", transport="builtin", source="built-in", builtin_tools=lambda: [])
    host = PluginHost(project, builtins=[builtin], reserved_names={"shell", "editor"})
    agent = Agent(model=ScriptedModel([text("x")]), tools=[], callback_handler=None)
    host.attach(agent)

    row = host.enable("echo")
    check(not row["ok"] and row["status"] == "blocked" and "trust" in row["error"], "a project's MCP server is blocked until /trust")
    check("echo" not in agent.tool_names, "…and contributes no tools")

    settings_store.trust_project(project)
    row = host.enable("echo")
    check(row["ok"] and row["status"] == "running" and sorted(row["tools"]) == ["echo", "slow_count"], f"after /trust it starts ({row})")
    check({"echo", "slow_count"} <= set(agent.tool_names), "its tools are hot-added to the live agent")
    check(host.requires_approval("echo") is True and host.requires_approval("shell") is None, "plugin tools ask for approval; core tools are not plugin tools")
    check("slow_count" in host.untrusted_names and "shell" not in host.untrusted_names, "plugin output is marked untrusted (live membership)")
    check(host.descriptions() == [("echo", "tools", "echo test server")], "running plugins are routing targets with descriptions")

    # A late-attached agent gets the running plugin's tools immediately.
    late = Agent(model=ScriptedModel([call("slow_count", {"n": 4}), text("ok")]), tools=[], callback_handler=None)
    host.attach(late)
    host.note_call("slow_count", True)
    late("count")
    host.note_call("slow_count", False)
    results = [b["toolResult"]["content"][0]["text"] for m in late.messages for b in m["content"] if "toolResult" in b]
    check(results == ["counted to 4"], f"a plugin tool runs through the agent loop ({results})")
    progress = [p for k, p in events if k == "task" and p["id"].startswith("mcp-echo-slow_count")]
    check(progress and progress[-1]["done"] == 4 and progress[-1]["total"] == 4, f"MCP progress becomes task events ({[p['done'] for p in progress]})")
    check(any(k == "plugins" and p["plugin"]["status"] == "running" for k, p in events), "plugin lifecycle is announced")

    # A second server whose tool name clashes gets prefixed with its own name.
    (project / ".cordata").mkdir(exist_ok=True)
    (project / ".cordata" / "mcp.local.json").write_text(
        json.dumps({"mcpServers": {"dup": {"command": ECHO[0], "args": ECHO[1:], "env": {"ECHO_SERVER_NAME": "dup"}}}})
    )
    host.discover()
    row = host.enable("dup")
    check(row["ok"] and sorted(row["tools"]) == ["dup_echo", "dup_slow_count"], f"a clashing server is prefixed ({row.get('tools')})")

    row = host.disable("echo", persist=True)
    check(row["status"] == "disabled" and not {"echo", "slow_count"} & set(agent.tool_names), "disable hot-removes its tools")
    check({"dup_echo"} <= set(agent.tool_names), "…leaving other plugins alone")
    check("echo" in host.persisted_disabled(), "a persisted disable is remembered for this folder")
    fresh = PluginHost(project, reserved_names={"shell"})
    fresh.start_autostart()
    states = {r["name"]: r["status"] for r in fresh.status()}
    check(states.get("echo") == "disabled" and states.get("dup") == "running", f"autostart honours it ({states})")
    fresh.shutdown()

    builtin.builtin_tools = lambda: memory_tools
    host.enable("memory")
    check("core_echo" in agent.tool_names, "a built-in plugin hot-adds its tools")
    host.disable("memory")
    check("core_echo" not in agent.tool_names, "…and hot-removes them")
    check(host.requires_approval("core_echo") is None, "built-in plugin tools keep their own approval rules")
    host.shutdown()
    unsubscribe()


def test_lsp() -> None:
    if not shutil.which("rust-analyzer") or not shutil.which("cargo"):
        print("skip lsp: rust-analyzer/cargo not installed")
        return
    crate = Path(_TMP.name) / "demo_crate"
    subprocess.run(["cargo", "new", "--lib", "--vcs", "none", "-q", str(crate)], check=True)
    lib = crate / "src" / "lib.rs"
    lib.write_text("pub fn double(x: i32) -> i32 {\n    x * 2\n}\n\npub fn broken() -> i32 {\n    let s: String = double(2);\n    s.len() as i32\n}\n")

    # A cloned, untrusted crate: rust-analyzer is told not to run its build scripts, proc-macros or `cargo check`
    # (that is the crate's own code). Its type errors come from `cargo check`, so the rest of this test trusts it.
    check(lsp_bridge.LspClient(lsp_bridge.SERVERS[0], ("rust-analyzer",), str(crate), trusted=False).restricted == lsp_bridge.UNTRUSTED_OPTIONS["rust"], "an untrusted folder's rust-analyzer runs restricted")
    check(lsp_bridge.LspClient(lsp_bridge.SERVERS[0], ("rust-analyzer",), str(crate), trusted=False)._configuration("rust-analyzer")["cargo"]["buildScripts"]["enable"] is False, "…including when it asks for its configuration")
    check(not lsp_bridge.folder_trusted(str(crate)), "the crate starts untrusted")
    settings_store.trust_project(str(crate))
    check(lsp_bridge.LspClient(lsp_bridge.SERVERS[0], ("rust-analyzer",), str(crate), trusted=lsp_bridge.folder_trusted(str(crate))).restricted is None, "a trusted folder's rust-analyzer runs as configured")

    events: list = []
    unsubscribe = harness_events.subscribe(lambda kind, payload: events.append((kind, payload)))
    os.chdir(crate)
    manager = lsp_bridge.manager_for(crate)
    check([spec.name for spec, cmd in manager.detected if cmd] == ["rust"], "rust-analyzer is detected for a Cargo crate")

    started = time.monotonic()
    report = ""
    while time.monotonic() - started < 120:
        report = lsp_bridge.lsp_diagnostics("src/lib.rs")
        if "error" in report:
            break
        time.sleep(2)
    check("src/lib.rs:6" in report and "error" in report, f"lsp_diagnostics reports the type error ({report[:200]!r})")
    print(f"   (first diagnostics after {time.monotonic() - started:.1f}s)")

    definition = lsp_bridge.lsp_definition("src/lib.rs", 6, 22)
    check("src/lib.rs:1:" in definition and "pub fn double" in definition, f"go-to-definition resolves ({definition!r})")

    fix = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -6,2 +6,2 @@\n-    let s: String = double(2);\n-    s.len() as i32\n+    let s: i32 = double(2);\n+    s\n"
    agent = Agent(
        model=ScriptedModel([call("apply_patch", {"patch": fix}), text("fixed")]),
        tools=[patch_tool.apply_patch],
        hooks=[lsp_bridge.LspEditHooks(timeout=30)],
        callback_handler=None,
    )
    agent("fix it")
    result = [b["toolResult"] for m in agent.messages for b in m["content"] if "toolResult" in b][0]
    text_out = "\n".join(c.get("text", "") for c in result["content"])
    check(result["status"] == "success" and "patched src/lib.rs" in text_out, "the fix applies through the agent loop")
    check("no diagnostics after this edit" in text_out, f"fresh diagnostics are appended to the edit's result ({text_out[-200:]!r})")
    check(any(k == "lsp" and p.get("event") == "diagnostics" and p.get("errors") == 0 for k, p in events), "a clean-diagnostics event is emitted")
    tasks = [p for k, p in events if k == "task" and p.get("source") == "lsp"]
    check(any(t["state"] == "completed" for t in tasks), f"server progress became task events ({len(tasks)} updates)")
    status = manager.status()
    check(status[0]["state"] == "running", f"status reports the running server ({status})")
    lsp_bridge.shutdown_all()
    unsubscribe()


def main() -> None:
    test_patch()
    test_plugins()
    test_lsp()
    print("\nPASS: apply_patch, the plugin host (trust, hot swap, progress) and the LSP bridge")


if __name__ == "__main__":
    main()

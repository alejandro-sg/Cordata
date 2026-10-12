"""Preflight: an edit is checked before anyone is asked about it.

* **unit** — syntax (Python, JSON, TOML) blames only what the edit *newly* breaks; the linter runs on the proposed text
  through stdin, against the file's own baseline (a stand-in ``ruff`` on PATH speaks ruff's JSON); the language-server
  check judges all files of one edit together, blames only new errors, and reports cross-file errors (an import that
  does not resolve yet) as notes, not refusals; a check that cannot run is "skipped", never a pass; README edits take
  the synchronous path; the retry tracker counts per file per turn and resets on a pass.
* **real language server** — rust-analyzer (when installed) judges an unsaved proposed text without a ``cargo check``.
* **Gate 2, through the real ACP server** — the agent proposes a patch with a syntax error: it is not applied and the
  person is not asked; the agent gets the error, proposes the fixed patch, and *that* is the one approval the person
  sees. Then the give-up path: with ``preflight.maxRetries: 1`` an edit that keeps failing ends the turn's editing and
  the turn's earlier edit is rolled back from its checkpoint — the person's own git untouched.

    backend/.venv/bin/python _preflight_smoke_test.py
"""

import asyncio
import json
import os
import shutil
import stat
import subprocess
import time
from pathlib import Path
from types import SimpleNamespace

os.environ["CORDATA_PRISM"] = "off"

import provider_settings  # noqa: F401  (before agent_common, which chdirs)

import acp  # noqa: E402

import harness_events  # noqa: E402
import lsp_bridge  # noqa: E402
import preflight  # noqa: E402
from _acp_harness_smoke_test import MODELS, ROOT, Conn, call, fresh_server, laya_answer, orchestrator_model, settle, text  # noqa: E402
from harness_testing import check  # noqa: E402

ROUTE = laya_answer("code", {"code": 0.97, "answer": 0.03})


def repo(name: str, files: dict[str, str]) -> Path:
    path = ROOT / name
    path.mkdir(parents=True)
    for rel, body in files.items():
        (path / rel).parent.mkdir(parents=True, exist_ok=True)
        (path / rel).write_text(body)
    return path


def patch(path: str, old: str, new: str) -> str:
    return f"--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-{old}\n+{new}\n"


# ------------------------------------------------------------------------------------------------- unit


def test_syntax() -> None:
    root = repo("syntax", {"ok.py": "x = 1\n", "broken.py": "def f(:\n", "conf.json": '{"a": 1}\n', "conf.toml": "a = 1\n"})
    os.environ["CORDATA_PREFLIGHT_RUFF"] = "off"
    bad = preflight.check(str(root), "apply_patch", {"patch": patch("ok.py", "x = 1", "x = (1")}, lsp=False)
    check(not bad.ok and bad.problems[0].check == "syntax" and bad.problems[0].path == "ok.py" and bad.problems[0].line == 1, f"a Python syntax error is caught ({bad.problems[0].render()})")
    good = preflight.check(str(root), "apply_patch", {"patch": patch("ok.py", "x = 1", "x = 2")}, lsp=False)
    check(good.ok and "syntax" in good.ran and "ruff (not installed)" in good.skipped, "a good edit passes, and a check that could not run says skipped")
    already = preflight.check(str(root), "apply_patch", {"patch": patch("broken.py", "def f(:", "def g(:")}, lsp=False)
    check(already.ok, "a file that did not parse before is not this edit's fault")
    check(not preflight.check(str(root), "file_write", {"path": "conf.json", "content": '{"a": }'}, lsp=False).ok, "JSON")
    check(not preflight.check(str(root), "editor", {"command": "str_replace", "path": "conf.toml", "old_str": "a = 1", "new_str": "a = = 1"}, lsp=False).ok, "TOML, through editor str_replace")
    check(preflight.check(str(root), "editor", {"command": "create", "path": "new.py", "file_text": "def ok():\n    return 1\n"}, lsp=False).ok, "a new file with good syntax")
    check(not preflight.applicable("editor", {"command": "create", "path": "README.md"}) and preflight.applicable("apply_patch", {"patch": patch("a.py", "x", "y")}), "a README edit takes the synchronous path; a .py edit is checked")


def test_ruff_baseline() -> None:
    root = repo("lint", {"mod.py": "import os  # LINT\nvalue = 1\n"})
    bindir = ROOT / "bin"
    bindir.mkdir(exist_ok=True)
    fake = bindir / "ruff"
    fake.write_text(
        "#!/usr/bin/env python3\n"
        "import json, sys\n"
        "rows = [{'code': 'F401', 'message': 'unused import', 'location': {'row': n + 1, 'column': 1}} for n, line in enumerate(sys.stdin.read().splitlines()) if 'LINT' in line]\n"
        "print(json.dumps(rows)); sys.exit(1 if rows else 0)\n"
    )
    fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
    host_path = os.environ["PATH"]
    os.environ["PATH"] = f"{bindir}{os.pathsep}{host_path}"
    os.environ.pop("CORDATA_PREFLIGHT_RUFF", None)
    try:
        check(preflight.ruff_binary(str(root)) == str(fake), "a ruff on PATH is the one used")
        same = preflight.check(str(root), "apply_patch", {"patch": patch("mod.py", "import os  # LINT", "import os  # LINT  (comment)")}, lsp=False)
        check(same.ok and "ruff" in same.ran, "a lint finding the file already had is not this edit's")
        more = preflight.check(str(root), "file_write", {"path": "mod.py", "content": "import os  # LINT\nimport sys  # LINT\nvalue = 1\n"}, lsp=False)
        check(not more.ok and more.problems[0].render() == "mod.py:2: [ruff] F401 unused import", f"a new one is ({more.problems[0].render() if more.problems else '—'})")
        (root / ".venv" / "bin").mkdir(parents=True)
        shutil.copy(fake, root / ".venv" / "bin" / "ruff")
        # A PATH with no ruff on it at all — the host may have one of its own (Homebrew's, say).
        os.environ["PATH"] = os.pathsep.join(d for d in os.environ["PATH"].split(os.pathsep)[1:] if not os.path.exists(os.path.join(d, "ruff")))
        check(preflight.ruff_binary(str(root)) is None and preflight.ruff_binary(str(root), trusted=True) == str(root / ".venv" / "bin" / "ruff"), "a repository's own .venv/bin/ruff only runs for a trusted folder")
    finally:
        os.environ["CORDATA_PREFLIGHT_RUFF"] = "off"
        os.environ["PATH"] = host_path


class FakeClient:
    """Stands in for a running server: its diagnostics are a function of every file's proposed text."""

    def __init__(self, judge):
        self.judge = judge
        self.calls: list[list[str]] = []

    def check_texts(self, files, timeout=5.0):
        texts = {str(Path(p).resolve()): t for p, t in files}
        self.calls.append(sorted(Path(p).name for p in texts))
        return {path: self.judge(path, text, texts) for path, text in texts.items()}


class FakeManager:
    enabled = True

    def __init__(self, client):
        self.client = client

    def spec_for(self, path):
        return object() if str(path).endswith(".py") else None

    def client_for(self, path, start=True):
        return self.client


def test_lsp_logic() -> None:
    root = repo("lsp", {"lib.py": "def total():\n    return 1\n", "use.py": "from lib import total\n", "bad.py": "x: int = 'a'\n"})

    def judge(path, text, together):
        out = []
        if path.endswith("use.py") and "order_total" in text and not any("def order_total" in t for t in together.values()) and "def order_total" not in (Path(path).parent / "lib.py").read_text():
            out.append({"severity": 1, "code": "reportAttributeAccessIssue", "message": '"order_total" is unknown import symbol', "range": {"start": {"line": 0}}})
        if "int = 'a'" in text:
            out.append({"severity": 1, "code": "reportAssignmentType", "message": "str is not int", "range": {"start": {"line": 0}}})
        if "Dict" in text and "[" not in text.split("Dict", 1)[1][:1]:
            out.append({"severity": 1, "code": "reportMissingTypeArgument", "message": 'Expected type arguments for generic class "Dict"', "range": {"start": {"line": 0}}})
        if "return None + 1" in text:
            out.append({"severity": 1, "code": "reportOperatorIssue", "message": "Operator + not supported", "range": {"start": {"line": 1}}})
        return out

    client = FakeClient(judge)
    manager = FakeManager(client)
    rename = (
        "--- a/lib.py\n+++ b/lib.py\n@@ -1 +1 @@\n-def total():\n+def order_total():\n"
        "--- a/use.py\n+++ b/use.py\n@@ -1 +1 @@\n-from lib import total\n+from lib import order_total\n"
    )
    together = preflight.check(str(root), "apply_patch", {"patch": rename}, manager=manager)
    check(together.ok and client.calls[-1] == ["lib.py", "use.py"], "a multi-file rename is judged as one change (both files sent together)")
    alone = preflight.check(str(root), "apply_patch", {"patch": "--- a/use.py\n+++ b/use.py\n@@ -1 +1 @@\n-from lib import total\n+from lib import order_total\n"}, manager=manager)
    check(alone.ok and alone.notes and "unknown import symbol" in alone.notes[0].message, "an import that does not resolve yet is a note, not a refusal (the agent may be about to add it)")
    new_error = preflight.check(str(root), "apply_patch", {"patch": "--- a/lib.py\n+++ b/lib.py\n@@ -1,2 +1,2 @@\n def total():\n-    return 1\n+    return None + 1\n"}, manager=manager)
    check(not new_error.ok and new_error.problems[0].render() == "lib.py:2: [lsp] [reportOperatorIssue] Operator + not supported", "a new type error in the edited file is refused")
    old_error = preflight.check(str(root), "apply_patch", {"patch": "--- a/bad.py\n+++ b/bad.py\n@@ -1 +1,2 @@\n x: int = 'a'\n+y = 2\n"}, manager=manager)
    check(old_error.ok, "an error the file already had is not this edit's")
    strict = preflight.check(str(root), "editor", {"command": "create", "path": str(root / "fresh.py"), "file_text": "from typing import Dict\n\ndef f() -> Dict:\n    return {}\n"}, manager=manager)
    check(strict.ok and strict.notes and "reportMissingTypeArgument" in strict.notes[0].message, "a strictness-only complaint about a brand-new file is a note, not a refusal")
    cached = len(client.calls)
    preflight.check(str(root), "apply_patch", {"patch": "--- a/bad.py\n+++ b/bad.py\n@@ -1 +1,2 @@\n x: int = 'a'\n+y = 2\n"}, manager=manager)
    check(len(client.calls) == cached, "the same call on the same files is answered from the cache (an approval resume does not re-check)")


def test_tracker() -> None:
    track = preflight.tracker("unit")
    check(track.failed("t1", ["a.py"]) == 1 and track.failed("t1", ["a.py"]) == 2 and track.failed("t1", ["b.py"]) == 1, "failures count per file")
    track.passed("t1", ["a.py"])
    check(track.failed("t1", ["a.py"]) == 1, "a pass resets that file")
    track.give_up("t1", "stop")
    check(track.stopped("t1") == "stop" and track.stopped("t2") is None, "giving up holds for the turn, and only that turn")


def test_real_rust_analyzer() -> None:
    if not shutil.which("cargo") or lsp_bridge.locate("rust-analyzer") is None:
        print("   (rust-analyzer not installed: the real-server check is skipped)")
        return
    crate = ROOT / "preflight_crate"
    subprocess.run(["cargo", "new", "--lib", "--vcs", "none", "-q", str(crate)], check=True)
    lib = crate / "src" / "lib.rs"
    lib.write_text("pub fn double(x: i32) -> i32 {\n    x * 2\n}\n")
    client = lsp_bridge.LspClient(lsp_bridge.SERVERS[0], (lsp_bridge.locate("rust-analyzer"),), str(crate), trusted=False)
    client.initialize()
    try:
        deadline = time.monotonic() + 60
        broken = []
        while time.monotonic() < deadline and not broken:
            broken = [d for d in client.check_text(str(lib), "pub fn double(x: i32) -> i32 {\n    x * 2\n\n", 6.0) if d.get("severity", 1) == 1]
        check(broken and "syntax" in str(broken[0].get("code", "")), f"rust-analyzer judges an unsaved proposed text ({broken[0].get('message') if broken else '—'})")
        check(lib.read_text().endswith("x * 2\n}\n"), "…without the file being written")
        check(not (crate / "target").exists(), "…and without a cargo build (nothing ran)")
    finally:
        client.shutdown()


# ------------------------------------------------------------------------------------------------- through the server


class CountingConn(Conn):
    def __init__(self) -> None:
        super().__init__()
        self.asked: list[dict] = []

    async def request_permission(self, session_id, tool_call, options, **kwargs):
        self.asked.append(getattr(tool_call, "raw_input", None) or {})
        return await super().request_permission(session_id, tool_call, options, **kwargs)


def user_settings(values: dict) -> None:
    home = Path(os.environ["CORDATA_HOME"])
    home.mkdir(parents=True, exist_ok=True)
    (home / "settings.json").write_text(json.dumps(values))


async def session(cwd: Path, steps):
    orchestrator_model(steps)
    server, _ = fresh_server(ROUTE)
    conn = CountingConn()
    server.on_connect(conn)
    sid = (await server.new_session(cwd=str(cwd))).session_id
    return server, conn, sid


async def test_gate_self_correction() -> None:
    user_settings({"lsp.enabled": False})
    root = repo("gate2", {"calc.py": "def add(a, b):\n    return a + b\n"})
    broken = "--- a/calc.py\n+++ b/calc.py\n@@ -1,2 +1,2 @@\n def add(a, b):\n-    return a + b\n+    return (a + b\n"
    fixed = "--- a/calc.py\n+++ b/calc.py\n@@ -1,2 +1,2 @@\n def add(a, b):\n-    return a + b\n+    return a + b + 0\n"
    events: list[dict] = []
    unsubscribe = harness_events.subscribe(lambda kind, payload: events.append(payload) if kind == "preflight" else None)
    try:
        server, conn, sid = await session(root, [call("apply_patch", {"patch": broken}), call("apply_patch", {"patch": fixed}), text("done")])
        await server.prompt(sid, [acp.text_block("make add return a + b + 0")])
        await settle()
    finally:
        unsubscribe()
    seen = MODELS["orchestrator"].calls[1]["last_text"]
    check("[preflight] apply_patch was not applied" in seen and "calc.py:2: [syntax]" in seen, "the broken patch comes back to the agent with the error")
    check(len(conn.asked) == 1 and conn.asked[0].get("patch") == fixed, f"the person is asked once — about the fixed patch ({len(conn.asked)} prompt(s))")
    check((root / "calc.py").read_text() == "def add(a, b):\n    return a + b + 0\n", "the fixed patch landed; the broken one never touched the file")
    check([e["phase"] for e in events] == ["failed", "passed"], f"preflight said failed, then passed ({[e['phase'] for e in events]})")


async def test_give_up_rolls_back() -> None:
    user_settings({"lsp.enabled": False, "preflight.maxRetries": 1, "permissions.defaultMode": "auto"})
    root = repo("gate2b", {"calc.py": "def add(a, b):\n    return a + b\n"})
    subprocess.run(["git", "init", "-q"], cwd=root, check=True)
    head = (root / ".git" / "HEAD").read_text()
    broken = "--- a/calc.py\n+++ b/calc.py\n@@ -1,2 +1,2 @@\n def add(a, b):\n-    return a + b\n+    return (a + b\n"
    server, conn, sid = await session(root, [
        call("editor", {"command": "create", "path": "helper.py", "file_text": "def helper():\n    return 1\n"}),
        call("apply_patch", {"patch": broken}),
        call("apply_patch", {"patch": broken.replace("(a + b", "[a + b")}),
        call("editor", {"command": "create", "path": "more.py", "file_text": "x = 1\n"}),
        text("could not do it"),
    ])
    await server.set_session_mode(sid, "auto")
    await server.prompt(sid, [acp.text_block("refactor calc")])
    await settle()
    calls = MODELS["orchestrator"].calls
    check("[preflight]" in calls[2]["last_text"] and "(1 attempt(s) left)" in calls[2]["last_text"], "the first failure is sent back for another attempt (one left)")
    last = calls[3]["last_text"]
    check("this is the last attempt" in last and "Stop editing" in last, "the second failure ends the turn's editing")
    check("put back as they were before it" in last and "helper.py" in last, f"…and says the turn's earlier edit was rolled back ({last[last.find('The '):][:120]!r})")
    check(not (root / "helper.py").exists(), "helper.py, created earlier in the turn, is gone again")
    check((root / "calc.py").read_text() == "def add(a, b):\n    return a + b\n", "calc.py is as it was")
    check("Editing has stopped" in calls[4]["last_text"] and not (root / "more.py").exists(), "a later edit in the same turn is refused")
    check((root / ".git" / "HEAD").read_text() == head and not subprocess.run(["git", "log", "-1"], cwd=root, capture_output=True).stdout, "the person's own git was not committed to")


async def main() -> None:
    test_syntax()
    test_ruff_baseline()
    test_lsp_logic()
    test_tracker()
    test_real_rust_analyzer()
    await test_gate_self_correction()
    await test_give_up_rolls_back()
    print("\npreflight: all checks passed")


if __name__ == "__main__":
    asyncio.run(main())

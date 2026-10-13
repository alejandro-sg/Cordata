"""What the native app's editor asks the server for (docs/app.md §4b, phase P3) — offline, through the real ACP server.

1. **squiggles come from the same trust-gated servers the agent uses** — `lsp/diagnostics` refuses a folder that is not
   trusted (and says trust is what it needs), and a path outside the project;
2. **a file no server handles is not an error** — no diagnostics, no server;
3. **unsaved text is checked without being written** — with rust-analyzer installed, a syntax error in the editor's
   buffer comes back as a diagnostic while the file on disk is untouched (skipped when it is not installed);
4. `lsp/status` says whether the folder is trusted, so the editor can offer trust instead of an empty gutter.

    backend/.venv/bin/python _editor_surface_smoke_test.py
"""

from __future__ import annotations

import asyncio
import os
import shutil
import time

os.environ["CORDATA_PRISM"] = "off"

import provider_settings  # noqa: F401,E402

import lsp_bridge  # noqa: E402
import settings_store  # noqa: E402
from _acp_harness_smoke_test import fresh_server, laya_answer, orchestrator_model, project, text  # noqa: E402
from harness_testing import check  # noqa: E402

ROUTE = laya_answer("code", {"code": 0.97, "answer": 0.03})


async def ext(server, method: str, sid: str, **params) -> dict:
    return await server.ext_method(f"_cordata/{method}", {"sessionId": sid, **params})


async def main() -> None:
    cwd = project("editor")
    (cwd / "Cargo.toml").write_text('[package]\nname = "editor_demo"\nversion = "0.1.0"\nedition = "2021"\n\n[lib]\npath = "src/lib.rs"\n')
    (cwd / "src").mkdir()
    lib = cwd / "src" / "lib.rs"
    lib.write_text("pub fn double(x: i32) -> i32 {\n    x * 2\n}\n")
    (cwd / "notes.txt").write_text("plain\n")

    orchestrator_model([text("hi")])
    server, _ = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(cwd))).session_id

    print("\n=== 1. trust and confinement ===")
    status = await ext(server, "lsp/status", sid)
    check(status["trusted"] is False and isinstance(status["servers"], list), f"lsp/status says the folder is not trusted ({status['trusted']})")
    untrusted = await ext(server, "lsp/diagnostics", sid, path="src/lib.rs")
    check(not untrusted["ok"] and untrusted.get("needs") == "trust", f"no language server starts in an untrusted folder ({untrusted})")
    outside = await ext(server, "lsp/diagnostics", sid, path="../../etc/hosts")
    check(not outside["ok"] and "outside" in outside["reason"], f"a path outside the project is refused ({outside})")
    absolute = await ext(server, "lsp/diagnostics", sid, path="/etc/hosts")
    check(not absolute["ok"] and "outside" in absolute["reason"], "…including an absolute one")

    settings_store.trust_project(str(cwd))
    check((await ext(server, "lsp/status", sid))["trusted"], "trusted, lsp/status says so")

    print("\n=== 2. a file no server handles ===")
    plain = await ext(server, "lsp/diagnostics", sid, path="notes.txt")
    check(plain["ok"] and plain["diagnostics"] == [] and plain["server"] is None, f"no server, no diagnostics, no error ({plain})")

    print("\n=== 3. unsaved text ===")
    if not shutil.which("rust-analyzer"):
        print("skip: rust-analyzer not installed")
    else:
        before = lib.read_text()
        buffer = "pub fn double(x: i32) -> i32 {\n    x * 2\n}\n\npub fn broken( {\n"
        got: dict = {}
        started = time.monotonic()
        while time.monotonic() - started < 90:
            got = await ext(server, "lsp/diagnostics", sid, path="src/lib.rs", text=buffer)
            if got.get("ok") and got["diagnostics"]:
                break
            await asyncio.sleep(1.5)
        check(got.get("ok") and got.get("server") == "rust", f"rust-analyzer answers for a .rs file ({got.get('server')}, {got.get('reason')})")
        rows = got.get("diagnostics") or []
        check(any(row["start"]["line"] >= 4 and row["severity"] == 1 for row in rows), f"the buffer's syntax error comes back with its line ({rows[:2]})")
        check(all({"start", "end", "severity", "message", "source"} <= set(row) for row in rows), "every row has what the editor draws")
        check(lib.read_text() == before, "the file on disk is untouched")
        print(f"   (diagnostics after {time.monotonic() - started:.1f}s)")
    lsp_bridge.shutdown_all()
    print("\nALL OK")


if __name__ == "__main__":
    asyncio.run(main())

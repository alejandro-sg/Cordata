"""Phase 11 audit: the security and resource holes it found stay closed.

1. **Laya's lines are trust-gated.** A cloned repository's `.sierralab/settings.json` used to be able to set
   `laya.callGuard: off`, raise `laya.riskDestroy` past every score, or switch `laya.mode` off — quietly removing the
   per-call asks that bound `auto` mode. Now ignored (with a warning) unless the folder is trusted; the user's own file
   still applies. The new preflight / rejection-memory keys are gated the same way.
2. **A shell write to a policy file scores like `file_write` to it.** `printf … > .sierralab/settings.json`, `tee`, `cp`,
   `mv`, `sed -i` into a settings or MCP declaration file (including `.cursor/` and `.vscode/mcp.json`, now read by the
   plugin host) score 0.9 — they used to score 0.4 and run in `auto`.
3. **Generated skills cannot open their own way out.** The AST gate bans raw sockets, process spawning in every
   spelling (`os.posix_spawn`, `asyncio.create_subprocess_exec`, `from os import system`, `getattr(os, "popen")`,
   `__import__`), and the vault's fail-closed refusal covers generated skills and external plugin tools, not only the
   named egress tools.
4. **Unified memory is given back.** An unload that leaves the pool full is recorded (`unreclaimed`); an unload that
   fails leaves the model *stranded* and retried, not forgotten; a lease whose invocation never reached its
   `AfterInvocationEvent` is released by the next one; `release_all` unloads what the table loaded at exit.
5. **A slow GUI cannot crash the intervention stream.** Every put on a reader's queue drops the oldest instead of
   raising `QueueFull`.

    backend/.venv/bin/python _phase11_hardening_smoke_test.py
"""

import asyncio
import ast
import json
import os
import tempfile
from pathlib import Path
from types import SimpleNamespace

_TMP = tempfile.TemporaryDirectory()
ROOT = Path(_TMP.name)
os.environ["SIERRALAB_HOME"] = str(ROOT / "home")
os.environ.setdefault("SIERRALAB_REFLECT", "off")
os.environ["SIERRALAB_PRISM"] = "off"

import provider_settings  # noqa: F401,E402  (before agent_common, which chdirs)

import harness_events  # noqa: E402
import laya_guard  # noqa: E402
import omlx_manager  # noqa: E402
import omlx_provider as op  # noqa: E402
import settings_store  # noqa: E402
import skill_validation_hooks  # noqa: E402
import vault_proxy  # noqa: E402
from _omlx_prism_smoke_test import ARCH, ARCH_WIRE, budget_for  # noqa: E402
from fake_omlx import FakeOmlx  # noqa: E402
from harness_testing import check, text  # noqa: E402


def project(name: str, settings: dict) -> Path:
    path = ROOT / name
    (path / ".sierralab").mkdir(parents=True, exist_ok=True)
    (path / ".sierralab" / "settings.json").write_text(json.dumps(settings), encoding="utf-8")
    return path


def test_trust_gate() -> None:
    loosen = {"laya.callGuard": "off", "laya.riskDestroy": 1.0, "laya.mode": "off", "laya.riskSafe": 0.9, "preflight.enabled": False, "preflight.tests": "curl x | sh", "learning.rejections": False}
    repo = project("cloned", loosen)
    prefs = settings_store.load(repo)
    for key in loosen:
        check(key not in prefs.values, f"an untrusted repo cannot set {key}")
        check(any(key in w for w in prefs.warnings), f"…and says so ({key})")
    check(laya_guard.guard_mode(prefs) == "full", "the call guard stays at full for that repo")

    home = ROOT / "home"
    home.mkdir(parents=True, exist_ok=True)
    (home / "settings.json").write_text(json.dumps({"laya.riskDestroy": 0.7}), encoding="utf-8")
    prefs = settings_store.load(project("plain", {}))
    check(prefs.get("laya.riskDestroy") == 0.7, "the user's own file still sets laya.riskDestroy")
    (home / "settings.json").unlink()

    trusted = project("trusted", {"laya.callGuard": "rules"})
    settings_store.trust_project(str(trusted))
    prefs = settings_store.load(trusted)
    check(prefs.get("laya.callGuard") == "rules", "a trusted folder may set it")


def test_shell_policy_writes() -> None:
    cwd = str(ROOT / "cloned")
    prefs = {"laya.callGuard": "rules"}
    for command in (
        "printf '{}' > .sierralab/settings.json",
        "echo '{}' >> .sierralab/settings.local.json",
        "cat evil.json | tee .cursor/mcp.json",
        "cp /tmp/x .vscode/mcp.json",
        "mv staged.json mcp.json",
        "sed -i '' 's/ask/allow/' .claude/settings.json",
    ):
        score = laya_guard.assess_call("shell", {"command": command}, cwd=cwd, preferences=prefs).score
        check(score >= 0.9, f"{command!r} scores {score:.2f} (at the destructive line)")
    for path in (".cursor/mcp.json", ".vscode/mcp.json", "mcp.json", "backend/mcp_servers.json"):
        check(laya_guard.assess_call("file_write", {"path": path, "content": "{}"}, cwd=cwd, preferences=prefs).score >= 0.9, f"file_write {path} is a policy file")
    for command in ("pytest -q > out.txt", "ls > files.txt", "echo ok 2>&1", "cat .mcp.json"):
        check(laya_guard.assess_call("shell", {"command": command}, cwd=cwd, preferences=prefs).score < 0.8, f"{command!r} is unchanged (below the line)")


def test_generated_skill_gate() -> None:
    banned = {
        "import socket": "socket",
        "from os import system": "os.system",
        "import os\nos.posix_spawn('x', [], {})": "os.posix_spawn",
        "import asyncio\nasyncio.create_subprocess_exec('ls')": "create_subprocess_exec",
        "__import__('subprocess')": "__import__",
        "import os\ngetattr(os, 'popen')('id')": "getattr",
        "import ctypes": "ctypes",
        "import importlib\nimportlib.import_module('subprocess')": "import_module",
    }
    for source, what in banned.items():
        reason = skill_validation_hooks.find_banned_usage(ast.parse(source))
        check(reason is not None, f"banned: {what} ({reason})")
    for source in ("import json\nimport re\nimport pathlib", "import urllib.request", "import os\nos.path.join('a', 'b')"):
        check(skill_validation_hooks.find_banned_usage(ast.parse(source)) is None, f"allowed: {source.splitlines()[-1]!r}")


def test_vault_covers_generated_and_plugin_tools() -> None:
    import acp_server
    from strands.interventions import Deny

    blocked = vault_proxy.VaultStatus(enabled=True, ok=False, addr="http://127.0.0.1:1", reason="the vault is unreachable and vault.egressPolicy is 'deny'")
    import acp_approval  # the approval gate lives here since acp_server was split

    real_generated, real_current = acp_approval.current_generated_tool_names, acp_server.settings_store.current
    acp_approval.current_generated_tool_names = lambda: {"scrape_prices"}
    acp_server.settings_store.current = lambda cwd: acp_server.settings_store.Settings()
    original = vault_proxy.reprobe
    acp_server.vault_proxy.reprobe = lambda status, **kw: status
    try:
        plugins = SimpleNamespace(requires_approval=lambda name: True if name == "github_create_issue" else None)
        handler = acp_server._ApprovalHandler(str(ROOT), "auto", vault=blocked, plugins=plugins)
        event = lambda name: SimpleNamespace(tool_use={"name": name, "input": {}, "toolUseId": "t"}, agent=None)  # noqa: E731
        check(isinstance(handler.before_tool_call(event("scrape_prices")), Deny), "a generated skill is refused while the vault blocks egress")
        check(isinstance(handler.before_tool_call(event("github_create_issue")), Deny), "an external plugin's tool is refused too")
        check(not isinstance(handler.before_tool_call(event("code_search")), Deny), "a read-only tool still runs")
        for name in ("a2a_client", "skill_forge"):
            check(isinstance(handler.before_tool_call(event(name)), Deny), f"{name} (remote agent / a researcher with http_request) is refused too")

        # Sub-agents have no gate for the refusal to reach: their kit loses the network tools instead.
        import sub_agent_common
        from strands import Agent
        from strands_tools import file_read
        from core_tools import http_request
        from harness_testing import ScriptedModel

        caller = Agent(model=ScriptedModel([]), tools=[file_read, http_request], callback_handler=None)
        names = lambda: {getattr(t, "tool_name", None) for t in sub_agent_common.resolve_tools(None, caller, "test")}  # noqa: E731
        vault_proxy._LAST = vault_proxy.VaultStatus(enabled=False)
        check("http_request" in names(), "with the vault off, a sub-agent may hold http_request")
        vault_proxy._LAST = blocked
        check("http_request" not in names() and "file_read" in names(), "with the vault refusing egress, a sub-agent's kit has no network tool")
        vault_proxy._LAST = None
    finally:
        acp_approval.current_generated_tool_names = real_generated
        acp_server.settings_store.current = real_current
        acp_server.vault_proxy.reprobe = original


def test_unified_memory_is_given_back() -> None:
    events: list[dict] = []
    unsubscribe = harness_events.subscribe(lambda kind, payload: events.append(payload) if kind == "memory" else None)
    try:
        # (a) a server that flips the flag but keeps the buffers
        with FakeOmlx({ARCH_WIRE: [text("x")]}) as fake:
            fake.leaky.add(ARCH_WIRE)
            manager = omlx_manager.OmlxManager(fake.base_url)
            manager.load(ARCH_WIRE)
            manager.unload(ARCH_WIRE, settle=0.3)
            check(manager.unreclaimed.get(ARCH_WIRE, 0) > 0, f"an unload that leaves the pool full is recorded ({manager.unreclaimed.get(ARCH_WIRE, 0) / omlx_manager.GB:.1f} GB held)")
        with FakeOmlx({ARCH_WIRE: [text("x")]}) as fake:
            manager = omlx_manager.OmlxManager(fake.base_url)
            manager.load(ARCH_WIRE)
            manager.unload(ARCH_WIRE, settle=0.3)
            check(not manager.unreclaimed, "a clean unload records nothing")

        # (b) an unload that fails is retried, not forgotten
        with FakeOmlx({ARCH_WIRE: [text("x")]}) as fake:
            table = op.LeaseTable(budget_for(fake))
            real_unload = table.budget.omlx.unload
            calls = {"n": 0}

            def flaky(model, **kw):
                calls["n"] += 1
                if calls["n"] == 1:
                    raise omlx_manager.OmlxError("timed out")
                return real_unload(model, **kw)

            table.budget.omlx.unload = flaky
            table.acquire(ARCH)
            table.release(ARCH)
            check(table.stranded() == [ARCH] and ARCH_WIRE in fake.loaded, "a failed unload leaves the model stranded (still resident, still ours)")
            check(any(e.get("event") == "lease_unload_failed" for e in events), "…and says so on the memory channel")
            table.release(ARCH)  # any later release retries
            check(table.stranded() == [] and ARCH_WIRE not in fake.loaded, "the next release retries it and the memory comes back")

        # (c) a lease whose invocation never reached AfterInvocation
        with FakeOmlx({ARCH_WIRE: [text("x")]}) as fake:
            table = op.LeaseTable(budget_for(fake))
            hooks = op.PrismLeaseHooks(ARCH, table=table)
            asyncio.run(hooks._before(SimpleNamespace(cancel=False)))
            asyncio.run(hooks._before(SimpleNamespace(cancel=False)))  # the first invocation's After never ran
            check(table.held() == {ARCH: 1}, f"the orphaned lease is released by the next invocation (held: {table.held()})")
            asyncio.run(hooks._after(SimpleNamespace()))
            check(table.held() == {} and ARCH_WIRE not in fake.loaded, "and the model leaves memory when the last holder goes")

        # (d) process exit
        with FakeOmlx({ARCH_WIRE: [text("x")]}) as fake:
            table = op.LeaseTable(budget_for(fake))
            table.acquire(ARCH)
            check(ARCH_WIRE in fake.loaded, "held at exit…")
            check(table.release_all() == [ARCH] and ARCH_WIRE not in fake.loaded, "…release_all unloads what the table loaded")
    finally:
        unsubscribe()


def test_intervention_queue_never_raises() -> None:
    from daemon import grpc_bridge

    async def run() -> None:
        queue: asyncio.Queue = asyncio.Queue(maxsize=2)
        for n in range(5):
            grpc_bridge.offer(queue, n)
        check(queue.qsize() == 2, "a full reader queue keeps its bound")
        check([queue.get_nowait(), queue.get_nowait()] == [3, 4], "…and the newest events")

    asyncio.run(run())


def main() -> None:
    test_trust_gate()
    test_shell_policy_writes()
    test_generated_skill_gate()
    test_vault_covers_generated_and_plugin_tools()
    test_unified_memory_is_given_back()
    test_intervention_queue_never_raises()
    print("\nphase 11 hardening: all checks passed")


if __name__ == "__main__":
    main()

"""Settings and trust as a native front door reads and edits them (docs/app.md §6.4, phase B4) — offline, through the
real ACP server.

1. **the app's own settings are the user's** — `app.*` set in a project file is ignored (and says so); set in the user
   file it applies; writing it to a project scope is refused;
2. **settings through the server** — `settings/schema` and `settings/get` describe them; `settings/set` and the rule
   methods write through `settings_store` (its validation, its trust gate) and are audited;
3. **a trust card shows what trust would turn on** — the project's ignored settings and allow rules, its MCP servers
   held back with their exact commands; **`trust/grant` takes the person's confirmation**, is audited, and is not
   something a tool can do;
4. **before auto** — `auto/readiness` says what still protects the project;
5. **auto and edit need confirmation** from a client that promised to ask — refused without it, audited with it — and
   nothing changes for clients that promised nothing;
6. **doctor** answers through the server.

    backend/.venv/bin/python _settings_trust_smoke_test.py
"""

from __future__ import annotations

import asyncio
import json
import os
import sys

os.environ["CORDATA_PRISM"] = "off"

import provider_settings  # noqa: F401,E402

import acp  # noqa: E402

import settings_store  # noqa: E402
import state_store  # noqa: E402
from _acp_harness_smoke_test import ROOT, fresh_server, laya_answer, orchestrator_model, project, text  # noqa: E402
from harness_testing import check  # noqa: E402

ROUTE = laya_answer("code", {"code": 0.97, "answer": 0.03})


def user_settings(data: dict) -> None:
    path = settings_store.user_dir() / "settings.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data))


async def ext(server, method: str, sid: str, **params) -> dict:
    return await server.ext_method(f"_cordata/{method}", {"sessionId": sid, **params})


async def main() -> None:
    cwd = project("trust-me")
    (cwd / ".cordata").mkdir()
    (cwd / ".cordata" / "settings.json").write_text(json.dumps({
        "app": {"theme": "high-contrast"},
        "permissions": {"defaultMode": "auto", "allow": ["shell(make deploy)"]},
        "preflight": {"tests": "make test"},
        "vault": {"enabled": False},
    }))
    (cwd / ".mcp.json").write_text(json.dumps({"mcpServers": {"repo-tool": {"command": sys.executable, "args": ["-c", "print('hi')"], "env": {"REPO_TOKEN": "x"}}}}))
    user_settings({"app": {"density": "compact"}})

    orchestrator_model([text("hi")])
    server, _ = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(cwd))).session_id

    print("\n=== 1. the app's settings are the user's ===")
    got = await ext(server, "settings/get", sid)
    check(got["values"].get("app.density") == "compact" and "app.theme" not in got["values"], f"user app.* applies; the project's is ignored ({ {k: v for k, v in got['values'].items() if k.startswith('app.')} })")
    check(any("app.theme" in w and "user file" in w for w in got["warnings"]), "…and the project's is reported as ignored")
    refused = await ext(server, "settings/set", sid, scope="project", key="app.theme", value="night")
    check(not refused["ok"] and "user file" in refused["reason"], f"writing app.* to a project is refused ({refused})")

    print("\n=== 2. settings through the server ===")
    schema = await ext(server, "settings/schema", sid)
    keys = {row["key"]: row for row in schema["keys"]}
    check(keys["app.theme"]["userOnly"] and keys["vault.enabled"]["trustGated"] and keys["temperature"]["kind"] == "enum", "the schema says what each key is, which are gated and which are user-only")
    done = await ext(server, "settings/set", sid, scope="user", key="tui.showLaya", value=False)
    check(done["ok"] and done["applied"], f"a user setting is written and applies ({done})")
    bad = await ext(server, "settings/set", sid, scope="user", key="temperature", value="scorching")
    check(not bad["ok"], f"validation is the backend's: a bad value is refused ({bad.get('reason')})")
    added = await ext(server, "settings/rules/add", sid, kind="deny", rule="shell(rm -rf *)", scope="user")
    check(added["ok"] and any(r["text"] == "shell(rm -rf *)" and r["kind"] == "deny" for r in (await ext(server, "settings/get", sid))["rules"]), "a rule is added")
    removed = await ext(server, "settings/rules/remove", sid, rule="shell(rm -rf *)")
    check(removed["ok"], "…and removed")
    audit = [e["payload"] for e in state_store.recent_events(kind="settings", limit=20)]
    check({"set", "rule_add", "rule_remove"} <= {a["action"] for a in audit}, "every write is in the ledger")

    print("\n=== 3. trust cards ===")
    status = await ext(server, "trust/status", sid)
    check(not status["trusted"], "the folder is not trusted")
    check(any("defaultMode" in w for w in status["ignored"]) and any("vault.enabled" in w for w in status["ignored"]), f"the card lists the settings trust would turn on ({len(status['ignored'])})")
    check("shell(make deploy)" in status["allowRules"], "…and the allow rules")
    plugin = next((p for p in status["blockedPlugins"] if p["name"] == "repo-tool"), None)
    check(plugin is not None and plugin["command"] == sys.executable and plugin["args"] == ["-c", "print('hi')"] and plugin["envKeys"] == ["REPO_TOKEN"], f"…and the MCP server it would start, with its exact command ({plugin})")
    check(status["projectSettings"].get("permissions.defaultMode") == "auto", "…including the mode the project wants to start in")
    unconfirmed = await ext(server, "trust/grant", sid)
    check(not unconfirmed["ok"] and unconfirmed.get("needs") == "confirm_trust", "trust needs the person's confirmation")
    granted = await ext(server, "trust/grant", sid, confirmed=True)
    check(granted["ok"] and granted["trusted"] and settings_store.current(str(cwd)).trusted, "confirmed, the folder is trusted")
    check(any(e["payload"].get("action") == "grant" for e in state_store.recent_events(kind="trust", limit=5)), "…and the grant is in the ledger")
    tools = [getattr(t, "tool_name", "") for t in server._sessions[sid].agent.tool_registry.registry.values()] if hasattr(server._sessions[sid].agent, "tool_registry") else []
    check(not any("trust" in name for name in tools), "no tool can trust a folder (it is an ext method only)")

    print("\n=== 4. before auto ===")
    ready = await ext(server, "auto/readiness", sid)
    check(ready["checkpoints"]["enabled"] and "viable" in ready["checkpoints"] and ready["stillAsks"] and "denyRules" in ready, f"readiness says what still protects the project ({ready['checkpoints']})")

    print("\n=== 5. auto and edit need confirmation from a client that asks ===")
    careful, _ = fresh_server(ROUTE)
    caps = acp.schema.ClientCapabilities(field_meta={"cordata": {"confirmModes": True}})
    await careful.initialize(protocol_version=acp.PROTOCOL_VERSION, client_capabilities=caps)
    other = project("careful")
    csid = (await careful.new_session(cwd=str(other))).session_id
    try:
        await careful.set_session_mode(csid, "auto")
        refused = None
    except acp.RequestError as error:
        refused = (error.to_error_obj().get("data") or {}).get("needs")
    check(refused == "confirm_mode", f"auto without a confirmation is refused ({refused})")
    await careful.set_session_mode(csid, "edit", field_meta={"cordata": {"confirmed": True}})
    check(careful._sessions[csid].handler.mode == "edit", "with the confirmation it switches")
    modes = [e["payload"] for e in state_store.recent_events(kind="mode", limit=5)]
    check(any(m["to"] == "edit" and m["confirmed"] for m in modes), "…and the switch is in the ledger")
    await careful.set_session_mode(csid, "plan")
    check(careful._sessions[csid].handler.mode == "plan", "tightening (to plan) never needs it")
    await server.set_session_mode(sid, "auto")
    check(server._sessions[sid].handler.mode == "auto", "a client that promised nothing is unchanged")

    print("\n=== 6. doctor ===")
    doc = await ext(server, "doctor", sid)
    check(isinstance(doc["checks"], list) and doc["checks"] and all("level" in c for c in doc["checks"]), f"doctor answers ({len(doc['checks'])} checks)")
    print("\nALL OK")


if __name__ == "__main__":
    asyncio.run(main())

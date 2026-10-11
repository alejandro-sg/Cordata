"""The token diet and the approval/receipt notifications, through the real ACP server object (offline).

Same rig as `_acp_harness_smoke_test.py` — real `_build_session`, router, hooks, specialists — with scripted models.

1. **per-call overhead** — the first model call of a plain turn is sent with a fraction of the tool schema a
   session otherwise carries (`enable_tools` + the core kit, not the 19 rarely-used tools), measured in bytes of the
   specs the model was actually handed; the full kit is back in the registry after the turn.
2. **nothing is lost** — a turn that mentions a hidden tool's subject gets it from the start; the agent can ask for
   one with `enable_tools`; `tokens`-style switches (`context.coldTools: false`) restore the old behaviour.
3. **a huge tool result** is capped in what the model reads next, with its head and tail kept (with `context.offload: false`;
   by default it is set aside whole and read back by slice — see `_offload_acp_smoke_test.py`).
4. **the approval card has its context** — the permission request's `_meta.sierralab` carries what the person
   asked, what the agent said just before the call, and the project, so a remote front door can say *why*.
5. **a turn announces what it cost** — `_sierralab/turn`: seconds, tool calls, tokens in and out. That is what the
   Discord receipt (and any other client's status line) is built from, with no model call of its own.

    backend/.venv/bin/python _diet_acp_smoke_test.py
"""

import asyncio
import json
import sys

import acp

import context_diet
from _acp_harness_smoke_test import ROOT, call, check, fresh_server, laya_answer, orchestrator_model, project, settle, text, MODELS  # noqa: F401

ROUTE = laya_answer("code", {"code": 0.97, "answer": 0.03})


def spec_bytes(specs) -> int:
    return sum(len(json.dumps(spec)) for spec in specs or [])


async def test_a_plain_turn_sends_a_fraction_of_the_schema() -> None:
    cwd = project("diet-plain")
    seen: dict = {}

    def reply(messages, system_prompt, tool_specs):
        seen["names"] = [spec["name"] for spec in tool_specs or []]
        seen["bytes"] = spec_bytes(tool_specs)
        seen["prompt"] = str(system_prompt)
        return text("hello")

    orchestrator_model([reply])
    server, conn = fresh_server(ROUTE)
    created = await server.new_session(cwd=str(cwd))
    state = server._sessions[created.session_id]
    full = spec_bytes(state.agent.tool_registry.get_all_tool_specs())
    await server.prompt(created.session_id, [acp.text_block("explain what this project does")])
    await settle()

    cold = set(context_diet.COLD_TOOLS)
    check(not (cold & set(seen["names"])), f"the rarely-used tools are not sent on a plain turn ({sorted(cold & set(seen['names'])) or 'none sent'})")
    check({"shell", "editor", "file_read", "file_write", "python_repl", "architect", "debugger", "enable_tools"} <= set(seen["names"]), "the core kit, the specialists and the loader are")
    saved = 1 - seen["bytes"] / full
    check(saved >= 0.45, f"the schema a model call carries fell by {saved:.0%} ({full:,} → {seen['bytes']:,} bytes ≈ {(full - seen['bytes']) // 4:,} tokens per call)")
    check("Tools not loaded this turn" in seen["prompt"] and "enable_tools" in seen["prompt"], "and the prompt says what is not loaded and how to load it")
    listed_skills = seen["prompt"].count("<name>")
    check(len(seen["prompt"]) < 7_000 and 1 <= listed_skills <= 4, f"the system prompt is {len(seen['prompt']):,} characters with {listed_skills} skills listed, not the ~21,000 with all 49")
    after = {spec["name"] for spec in state.agent.tool_registry.get_all_tool_specs()}
    check(cold <= after and "Tools not loaded" not in str(state.agent.system_prompt), "after the turn every tool is back and the prompt is clean")
    print(f"   per-call tool schema: {full:,} → {seen['bytes']:,} bytes")


async def test_nothing_is_taken_away() -> None:
    cwd = project("diet-keeps")
    seen: list[list[str]] = []

    def watch(step):
        def reply(messages, system_prompt, tool_specs):
            seen.append([spec["name"] for spec in tool_specs or []])
            return step

        return reply

    orchestrator_model([watch(call("enable_tools", {"names": ["context_usage"]})), watch(text("loaded"))])
    server, conn = fresh_server(ROUTE)
    created = await server.new_session(cwd=str(cwd))
    await server.prompt(created.session_id, [acp.text_block("tidy up the readme")])
    await settle()
    check("context_usage" not in seen[0] and "context_usage" in seen[1], "the agent can load a hidden tool and has it on its very next step")

    seen.clear()
    orchestrator_model([watch(text("ok"))])
    server, conn = fresh_server(ROUTE)
    created = await server.new_session(cwd=str(cwd))
    await server.prompt(created.session_id, [acp.text_block("how much of the context window is used?")])
    await settle()
    check("context_usage" in seen[0] and "http_request" not in seen[0], "a message about context usage has that tool from the start, and not an unrelated tool")

    seen.clear()
    off = project("diet-off")
    (off / ".sierralab").mkdir()
    (off / ".sierralab" / "settings.json").write_text(json.dumps({"context.coldTools": False}))
    settings_trust(off)
    orchestrator_model([watch(text("ok"))])
    server, conn = fresh_server(ROUTE)
    created = await server.new_session(cwd=str(off))
    await server.prompt(created.session_id, [acp.text_block("tidy up the readme")])
    await settle()
    check({"context_usage", "http_request", "set_temperature"} <= set(seen[0]), "context.coldTools=false (set by the person, in a trusted folder) sends every tool, as before")


def settings_trust(path) -> None:
    import settings_store

    settings_store.trust_project(str(path))


async def test_a_huge_result_is_capped() -> None:
    """The fallback for `context.offload: false` — with offloading on (the default) the result is set aside whole instead,
    and `_offload_acp_smoke_test.py` covers that."""
    cwd = project("diet-cap")
    (cwd / ".sierralab").mkdir()
    (cwd / ".sierralab" / "settings.json").write_text(json.dumps({"context.offload": False}))
    settings_trust(cwd)
    (cwd / "big.log").write_text("HEADLINE\n" + "noise line\n" * 20_000 + "FINAL: exit 1\n")
    seen: dict = {}

    def after(messages, system_prompt, tool_specs):
        from harness_testing import last_text

        seen["result"] = last_text(messages)
        return text("read it")

    orchestrator_model([call("file_read", {"path": "big.log", "mode": "view"}), after])
    server, conn = fresh_server(ROUTE)
    created = await server.new_session(cwd=str(cwd))
    await server.prompt(created.session_id, [acp.text_block("read big.log")])
    await settle()
    result = seen["result"]
    check(len(result) < context_diet.DEFAULT_TOOL_OUTPUT_CHARS + 1500, f"the file_read result is capped in what the model reads next ({len(result):,} characters of a 220,000-character file)")
    check("HEADLINE" in result and "FINAL: exit 1" in result and "characters omitted to save tokens" in result, "head and tail are kept and the cut is announced")


class Recording:
    """An ACP client that records the permission requests it is sent (with their `_meta`) and every extension notification."""

    def __init__(self, conn) -> None:
        self.conn = conn
        self.asks: list[dict] = []

    async def request_permission(self, session_id, tool_call, options, **kwargs):
        self.asks.append({"tool_call": tool_call, "options": options, "meta": kwargs.get("sierralab") or kwargs.get("field_meta") or kwargs})
        return acp.schema.RequestPermissionResponse(outcome=acp.schema.AllowedOutcome(outcome="selected", option_id="allow"))

    def __getattr__(self, name):
        return getattr(self.conn, name)


async def test_the_approval_card_has_its_context_and_the_turn_its_receipt() -> None:
    cwd = project("diet-card")
    # One turn: the model narrates, then asks for a shell call that needs approval — in one assistant message.
    orchestrator_model([call("shell", {"command": "rm -rf build"}, "card-1", says="I'll clear the old build output first, then rebuild."), text("done")])
    server, conn = fresh_server(laya_answer("code", {"code": 0.97, "answer": 0.03}, risk=0.95, risk_conf=0.9))
    recording = Recording(conn)
    server._conn = recording
    created = await server.new_session(cwd=str(cwd))
    await server.prompt(created.session_id, [acp.text_block("clean the build directory and rebuild")])
    await settle()

    check(len(recording.asks) >= 1, "the shell call paused for a decision")
    meta = recording.asks[0]["meta"]
    check(isinstance(meta, dict) and meta.get("goal") == "clean the build directory and rebuild", f"the request carries what the person asked ({meta.get('goal') if isinstance(meta, dict) else meta!r})")
    check(meta.get("project") == str(cwd.resolve()) or meta.get("project") == str(cwd), f"…and the project it runs in ({meta.get('project')})")
    check(meta.get("said") == "I'll clear the old build output first, then rebuild.", f"…and what the agent said just before the call ({meta.get('said')!r})")
    check(meta.get("command") == "rm -rf build" and meta.get("tool") == "shell", "alongside the exact command")

    after = {spec["name"] for spec in server._sessions[created.session_id].agent.tool_registry.get_all_tool_specs()}
    check(set(context_diet.COLD_TOOLS) <= after and "Tools not loaded" not in str(server._sessions[created.session_id].agent.system_prompt), "a turn that paused for an approval and resumed still ends with every tool back and a clean prompt")

    turns = [params for method, params in conn.ext if method == "sierralab/turn"]
    check(len(turns) == 1, f"the turn announced what it cost, once ({len(turns)})")
    receipt = turns[0]
    check(receipt["stop"] == "end_turn" and receipt["seconds"] >= 0 and "tokensIn" in receipt and "tokensOut" in receipt and receipt["calls"] >= 1, f"seconds, stop reason, tokens and tool calls are in it ({receipt})")


async def main() -> None:
    await test_a_plain_turn_sends_a_fraction_of_the_schema()
    await test_nothing_is_taken_away()
    await test_a_huge_result_is_capped()
    await test_the_approval_card_has_its_context_and_the_turn_its_receipt()
    print("\nPASS: a plain turn carries a fraction of the tool schema, nothing is unavailable, a huge result is capped, the approval card knows why, and a turn reports what it cost")


if __name__ == "__main__":
    asyncio.run(main())
    sys.stdout.flush()

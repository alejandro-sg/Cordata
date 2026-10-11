"""Strands' `ContextOffloader`, the per-turn limits and native cancellation, through the real ACP server object (offline).

Same rig as `_diet_acp_smoke_test.py` — real `_build_session`, hooks, plugins, specialists — with scripted models.

1. **a big result is set aside, not cut** — the model reads a head+tail preview and a reference; the whole result is on
   disk (private: 0700 dir, 0600 file); `retrieve_offloaded_content` returns the slice it asks for; that read-back is
   fenced as untrusted data; the retrieval tool is not in the schema until something was offloaded.
2. **what must not be stored is not** — a registered credential in the output is masked before it reaches the disk.
3. **the knobs** — `context.offload: false` cuts instead (the old behaviour), `context.toolOutputChars` moves the threshold.
4. **what is already an answer is not offloaded** — the veto list; and files age out by time.
5. **limits** — `context.maxSteps` / `context.maxTurnUsage` pause a turn with a `limit_*` stop (ACP `max_turn_requests`)
   that says why, keeps the history valid and carries on at "continue" — across checkpoint resumes, which are separate
   Strands invocations.
6. **cancel** — `session/cancel` reaches the running agent (`agent.cancel()`), a tool that polls the signal stops, and
   a cancel that arrives while idle does not poison the next turn.
7. **proactive compression** — the history is summarised before the call that would overflow the window, not after the
   provider rejects it.
8. **the summary's author** — the fast model tier writes it when one is configured (`context.summaryModel`).

    backend/.venv/bin/python _offload_acp_smoke_test.py
"""

import asyncio
import json
import os
import re
import sys
import time

import acp
from strands import Agent, tool

import context_diet
import context_offload
import secret_guard
import settings_store
from _acp_harness_smoke_test import ROOT, call, check, fresh_server, laya_answer, orchestrator_model, project, settle, text, texts  # noqa: F401
from harness_testing import ScriptedModel, last_text
from prompt_security import SecretRedactionHooks, UntrustedToolResultHooks

ROUTE = laya_answer("code", {"code": 0.97, "answer": 0.03})
SECRET = "S3CR3T-credential-value-9f8e7d6c"


def trusted_project(name: str, settings: dict):
    cwd = project(name)
    (cwd / ".sierralab").mkdir()
    (cwd / ".sierralab" / "settings.json").write_text(json.dumps(settings))
    settings_store.trust_project(str(cwd))
    return cwd


def big_log(path, *, secret: bool = False) -> None:
    lines = ["HEADLINE: build started"] + [f"noise line {i}" for i in range(9_000)]
    lines[4_500] = "MIDDLE-NEEDLE-4242 value=17"
    if secret:
        lines[4_600] = f"leaked token {SECRET} in the middle"
        lines[-1] = f"tail also {SECRET}"
    lines.append("FINAL: exit 1")
    path.write_text("\n".join(lines) + "\n")


def stored_files(session_id: str):
    base = context_offload.root() / "".join(ch if ch.isalnum() or ch in "-_" else "_" for ch in session_id)
    return [p for p in base.rglob("*") if p.is_file()] if base.exists() else []


async def test_a_big_result_is_set_aside_and_readable() -> None:
    cwd = project("off-a")
    big_log(cwd / "big.log")
    seen: dict = {"specs": [], "preview": "", "read_back": ""}

    def after_read(messages, system_prompt, tool_specs):
        seen["specs"].append([s["name"] for s in tool_specs or []])
        seen["preview"] = last_text(messages)
        match = re.search(r"\[Stored references:\]\s+(\S+) \(", seen["preview"])
        seen["ref"] = match.group(1) if match else ""
        return call(context_offload.RETRIEVE_TOOL, {"reference": seen["ref"], "pattern": "NEEDLE"})

    def after_retrieve(messages, system_prompt, tool_specs):
        seen["read_back"] = last_text(messages)
        return text("the needle says value=17")

    def first(messages, system_prompt, tool_specs):
        seen["specs"].append([s["name"] for s in tool_specs or []])
        seen["prompt"] = str(system_prompt)
        return call("file_read", {"path": "big.log", "mode": "view"})

    orchestrator_model([first, after_read, after_retrieve])
    server, conn = fresh_server(ROUTE)
    created = await server.new_session(cwd=str(cwd))
    await server.prompt(created.session_id, [acp.text_block("what failed in big.log?")])
    await settle()

    preview = seen["preview"]
    check(context_offload.OFFLOADED_MARK in preview and "retrieve_offloaded_content" in preview, "a big result reaches the model as an offload notice with the way to read it back")
    check("HEADLINE: build started" in preview and "FINAL: exit 1" in preview, "the preview keeps the head and the tail (an exit code is at the end)")
    check("characters of the middle are in the stored copy" in preview and len(preview) < context_diet.DEFAULT_TOOL_OUTPUT_CHARS, f"…says what is only in the stored copy, and is small ({len(preview):,} characters of ~110,000)")
    check(seen["ref"], f"a reference is given ({seen['ref']})")

    check(context_offload.RETRIEVE_TOOL not in seen["specs"][0], "the retrieval tool is not in the first call's schema (nothing is set aside yet)")
    check(context_offload.RETRIEVE_TOOL in seen["specs"][1], "…and is the moment something is")
    check(context_offload.RETRIEVE_TOOL not in seen["prompt"], "the 'not loaded' listing never mentions it (the diet loads it by itself)")

    files = stored_files(created.session_id)
    check(len(files) == 1 and files[0].stat().st_size > 100_000, f"the whole result is on disk ({[f.stat().st_size for f in files]} bytes)")
    check(oct(files[0].stat().st_mode & 0o777) == "0o600", f"…readable by the owner only ({oct(files[0].stat().st_mode & 0o777)})")
    check(oct(context_offload.root().stat().st_mode & 0o777) == "0o700", f"…under a 0700 directory ({oct(context_offload.root().stat().st_mode & 0o777)})")
    check(files[0].parent.name == "offloader" and seen["ref"] in files[0].name, "…named by the reference")

    read_back = seen["read_back"]
    check("MIDDLE-NEEDLE-4242 value=17" in read_back, "retrieving with a pattern returns the line from the middle that was not in the preview")
    check("offloaded tool output (read back)" in read_back and "<<<" in read_back, "…fenced as untrusted data, like any tool output read from outside")

    texts_after = texts(conn)
    check("value=17" in texts_after, "the agent answered from it")


async def test_a_reference_is_not_a_path() -> None:
    cwd = project("off-trav")
    big_log(cwd / "big.log")
    seen: dict = {"reads": []}
    sessions: dict = {}

    def attempt(reference):
        def reply(messages, system_prompt, tool_specs):
            if "sid" in sessions:
                base = context_offload.root() / sessions["sid"]
                (base / "canary.txt").write_text("CANARY-SIBLING")
                (context_offload.root() / "canary-root.txt").write_text("CANARY-ROOT")
            return call(context_offload.RETRIEVE_TOOL, {"reference": reference})

        return reply

    follow_ups = ["../../canary-root.txt", str(context_offload.root() / "canary-root.txt")]

    def record(messages, system_prompt, tool_specs):
        seen["reads"].append(last_text(messages))
        return call(context_offload.RETRIEVE_TOOL, {"reference": follow_ups.pop(0)}) if follow_ups else text("done")

    orchestrator_model([call("file_read", {"path": "big.log", "mode": "view"}), attempt("../canary.txt"), record, record, record])
    server, conn = fresh_server(ROUTE)
    created = await server.new_session(cwd=str(cwd))
    sessions["sid"] = "".join(ch if ch.isalnum() or ch in "-_" else "_" for ch in created.session_id)
    await server.prompt(created.session_id, [acp.text_block("read big.log, then try some references")])
    await settle()
    blob = "\n".join(seen["reads"])
    check(seen["reads"] and "CANARY" not in blob, f"a reference that climbs out of the store or names a path returns nothing from outside it ({len(seen['reads'])} attempts)")


async def test_a_credential_never_reaches_the_disk() -> None:
    secret_guard.register_secret(SECRET)
    cwd = project("off-b")
    big_log(cwd / "big.log", secret=True)
    seen: dict = {}

    def after_read(messages, system_prompt, tool_specs):
        seen["preview"] = last_text(messages)
        return text("done")

    orchestrator_model([call("file_read", {"path": "big.log", "mode": "view"}), after_read])
    server, conn = fresh_server(ROUTE)
    created = await server.new_session(cwd=str(cwd))
    await server.prompt(created.session_id, [acp.text_block("read big.log")])
    await settle()
    files = stored_files(created.session_id)
    stored = b"".join(f.read_bytes() for f in files)
    check(files and SECRET.encode() not in stored and secret_guard.REDACTED.encode() in stored, "a registered credential in a big result is masked in the stored copy")
    check(SECRET not in seen["preview"], "…and in what the model reads")


async def test_the_knobs() -> None:
    # offload off: cut, as before — no retrieval tool, no files.
    off = trusted_project("off-c", {"context.offload": False})
    big_log(off / "big.log")
    seen: dict = {}

    def after(messages, system_prompt, tool_specs):
        seen["result"] = last_text(messages)
        seen["specs"] = [s["name"] for s in tool_specs or []]
        return text("ok")

    orchestrator_model([call("file_read", {"path": "big.log", "mode": "view"}), after])
    server, conn = fresh_server(ROUTE)
    created = await server.new_session(cwd=str(off))
    await server.prompt(created.session_id, [acp.text_block("read big.log")])
    await settle()
    check(context_diet.CAPPED_MARK in seen["result"] and context_offload.OFFLOADED_MARK not in seen["result"], "context.offload=false: the result is cut head+tail, as before")
    check(not stored_files(created.session_id) and context_offload.RETRIEVE_TOOL not in seen["specs"], "…nothing is written and the retrieval tool does not exist")

    # a lower threshold offloads what the default would let through.
    mid = project("off-d")
    (mid / "mid.log").write_text("x" * 5_000 + "\nTAIL-MARK\n")
    results: dict = {}

    def grab(label):
        def reply(messages, system_prompt, tool_specs):
            results[label] = last_text(messages)
            return text("ok")

        return reply

    orchestrator_model([call("file_read", {"path": "mid.log", "mode": "view"}), grab("default")])
    server, conn = fresh_server(ROUTE)
    created = await server.new_session(cwd=str(mid))
    await server.prompt(created.session_id, [acp.text_block("read mid.log")])
    check(context_offload.OFFLOADED_MARK not in results["default"], "a 5,000-character result is under the default limit and stays whole")

    low = trusted_project("off-e", {"context.toolOutputChars": 2_000})
    (low / "mid.log").write_text("x" * 5_000 + "\nTAIL-MARK\n")
    orchestrator_model([call("file_read", {"path": "mid.log", "mode": "view"}), grab("low")])
    server, conn = fresh_server(ROUTE)
    created = await server.new_session(cwd=str(low))
    await server.prompt(created.session_id, [acp.text_block("read mid.log")])
    check(context_offload.OFFLOADED_MARK in results["low"] and "TAIL-MARK" in results["low"], "context.toolOutputChars=2000 moves the threshold: the same result is offloaded, tail kept")


def test_the_veto_and_the_housekeeping() -> None:
    seen: dict = {}

    @tool
    def big_output() -> str:
        """Returns a lot."""
        return "line\n" * 6_000

    @tool(name="orchestrate")
    def swarm_answer() -> str:
        """The answer itself."""
        return "answer\n" * 6_000

    base = ROOT / "unit-offload"
    storage = context_offload.PrivateFileStorage(base)
    offloader = context_offload.HarnessOffloader(storage, limit_chars=lambda: 6_000)
    model = ScriptedModel([call("big_output"), call("orchestrate"), lambda m, s, t: seen.setdefault("last", last_text(m)) and text("done")])
    agent = Agent(
        model=model,
        tools=[big_output, swarm_answer],
        plugins=[offloader],
        hooks=[UntrustedToolResultHooks(), SecretRedactionHooks(), context_diet.ContextDiet(None, scope_tools=False)],
        callback_handler=None,
    )
    agent("go")
    results = [
        block["toolResult"]
        for message in agent.messages
        for block in message.get("content") or []
        if isinstance(block, dict) and "toolResult" in block
    ]
    check(context_offload.is_offloaded(results[0]), "an ordinary tool's big result is offloaded")
    swarm_text = "".join(part.get("text", "") for part in results[1]["content"])
    check(not context_offload.is_offloaded(results[1]) and context_diet.CAPPED_MARK in swarm_text, "a veto-listed tool's result is not offloaded — it is the answer (the diet's cap is its only limit)")
    check(offloader.offloaded == 1 and offloader.offloaded_chars > 20_000, f"the offloader counts what it set aside ({offloader.offloaded} result, {offloader.offloaded_chars:,} characters)")

    # head_tail
    cut = context_offload.head_tail("A" * 100 + "M" * 1000 + "Z" * 100, 300)
    check(cut.startswith("A") and cut.endswith("Z") and "of the middle are in the stored copy" in cut and len(cut) < 450, "head_tail keeps both ends")
    check(context_offload.head_tail("short", 300) == "short", "…and leaves short text alone")

    # prune: by time, not by loop cycle
    old = base / "old-session"
    new = base / "new-session"
    for directory, age_days in ((old, 30), (new, 1)):
        (directory / "offloader").mkdir(parents=True)
        file = directory / "offloader" / "x"
        file.write_bytes(b"x")
        stamp = time.time() - age_days * 86400
        os.utime(file, (stamp, stamp))
        os.utime(directory / "offloader", (stamp, stamp))
        os.utime(directory, (stamp, stamp))
    removed = context_offload.prune(base=base)
    check(removed == 1 and not old.exists() and new.exists(), "prune removes a session untouched for a week and keeps a recent one")

    # a conversation that already holds an offload loads the retrieval tool from its first turn (a resumed session)
    diet = context_diet.ContextDiet(None)
    holder = type("A", (), {"messages": [{"role": "user", "content": [{"toolResult": {"toolUseId": "t", "status": "success", "content": [{"text": "[Offloaded: 1 blocks, ~9,000 tokens]\n…"}]}}]}]})()
    check(diet._history_has_offloads(holder), "a resumed conversation that holds an offload is recognised")


async def test_limits_pause_a_turn_and_it_continues() -> None:
    cwd = trusted_project("lim-a", {"context.maxSteps": 3})
    for index in range(12):
        (cwd / f"f{index}.txt").write_text(f"file {index}\n")
    reads = [call("file_read", {"path": f"f{index}.txt", "mode": "view"}) for index in range(12)]
    orchestrator_model(reads)
    server, conn = fresh_server(ROUTE)
    created = await server.new_session(cwd=str(cwd))
    response = await server.prompt(created.session_id, [acp.text_block("read every file")])
    await settle()
    model = __import__("_acp_harness_smoke_test").MODELS["orchestrator"]
    check(response.stop_reason == "max_turn_requests", f"a turn over context.maxSteps stops with ACP's max_turn_requests ({response.stop_reason})")
    check(model.index <= 4, f"…after about 3 model calls, not 12 ({model.index})")
    said = texts(conn)
    check("Paused after 3 steps" in said and "continue" in said and "context.maxSteps" in said, "…and says why, that nothing is lost, and what to do")
    messages = server._sessions[created.session_id].agent.messages
    uses = sum(1 for m in messages for b in m.get("content") or [] if isinstance(b, dict) and "toolUse" in b)
    results = sum(1 for m in messages for b in m.get("content") or [] if isinstance(b, dict) and "toolResult" in b)
    check(uses == results, f"…the history is valid: every tool call has its result ({uses}/{results})")

    # carry on
    model.steps = [text("picked up where I stopped")]
    model.index = 0
    response = await server.prompt(created.session_id, [acp.text_block("continue")])
    await settle()
    check(response.stop_reason == "end_turn" and "picked up where I stopped" in texts(conn), "'continue' carries on, with a fresh budget")

    # no cap
    free = trusted_project("lim-b", {"context.maxSteps": 0})
    for index in range(8):
        (free / f"f{index}.txt").write_text(f"file {index}\n")
    orchestrator_model([call("file_read", {"path": f"f{index}.txt", "mode": "view"}) for index in range(8)] + [text("read them all")])
    server, conn = fresh_server(ROUTE)
    created = await server.new_session(cwd=str(free))
    response = await server.prompt(created.session_id, [acp.text_block("read every file")])
    check(response.stop_reason == "end_turn" and "read them all" in texts(conn), "context.maxSteps=0 turns the cap off")

    # the default is not so tight that an ordinary multi-step turn notices it
    plain = project("lim-c")
    for index in range(8):
        (plain / f"f{index}.txt").write_text(f"file {index}\n")
    orchestrator_model([call("file_read", {"path": f"f{index}.txt", "mode": "view"}) for index in range(8)] + [text("all eight")])
    server, conn = fresh_server(ROUTE)
    created = await server.new_session(cwd=str(plain))
    response = await server.prompt(created.session_id, [acp.text_block("read every file")])
    check(response.stop_reason == "end_turn" and "all eight" in texts(conn), "an eight-step turn runs under the default cap")


async def test_a_token_budget_pauses_a_turn() -> None:
    cwd = trusted_project("lim-d", {"context.maxSteps": 0, "context.maxTurnUsage": 2_500})
    for index in range(10):
        (cwd / f"f{index}.txt").write_text(f"file {index}\n")
    orchestrator_model([call("file_read", {"path": f"f{index}.txt", "mode": "view"}, tokens=1_000) for index in range(10)])
    server, conn = fresh_server(ROUTE)
    created = await server.new_session(cwd=str(cwd))
    response = await server.prompt(created.session_id, [acp.text_block("read every file")])
    await settle()
    model = __import__("_acp_harness_smoke_test").MODELS["orchestrator"]
    check(response.stop_reason == "max_turn_requests" and model.index <= 4, f"a turn over context.maxTurnUsage stops (after {model.index} of 10 model calls)")
    check("Paused after 2,500 tokens" in texts(conn), "…and says it was the token budget")


async def test_cancel_reaches_the_running_agent() -> None:
    cwd = project("cancel-a")
    state: dict = {"saw_signal": False}

    @tool(context=True)
    async def waiter(tool_context) -> str:
        """Waits, looking at the cancel signal as a well-behaved long-running tool does."""
        for _ in range(200):
            if tool_context.cancel_signal.is_set():
                state["saw_signal"] = True
                return "stopped early"
            await asyncio.sleep(0.05)
        return "ran to the end"

    orchestrator_model([call("waiter", {}), text("never reached")])
    server, conn = fresh_server(ROUTE)
    created = await server.new_session(cwd=str(cwd))
    server._sessions[created.session_id].agent.tool_registry.register_tool(waiter)

    # a cancel while idle must not poison the next turn
    await server.cancel(created.session_id)
    check(created.session_id not in server._streaming, "nothing is running: a cancel only marks the session")

    started = time.monotonic()
    running = asyncio.create_task(server.prompt(created.session_id, [acp.text_block("wait for it")]))
    await asyncio.sleep(0.5)
    check(created.session_id in server._streaming, "the running agent is registered while its turn streams")
    await server.cancel(created.session_id)
    response = await asyncio.wait_for(running, 10)
    took = time.monotonic() - started
    check(response.stop_reason == "cancelled", f"the turn ends cancelled ({response.stop_reason})")
    check(state["saw_signal"] and took < 5, f"…and a tool that polls Strands' cancel signal saw it and stopped ({took:.1f}s, not the 10s it would have taken)")
    check(created.session_id not in server._streaming, "…and the agent is unregistered afterwards")

    messages = server._sessions[created.session_id].agent.messages
    uses = sum(1 for m in messages for b in m.get("content") or [] if isinstance(b, dict) and "toolUse" in b)
    results = sum(1 for m in messages for b in m.get("content") or [] if isinstance(b, dict) and "toolResult" in b)
    check(uses == results, f"the history is valid after a cancel: every tool call has its result ({uses}/{results})")

    # the next turn is not cancelled by what is left of this one
    model = __import__("_acp_harness_smoke_test").MODELS["orchestrator"]
    model.steps = [text("a fresh turn")]
    model.index = 0
    response = await server.prompt(created.session_id, [acp.text_block("hello again")])
    await settle()
    check(response.stop_reason == "end_turn" and "a fresh turn" in texts(conn), "the next turn runs normally")
    check(not server._sessions[created.session_id].agent.cancel_signal.is_set(), "…and leaves no stale cancel signal behind")


def test_history_is_summarised_before_the_window_overflows() -> None:
    """`proactive_compression` (agent_common.build_conversation_manager): the summary is written *before* the call that
    would have been rejected, so the rejected round trip is never paid for."""
    import agent_common

    model = ScriptedModel([text("SUMMARY: the earlier work, in brief"), text("answer on a small history")])
    model.update_config(context_window_limit=2_000)  # tokens; the history below is ~5,000
    history = []
    for index in range(12):
        history.append({"role": "user", "content": [{"text": f"question {index}: " + "words " * 150}]})
        history.append({"role": "assistant", "content": [{"text": f"reply {index}: " + "more " * 150}]})
    agent = Agent(model=model, messages=history, conversation_manager=agent_common.build_conversation_manager(), callback_handler=None)
    before = len(agent.messages)
    result = agent("next question")
    check(len(model.calls) == 2 and "SUMMARY" in str(agent.messages[0]), f"over 70% of the window, the history is summarised first, then the call is made ({len(model.calls)} model calls)")
    check(len(agent.messages) < before and str(result).strip().startswith("answer"), f"…the oldest 30% became one summary message ({before} to {len(agent.messages)} messages) and the turn answered")

    roomy = ScriptedModel([text("no summary needed")])
    roomy.update_config(context_window_limit=1_000_000)
    agent = Agent(model=roomy, messages=history[:], conversation_manager=agent_common.build_conversation_manager(), callback_handler=None)
    agent("next question")
    check(len(roomy.calls) == 1 and len(agent.messages) == len(history) + 2, "well under the window nothing is summarised")


def test_the_summary_can_come_from_the_fast_tier() -> None:
    """`context.summaryModel` / MODEL_TIER_FAST: compaction is a model call the person pays for; it is rewritten history,
    not new reasoning, so the fast tier writes it when one is configured — and nothing changes when none is."""
    import agent_common
    import settings
    import sub_agent_common

    def history():
        out = []
        for index in range(12):
            out.append({"role": "user", "content": [{"text": f"question {index}: " + "words " * 150}]})
            out.append({"role": "assistant", "content": [{"text": f"reply {index}: " + "more " * 150}]})
        return out

    def run(summarizer="fast"):
        main = ScriptedModel([text("answer from the main model")])
        main.update_config(context_window_limit=2_000)
        agent = Agent(model=main, messages=history(), conversation_manager=agent_common.build_conversation_manager(summarizer), callback_handler=None)
        agent("next question")
        return main, agent

    saved = (settings.MODEL_TIER_FAST, sub_agent_common.build_sub_agent_model)
    fast = ScriptedModel([text("SUMMARY written by the fast model")], model_id="fast-model")
    sub_agent_common.build_sub_agent_model = lambda tier=None, role=None: fast
    try:
        settings.MODEL_TIER_FAST = ""
        main_a = ScriptedModel([text("SUMMARY by the main model"), text("answer")])
        main_a.update_config(context_window_limit=2_000)
        agent = Agent(model=main_a, messages=history(), conversation_manager=agent_common.build_conversation_manager(), callback_handler=None)
        agent("next question")
        check(len(main_a.calls) == 2 and not fast.calls, "no fast tier configured: the session's own model writes the summary, as before")

        settings.MODEL_TIER_FAST = "fast-model"
        main, agent = run()
        check(len(fast.calls) == 1 and len(main.calls) == 1 and "fast model" in str(agent.messages[0]), "with a fast tier, the fast model writes the summary and the main model makes only its own call")

        fast.index, fast.calls = 0, []
        main_b = ScriptedModel([text("SUMMARY by the main model"), text("answer")])
        main_b.update_config(context_window_limit=2_000)
        agent = Agent(model=main_b, messages=history(), conversation_manager=agent_common.build_conversation_manager("main"), callback_handler=None)
        agent("next question")
        check(len(main_b.calls) == 2 and not fast.calls, "context.summaryModel=main keeps the summary on the session's own model even then")
    finally:
        settings.MODEL_TIER_FAST, sub_agent_common.build_sub_agent_model = saved


async def main() -> None:
    test_history_is_summarised_before_the_window_overflows()
    test_the_summary_can_come_from_the_fast_tier()
    await test_a_big_result_is_set_aside_and_readable()
    await test_a_reference_is_not_a_path()
    await test_a_credential_never_reaches_the_disk()
    await test_the_knobs()
    test_the_veto_and_the_housekeeping()
    await test_limits_pause_a_turn_and_it_continues()
    await test_a_token_budget_pauses_a_turn()
    await test_cancel_reaches_the_running_agent()
    print("\nPASS: big results are set aside (not cut) and read back by slice, privately and without credentials; limits pause a turn and it continues; cancel reaches the running agent")


if __name__ == "__main__":
    asyncio.run(main())
    sys.stdout.flush()

"""The session surface a native front door reads (docs/app.md §6.1, phase B1), offline, through the real ACP server.

1. **the checklist is ACP's plan** — a `todo_write` reaches every client as a native `plan` update (beside
   `_cordata/tasklist`), and a reloaded session gets its checklist back from the ledger;
2. **usage** — the context window's fill as `usage_update` after a turn, and the turn's tokens on `PromptResponse.usage`;
3. **replay has tool calls** — `session/load` in a new process replays the prompt, the reply, every tool call with its
   input and result, and an edit as a diff of what it *said*, all tagged `_meta.cordata.replay`;
4. **history pages** — `_cordata/session/history` serves a conversation newest-first, by cursor, with no agent built;
5. **fork** — a new conversation from before turn 2 has turn 1 only, loads, and leaves the source as it was;
6. **mode and config changes are echoed** to every client (`current_mode_update`, `config_option_update`);
7. **images** — attached image blocks are validated (type, size, encoding) and refused, saying why, when no vision
   model is configured; `initialize` advertises images only when one is.

    backend/.venv/bin/python _session_surface_smoke_test.py
"""

from __future__ import annotations

import asyncio
import base64
import json
import os

os.environ["CORDATA_PRISM"] = "off"

import provider_settings  # noqa: F401,E402

import acp  # noqa: E402

import acp_protocol  # noqa: E402
import acp_server  # noqa: E402
import session_history  # noqa: E402
import state_store  # noqa: E402
import task_tool  # noqa: E402
from _acp_harness_smoke_test import MODELS, SESSIONS, call, fresh_server, laya_answer, orchestrator_model, project, settle, text  # noqa: E402
from harness_testing import check  # noqa: E402

ROUTE = laya_answer("code", {"code": 0.97, "answer": 0.03})


def kinds(updates) -> list[str]:
    return [getattr(u, "session_update", type(u).__name__) for u in updates]


def meta(update) -> dict:
    return (getattr(update, "field_meta", None) or {}).get("cordata") or {}


async def build_conversation(name: str):
    """Two turns: a checklist and a shell command, then an edit — through the real server on scripted models."""
    cwd = project(name)
    (cwd / "notes.txt").write_text("alpha\nbeta\n")
    orchestrator_model([
        call("todo_write", {"todos": [{"content": "look around", "status": "completed"}, {"content": "change the notes", "status": "in_progress"}]}),
        call("shell", {"command": "echo listed"}),
        text("first answer"),
        call("editor", {"command": "str_replace", "path": "notes.txt", "old_str": "beta", "new_str": "gamma"}),
        text("second answer"),
    ])
    MODELS["orchestrator"].update_config(context_window_limit=100_000)
    server, conn = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(cwd))).session_id
    await server.set_session_mode(sid, "auto")
    first = await server.prompt(sid, [acp.text_block("tidy the notes")])
    await settle()
    second = await server.prompt(sid, [acp.text_block("now change beta")])
    await settle()
    return cwd, sid, server, conn, first, second


async def test_plan_usage_and_replay() -> None:
    print("\n=== 1–3. plan, usage, and a replay with tool calls ===")
    cwd, sid, server, conn, first, second = await build_conversation("surface")
    plans = [u for u in conn.updates if getattr(u, "session_update", "") == "plan"]
    check(plans and [e.content for e in plans[-1].entries] == ["look around", "change the notes"], f"the checklist arrives as ACP's plan ({len(plans)} plan updates)")
    check(plans and [e.status for e in plans[-1].entries] == ["completed", "in_progress"], "…with each step's status")
    check(any(m == "cordata/tasklist" for m, _ in conn.ext), "…and still as _cordata/tasklist for Cordata's own clients")
    usage = [u for u in conn.updates if getattr(u, "session_update", "") == "usage_update"]
    check(usage and usage[-1].size == 100_000 and usage[-1].used > 0, f"the context fill arrives as usage_update ({usage[-1].used if usage else None}/{usage[-1].size if usage else None})")
    check(usage and "tokensIn" in meta(usage[-1]) and "estimated" in meta(usage[-1]), "…with token counts and the estimate flag in _meta.cordata")
    check(first.usage is not None and first.usage.total_tokens > 0 and first.usage.total_tokens == first.usage.input_tokens + first.usage.output_tokens, f"the turn's tokens ride on the PromptResponse ({first.usage})")

    # A new process (the daemon restarted): load replays everything, from the snapshot.
    acp_server._LEASED.discard(sid)
    state_store.release_session(sid)
    task_tool._lists.pop(sid, None)
    server2, conn2 = fresh_server(ROUTE)
    await server2.load_session(cwd=str(cwd), session_id=sid)
    await settle()
    replayed = conn2.updates
    kinds2 = kinds(replayed)
    check(kinds2.count("user_message_chunk") == 2 and kinds2.count("agent_message_chunk") >= 2, f"both prompts and both replies replay ({kinds2.count('user_message_chunk')} prompts)")
    starts = [u for u in replayed if getattr(u, "session_update", "") == "tool_call"]
    names = [u.title for u in starts]
    check(names == ["todo_write", "shell", "editor"], f"every tool call replays, in order ({names})")
    shell = next(u for u in starts if u.title == "shell")
    check((shell.raw_input or {}).get("command") == "echo listed" and shell.kind == "execute", f"…with its input and kind ({shell.raw_input}, {shell.kind})")
    results = {u.tool_call_id: u for u in replayed if getattr(u, "session_update", "") == "tool_call_update"}
    shell_result = results.get(shell.tool_call_id)
    check(shell_result is not None and shell_result.status == "completed" and "listed" in json.dumps(shell_result.model_dump(mode="json")), "…and its result")
    edit = next(u for u in starts if u.title == "editor")
    diff = (edit.content or [None])[0]
    check(diff is not None and getattr(diff, "type", "") == "diff" and diff.old_text == "beta" and diff.new_text == "gamma", "an edit replays as the diff it said, not today's file")
    check(all(meta(u).get("replay") for u in replayed if getattr(u, "session_update", "") in ("tool_call", "tool_call_update", "user_message_chunk")), "every replayed update is tagged replay")
    turns = sorted({meta(u).get("turn") for u in replayed if meta(u).get("replay")})
    check(turns == [1, 2], f"…with its turn ({turns})")
    plan_again = [u for u in replayed if getattr(u, "session_update", "") == "plan"]
    check(plan_again and [e.content for e in plan_again[-1].entries] == ["look around", "change the notes"], "the checklist comes back on reload, from the ledger")
    return cwd, sid


async def test_history_and_fork(cwd, sid) -> None:
    print("\n=== 4–5. history pages and forks ===")
    server, _ = fresh_server(ROUTE)  # holds no session: history is read from disk
    page = await server.ext_method("_cordata/session/history", {"sessionId": sid, "limit": 3})
    check(page["ok"] and page["end"] == page["total"] and page["nextCursor"] is not None, f"the newest page ends at the conversation's end ({page['start']}–{page['end']} of {page['total']})")
    seen = [page]
    while seen[-1]["nextCursor"] is not None and len(seen) < 20:
        seen.append(await server.ext_method("_cordata/session/history", {"sessionId": sid, "cursor": seen[-1]["nextCursor"], "limit": 3}))
    starts = [p["start"] for p in seen]
    check(seen[-1]["start"] == 0 and starts == sorted(starts, reverse=True), f"cursors walk back to the start ({starts})")
    covered = sorted({u["_meta"]["cordata"]["index"] for p in seen for u in p["updates"]})
    total_messages = len(session_history.read_messages(sid, SESSIONS))
    check(covered[0] == 0 and covered[-1] == total_messages - 1, "every message is on some page")
    check(all(u["_meta"]["cordata"]["turn"] in (1, 2) for p in seen for u in p["updates"]), "turn numbers are the conversation's, not the page's")
    missing = await server.ext_method("_cordata/session/history", {"sessionId": "no-such"})
    check(missing == {"ok": False, "reason": "no such conversation"}, "an unknown conversation says so")

    before = (SESSIONS / "session" / sid / "scopes" / "agent" / "default" / "snapshots" / "snapshot_latest.json").read_bytes()
    loader, _ = fresh_server(ROUTE)
    await loader.load_session(cwd=str(cwd), session_id=sid)
    forked = await loader.ext_method("_cordata/conversation/fork", {"sessionId": sid, "turn": 2})
    check(forked.get("ok") and forked["sessionId"] != sid, f"a fork from before turn 2 is a new conversation ({forked})")
    messages = session_history.read_messages(forked["sessionId"], SESSIONS)
    prompts = [b.get("text") for m in messages for b in m.get("content") or [] if m.get("role") == "user" and b.get("text")]
    check(prompts == ["tidy the notes"], f"…holding turn 1 only ({prompts})")
    check((SESSIONS / "session" / sid / "scopes" / "agent" / "default" / "snapshots" / "snapshot_latest.json").read_bytes() == before, "the source is untouched")
    row = state_store.get_session(forked["sessionId"]) or {}
    check(row.get("origin") == "fork" and "fork of" in (row.get("title") or ""), "the fork is listed, as a fork")
    other, conn = fresh_server(ROUTE)
    await other.load_session(cwd=str(cwd), session_id=forked["sessionId"])
    check(len(other._sessions[forked["sessionId"]].agent.messages) == len(messages), "the fork loads like any conversation")
    check(any(e["kind"] == "conversation_fork" for e in state_store.recent_events(session_id=sid, limit=20)), "the fork is in the ledger")
    bad = await loader.ext_method("_cordata/conversation/fork", {"sessionId": sid, "turn": 9})
    check(bad == {"ok": False, "reason": "no checkpoint before turn 9"}, "a turn with no checkpoint is refused")


async def test_mode_and_config_echo() -> None:
    print("\n=== 6. mode and config changes are echoed ===")
    orchestrator_model([text("hi")])
    server, conn = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(project("echo")))).session_id
    await server.set_session_mode(sid, "plan")
    await settle()
    modes = [u for u in conn.updates if getattr(u, "session_update", "") == "current_mode_update"]
    check(modes and modes[-1].current_mode_id == "plan", "a mode change is announced to every client")
    await server.set_config_option("temperature", sid, "focused")
    await settle()
    configs = [u for u in conn.updates if getattr(u, "session_update", "") == "config_option_update"]
    check(bool(configs) and any(o.id == "temperature" for o in configs[-1].config_options), "a config change is announced with the options")


async def test_images() -> None:
    print("\n=== 7. attached images ===")
    png = base64.b64encode(b"\x89PNG\r\n\x1a\n" + b"\x00" * 64).decode()
    blocks = [
        acp.schema.ImageContentBlock(type="image", data=png, mime_type="image/png"),
        acp.schema.ImageContentBlock(type="image", data=png, mime_type="image/tiff"),
        acp.schema.ImageContentBlock(type="image", data="not base64!!", mime_type="image/jpeg"),
        acp.schema.ImageContentBlock(type="image", data=base64.b64encode(b"x" * (acp_protocol.MAX_PROMPT_IMAGE_BYTES + 1)).decode(), mime_type="image/png"),
    ]
    images, dropped = acp_server._prompt_images(blocks)
    check(len(images) == 1 and images[0]["format"] == "png" and images[0]["bytes"].startswith(b"\x89PNG"), "a PNG is decoded and kept")
    check(len(dropped) == 3 and any("tiff" in d for d in dropped) and any("decode" in d for d in dropped) and any("MiB" in d for d in dropped), f"wrong type, bad encoding and oversize are each dropped, with a reason ({dropped})")

    orchestrator_model([text("answered without the picture")])
    server, conn = fresh_server(ROUTE)
    init = await server.initialize(protocol_version=acp.PROTOCOL_VERSION)
    caps = init.agent_capabilities.prompt_capabilities
    check(caps is not None and caps.image is False and caps.embedded_context is True, "with no vision model, images are not advertised (embedded files are)")
    sid = (await server.new_session(cwd=str(project("images")))).session_id
    response = await server.prompt(sid, [acp.text_block("what is in this?"), blocks[0]])
    await settle()
    said = " ".join(str(u) for u in conn.updates)
    check(response.stop_reason == "end_turn" and "no vision model is configured" in said, "the turn runs on the text, and says the image was not attached and why")
    check(not any("image" in b for m in server._sessions[sid].agent.messages for b in m.get("content") or []), "no image reaches the history")


async def main() -> None:
    cwd, sid = await test_plan_usage_and_replay()
    await test_history_and_fork(cwd, sid)
    await test_mode_and_config_echo()
    await test_images()
    print("\nALL OK")


if __name__ == "__main__":
    asyncio.run(main())

"""A conversation's history as ACP updates, read straight from its snapshot — no agent, no model (docs/app.md §6.1).

Three uses:

* **replay** on ``session/load``: what the person said, what the agent said and thought, *and every tool call with its
  result* — a front door rebuilding its transcript used to get the text only (``_replay_history`` read nothing else).
  Edits replay as diffs of what the call *said* (``edit_preview.from_input``), never of today's file;
* **paging** (``_cordata/session/history``): a long conversation in pages, newest first, without the notification flood
  of a full ``session/load`` — and for a conversation no process holds, so the daemon can serve it;
* **fork** (``_cordata/conversation/fork``): a new conversation starting from this one as it stood before a given turn
  (or as it is now), while the source runs on untouched.

Every update carries ``_meta.cordata = {"replay": true, "turn": n, "index": i}`` so a client can tell history from live
events. Stored tool results are what the hooks left (masked, fenced, offloaded); the text replayed is masked once more.
Stdlib, the ACP schema and two small harness modules: importable by the daemon.
"""

from __future__ import annotations

import json
import os
import uuid
from collections.abc import Callable
from pathlib import Path
from typing import Any

import acp

import edit_preview
import secret_patterns

#: Where a conversation's snapshots live (``session_store.snapshot_path``), repeated here so the daemon can read them
#: without importing Strands.
SNAPSHOT_ROOT = "session"
AGENT_ID = "default"

#: Result text per tool call kept in a replayed update; the rest is in the snapshot (and the offload store).
MAX_RESULT_CHARS = 8_000
PAGE_DEFAULT = 40
PAGE_MAX = 200


def snapshot_path(session_id: str, base: str | Path) -> Path:
    return Path(base) / SNAPSHOT_ROOT / session_id / "scopes" / "agent" / AGENT_ID / "snapshots" / "snapshot_latest.json"


def history_dir(session_id: str, base: str | Path) -> Path:
    return snapshot_path(session_id, base).parent / "immutable_history"


def _masked(text: str) -> str:
    return secret_patterns.scrub(text)[0]


def _is_prompt(message: dict) -> bool:
    """A user message that is the person's words (not a batch of tool results): it starts a turn."""
    blocks = message.get("content") or []
    return message.get("role") == "user" and any(isinstance(b, dict) and b.get("text") for b in blocks) and not any(isinstance(b, dict) and "toolResult" in b for b in blocks)


def _result_text(result: dict) -> str:
    parts = []
    for item in result.get("content") or []:
        if not isinstance(item, dict):
            continue
        if isinstance(item.get("text"), str):
            parts.append(item["text"])
        elif "json" in item:
            parts.append(json.dumps(item["json"], ensure_ascii=False, default=str))
    text = "\n".join(parts)
    if len(text) > MAX_RESULT_CHARS:
        text = text[:MAX_RESULT_CHARS] + f"\n… ({len(text) - MAX_RESULT_CHARS} more characters in the session record)"
    return _masked(text)


def replay(
    messages: list[dict],
    *,
    thoughts: bool = True,
    kind_of: Callable[[str], str] = lambda _name: "other",
    first_index: int = 0,
) -> list[Any]:
    """``messages`` (a Strands history, or a slice of one starting at ``first_index``) as ACP ``session/update``s."""
    updates: list[Any] = []
    turn = 0
    for offset, message in enumerate(messages):
        if not isinstance(message, dict):
            continue
        index = first_index + offset
        if _is_prompt(message):
            turn += 1
        role = message.get("role")
        for block in message.get("content") or []:
            if not isinstance(block, dict):
                continue
            update: Any = None
            if isinstance(block.get("text"), str) and block["text"]:
                text = _masked(block["text"])
                update = acp.update_user_message_text(text) if role == "user" else acp.update_agent_message_text(text)
            elif "reasoningContent" in block and thoughts:
                text = ((block.get("reasoningContent") or {}).get("reasoningText") or {}).get("text")
                if isinstance(text, str) and text:
                    update = acp.update_agent_thought_text(_masked(text))
            elif "toolUse" in block:
                use = block["toolUse"] or {}
                name, tool_input = str(use.get("name") or "tool"), use.get("input")
                diffs = [acp.tool_diff_content(path, new, old) for path, old, new in edit_preview.from_input(name, tool_input)]
                update = acp.start_tool_call(str(use.get("toolUseId") or uuid.uuid4()), name, kind=kind_of(name), status="pending", raw_input=tool_input, content=diffs or None)
            elif "toolResult" in block:
                result = block["toolResult"] or {}
                text = _result_text(result)
                status = "failed" if result.get("status") == "error" else "completed"
                content = [acp.tool_content(acp.text_block(text))] if text else None
                update = acp.update_tool_call(str(result.get("toolUseId") or ""), status=status, content=content)
            if update is not None:
                update.field_meta = {"cordata": {"replay": True, "turn": turn, "index": index}}
                updates.append(update)
    return updates


def read_messages(session_id: str, base: str | Path, *, checkpoint: str | None = None) -> list[dict] | None:
    """The conversation's messages from disk (its latest snapshot, or a turn checkpoint), or None if there is none."""
    path = history_dir(session_id, base) / f"snapshot_{checkpoint}.json" if checkpoint else snapshot_path(session_id, base)
    try:
        blob = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    messages = (blob.get("data") or {}).get("messages")
    return messages if isinstance(messages, list) else None


def page(session_id: str, base: str | Path, *, cursor: int | None = None, limit: int = PAGE_DEFAULT, kind_of: Callable[[str], str] = lambda _name: "other") -> dict:
    """One page of history, newest first: the messages before ``cursor`` (a message index; None = the end), as updates
    in conversation order. ``nextCursor`` is where the next (older) page ends, or None at the start. A page never
    splits a tool call from its result: it starts at a prompt when one is near."""
    messages = read_messages(session_id, base)
    if messages is None:
        return {"ok": False, "reason": "no such conversation"}
    limit = max(1, min(int(limit or PAGE_DEFAULT), PAGE_MAX))
    end = len(messages) if cursor is None else max(0, min(int(cursor), len(messages)))
    start = max(0, end - limit)
    # Back up to the turn's prompt (within one more page) so the page opens on something a reader recognises.
    for back in range(start, max(-1, start - limit), -1):
        if back <= 0 or _is_prompt(messages[back]):
            start = max(0, back)
            break
    # Turn numbers are counted from the start of the conversation, not of the page.
    turns_before = sum(1 for m in messages[:start] if isinstance(m, dict) and _is_prompt(m))
    updates = replay(messages[start:end], kind_of=kind_of, first_index=start)
    for update in updates:
        meta = (update.field_meta or {}).get("cordata") or {}
        meta["turn"] = meta.get("turn", 0) + turns_before
    return {
        "ok": True,
        "sessionId": session_id,
        "total": len(messages),
        "start": start,
        "end": end,
        "nextCursor": start if start > 0 else None,
        "updates": [update.model_dump(mode="json", by_alias=True, exclude_none=True) for update in updates],
    }


def fork(session_id: str, base: str | Path, *, checkpoint: str | None = None, turn: int | None = None) -> dict:
    """A new conversation that starts where this one stood: before turn ``turn`` (its turn checkpoint), at a named
    ``checkpoint``, or as it is now. The source is only read. A parked approval does not travel (the fork's process
    could never answer it), and the turn counter restarts from the copy's own history."""
    source: Path
    if turn is not None and checkpoint is None:
        found = None
        for path in sorted(history_dir(session_id, base).glob("snapshot_*.json")):
            try:
                meta = json.loads(path.read_text(encoding="utf-8")).get("app_data") or {}
            except (OSError, ValueError):
                continue
            if int(meta.get("turn") or 0) == int(turn) and meta.get("kind", "turn") == "turn":
                found = path
        if found is None:
            return {"ok": False, "reason": f"no checkpoint before turn {turn}"}
        source = found
    elif checkpoint:
        source = history_dir(session_id, base) / f"snapshot_{checkpoint}.json"
    else:
        source = snapshot_path(session_id, base)
    try:
        blob = json.loads(source.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {"ok": False, "reason": "the conversation (or that checkpoint) cannot be read"}
    data = blob.setdefault("data", {})
    # Strands' own empty state (``_InterruptState().to_dict()``): nothing parked, nothing activated.
    data["interrupt_state"] = {"interrupts": {}, "context": {}, "activated": False}
    blob["app_data"] = {"forked_from": session_id, "checkpoint": source.stem.removeprefix("snapshot_")}
    new_id = str(uuid.uuid4())
    target = snapshot_path(new_id, base)
    for parent in reversed([p for p in target.parents if str(p).startswith(str(Path(base)))]):
        parent.mkdir(mode=0o700, exist_ok=True)
    temp = target.with_suffix(".tmp")
    fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as out:
        json.dump(blob, out, ensure_ascii=False)
    os.replace(temp, target)
    return {"ok": True, "sessionId": new_id, "from": session_id, "messages": len(data.get("messages") or [])}


#: Bounds for `search`: conversations read, a snapshot's size, matches returned.
SEARCH_MAX_SESSIONS = 300
SEARCH_MAX_BYTES = 8 * 1024 * 1024
SEARCH_MAX_RESULTS = 50


def search(session_ids: list[str], base: str | Path, query: str, *, limit: int = 20) -> list[dict]:
    """Conversations whose words contain ``query`` (case-insensitive): ``[{sessionId, role, snippet}]``, one match per
    conversation, in the order the ids were given (newest first). Reads the person's and the assistant's text only — not
    tool output — and masks every snippet. Bounded in conversations read, bytes per snapshot and results."""
    needle = (query or "").strip().lower()
    if len(needle) < 2:
        return []
    limit = max(1, min(int(limit), SEARCH_MAX_RESULTS))
    found: list[dict] = []
    for session_id in session_ids[:SEARCH_MAX_SESSIONS]:
        path = snapshot_path(session_id, base)
        try:
            if path.stat().st_size > SEARCH_MAX_BYTES:
                continue
        except OSError:
            continue
        for message in read_messages(session_id, base) or []:
            role = message.get("role")
            if role not in ("user", "assistant"):
                continue
            text = " ".join(b["text"] for b in message.get("content") or [] if isinstance(b, dict) and isinstance(b.get("text"), str))
            at = text.lower().find(needle)
            if at < 0:
                continue
            start = max(0, at - 60)
            snippet = ("…" if start else "") + " ".join(text[start : at + len(needle) + 80].split()) + "…"
            found.append({"sessionId": session_id, "role": role, "snippet": _masked(snippet)})
            break
        if len(found) >= limit:
            break
    return found

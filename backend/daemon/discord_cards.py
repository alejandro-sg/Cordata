"""What the Discord front door says, as plain data — no `discord` import, no I/O.

Everything a person reads in the channel is built here so it can be asserted on without a gateway,
and so there is one place to hold the rules that make it worth reading:

- **Say what, where and why before how risky.** A card that opens with `shell scored 0.99` asks the
  person to trust a number. One that opens with "The agent wants to run a shell command in
  *sierralab-agents*. You asked: … It said: …" can be answered from a phone, in a queue, in five
  seconds. The score and Laya's reason are still there — as evidence, second.
- **Honest buttons.** "Allow" and "Deny" do exactly that. (The old "Reject & Isolate" did not isolate
  anything; a label that promises a containment it does not perform is worse than a plain one.)
- **A decision collapses to one line.** Once answered, a card keeps the command and the verdict and
  drops the rest, so the channel reads as a log rather than a wall of expired prompts.
- **Zero tokens.** Status, usage and help are answered from state the daemon already holds. Asking
  "what are you working on?" must not cost a model call — least of all the one the agent is busy
  making.

Specs are `{title, description, fields: [(name, value, inline)], color, footer}`; `DiscordRemote._embed`
is the single place that turns one into an `Embed`, masking secrets on the way.
"""

from __future__ import annotations

import re
import time
from typing import Any, Mapping

DANGER_COLOR = 0xE74C3C
WARN_COLOR = 0xF1C40F
DONE_COLOR = 0x2ECC71
INFO_COLOR = 0x3498DB
MUTED_COLOR = 0x95A5A6

#: Discord's limits, with room: a field is 1024, a description 4096, a message 2000.
MAX_BODY_CHARS = 900
MAX_CONTEXT_CHARS = 280
MAX_MESSAGE_CHARS = 1900
MAX_NAME_CHARS = 90

APPROVE_LABEL = "Allow"
APPROVE_ONCE_LABEL = "Allow once"
DENY_LABEL = "Deny"
DENY_STOP_LABEL = "Deny & stop"
ALL_ROUTINE_LABEL = "Allow routine steps in this chat"

UNAUTHORIZED = "Not your call: this daemon only accepts decisions from its configured operator."
ALREADY = "Already decided (or the session ended) — nothing to do."

CONSEQUENCE = "**Allow** runs it now. **Deny** tells the agent no; it has to find another way."


# --------------------------------------------------------------------------- small helpers


def clip(text: str, limit: int) -> str:
    """One line, clipped with an ellipsis. A gloss for context — never used on the command itself."""
    flat = " ".join((text or "").split())
    return flat if len(flat) <= limit else flat[: limit - 1].rstrip() + "…"


def fence(text: str, lang: str) -> str:
    """`text` in a fenced block, closed. A body that contains a fence of its own would break out of
    this one and turn the rest of the message into markdown, so fences inside the text are flattened
    to quotes first — a diff is read here, it is not executed and it does not need to round-trip."""
    body = (text or "").replace("```", "'''")
    return f"```{lang}\n{body}\n```"


def fmt_tokens(count: int | float | None) -> str:
    n = int(count or 0)
    if n >= 1_000_000:
        return f"{n / 1_000_000:.1f}M"
    if n >= 1000:
        return f"{n / 1000:.1f}k"
    return str(n)


def fmt_seconds(seconds: float | None) -> str:
    s = int(round(seconds or 0))
    if s < 60:
        return f"{s}s"
    minutes, rest = divmod(s, 60)
    if minutes < 60:
        return f"{minutes}m {rest:02d}s"
    hours, minutes = divmod(minutes, 60)
    return f"{hours}h {minutes:02d}m"


def project_name(cwd: str | None) -> str:
    cwd = (cwd or "").rstrip("/")
    return cwd.rsplit("/", 1)[-1] if cwd else "an unknown folder"


def split_message(text: str, limit: int = MAX_MESSAGE_CHARS) -> list[str]:
    """Cut a long reply into Discord-sized messages, on a paragraph, then a line, then a space — and
    only then mid-word. A fenced block that a cut would split is closed and reopened, so one chunk's
    unterminated fence never swallows the next chunk's prose."""
    text = (text or "").strip()
    if not text:
        return []
    chunks: list[str] = []
    room = limit - 8  # headroom for the closing fence a cut may need to add
    while len(text) > limit:
        window = text[:room]
        cut = max(window.rfind("\n\n"), window.rfind("\n"), window.rfind(" "))
        if cut < room // 2:
            cut = room
        piece, text = text[:cut].rstrip(), text[cut:].lstrip("\n")
        if piece.count("```") % 2 == 1:
            # The cut landed inside a fenced block: close it here and reopen it on the next chunk, whose
            # own opening marker then balances the count again.
            piece += "\n```"
            text = "```\n" + text
        chunks.append(piece)
    if text:
        chunks.append(text)
    return chunks


# --------------------------------------------------------------------------- agent markdown → Discord

_BR = re.compile(r"(?i)<br\s*/?>")
_TABLE_SEP = re.compile(r"^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)+\|?\s*$")
_TABLE_ROW = re.compile(r"^\s*\|")
_RULE = re.compile(r"^\s*([-*_])\1{2,}\s*$")
_BULLET = re.compile(r"^\s*([•·▪‣◦]|[-*+])\s+")
_HEADING_DEEP = re.compile(r"^\s*#{4,}\s+(.*\S)\s*$")


def _split_cells(row: str) -> list[str]:
    row = row.strip()
    if row.startswith("|"):
        row = row[1:]
    if row.endswith("|"):
        row = row[:-1]
    return [cell.strip() for cell in row.split("|")]


def _cell_parts(cell: str) -> tuple[list[str], bool]:
    """A table cell as its separate lines: `a<br>• b<br>• c` → ["a", "b", "c"]. The flag says whether the cell
    itself was written as a list (any part led with a bullet), which decides how it is laid out."""
    parts, bulleted = [], False
    for raw in _BR.split(cell):
        raw = raw.strip()
        marker = _BULLET.match(raw + " ")
        if marker and marker.group(1) in "•·▪‣◦-*+":
            bulleted = True
            raw = raw[marker.end() - 1 :].strip() if len(raw) >= marker.end() - 1 else ""
        if raw:
            parts.append(raw)
    return parts, bulleted


def _bold(label: str) -> str:
    label = label.strip()
    return label if label.startswith("**") and label.endswith("**") else f"**{label}**"


def render_table(header: list[str], rows: list[list[str]]) -> str:
    """A GitHub table as something a phone shows: one block per row, the first column as its label and the rest
    as lines under it. Discord does not render tables, and a monospaced grid is wider than a phone screen.

    Two columns read as "**label** — text" (or the label over a bullet list when the cell was one); more read as
    "- **Header:** value" under the label."""
    blocks: list[str] = []
    for row in rows:
        row = row + [""] * (len(header) - len(row))
        label = _bold(row[0]) if row[0].strip() else ""
        lines: list[str] = []
        if len(header) <= 2:
            parts, bulleted = _cell_parts(row[1] if len(row) > 1 else "")
            if len(parts) == 1 and not bulleted and label:
                lines = [f"{label} — {parts[0]}"]
            else:
                lines = ([label] if label else []) + [f"- {part}" for part in parts]
        else:
            if label:
                lines.append(label)
            for name, cell in zip(header[1:], row[1:]):
                parts, _ = _cell_parts(cell)
                if parts:
                    lines.append(f"- **{name.strip('* ')}:** " + "; ".join(parts))
        if lines:
            blocks.append("\n".join(lines))
    return "\n\n".join(blocks)


def _convert_prose(text: str) -> str:
    lines = text.split("\n")
    out: list[str] = []
    i = 0
    while i < len(lines):
        line = lines[i]
        if _TABLE_ROW.match(line) and i + 1 < len(lines) and _TABLE_SEP.match(lines[i + 1]):
            header = _split_cells(line)
            i += 2
            rows: list[list[str]] = []
            while i < len(lines) and _TABLE_ROW.match(lines[i]):
                rows.append(_split_cells(lines[i]))
                i += 1
            rendered = render_table(header, rows)
            if rendered:
                out.append(rendered)
            continue
        i += 1
        if _RULE.match(line) and not _TABLE_SEP.match(line):
            out.append("───────────")
            continue
        deep = _HEADING_DEEP.match(line)
        if deep:
            out.append(f"**{deep.group(1)}**")
            continue
        line = _BR.sub("\n", line)
        out.append(re.sub(r"^(\s*)[•·▪‣◦]\s+", r"\1- ", line, flags=re.MULTILINE))
    return "\n".join(out)


def discord_markdown(text: str) -> str:
    """What an agent wrote, as Discord will render it. Models write GitHub markdown — tables, `<br>`, rules,
    `####` headings — and Discord renders none of it: a table arrives as a wall of pipes and `<br>` as the literal
    tag. Tables become one block per row (see `render_table`), `<br>` a line break, a rule a short line, deep
    headings bold. Fenced code is left exactly as written, and an unterminated fence (a reply still streaming)
    stays code to the end. Never loses text: what it does not recognise passes through."""
    out: list[str] = []
    prose: list[str] = []
    in_code = False

    def flush() -> None:
        if prose:
            out.append(_convert_prose("\n".join(prose)))
            prose.clear()

    for line in text.split("\n"):
        if line.lstrip().startswith("```"):
            if not in_code:
                flush()
            in_code = not in_code
            out.append(line)
        elif in_code:
            out.append(line)
        else:
            prose.append(line)
    flush()
    return "\n".join(out)


def thread_title(text: str, *, prefix: str = "💬 ") -> str:
    """A thread named after what was asked: first line, mentions and links stripped, clipped."""
    first = (text or "").strip().splitlines()[0] if (text or "").strip() else "chat"
    first = re.sub(r"<@!?\d+>|<#\d+>|https?://\S+", "", first)
    clean = " ".join(first.split()) or "chat"
    room = MAX_NAME_CHARS - len(prefix)
    return prefix + (clean[: room - 1].rstrip() + "…" if len(clean) > room else clean)


# --------------------------------------------------------------------------- intent: commands and questions

COMMANDS = ("help", "status", "stop", "new", "mode", "usage", "back", "conversations", "continue")

_STATUS_WORDS = re.compile(
    r"^(status|progress|update|updates|report|sitrep)$"
    r"|\b(what('?s| is| are)? (you|u|the agents?|everyone|it)( all)?( up to| working| doing)"
    r"|what are you (working|doing)|what('?s| is) (going on|happening|the status|the progress)"
    r"|where are (you|we)( at)?|how('?s| is) it going|how far( along| in)?|are you (done|still working|stuck)"
    r"|any (progress|updates?)|what('?s| is) (the )?(agent|agents) (doing|up to|working on))\b"
)


def is_status_question(text: str) -> bool:
    """A short message that is *asking where things stand*. Answered from the daemon's own state, so
    the agent is not interrupted (and no tokens are spent) to say what the daemon already knows.

    Deliberately narrow: ≤ 12 words, and a miss is cheap — the message goes to the agent as an
    ordinary prompt or steering note. A false hit would swallow a real instruction."""
    cleaned = re.sub(r"<@!?\d+>|[^\w\s']", " ", (text or "").lower()).strip()
    if not cleaned or len(cleaned.split()) > 12:
        return False
    return bool(_STATUS_WORDS.search(cleaned))


def parse_command(text: str) -> tuple[str, str] | None:
    """`!name rest` → (name, rest) for a known command, `("?", name)` for an unknown `!word`, else None.

    A bang rather than a slash: Discord's client swallows `/word` as an application command and never
    sends unknown ones, and registering real slash commands needs a guild sync this daemon has no way to
    ask the operator to authorise. A bang is plain text the message-content intent already delivers."""
    stripped = (text or "").strip()
    if not stripped.startswith("!") or len(stripped) < 2 or stripped[1].isspace():
        return None
    head, _, rest = stripped[1:].partition(" ")
    name = head.lower()
    if name in COMMANDS:
        return name, rest.strip()
    return "?", name


def parse_back(arg: str) -> dict[str, Any] | str | None:
    """`!back`'s argument as the params of `_sierralab/conversation/rewind`, the string `"list"`, or None when it is not
    something `!back` takes."""
    words = (arg or "").split()
    if not words:
        return {"turns": 1}
    if words == ["list"] or words == ["ls"]:
        return "list"
    if len(words) == 1 and words[0].isdigit() and int(words[0]) > 0:
        return {"turns": int(words[0])}
    if len(words) == 2 and words[0] == "to" and words[1].isdigit() and int(words[1]) > 0:
        return {"index": int(words[1])}
    return None


def back_text(result: Mapping[str, Any]) -> str:
    """What `!back` answers: what the agent forgot, and what is left alone."""
    if not result.get("ok"):
        return f"↶ {result.get('reason') or 'could not go back'}"
    forgot = [str(p) for p in result.get("forgot") or [] if p]
    if not forgot:
        return f"↶ Restored an earlier checkpoint — {result.get('now', 0)} turn(s) in the conversation. Files are untouched."
    lines = [f"↶ Went back {len(forgot)} turn{'s' if len(forgot) != 1 else ''} — I no longer remember:"]
    lines += [f"• {p[:140]}" for p in forgot[:6]]
    if len(forgot) > 6:
        lines.append(f"• … and {len(forgot) - 6} more")
    lines.append("-# The messages above stay in this thread, but I won't see them. Files are untouched. `!back list` shows the state I left, to restore it.")
    return "\n".join(lines)


def checkpoints_text(listing: Mapping[str, Any]) -> str:
    if not listing.get("enabled", True):
        return "This chat has no checkpoints (it is not on snapshot storage)."
    rows = listing.get("checkpoints") or []
    if not rows:
        return "No checkpoints yet — one is taken as each turn starts."
    lines = [f"**Checkpoints** (newest first) — {listing.get('turns', 0)} turn(s) so far. `!back to <number>` restores one:"]
    for row in rows[:12]:
        label = f"before turn {row.get('turn')}: {str(row.get('prompt') or '')[:90]}" if row.get("kind") == "turn" else f"↺ {str(row.get('prompt') or '')[:90]}"
        lines.append(f"`{row.get('index')}` · {row.get('messages', 0)} msgs · {label}")
    return "\n".join(lines)


def steer_text(content: str) -> str:
    """The directive in a message, or "" when it carries none.

    `> steer: <text>` (with or without the colon) is the explicit form; a plain sentence in a task
    thread the bot opened is the other, because a reply there *is* the natural way to steer it. A bare
    `> steer:` with nothing after it is nothing — an empty directive must not reach the model."""
    text = (content or "").strip()
    lowered = text.lower()
    for prefix in ("> steer:", ">steer:", "> steer", ">steer"):
        if lowered.startswith(prefix):
            return text[len(prefix) :].strip()
    return text


def is_explicit_steer(content: str) -> bool:
    lowered = (content or "").strip().lower()
    return lowered.startswith(("> steer", ">steer"))


# --------------------------------------------------------------------------- approval cards

_EDIT_PATH = re.compile(r"^\+\+\+ (?:b/)?(\S+)", re.MULTILINE)


def body_of(meta: Mapping[str, Any]) -> tuple[str, str, str]:
    """`(label, text, fence language)`: the thing the decision is about — the diff for an edit, the
    command for a shell call, the input as JSON for anything else. `intervention_view.build` already
    picked and masked it, so this only labels."""
    kind = str(meta.get("kind") or "other")
    if kind == "diff" and meta.get("diff"):
        return "Changes", str(meta["diff"]), "diff"
    if kind == "shell" and meta.get("command"):
        return "Command", str(meta["command"]), "sh"
    return "Input", str(meta.get("input_json") or meta.get("command") or ""), "json"


def what_it_wants(meta: Mapping[str, Any]) -> str:
    """The verb phrase: "run a shell command", "edit `src/a.py`", "hand a task to the Claude Code CLI"."""
    tool = str(meta.get("tool") or "tool")
    if tool == "shell":
        return "run a shell command"
    if tool == "python_repl":
        return "run Python code"
    if tool in ("editor", "file_write", "apply_patch"):
        paths = list(dict.fromkeys(_EDIT_PATH.findall(str(meta.get("diff") or ""))))
        if len(paths) == 1:
            return f"edit `{paths[0]}`"
        return f"edit {len(paths)} files" if paths else "change files"
    if tool in ("http_request", "web_fetch"):
        return "make a network request"
    if tool == "claude_code_engineer":
        return "hand a task to the Claude Code CLI (this spends your Claude Code usage)"
    return f"use the `{tool}` tool"


def who(meta: Mapping[str, Any]) -> str:
    pod = str(meta.get("pod") or "orchestrator")
    return "The agent" if pod == "orchestrator" else f"The **{pod}** agent"


def why_it_stopped(meta: Mapping[str, Any]) -> str:
    """Laya's reason without its own arithmetic: "can destroy a disk — risk 0.99 is at or above this
    agent's gate of 0.80" becomes "can destroy a disk", and the number moves to its own field."""
    reason = str(meta.get("reason") or "")
    reason = re.sub(r"^Laya Guard:\s*", "", reason)
    return re.split(r"\s+—\s+risk\s", reason, maxsplit=1)[0].strip()


def approval_card(
    meta: Mapping[str, Any], *, project: str, where: str, routine: bool, offers_always: str = "", redactions: int = 0, scratch: bool = False, can_stop: bool = False
) -> dict[str, Any]:
    """The prompt for one paused call.

    `routine` is a call below the gate that merely asks because the mode says so (`build`), as opposed
    to one Laya armed. It is quieter on purpose: amber not red, no alarm in the title, and the card's
    caller offers a way to stop being asked about them."""
    score = meta.get("risk_score")
    band = str(meta.get("risk_band") or "")
    label, shown, lang = body_of(meta)
    truncated = len(shown) > MAX_BODY_CHARS
    body = shown[:MAX_BODY_CHARS] + ("\n(truncated — the full call is in the containment view)" if truncated else "")

    lines = []
    if scratch:
        lines.append("🧪 *This session runs in a temporary folder — most likely a test run, not your work.*")
    lines.append(f"{who(meta)} wants to {what_it_wants(meta)} in **{project}**.")
    if meta.get("goal"):
        lines.append(f"> **You asked:** {clip(str(meta['goal']), MAX_CONTEXT_CHARS)}")
    if meta.get("said"):
        lines.append(f"> **It said:** {clip(str(meta['said']), MAX_CONTEXT_CHARS)}")

    fields: list[tuple[str, str, bool]] = [(label, fence(body, lang), False)]
    reason = why_it_stopped(meta)
    if not routine and reason:
        fields.append(("Why it stopped", reason[:1024], False))
    if score is not None and not routine:
        fields.append(("Laya's reading", f"**{band or 'unbanded'}** · {float(score):.2f}", True))
    fields.append(("Where", where[:1024], True))
    if offers_always:
        fields.append(("Always allow", f"`{offers_always[:200]}` — saved to this project's local settings", True))
    if redactions:
        fields.append(("Masked", f"{redactions} credential-shaped span(s) hidden in this view", True))
    return {
        "title": ("🧪 " if scratch else "") + ("Approve this step?" if routine else "⚠ Needs your decision"),
        "description": "\n".join(lines),
        "fields": fields,
        "color": WARN_COLOR if routine else DANGER_COLOR,
        "footer": CONSEQUENCE + (" **Deny & stop** also cancels the turn." if can_stop else ""),
    }


def resolved_card(meta: Mapping[str, Any], *, verdict: str, by: str, when: float | None = None) -> dict[str, Any]:
    """A decided card, collapsed to what a log needs: the verdict, the call, who and when."""
    _, shown, lang = body_of(meta)
    stamp = time.strftime("%H:%M", time.localtime(when or time.time()))
    glyph = {
        "approved": "✅ Allowed", "rejected": "🚫 Denied", "always": "✅ Always allowed", "stopped": "🛑 Denied, and the turn was stopped",
        "auto": "⚡ Allowed; routine steps will now run on their own",
    }.get(verdict, "⌛ No longer waiting")
    return {
        "title": f"{glyph} · {what_it_wants(meta).split(' (')[0]}",
        "description": f"{by} · {stamp}",
        "fields": [("", fence(clip(shown, 200), lang), False)],
        "color": DONE_COLOR if verdict in ("approved", "always", "auto") else (DANGER_COLOR if verdict in ("rejected", "stopped") else MUTED_COLOR),
    }


# --------------------------------------------------------------------------- status, usage, help


def turn_footer(turn: Mapping[str, Any] | None, *, stop: str = "end_turn", seconds: float | None = None) -> str:
    """The receipt under a reply: time, tool calls and tokens. Always shown — a person watching
    their credits should not have to ask what a turn cost."""
    t = dict(turn or {})
    took = fmt_seconds(t.get("seconds") if t.get("seconds") is not None else seconds)
    if stop == "cancelled":
        return f"■ stopped after {took}"
    parts = [f"✓ {took}"]
    calls = int(t.get("calls") or 0)
    if calls:
        parts.append(f"{calls} tool call{'s' if calls != 1 else ''}")
    tin, tout = int(t.get("tokensIn") or 0), int(t.get("tokensOut") or 0)
    if tin or tout:
        parts.append(f"{fmt_tokens(tin)} in · {fmt_tokens(tout)} out")
    if stop not in ("end_turn", "", None):
        parts.append(stop.replace("_", " "))
    return " · ".join(parts)


def quota_fields(quota: Mapping[str, Any] | None) -> list[tuple[str, str, bool]]:
    """The orchestrator's quota pools as gauges, and its latest handoff (`orchestrator/status.py`'s payload)."""
    if not quota:
        return []
    from orchestrator.status import handoff_line, pool_line

    pools = [p for p in quota.get("pools") or [] if p.get("limit") or p.get("used") or p.get("provider_pct") is not None]
    fields: list[tuple[str, str, bool]] = []
    if pools:
        fields.append(("Quota", "```\n" + "\n".join(pool_line(p) for p in pools)[:980] + "\n```", False))
    handoffs = quota.get("handoffs") or []
    if handoffs:
        fields.append(("Last handoff", clip(handoff_line(handoffs[0]), 200), False))
    return fields


def status_card(rows: list[Mapping[str, Any]], *, usage: Mapping[str, Any] | None = None, note: str = "", quota: Mapping[str, Any] | None = None) -> dict[str, Any]:
    """Where every agent stands, from live daemon state. `rows` is one dict per session:
    `project`, `state` (`idle` | `working` | `waiting`), `detail`, `since` (seconds), `pending`, `kind`
    (`chat` | `watched`), `mission`. `quota` (optional) adds the orchestrator's pool gauges."""
    if not rows:
        return {
            "title": "Nothing running",
            "description": "No agent is working right now. Say something here to start a chat." + (f"\n{note}" if note else ""),
            "fields": quota_fields(quota),
            "color": MUTED_COLOR,
            "footer": "Answered from the daemon's own state — no model call.",
        }
    waiting = sum(int(r.get("pending") or 0) for r in rows)
    working = sum(1 for r in rows if r.get("state") == "working")
    headline = (
        f"{waiting} decision{'s' if waiting != 1 else ''} waiting on you"
        if waiting
        else (f"{working} agent{'s' if working != 1 else ''} working" if working else "All idle")
    )
    fields: list[tuple[str, str, bool]] = []
    for row in rows:
        glyph = {"working": "🟢", "waiting": "🟠", "idle": "⚪"}.get(str(row.get("state")), "⚪")
        lines = [f"{glyph} **{row.get('state', 'idle')}**" + (f" for {fmt_seconds(row['since'])}" if row.get("since") else "")]
        if row.get("detail"):
            lines.append(f"now: {clip(str(row['detail']), 120)}")
        if row.get("mission"):
            lines.append(f"asked: “{clip(str(row['mission']), 120)}”")
        if row.get("pending"):
            lines.append(f"🟠 {row['pending']} approval{'s' if row['pending'] != 1 else ''} waiting")
        kind = "your chat" if row.get("kind") == "chat" else "terminal/GUI session"
        fields.append((f"{row.get('project', '?')} · {kind}", "\n".join(lines), False))
    fields += quota_fields(quota)
    footer = "Answered from the daemon's own state — no model call."
    if usage and (usage.get("turns") or 0):
        footer = f"Since the daemon started: {usage['turns']} turn(s) · {fmt_tokens(usage.get('tokensIn'))} in · {fmt_tokens(usage.get('tokensOut'))} out. " + footer
    return {"title": headline, "description": note, "fields": fields, "color": WARN_COLOR if waiting else INFO_COLOR, "footer": footer}


def usage_card(usage: Mapping[str, Any], per_chat: list[Mapping[str, Any]]) -> dict[str, Any]:
    fields: list[tuple[str, str, bool]] = [
        ("Turns", str(usage.get("turns") or 0), True),
        ("Tokens in", fmt_tokens(usage.get("tokensIn")), True),
        ("Tokens out", fmt_tokens(usage.get("tokensOut")), True),
        ("Tool calls", str(usage.get("calls") or 0), True),
        ("Agent time", fmt_seconds(usage.get("seconds")), True),
    ]
    if per_chat:
        top = sorted(per_chat, key=lambda c: -(int(c.get("tokensIn") or 0) + int(c.get("tokensOut") or 0)))[:5]
        fields.append(("Heaviest chats", "\n".join(f"{clip(str(c.get('title') or c.get('project')), 40)} — {fmt_tokens(c.get('tokensIn'))} in · {fmt_tokens(c.get('tokensOut'))} out" for c in top), False))
    return {
        "title": "Usage since the daemon started",
        "description": "What the model calls behind these chats have consumed. Local models cost no credits; cloud and Claude Code runs do.",
        "fields": fields,
        "color": INFO_COLOR,
        "footer": "Counted from each turn's own receipt — no model call to produce this.",
    }


def help_card(operator_id: int, *, chat_ready: bool = True) -> dict[str, Any]:
    talk = (
        "Just write here. Each message starts a **thread** with an agent; reply in the thread to continue. "
        "Messages sent while it is working steer it at its next step."
        if chat_ready
        else "Chat needs a daemon that can start sessions."
    )
    return {
        "title": "How this works",
        "description": f"Only <@{operator_id}> can use this bot.",
        "fields": [
            ("Talk to an agent", talk, False),
            ("Ask where things stand", "“what are you working on?” · `!status` — answered instantly, costs nothing.", False),
            ("Control", "`!stop` cancel the running turn · `!mode auto|build|plan` how often it asks · `!new <folder>` chat in a specific project · `!usage` tokens spent", False),
            ("Same conversation everywhere", "Every conversation has a handle (`c3f9a1e`). `!conversations` lists them — the terminal's, the menu bar's and the web app's too. `!continue <handle>` picks one up in a thread here; turns from the other doors show up in it as they finish. In a terminal: `acp-tui --conversation <handle>`.", False),
            ("Take it back", "`!back` makes the agent forget the last turn (`!back 2` the last two) · `!back list` shows the checkpoints · `!back to <number>` restores one — including the state a `!back` left. Files are not touched.", False),
            ("Approvals", "Only things that need you reach this channel. **Allow routine steps in this chat** on the first card stops the routine ones; destructive calls always still ask.", False),
        ],
        "color": INFO_COLOR,
        "footer": "In a terminal/GUI session's thread: `> steer: <note>` or any reply steers it.",
    }


DOOR_NAMES = {"discord": "Discord", "terminal": "Terminal", "menubar": "Menu bar", "web": "Web app"}


def door_label(origin: str) -> str:
    return DOOR_NAMES.get(origin, origin or "Somewhere")


def conversations_text(rows: list[dict[str, Any]], now: float) -> str:
    """`!conversations`: one line per conversation, newest first, with what to type to continue one."""
    if not rows:
        return "No conversations yet — write a message here to start one."
    lines = ["**Conversations** — shared with the terminal, the menu bar and the web app:"]
    for row in rows:
        age = _ago(now - float(row.get("lastUsed") or now))
        title = str(row.get("title") or "untitled")[:60]
        running = f" · ⏳ running a turn from {door_label(str(row.get('turnOrigin') or ''))}" if row.get("busy") else ""
        turns = int(row.get("turns") or 0)
        lines.append(f"`{row['handle']}` {title} — {turns} turn{'s' if turns != 1 else ''} · from {door_label(str(row.get('origin') or ''))} · {age}{running}")
    lines.append("-# `!continue <handle>` picks one up here · `acp-tui --conversation <handle>` in a terminal")
    return "\n".join(lines)


def mirrored_turn(origin: str, asked: str, reply: str, stop: str) -> list[str]:
    """A turn another door ran in a conversation this thread follows, as messages: who asked what, then
    the reply. Said as someone else's turn, so it never reads as this thread's own."""
    head = f"-# ↪ **{door_label(origin)}** asked:\n> {' '.join((asked or '…').split())[:300]}"
    body = (reply or "").strip() or "*(no reply text)*"
    parts = [head, *split_message(body)]
    if stop and stop != "end_turn":
        parts.append(f"-# stopped: {stop}")
    return parts


def _ago(seconds: float) -> str:
    seconds = max(0, int(seconds))
    if seconds < 60:
        return "just now"
    if seconds < 3600:
        return f"{seconds // 60}m ago"
    if seconds < 86_400:
        return f"{seconds // 3600}h ago"
    return f"{seconds // 86_400}d ago"

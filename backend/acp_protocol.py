"""The ACP wire, as the server speaks it: tool kinds, text and prompt extraction, the specialist-progress renderer,
the recent-context digest, temperature presets, and the guard that keeps stray prints off the protocol's stdout.

Split out of ``acp_server.py``, which re-exports every name here.
"""

import os
import claude_code_tool
import context_offload
import lsp_bridge
import orchestrate_tool

_TOOL_KIND: dict[str, str] = {
    "shell": "execute",
    "python_repl": "execute",
    "file_write": "edit",
    "editor": "edit",
    "apply_patch": "edit",
    "file_read": "read",
    "http_request": "fetch",
    "web_fetch": "fetch",
    "agent_memory": "other",
    "a2a_client": "fetch",
    "architect": "think",
    "debugger": "other",
    claude_code_tool.TOOL_NAME: "execute",
    orchestrate_tool.TOOL_NAME: "execute",
    "context_digest": "think",
    "ollama_resource_manager": "other",
    "omlx_resource_manager": "other",
    context_offload.RETRIEVE_TOOL: "read",
    "present_plan": "switch_mode",
    "ask_user": "other",
    **{t.tool_name: "search" for t in lsp_bridge.LSP_TOOLS},
}


def _tool_kind(tool_name: str) -> str:
    return _TOOL_KIND.get(tool_name, "other")


def _text_of(blocks) -> str:
    return " ".join(b["text"] for b in blocks or [] if isinstance(b, dict) and isinstance(b.get("text"), str)).strip()


def _turn_context(agent) -> tuple[str, str]:
    """``(what the person asked, what the agent said just before the call)`` for an approval card.

    The newest assistant message is the one carrying the tool call being asked about, so its text
    blocks are the agent's own account of why; an older one would be a different step's. The goal is
    the newest user message that is the person's words rather than a tool result. Either may be empty
    — that is reported as empty, not guessed at."""
    goal = said = ""
    seen_assistant = False
    for message in reversed(list(getattr(agent, "messages", None) or [])):
        if not isinstance(message, dict):
            continue
        blocks = message.get("content") or []
        if message.get("role") == "assistant" and not seen_assistant:
            seen_assistant = True
            said = _text_of(blocks)
        elif message.get("role") == "user" and not any(isinstance(b, dict) and "toolResult" in b for b in blocks):
            text = _text_of(blocks)
            if text:
                goal = text
                break
    return goal, said


# How much of a running specialist's latest reasoning one progress update carries.
_REASONING_TAIL_CHARS = 120

# Config option "temperature" exposes strands' set_temperature tool as a
# select instead of a free-float, since SessionConfigOptionSelect only takes
# discrete option ids — matches the granularity a picker in an editor UI
# actually wants (three presets, not a number field).
_TEMPERATURE_PRESETS = [("focused", 0.2, "Focused"), ("balanced", 0.7, "Balanced"), ("creative", 1.2, "Creative")]


def _closest_temperature_preset(temperature: float | None) -> str:
    if temperature is None:
        return "balanced"
    return min(_TEMPERATURE_PRESETS, key=lambda preset: abs(preset[1] - temperature))[0]


def _extract_prompt_text(blocks: list) -> str:
    """Text + @-mentioned content from one prompt turn.

    Previously only TextContentBlock was read — every ResourceContentBlock
    (a plain @-mention, Zed sends just a name/uri/description, no content)
    and EmbeddedResourceContentBlock (Zed inlined the actual file text) was
    silently dropped, so mentioning a file in Zed never actually reached the
    model. An embedded resource's real text is inlined directly; a bare
    reference is turned into a line the model can act on itself (it has
    file_read).
    """
    parts: list[str] = []
    for block in blocks:
        block_type = getattr(block, "type", None)
        if block_type == "text":
            parts.append(block.text)
        elif block_type == "resource_link":
            label = getattr(block, "title", None) or getattr(block, "name", block.uri)
            parts.append(f"[Referenced file: {label} ({block.uri})]")
        elif block_type == "resource":
            resource = block.resource
            uri = getattr(resource, "uri", "unknown")
            text = getattr(resource, "text", None)
            parts.append(f"[Attached: {uri}]\n{text}" if text is not None else f"[Attached binary resource: {uri}]")
    return "\n\n".join(part for part in parts if part)
#: Image types a prompt may attach (what Strands' image block takes), and the largest one accepted.
_IMAGE_FORMATS = {"image/png": "png", "image/jpeg": "jpeg", "image/jpg": "jpeg", "image/gif": "gif", "image/webp": "webp"}
MAX_PROMPT_IMAGE_BYTES = 4 * 1024 * 1024


def _prompt_images(blocks: list) -> tuple[list[dict], list[str]]:
    """The images attached to a prompt (ACP ``image`` blocks), decoded and checked, as ``{"bytes", "format"}``; and a
    reason for each one dropped. Built here from validated bytes — never by passing the client's dict through — so a
    prompt can only ever add an image, not a forged tool call or any other block."""
    import base64
    import binascii

    images: list[dict] = []
    dropped: list[str] = []
    for block in blocks or []:
        if getattr(block, "type", None) != "image":
            continue
        fmt = _IMAGE_FORMATS.get(str(getattr(block, "mime_type", "") or "").lower())
        if fmt is None:
            dropped.append(f"an image of type {getattr(block, 'mime_type', '?')!r} (png, jpeg, gif or webp only)")
            continue
        try:
            data = base64.b64decode(str(getattr(block, "data", "") or ""), validate=True)
        except (binascii.Error, ValueError):
            dropped.append("an image that did not decode")
            continue
        if not data or len(data) > MAX_PROMPT_IMAGE_BYTES:
            dropped.append(f"an image of {len(data) // 1024} KiB (the limit is {MAX_PROMPT_IMAGE_BYTES // (1024 * 1024)} MiB)")
            continue
        images.append({"bytes": data, "format": fmt})
    return images, dropped


#: The longest single JSON-RPC line this child reads from its ACP client (see `_main`).
STDIO_LINE_LIMIT = 32 * 1024 * 1024
_SPECIALIST_CONTEXT_CHARS = 4000


def _recent_context(messages: list, limit: int = _SPECIALIST_CONTEXT_CHARS) -> str:
    """The last turns' text, newest last, for a specialist that starts fresh."""
    parts: list[str] = []
    used = 0
    for message in reversed(messages):
        texts = [b["text"] for b in message.get("content") or [] if isinstance(b, dict) and isinstance(b.get("text"), str)]
        if not texts:
            continue
        line = f"{message.get('role')}: {' '.join(texts)}"
        if used + len(line) > limit:
            break
        parts.append(line)
        used += len(line)
    return "\n".join(reversed(parts))


class _SpecialistProgress:
    """A running specialist's activity, rendered into its tool card: the tools
    it called and the tail of what it is writing."""

    def __init__(self) -> None:
        self.lines: list[str] = []
        self.seen: set[str] = set()
        self.text = ""

    def feed(self, data: dict) -> bool:
        changed = False
        if data.get("type") == "tool_use_stream":
            current = data.get("current_tool_use") or {}
            tool_id = current.get("toolUseId")
            if tool_id and tool_id not in self.seen:
                self.seen.add(tool_id)
                self.lines.append(f"→ {current.get('name') or 'tool'}")
                changed = True
        elif isinstance(data.get("data"), str):
            self.text += data["data"]
            changed = len(self.text) % 48 < len(data["data"])  # re-render every ~48 chars
        return changed

    def render(self, name: str) -> str:
        lines = [f"{name}: " + line for line in self.lines[-8:]]
        tail = " ".join(self.text.split())[-_REASONING_TAIL_CHARS:]
        if tail:
            lines.append(f"thinking: {name} - {tail}")
        return "\n".join(lines)


class _ProtocolStdout:
    """``sys.stdout`` while serving ACP: text writes (``print`` — Strands'
    default PrintingCallbackHandler, strands_tools' rich panels, a stray debug
    line) go to stderr; only the binary ``.buffer`` is the real stdout, which
    acp.stdio's transport writes JSON-RPC frames to.

    Why this exists: ACP frames and ``print()`` shared one stream, so streamed
    reply text printed *without a newline* glued itself onto the next frame
    (``pong{"jsonrpc":...}``). The client could not parse that line, answered
    ``-32700 Parse error`` and dropped the frame — every agent_message_chunk
    could vanish, deterministically with PYTHONUNBUFFERED=1 and at random
    (whenever the 8 KB pipe buffer flushed) without it.
    """

    def __init__(self, protocol_file, stray) -> None:
        # `.buffer` is what acp.stdio's Windows transport writes to;
        # `.fileno()` is what its POSIX path hands to loop.connect_write_pipe —
        # both must be the protocol stream, never stderr.
        self.buffer = protocol_file
        self._protocol = protocol_file
        self._stray = stray

    def fileno(self) -> int:
        return self._protocol.fileno()

    def close(self) -> None:
        self._protocol.close()

    def write(self, text: str) -> int:
        return self._stray.write(text)

    def flush(self) -> None:
        self._stray.flush()

    def isatty(self) -> bool:
        return False

    def __getattr__(self, name):
        return getattr(self._stray, name)


def _protect_protocol_stdout() -> None:
    """Reserve the real stdout for ACP frames and send everything else to stderr.

    fd 1 is re-pointed at stderr too, so a child process that inherits stdout
    (or any C-level write) can't reach the protocol stream either.
    """
    import sys

    sys.stdout.flush()
    protocol_fd = os.dup(1)
    os.dup2(2, 1)
    sys.stdout = _ProtocolStdout(os.fdopen(protocol_fd, "wb", buffering=0), sys.stderr)

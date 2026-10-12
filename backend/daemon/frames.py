"""WebSocket frames <-> the child's stdio, without re-framing.

The daemon proxies lines verbatim. It reads exactly as much JSON as it needs to
answer two questions — which session is this, and what method is it — and then
forwards the original bytes unchanged. It never decodes a message into a typed
object and never re-encodes one.

That is a deliberate choice against the obvious alternative. ACP is a large,
versioned schema (see `acp/` and the `_cordata/*` extension methods): decoding
and re-encoding would mean every protocol addition needing a matching change
here, and a daemon running one version behind would silently mangle messages it
did not recognise. Passing bytes through means `--transport ws` and
`--transport stdio` carry byte-identical semantics, so `events.rs` and `app.rs`
on the Rust side need no change at all, and a future ACP feature works through
the daemon the day it lands.

The cost is that the daemon cannot enforce what it does not parse. It cannot
inspect a tool call or rewrite a permission prompt — so the approval gate, the
Laya routing and the egress policy all stay in the child, where they already
are. This module is transport, and only transport.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any

#: Frames at or above this size are logged as a warning rather than a line.
#: ACP messages are small (a prompt, a tool result, a diff); a megabyte means
#: something is streaming that should have been chunked, and the line-oriented
#: child protocol would be the wrong place to find out.
LARGE_FRAME = 1_000_000


@dataclass(frozen=True)
class Envelope:
    """What the daemon needs to know about a line, and nothing more."""

    session_id: str | None
    method: str | None
    is_request: bool
    request_id: Any = None


def inspect(line: str) -> Envelope:
    """Read enough of one ACP line to route and log it.

    Never raises: a line the daemon cannot parse is still forwarded. A client
    speaking a newer ACP, or a malformed frame a user is debugging, must reach
    the child intact so the child's own error handling — which knows the
    protocol — is what reports the problem.
    """
    try:
        message = json.loads(line)
    except (TypeError, ValueError):
        return Envelope(None, None, False)
    if not isinstance(message, dict):
        return Envelope(None, None, False)

    method = message.get("method")
    params = message.get("params")
    session_id = None
    if isinstance(params, dict):
        session_id = params.get("sessionId") or params.get("session_id")
    # JSON-RPC: a request carries an id, a notification does not. Only requests
    # are answered, and only answers need correlating.
    request_id = message.get("id")
    return Envelope(
        session_id=session_id if isinstance(session_id, str) else None,
        method=method if isinstance(method, str) else None,
        is_request=request_id is not None and method is not None,
        request_id=request_id,
    )


def forward_line(line: str) -> bytes:
    """One line as the bytes the child's stdin expects.

    The child reads newline-delimited JSON (`acp.stdio`), so exactly one `\\n`
    is appended and any trailing whitespace the client sent is dropped — a
    stray `\\r` from a client that speaks CRLF would otherwise terminate the
    child's read on a line it then fails to parse."""
    return (line.rstrip("\r\n") + "\n").encode("utf-8")


def outbound_line(raw: bytes) -> str | None:
    """A chunk from the child's stdout as one frame, or None if it is not one.

    Returns None for a blank line: the child's own writers are careful never to
    emit one (`acp_server.py`'s stdout discipline), but a blank line on the wire
    would become an empty WS frame that a client would try to parse."""
    text = raw.decode("utf-8", errors="replace").strip()
    return text or None

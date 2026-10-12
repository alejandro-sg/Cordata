"""The agent's shell, file editor and web tools, from the Strands SDK's vended tools.

Why. `strands_tools.shell` and `strands_tools.editor` are deprecated (strands-agents-tools 0.8.9: "becomes an error log in
v0.9.0"), and the SDK ships their replacements: `strands.vended_tools.shell` and `file_editor`, which run through
`agent.sandbox` — stateless `sh -c` in its own process group, killed as a tree on timeout *and* when the call is cancelled
(the old tool left a cancelled `npm test` running) — and are a fraction of the size. Measured, tool schema per model call:
shell 2,901 → 535 bytes, editor 5,708 → 1,273: ~1,700 tokens off every call of every session, for the two tools every
turn carries.

What is kept. The model-facing names stay `shell` and `editor`, and `editor` keeps its parameters (`command`, `path`,
`file_text`, `old_str`, `new_str`, `insert_line`, `view_range`) — the approval gate, Laya Guard, the settings rules people
have already written (`shell(git *)`, `editor(src/**)`), checkpoints, the diff preview and the LSP hook all key on those.
The SDK ships the tools; this module only fits two things around them, as hooks, because they differ from what the
harness was built on:

* **A shell result is text with an exit code.** The SDK's reports `{"output": …, "error": …, "exit_code": 3}` as a JSON
  string with status `success` — a failing command would read as a passing one to everything that asks "did it work"
  (outcomes, self-heal, the router's p(ok) learning, the stall guard). `CoreToolHooks` rewrites it, ahead of every
  other `AfterToolCallEvent` hook (explicit `HookOrder`, the SDK's own mechanism), to `Exit Code: N` + the output, status
  `error` on non-zero — and a timeout (the SDK raises) to `Exit Code: 124` with what the command had printed.
* **A stop stops the command.** Strands' cancellation is cooperative: `agent.cancel()` stops the loop around a tool, never
  inside one, and a shell command is a tool that does not look. `HarnessSandbox` is the SDK's own local environment with one
  addition — a running command watches the agent's cancel signal and, when it is set, closes the process stream (which
  kills the whole process group) and returns what it had printed with exit code 130. Esc, `!stop` and "Deny & stop" now
  end a runaway build instead of waiting for it.
* **Defaults that suit a coding agent.** The SDK's shell times out at 120 s; the tool it replaces allowed 900
  (`SHELL_DEFAULT_TIMEOUT`), and a test suite or a build is a normal command. The editor wants absolute paths and answers
  a relative one with an error and a hint, costing a model step: the hook resolves it against the project instead.

The web tools follow the same rule — the SDK ships them, this module fits them:

* **`http_request`** is `strands_tools.http_request`'s replacement (deprecated the same way). It is a thin shim over an
  `httpx` client that the operator supplies; `FreshClient` supplies one *per call* — redirects followed (httpx's default
  is not to), a 30 s ceiling, and the process environment read at call time, because the Agent Vault's proxy and CA
  variables (`vault_proxy.install`) can appear after the session started and a long-lived client would never see
  them. The SDK raises on a 4xx/5xx and drops the body, which is where an API says what was wrong; the hook puts it
  back, fenced as untrusted, and shapes the success case to `HTTP 200 OK` + the few headers that matter + the body
  (the SDK's JSON string with every header costs several times the tokens).
* **`web_fetch`** is new: fetch a page and have an *analyst* answer a prompt about it, so the page never enters the main
  agent's context (`mode="agentic"`, the SDK's default) — one call to a fast model instead of ~12k tokens carried by every
  later call. `context.webFetch: markdown` returns the page as markdown instead (the offloader sets a long one aside).
  Its answer is derived from untrusted text and is fenced like any other fetched content.

Stdlib plus Strands.
"""

from __future__ import annotations

import asyncio
import datetime
import json
import logging
import os
import weakref
from collections import OrderedDict
from collections.abc import AsyncGenerator
from typing import Any

import httpx

from strands.hooks import AfterToolCallEvent, BeforeToolCallEvent, HookProvider, HookRegistry
from strands.models.model import Model
from strands.hooks.registry import HookOrder
from strands.sandbox import ExecutionResult, StreamChunk
from strands.sandbox.not_a_sandbox_local_environment import NotASandboxLocalEnvironment
from strands.vended_plugins.context_injector import ContextInjector
from strands.vended_tools.file_editor import make_file_editor
from strands.vended_tools.http_request import make_http_request
from strands.vended_tools.shell import make_shell
from strands.vended_tools.web_fetch import make_web_fetch

logger = logging.getLogger(__name__)

#: Model-facing name of the file editor. The SDK's own is `file_editor`; `editor` is what the approval gate, the settings
#: rules, Laya Guard and the checkpoints were written against, and what people's saved "always allow" rules say.
EDITOR_NAME = "editor"
editor = make_file_editor(name=EDITOR_NAME)

#: What the model is told, matched to what `CoreToolHooks` makes of the result (the SDK's own description speaks of
#: `output`, `error` and `exit_code` fields that the hook folds into text).
SHELL_DESCRIPTION = (
    "Runs a shell command in the project directory. Returns `Exit Code: N` (non-zero means it failed), then its output, "
    "then its stderr under [stderr]. Each call is a fresh `sh`: `cd` and variables do not persist, so chain with &&. "
    "`timeout` is in seconds (default 900); a command that exceeds it is killed."
)
shell = make_shell(name="shell", description=SHELL_DESCRIPTION)

DEFAULT_TIMEOUT_SECONDS = 900
MAX_TIMEOUT_SECONDS = 7_200

#: Before every other hook at the same event, and after nothing the SDK itself needs first: the result and the input are
#: shaped here so that everything registered at the default order sees the final form.
SHAPE_ORDER = HookOrder.DEFAULT - 50

TIMED_OUT = "SandboxTimeoutError"


def default_timeout() -> int:
    try:
        return max(1, min(MAX_TIMEOUT_SECONDS, int(os.environ.get("SHELL_DEFAULT_TIMEOUT", DEFAULT_TIMEOUT_SECONDS))))
    except ValueError:
        return DEFAULT_TIMEOUT_SECONDS


def absolute(path: str, cwd: str | None = None) -> str:
    """`path` as an absolute path: `~` expanded, a relative one resolved against the project (the process's directory)."""
    expanded = os.path.expanduser(path)
    return expanded if os.path.isabs(expanded) else os.path.normpath(os.path.join(cwd or os.getcwd(), expanded))


def _payload(result: dict) -> dict | None:
    """The `{"output", "error", "exit_code"}` an SDK shell call reports, wherever it put it."""
    for block in result.get("content") or []:
        if not isinstance(block, dict):
            continue
        if isinstance(block.get("json"), dict) and "exit_code" in block["json"]:
            return block["json"]
        text = block.get("text")
        if isinstance(text, str):
            candidate = text[text.find("{") :] if TIMED_OUT in text else text
            try:
                data = json.loads(candidate)
            except ValueError:
                continue
            if isinstance(data, dict) and "exit_code" in data:
                return data
    return None


def render(data: dict, *, note: str = "") -> str:
    """`Exit Code: N`, the command's output, and its stderr under its own label — compact, and the first line is what
    `laya_router.command_ok` and the rest read."""
    parts = [f"Exit Code: {int(data.get('exit_code') or 0)}"]
    if note:
        parts.append(note)
    output = str(data.get("output") or "").rstrip("\n")
    if output:
        parts.append(output)
    error = str(data.get("error") or "").rstrip("\n")
    if error:
        parts.append("[stderr]\n" + error)
    return "\n".join(parts)


def shape_shell_result(result: Any, tool_input: Any = None) -> bool:
    """Rewrite an SDK shell result in place into the text form above. True if it was rewritten."""
    if not isinstance(result, dict):
        return False
    data = _payload(result)
    if data is None:
        return False
    timed_out = any(isinstance(b, dict) and isinstance(b.get("text"), str) and TIMED_OUT in b["text"] for b in result.get("content") or [])
    code = int(data.get("exit_code") or 0)
    seconds = ""
    if timed_out and isinstance(tool_input, dict):
        seconds = str(tool_input.get("timeout") or default_timeout())
    note = f"(timed out after {seconds}s and was killed)" if timed_out else ""
    result["content"] = [{"text": render(data, note=note)}]
    result["status"] = "success" if code == 0 and not timed_out else "error"
    return True


def caseless_glob(path: str) -> str:
    """`~/Downloads/*figma*` → `~/Downloads/*[fF][iI][gG][mM][aA]*`: the last component of a find pattern, matched
    without regard to case. `glob` is case-sensitive even on a case-insensitive disk, and a model asked for "the figma
    benefits" writes lowercase while the file says `Figma 2026 OE Benefits Guide.pdf` — it got "no files" and gave up.
    Only a component that is already a pattern is touched; letters inside an existing `[...]` are left alone."""
    head, tail = os.path.split(path)
    if not any(c in tail for c in "*?["):
        return path
    out, in_class = [], False
    for c in tail:
        if c == "[":
            in_class = True
        elif c == "]":
            in_class = False
        out.append(f"[{c.lower()}{c.upper()}]" if c.isalpha() and not in_class and c.lower() != c.upper() else c)
    return os.path.join(head, "".join(out)) if head else "".join(out)


#: file_read modes that list or describe a file rather than return its contents — left as they are for a PDF too.
_PDF_KEEP_MODES = {"find", "stats", "time_machine"}


def shape_pdf_result(result: Any, tool_input: Any = None) -> bool:
    """Replace what `file_read` made of a PDF with its text. True if it was replaced.

    `view`/`preview` decode the bytes as UTF-8 and fail; `document` returns the raw bytes as a document block, which an
    OpenAI-compatible local server cannot read. Either way the model was left with nothing it could quote."""
    if not isinstance(result, dict) or not isinstance(tool_input, dict):
        return False
    path = tool_input.get("path")
    if not isinstance(path, str) or not path.lower().endswith(".pdf") or tool_input.get("mode") in _PDF_KEEP_MODES:
        return False
    path = os.path.expanduser(path)
    if not os.path.isfile(path):
        return False
    try:
        from pypdf import PdfReader  # noqa: PLC0415 — only a PDF read pays for the import

        pages = PdfReader(path).pages
        text = "\n\n".join(f"[page {i}]\n{(page.extract_text() or '').strip()}" for i, page in enumerate(pages, 1))
    except Exception as error:
        result["status"] = "error"
        result["content"] = [{"text": f"Could not extract text from {path}: {type(error).__name__}: {error}"}]
        return True
    if not text.replace("[page", "").strip(" \n]0123456789"):
        text = f"{path} has {len(pages)} page(s) but no extractable text (likely scanned images)."
    result["status"] = "success"
    result["content"] = [{"text": f"{path} — {len(pages)} page(s), text extracted:\n\n{text}"}]
    return True


CANCELLED_EXIT = 130
CANCEL_POLL_SECONDS = 0.1


class HarnessSandbox(NotASandboxLocalEnvironment):
    """The SDK's host environment (`sh -c`, own process group, killed as a tree on timeout), whose commands also end when
    the owning agent is cancelled. `bind` it to the agent after the agent exists — the sandbox is a constructor argument
    of the agent it watches."""

    def __init__(self) -> None:
        super().__init__()
        self._agent: weakref.ReferenceType | None = None

    def bind(self, agent: Any) -> HarnessSandbox:
        self._agent = weakref.ref(agent)
        return self

    def _cancelled(self) -> bool:
        agent = self._agent() if self._agent is not None else None
        try:
            return agent is not None and agent.cancel_signal.is_set()
        except Exception:
            return False

    async def execute_streaming(self, command: str, **kwargs: Any) -> AsyncGenerator[StreamChunk | ExecutionResult, None]:
        inner = super().execute_streaming(command, **kwargs)
        out: list[str] = []
        err: list[str] = []
        pending: asyncio.Future | None = None
        try:
            while True:
                pending = asyncio.ensure_future(inner.__anext__())
                while True:
                    done, _ = await asyncio.wait({pending}, timeout=CANCEL_POLL_SECONDS)
                    if done:
                        break
                    if self._cancelled():
                        pending.cancel()
                        await asyncio.gather(pending, return_exceptions=True)  # the stream's `finally` kills the group
                        pending = None
                        yield ExecutionResult(exit_code=CANCELLED_EXIT, stdout="".join(out), stderr="".join(err) + "[cancelled: the turn was stopped]")
                        return
                try:
                    item = pending.result()
                except StopAsyncIteration:
                    pending = None
                    return
                pending = None
                if isinstance(item, StreamChunk):
                    (err if item.stream_type == "stderr" else out).append(item.data)
                yield item
        finally:
            if pending is not None and not pending.done():
                pending.cancel()
                await asyncio.gather(pending, return_exceptions=True)
            await inner.aclose()


# ---------------------------------------------------------------------------- the web tools

HTTP_TIMEOUT_SECONDS = 30.0
MAX_REDIRECTS = 5
#: What of an error response's body the model gets back (an API says what was wrong there).
ERROR_BODY_CHARS = 2_000
#: The few response headers worth the tokens; HEAD and OPTIONS (whose point *is* the headers) get them all.
KEPT_HEADERS = ("content-type", "location", "retry-after", "www-authenticate", "link", "content-length")

_ERROR_BODIES: OrderedDict[tuple[str, str], str] = OrderedDict()


class FreshClient:
    """The `httpx` client the SDK's `http_request` is given: a new one for every call (module docstring), closed when the
    response is. Implements exactly what the tool uses of a client — `timeout`, `build_request`, `send`."""

    timeout = httpx.Timeout(HTTP_TIMEOUT_SECONDS)

    def build_request(self, method: str, url: str, *, headers: Any = None, content: Any = None, extensions: Any = None) -> httpx.Request:
        return httpx.Request(method, url, headers=headers, content=content, extensions=extensions)

    async def send(self, request: httpx.Request, *, stream: bool = False) -> httpx.Response:
        client = httpx.AsyncClient(follow_redirects=True, max_redirects=MAX_REDIRECTS, timeout=self.timeout)
        try:
            response = await client.send(request, stream=True)
        except BaseException:
            await client.aclose()
            raise
        if response.status_code >= 400:
            # The SDK raises on this status and never reads the body; the body is what says what was wrong.
            try:
                body = (await response.aread()).decode(response.encoding or "utf-8", errors="replace")
            except Exception:
                body = ""
            if body.strip():
                _ERROR_BODIES[(request.method, str(request.url))] = body[:ERROR_BODY_CHARS]
                while len(_ERROR_BODIES) > 16:
                    _ERROR_BODIES.popitem(last=False)
        close = response.aclose

        async def aclose() -> None:
            try:
                await close()
            finally:
                await client.aclose()

        response.aclose = aclose  # type: ignore[method-assign]
        return response


HTTP_DESCRIPTION = (
    "Makes an HTTP request (GET, POST, PUT, DELETE, PATCH, HEAD, OPTIONS). Returns `HTTP <status>`, the headers that matter "
    "and the body. Follows redirects; 30 s limit. A 4xx or 5xx is an error that includes the response body."
)
http_request = make_http_request(name="http_request", description=HTTP_DESCRIPTION, client=FreshClient())  # type: ignore[arg-type]


class UsageTally:
    """Tokens spent by model calls that happen outside an agent's own metrics — here, the `web_fetch` analyst, which the
    SDK runs as a fresh agent per call and then lets go of. Read by the per-turn receipt and the turn budget."""

    def __init__(self) -> None:
        self.input = 0
        self.output = 0
        self.calls = 0

    @property
    def total(self) -> int:
        return self.input + self.output

    def add(self, usage: Any) -> None:
        if not isinstance(usage, dict):
            return
        self.input += int(usage.get("inputTokens") or 0)
        self.output += int(usage.get("outputTokens") or 0)
        self.calls += 1


class CountingModel(Model):
    """A model that is another model, and writes down what its calls cost (`UsageTally`). The wrapped model is the same
    object the session uses — a `/model` switch, a changed temperature, the context window — so everything but `stream` is
    its."""

    def __init__(self, inner: Model, tally: UsageTally) -> None:
        self._inner = inner
        self._tally = tally

    @property
    def stateful(self) -> bool:  # type: ignore[override]
        return bool(getattr(self._inner, "stateful", False))

    def update_config(self, **model_config: Any) -> None:
        self._inner.update_config(**model_config)

    def get_config(self) -> Any:
        return self._inner.get_config()

    async def count_tokens(self, *args: Any, **kwargs: Any) -> int:  # type: ignore[override]
        return await self._inner.count_tokens(*args, **kwargs)

    def structured_output(self, *args: Any, **kwargs: Any) -> Any:
        return self._inner.structured_output(*args, **kwargs)

    async def stream(self, *args: Any, **kwargs: Any) -> AsyncGenerator[Any, None]:
        async for event in self._inner.stream(*args, **kwargs):
            if isinstance(event, dict) and isinstance(event.get("metadata"), dict):
                self._tally.add(event["metadata"].get("usage"))
            yield event


def web_fetch_tool(*, mode: str = "agentic", model: Any = None, tally: UsageTally | None = None) -> Any:
    """`web_fetch`, with its analyst on `model` (the fast tier when one is configured) or, when None, the session's own.

    With a `tally`, the analyst's token use is written down there. Pass the session's own model (not None) when the tally
    should count a session-model analyst too — the SDK's fallback to "the host agent's model" reaches an object this
    function never sees."""
    if tally is not None and model is not None:
        model = CountingModel(model, tally)
    return make_web_fetch(name="web_fetch", mode="markdown" if mode == "markdown" else "agentic", model=model)


#: The default instance, for a front door with no per-session wiring (the AG-UI path): analyst on the host agent's model.
web_fetch = web_fetch_tool()


def shape_http_result(result: Any, tool_input: Any = None) -> bool:
    """Rewrite an SDK `http_request` result in place: text, `HTTP <status> <reason>`, key headers, the body — and for an
    error, the response body the SDK dropped. True if rewritten."""
    if not isinstance(result, dict):
        return False
    data = tool_input if isinstance(tool_input, dict) else {}
    method, url = str(data.get("method") or "GET").upper(), str(data.get("url") or "")
    texts = [b["text"] for b in result.get("content") or [] if isinstance(b, dict) and isinstance(b.get("text"), str)]
    if result.get("status") == "success":
        for text in texts:
            try:
                payload = json.loads(text)
            except ValueError:
                continue
            if isinstance(payload, dict) and "status" in payload and "body" in payload:
                headers = payload.get("headers") or {}
                shown = headers if method in ("HEAD", "OPTIONS") else {k: headers[k] for k in KEPT_HEADERS if k in headers}
                lines = [f"HTTP {payload.get('status')} {payload.get('status_text') or ''}".rstrip(), *(f"{k}: {v}" for k, v in shown.items())]
                body = str(payload.get("body") or "")
                result["content"] = [{"text": "\n".join(lines) + ("\n\n" + body if body else "")}]
                return True
        return False
    # An error: the SDK's message names the status; the body (kept by `FreshClient`) says why.
    body = _ERROR_BODIES.pop((method, url), None)
    if body is None:
        for key in [k for k in _ERROR_BODIES if k[0] == method and url and k[1].rstrip("/") == url.rstrip("/")]:
            body = _ERROR_BODIES.pop(key)
    if not body or not texts:
        return False
    from prompt_security import wrap_untrusted  # noqa: PLC0415 — keeps this module importable on its own

    result["content"] = [{"text": texts[0]}, {"text": wrap_untrusted(f"response body of the failed request to {url}", body)}]
    return True


def now_text(moment: datetime.datetime | None = None) -> str:
    moment = moment or datetime.datetime.now().astimezone()
    offset = moment.strftime("%z")
    return f"Current date and time: {moment:%Y-%m-%d %H:%M} ({moment:%A}), UTC{offset[:3]}:{offset[3:]}"


def clock() -> ContextInjector:
    """The current date and time, handed to the model with each message the person sends (docs: plugins → Context Injector).

    Replaces the `current_time` tool (deprecated in strands-agents-tools 0.8.9 with exactly this advice): about 20 tokens
    on the turn, instead of a tool schema on every call and a round trip whenever a date matters. Ephemeral — it augments
    the model's input for that call and never enters the stored conversation, so a session resumed next week is told
    the time it is then, not the time it was."""
    return ContextInjector(lambda context: now_text(), name="cordata:clock", trigger="userTurn")


class CoreToolHooks(HookProvider):
    """Fits the SDK's shell, editor and `http_request` to the harness (module docstring). One per agent that carries them.

    `root` is the project directory a relative editor path is resolved against, *as the session names it* — the process's
    own `os.getcwd()` is the physical path (symlinks resolved: `/var/…` is `/private/var/…` on macOS), and a path under
    that spelling would read to Laya Guard as "outside the project"."""

    def __init__(self, root: str | None = None) -> None:
        self._root = root

    def register_hooks(self, registry: HookRegistry, **kwargs: Any) -> None:
        registry.add_callback(BeforeToolCallEvent, self._before, order=SHAPE_ORDER)
        registry.add_callback(AfterToolCallEvent, self._after, order=SHAPE_ORDER)

    def _before(self, event: BeforeToolCallEvent) -> None:
        name = event.tool_use.get("name")
        data = event.tool_use.get("input")
        if not isinstance(data, dict):
            return
        if name == "shell":
            if data.get("timeout") in (None, 0, ""):
                data["timeout"] = default_timeout()
            else:
                try:
                    data["timeout"] = max(1, min(MAX_TIMEOUT_SECONDS, int(data["timeout"])))
                except (TypeError, ValueError):
                    data["timeout"] = default_timeout()
        elif name == EDITOR_NAME and isinstance(data.get("path"), str) and data["path"]:
            data["path"] = absolute(data["path"], self._root)
        elif name == "file_read" and data.get("mode") == "find" and isinstance(data.get("path"), str):
            data["path"] = caseless_glob(data["path"])

    def _after(self, event: AfterToolCallEvent) -> None:
        name = event.tool_use.get("name")
        try:
            if name == "shell":
                shape_shell_result(event.result, event.tool_use.get("input"))
            elif name == "http_request":
                shape_http_result(event.result, event.tool_use.get("input"))
            elif name == "file_read":
                shape_pdf_result(event.result, event.tool_use.get("input"))
        except Exception:
            logger.debug("core_tools: could not shape a %s result", name, exc_info=True)

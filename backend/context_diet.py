"""Spend fewer tokens per turn without taking a capability away.

Where the tokens actually go. An agent turn is a loop, and every model call in it re-sends *everything*:
the system prompt, every tool's schema, and the whole conversation so far. Measured on this harness
(`acp_server._build_session`, no plugins): 29 tools = ~43 KB of schema ≈ **10.8k tokens on every call**, before
a word of conversation — and a ten-tool turn is eleven calls. The conversation is the other multiplier: one
`file_read` of a big file is paid for again on every later call of the turn, and every later turn.

Three levers, all of which leave the agent able to do exactly what it could before:

1. **Cold tools are not sent until a turn needs them.** The model-control and resource tools, the swarm/graph tools and
   the Claude Code CLI are used a few times a session. Each is hidden at the start of a turn unless the person's message mentions its subject
   (or the Laya route says "web"), or the agent used it recently — and the agent can always load one with
   `enable_tools`, which makes it available from its next step. A hidden tool is therefore a one-step detour,
   never a dead end, and a one-line list of what is hidden (names and what they do) rides in the prompt so the
   agent knows to ask.
2. **One tool result cannot swamp the context.** A result over `context.toolOutputChars` keeps its head and its
   tail (an error or an exit code is at one end or the other) and says how much was left out and how to ask for
   a narrower slice.
3. **Old tool output ages.** Past the most recent few messages, a tool result is shortened to a stub and a huge
   tool *input* (a whole file the agent once wrote) likewise. The file is on disk and the command's outcome is
   in the agent's own later messages; what the agent needs from step 3 on turn 12 is not its full output.

What this is not. It changes no decision, removes no approval, and edits no stored history on disk: shortening is
of the copy the model is about to read (`agent.messages` in memory), idempotent (a marker stops it shortening
twice), and structure-preserving — every tool call keeps its matching result, so a provider never sees an
orphaned one. The first message of a conversation (the task, as the person worded it) is never touched.

Stdlib plus Strands' hook types.
"""

from __future__ import annotations

import json
import logging
import re
from collections.abc import Callable
from typing import Any

from context_offload import RETRIEVE_TOOL, is_offloaded

from strands import tool
from strands.hooks import (
    AfterInvocationEvent,
    AfterToolCallEvent,
    BeforeInvocationEvent,
    BeforeModelCallEvent,
    HookProvider,
    HookRegistry,
)
from strands.types.tools import ToolContext

logger = logging.getLogger(__name__)

#: Strands' own `context_manager="auto"` truncates a tool result past 1,500 tokens (keeping a 750-token preview) — defaults
#: its docs report as the best of ContextBench's evaluations. 6,000 characters is that threshold at ~4 characters a
#: token; unlike its head-only preview this keeps the tail too, where an error or an exit code lives.
DEFAULT_TOOL_OUTPUT_CHARS = 6_000
DEFAULT_AGED_OUTPUT_CHARS = 1_200
#: A tool *input* this long (a whole file the agent wrote) is shortened once it is old.
AGED_INPUT_CHARS = 4_000
#: How many of the newest messages are left exactly as they are: the model's next decision is made from
#: these, so they stay in full.
KEEP_RECENT_MESSAGES = 8
#: A cold tool the agent used stays loaded for this many turns — a follow-up ("now do the same for…") must
#: not have to ask for the tool again.
RECENT_TURNS = 3
HEAD_SHARE = 0.55

AGED_MARK = "[shortened to save tokens"
CAPPED_MARK = "characters omitted to save tokens"

#: Tools that are used a few times a session and cost the most per call. `name -> (what it is for, a
#: pattern that means "this turn needs it")`. Patterns are deliberately generous: a false positive costs one
#: tool's schema on one turn, a false negative costs one `enable_tools` step.
#: What a message about *models* looks like — the subject of the resource and model-control tools. Generous on
#: purpose: these are small schemas and "get the qwen model" is as much a request to pull one as "ollama pull".
_MODEL_WORDS = r"\bmodels?\b|ollama|omlx|prism|\b(un)?load\b|\bpull\b|vram|resident|memory budget|qwen|llama|glm|kimi|gemma|mistral|deepseek"

#: What a message that needs the network looks like — the subject of both web tools.
_WEB_WORDS = r"https?:|www\.|\.(com|org|io|net|dev|ai|app)\b|\b(url|web|website|fetch|download|api|endpoint|internet|online|search|look ?up|docs?|documentation|latest|curl|http)\b"

#: What a message about code looks like — the subject of the search and outline tools. Generous: a false positive costs
#: 2.3 KB of schema on one turn, a false negative one `enable_tools` step; "hello" and "what's the weather" do not match.
_CODE_WORDS = (
    r"\b(find|where|search|grep|locate|look ?up|references?|usages?|used|calls?|callers?|defin\w*|declar\w*|implement\w*"
    r"|functions?|methods?|class(es)?|structs?|symbols?|modules?|files?|code|bugs?|fix\w*|refactor\w*|rename\w*|errors?|tests?"
    r"|failing|compile\w*|build|lint\w*|imports?|variables?|types?|outline|read)\b|\.\w{1,4}\b|::|\w\(\)"
)
#: Routes that hand the turn to code work: the search and outline tools come with them whatever the message says.
#: Not Laya's plain "code" route — that is most turns, and the message itself says whether it needs a search.
CODE_ROUTES = frozenset({"debugger", "architect", "team"})
#: The marker `code_nav.LargeFileHooks` leaves on a read it replaced with an outline.
OUTLINE_MARK = "outline instead of its text"

COLD_TOOLS: dict[str, tuple[str, str]] = {
    "code_search": ("search the project's code by pattern (ripgrep)", _CODE_WORDS),
    "code_outline": ("a file's classes/functions with line ranges, or one symbol's source", _CODE_WORDS),
    "http_request": ("HTTP requests to a website or API", _WEB_WORDS),
    "web_fetch": ("read a web page and answer a question about it", _WEB_WORDS),
    "claude_code_engineer": ("hand a large multi-file coding task to the Claude Code CLI (spends Claude Code usage)", r"claude|refactor|rewrite|migrat|\bimplement|multi-?file|across the (code|repo|project)|large (change|task|feature)"),
    "local_swarm": ("a local multi-agent swarm", r"swarm|multi-?agent|parallel agents"),
    "local_graph": ("a local agent graph pipeline", r"\bgraph\b|pipeline|multi-?agent|workflow"),
    "a2a_client": ("call a remote A2A agent", r"a2a|agent[- ]to[- ]agent|remote agent"),
    "skill_forge": ("write or improve a skill", r"\bskills?\b|forge"),
    "proposer": ("propose a change to the harness itself", r"propos|self[- ]?improv|improve (yourself|the harness)"),
    "ollama_resource_manager": ("pull, list or unload Ollama models", _MODEL_WORDS),
    "omlx_resource_manager": ("load or unload oMLX/Prism models", _MODEL_WORDS),
    "mcp_resources": ("list and read MCP servers' resources and resource templates", r"\bmcp\b|resources?|templates?|\bserver|schema|database|postgres|github"),
    "environment_profile": ("toolchain versions, dependency manifests and memory of this project and machine", r"environment|toolchains?|versions?|installed|rustc|node|python3?|\bpip\b|cargo|dependenc|unified memory|\bram\b|machine|profile"),
    "context_digest": ("digest a long file or log locally", r"digest|summari[sz]e|long (file|log|output|document)|\btl;?dr"),
    "set_sandbox_mode": ("change the sandbox mode", r"sandbox"),
    "open_game": ("open a game", r"\bgames?\b|\bplay\b"),
    "set_memory_window": ("change how much history is kept", r"memory window|context window|history|forget"),
    "set_active_model": ("switch the model", _MODEL_WORDS + r"|switch to|use (a )?(different|other|bigger|smaller)"),
    "set_temperature": ("change the sampling temperature", r"temperature|creative|deterministic|focused"),
    "context_usage": ("report context-window usage", r"context|tokens?|usage|how much"),
    "list_models": ("list the available models", _MODEL_WORDS + r"|available"),
    # Nothing to read back until something has been set aside: hidden until `context_offload` offloads a result (or the
    # history already holds one), loaded the moment it does — see `ContextDiet._after_tool`. Never listed in the prompt.
    RETRIEVE_TOOL: ("read back tool output that was set aside", r"(?!)"),
}
#: Cold tools the diet loads by itself, so the "not loaded" listing does not mention them.
AUTO_LOADED = frozenset({RETRIEVE_TOOL})
_COLD_RE = {name: re.compile(pattern, re.IGNORECASE) for name, (_, pattern) in COLD_TOOLS.items()}


# ---------------------------------------------------------------------------- pure helpers


def shorten(text: str, limit: int, *, what: str = "output") -> str:
    """`text` cut to about `limit` characters, keeping the head and the tail — an error line, a summary or an
    exit code is at one end — and saying exactly how much is gone. Text already within the limit, or already
    shortened, is returned as it is."""
    if len(text) <= limit or AGED_MARK in text or CAPPED_MARK in text:
        return text
    head = max(1, int(limit * HEAD_SHARE))
    tail = max(1, limit - head)
    omitted = len(text) - head - tail
    return (
        f"{text[:head].rstrip()}\n…[{omitted:,} {CAPPED_MARK}; {len(text):,} in all — read a narrower slice "
        f"(a line range, a grep, head/tail) if you need the middle]…\n{text[-tail:].lstrip()}"
    )


def _shorten_aged(text: str, limit: int) -> str:
    if len(text) <= limit or AGED_MARK in text or CAPPED_MARK in text:
        return text
    head = max(1, int(limit * HEAD_SHARE))
    tail = max(1, limit - head)
    return f"{text[:head].rstrip()}\n…{AGED_MARK}: {len(text) - head - tail:,} of {len(text):,} characters of earlier output omitted]…\n{text[-tail:].lstrip()}"


def _block_text(block: dict[str, Any]) -> str | None:
    if isinstance(block.get("text"), str):
        return block["text"]
    if "json" in block:
        try:
            return json.dumps(block["json"], ensure_ascii=False, default=str)
        except (TypeError, ValueError):
            return None
    return None


def cap_result(result: Any, limit: int) -> tuple[Any, int]:
    """A tool result with every text/json block over `limit` shortened. `(result, characters saved)`; the same
    object back, untouched, when nothing was over."""
    if not isinstance(result, dict) or limit <= 0:
        return result, 0
    content = result.get("content")
    if not isinstance(content, list):
        return result, 0
    saved = 0
    out: list[Any] = []
    for block in content:
        text = _block_text(block) if isinstance(block, dict) else None
        if text is not None and len(text) > limit:
            cut = shorten(text, limit)
            if cut is not text:
                saved += len(text) - len(cut)
                out.append({"text": cut})
                continue
        out.append(block)
    if not saved:
        return result, 0
    return {**result, "content": out}, saved


def _age_input(value: Any, limit: int) -> tuple[Any, int]:
    """Shorten any string deep in a tool input that is over `limit`; the shape (dict/list/str) is kept."""
    if isinstance(value, str):
        cut = _shorten_aged(value, limit)
        return cut, len(value) - len(cut)
    if isinstance(value, dict):
        saved, out = 0, {}
        for key, item in value.items():
            out[key], n = _age_input(item, limit)
            saved += n
        return (out, saved) if saved else (value, 0)
    if isinstance(value, list):
        saved, out_list = 0, []
        for item in value:
            new, n = _age_input(item, limit)
            out_list.append(new)
            saved += n
        return (out_list, saved) if saved else (value, 0)
    return value, 0


def age_messages(messages: list[Any], *, keep: int = KEEP_RECENT_MESSAGES, result_limit: int = DEFAULT_AGED_OUTPUT_CHARS, input_limit: int = AGED_INPUT_CHARS) -> int:
    """Shorten old tool output and old huge tool inputs, in place. Returns characters saved.

    The first message and the newest `keep` are left alone. Only the *text* inside tool results and tool-use
    inputs changes: every block stays, so each tool call still has its result."""
    if result_limit <= 0 or len(messages) <= keep + 1:
        return 0
    saved = 0
    for message in messages[1 : len(messages) - keep]:
        if not isinstance(message, dict):
            continue
        for block in message.get("content") or []:
            if not isinstance(block, dict):
                continue
            result = block.get("toolResult")
            if isinstance(result, dict) and isinstance(result.get("content"), list):
                for index, part in enumerate(result["content"]):
                    text = _block_text(part) if isinstance(part, dict) else None
                    if text is not None and len(text) > result_limit:
                        cut = _shorten_aged(text, result_limit)
                        if cut is not text:
                            saved += len(text) - len(cut)
                            result["content"][index] = {"text": cut}
            use = block.get("toolUse")
            if isinstance(use, dict) and "input" in use:
                new_input, n = _age_input(use["input"], input_limit)
                if n:
                    use["input"] = new_input
                    saved += n
    return saved


def _result_text(result: Any) -> str:
    if not isinstance(result, dict):
        return ""
    return " ".join(block.get("text", "") for block in result.get("content") or [] if isinstance(block, dict) and isinstance(block.get("text"), str))


def cold_for(text: str, *, cold: dict[str, Any], recent: set[str], route: str | None = None) -> list[str]:
    """Which of the loaded cold tools this turn can do without: all of them, minus any the person's message
    mentions the subject of, any used in the last few turns, and the two web tools on a "web" route."""
    keep = set(recent)
    if route == "web":
        keep.update(("http_request", "web_fetch"))
    if route in CODE_ROUTES:
        keep.update(("code_search", "code_outline"))
    return [name for name in cold if name not in keep and not _COLD_RE[name].search(text or "")]


# ---------------------------------------------------------------------------- the hook

#: One diet per agent, so the `enable_tools` tool — a module-level function the model calls — can find the
#: one that hid its tools. Keyed by `id(agent)`, as `laya_router.RouterHooks` keys its turns.
_ACTIVE: dict[int, "ContextDiet"] = {}


class ContextDiet(HookProvider):
    """The three levers above, as one hook provider. `scope_tools=False` for a sub-agent whose kit is fixed."""

    def __init__(
        self,
        preferences: Callable[[], Any] | None = None,
        *,
        scope_tools: bool = True,
        route: Callable[[Any], str | None] | None = None,
    ) -> None:
        self._preferences = preferences
        self.scope_tools = scope_tools
        self._route = route
        self._hidden: dict[str, Any] = {}
        self._block: dict | None = None
        self._turn = 0
        self._used: dict[str, int] = {}
        self._offloaded = False
        #: What this diet has saved, for a status line or a test. Characters, not tokens: the honest unit.
        self.saved = {"results": 0, "aged": 0, "hidden_tools": 0}

    # -- settings --------------------------------------------------------------

    def _get(self, key: str, default: Any) -> Any:
        try:
            value = self._preferences().get(key, default) if self._preferences is not None else default
        except Exception:
            return default
        return default if value is None else value

    def _limit(self, key: str, default: int) -> int:
        try:
            return int(float(self._get(key, default)))
        except (TypeError, ValueError):
            return default

    # -- wiring ----------------------------------------------------------------

    def register_hooks(self, registry: HookRegistry, **kwargs: Any) -> None:
        registry.add_callback(AfterToolCallEvent, self._after_tool)
        registry.add_callback(BeforeModelCallEvent, self._before_model)
        if self.scope_tools:
            registry.add_callback(BeforeInvocationEvent, self._start_turn)
            registry.add_callback(AfterInvocationEvent, self._end_turn)

    # -- lever 2: cap one result ------------------------------------------------

    def _after_tool(self, event: AfterToolCallEvent) -> None:
        name = str(event.tool_use.get("name") or "")
        if name:
            self._used[name] = self._turn
        if is_offloaded(event.result):
            # `context_offload` set this result aside: the agent will want to read part of it back.
            self._offloaded = True
            if RETRIEVE_TOOL in self._hidden:
                self.enable(event.agent, [RETRIEVE_TOOL])
        if name in ("file_read", "editor") and "code_outline" in self._hidden and OUTLINE_MARK in _result_text(event.result):
            # A long file came back as its outline (code_nav.LargeFileHooks), which says to read a symbol with code_outline.
            self.enable(event.agent, ["code_outline"])
        capped, saved = cap_result(event.result, self._limit("context.toolOutputChars", DEFAULT_TOOL_OUTPUT_CHARS))
        if saved:
            event.result = capped
            self.saved["results"] += saved

    # -- lever 3: age the history ------------------------------------------------

    def _before_model(self, event: BeforeModelCallEvent) -> None:
        limit = self._limit("context.agedOutputChars", DEFAULT_AGED_OUTPUT_CHARS)
        try:
            self.saved["aged"] += age_messages(event.agent.messages, result_limit=limit)
        except Exception:
            logger.debug("context diet: aging failed", exc_info=True)

    # -- lever 1: cold tools ------------------------------------------------------

    def _start_turn(self, event: BeforeInvocationEvent) -> None:
        from laya_triage import user_text  # noqa: PLC0415 — keeps this module importable without the router

        text = user_text(event.messages)
        if text is None or text.startswith("[self-heal]"):
            return  # an approval resume or a self-heal continuation: the same turn, with the same tools
        agent = event.agent
        self._restore(agent)
        self._turn += 1
        if self._get("context.coldTools", True) is False:
            return
        registry = agent.tool_registry.registry
        loaded = {name: tool_obj for name, tool_obj in registry.items() if name in COLD_TOOLS}
        recent = {name for name, turn in self._used.items() if self._turn - turn <= RECENT_TURNS}
        if self._offloaded or self._history_has_offloads(agent):
            recent.add(RETRIEVE_TOOL)
        route = None
        if self._route is not None:
            try:
                route = self._route(agent)
            except Exception:
                route = None
        hide = cold_for(text, cold=loaded, recent=recent, route=route)
        if not hide:
            return
        for name in hide:
            self._hidden[name] = registry.pop(name)
        self.saved["hidden_tools"] += len(hide)
        _ACTIVE[id(agent)] = self
        listed = [name for name in hide if name not in AUTO_LOADED]
        if not listed:
            return
        try:
            from laya_router import add_prompt_block  # noqa: PLC0415

            listing = "; ".join(f"{name} ({COLD_TOOLS[name][0]})" for name in listed)
            self._block = add_prompt_block(
                agent,
                "Tools not loaded this turn, to keep requests small: " + listing + ". "
                "If you need one, call enable_tools with its name and it is available from your next step.",
            )
        except Exception:
            logger.debug("context diet: could not add the tool listing", exc_info=True)

    def _history_has_offloads(self, agent: Any) -> bool:
        """Whether the conversation (a resumed session's, say) already holds a result `context_offload` set aside."""
        for message in getattr(agent, "messages", None) or []:
            for block in (message.get("content") or []) if isinstance(message, dict) else []:
                if isinstance(block, dict) and is_offloaded(block.get("toolResult")):
                    self._offloaded = True
                    return True
        return False

    def _end_turn(self, event: AfterInvocationEvent) -> None:
        stop_reason = getattr(event.result, "stop_reason", None)
        if stop_reason in ("interrupt", "checkpoint") or event.resume is not None:
            return  # the same turn continues (an approval, a checkpoint pause, a self-heal resume)
        self._restore(event.agent)

    def _restore(self, agent: Any) -> None:
        for name, tool_obj in self._hidden.items():
            agent.tool_registry.registry.setdefault(name, tool_obj)
        self._hidden.clear()
        if self._block is not None:
            try:
                from laya_router import remove_prompt_block  # noqa: PLC0415

                remove_prompt_block(agent, self._block)
            except Exception:
                logger.debug("context diet: could not remove the tool listing", exc_info=True)
            self._block = None
        _ACTIVE.pop(id(agent), None)

    def enable(self, agent: Any, names: list[str]) -> tuple[list[str], list[str]]:
        """Load hidden tools for the rest of this turn. `(loaded, unknown)`."""
        loaded, unknown = [], []
        for raw in names:
            name = str(raw).strip()
            tool_obj = self._hidden.pop(name, None)
            if tool_obj is not None:
                agent.tool_registry.registry[name] = tool_obj
                self._used[name] = self._turn
                loaded.append(name)
            elif name in agent.tool_registry.registry:
                loaded.append(name)  # already loaded: not an error
            else:
                unknown.append(name)
        return loaded, unknown


@tool(context=True)
def enable_tools(names: list[str], tool_context: ToolContext) -> str:
    """Load tools that are listed as "not loaded this turn", so you can use them from your next step.

    Args:
        names: The tool names to load, exactly as listed.
    """
    diet = _ACTIVE.get(id(tool_context.agent))
    if diet is None:
        return "Every tool is already loaded."
    loaded, unknown = diet.enable(tool_context.agent, list(names or []))
    parts = []
    if loaded:
        parts.append(f"Loaded: {', '.join(loaded)}. Use them on your next step.")
    if unknown:
        parts.append(f"Not a hidden tool: {', '.join(unknown)}.")
    return " ".join(parts) or "Nothing to load."

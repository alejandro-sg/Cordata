"""Shared building blocks for this deployment's multi-agent tools
(local_swarm_tool.py, local_graph_tool.py) — both wrap a native
strands.multiagent primitive directly rather than strands_tools.swarm (see
local_swarm_tool.py's module docstring for why), so both need the same
local-model construction, the same sub-agent tool safety rule, and the same
stream-event-to-progress-log translation. Kept here once instead of
duplicated across the two tool files.
"""

import logging
from typing import Any

from strands import Agent
from strands.agent.conversation_manager import SummarizingConversationManager
from strands.models.openai import OpenAIModel

from settings import OMLX_BASE_URL, OMLX_MODEL_ID

DEFAULT_SYSTEM_PROMPT = "You are a helpful AI assistant specializing in collaborative problem solving."

# max_tokens deliberately raised well above oMLX's own server-side default —
# without this, a genuinely long reply (e.g. writing real dashboard code)
# hits MaxTokensReachedException mid-generation and gets silently truncated,
# leaving only a partial message in history. Doesn't affect latency until
# the model actually needs that many tokens.
DEFAULT_MAX_TOKENS = 16384

# Generous replacements for strands.multiagent.{Swarm,Graph}'s own timeout
# defaults (300s/900s), which are tuned for fast cloud models. Observed
# live: a Swarm node hit "execution timed out after 300.0s" on this
# deployment's local 27B model doing multi-step reasoning, which tore down
# the HTTP stream mid-response and surfaced as a bare "network error" on
# the frontend with no indication the real cause was a timeout deep inside
# the swarm/graph. Same reasoning as the undici body-timeout fix in
# frontend/app/api/copilotkit/[[...route]]/route.ts.
DEFAULT_NODE_TIMEOUT = 1800.0
DEFAULT_EXECUTION_TIMEOUT = 3600.0

# Tools no sub-agent may ever be given. main.py's HITL gating
# (ToolBehavior(interrupt_on_call=True), SkillValidationHooks) is wired onto
# the *outer* StrandsAgent/AG-UI layer only — a sub-agent built here is a
# plain strands.Agent constructed directly in Python, with no AG-UI
# transport wrapping it and no session of its own, so there is no mechanism
# for it to pause and show an approval card the way the top-level agent
# does. Giving a sub-agent shell/python_repl/file_write/editor (or a
# skills_generated/* tool, gated the same way) would mean genuinely ungated
# host access — silently denying them here is the safe default until (if
# ever) a real nested-interrupt path exists for multi-agent tool calls.
# `editor` belongs in this set for the same reason as file_write: per its
# own module docstring (strands_tools/editor.py) it can Create/Replace/
# Insert file content, not just view it — real host-filesystem writes, even
# though (worth flagging separately) main.py's own _APPROVAL_REQUIRED_TOOLS
# doesn't gate it for the top-level agent either. Resolved lazily against
# agent_common.GENERATED_TOOL_NAMES to avoid a circular import (agent_common
# imports these tool modules at load time).
DENIED_TOOLS = {"shell", "python_repl", "file_write", "editor"}

# Tools reserved for the top-level orchestrator, excluded even from the
# "give every safe tool" default below (see resolve_tools) and even if a
# spec explicitly asks for one by name. local_swarm/local_graph: a sub-agent
# recursively spinning up its own team is unbounded fan-out with no limit on
# depth. skill_forge/proposer: drafting new tools or harness changes is a
# self-improvement decision for the orchestrator, not a narrow specialist —
# and both already refuse to write anything themselves (see their own
# module docstrings), so a sub-agent given either could still only ever
# hand a draft back up, never write it; excluded anyway, for the same
# "not a narrow specialist's job" reasoning as skill_forge, and because
# proposer's own regression-test gate shells out via subprocess.run, which
# resolve_tools has no visibility into if a sub-agent somehow reached it.
# The model/temperature/memory-window/list_models/context_usage tools
# mutate process-wide or session-wide config — a sub-agent silently
# switching the active model mid-swarm would be a surprising, hard-to-trace
# side effect.
ORCHESTRATOR_ONLY_TOOLS = {
    "local_swarm",
    "local_graph",
    "skill_forge",
    "proposer",
    "list_models",
    "set_active_model",
    "set_temperature",
    "set_memory_window",
    "context_usage",
    "set_sandbox_mode",
}


def build_local_model() -> OpenAIModel:
    from model_control_tool import _context_window_limit

    return OpenAIModel(
        client_args={"api_key": "not-needed", "base_url": OMLX_BASE_URL},
        model_id=OMLX_MODEL_ID,
        params={"max_tokens": DEFAULT_MAX_TOKENS},
        context_window_limit=_context_window_limit(OMLX_MODEL_ID),
    )


def build_conversation_manager() -> SummarizingConversationManager:
    """Same strategy the top-level agent gets (agent_common.py) — summarize
    older turns instead of silently dropping them. Strands' own default
    when a sub-agent gets none (SlidingWindowConversationManager) just
    truncates once ~40 messages pass, no compaction — fine for a one-shot
    reply, lossy for a sub-agent mid-way through several handoffs and tool
    calls in a long-running swarm/graph. No summarization_agent given, so
    Strands uses the sub-agent's own (local) model to write the summary."""
    return SummarizingConversationManager(preserve_recent_messages=10)


def resolve_tools(tool_names: list[str] | None, caller: Agent, tool_label: str) -> list:
    import agent_common

    denied = DENIED_TOOLS | agent_common.GENERATED_TOOL_NAMES | ORCHESTRATOR_ONLY_TOOLS
    available = caller.tool_registry.registry

    if tool_names is None:
        # No "tools" key at all: default to every generically-useful tool
        # the caller has, rather than leaving the sub-agent toolless — the
        # exact failure mode observed the first time this was tried (a
        # researcher sub-agent with no explicit tools reached for shell
        # instead of http_request). An explicit empty list ([]) is treated
        # differently below — that's a deliberate "give this one nothing".
        return [tool for name, tool in available.items() if name not in denied]

    requested_denied = set(tool_names) & denied
    allowed_names = [name for name in tool_names if name not in denied]
    resolved = [available[name] for name in allowed_names if name in available]
    missing = set(allowed_names) - set(available)

    logger = logging.getLogger(__name__)
    if requested_denied:
        logger.warning(
            "%s: denying tool(s) reserved for the orchestrator: %s — sub-agents have no "
            "human-in-the-loop gate and may not recursively spawn their own sub-agent team",
            tool_label,
            sorted(requested_denied),
        )
    if missing:
        logger.warning("%s: skipping unknown tool names %s", tool_label, sorted(missing))
    return resolved


def build_agent_nodes(agent_specs: list[dict[str, Any]], caller: Agent, tool_label: str) -> dict[str, Agent]:
    """One Agent per spec, keyed by its (de-duplicated) name."""
    nodes: dict[str, Agent] = {}
    for i, spec in enumerate(agent_specs):
        if not isinstance(spec, dict):
            raise ValueError(f"Agent specification {i} must be a dictionary")

        name = spec.get("name") or f"agent_{i + 1}"
        while name in nodes:
            name = f"{name}_2"

        nodes[name] = Agent(
            name=name,
            system_prompt=spec.get("system_prompt") or DEFAULT_SYSTEM_PROMPT,
            model=build_local_model(),
            tools=resolve_tools(spec.get("tools"), caller, tool_label),
            conversation_manager=build_conversation_manager(),
            callback_handler=None,
        )
    return nodes


def summarize_node_result(node_result: Any) -> str:
    # AgentResult has no .content of its own — the text lives in
    # .message["content"], the same plain dict-block shape every Strands
    # message uses elsewhere in this codebase (role/content/metadata).
    result = getattr(node_result, "result", None)
    message = getattr(result, "message", None) or {}
    content = message.get("content") or []
    return "".join(block.get("text", "") for block in content if isinstance(block, dict) and block.get("text"))


def progress_entry(event: dict[str, Any]) -> dict[str, Any] | None:
    """One stream_async() event (from either Swarm or Graph — both emit the
    same multiagent_* event shapes) as a small JSON-serializable log entry
    for the frontend's live sub-agent view.

    Drops multiagent_node_stream (per-token internals of each sub-agent's
    own reply — most of it is far too chatty for a progress log, and its
    payload can carry live Agent object references that aren't JSON-
    serializable anyway) *except* for the reasoning-text slice of it, which
    reasoning_delta() below pulls out separately for the frontend's per-node
    "thinking" tab — and multiagent_result (the tool's own return value
    already carries that summary).
    """
    event_type = event.get("type")
    if event_type == "multiagent_node_start":
        return {"type": "node_start", "node": event.get("node_id")}
    if event_type == "multiagent_node_stop":
        return {"type": "node_stop", "node": event.get("node_id"), "summary": str(event.get("node_result", ""))[:500]}
    if event_type == "multiagent_handoff":
        return {
            "type": "handoff",
            "from": list(event.get("from_node_ids") or []),
            "to": list(event.get("to_node_ids") or []),
            "message": str(event.get("message", ""))[:500],
        }
    return None


# How many new reasoning characters to accumulate, per node, before the next
# {"state": ...} yield — reasoning arrives as many small per-token deltas
# (strands.types._events.ReasoningTextStreamEvent), and every yield resends
# the *entire* accumulated state as a fresh snapshot (ag_ui_strands has no
# diffing — confirmed reading its agent.py), so yielding on every single
# token would mean re-sending the whole growing reasoning transcript, for
# every node, many times a second. This throttles the update cadence without
# dropping any text — deltas keep accumulating in the caller's `reasoning`
# dict between yields, nothing is lost, only how often the snapshot goes out.
REASONING_YIELD_CHARS = 24


def reasoning_delta(event: dict[str, Any]) -> tuple[str, str] | None:
    """(node_id, text) if this multiagent_node_stream event is a reasoning-
    text delta from that node's own model, else None — for anything else
    (the node's regular reply text, tool-call deltas, ...) as well as any
    non-multiagent_node_stream event.
    """
    if event.get("type") != "multiagent_node_stream":
        return None
    inner = event.get("event") or {}
    if not inner.get("reasoning"):
        return None
    delta = inner.get("reasoningText")
    node_id = event.get("node_id")
    if not delta or not node_id:
        return None
    return node_id, delta

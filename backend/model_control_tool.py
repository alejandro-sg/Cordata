"""Lets the agent (and, via main.py's REST routes + AG-UI shared state, the
frontend) see and change the runtime config of the local oMLX-backed agent:
which model is active, its sampling temperature, how much conversation
history stays unsummarized, and current token/context-window usage.

oMLX can have several models loaded at once (query its own /v1/models) — the
model switch doesn't load/unload anything, it only points this backend's
OpenAIModel at a different already-loaded model_id, via Strands' own
``Model.update_config()``. Every setter here follows the same shape: mutate
the live per-thread Agent (via ``tool_context.agent``, so it applies for the
rest of this conversation with no restart) and also mutate the matching
``agent_common.ACTIVE_*`` global (so a *new* session started afterward
inherits the change too, until the backend restarts).
"""

import json
import urllib.request
from urllib.error import URLError

from strands import tool
from strands.types.tools import ToolContext

from settings import OMLX_BASE_URL


def _fetch_model_catalog() -> list[dict]:
    """Query oMLX's own OpenAI-compatible /models endpoint, raw entries.

    Plain urllib, not the http_request tool: this needs to run as an ordinary
    Python call (from main.py's REST route and from this module's own
    tools), not as a gated agent tool call.
    """
    try:
        with urllib.request.urlopen(f"{OMLX_BASE_URL}/models", timeout=5) as response:
            payload = json.load(response)
    except (URLError, TimeoutError, json.JSONDecodeError):
        return []
    return payload.get("data", [])


def _fetch_available_models() -> list[str]:
    return [entry["id"] for entry in _fetch_model_catalog()]


def _context_window_limit(model_id: str) -> int | None:
    """oMLX reports each model's real context length as max_model_len — used
    to set OpenAIModel's context_window_limit so estimate_utilization()
    (and the frontend's usage display) reflect the actual loaded model
    instead of Strands' generic fallback constant."""
    for entry in _fetch_model_catalog():
        if entry.get("id") == model_id:
            return entry.get("max_model_len")
    return None


@tool
def list_models() -> str:
    """List the model ids currently being served by the local oMLX server.

    Use this before set_active_model, to confirm the model id you want to
    switch to is actually loaded and available right now.
    """
    models = _fetch_available_models()
    if not models:
        return f"No models found — is oMLX running at {OMLX_BASE_URL}?"
    return "Available models: " + ", ".join(models)


@tool(context=True)
def set_active_model(model_id: str, tool_context: ToolContext) -> str:
    """Switch this conversation's model to a different one already loaded in oMLX.

    Takes effect immediately for the rest of this session — no restart
    needed. Also becomes the default for any new session started after this
    call, until changed again or the backend restarts.

    Args:
        model_id: A model id from list_models' output, exactly as listed.
    """
    available = _fetch_available_models()
    # Fail closed, not open: `available` is also empty when oMLX is simply
    # unreachable (_fetch_model_catalog swallows URLError/TimeoutError/
    # JSONDecodeError into []) — the previous `if available and ...` read
    # that the same way as "catalog fetched, genuinely zero models" and let
    # *any* string through unchecked in exactly the case there's no way to
    # verify it. A transient oMLX hiccup should mean "try again", not "the
    # active model is now silently whatever string was passed in."
    if not available:
        return f"Could not reach oMLX at {OMLX_BASE_URL} to verify '{model_id}' is loaded — refusing to switch blind. Try list_models first, or retry once oMLX responds."
    if model_id not in available:
        return f"'{model_id}' is not currently loaded in oMLX. Available: {', '.join(available)}"

    config_update: dict = {"model_id": model_id}
    limit = _context_window_limit(model_id)
    if limit is not None:
        config_update["context_window_limit"] = limit
    tool_context.agent.model.update_config(**config_update)

    import agent_common

    agent_common.ACTIVE_MODEL_ID = model_id

    return f"Active model switched to '{model_id}'."


@tool(context=True)
def set_temperature(temperature: float, tool_context: ToolContext) -> str:
    """Set the sampling temperature for this conversation's model.

    Lower is more deterministic/focused, higher is more varied. Takes effect
    on the next reply; also becomes the default for new sessions.

    Args:
        temperature: A value from 0.0 (deterministic) to 2.0 (very random).
    """
    if not 0.0 <= temperature <= 2.0:
        return "Error: temperature must be between 0.0 and 2.0."

    current_params = dict(tool_context.agent.model.get_config().get("params") or {})
    current_params["temperature"] = temperature
    tool_context.agent.model.update_config(params=current_params)

    import agent_common

    agent_common.ACTIVE_TEMPERATURE = temperature

    return f"Temperature set to {temperature}."


@tool(context=True)
def set_memory_window(preserve_recent_messages: int, tool_context: ToolContext) -> str:
    """Set how many recent messages stay verbatim before older ones get
    summarized by this conversation's SummarizingConversationManager.

    Higher keeps more literal detail in context (costs more tokens per
    turn); lower summarizes sooner (cheaper, less literal recall of older
    turns). Takes effect immediately; also becomes the default for new
    sessions.

    Args:
        preserve_recent_messages: How many of the most recent messages to
            always keep verbatim. Must be at least 2.
    """
    if preserve_recent_messages < 2:
        return "Error: preserve_recent_messages must be at least 2."

    tool_context.agent.conversation_manager.preserve_recent_messages = preserve_recent_messages

    import agent_common

    agent_common.ACTIVE_MEMORY_WINDOW = preserve_recent_messages

    return f"Memory window set to {preserve_recent_messages} messages."


@tool(context=True)
def context_usage(tool_context: ToolContext) -> str:
    """Report this conversation's most recent token usage and the active
    model's context window limit.

    Returns a JSON object: input_tokens, output_tokens, total_tokens (from
    the most recent model reply that reported usage), and
    context_window_limit (the active model's max context length).
    """
    usage: dict = {}
    for message in reversed(tool_context.agent.messages):
        candidate = (message.get("metadata") or {}).get("usage")
        if candidate:
            usage = candidate
            break

    return json.dumps(
        {
            "input_tokens": usage.get("inputTokens"),
            "output_tokens": usage.get("outputTokens"),
            "total_tokens": usage.get("totalTokens"),
            "context_window_limit": tool_context.agent.model.get_config().get("context_window_limit"),
        }
    )

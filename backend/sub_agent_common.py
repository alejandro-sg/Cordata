"""Shared building blocks for the harness's delegated model calls: the sub-agent model (same provider as the
orchestrator, at a tier), the summarising conversation manager, and the sub-agent tool safety rule
(``resolve_tools``: ``DENIED_TOOLS`` and ``ORCHESTRATOR_ONLY_TOOLS``) that specialists, the digest and the observer
are built under. The in-process multi-agent teams that used to live beside this (local_graph, local_swarm, the Laya
team pipeline) were replaced by the orchestrator (``orchestrate_tool.py``, ``orchestrator/``).
"""

import logging

from ag_ui_strands.client_proxy_tool import registered_proxy_names
from strands import Agent
from strands.agent.conversation_manager import SummarizingConversationManager

import settings
import vault_proxy

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
DENIED_TOOLS = {"shell", "python_repl", "file_write", "editor", "apply_patch"}

# Tools reserved for the top-level orchestrator, excluded even from the
# "give every safe tool" default below (see resolve_tools) and even if a
# spec explicitly asks for one by name. orchestrate: a sub-agent starting a
# whole plan/execute/audit run is unbounded fan-out and quota spend with no
# gate. skill_forge/proposer: drafting new tools or harness changes is a
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
    "orchestrate",
    # Unreviewed egress to a third-party host (a2a_client_tool.py) — a
    # sub-agent has no gate, and no credentials it could be handed either.
    "a2a_client",
    "skill_forge",
    "proposer",
    "list_models",
    "set_active_model",
    "set_temperature",
    "set_memory_window",
    "context_usage",
    "set_sandbox_mode",
    # The Meta-Harness tools (Phase 6/7): spawning the Claude Code CLI is a gated action only the
    # orchestrator's approval prompt can authorise, and a sub-agent loading or stopping models
    # would be moving the machine's memory under the orchestrator's feet.
    "claude_code_engineer",
    # `web_fetch` runs a model call of its own over a whole page: a swarm node does not get to spend that on its own.
    "web_fetch",
    "omlx_resource_manager",
    "ollama_resource_manager",
    "context_digest",
}


# Model tiers: "fast" < "standard" < "strong". Each maps to a model id on the
# ACTIVE provider (settings.MODEL_TIER_*); an unset tier means "the provider's
# own model", so nothing changes until a tier is configured.
TIERS = ("fast", "standard", "strong")


def tier_model_id(tier: str | None) -> str | None:
    configured = {
        "fast": settings.MODEL_TIER_FAST,
        "standard": settings.MODEL_TIER_STANDARD,
        "strong": settings.MODEL_TIER_STRONG,
    }.get(tier or "")
    return configured or None


def build_sub_agent_model(tier: str | None = None, role: str | None = None):
    """A sub-agent's model: the SAME provider the orchestrator runs on (local
    oMLX, Anthropic, or an OpenAI-compatible cloud endpoint such as Ollama
    Cloud), at the requested tier.

    Used to be hardwired to local oMLX regardless of MODEL_PROVIDER, so with a
    cloud provider active the orchestrator ran on the cloud model while every
    sub-agent silently still needed the local server. Going through
    agent_common.build_model reuses the one place the provider's client — and
    its API key, via secret_guard — is built, so a sub-agent needs no key of
    its own and never receives one: the key never enters its tools, prompts or
    environment. Imported lazily: agent_common imports the tool modules that
    import this one.

    The local roles (``omlx_provider.ROLES``: ``architect`` planning, ``digest``) are
    the exception to "the orchestrator's provider": when ``omlx.prism`` is on and oMLX
    answers, they run on the Prism models on this machine, with the provider this
    function would otherwise have returned as the router's failover. The returned model
    needs ``omlx_provider.hooks_for(model)`` on its Agent (the load/unload lease).
    """
    base = _provider_model(tier, role)
    import omlx_provider

    if role in omlx_provider.ROLES and omlx_provider.enabled():
        return omlx_provider.build_models(role, fallback=base).router
    return base


def _provider_model(tier: str | None, role: str | None):
    import agent_common

    if agent_common.ACTIVE_PROVIDER == "ollama_cloud":
        # A tier the user configured names a registry alias and beats the role table
        # (explicit config over defaults). A *role* gets Strands' native ModelRouter
        # over the cloud candidates — its profile's model first, the others as
        # failover, reasoning budget by role; a roleless node (a team member) gets
        # one plain model.
        import ollama_cloud

        configured = tier_model_id(tier)
        if role:
            return ollama_cloud.build_models(role, model=configured or None).router
        return ollama_cloud.build_single(None, model=configured or None)
    return agent_common.build_model(model_id=tier_model_id(tier))


def build_conversation_manager() -> SummarizingConversationManager:
    """Same strategy the top-level agent gets (agent_common.py) — summarize
    older turns instead of silently dropping them. Strands' own default
    when a sub-agent gets none (SlidingWindowConversationManager) just
    truncates once ~40 messages pass, no compaction — fine for a one-shot
    reply, lossy for a sub-agent mid-way through several handoffs and tool
    calls in a long-running swarm/graph. No summarization_agent given, so
    Strands uses the sub-agent's own (local) model to write the summary."""
    return SummarizingConversationManager(preserve_recent_messages=10, proactive_compression=True)


def resolve_tools(tool_names: list[str] | None, caller: Agent, tool_label: str) -> list:
    import agent_common

    # current_generated_tool_names(), not the static GENERATED_TOOL_NAMES —
    # a sub-agent has no human-in-the-loop gate at all, so this denylist
    # must reflect a tool hot-loaded onto `caller` five seconds ago
    # (skill_validation_hooks.py's hot_reload_generated_skill) exactly as
    # strictly as one that's been there since process start, not just
    # whatever existed when this process launched.
    #
    # registered_proxy_names(caller.tool_registry) — every useFrontendTool
    # the browser has registered (toggle_theme today) becomes a real entry in
    # caller.tool_registry.registry via ag_ui_strands' own sync_proxy_tools
    # (confirmed reading ag_ui_strands/agent.py — it runs on the *live*
    # top-level agent, which is exactly `caller` here for the orchestrator's
    # own tool set). The registered proxy tool's body
    # (ag_ui_strands/client_proxy_tool.py's create_proxy_tool._proxy_func)
    # is a synchronous no-op that *unconditionally* returns
    # {"status": "success", "content": [{"text": "Forwarded to client"}]} —
    # the real browser round-trip only happens because ag_ui_strands' own
    # outer loop separately intercepts it for the top-level agent. A
    # sub-agent given one of these would get that same fake success back
    # with no effect at all — it would report having done something that
    # never happened, silently: worse than a missing tool, since nothing
    # signals the failure. Excluded by
    # the library's own proxy marker (registered_proxy_names), not a
    # per-name list, so this never needs updating when a new frontend tool
    # is added later.
    denied = (
        DENIED_TOOLS
        | agent_common.current_generated_tool_names()
        # An MCP server's tools reach out to whatever that server is; the orchestrator asks a person
        # before each one (acp_server.APPROVAL_REQUIRED_TOOLS) and a sub-agent has no one to ask.
        | set(agent_common.MCP_TOOL_NAMES)
        | ORCHESTRATOR_ONLY_TOOLS
        | registered_proxy_names(caller.tool_registry)
        # A sub-agent has no approval gate, so the vault's fail-closed refusal cannot reach its calls: while the vault
        # blocks egress (vault.egressPolicy: deny, vault down) its kit simply has no network tools.
        | (vault_proxy.SUB_AGENT_EGRESS_TOOLS if vault_proxy.egress_blocked() else frozenset())
    )
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

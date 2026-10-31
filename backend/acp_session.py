"""One session's shape and setup: the session record, the system prompts (project and chat), history replay for a
loaded session, the startup preferences, the router and self-heal configuration, the vision-model swap, and settling
a cancelled turn.

Split out of ``acp_server.py``, which re-exports every name here.
"""

import logging
import os
from dataclasses import dataclass, field
from pathlib import Path
import acp
import acp.stdio
import session_store
import ollama_resource_tool
import settings as app_settings
import settings_store
import os_context
from laya_router import Policy, RouterHooks
from plugin_host import PluginHost
from self_heal import HealConfig, SelfHealHooks
from acp_approval import _ApprovalHandler  # noqa: E402
from acp_protocol import _TEMPERATURE_PRESETS, _tool_kind  # noqa: E402
import session_history  # noqa: E402

def _replay_history(messages: list[dict]) -> list:
    """Turn an Agent's restored message history into ACP session/update notifications, for load_session — a client
    rebuilds its chat view entirely from these. Text, reasoning, and every tool call with its result
    (``session_history.replay``); it used to be the text alone."""
    return session_history.replay(messages, kind_of=_tool_kind)


def _project_system_prompt(cwd: str) -> str:
    """agent_common.SYSTEM_PROMPT hardcodes backend/workspace/ as the
    sandbox root — correct for the AG-UI/web front door, actively wrong
    here: an ACP client (Zed) is meant to work on whatever project it has
    open, told to us via new_session/load_session's own ``cwd`` — a totally
    different project every time, not one fixed scratch directory. Real bug
    this fixes: the agent used to answer "what project are we in?" by
    describing this repo's own backend, regardless of what Zed had open,
    because nothing here ever read the ``cwd`` the protocol was already
    handing over.
    """
    return (
        f"You are a coding agent working in the project at {cwd}. Your shell, file, and code tools "
        "operate relative to this directory — explore it first if you're not sure what's there. "
        "To find code, search — don't read: code_search (ripgrep) finds where a name or string is defined or used, "
        "lsp_definition / lsp_references / lsp_hover give the exact semantic answer once you have a position, and "
        "code_outline lists a file's classes and functions or returns one of them. Read a whole file only when it is "
        "short; a long one comes back as an outline. "
        "For multi-line or multi-file changes prefer apply_patch (a unified diff). After a command or "
        "test fails, read the [self-heal] notes appended to its result: they carry a root cause and, "
        "usually, a patch already verified in a sandbox copy. "
        "For a large refactor or implementation across many files, hand the whole task to claude_code_engineer "
        "(it runs the Claude Code CLI headless in this project and reports back); for multi-step work where a plan, "
        "an independent review and a check against the plan matter, use orchestrate. Judge either's result before "
        "you tell the user it is done. For planning, ask the architect; to condense bulk text, use context_digest. "
        "In plan mode nothing changes: investigate with the read-only tools, then call present_plan with the plan "
        "(goal, steps with the files each touches, how each is checked, risks) — the person approves it there and the "
        "session leaves plan mode; do not just describe the plan in chat. If the person's change of mind comes back "
        "as a revision request, revise and present again. When the work turns on a decision only the person can make, "
        "call ask_user with two to six short options instead of asking in prose; never ask_user for something you can read in the code. "
        + ollama_resource_tool.MEMORY_GUIDANCE
    )


def _is_chat_scratch(cwd: str) -> bool:
    """The folder a Discord chat with no project runs in (daemon/chat.py `workspace_dir`). Compared by path rather
    than imported: the daemon package is not this process's to load."""
    try:
        return os.path.realpath(cwd) == os.path.realpath(settings_store.user_dir() / "discord-workspace")
    except OSError:
        return False


def _chat_system_prompt(cwd: str) -> str:
    """For a chat with no project. The coding prompt above told the model its tools "operate relative to" an empty
    scratch folder, and it concluded — and told the operator, three times — that it could not see ~/Downloads, then
    guessed a filename instead of listing the folder. The tools were never confined; the prompt was."""
    home = str(Path.home())
    return (
        f"You are the operator's personal agent on their Mac, reached from a chat. {cwd} is an empty scratch folder, "
        f"not a project: their files are where they keep them — home is {home} (Downloads, Documents, Desktop, …). "
        "Your tools reach any path on this machine; use absolute paths. Never guess a filename: list first. "
        f"To find a file by name, call file_read with mode \"find\" and a glob of one keyword, e.g. path "
        f"\"{home}/Downloads/*figma*\" (case does not matter); then file_read the path it found (a PDF comes back as "
        "text). shell (ls, mdfind) works too but asks the "
        "operator first. Risky calls ask first; reading does not. "
        + ollama_resource_tool.MEMORY_GUIDANCE
    )


#: Ollama `:cloud` models that read images themselves: a picture the person attaches goes to them as it is, with no
#: detour through ``capture.visionModel``. Also the Vision Model choices in the app's Settings → Models.
CLOUD_VISION_MODELS = frozenset({"mistral-large-4:cloud", "gemma4:31b:cloud", "minimax-m3.1-flash:cloud"})


def _native_vision(agent) -> str | None:
    """The session's own model when it sees images itself — Claude (``AnthropicModel``), or an Ollama ``:cloud``
    vision model (`CLOUD_VISION_MODELS`) — so an image the person attached can go to it without a vision model being
    configured. ``None`` when it cannot see."""
    import ollama_cloud

    model = getattr(agent, "model", None)
    if model is None:
        return None
    model_id = str((model.get_config() or {}).get("model_id") or "")
    try:
        from strands.models.anthropic import AnthropicModel
    except ImportError:
        AnthropicModel = None
    if AnthropicModel is not None and isinstance(model, AnthropicModel):
        return model_id or "claude"
    if isinstance(model, ollama_cloud.OllamaCloudModel) and model_id in CLOUD_VISION_MODELS:
        return model_id
    return None


def _model_for_vision(agent, vision: dict) -> tuple[object | None, str | None]:
    """Point the session's model at the vision model for one screenshot turn. Returns a function that
    puts the session's own model back, or ``None`` and the reason the image cannot be sent.

    Not `_apply_model`: that is a switch the person makes, and it moves the defaults later sessions
    inherit. This is one turn's detour, and it checks what a model switch does not — where the pixels
    would go, and whether the turn's history can forget them afterwards:

    * **Ollama only.** Its local daemon serves local tags and `:cloud` tags behind one endpoint, so
      "is this on my machine" has an answer (the tag, and `/api/tags`). oMLX would load a vision model
      outside the memory budget (`PrismModel.require_loaded`), and any other provider is off-machine
      as far as this harness can vouch — both are refused rather than guessed at.
    * **Local unless the person said otherwise** (`screenshot_cloud_ok`, from `capture.screenshotCloud`).
    * **Never pulled.** A local tag that is not installed is refused, the registry's own rule.
    * **Forgettable.** The image is stripped from the history when the turn ends and the snapshot
      re-saved; a legacy per-message store cannot rewrite what it already wrote, so it is refused.
    """
    import ollama_cloud

    if vision.get("native"):
        # The session's own model sees the image (`_native_vision`): no detour, but it must still be forgettable.
        if not isinstance(getattr(agent, "_session_manager", None), session_store.SnapshotSessionManager | type(None)):
            return None, "the image was not sent: this session's store cannot remove an image after the turn"
        return (lambda: None), None
    model = agent.model
    target = str(vision.get("model") or "").strip()
    if not isinstance(model, ollama_cloud.OllamaCloudModel):
        return None, f"the screenshot was not sent: vision turns run on the Ollama provider, and this session uses {type(model).__name__}"
    if not isinstance(getattr(agent, "_session_manager", None), session_store.SnapshotSessionManager | type(None)):
        return None, "the screenshot was not sent: this session's store cannot remove an image after the turn"
    cloud = os_context.is_cloud_model(target)
    if cloud and not vision.get("cloud_ok"):
        return None, f"the screenshot was not sent: {target} is a cloud model and screenshots stay on this machine (capture.screenshotCloud)"
    if not cloud and target not in ollama_cloud.installed_local_tags(model.base_url):
        return None, f"the screenshot was not sent: {target} is not installed in the local Ollama daemon (this harness never pulls models)"
    before_id = model.get_config().get("model_id")
    before_spec = getattr(model, "spec", None)
    model.update_config(model_id=target)

    def restore() -> None:
        model.update_config(model_id=before_id)
        if before_spec is not None:
            model.spec = before_spec

    return restore, None


def _apply_temperature(agent, value) -> bool:
    preset = next((p for p in _TEMPERATURE_PRESETS if p[0] == value), None)
    if preset is None:
        return False
    current_params = dict(agent.model.get_config().get("params") or {})
    current_params["temperature"] = preset[1]
    agent.model.update_config(params=current_params)

    import agent_common

    agent_common.ACTIVE_TEMPERATURE = preset[1]
    return True


def generation_config(provider: str, preferences, params: dict) -> dict:
    """The model-config update for ``model.maxTokens`` and ``model.thinking``, or ``{}`` when neither applies.

    Anthropic (``AnthropicConfig``): ``max_tokens`` is top-level; thinking rides in ``params``. Current Claude models
    take only adaptive thinking (a fixed ``budget_tokens`` is rejected), so ``auto`` sends ``{"type": "adaptive"}`` and
    ``always`` adds ``output_config.effort = "high"`` for deeper thinking every turn; ``off`` sends nothing (some
    current models reject an explicit ``disabled``). Sampling parameters are not sent alongside thinking.
    OpenAI-compatible models take ``max_tokens`` as a sampling param and have no thinking switch here."""
    raw = preferences.get("model.maxTokens")
    max_tokens = int(raw) if isinstance(raw, (int, float)) and raw >= 1024 else None
    thinking = preferences.get("model.thinking")
    params = dict(params or {})
    if provider == "anthropic":
        update: dict = {}
        if max_tokens is not None:
            update["max_tokens"] = max_tokens
        if thinking in ("auto", "always"):
            params["thinking"] = {"type": "adaptive"}
            if thinking == "always":
                params["output_config"] = {**(params.get("output_config") or {}), "effort": "high"}
            params.pop("temperature", None)
            update["params"] = params
        elif thinking == "off" and ("thinking" in params or "output_config" in params):
            params.pop("thinking", None)
            params.pop("output_config", None)
            update["params"] = params
        return update
    if max_tokens is not None:
        params["max_tokens"] = max_tokens
        return {"params": params}
    return {}


def _apply_generation(agent, preferences) -> bool:
    """Apply ``model.maxTokens`` / ``model.thinking`` to the session's model. Never raises: a provider that refuses a
    key keeps its current config (logged)."""
    try:
        import agent_common

        update = generation_config(agent_common.ACTIVE_PROVIDER, preferences, agent.model.get_config().get("params") or {})
        if update:
            agent.model.update_config(**update)
        return bool(update)
    except Exception:
        logging.getLogger(__name__).warning("generation settings not applied", exc_info=True)
        return False


def _apply_startup_preferences(preferences) -> None:
    """Preferences that configure modules which read them when a session is
    built (Laya's mode and gate, the sub-agent model tiers). A value in a
    settings file beats the env/.env default; unset keys leave the env alone."""
    if preferences.get("laya.mode"):
        app_settings.LAYA_TRIAGE_MODE = preferences.get("laya.mode")
    if preferences.get("laya.threshold") is not None:
        app_settings.LAYA_TRIAGE_THRESHOLD = float(preferences.get("laya.threshold"))
    for tier in ("fast", "standard", "strong"):
        if preferences.get(f"tiers.{tier}"):
            setattr(app_settings, f"MODEL_TIER_{tier.upper()}", preferences.get(f"tiers.{tier}"))


def _router_policy(preferences) -> Policy:
    return Policy(
        gate=float(preferences.get("laya.threshold", app_settings.LAYA_TRIAGE_THRESHOLD)),
        min_confidence=float(preferences.get("laya.minConfidence", 0.5)),
        dispatch=preferences.get("laya.dispatch", "direct"),
        team_dispatch=preferences.get("laya.teamDispatch", "direct"),
        on_abstain=preferences.get("laya.onAbstain", "escalate"),
        frontier_model=preferences.get("tiers.strong") or app_settings.MODEL_TIER_STRONG or None,
    )


def _heal_config(preferences) -> HealConfig:
    return HealConfig(
        enabled=preferences.get("selfHeal.enabled", True) is not False,
        max_attempts=int(preferences.get("selfHeal.maxAttempts", 3)),
        verify=preferences.get("selfHeal.verify", "sandbox"),
        auto_apply=preferences.get("selfHeal.autoApply", False) is True,
        memory=preferences.get("selfHeal.memory", True) is not False,
    )
_LAYA_DECIDE_TIMEOUT = 2.0


@dataclass
class _Session:
    agent: object
    handler: _ApprovalHandler
    cwd: str
    router_hooks: RouterHooks
    heal: SelfHealHooks
    specialists: dict = field(default_factory=dict)
    plugins: PluginHost | None = None
    #: The settings resolved for this session's cwd. Held so a turn can tell
    #: whether the trace corpus is switched on (trace_store.enabled) without
    #: re-reading the files, which would pick up an edit made mid-session and
    #: disagree with every other decision this session already made from these.
    preferences: object = None
    #: Turns left in which adaptive mode stays out of the way because the person chose a mode by hand (mode_advisor.HOLD_TURNS).
    adaptive_hold: int = 0


def _settle_cancelled_turn(agent) -> None:
    """Leave a cancelled agent reusable.

    A turn cancelled right after an approval was answered (an E-Stop, or Esc a
    moment after "Allow") is abandoned while Strands still believes it is mid-
    resume, so the *next* prompt dies with "must resume from interrupt". It also
    leaves the model's last tool call without a result, which providers reject.
    Close both: drop the interrupt state, and answer any dangling tool call with
    an error result saying it did not run."""
    state = getattr(agent, "_interrupt_state", None)
    if state is not None and getattr(state, "activated", False):
        state.deactivate()
    messages = getattr(agent, "messages", None)
    if not messages or messages[-1].get("role") != "assistant":
        return
    uses = [block["toolUse"] for block in messages[-1].get("content") or [] if isinstance(block, dict) and "toolUse" in block]
    if not uses:
        return
    message = {
        "role": "user",
        "content": [
            {"toolResult": {"toolUseId": use["toolUseId"], "status": "error", "content": [{"text": "cancelled: this call did not run"}]}}
            for use in uses
        ],
    }
    messages.append(message)
    manager = getattr(agent, "_session_manager", None)
    if manager is not None:
        try:
            session_store.record(manager, message, agent)
        except Exception:
            logging.getLogger(__name__).debug("could not persist the cancelled tool results", exc_info=True)

"""Read/write the active model provider config (local oMLX / Anthropic /
an OpenAI-compatible cloud endpoint) — the settings-UI counterpart to
model_control_tool.py's chat-tool-driven model switch.

Unlike model_control_tool.py's tools, this is called from a plain REST route
(main.py's /provider-config), not a chat turn — there's no ToolContext or
live per-thread Agent to reach through, so this writes agent_common.ACTIVE_*
directly instead of tool_context.agent.model.update_config(). The next
build_model() call (a new thread, or any thread reconstructed after this)
picks the change up immediately; an *already-running* thread's Agent keeps
whatever Model instance it was built with, same as every other ACTIVE_*
global in this codebase.

agent_common is imported lazily, inside each function, not at module level —
same reason model_control_tool.py does it: agent_common's own first import
triggers its os.chdir(WORKSPACE_DIR) side effect, and this module has to
stay importable before that happens (main.py imports it in the same early,
pre-chdir block as model_control_tool/sandbox_functions_api
— see that file's own comment on why import order here is load-bearing).
"""

import os
from pathlib import Path

from secret_guard import Secret

_ENV_PATH = Path(__file__).resolve().parent / ".env"

_VALID_PROVIDERS = {"local", "anthropic", "openai_compatible", "ollama_cloud"}


def _update_env_file(updates: dict[str, str]) -> None:
    """Rewrite matching `KEY=value` lines in backend/.env, appending any key
    not already present. Comments, blank lines, and every other key are left
    untouched. Atomic write (tmp file + os.replace) so a crash mid-write
    can't leave .env half-written.
    """
    if not updates:
        return

    lines = _ENV_PATH.read_text().splitlines() if _ENV_PATH.exists() else []
    remaining = dict(updates)
    for i, line in enumerate(lines):
        stripped = line.strip()
        if not stripped or stripped.startswith("#") or "=" not in stripped:
            continue
        key = stripped.split("=", 1)[0].strip()
        if key in remaining:
            lines[i] = f"{key}={remaining.pop(key)}"
    for key, value in remaining.items():
        lines.append(f"{key}={value}")

    tmp_path = _ENV_PATH.with_name(_ENV_PATH.name + ".tmp")
    tmp_path.write_text("\n".join(lines) + "\n")
    os.replace(tmp_path, _ENV_PATH)


def get_provider_state() -> dict:
    """Redacted snapshot of the current provider config — never a raw API
    key, only whether one is set. Safe to return straight from a REST route
    with no further filtering.
    """
    import agent_common
    import ollama_cloud
    from settings import OMLX_BASE_URL

    return {
        "provider": agent_common.ACTIVE_PROVIDER,
        "local": {
            "base_url": OMLX_BASE_URL,
            "model_id": agent_common.ACTIVE_MODEL_ID,
        },
        "anthropic": {
            "model_id": agent_common.ACTIVE_ANTHROPIC_MODEL_ID,
            "api_key_set": bool(agent_common.ACTIVE_ANTHROPIC_API_KEY),
        },
        "ollama_cloud": {
            "base_url": ollama_cloud.default_base_url(),
            "models": list(ollama_cloud.REGISTRY),
            "roles": {role: {"model": p.model, "effort": p.effort} for role, p in ollama_cloud.ROLES.snapshot().items()},
        },
        "openai_compatible": {
            "base_url": agent_common.ACTIVE_CLOUD_BASE_URL,
            "model_id": agent_common.ACTIVE_CLOUD_MODEL_ID,
            "api_key_set": bool(agent_common.ACTIVE_CLOUD_API_KEY),
        },
    }


def set_provider_config(
    provider: str | None = None,
    anthropic_api_key: str | None = None,
    anthropic_model_id: str | None = None,
    openai_base_url: str | None = None,
    openai_api_key: str | None = None,
    openai_model_id: str | None = None,
) -> dict:
    """Switch the active provider and/or update its config. Only the fields
    actually passed (non-None) are touched — flipping `provider` alone
    doesn't blank out a key already saved for a provider you're not
    switching to, and switching *to* Anthropic without a key just leaves
    ACTIVE_PROVIDER pointed at a provider build_model() will fail to
    construct until a key is also saved (same "you broke it, you saw the
    error" tradeoff set_active_model already accepts for an unreachable
    oMLX server).

    Persists every touched field *except API keys* to backend/.env (survives
    a restart) in addition to the immediate in-memory agent_common.ACTIVE_*
    mutation (takes effect with no restart). An API key is held in memory
    only: backend/.env is on disk where the agent's file tools can read it, so
    a key set here lasts until the process restarts — for a durable key,
    export CLOUD_OPENAI_API_KEY / ANTHROPIC_API_KEY (or OLLAMA_API_KEY) in
    the shell that launches the backend. See secret_guard.py.

    Raises ValueError for an unrecognized provider string; main.py's route
    turns that into a 400.
    """
    if provider is not None and provider not in _VALID_PROVIDERS:
        raise ValueError(f"provider must be one of {sorted(_VALID_PROVIDERS)}, got {provider!r}")

    import agent_common

    env_updates: dict[str, str] = {}

    if provider is not None:
        agent_common.ACTIVE_PROVIDER = provider
        env_updates["MODEL_PROVIDER"] = provider
    if anthropic_api_key is not None:
        agent_common.ACTIVE_ANTHROPIC_API_KEY = Secret(anthropic_api_key)
    if anthropic_model_id is not None:
        agent_common.ACTIVE_ANTHROPIC_MODEL_ID = anthropic_model_id
        env_updates["ANTHROPIC_MODEL_ID"] = anthropic_model_id
    if openai_base_url is not None:
        agent_common.ACTIVE_CLOUD_BASE_URL = openai_base_url
        env_updates["CLOUD_OPENAI_BASE_URL"] = openai_base_url
    if openai_api_key is not None:
        agent_common.ACTIVE_CLOUD_API_KEY = Secret(openai_api_key)
    if openai_model_id is not None:
        agent_common.ACTIVE_CLOUD_MODEL_ID = openai_model_id
        env_updates["CLOUD_OPENAI_MODEL_ID"] = openai_model_id

    _update_env_file(env_updates)
    return get_provider_state()

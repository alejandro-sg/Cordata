"""Regression check: sub-agents (the specialists, the digest) run on the
ACTIVE provider — so a cloud provider's key is used by sub-agents too, with no
key of their own — at their tier, and the key never reaches anything a
sub-agent can see.

No LLM, no network: builds the models/agents and inspects them.

    backend/.venv/bin/python _sub_agent_model_smoke_test.py
"""

import provider_settings  # noqa: F401  (imported before agent_common, which chdirs — see main.py)

import agent_common
import secret_guard as sg
import settings
import sub_agent_common as sac
from strands import tool
from strands.models.model import Model

KEY = "subagent-key-0123456789-abcdef-FAKE"


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


class NoModel(Model):
    def update_config(self, **kw):
        pass

    def get_config(self):
        return {}

    async def structured_output(self, *a, **k):
        raise NotImplementedError
        yield  # pragma: no cover

    async def stream(self, *a, **k):
        raise NotImplementedError
        yield  # pragma: no cover


@tool
def calculator() -> str:
    """calc"""
    return "4"


def model_id(model) -> str:
    return model.get_config()["model_id"]


def main() -> None:
    # --- cloud provider, as configured for Ollama Cloud
    agent_common.ACTIVE_PROVIDER = "openai_compatible"
    agent_common.ACTIVE_CLOUD_BASE_URL = "https://ollama.com/v1"
    agent_common.ACTIVE_CLOUD_API_KEY = sg.Secret(KEY)
    agent_common.ACTIVE_CLOUD_MODEL_ID = "m-active"
    settings.MODEL_TIER_FAST, settings.MODEL_TIER_STANDARD, settings.MODEL_TIER_STRONG = "m-fast", "", "m-strong"

    default = sac.build_sub_agent_model(None)
    check(type(default).__name__ == "OpenAIModel" and model_id(default) == "m-active", "no tier: the sub-agent uses the active provider's own model")
    check(default.client_args["base_url"] == "https://ollama.com/v1", "…on the same endpoint as the orchestrator")
    check(default.client_args["api_key"] == KEY, "…with the same API key — a sub-agent needs no key of its own")
    check(model_id(sac.build_sub_agent_model("fast")) == "m-fast", "tier 'fast' maps to MODEL_TIER_FAST")
    check(model_id(sac.build_sub_agent_model("standard")) == "m-active", "an unset tier ('standard') falls back to the provider's model")
    check(model_id(sac.build_sub_agent_model("strong")) == "m-strong", "tier 'strong' maps to MODEL_TIER_STRONG")
    check(model_id(sac.build_sub_agent_model("bogus")) == "m-active", "an unknown tier name is ignored")
    check(model_id(agent_common.build_model()) == "m-active", "the orchestrator's build_model() is unchanged without an override")

    # --- anthropic and local providers honour the same override
    agent_common.ACTIVE_PROVIDER = "anthropic"
    agent_common.ACTIVE_ANTHROPIC_API_KEY = sg.Secret(KEY)
    agent_common.ACTIVE_ANTHROPIC_MODEL_ID = "claude-active"
    check(type(sac.build_sub_agent_model("fast")).__name__ == "AnthropicModel" and model_id(sac.build_sub_agent_model("fast")) == "m-fast", "the anthropic provider takes tiers too")
    agent_common.ACTIVE_PROVIDER = "local"
    agent_common.ACTIVE_MODEL_ID = "local-active"
    local = sac.build_sub_agent_model(None)
    check(model_id(local) == "local-active" and local.client_args["api_key"] == "not-needed", "the local provider still works (and uses the live ACTIVE_MODEL_ID)")
    agent_common.ACTIVE_PROVIDER = "openai_compatible"

    # --- a real sub-agent: the architect specialist, on the active provider at its tier
    import specialists
    from prompt_security import SecretRedactionHooks

    node = specialists.build_specialist(specialists.ARCHITECT, hooks=[SecretRedactionHooks()])
    check(model_id(node.model) in ("m-active", "m-fast", "m-standard", "m-strong"), f"a specialist runs on the active provider's models ({model_id(node.model)})")

    # --- the key never reaches anything a sub-agent can see
    surfaces = {
        "system prompt": node.system_prompt or "",
        "tool names": ",".join(node.tool_registry.registry),
        "str(model config)": str(node.model.get_config()),
        "repr of the settings secret": repr(settings.CLOUD_OPENAI_API_KEY) + repr(agent_common.ACTIVE_CLOUD_API_KEY),
    }
    for name, text in surfaces.items():
        check(KEY not in text, f"the API key is not in a sub-agent's {name}")
    # Behavioural: fire a real AfterToolCallEvent through the node's own hook registry.
    from strands.hooks import AfterToolCallEvent

    result = {"status": "success", "content": [{"text": f"cat .env -> KEY={KEY}"}]}
    event = AfterToolCallEvent(agent=node, selected_tool=None, tool_use={"name": "file_read", "toolUseId": "x", "input": {}}, invocation_state={}, result=result)
    node.hooks.invoke_callbacks(event)
    check(KEY not in str(result) and sg.REDACTED in str(result), "a tool result inside a sub-agent is masked by its own hooks")
    print("\nPASS: sub-agents follow the active provider, tiered, with no key of their own")


if __name__ == "__main__":
    main()

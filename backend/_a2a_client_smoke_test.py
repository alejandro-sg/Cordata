"""a2a_client tool: consuming remote A2A-protocol agents as an orchestrator
tool (a2a_client_tool.py, on top of strands.agent.a2a_agent.A2AAgent).

* the tool is in the static orchestrator toolset and typed as a fetch in the
  ACP server's tool-kind map;
* a non-http(s) endpoint is refused without any network activity;
* an unreachable endpoint comes back as a tool error result, not a raise;
* sub-agents can never get it (unreviewed third-party egress needs a gate),
  even when a sub-agent spec explicitly asks for it by name.
No live remote agent is needed: refused/failed calls prove the failure paths,
and everything else is construction-time work.

    backend/.venv/bin/python _a2a_client_smoke_test.py
"""

import os
import tempfile

_TMP = tempfile.TemporaryDirectory()
os.environ["SIERRALAB_HOME"] = _TMP.name

import acp_server  # noqa: E402
import agent_common  # noqa: E402
import sub_agent_common as sac  # noqa: E402
from a2a_client_tool import a2a_client  # noqa: E402
from harness_testing import check  # noqa: E402

TOOL_NAME = getattr(a2a_client, "tool_name", "a2a_client")
check(TOOL_NAME == "a2a_client", f"tool registered as a2a_client ({TOOL_NAME})")
check(any(getattr(t, "tool_name", None) == "a2a_client" for t in agent_common.TOOLS), "a2a_client in the static orchestrator toolset")
check(acp_server._tool_kind("a2a_client") == "fetch", "ACP clients see it as a fetch tool (like http_request)")


def tool_text(result: dict) -> str:
    content = result.get("content") or []
    return "\n".join(block.get("text", "") for block in content if isinstance(block, dict))


async def main() -> None:
    result = await a2a_client(endpoint="ftp://nope", prompt="hi")
    check(result["status"] == "error" and "http(s) URL" in tool_text(result), "non-http(s) endpoint refused with no network call")
    result = await a2a_client(endpoint="not a url", prompt="hi")
    check(result["status"] == "error", "garbage endpoint refused")
    result = await a2a_client(endpoint="http://127.0.0.1:9", prompt="hi", timeout_seconds=1)
    check(result["status"] == "error" and "a2a_client http://127.0.0.1:9" in tool_text(result), "unreachable remote agent is a tool error, not a raise")

    caller = agent_common.build_template_agent()
    resolved = sac.resolve_tools(["a2a_client"], caller, "smoke-test")
    check(not resolved, "a sub-agent explicit request for a2a_client is denied")
    default = [getattr(t, "tool_name", None) for t in sac.resolve_tools(None, caller, "smoke-test")]
    check("a2a_client" not in default, "a2a_client not in the default sub-agent tool set")
    print(f"a2a client smoke test: all checks passed ({os.environ['SIERRALAB_HOME']})")


import asyncio  # noqa: E402

if __name__ == "__main__":
    asyncio.run(main())
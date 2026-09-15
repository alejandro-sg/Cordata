"""Builds and runs a Strands Swarm directly, not through strands_tools.swarm.

That tool's own agent-spec model wiring is broken against the installed
strands-agents-tools version: `_create_custom_agents` builds each sub-agent
via `Agent(name=..., system_prompt=..., tools=..., ...)` with no `model=`
kwarg — and `Agent.__init__` defaults an unset model straight to
`BedrockModel()` *at construction time*, before `_create_custom_agents` ever
gets to its "configure model provider" step below that. That step only sets
`swarm_agent.model_provider` / `swarm_agent.model_settings` as plain
post-hoc attributes; nothing reads them back into `.model`, so the
already-constructed BedrockModel is what actually runs. In this environment
that crashes immediately on AWS credential discovery ("Missing Dependency:
... botocore[crt]") before ever reaching oMLX — installing that dependency
would not fix anything, it would just let a real cloud model provider get
further, which this deployment explicitly never wants (100% local, no cloud
calls). A version-skew bug between strands-agents and strands-agents-tools,
the same class of issue that made agent_memory_tool.py replace the broken
mem0_memory instead of patching it (see README) — not something to
hand-patch inside the installed package.

`strands_tools.swarm` itself just hands a plain list of already-constructed
`Agent` objects to `strands.multiagent.Swarm(nodes=...)` — that SDK class
injects its own coordination tools (`handoff_to_agent`,
`complete_swarm_task`) onto whatever nodes it's given, regardless of how
they were built. So this builds that list directly instead, with a real
`model=` on every agent from the start. Shared sub-agent plumbing (local
model construction, the tool-safety denylist, progress-event shaping) lives
in sub_agent_common.py — local_graph_tool.py (the parallel/DAG counterpart,
for genuinely independent sub-tasks) reuses the same pieces.
"""

from typing import Any

from strands import tool
from strands.multiagent import Swarm
from strands.types.tools import ToolContext

import sub_agent_common as sac

_TOOL_LABEL = "local_swarm"


@tool(context=True)
async def local_swarm(task: str, agents: list[dict[str, Any]], tool_context: ToolContext) -> dict[str, Any]:
    """Spin up a team of specialized Strands sub-agents to collaborate on one task.

    Every sub-agent automatically runs on this deployment's local model — you
    never specify model_provider or model_settings, this tool fills them in.
    Just describe each specialist by name, role, and (optionally) which of
    your own tools they should have.

    Sub-agents can never be given shell, python_repl, file_write, or any
    generated skill tool — those need human approval, which only you (the
    top-level agent) can trigger. If the task genuinely needs one of those
    (e.g. writing real files to disk), do that part yourself instead of
    delegating it to a sub-agent; use the team only for the parts that don't
    need those tools (research, drafting, review).

    Use local_graph instead when the sub-tasks are genuinely independent
    (no one needs another's output) — local_swarm hands off one agent at a
    time in sequence, even when nothing requires that ordering.

    Args:
        task: The overall task for the team to accomplish together.
        agents: One entry per specialist: {"name": str, "system_prompt": str,
            "tools": list[str] (optional, tool names from your own toolset)}.
    """
    if not agents:
        raise ValueError("At least one agent specification is required")

    nodes = list(sac.build_agent_nodes(agents, tool_context.agent, _TOOL_LABEL).values())
    sdk_swarm = Swarm(nodes=nodes, node_timeout=sac.DEFAULT_NODE_TIMEOUT, execution_timeout=sac.DEFAULT_EXECUTION_TIMEOUT)

    # An async-generator @tool: every value yielded except the last becomes
    # a streamed tool event; the last becomes the actual tool result (see
    # strands/tools/decorator.py's stream()). ag_ui_strands auto-emits a
    # StateSnapshotEvent for any {"state": ...}-shaped streamed value with
    # no extra backend wiring needed (see ToolStreamEventHandler's
    # docstring) — merged onto whatever's already in agent state (model,
    # temperature, tone, ...) each time, so a swarm run's progress log
    # doesn't clobber unrelated settings live on the frontend.
    base_state = dict(tool_context.agent.state.get() or {})
    log: list[dict[str, Any]] = []
    reasoning: dict[str, str] = {}
    reasoning_pending = 0
    final_result = None

    def _state() -> dict[str, Any]:
        # "kind" mirrors local_graph_tool.py's discriminator, so the frontend
        # never has to infer which tool produced a given progress log from
        # field absence (e.g. a missing graph_structure). "reasoning" feeds
        # SwarmProgress.tsx's per-node thinking tab — see sub_agent_common.
        return {**base_state, "kind": "swarm", "swarm_progress": list(log), "reasoning": dict(reasoning)}

    async for event in sdk_swarm.stream_async(task):
        if event.get("type") == "multiagent_result":
            final_result = event["result"]
            continue

        delta_hit = sac.reasoning_delta(event)
        if delta_hit is not None:
            node_id, delta = delta_hit
            reasoning[node_id] = reasoning.get(node_id, "") + delta
            reasoning_pending += len(delta)
            if reasoning_pending < sac.REASONING_YIELD_CHARS:
                continue
            reasoning_pending = 0
            yield {"state": _state()}
            continue

        entry = sac.progress_entry(event)
        if entry is None:
            continue
        log.append(entry)
        yield {"state": _state()}

    yield {
        "status": str(final_result.status),
        "execution_time_ms": final_result.execution_time,
        "iterations": final_result.execution_count,
        "collaboration_chain": [node.node_id for node in (final_result.node_history or [])],
        "results": {name: sac.summarize_node_result(node_result) for name, node_result in (final_result.results or {}).items()},
    }

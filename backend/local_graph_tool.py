"""Builds and runs a Strands Graph directly — the parallel/DAG counterpart to
local_swarm_tool.py's sequential hand-off pattern. Use this when sub-tasks
are genuinely independent (nothing needs another's output);
strands.multiagent.Graph runs every node with no unmet dependency
concurrently (via asyncio.gather internally, confirmed by reading
graph.py's own _execute_nodes_parallel), instead of Swarm's one-agent-at-a-
time collaboration.

Shares its local-model construction, tool-safety denylist, and progress-
event shaping with local_swarm_tool.py — see sub_agent_common.py; both
strands.multiagent.Swarm and strands.multiagent.Graph emit the same
multiagent_node_start/multiagent_node_stop/multiagent_result event shapes
from stream_async(), confirmed empirically against this deployment's local
model.
"""

from typing import Any

from strands import tool
from strands.multiagent import GraphBuilder
from strands.types.tools import ToolContext

import sub_agent_common as sac

_TOOL_LABEL = "local_graph"


@tool(context=True)
async def local_graph(task: str, agents: list[dict[str, Any]], tool_context: ToolContext) -> dict[str, Any]:
    """Spin up a team of specialized Strands sub-agents that run in parallel.

    Every sub-agent automatically runs on this deployment's local model,
    same as local_swarm. Unlike local_swarm's sequential hand-off, nodes
    with no unmet dependency run concurrently — use this when sub-tasks are
    genuinely independent (e.g. researching three unrelated things at
    once); use local_swarm instead when each step needs the previous one's
    output, or the team needs to hand off and coordinate autonomously.

    Sub-agents can never be given shell, python_repl, file_write, or any
    generated skill tool — those need human approval, which only you (the
    top-level agent) can trigger. If the task genuinely needs one of those,
    do that part yourself instead of delegating it to a sub-agent.

    Args:
        task: The overall task for the team to accomplish together.
        agents: One entry per specialist: {"name": str, "system_prompt": str,
            "tools": list[str] (optional, tool names from your own toolset),
            "depends_on": list[str] (optional, names of other agents in this
            same list whose result this one needs first — omit for an agent
            that can start immediately, in parallel with any other agent
            that also has no dependencies)}.
    """
    if not agents:
        raise ValueError("At least one agent specification is required")

    nodes_by_name = sac.build_agent_nodes(agents, tool_context.agent, _TOOL_LABEL)

    builder = GraphBuilder()
    for name, node_agent in nodes_by_name.items():
        builder.add_node(node_agent, name)
    for spec, name in zip(agents, nodes_by_name):
        for dependency in spec.get("depends_on") or []:
            if dependency in nodes_by_name:
                builder.add_edge(dependency, name)
    builder.set_node_timeout(sac.DEFAULT_NODE_TIMEOUT)
    builder.set_execution_timeout(sac.DEFAULT_EXECUTION_TIMEOUT)
    graph = builder.build()

    # The dependency structure the frontend needs to draw real parallel
    # lanes instead of one flat rail (see SwarmProgress.tsx) — graph.edges
    # is a set[GraphEdge], each with .from_node/.to_node (GraphNode, same
    # .node_id used below for execution_order), populated by builder.build().
    structure = {
        "nodes": list(nodes_by_name.keys()),
        "edges": [[edge.from_node.node_id, edge.to_node.node_id] for edge in graph.edges],
    }

    # Same async-generator streaming pattern as local_swarm — see its
    # comments for why this shape (yield {"state": ...} mid-run, the real
    # result last) needs no extra backend wiring to reach the frontend live.
    # "kind" lets the frontend tell a graph run's progress log apart from a
    # swarm run's without guessing from field presence. "reasoning" feeds
    # SwarmProgress.tsx's per-node thinking tab — see sub_agent_common.
    base_state = dict(tool_context.agent.state.get() or {})
    log: list[dict[str, Any]] = []
    reasoning: dict[str, str] = {}
    reasoning_pending = 0
    final_result = None

    def _state() -> dict[str, Any]:
        return {
            **base_state,
            "kind": "graph",
            "graph_structure": structure,
            "swarm_progress": list(log),
            "reasoning": dict(reasoning),
        }

    async for event in graph.stream_async(task):
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
        "execution_order": [node.node_id for node in (final_result.execution_order or [])],
        "graph_structure": structure,
        "results": {name: sac.summarize_node_result(node_result) for name, node_result in (final_result.results or {}).items()},
    }

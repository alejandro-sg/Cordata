"""Regression check for local_graph_tool.py's graph_structure extraction —
the {"nodes": [...], "edges": [[from, to], ...]} block SwarmProgress.tsx
needs to render real dependency waves (see that file's computeWaves()).

Builds a real strands.multiagent.GraphBuilder with real (but never-run)
Agent nodes, the same way local_graph_tool.py itself does, and extracts
graph.edges the same way — but never calls stream_async()/execute(), so no
oMLX call happens and no LLM output is needed. Pure and fast on purpose,
same spirit as _subagent_denylist_smoke_test.py. Run directly:

    backend/.venv/bin/python _local_graph_structure_smoke_test.py
"""

from strands.multiagent import GraphBuilder

import agent_common
import sub_agent_common as sac


def _extract_structure(nodes_by_name: dict, graph) -> dict:
    # Mirrors local_graph_tool.py's own structure-building line exactly —
    # if that code's assumptions about the Strands API (graph.edges as a
    # set of GraphEdge, each with .from_node/.to_node.node_id) ever break
    # against a future strands-agents upgrade, this fails here first.
    return {
        "nodes": list(nodes_by_name.keys()),
        "edges": [[edge.from_node.node_id, edge.to_node.node_id] for edge in graph.edges],
    }


def main() -> None:
    caller = agent_common.build_template_agent()

    # Same topology as the sub-agent-team SKILL.md example: three
    # independent researchers, one summarizer depending on all three.
    agents = [
        {"name": "bitcoin_researcher", "system_prompt": "research bitcoin"},
        {"name": "ethereum_researcher", "system_prompt": "research ethereum"},
        {"name": "solana_researcher", "system_prompt": "research solana"},
        {
            "name": "summarizer",
            "system_prompt": "summarize",
            "depends_on": ["bitcoin_researcher", "ethereum_researcher", "solana_researcher"],
        },
    ]

    nodes_by_name = sac.build_agent_nodes(agents, caller, "smoke-test")

    builder = GraphBuilder()
    for name, node_agent in nodes_by_name.items():
        builder.add_node(node_agent, name)
    for spec, name in zip(agents, nodes_by_name):
        for dependency in spec.get("depends_on") or []:
            if dependency in nodes_by_name:
                builder.add_edge(dependency, name)
    graph = builder.build()

    structure = _extract_structure(nodes_by_name, graph)

    assert structure["nodes"] == [
        "bitcoin_researcher",
        "ethereum_researcher",
        "solana_researcher",
        "summarizer",
    ], f"unexpected node order: {structure['nodes']}"

    expected_edges = {
        ("bitcoin_researcher", "summarizer"),
        ("ethereum_researcher", "summarizer"),
        ("solana_researcher", "summarizer"),
    }
    actual_edges = {tuple(edge) for edge in structure["edges"]}
    assert actual_edges == expected_edges, f"unexpected edges: {actual_edges}"

    # Every edge endpoint must be a real node name — this is exactly what
    # SwarmProgress.tsx's computeWaves() assumes when it does inDegree.has(x).
    node_set = set(structure["nodes"])
    for from_id, to_id in structure["edges"]:
        assert from_id in node_set and to_id in node_set, f"edge references unknown node: {from_id} -> {to_id}"

    print(f"PASS: local_graph's graph_structure extraction matches the real Strands Graph API ({len(structure['nodes'])} nodes, {len(structure['edges'])} edges)")


if __name__ == "__main__":
    main()

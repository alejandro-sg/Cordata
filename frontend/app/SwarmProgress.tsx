"use client";

import { useEffect, useRef, useState } from "react";
import { useAgent } from "@copilotkit/react-core/v2";

type SwarmEntry =
  | { type: "node_start"; node: string }
  | { type: "node_stop"; node: string; summary: string }
  | { type: "handoff"; from: string[]; to: string[]; message: string };

// Streamed only by local_graph_tool.py (local_swarm has no dependency
// structure to report — its nodes genuinely run one at a time). node ids
// match SwarmEntry.node exactly, both sourced from the same
// sub_agent_common.build_agent_nodes() names on the backend.
type GraphStructure = { nodes: string[]; edges: [string, string][] };

type NodeState = {
  key: string;
  name: string;
  status: "pending" | "running" | "done";
  summary?: string;
};

// Kahn's algorithm: groups graph_structure's nodes into dependency "waves" —
// wave 0 has no unmet dependency, wave 1 depends only on wave 0, etc. This
// is exactly the batching strands.multiagent.Graph itself uses internally
// to decide what runs concurrently (asyncio.gather per wave), so a wave
// here is a faithful picture of what actually ran at the same time, not a
// frontend guess. Any node left over after the frontier empties (a cycle,
// or an edge naming a node outside `nodes`) is dumped into one trailing
// wave rather than silently dropped.
function computeWaves(structure: GraphStructure): string[][] {
  const inDegree = new Map<string, number>();
  const dependents = new Map<string, string[]>();
  for (const name of structure.nodes) {
    inDegree.set(name, 0);
    dependents.set(name, []);
  }
  for (const [from, to] of structure.edges) {
    if (!inDegree.has(from) || !inDegree.has(to)) continue;
    dependents.get(from)!.push(to);
    inDegree.set(to, (inDegree.get(to) ?? 0) + 1);
  }

  const waves: string[][] = [];
  const seen = new Set<string>();
  let frontier = structure.nodes.filter((name) => inDegree.get(name) === 0);

  while (frontier.length > 0) {
    waves.push(frontier);
    const next: string[] = [];
    for (const name of frontier) {
      seen.add(name);
      for (const dependent of dependents.get(name) ?? []) {
        const remaining = (inDegree.get(dependent) ?? 0) - 1;
        inDegree.set(dependent, remaining);
        if (remaining === 0) next.push(dependent);
      }
    }
    frontier = next;
  }

  const leftover = structure.nodes.filter((name) => !seen.has(name));
  if (leftover.length > 0) waves.push(leftover);
  return waves;
}

// Sub-agents often answer with a fenced block (```...```), sometimes as the
// whole reply, sometimes with prose before/after it — strip every fence
// wherever it appears rather than only recognizing a whole-string fence,
// which missed the (more common, verified against a real run) case of a
// fence embedded in surrounding text. Otherwise the literal backticks show
// up as what reads like broken formatting instead of intentional output.
function cleanSummary(text: string): string {
  return text
    .trim()
    .replace(/```[a-zA-Z]*\n?([\s\S]*?)```/g, (_, inner: string) => inner.trim())
    .trim();
}

/**
 * Live rail of local_swarm/local_graph's sub-agents as they work —
 * CopilotKit's documented "delegation log" pattern
 * (docs.copilotkit.ai/multi-agent/subagents): a supervisor's tool mutates a
 * shared-state slot as it runs, the frontend renders every new entry live,
 * turning what would otherwise be a long opaque spinner into visible
 * progress. Backend side: local_swarm_tool.py/local_graph_tool.py are
 * async-generator @tools that yield {"state": {..., "swarm_progress": [...]}}
 * after every node-start/node-stop/handoff event from
 * strands.multiagent.{Swarm,Graph}'s own stream_async() — ag_ui_strands
 * auto-converts that into a live StateSnapshotEvent, no custom backend
 * event handler needed.
 *
 * agent.state resets to {} once the tool call itself completes (nothing
 * persists swarm_progress into durable agent state — it's a stream of
 * events, not a state field, so an old thread you resume never has it) — so
 * this keeps the last non-empty snapshot in local state instead of
 * re-reading agent.state directly, otherwise the whole panel would vanish
 * the instant the swarm finished. Tracked per agent.threadId so switching
 * threads (or starting a new chat) clears it instead of leaking a *previous*
 * thread's sub-agent activity into the one you're now looking at.
 *
 * Interactive: click a finished node to expand its full reply.
 *
 * local_graph runs also stream graph_structure (see local_graph_tool.py) —
 * the dependency edges between this run's nodes. When present, nodes render
 * as dependency "waves" (columns of concurrently-runnable nodes, computed
 * via computeWaves()) instead of one flat rail, so a graph run with real
 * parallel branches actually looks parallel. local_swarm runs never stream
 * this field (nothing to report — swarm nodes always run one at a time), so
 * they keep rendering as the original flat rail below, unchanged.
 *
 * Always rendered once its own toggle (ThreadSidebar's "Sub-agents panel"
 * checkbox, persisted like dark mode) is on — an idle placeholder shows
 * before any sub-agent has run this thread, so the panel is a stable,
 * predictable part of the layout instead of something that only appears
 * (and shifts the chat pane's width) the first time it has something to say.
 */
export default function SwarmProgress() {
  const { agent } = useAgent({ agentId: "sierralab_agent" });
  const [entries, setEntries] = useState<SwarmEntry[]>([]);
  const [graphStructure, setGraphStructure] = useState<GraphStructure | undefined>(undefined);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const seenRunRef = useRef(false);
  const threadIdRef = useRef<string | undefined>(undefined);

  useEffect(() => {
    if (threadIdRef.current !== agent.threadId) {
      threadIdRef.current = agent.threadId;
      seenRunRef.current = false;
      setEntries([]);
      setGraphStructure(undefined);
      setExpanded(new Set());
    }

    const state = agent.state as { swarm_progress?: SwarmEntry[]; graph_structure?: GraphStructure } | undefined;
    const live = state?.swarm_progress;
    if (live && live.length > 0) {
      seenRunRef.current = true;
      setEntries(live);
    }
    if (state?.graph_structure) {
      setGraphStructure(state.graph_structure);
    }
  }, [agent.state, agent.threadId]);

  const isIdle = !seenRunRef.current || entries.length === 0;

  const nodes: NodeState[] = [];
  const nodeIndex = new Map<string, number>();
  for (const entry of entries) {
    if (entry.type === "node_start") {
      const key = `${entry.node}-${nodes.length}`;
      nodeIndex.set(entry.node, nodes.length);
      nodes.push({ key, name: entry.node, status: "running" });
    } else if (entry.type === "node_stop") {
      const idx = nodeIndex.get(entry.node);
      if (idx !== undefined) nodes[idx] = { ...nodes[idx], status: "done", summary: entry.summary };
    }
  }
  const handoffs = entries.filter((e): e is Extract<SwarmEntry, { type: "handoff" }> => e.type === "handoff");
  const nodeByName = new Map(nodes.map((n) => [n.name, n]));
  const waves = graphStructure ? computeWaves(graphStructure) : null;

  const toggle = (key: string) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  };

  // Button + optional expanded summary only — no outer wrapper or key, since
  // the two call sites below (flat rail vs. lane column) each need a
  // differently-classed wrapper around it, and only they know this node's
  // position for the key.
  const renderNode = (node: NodeState) => (
    <>
      <button
        type="button"
        className={`swarm-rail-node swarm-rail-node-${node.status}`}
        onClick={() => toggle(node.key)}
        disabled={!node.summary}
        aria-expanded={expanded.has(node.key)}
      >
        <span className={`swarm-rail-dot swarm-rail-dot-${node.status}`} aria-hidden="true" />
        <span className="swarm-rail-node-name">{node.name}</span>
        <span className="swarm-rail-node-status">
          {node.status === "pending" ? "waiting…" : node.status === "running" ? "working…" : "done"}
        </span>
      </button>
      {expanded.has(node.key) && node.summary && <pre className="swarm-rail-summary">{cleanSummary(node.summary)}</pre>}
    </>
  );

  return (
    <aside className="swarm-rail">
      <div className="swarm-rail-title">Sub-agents</div>
      {isIdle ? (
        <p className="swarm-rail-idle">
          No sub-agent activity yet. Ask the agent to use <code>local_swarm</code> or <code>local_graph</code> and it
          will show up here live.
        </p>
      ) : waves ? (
        // local_graph: dependency waves as columns — everything in one
        // column genuinely ran concurrently on the backend (asyncio.gather
        // over that same batch), so this is a real picture of parallelism,
        // not a decorative layout. A node not yet started still renders
        // (as "pending") since graph_structure carries every node's name
        // upfront, before its first node_start event arrives.
        <div className="swarm-rail-lanes">
          {waves.map((wave, waveIdx) => (
            <div className="swarm-rail-lane-group" key={waveIdx}>
              <div className="swarm-rail-lane">
                {wave.map((name) => {
                  const node = nodeByName.get(name) ?? { key: name, name, status: "pending" as const };
                  return (
                    <div className="swarm-rail-step" key={node.key}>
                      {renderNode(node)}
                    </div>
                  );
                })}
              </div>
              {waveIdx < waves.length - 1 && (
                <div className="swarm-rail-lane-connector" aria-hidden="true">
                  <span className="swarm-rail-lane-connector-line" />
                </div>
              )}
            </div>
          ))}
        </div>
      ) : (
        <div className="swarm-rail-track">
          {nodes.map((node, i) => (
            <div className="swarm-rail-step" key={node.key}>
              {renderNode(node)}
              {i < nodes.length - 1 && (
                // Label is deliberately generic: local_swarm nodes genuinely
                // hand off (a real message exists, shown on hover); this
                // branch never has a graph_structure, so anything shown here
                // is real swarm hand-off order, not dependency order.
                <div className="swarm-rail-connector" title={handoffs[i]?.message?.trim() || undefined}>
                  <span className="swarm-rail-connector-line" aria-hidden="true" />
                  <span className="swarm-rail-connector-label">{handoffs[i] ? "hand-off" : "next"}</span>
                </div>
              )}
            </div>
          ))}
        </div>
      )}
    </aside>
  );
}

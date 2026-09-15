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

// node id -> accumulated reasoning text so far, streamed by both tools (see
// sub_agent_common.reasoning_delta) whenever a sub-agent's own model emits
// reasoning/thinking content — not every model does, so a node with none
// here just never gets a "Thinking" tab.
type ReasoningMap = Record<string, string>;

type NodeState = {
  key: string;
  name: string;
  status: "pending" | "running" | "done";
  summary?: string;
};

type Tab = "answer" | "thinking";

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

function formatElapsed(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  if (seconds < 60) return `${seconds}s`;
  return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
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
 * Each node card has two independent affordances: click the row to expand
 * it, and (once it has both a reasoning trace and a final answer) a small
 * tab pair to switch which one is showing — reasoning streams in live while
 * a node is still "working…", so you can watch it think before it answers,
 * not just read the answer after the fact.
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
 * Three independent header controls, each a different "I want less of
 * this": the collapse chevron minimizes the body without removing the
 * panel ("out of the way for a second"); the fullscreen button (⛶) opens a
 * full-viewport overlay with the exact same content at a much more
 * readable width — for actually reading several nodes' reasoning traces
 * side by side, not squeezed into a 272px rail; the ✕ calls onHide, which
 * is the *same* state ThreadSidebar's "Sub-agents panel" checkbox drives
 * (passed down from page.tsx) — a second, more discoverable place to turn
 * the whole thing off besides a checkbox buried in the settings footer.
 */
export default function SwarmProgress({ onHide }: { onHide: () => void }) {
  const { agent } = useAgent({ agentId: "sierralab_agent" });
  const [entries, setEntries] = useState<SwarmEntry[]>([]);
  const [graphStructure, setGraphStructure] = useState<GraphStructure | undefined>(undefined);
  const [reasoning, setReasoning] = useState<ReasoningMap>({});
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [activeTab, setActiveTab] = useState<Map<string, Tab>>(new Map());
  const [collapsed, setCollapsed] = useState(false);
  const [fullscreen, setFullscreen] = useState(false);
  const [, setTick] = useState(0);
  const seenRunRef = useRef(false);
  const threadIdRef = useRef<string | undefined>(undefined);
  const nodeStartRef = useRef<Map<string, number>>(new Map());

  useEffect(() => {
    if (threadIdRef.current !== agent.threadId) {
      threadIdRef.current = agent.threadId;
      seenRunRef.current = false;
      setEntries([]);
      setGraphStructure(undefined);
      setReasoning({});
      setExpanded(new Set());
      setActiveTab(new Map());
      nodeStartRef.current = new Map();
    }

    const state = agent.state as
      | { swarm_progress?: SwarmEntry[]; graph_structure?: GraphStructure; reasoning?: ReasoningMap }
      | undefined;
    const live = state?.swarm_progress;
    if (live && live.length > 0) {
      seenRunRef.current = true;
      setEntries(live);
    }
    if (state?.graph_structure) {
      setGraphStructure(state.graph_structure);
    }
    if (state?.reasoning) {
      setReasoning(state.reasoning);
    }
  }, [agent.state, agent.threadId]);

  const nodes: NodeState[] = [];
  const nodeIndex = new Map<string, number>();
  for (const entry of entries) {
    if (entry.type === "node_start") {
      const key = `${entry.node}-${nodes.length}`;
      nodeIndex.set(entry.node, nodes.length);
      nodes.push({ key, name: entry.node, status: "running" });
      if (!nodeStartRef.current.has(key)) nodeStartRef.current.set(key, Date.now());
    } else if (entry.type === "node_stop") {
      const idx = nodeIndex.get(entry.node);
      if (idx !== undefined) nodes[idx] = { ...nodes[idx], status: "done", summary: entry.summary };
    }
  }

  // Ticks once a second, only while something is actually running, purely
  // to force the live per-node timer to re-render — no other state depends
  // on it. Stops itself the instant nothing is running instead of leaving a
  // background interval ticking for an idle panel.
  const anyRunning = nodes.some((n) => n.status === "running");
  useEffect(() => {
    if (!anyRunning) return;
    const id = setInterval(() => setTick((t) => t + 1), 1000);
    return () => clearInterval(id);
  }, [anyRunning]);

  useEffect(() => {
    if (!fullscreen) return;
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") setFullscreen(false);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [fullscreen]);

  const isIdle = !seenRunRef.current || entries.length === 0;

  const handoffs = entries.filter((e): e is Extract<SwarmEntry, { type: "handoff" }> => e.type === "handoff");
  const nodeByName = new Map(nodes.map((n) => [n.name, n]));
  const waves = graphStructure ? computeWaves(graphStructure) : null;

  const totalCount = graphStructure ? graphStructure.nodes.length : nodes.length;
  const doneCount = nodes.filter((n) => n.status === "done").length;

  const toggle = (key: string) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  };

  const setTab = (key: string, tab: Tab) => {
    setActiveTab((prev) => {
      const next = new Map(prev);
      next.set(key, tab);
      return next;
    });
  };

  // Button + optional expanded detail (tabs + reasoning/answer body) — no
  // outer wrapper or key, since the two call sites below (flat rail vs.
  // lane column) each need a differently-classed wrapper around it, and
  // only they know this node's position for the key.
  const renderNode = (node: NodeState) => {
    const thinking = reasoning[node.name];
    const hasAnswer = Boolean(node.summary);
    const hasThinking = Boolean(thinking);
    const expandable = hasAnswer || hasThinking;
    const tab = activeTab.get(node.key) ?? (hasAnswer ? "answer" : "thinking");
    const elapsedMs = node.status === "running" ? Date.now() - (nodeStartRef.current.get(node.key) ?? Date.now()) : 0;

    return (
      <>
        <button
          type="button"
          className={`swarm-rail-node swarm-rail-node-${node.status}`}
          onClick={() => toggle(node.key)}
          disabled={!expandable}
          aria-expanded={expanded.has(node.key)}
        >
          <span className={`swarm-rail-dot swarm-rail-dot-${node.status}`} aria-hidden="true" />
          <span className="swarm-rail-node-name">{node.name}</span>
          {node.status === "running" && <span className="swarm-rail-node-timer">{formatElapsed(elapsedMs)}</span>}
          <span className="swarm-rail-node-status">
            {node.status === "pending" ? "waiting…" : node.status === "running" ? "working…" : "done"}
          </span>
        </button>
        {expanded.has(node.key) && expandable && (
          <div className="swarm-rail-detail">
            {hasAnswer && hasThinking && (
              <div className="swarm-rail-tabs" role="tablist">
                <button
                  type="button"
                  role="tab"
                  aria-selected={tab === "answer"}
                  className={`swarm-rail-tab${tab === "answer" ? " active" : ""}`}
                  onClick={() => setTab(node.key, "answer")}
                >
                  Answer
                </button>
                <button
                  type="button"
                  role="tab"
                  aria-selected={tab === "thinking"}
                  className={`swarm-rail-tab${tab === "thinking" ? " active" : ""}`}
                  onClick={() => setTab(node.key, "thinking")}
                >
                  Thinking
                </button>
              </div>
            )}
            {tab === "thinking" && hasThinking ? (
              <pre className="swarm-rail-summary swarm-rail-summary-thinking">{thinking}</pre>
            ) : hasAnswer ? (
              <pre className="swarm-rail-summary">{cleanSummary(node.summary!)}</pre>
            ) : (
              <pre className="swarm-rail-summary swarm-rail-summary-thinking">{thinking}</pre>
            )}
          </div>
        )}
      </>
    );
  };

  const body = isIdle ? (
    <p className="swarm-rail-idle">
      No sub-agent activity yet. Ask the agent to use <code>local_swarm</code> or <code>local_graph</code> and it will
      show up here live.
    </p>
  ) : waves ? (
    // local_graph: dependency waves as columns — everything in one column
    // genuinely ran concurrently on the backend (asyncio.gather over that
    // same batch), so this is a real picture of parallelism, not a
    // decorative layout. A node not yet started still renders (as
    // "pending") since graph_structure carries every node's name upfront,
    // before its first node_start event arrives.
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
            // hand off (a real message exists, shown on hover); this branch
            // never has a graph_structure, so anything shown here is real
            // swarm hand-off order, not dependency order.
            <div className="swarm-rail-connector" title={handoffs[i]?.message?.trim() || undefined}>
              <span className="swarm-rail-connector-line" aria-hidden="true" />
              <span className="swarm-rail-connector-label">{handoffs[i] ? "hand-off" : "next"}</span>
            </div>
          )}
        </div>
      ))}
    </div>
  );

  const badge = !isIdle && (
    <span className="swarm-rail-badge">
      {doneCount}/{totalCount || nodes.length}
    </span>
  );

  const progressBar = !isIdle && totalCount > 0 && (
    <div className="swarm-rail-progress-track">
      <div className="swarm-rail-progress-fill" style={{ width: `${(doneCount / totalCount) * 100}%` }} />
    </div>
  );

  return (
    <>
      <aside className={`swarm-rail${fullscreen ? " swarm-rail-hidden" : ""}`}>
        <div className="swarm-rail-header">
          <div className="swarm-rail-title">Sub-agents</div>
          {badge}
          <div className="swarm-rail-actions">
            <button
              type="button"
              className="swarm-rail-icon-btn"
              onClick={() => setFullscreen(true)}
              disabled={isIdle}
              aria-label="Open sub-agents in fullscreen"
              title="Fullscreen"
            >
              ⛶
            </button>
            <button
              type="button"
              className="swarm-rail-icon-btn"
              onClick={() => setCollapsed((c) => !c)}
              aria-label={collapsed ? "Expand sub-agents panel" : "Collapse sub-agents panel"}
              aria-expanded={!collapsed}
              title={collapsed ? "Expand" : "Collapse"}
            >
              {collapsed ? "▸" : "▾"}
            </button>
            <button
              type="button"
              className="swarm-rail-icon-btn"
              onClick={onHide}
              aria-label="Hide sub-agents panel"
              title="Hide panel"
            >
              ✕
            </button>
          </div>
        </div>
        {!collapsed && progressBar}
        {!collapsed && body}
      </aside>

      {fullscreen && (
        <div className="swarm-fullscreen" role="dialog" aria-modal="true" aria-label="Sub-agents, fullscreen">
          <div className="swarm-fullscreen-header">
            <div className="swarm-rail-title">Sub-agents</div>
            {badge}
            <button
              type="button"
              className="swarm-fullscreen-close"
              onClick={() => setFullscreen(false)}
              aria-label="Exit fullscreen"
            >
              Exit fullscreen  ✕
            </button>
          </div>
          {progressBar}
          <div className="swarm-fullscreen-body">{body}</div>
        </div>
      )}
    </>
  );
}

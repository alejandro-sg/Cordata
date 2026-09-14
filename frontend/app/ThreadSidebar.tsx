"use client";

import { useEffect, useState } from "react";
import AgentSettings from "./AgentSettings";

interface Thread {
  id: string;
  name: string | null;
  updatedAt: string;
}

/**
 * Hand-rolled conversation history sidebar.
 *
 * CopilotKit ships a ready-made CopilotThreadsDrawer, but it requires
 * CopilotKit Intelligence (their cloud/enterprise-self-hosted thread store,
 * gated by a license key) — see README. This talks directly to the plain
 * self-hosted runtime's free REST endpoints instead: list + resume only, no
 * rename/archive/delete (the self-hosted "sse" mode doesn't expose those).
 */
export default function ThreadSidebar({
  activeThreadId,
  onSelect,
  onNew,
  dark,
  onToggleDark,
  inspectorOn,
  onToggleInspector,
  subAgentsOn,
  onToggleSubAgents,
}: {
  activeThreadId: string;
  onSelect: (id: string) => void;
  onNew: () => void;
  dark: boolean;
  onToggleDark: () => void;
  inspectorOn: boolean;
  onToggleInspector: () => void;
  subAgentsOn: boolean;
  onToggleSubAgents: () => void;
}) {
  const [threads, setThreads] = useState<Thread[]>([]);

  const refresh = () => {
    fetch("/api/copilotkit/threads")
      .then((res) => res.json())
      .then((data) => setThreads(data.threads ?? []))
      .catch(() => setThreads([]));
  };

  useEffect(() => {
    refresh();
    const interval = setInterval(refresh, 5000);
    return () => clearInterval(interval);
  }, []);

  return (
    <aside className="thread-sidebar">
      <button className="thread-sidebar-new" onClick={onNew}>
        + New chat
      </button>
      <div className="thread-sidebar-list">
        {threads.length === 0 && <p className="thread-sidebar-empty">No conversations yet</p>}
        {threads.map((t) => (
          <button
            key={t.id}
            className={"thread-sidebar-item" + (t.id === activeThreadId ? " active" : "")}
            onClick={() => onSelect(t.id)}
          >
            <span className="thread-sidebar-name">{t.name || t.id.slice(0, 8)}</span>
            <span className="thread-sidebar-time">{new Date(t.updatedAt).toLocaleString()}</span>
          </button>
        ))}
      </div>
      <div className="thread-sidebar-footer">
        <AgentSettings />
        <label className="thread-sidebar-toggle">
          <input type="checkbox" checked={dark} onChange={onToggleDark} />
          Dark theme
        </label>
        <label className="thread-sidebar-toggle">
          <input type="checkbox" checked={inspectorOn} onChange={onToggleInspector} />
          CopilotKit dev tools
        </label>
        <label className="thread-sidebar-toggle">
          <input type="checkbox" checked={subAgentsOn} onChange={onToggleSubAgents} />
          Sub-agents panel
        </label>
      </div>
    </aside>
  );
}

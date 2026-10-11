"use client";

import { useEffect, useState } from "react";
import SettingsModal from "./SettingsModal";

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
}: {
  activeThreadId: string;
  onSelect: (id: string) => void;
  onNew: () => void;
  dark: boolean;
  onToggleDark: () => void;
  inspectorOn: boolean;
  onToggleInspector: () => void;
}) {
  const [threads, setThreads] = useState<Thread[]>([]);
  // The laptop hub's conversations (terminal, Discord, menu bar, web) — empty unless hub mode is on.
  const [shared, setShared] = useState<{ handle: string; title: string; origin: string; busy: boolean }[]>([]);
  const [settingsOpen, setSettingsOpen] = useState(false);

  const refresh = () => {
    fetch("/api/copilotkit/threads")
      .then((res) => res.json())
      .then((data) => setThreads(data.threads ?? []))
      .catch(() => setThreads([]));
    fetch("/api/hub/conversations")
      .then((res) => res.json())
      .then((data) => setShared(data.enabled ? data.conversations ?? [] : []))
      .catch(() => setShared([]));
  };

  useEffect(() => {
    refresh();
    const interval = setInterval(refresh, 5000);
    return () => clearInterval(interval);
  }, []);

  return (
    <aside className="thread-sidebar">
      <div className="thread-sidebar-brand">
        <span className="thread-sidebar-brand-mark" aria-hidden="true" />
        Trust Fabric
      </div>
      <button className="thread-sidebar-new" onClick={onNew}>
        + New chat
      </button>
      {shared.length > 0 && (
        <div className="thread-sidebar-list">
          <p className="thread-sidebar-empty">Shared with terminal · Discord · menu bar</p>
          {shared.map((c) => (
            <button
              key={c.handle}
              className={"thread-sidebar-item" + (c.handle === activeThreadId ? " active" : "")}
              onClick={() => onSelect(c.handle)}
            >
              <span className="thread-sidebar-name">{(c.busy ? "● " : "") + (c.title || c.handle)}</span>
              <span className="thread-sidebar-time">{c.handle} · from {c.origin}</span>
            </button>
          ))}
        </div>
      )}
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
        <button className="thread-sidebar-settings-button" onClick={() => setSettingsOpen(true)}>
          ⚙ Settings
        </button>
        <label className="thread-sidebar-toggle">
          <input type="checkbox" checked={dark} onChange={onToggleDark} />
          Dark theme
        </label>
        <label className="thread-sidebar-toggle">
          <input type="checkbox" checked={inspectorOn} onChange={onToggleInspector} />
          CopilotKit dev tools
        </label>
      </div>
      {settingsOpen && <SettingsModal onClose={() => setSettingsOpen(false)} />}
    </aside>
  );
}

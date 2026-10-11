"use client";

import { useEffect, useRef, useState } from "react";
import { useAgent, useCopilotKit } from "@copilotkit/react-core/v2";
import { randomId } from "./randomId";

const STORAGE_KEY = "sierralab-agents:agent-settings";

type PersistedSettings = {
  model?: string;
  temperature?: number;
  memoryWindow?: number;
  tone?: string;
  length?: string;
};

type AgentConfigState = {
  active_model?: string;
  available_models?: string[];
  temperature?: number | null;
  memory_window?: number;
  context_window_limit?: number | null;
  context_usage?: {
    input_tokens?: number;
    output_tokens?: number;
    total_tokens?: number;
    context_window_limit?: number;
  };
};

type ProviderState = {
  provider: "local" | "anthropic" | "openai_compatible";
  local: { base_url: string; model_id: string };
  anthropic: { model_id: string; api_key_set: boolean };
  openai_compatible: { base_url: string; model_id: string; api_key_set: boolean };
};

type McpServer = {
  name: string;
  command?: string;
  url?: string;
  [key: string]: unknown;
};

type SkillEntry = { name: string; description: string };

type Tab = "model" | "mcp" | "skills";

function loadPersisted(): PersistedSettings {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    return raw ? JSON.parse(raw) : {};
  } catch {
    return {};
  }
}

function savePersisted(patch: Partial<PersistedSettings>) {
  try {
    const current = loadPersisted();
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ ...current, ...patch }));
  } catch {
    // localStorage unavailable (private mode, etc.) — setting still applies
    // for this session, it just won't survive a refresh.
  }
}

/**
 * Full-viewport settings overlay — Model (provider + today's per-thread
 * config), MCP servers, and Skills, behind one sidebar button instead of an
 * always-open footer panel (formerly AgentSettings.tsx, rendered inline in
 * ThreadSidebar's footer — its logic lives in this file's Model tab now).
 * A full-viewport overlay (.settings-modal).
 *
 * Model tab keeps AgentSettings.tsx's original two mechanisms:
 * - Model / temperature / memory window: real backend mutations via chat
 *   tool calls (set_active_model etc., model_control_tool.py) through the
 *   documented "Programmatic Control" pattern (agent.addMessage +
 *   copilotkit.runAgent) — carries this app's usual local-model
 *   tool-calling-reliability caveat.
 * - Tone / response length: agent.setState() + main.py's
 *   state_context_builder — deterministic, no tool call needed.
 * Plus, new here:
 * - Provider (local/Anthropic/OpenAI-compatible): a plain REST call
 *   (/api/provider-config -> backend's /provider-config), not a chat tool —
 *   switching providers or saving a key shouldn't depend on this app's
 *   local-model tool-calling reliability, especially since switching away
 *   from that same local model is exactly the case where you can't trust it
 *   to reliably call a tool. A saved key is never echoed back — the field
 *   just shows "(configured ✓)".
 */
export default function SettingsModal({ onClose }: { onClose: () => void }) {
  const { agent, isReady } = useAgent({ agentId: "sierralab_agent" });
  const { copilotkit } = useCopilotKit();
  const [tab, setTab] = useState<Tab>("model");

  const [initial, setInitial] = useState<AgentConfigState>({});
  const [persisted, setPersisted] = useState<PersistedSettings>({});
  const [pending, setPending] = useState(false);
  const restoreAttempted = useRef(false);
  const styleRestored = useRef(false);
  const usageFetchedOnOpen = useRef(false);
  const prevRunningRef = useRef(agent.isRunning);
  const autoRefreshPendingRef = useRef(false);

  const [providerState, setProviderState] = useState<ProviderState | null>(null);
  const [providerDraft, setProviderDraft] = useState({
    anthropicApiKey: "",
    anthropicModelId: "",
    openaiBaseUrl: "",
    openaiApiKey: "",
    openaiModelId: "",
  });
  const [providerSaving, setProviderSaving] = useState(false);
  const [providerError, setProviderError] = useState<string | null>(null);

  const [servers, setServers] = useState<McpServer[]>([]);
  const [mcpName, setMcpName] = useState("");
  const [mcpConfigText, setMcpConfigText] = useState("");
  const [mcpError, setMcpError] = useState<string | null>(null);
  const [mcpRestartNotice, setMcpRestartNotice] = useState(false);

  const [skills, setSkills] = useState<SkillEntry[]>([]);
  const [skillName, setSkillName] = useState("");
  const [skillDescription, setSkillDescription] = useState("");
  const [skillBody, setSkillBody] = useState("");
  const [skillError, setSkillError] = useState<string | null>(null);

  const refreshServers = () => {
    fetch("/api/mcp-servers")
      .then((res) => res.json())
      .then(setServers)
      .catch(() => setServers([]));
  };

  const refreshSkills = () => {
    fetch("/api/skills")
      .then((res) => res.json())
      .then(setSkills)
      .catch(() => setSkills([]));
  };

  useEffect(() => {
    fetch("/api/backend-config")
      .then((res) => res.json())
      .then(setInitial)
      .catch(() => setInitial({}));
    setPersisted(loadPersisted());

    fetch("/api/provider-config")
      .then((res) => res.json())
      .then((data: ProviderState) => {
        setProviderState(data);
        setProviderDraft((prev) => ({
          ...prev,
          anthropicModelId: data.anthropic?.model_id ?? prev.anthropicModelId,
          openaiBaseUrl: data.openai_compatible?.base_url ?? prev.openaiBaseUrl,
          openaiModelId: data.openai_compatible?.model_id ?? prev.openaiModelId,
        }));
      })
      .catch(() => setProviderState(null));

    refreshServers();
    refreshSkills();
  }, []);

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [onClose]);

  const state = (agent.state ?? {}) as AgentConfigState;
  const activeModel = state.active_model ?? initial.active_model;
  const availableModels = state.available_models ?? initial.available_models ?? [];
  const temperature = state.temperature ?? initial.temperature;
  const memoryWindow = state.memory_window ?? initial.memory_window;
  const contextLimit = state.context_usage?.context_window_limit ?? state.context_window_limit ?? initial.context_window_limit;
  const usage = state.context_usage;

  const runInstruction = async (content: string) => {
    if (agent.isRunning) return;
    setPending(true);
    agent.addMessage({ id: randomId(), role: "user", content });
    try {
      await copilotkit.runAgent({ agent });
    } catch {
      // Surfaced in chat already via the run's own error handling.
    } finally {
      setPending(false);
    }
  };

  const changeModel = (modelId: string) => {
    if (!modelId || modelId === activeModel) return;
    savePersisted({ model: modelId });
    runInstruction(`Call set_active_model with model_id="${modelId}".`);
  };

  const changeTemperature = (value: number) => {
    savePersisted({ temperature: value });
    runInstruction(`Call set_temperature with temperature=${value}.`);
  };

  const changeMemoryWindow = (value: number) => {
    savePersisted({ memoryWindow: value });
    runInstruction(`Call set_memory_window with preserve_recent_messages=${value}.`);
  };

  const changeStyle = (patch: Partial<Pick<PersistedSettings, "tone" | "length">>) => {
    const next = { ...persisted, ...patch };
    setPersisted(next);
    savePersisted(patch);
    agent.setState({ ...(agent.state ?? {}), tone: next.tone, length: next.length });
  };

  const refreshUsage = () => runInstruction("Call context_usage.");

  // Restore explicit picks once, after the agent is ready — silently, via
  // POST /api/backend-config (main.py's set_agent_defaults), never through a
  // chat turn (a chat-turn restore used to fire "Call set_temperature..."
  // into every brand-new thread's permanent history whenever the persisted
  // value differed from the backend's post-restart default — see
  // set_agent_defaults' own docstring). Only ever applies to a genuinely
  // fresh thread (zero messages).
  useEffect(() => {
    if (!isReady || restoreAttempted.current) return;
    if (Object.keys(persisted).length === 0) return;
    if (initial.active_model === undefined) return; // wait for the initial fetch
    if (agent.messages.length > 0) return;
    restoreAttempted.current = true;

    const patch: { model_id?: string; temperature?: number; memory_window?: number } = {};
    if (persisted.model && persisted.model !== activeModel && (availableModels.length === 0 || availableModels.includes(persisted.model))) {
      patch.model_id = persisted.model;
    }
    if (persisted.temperature !== undefined && persisted.temperature !== temperature) {
      patch.temperature = persisted.temperature;
    }
    if (persisted.memoryWindow !== undefined && persisted.memoryWindow !== memoryWindow) {
      patch.memory_window = persisted.memoryWindow;
    }
    if (Object.keys(patch).length === 0) return;

    fetch("/api/backend-config", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(patch),
    })
      .then((res) => res.json())
      .then(setInitial)
      .catch(() => {
        // Backend unreachable at mount — persisted settings just don't
        // restore this load; nothing here is worth surfacing as a chat error.
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isReady, persisted, initial]);

  // Tone/length restore separately and immediately — agent.setState needs
  // no tool call and no backend round trip, so there's no reason to wait.
  useEffect(() => {
    if (styleRestored.current || (!persisted.tone && !persisted.length)) return;
    styleRestored.current = true;
    agent.setState({ ...(agent.state ?? {}), tone: persisted.tone, length: persisted.length });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [persisted]);

  // Bring the Context row up to date the moment Settings is opened, rather
  // than showing whatever was last fetched (possibly minutes/turns ago, or
  // never, this session) until a manual Refresh click — same runInstruction
  // path the Refresh button already uses, just fired automatically once per
  // open. Skipped on a brand-new, message-less thread (nothing to report).
  useEffect(() => {
    if (!isReady || usageFetchedOnOpen.current || agent.messages.length === 0) return;
    usageFetchedOnOpen.current = true;
    runInstruction("Call context_usage.");
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isReady]);

  // ...and keep it live while Settings stays open: on every turn's
  // true -> false isRunning transition, fire one more "Call context_usage."
  // turn of our own — except when the transition we're reacting to *is*
  // that same auto-triggered turn finishing, which autoRefreshPendingRef
  // tracks. Without that guard this would be an infinite loop: our own call
  // flips isRunning true then false again, which is itself a true -> false
  // transition. The manual Refresh button stays as a fallback for when the
  // local model narrates context_usage instead of calling it — same
  // reliability caveat every other tool-driven setting here carries.
  useEffect(() => {
    const wasRunning = prevRunningRef.current;
    const isRunningNow = agent.isRunning;
    prevRunningRef.current = isRunningNow;
    if (!(wasRunning && !isRunningNow)) return;

    if (autoRefreshPendingRef.current) {
      autoRefreshPendingRef.current = false;
      return;
    }
    autoRefreshPendingRef.current = true;
    runInstruction("Call context_usage.");
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [agent.isRunning]);

  const saveProvider = (provider: ProviderState["provider"]) => {
    setProviderSaving(true);
    setProviderError(null);
    const body: Record<string, string> = { provider };
    if (provider === "anthropic") {
      if (providerDraft.anthropicApiKey) body.anthropic_api_key = providerDraft.anthropicApiKey;
      if (providerDraft.anthropicModelId) body.anthropic_model_id = providerDraft.anthropicModelId;
    }
    if (provider === "openai_compatible") {
      if (providerDraft.openaiBaseUrl) body.openai_base_url = providerDraft.openaiBaseUrl;
      if (providerDraft.openaiApiKey) body.openai_api_key = providerDraft.openaiApiKey;
      if (providerDraft.openaiModelId) body.openai_model_id = providerDraft.openaiModelId;
    }
    fetch("/api/provider-config", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    })
      .then(async (res) => {
        const data = await res.json();
        if (!res.ok) throw new Error(data.detail ?? "Failed to save.");
        setProviderState(data);
        setProviderDraft((prev) => ({ ...prev, anthropicApiKey: "", openaiApiKey: "" }));
      })
      .catch((err) => setProviderError(err instanceof Error ? err.message : "Failed to save."))
      .finally(() => setProviderSaving(false));
  };

  const addMcpServer = () => {
    setMcpError(null);
    if (!mcpName.trim()) {
      setMcpError("Name is required.");
      return;
    }
    let config: Record<string, unknown>;
    try {
      config = JSON.parse(mcpConfigText || "{}");
    } catch {
      setMcpError("Config must be valid JSON.");
      return;
    }
    fetch("/api/mcp-servers", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ name: mcpName.trim(), config }),
    })
      .then(async (res) => {
        const data = await res.json();
        if (!res.ok) throw new Error(data.detail ?? "Failed to add server.");
        setServers(data.servers ?? []);
        setMcpRestartNotice(Boolean(data.restart_required));
        setMcpName("");
        setMcpConfigText("");
      })
      .catch((err) => setMcpError(err instanceof Error ? err.message : "Failed to add server."));
  };

  const removeMcpServer = (name: string) => {
    fetch(`/api/mcp-servers/${encodeURIComponent(name)}`, { method: "DELETE" })
      .then(async (res) => {
        const data = await res.json();
        if (!res.ok) throw new Error(data.detail ?? "Failed to remove server.");
        setServers(data.servers ?? []);
        setMcpRestartNotice(Boolean(data.restart_required));
      })
      .catch(() => refreshServers());
  };

  const createSkill = () => {
    setSkillError(null);
    if (!skillName.trim() || !skillDescription.trim() || !skillBody.trim()) {
      setSkillError("Name, description, and body are all required.");
      return;
    }
    fetch("/api/skills", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ name: skillName.trim(), description: skillDescription.trim(), body: skillBody }),
    })
      .then(async (res) => {
        const data = await res.json();
        if (!res.ok) throw new Error(data.detail ?? "Failed to create skill.");
        refreshSkills();
        setSkillName("");
        setSkillDescription("");
        setSkillBody("");
      })
      .catch((err) => setSkillError(err instanceof Error ? err.message : "Failed to create skill."));
  };

  const deleteSkill = (name: string) => {
    if (!confirm(`Delete skill "${name}"? This can't be undone.`)) return;
    fetch(`/api/skills/${encodeURIComponent(name)}`, { method: "DELETE" }).then(refreshSkills, refreshSkills);
  };

  return (
    <div className="settings-modal" role="dialog" aria-modal="true" aria-label="Settings">
      <div className="settings-modal-header">
        <span className="settings-modal-title">Settings</span>
        <div className="settings-modal-tabs">
          <button className={"settings-modal-tab" + (tab === "model" ? " active" : "")} onClick={() => setTab("model")}>
            Model
          </button>
          <button className={"settings-modal-tab" + (tab === "mcp" ? " active" : "")} onClick={() => setTab("mcp")}>
            MCP
          </button>
          <button className={"settings-modal-tab" + (tab === "skills" ? " active" : "")} onClick={() => setTab("skills")}>
            Skills
          </button>
        </div>
        <button className="settings-modal-close" onClick={onClose} aria-label="Close settings">
          Close ✕
        </button>
      </div>

      <div className="settings-modal-body">
        {tab === "model" && (
          <div className="settings-modal-section">
            <div className="settings-modal-subsection-title">Provider</div>
            <div className="agent-settings-row">
              <span className="agent-settings-label">Active provider</span>
              <select
                className="agent-settings-select"
                value={providerState?.provider ?? "local"}
                disabled={providerSaving}
                onChange={(e) => saveProvider(e.target.value as ProviderState["provider"])}
              >
                <option value="local">Local (oMLX)</option>
                <option value="anthropic">Anthropic</option>
                <option value="openai_compatible">OpenAI-compatible</option>
              </select>
            </div>

            {providerState?.provider === "local" && (
              <p className="settings-modal-hint">
                {providerState.local.model_id} @ {providerState.local.base_url}
              </p>
            )}

            {providerState?.provider === "anthropic" && (
              <>
                <div className="agent-settings-row">
                  <span className="agent-settings-label">Model ID</span>
                  <input
                    className="agent-settings-number"
                    value={providerDraft.anthropicModelId}
                    onChange={(e) => setProviderDraft((prev) => ({ ...prev, anthropicModelId: e.target.value }))}
                  />
                </div>
                <div className="agent-settings-row">
                  <span className="agent-settings-label">
                    API key {providerState.anthropic.api_key_set ? "(configured ✓)" : "(not set)"}
                  </span>
                  <input
                    type="password"
                    autoComplete="off"
                    className="agent-settings-number"
                    placeholder={providerState.anthropic.api_key_set ? "•••• (leave blank to keep)" : "sk-ant-..."}
                    value={providerDraft.anthropicApiKey}
                    onChange={(e) => setProviderDraft((prev) => ({ ...prev, anthropicApiKey: e.target.value }))}
                  />
                </div>
                <button className="agent-settings-refresh" disabled={providerSaving} onClick={() => saveProvider("anthropic")}>
                  Save
                </button>
              </>
            )}

            {providerState?.provider === "openai_compatible" && (
              <>
                <div className="agent-settings-row">
                  <span className="agent-settings-label">Base URL</span>
                  <input
                    className="agent-settings-number"
                    value={providerDraft.openaiBaseUrl}
                    onChange={(e) => setProviderDraft((prev) => ({ ...prev, openaiBaseUrl: e.target.value }))}
                  />
                </div>
                <div className="agent-settings-row">
                  <span className="agent-settings-label">Model ID</span>
                  <input
                    className="agent-settings-number"
                    value={providerDraft.openaiModelId}
                    onChange={(e) => setProviderDraft((prev) => ({ ...prev, openaiModelId: e.target.value }))}
                  />
                </div>
                <div className="agent-settings-row">
                  <span className="agent-settings-label">
                    API key {providerState.openai_compatible.api_key_set ? "(configured ✓)" : "(not set)"}
                  </span>
                  <input
                    type="password"
                    autoComplete="off"
                    className="agent-settings-number"
                    placeholder={providerState.openai_compatible.api_key_set ? "•••• (leave blank to keep)" : "sk-..."}
                    value={providerDraft.openaiApiKey}
                    onChange={(e) => setProviderDraft((prev) => ({ ...prev, openaiApiKey: e.target.value }))}
                  />
                </div>
                <button className="agent-settings-refresh" disabled={providerSaving} onClick={() => saveProvider("openai_compatible")}>
                  Save
                </button>
              </>
            )}
            {providerError && <p className="settings-modal-error">{providerError}</p>}

            {availableModels.length > 0 && (
              <>
                <div className="settings-modal-subsection-title">This conversation</div>
                <div className="agent-settings-row">
                  <span className="agent-settings-label">Model</span>
                  <select
                    className="agent-settings-select"
                    value={activeModel ?? ""}
                    disabled={pending || agent.isRunning}
                    onChange={(e) => changeModel(e.target.value)}
                  >
                    {activeModel && !availableModels.includes(activeModel) && <option value={activeModel}>{activeModel}</option>}
                    {availableModels.map((id) => (
                      <option key={id} value={id}>
                        {id}
                      </option>
                    ))}
                  </select>
                </div>

                <div className="agent-settings-row">
                  <span className="agent-settings-label">Temperature{temperature != null ? ` (${temperature})` : ""}</span>
                  <input
                    type="range"
                    min={0}
                    max={2}
                    step={0.1}
                    value={temperature ?? 1}
                    disabled={pending || agent.isRunning}
                    onChange={(e) => changeTemperature(Number(e.target.value))}
                  />
                </div>

                <div className="agent-settings-row">
                  <span className="agent-settings-label">Memory window</span>
                  <input
                    type="number"
                    className="agent-settings-number"
                    min={2}
                    max={50}
                    value={memoryWindow ?? 10}
                    disabled={pending || agent.isRunning}
                    onChange={(e) => {
                      const value = Number(e.target.value);
                      if (Number.isFinite(value) && value >= 2) changeMemoryWindow(value);
                    }}
                  />
                </div>

                <div className="agent-settings-row">
                  <span className="agent-settings-label">Tone</span>
                  <select
                    className="agent-settings-select"
                    value={persisted.tone ?? ""}
                    onChange={(e) => changeStyle({ tone: e.target.value || undefined })}
                  >
                    <option value="">Default</option>
                    <option value="casual">Casual</option>
                    <option value="formal">Formal</option>
                    <option value="playful">Playful</option>
                  </select>
                </div>

                <div className="agent-settings-row">
                  <span className="agent-settings-label">Response length</span>
                  <select
                    className="agent-settings-select"
                    value={persisted.length ?? ""}
                    onChange={(e) => changeStyle({ length: e.target.value || undefined })}
                  >
                    <option value="">Default</option>
                    <option value="short">Short</option>
                    <option value="medium">Medium</option>
                    <option value="long">Long</option>
                  </select>
                </div>

                <div className="agent-settings-row agent-settings-usage">
                  <span className="agent-settings-label">
                    Context{" "}
                    {usage?.total_tokens != null && contextLimit
                      ? `${usage.total_tokens.toLocaleString()} / ${contextLimit.toLocaleString()}`
                      : "—"}
                  </span>
                  <button className="agent-settings-refresh" disabled={pending || agent.isRunning} onClick={refreshUsage}>
                    Refresh
                  </button>
                </div>
              </>
            )}
          </div>
        )}

        {tab === "mcp" && (
          <div className="settings-modal-section">
            {mcpRestartNotice && <p className="settings-modal-notice">Restart the backend for this change to take effect.</p>}
            {servers.length === 0 && <p className="settings-modal-hint">No MCP servers configured.</p>}
            {servers.map((server) => (
              <div key={server.name} className="settings-modal-list-row">
                <span className="settings-modal-list-name">{server.name}</span>
                <span className="settings-modal-list-detail">{server.command ?? server.url ?? ""}</span>
                <button className="settings-modal-remove" onClick={() => removeMcpServer(server.name)}>
                  Remove
                </button>
              </div>
            ))}

            <div className="settings-modal-subsection-title">Add server</div>
            <div className="agent-settings-row">
              <span className="agent-settings-label">Name</span>
              <input className="agent-settings-number" value={mcpName} onChange={(e) => setMcpName(e.target.value)} placeholder="fetch" />
            </div>
            <div className="agent-settings-row">
              <span className="agent-settings-label">Config (JSON — the value that would go under this name in an mcpServers block)</span>
              <textarea
                className="settings-modal-textarea"
                rows={4}
                value={mcpConfigText}
                onChange={(e) => setMcpConfigText(e.target.value)}
                placeholder={'{"command": "npx", "args": ["-y", "@modelcontextprotocol/server-fetch"]}'}
              />
            </div>
            {mcpError && <p className="settings-modal-error">{mcpError}</p>}
            <button className="agent-settings-refresh" onClick={addMcpServer}>
              Add server
            </button>
          </div>
        )}

        {tab === "skills" && (
          <div className="settings-modal-section">
            {skills.length === 0 && <p className="settings-modal-hint">No skills yet.</p>}
            {skills.map((skill) => (
              <div key={skill.name} className="settings-modal-list-row">
                <span className="settings-modal-list-name">{skill.name}</span>
                <span className="settings-modal-list-detail">{skill.description}</span>
                <button className="settings-modal-remove" onClick={() => deleteSkill(skill.name)}>
                  Delete
                </button>
              </div>
            ))}

            <div className="settings-modal-subsection-title">Create skill</div>
            <div className="agent-settings-row">
              <span className="agent-settings-label">Name (lowercase, digits, hyphens)</span>
              <input
                className="agent-settings-number"
                value={skillName}
                onChange={(e) => setSkillName(e.target.value)}
                placeholder="pdf-table-extraction"
              />
            </div>
            <div className="agent-settings-row">
              <span className="agent-settings-label">Description</span>
              <input
                className="agent-settings-number"
                value={skillDescription}
                onChange={(e) => setSkillDescription(e.target.value)}
                placeholder="When this skill applies, one sentence"
              />
            </div>
            <div className="agent-settings-row">
              <span className="agent-settings-label">Body</span>
              <textarea
                className="settings-modal-textarea"
                rows={6}
                value={skillBody}
                onChange={(e) => setSkillBody(e.target.value)}
                placeholder="The actual instructions..."
              />
            </div>
            {skillError && <p className="settings-modal-error">{skillError}</p>}
            <button className="agent-settings-refresh" onClick={createSkill}>
              Create skill
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

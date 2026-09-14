"use client";

import { useEffect, useRef, useState } from "react";
import { useAgent, useCopilotKit } from "@copilotkit/react-core/v2";

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
 * Agent runtime config, surfaced and controlled from the frontend — the
 * CopilotKit-native answer to "make agent config available via the
 * frontend" (docs/self-healing-architecture.md §5 flagged AG-UI shared
 * state as wired-but-unused; this is the first real use of it in this
 * repo). Two different mechanisms, used deliberately per setting:
 *
 * - Model / temperature / memory window: real backend mutations
 *   (Model.update_config, conversation_manager.preserve_recent_messages),
 *   so they go through real tool calls (set_active_model, set_temperature,
 *   set_memory_window in model_control_tool.py) via the documented
 *   "Programmatic Control" pattern (agent.addMessage + copilotkit.runAgent)
 *   — same local-model tool-calling-reliability caveat as everywhere else
 *   in this app: a narrated-instead-of-called turn means the change, or
 *   the restore-on-load below, didn't actually take effect backend-side.
 * - Tone / response length: pure prompt-shaping, no backend mutation
 *   needed, so they go through agent.setState() + main.py's
 *   state_context_builder instead — deterministic, applied every turn
 *   regardless of the model's own tool-calling behavior. This is
 *   CopilotKit's own documented "Agent Config" pattern.
 *
 * Persistence: every setting the user explicitly changes is written to one
 * localStorage entry (Next.js client state, survives a refresh) and
 * restored once per load — tone/length via agent.setState (instant, no
 * backend round trip) and model/temperature/memory window via the same
 * tool-call path a manual change would use (so restoring them carries the
 * same reliability caveat as changing them).
 */
export default function AgentSettings() {
  const { agent, isReady } = useAgent({ agentId: "sierralab_agent" });
  const { copilotkit } = useCopilotKit();
  const [initial, setInitial] = useState<AgentConfigState>({});
  const [persisted, setPersisted] = useState<PersistedSettings>({});
  const [pending, setPending] = useState(false);
  const restoreAttempted = useRef(false);
  const styleRestored = useRef(false);

  useEffect(() => {
    fetch("/api/backend-config")
      .then((res) => res.json())
      .then(setInitial)
      .catch(() => setInitial({}));
    setPersisted(loadPersisted());
  }, []);

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
    agent.addMessage({ id: crypto.randomUUID(), role: "user", content });
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
  // chat turn. This used to call runInstruction, the same real-tool-call
  // path a manual slider drag uses — reasonable for "the user just dragged
  // the slider," wrong for "the app happened to mount": that fake user
  // message (e.g. "Call set_temperature with temperature=1.1.") landed
  // permanently in every new thread's history, every time the persisted
  // value differed from the backend's current default. And it *always*
  // differs right after a backend restart, since ACTIVE_TEMPERATURE etc.
  // are process-memory globals that reset to their env-derived defaults —
  // so this fired on every single new thread, forever, until the two
  // happened to match again. Reported live: exactly that message as a
  // brand-new thread's first turn, on every app load. set_agent_defaults'
  // own docstring has the fuller version of this.
  //
  // Only ever applies to a genuinely fresh thread (zero messages) — a
  // resumed thread (e.g. reopening the app into your last conversation) can
  // be sitting on an unresolved human-in-the-loop interrupt, and even a
  // silent REST call has no reason to run against one: nothing here needs a
  // live per-thread Agent, only the process-wide defaults the *next*
  // thread's Agent gets built from.
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

  if (availableModels.length === 0) return null;

  return (
    <div className="agent-settings">
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
          Context {usage?.total_tokens != null && contextLimit ? `${usage.total_tokens.toLocaleString()} / ${contextLimit.toLocaleString()}` : "—"}
        </span>
        <button className="agent-settings-refresh" disabled={pending || agent.isRunning} onClick={refreshUsage}>
          Refresh
        </button>
      </div>
    </div>
  );
}

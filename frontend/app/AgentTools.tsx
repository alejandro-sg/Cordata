"use client";

import { useFrontendTool, useInterrupt } from "@copilotkit/react-core/v2";
import ApprovalCard from "./ApprovalCard";

/**
 * Registers this app's AG-UI-native capabilities with the agent:
 *
 * - `useFrontendTool`: a real frontend tool the backend Strands agent can
 *   call. ag_ui_strands proxies any tool CopilotKit declares (via
 *   RunAgentInput.tools) straight into the agent's Strands tool registry —
 *   no backend wiring needed beyond what's already in main.py.
 * - `useInterrupt`: renders the approve/deny UI for a native Strands
 *   interrupt — main.py's `ToolBehavior(interrupt_on_call=True)` pauses the
 *   agent loop itself server-side (a checkpoint), which is a DIFFERENT
 *   mechanism from a frontend-defined tool. `useHumanInTheLoop` is for the
 *   latter (the model calling a tool the browser owns) and does not resume
 *   a backend checkpoint — using it here for shell/python_repl/file_write
 *   was the bug: clicking Approve/Deny resolved a local promise nothing on
 *   the backend was waiting on, so the pending interrupt never cleared and
 *   the next turn always hit "Thread has pending interrupt(s) not addressed
 *   by resume". `useInterrupt`'s `resolve(payload)` is what actually
 *   submits the resume back over AG-UI (docs.copilotkit.ai: "Use an
 *   interrupt when the backend workflow must stop at a fixed checkpoint").
 *
 * One `useInterrupt` call per known tool name (`enabled` filters which
 * interrupt each call renders for), plus a catch-all for anything else —
 * covers any tool loaded from backend/skills_generated/* without needing to
 * know its name in advance, since main.py gates those dynamically too.
 *
 * Must render somewhere under <CopilotKitProvider> — it has no visual output
 * of its own beyond what the hooks mount.
 */
export default function AgentTools({ onToggleDark }: { onToggleDark: () => void }) {
  useFrontendTool({
    name: "toggle_theme",
    description: "Toggle the web UI between light and dark theme.",
    handler: async () => {
      onToggleDark();
      return "Theme toggled.";
    },
  });

  useInterrupt({
    enabled: (event) => toolNameOf(event.value) === "shell",
    render: ({ interrupt, resolve }) => (
      <ApprovalCard
        toolName="shell"
        summary={`Run: ${String(toolInputOf(interrupt).command ?? "")}`}
        onApprove={() => resolve({ approved: true })}
        onDeny={() => resolve({ approved: false })}
      />
    ),
  });

  useInterrupt({
    enabled: (event) => toolNameOf(event.value) === "python_repl",
    render: ({ interrupt, resolve }) => (
      <ApprovalCard
        toolName="python_repl"
        summary={String(toolInputOf(interrupt).code ?? "")}
        code
        onApprove={() => resolve({ approved: true })}
        onDeny={() => resolve({ approved: false })}
      />
    ),
  });

  useInterrupt({
    enabled: (event) => toolNameOf(event.value) === "file_write",
    render: ({ interrupt, resolve }) => (
      <ApprovalCard
        toolName="file_write"
        summary={`Write: ${String(toolInputOf(interrupt).path ?? "")}`}
        onApprove={() => resolve({ approved: true })}
        onDeny={() => resolve({ approved: false })}
      />
    ),
  });

  // Second HITL hand-off for the self-healing loop
  // (docs/self-healing-architecture.md §4, hand-off #2/#3): the first real
  // call to a tool skill_forge wrote to backend/skills_generated/. main.py
  // gates these by their actual (dynamic) tool name, not a fixed one, so
  // this is a catch-all for any interrupted tool that isn't one of the
  // three built-ins above — covers whatever skill_forge names its output
  // without this file needing to know it in advance.
  useInterrupt({
    enabled: (event) => {
      const name = toolNameOf(event.value);
      return Boolean(name) && !["shell", "python_repl", "file_write"].includes(name);
    },
    render: ({ interrupt, resolve }) => (
      <ApprovalCard
        toolName={toolNameOf(interrupt)}
        summary={JSON.stringify(toolInputOf(interrupt), null, 2)}
        code
        allowReason
        onApprove={() => resolve({ approved: true })}
        onDeny={(reason) => resolve({ approved: false, reason })}
      />
    ),
  });

  return null;
}

// The client-side Interrupt type's `metadata` is a generic passthrough
// record (see ag_ui_strands's own interrupt construction, which stamps
// {tool_name, tool_input, strandsName} onto it) — read loosely rather than
// importing AG-UI's internal type for two fields.
function toolNameOf(interrupt: unknown): string {
  return String((interrupt as { metadata?: { tool_name?: string } } | null)?.metadata?.tool_name ?? "");
}

function toolInputOf(interrupt: unknown): Record<string, unknown> {
  return (interrupt as { metadata?: { tool_input?: Record<string, unknown> } } | null)?.metadata?.tool_input ?? {};
}

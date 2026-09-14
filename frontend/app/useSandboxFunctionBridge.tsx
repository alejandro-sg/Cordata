"use client";

import { useCallback, useState } from "react";
import { z } from "zod";
import ApprovalCard from "./ApprovalCard";

// Same-origin, proxied through app/api/sandbox/[...path]/route.ts rather
// than fetching the backend directly from the browser — see that file's own
// comment for why a direct browser-to-8001 fetch (this file's first cut)
// was wrong the same way it would have been for app/api/backend-config's
// GET, just not caught until this specific gap got reported.
const SANDBOX_API_BASE = "/api/sandbox";

type PendingApproval = {
  toolName: string;
  summary: string;
  code?: boolean;
  onDecision: (approved: boolean) => void;
};

/**
 * Builds openGenerativeUI.sandboxFunctions for CopilotKitProvider
 * (docs.copilotkit.ai/generative-ui/open-generative-ui) — the narrow, named
 * bridges a generateSandboxedUi iframe can call via
 * Websandbox.connection.remote.<name>(args). Each handler runs on this page
 * with full host privilege, invoked by JS the *model* generated and the
 * iframe executes — so every one shows an approval card and waits for a
 * real click before ever calling the backend.
 *
 * Deliberately not the same mechanism as AgentTools.tsx's useInterrupt
 * cards: those resume a paused Strands tool call on the backend
 * (ag_ui_strands's interrupt_on_call). Nothing here is a Strands tool
 * call — see backend/sandbox_functions_api.py's module docstring — so
 * there is no backend checkpoint to resume. The approval decision lives
 * entirely in this hook's own state and resolves a local Promise before
 * the fetch ever fires; useInterrupt would be the wrong primitive here the
 * same way useHumanInTheLoop was the wrong one for shell/python_repl/
 * file_write (see AgentTools.tsx's own module comment on that past bug).
 *
 * Named "Bridge", not "useSandboxFunctions", to avoid colliding by name
 * with @copilotkit/react-core/v2's own useSandboxFunctions (a read-only
 * hook that returns whatever is currently registered) — this one instead
 * builds and owns the registration.
 *
 * The returned `sandboxFunctions` array is built exactly once (useState's
 * lazy initializer) so its identity is stable across renders, as
 * CopilotKit requires ("openGenerativeUI.sandboxFunctions must be a stable
 * array"); each handler closes over requestApproval, itself stable
 * (useCallback with no dependencies), so the one-time snapshot never goes
 * stale.
 */
export function useSandboxFunctionBridge() {
  const [pending, setPending] = useState<PendingApproval | null>(null);

  const requestApproval = useCallback((toolName: string, summary: string, code?: boolean) => {
    return new Promise<boolean>((resolve) => {
      // Deferred one microtask, not called synchronously here: a
      // sandboxFunction handler is invoked by CopilotKit's own internal
      // function-call dispatch, which wraps the call in flushSync — calling
      // setPending synchronously inside that (the very first thing this
      // function used to do) can land React mid-flushSync while it's
      // already rendering elsewhere in the tree, producing "flushSync was
      // called from inside a lifecycle method" in the console. queueMicrotask
      // lets that synchronous dispatch unwind first; the approval card still
      // appears effectively immediately (a microtask, not a user-visible
      // delay), and onDecision below fires from a real click handler later,
      // which was never the part at risk.
      queueMicrotask(() => {
        setPending({
          toolName,
          summary,
          code,
          onDecision: (approved) => {
            setPending(null);
            resolve(approved);
          },
        });
      });
    });
  }, []);

  const [sandboxFunctions] = useState(() => [
    {
      name: "queryBackend",
      description:
        "Read this app's current backend config: active model, temperature, memory window, context usage, and sandbox mode.",
      parameters: z.object({}),
      handler: async () => {
        const approved = await requestApproval("queryBackend", "Read backend config (model, temperature, sandbox mode).");
        if (!approved) return { error: "Denied by user." };
        const res = await fetch(`${SANDBOX_API_BASE}/agent-config`);
        if (!res.ok) return { error: `Backend returned ${res.status}` };
        return res.json();
      },
    },
    {
      name: "readWorkspaceFile",
      description:
        "Read a text file's contents from the agent's workspace directory. path must be relative and stay inside the workspace.",
      parameters: z.object({ path: z.string().describe("Relative path inside the workspace, e.g. 'notes/data.csv'.") }),
      handler: async ({ path }: { path: string }) => {
        const approved = await requestApproval("readWorkspaceFile", `Read file: ${path}`);
        if (!approved) return { error: "Denied by user." };
        const res = await fetch(`${SANDBOX_API_BASE}/workspace-file?path=${encodeURIComponent(path)}`);
        const body = await res.json().catch(() => ({}) as Record<string, unknown>);
        if (!res.ok) return { error: (body as { detail?: string }).detail ?? `Backend returned ${res.status}` };
        return body;
      },
    },
    {
      name: "runSnippet",
      description:
        "Run a short Python snippet on the backend and return its printed output. Requires the session's sandbox mode to be at least workspace-write — ask the user to have the agent call set_sandbox_mode first if this is denied.",
      parameters: z.object({ code: z.string().describe("Python source to execute.") }),
      handler: async ({ code }: { code: string }) => {
        const approved = await requestApproval("runSnippet", code, true);
        if (!approved) return { error: "Denied by user." };
        const res = await fetch(`${SANDBOX_API_BASE}/run-snippet`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ code }),
        });
        const body = await res.json().catch(() => ({}) as Record<string, unknown>);
        if (!res.ok) return { error: (body as { detail?: string }).detail ?? `Backend returned ${res.status}` };
        return body;
      },
    },
  ]);

  const approvalUi = pending ? (
    <div
      style={{
        position: "fixed",
        top: 16,
        right: 16,
        zIndex: 1000,
        maxWidth: 420,
        boxShadow: "0 4px 20px rgba(0,0,0,0.25)",
        borderRadius: 8,
      }}
    >
      <ApprovalCard
        toolName={`generated UI → ${pending.toolName}`}
        summary={pending.summary}
        code={pending.code}
        onApprove={() => {
          pending.onDecision(true);
          return Promise.resolve();
        }}
        onDeny={() => {
          pending.onDecision(false);
          return Promise.resolve();
        }}
      />
    </div>
  ) : null;

  return { sandboxFunctions, approvalUi };
}

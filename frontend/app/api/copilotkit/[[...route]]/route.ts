import {
  CopilotRuntime,
  InMemoryAgentRunner,
  createCopilotRuntimeHandler,
} from "@copilotkit/runtime/v2";
import { HttpAgent } from "@ag-ui/client";
import { Agent as UndiciAgent, fetch as undiciFetch } from "undici";

// Node's global fetch (undici under the hood) applies a default body timeout
// to streaming responses — fine for a normal API call, fatal for a run on a
// local model, which can legitimately run many minutes (a multi-step tool
// chain, or just a slow 27B model on this hardware). Without this,
// a long-but-healthy run gets killed client-side with "terminated" /
// UND_ERR_BODY_TIMEOUT even though the backend is still working fine —
// confirmed via backend logs: the agent loop only saw a CancelledError
// *after* this client gave up on it, never an error of its own.
//
// keepAlive is a second, independent fix needed once this frontend runs in a
// Docker container reaching the backend over the LAN (the mini-hosted
// deployment): a request with a real silent gap — e.g. a tool call executing
// before the model's follow-up reply starts streaming — got the connection
// dropped ("SocketError: other side closed" in the container, a plain
// CancelledError in the backend's own log, at the exact moment the gap
// started) even with the timeouts above at 0. Same symptom class, different
// cause: nothing here was applying a client-side timeout, so it's Docker
// Desktop's own NAT/virtualized network path silently reaping a TCP
// connection it sees no bytes on for a while — reproduced consistently: a
// fast reply survives the container hop, a reply with a multi-second silent
// gap doesn't. TCP keepalive sends real packets during that gap, which the
// NAT sees as activity and won't reap.
const noTimeoutDispatcher = new UndiciAgent({
  bodyTimeout: 0,
  headersTimeout: 0,
  connect: { keepAlive: true, keepAliveInitialDelay: 10_000 },
});

// Server-side only — the backend (Strands agent + oMLX model) stays on
// whichever machine runs it; the frontend never needs to know that beyond
// this one URL. Defaults to localhost for same-machine dev; set BACKEND_URL
// when the frontend is deployed separately (e.g. a Mac mini reaching back
// to the machine actually running the model over the LAN).
const BACKEND_URL = process.env.BACKEND_URL ?? "http://localhost:8001";

const runtime = new CopilotRuntime({
  agents: {
    // Must match agui_agent.name in backend/main.py and the `agent` prop in app/layout.tsx
    sierralab_agent: new HttpAgent({
      url: BACKEND_URL,
      // Hub mode (backend/hub_client.py): the backend's agent endpoint drives agents on the laptop and
      // requires this token. Harmless when the backend runs its own agent.
      headers: process.env.SANDBOX_API_TOKEN ? { "X-Sandbox-Token": process.env.SANDBOX_API_TOKEN } : undefined,
      fetch: (url, requestInit) => undiciFetch(url, { ...requestInit, dispatcher: noTimeoutDispatcher } as never) as unknown as Promise<Response>,
    }),
  },
  runner: new InMemoryAgentRunner(),
  // Auto-applies OpenGenerativeUIMiddleware, which registers a
  // `generateSandboxedUi` frontend tool the agent can call to stream live
  // HTML/CSS/JS into a sandboxed iframe (docs.copilotkit.ai/generative-ui/
  // open-generative-ui). No frontend changes needed beyond this flag — the
  // same RunAgentInput.tools proxying AgentTools.tsx's own comment already
  // documents (any CopilotKit-declared frontend tool reaches the Strands
  // tool registry automatically) picks this one up too, and the built-in
  // activity renderer that mounts the iframe is auto-registered by
  // CopilotKitProvider once it sees the runtime advertises the capability.
  openGenerativeUI: true,
});

const handler = createCopilotRuntimeHandler({
  runtime,
  basePath: "/api/copilotkit",
});

export const GET = handler;
export const POST = handler;

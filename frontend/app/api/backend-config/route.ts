// Server-side proxy for the backend's /models config endpoint. AgentSettings
// (a client component) needs this at load time before any tool has run, but
// it must never fetch the backend directly from the browser — the backend is
// meant to stay reachable only from this Next.js server (see backend/main.py:
// "Don't expose this beyond localhost"), and a direct browser fetch breaks
// outright once frontend and backend live on different machines. Same
// BACKEND_URL env var as the main CopilotKit runtime route.
const BACKEND_URL = process.env.BACKEND_URL ?? "http://localhost:8001";

export async function GET() {
  try {
    const res = await fetch(`${BACKEND_URL}/models`, { cache: "no-store" });
    const body = await res.text();
    return new Response(body, {
      status: res.status,
      headers: { "content-type": res.headers.get("content-type") ?? "application/json" },
    });
  } catch {
    return Response.json({}, { status: 502 });
  }
}

// Silent restore path for AgentSettings.tsx's persisted model/temperature/
// memory-window — proxies to the backend's POST /models (see main.py's
// set_agent_defaults), same reason GET proxies instead of a direct browser
// fetch: the backend is only meant to be reachable from this Next.js server.
export async function POST(request: Request) {
  try {
    const body = await request.text();
    const res = await fetch(`${BACKEND_URL}/models`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body,
      cache: "no-store",
    });
    const responseBody = await res.text();
    return new Response(responseBody, {
      status: res.status,
      headers: { "content-type": res.headers.get("content-type") ?? "application/json" },
    });
  } catch {
    return Response.json({}, { status: 502 });
  }
}

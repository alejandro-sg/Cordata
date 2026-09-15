// Server-side proxy for backend/sandbox_functions_api.py's /sandbox/* routes.
// Same reason app/api/backend-config/route.ts proxies instead of a direct
// browser fetch: the backend is meant to stay reachable only from this
// Next.js server, and a direct browser fetch to a hardcoded backend origin
// breaks outright once frontend and backend run on different machines (the
// LAN/mini deployment route.ts's own BACKEND_URL comment describes).
// useSandboxFunctionBridge.tsx used to fetch NEXT_PUBLIC_BACKEND_URL
// directly from the browser — a real gap from not having read this file's
// sibling before writing that one.
//
// SANDBOX_API_TOKEN, when set, is forwarded as X-Sandbox-Token — the
// backend's _require_sandbox_token dependency (sandbox_functions_api.py)
// rejects any /sandbox/* request missing it once configured. main.py binds
// to 0.0.0.0, not 127.0.0.1 (this deployment really does run frontend and
// backend on separate LAN machines), so without this, anything on the LAN
// could call runSnippet directly and skip the approval card in
// useSandboxFunctionBridge.tsx entirely — the card is real client-side
// state, but it was never a network-level gate. Set the same value in
// backend/.env and this app's .env.local to close that.
const BACKEND_URL = process.env.BACKEND_URL ?? "http://localhost:8001";
const SANDBOX_API_TOKEN = process.env.SANDBOX_API_TOKEN;

async function proxy(request: Request, path: string[]) {
  const incoming = new URL(request.url);
  const target = `${BACKEND_URL}/sandbox/${path.join("/")}${incoming.search}`;

  const init: RequestInit = { method: request.method, cache: "no-store" };
  const headers: Record<string, string> = {};
  if (request.method !== "GET" && request.method !== "HEAD") {
    headers["content-type"] = "application/json";
    init.body = await request.text();
  }
  if (SANDBOX_API_TOKEN) {
    headers["x-sandbox-token"] = SANDBOX_API_TOKEN;
  }
  init.headers = headers;

  try {
    const res = await fetch(target, init);
    const body = await res.text();
    return new Response(body, {
      status: res.status,
      headers: { "content-type": res.headers.get("content-type") ?? "application/json" },
    });
  } catch {
    return Response.json({ detail: "backend unreachable" }, { status: 502 });
  }
}

export async function GET(request: Request, { params }: { params: Promise<{ path: string[] }> }) {
  const { path } = await params;
  return proxy(request, path);
}

export async function POST(request: Request, { params }: { params: Promise<{ path: string[] }> }) {
  const { path } = await params;
  return proxy(request, path);
}

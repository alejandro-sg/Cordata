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
// X-Sandbox-Token — the backend's _require_sandbox_token dependency
// (sandbox_functions_api.py) rejects any /sandbox/* request missing it.
// main.py binds to 0.0.0.0, not 127.0.0.1 (this deployment really does run
// frontend and backend on separate LAN machines), so without this, anything
// on the LAN could call runSnippet directly and skip the approval card in
// useSandboxFunctionBridge.tsx entirely — the card is real client-side
// state, but it was never a network-level gate. See _sandboxToken.ts for
// where the token itself comes from (env-configured, or auto-fetched from
// the backend's own localhost-only endpoint) and why a 401/403 here gets
// one retry with a freshly re-fetched token before actually failing.
import { withSandboxToken } from "../../_sandboxToken";

const BACKEND_URL = process.env.BACKEND_URL ?? "http://localhost:8001";

async function proxy(request: Request, path: string[]) {
  const incoming = new URL(request.url);
  const target = `${BACKEND_URL}/sandbox/${path.join("/")}${incoming.search}`;

  let body: string | undefined;
  if (request.method !== "GET" && request.method !== "HEAD") {
    body = await request.text();
  }

  try {
    const res = await withSandboxToken(async (token) => {
      const headers: Record<string, string> = {};
      if (body !== undefined) headers["content-type"] = "application/json";
      if (token) headers["x-sandbox-token"] = token;
      return fetch(target, { method: request.method, cache: "no-store", headers, body });
    });
    const responseBody = await res.text();
    return new Response(responseBody, {
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

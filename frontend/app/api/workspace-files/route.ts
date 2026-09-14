// Server-side proxy for the backend's /workspace-files listing — same
// reasoning as api/backend-config/route.ts: the backend must stay reachable
// only from this Next.js server, never fetched directly from the browser.
const BACKEND_URL = process.env.BACKEND_URL ?? "http://localhost:8001";

export async function GET() {
  try {
    const res = await fetch(`${BACKEND_URL}/workspace-files`, { cache: "no-store" });
    const body = await res.text();
    return new Response(body, {
      status: res.status,
      headers: { "content-type": res.headers.get("content-type") ?? "application/json" },
    });
  } catch {
    return Response.json([], { status: 502 });
  }
}

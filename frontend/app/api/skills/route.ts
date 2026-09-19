// Server-side proxy for the backend's /skills listing + creation — same
// reasoning as api/backend-config/route.ts: the backend must stay reachable
// only from this Next.js server, never fetched directly from the browser.
// Per-name delete lives in ./[name]/route.ts.
const BACKEND_URL = process.env.BACKEND_URL ?? "http://localhost:8001";

export async function GET() {
  try {
    const res = await fetch(`${BACKEND_URL}/skills`, { cache: "no-store" });
    const body = await res.text();
    return new Response(body, {
      status: res.status,
      headers: { "content-type": res.headers.get("content-type") ?? "application/json" },
    });
  } catch {
    return Response.json([], { status: 502 });
  }
}

export async function POST(request: Request) {
  try {
    const body = await request.text();
    const res = await fetch(`${BACKEND_URL}/skills`, {
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

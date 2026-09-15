// Server-side proxy for backend/games_static_api.py's /games/* routes — a
// GET-only pass-through, mirroring ../sandbox/[...path]/route.ts's own
// BACKEND_URL/SANDBOX_API_TOKEN forwarding, but relaying the backend's real
// content-type verbatim instead of falling back to "application/json": that
// route's own responses are always JSON, these are html/js/css, and
// GameSandbox.tsx's <iframe src> won't execute a .html response served as
// anything else. Reads the body as a raw ArrayBuffer rather than .text() for
// the same reason — a JS/CSS file round-tripped through a decode+re-encode
// step isn't corrupted for these text-only cases either way, but there's no
// reason to assume text encoding here when the backend already sends the
// exact bytes to relay.
const BACKEND_URL = process.env.BACKEND_URL ?? "http://localhost:8001";
const SANDBOX_API_TOKEN = process.env.SANDBOX_API_TOKEN;

export async function GET(request: Request, { params }: { params: Promise<{ path: string[] }> }) {
  const { path } = await params;
  const target = `${BACKEND_URL}/games/${path.join("/")}`;

  const headers: Record<string, string> = {};
  if (SANDBOX_API_TOKEN) {
    headers["x-sandbox-token"] = SANDBOX_API_TOKEN;
  }

  let res: Response;
  try {
    res = await fetch(target, { headers, cache: "no-store" });
  } catch {
    return Response.json({ detail: "backend unreachable" }, { status: 502 });
  }

  const body = await res.arrayBuffer();
  return new Response(body, {
    status: res.status,
    headers: { "content-type": res.headers.get("content-type") ?? "application/octet-stream" },
  });
}

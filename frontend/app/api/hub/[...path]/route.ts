// Server-side proxy for backend/main.py's /hub/* routes — the conversation hub's list and history,
// shared with the terminal, Discord and the menu bar. GET only; the token is forwarded like the
// sandbox routes (see ../../_sandboxToken.ts).
import { withSandboxToken } from "../../_sandboxToken";

const BACKEND_URL = process.env.BACKEND_URL ?? "http://localhost:8001";

export async function GET(_request: Request, { params }: { params: Promise<{ path: string[] }> }) {
  const { path } = await params;
  const target = `${BACKEND_URL}/hub/${path.map(encodeURIComponent).join("/")}`;
  try {
    const res = await withSandboxToken((token) => {
      const headers: Record<string, string> = {};
      if (token) headers["x-sandbox-token"] = token;
      return fetch(target, { headers, cache: "no-store" });
    });
    return new Response(await res.text(), { status: res.status, headers: { "content-type": "application/json" } });
  } catch {
    return Response.json({ enabled: false, conversations: [] }, { status: 502 });
  }
}

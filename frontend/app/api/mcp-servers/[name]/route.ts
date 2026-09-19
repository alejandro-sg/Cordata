// Server-side proxy for the backend's DELETE /mcp-servers/{name} — same
// reasoning as ../route.ts.
const BACKEND_URL = process.env.BACKEND_URL ?? "http://localhost:8001";

export async function DELETE(_request: Request, { params }: { params: Promise<{ name: string }> }) {
  const { name } = await params;
  try {
    const res = await fetch(`${BACKEND_URL}/mcp-servers/${encodeURIComponent(name)}`, {
      method: "DELETE",
      cache: "no-store",
    });
    const body = await res.text();
    return new Response(body, {
      status: res.status,
      headers: { "content-type": res.headers.get("content-type") ?? "application/json" },
    });
  } catch {
    return Response.json({}, { status: 502 });
  }
}

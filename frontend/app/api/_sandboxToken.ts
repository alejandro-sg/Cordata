// Used by api/sandbox/[...path]/route.ts, which forwards the SANDBOX_API_TOKEN
// value as X-Sandbox-Token.
//
// backend/settings.py auto-generates SANDBOX_API_TOKEN when it's not set in
// backend/.env — a fresh secrets.token_hex(32) every backend process start,
// never written to disk. This module is how that value reaches this
// process without needing to be copied into frontend/.env.local by hand:
// GET <BACKEND_URL>/sandbox/_local-token, which the backend only answers to
// a caller on the same machine (backend/sandbox_functions_api.py's own
// _require_localhost) — safe for this server-side Next.js process (which,
// on a single-machine setup, genuinely is that same machine) but refused
// for anything else on the LAN, which is exactly who SANDBOX_API_TOKEN
// exists to keep out in the first place.
//
// Cached in memory, not fetched fresh on every request — but the backend
// restarts (and mints a *new* random token) on every file save during dev
// (uvicorn --reload), so a stale cached token is the expected steady state,
// not an edge case: both route.ts callers below retry once on a 401/403
// with a freshly re-fetched token before giving up, via
// withSandboxToken(fn) — fn receives the token and must return the
// upstream Response; a 401/403 from fn triggers exactly one retry.
const ENV_TOKEN = process.env.SANDBOX_API_TOKEN;
const BACKEND_URL = process.env.BACKEND_URL ?? "http://localhost:8001";

let cachedAutoToken: string | null = null;

async function fetchLocalToken(): Promise<string | null> {
  try {
    const res = await fetch(`${BACKEND_URL}/sandbox/_local-token`, { cache: "no-store" });
    if (!res.ok) return null;
    const body = (await res.json()) as { token?: unknown };
    return typeof body.token === "string" ? body.token : null;
  } catch {
    // Backend unreachable, or mid-restart — the caller's own fetch to the
    // real target will fail the same way and report it; no need to log twice.
    return null;
  }
}

async function currentToken(forceRefresh: boolean): Promise<string | null> {
  if (ENV_TOKEN) return ENV_TOKEN;
  if (forceRefresh || !cachedAutoToken) {
    cachedAutoToken = await fetchLocalToken();
  }
  return cachedAutoToken;
}

/**
 * Calls `fn(token)` with the current sandbox token (or null if unavailable —
 * a request with no token header still reaches the backend, which then
 * genuinely 403s it; that's a real, reportable failure, not something to
 * swallow here). If `fn`'s response is 401/403 and the token wasn't fixed
 * via SANDBOX_API_TOKEN (env-configured tokens are never stale — retrying
 * with the same value would just fail the same way), re-fetches a fresh
 * auto-generated token once and retries.
 */
export async function withSandboxToken(fn: (token: string | null) => Promise<Response>): Promise<Response> {
  const token = await currentToken(false);
  const res = await fn(token);
  if (!ENV_TOKEN && (res.status === 401 || res.status === 403)) {
    const refreshed = await currentToken(true);
    if (refreshed && refreshed !== token) {
      return fn(refreshed);
    }
  }
  return res;
}

"""Minimal MCP server wrapping Pi-hole v6's FTL REST API — status,
enable/disable blocking, top blocked domains. First test case for turning
other mini-hosted services (see ~/docker/ on sierralab-mini) into agent
tools via mcp_manager.py, so deliberately small: four tools, stdlib-only
HTTP, one auth-call-logout cycle per tool call rather than a cached
session — nothing here needs the complexity a longer-lived session would
add for a first test.

Run with the same interpreter backend/.venv already has `mcp` installed
in (this script has no other third-party dependency). Register it in
backend/mcp_servers.json, e.g.:

    {
      "mcpServers": {
        "pihole": {
          "command": "/absolute/path/to/backend/.venv/bin/python3",
          "args": ["/absolute/path/to/mcp-servers/pihole/server.py"],
          "env": {
            "PIHOLE_BASE_URL": "http://sierralab-mini.local:8080",
            "PIHOLE_API_PASSWORD": "${PIHOLE_API_PASSWORD}"
          }
        }
      }
    }

PIHOLE_API_PASSWORD is never hardcoded here or in mcp_servers.json's own
committed form — mcp_manager.py's ${VAR} interpolation (strands MCPClient's
own feature) pulls it from whatever environment actually runs the backend
process at connect time, same as this repo's other API keys.
"""

import json
import os
import urllib.error
import urllib.request

from mcp.server.mcpserver import MCPServer

BASE_URL = os.environ.get("PIHOLE_BASE_URL", "http://sierralab-mini.local:8080").rstrip("/")
API_PASSWORD = os.environ.get("PIHOLE_API_PASSWORD", "")

server = MCPServer(
    name="pihole",
    instructions="Admin tools for the Pi-hole DNS/ad-blocker on sierralab-mini: check status, "
    "pause or resume blocking, list today's top blocked domains.",
)


def _request(method: str, path: str, sid: str | None = None, body: dict | None = None) -> dict:
    """One raw call against Pi-hole's REST API. Raises RuntimeError with the
    API's own error message on any non-2xx response — Pi-hole's error body
    is already a clean {"error": {"message": ...}} shape (confirmed live
    against this exact deployment), no need to reformat it.
    """
    url = f"{BASE_URL}/api{path}"
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Content-Type", "application/json")
    if sid:
        req.add_header("sid", sid)
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            return json.loads(resp.read())
    except urllib.error.HTTPError as e:
        detail = e.read().decode(errors="replace")
        try:
            detail = json.loads(detail)["error"]["message"]
        except Exception:
            pass
        raise RuntimeError(f"Pi-hole API {method} {path} failed ({e.code}): {detail}") from e


def _authed_call(method: str, path: str, body: dict | None = None) -> dict:
    """Auth, make one call, log out — no session reuse across tool calls.
    A user-facing 401 here almost always means PIHOLE_API_PASSWORD is
    unset or stale (it's FTLCONF_webserver_api_password in
    ~/docker/pihole/docker-compose.yml on the mini), not a bug in this
    server.
    """
    if not API_PASSWORD:
        raise RuntimeError("PIHOLE_API_PASSWORD is not set — see this file's own module docstring.")
    auth = _request("POST", "/auth", body={"password": API_PASSWORD})
    sid = auth.get("session", {}).get("sid")
    if not sid:
        raise RuntimeError("Pi-hole auth succeeded but returned no session id — unexpected response shape.")
    try:
        return _request(method, path, sid=sid, body=body)
    finally:
        try:
            _request("DELETE", "/auth", sid=sid)
        except Exception:
            pass  # best-effort logout; an expired/already-dead session isn't worth surfacing as a tool error


@server.tool()
def pihole_status() -> dict:
    """Current blocking state plus today's query stats (queries, blocked, percent blocked)."""
    blocking = _authed_call("GET", "/dns/blocking")
    summary = _authed_call("GET", "/stats/summary")
    queries = summary.get("queries", {})
    return {
        "blocking_enabled": blocking.get("blocking") == "enabled",
        "blocking_timer_seconds": blocking.get("timer"),
        "queries_today": queries.get("total"),
        "blocked_today": queries.get("blocked"),
        "percent_blocked": queries.get("percent_blocked"),
    }


@server.tool()
def pihole_enable() -> dict:
    """Resume DNS blocking (undoes a pihole_disable pause)."""
    result = _authed_call("POST", "/dns/blocking", body={"blocking": True, "timer": None})
    return {"blocking_enabled": result.get("blocking") == "enabled"}


@server.tool()
def pihole_disable(seconds: int | None = None) -> dict:
    """Pause DNS blocking — indefinitely, or for `seconds` if given (auto-resumes after)."""
    result = _authed_call("POST", "/dns/blocking", body={"blocking": False, "timer": seconds})
    return {"blocking_enabled": result.get("blocking") == "enabled", "resumes_in_seconds": result.get("timer")}


@server.tool()
def pihole_top_blocked(count: int = 10) -> dict:
    """Today's most-blocked domains, most-blocked first."""
    result = _authed_call("GET", f"/stats/top_domains?blocked=true&count={count}")
    domains = result.get("top_domains") or result.get("domains") or []
    return {"top_blocked": domains}


if __name__ == "__main__":
    server.run()

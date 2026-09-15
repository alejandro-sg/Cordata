"""Regression check for sandbox_functions_api.py's _require_sandbox_token
and _require_localhost gates.

SANDBOX_API_TOKEN is never empty (settings.py auto-generates one when
SANDBOX_API_TOKEN isn't set in the environment) — /sandbox/* and /games/*
always require the correct token, whether it's the env-configured value or
the process's own auto-generated one. GET /sandbox/_local-token is how a
same-machine caller (the Next.js proxy, in the common single-machine case)
discovers that auto-generated value without it ever being written to disk;
that route is gated by _require_localhost instead (client IP, not the token
itself — asking for the token already implies not having it yet), and both
gates get their own cases below.

No oMLX/network dependency, runs against an in-process FastAPI TestClient.
Run directly:

    backend/.venv/bin/python _sandbox_token_smoke_test.py
"""

import os
import sys


def _fresh_client(client_host: tuple[str, int] = ("127.0.0.1", 12345)):
    # settings.py reads SANDBOX_API_TOKEN at import time — force every
    # affected module to re-import so each case sees the env var this
    # function just set, not a value cached from an earlier case.
    for mod in ["settings", "sandbox_functions_api"]:
        sys.modules.pop(mod, None)

    from fastapi import FastAPI
    from fastapi.testclient import TestClient

    import sandbox_functions_api as api

    app = FastAPI()
    api.register_sandbox_routes(app, lambda: {"active_model": "test"})
    # Default client=("127.0.0.1", ...) so _local-token's own localhost
    # check passes for cases that need to reach it as a legitimate
    # same-machine caller would; the one case testing that check itself
    # overrides this back to TestClient's own non-localhost default.
    return TestClient(app, client=client_host), api


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


def main() -> None:
    os.environ.pop("SANDBOX_API_TOKEN", None)
    client, api = _fresh_client()
    auto_token = api.SANDBOX_API_TOKEN
    check(bool(auto_token), "unset env: SANDBOX_API_TOKEN auto-generated, not empty")
    check(
        client.get("/sandbox/agent-config").status_code == 403,
        "auto-generated token: agent-config rejects a request with no header",
    )
    check(
        client.get("/sandbox/agent-config", headers={"x-sandbox-token": "wrong-value"}).status_code == 403,
        "auto-generated token: agent-config rejects the wrong token",
    )
    check(
        client.get("/sandbox/agent-config", headers={"x-sandbox-token": auto_token}).status_code == 200,
        "auto-generated token: agent-config accepts it",
    )
    check(
        client.get("/sandbox/_local-token").json().get("token") == auto_token,
        "auto-generated token: _local-token hands back the same value from a localhost caller",
    )

    # A caller NOT on this machine (TestClient's own default client=
    # ("testclient", 50000), not 127.0.0.1/::1) must never get the token —
    # that's the entire boundary _local-token relies on, since the route
    # itself carries no token-based gate (asking for the token implies not
    # having it yet).
    remote_client, _ = _fresh_client(client_host=("203.0.113.5", 54321))
    check(
        remote_client.get("/sandbox/_local-token").status_code == 403,
        "auto-generated token: _local-token refuses a non-localhost caller",
    )

    os.environ["SANDBOX_API_TOKEN"] = "test-secret-value"
    client, api = _fresh_client()
    check(
        client.get("/sandbox/agent-config").status_code == 403,
        "explicit token: agent-config rejects a request with no header",
    )
    check(
        client.get("/sandbox/agent-config", headers={"x-sandbox-token": "wrong-value"}).status_code == 403,
        "explicit token: agent-config rejects the wrong token",
    )
    check(
        client.get("/sandbox/agent-config", headers={"x-sandbox-token": "test-secret-value"}).status_code == 200,
        "explicit token: agent-config accepts the correct token",
    )
    check(
        client.post("/sandbox/run-snippet", json={"code": "1+1"}).status_code == 403,
        "explicit token: run-snippet rejects a request with no header, before even reaching the sandbox-mode check",
    )

    os.environ.pop("SANDBOX_API_TOKEN", None)
    print("PASS: sandbox token gate behaves as designed (always enforced; _local-token is localhost-only)")


if __name__ == "__main__":
    main()

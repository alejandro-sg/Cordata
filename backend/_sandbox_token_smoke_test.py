"""Regression check for sandbox_functions_api.py's _require_sandbox_token
gate: /sandbox/* must stay open when SANDBOX_API_TOKEN is unset (this
repo's current, zero-config default) and must reject any request missing
or presenting the wrong token once it's set — no oMLX/network dependency,
runs against an in-process FastAPI TestClient. Run directly:

    backend/.venv/bin/python _sandbox_token_smoke_test.py
"""

import os
import sys


def _fresh_client():
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
    return TestClient(app)


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


def main() -> None:
    os.environ.pop("SANDBOX_API_TOKEN", None)
    client = _fresh_client()
    check(
        client.get("/sandbox/agent-config").status_code == 200,
        "unset token: agent-config reachable with no header (backward-compatible default)",
    )
    check(
        client.get("/sandbox/agent-config", headers={"x-sandbox-token": "anything"}).status_code == 200,
        "unset token: agent-config reachable even with a header present",
    )

    os.environ["SANDBOX_API_TOKEN"] = "test-secret-value"
    client = _fresh_client()
    check(
        client.get("/sandbox/agent-config").status_code == 403,
        "set token: agent-config rejects a request with no header",
    )
    check(
        client.get("/sandbox/agent-config", headers={"x-sandbox-token": "wrong-value"}).status_code == 403,
        "set token: agent-config rejects the wrong token",
    )
    check(
        client.get("/sandbox/agent-config", headers={"x-sandbox-token": "test-secret-value"}).status_code == 200,
        "set token: agent-config accepts the correct token",
    )
    check(
        client.post("/sandbox/run-snippet", json={"code": "1+1"}).status_code == 403,
        "set token: run-snippet rejects a request with no header, before even reaching the sandbox-mode check",
    )

    os.environ.pop("SANDBOX_API_TOKEN", None)
    print("PASS: sandbox token gate behaves as designed (open when unset, enforced when set)")


if __name__ == "__main__":
    main()

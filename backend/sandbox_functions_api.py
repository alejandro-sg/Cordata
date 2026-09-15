"""REST surface for frontend/app/page.tsx's openGenerativeUI.sandboxFunctions
— the narrow, named bridges a generateSandboxedUi iframe can call back into
via `Websandbox.connection.remote.<name>(args)` (CopilotKit's Open Generative
UI: docs.copilotkit.ai/generative-ui/open-generative-ui).

These calls never go through the Strands agent's own tool-call loop — they're
plain HTTP requests a `sandboxFunction` handler makes from the host page, not
agent tool_use. That means none of the existing gating applies here:
ToolBehavior(interrupt_on_call=True) (main.py) and
strands.interventions.InterventionHandler (acp_server.py) both hook the
agent's before_tool_call event, which nothing here ever raises. The approval
card the frontend shows before calling these routes (see
useSandboxFunctionBridge.tsx) is real, but it is client-side state — it
cannot, by itself, stop a request that doesn't go through that client at
all. main.py binds uvicorn to 0.0.0.0 (this deployment genuinely runs
frontend and backend on separate LAN machines — see
frontend/.env.local.example), so "these routes are only reachable from
localhost" was never actually true: anything on the LAN could call
runSnippet directly and skip the approval card entirely, same class of gap
main.py's own comment says was the reason a raw /shell endpoint was refused
elsewhere. _require_sandbox_token below closes that at the network edge —
see its own docstring.

Three functions, deliberately not one general "run a command" hatch — see
sub_agent_common.py's DENIED_TOOLS comment for the same reasoning applied to
sub-agents: a closed, narrow vocabulary is checkable; an open one isn't.

  queryBackend        — read-only. Same payload /models already serves the
                        chat UI.
  readWorkspaceFile   — read-only, contained to agent_common.WORKSPACE_DIR
                        (the same directory every other tool's relative
                        paths already resolve against).
  runSnippet          — the only one that executes anything. Reuses
                        strands_tools.python_repl directly, the same
                        already-audited call convention
                        skill_validation_hooks.py's dry run uses ("Calls the
                        already-HITL-gated python_repl tool function
                        directly ... does not re-trigger the interrupt_on_
                        call approval a real agent-initiated call would").
                        Gated on sandbox_mode_tool's mode being at least
                        workspace-write, on top of the client-side approval
                        card.

All three now also require _require_sandbox_token — not just runSnippet:
queryBackend/readWorkspaceFile were "always available" before with no gate
of any kind, which is exactly backwards for a route enumerable and callable
by anything on the LAN.
"""

import logging
from pathlib import Path

import strands_tools.python_repl as python_repl_module
from fastapi import APIRouter, Depends, Header, HTTPException, Request
from pydantic import BaseModel

import sandbox_mode_tool
from agent_common import WORKSPACE_DIR
from settings import SANDBOX_API_TOKEN, SANDBOX_API_TOKEN_AUTO_GENERATED

router = APIRouter(prefix="/sandbox")

_MAX_READ_BYTES = 200_000

_logger = logging.getLogger(__name__)
if SANDBOX_API_TOKEN_AUTO_GENERATED:
    _logger.info(
        "SANDBOX_API_TOKEN not set — auto-generated one for this process. /sandbox/* and /games/* "
        "still require it (frontend fetches it from GET /sandbox/_local-token, which only answers "
        "same-machine callers); set SANDBOX_API_TOKEN explicitly in backend/.env and "
        "frontend/.env.local instead once this backend is reachable from a different machine than "
        "the frontend's own."
    )


def _require_sandbox_token(x_sandbox_token: str | None = Header(default=None)) -> None:
    """FastAPI dependency: reject any /sandbox/* or /games/* request that
    doesn't carry the shared secret.

    SANDBOX_API_TOKEN is never empty (settings.py auto-generates one when
    unconfigured), so this always enforces — a real network-edge check,
    independent of the frontend's approval card: it stops a request that
    skips the card entirely (a raw curl, another device on the LAN), which
    the card alone never could. It does not, and isn't meant to, prove a
    human actually clicked approve on a request the legitimate frontend
    proxy makes — that's still enforced client-side only, same as before.
    """
    if x_sandbox_token != SANDBOX_API_TOKEN:
        raise HTTPException(status_code=403, detail="missing or invalid X-Sandbox-Token")


def _require_localhost(request: Request) -> None:
    """FastAPI dependency for GET /_local-token: only a caller on this same
    machine may retrieve the current (possibly auto-generated) sandbox
    token.

    Deliberately not gated by _require_sandbox_token itself — that would be
    circular, a caller needs this route precisely because it doesn't have
    the token yet. The localhost check is the real boundary here instead,
    same reasoning odysseus-dev/odysseus's own INTERNAL_TOOL_TOKEN loopback
    uses: a same-machine caller (the Next.js server process, when frontend
    and backend share a host) could already read backend/.env directly if
    it wanted to, so handing back the live token to that same trust tier
    isn't a new exposure — a different machine on the LAN, which is exactly
    who SANDBOX_API_TOKEN exists to keep out, gets a plain 403 here with no
    token revealed, same as every other /sandbox/* route without one.
    """
    client_host = request.client.host if request.client else None
    if client_host not in ("127.0.0.1", "::1"):
        raise HTTPException(status_code=403, detail="local-token is only reachable from localhost")


def _resolve_in_workspace(raw_path: str) -> Path:
    """Contain a caller-supplied relative path to WORKSPACE_DIR.

    Two checks, not one: pathlib's own `/` operator discards the left side
    entirely when the right side is absolute (`Path("/a") / "/etc/passwd" ==
    Path("/etc/passwd")`) — rejecting an absolute or `..`-bearing raw string
    up front closes that before it ever reaches the join. Re-checking
    is_relative_to after resolve() is belt-and-suspenders against anything
    the first check's string-level view can't see (symlinks).
    """
    candidate = Path(raw_path)
    if candidate.is_absolute() or ".." in candidate.parts:
        raise HTTPException(status_code=400, detail=f"path must be relative and stay inside the workspace: {raw_path!r}")

    resolved_workspace = WORKSPACE_DIR.resolve()
    resolved = (WORKSPACE_DIR / candidate).resolve()
    if not resolved.is_relative_to(resolved_workspace):
        raise HTTPException(status_code=400, detail=f"path escapes the workspace: {raw_path!r}")
    return resolved


class RunSnippetRequest(BaseModel):
    code: str


def _python_repl_text(result: dict) -> str:
    return "".join(block.get("text", "") for block in result.get("content", []) if isinstance(block, dict))


def register_sandbox_routes(app, agent_config_payload) -> None:
    """Mount this module's router, given the existing /models payload
    builder — passed in rather than imported, so this module doesn't import
    main.py (which imports agent_common at module scope) and main.py doesn't
    need to import this module before `app` exists.
    """

    @router.get("/_local-token", dependencies=[Depends(_require_localhost)])
    def get_local_token() -> dict:
        return {"token": SANDBOX_API_TOKEN}

    @router.get("/agent-config", dependencies=[Depends(_require_sandbox_token)])
    def query_backend() -> dict:
        return agent_config_payload()

    @router.get("/workspace-file", dependencies=[Depends(_require_sandbox_token)])
    def read_workspace_file(path: str) -> dict:
        resolved = _resolve_in_workspace(path)
        if not resolved.is_file():
            raise HTTPException(status_code=404, detail=f"no such file in workspace: {path!r}")

        data = resolved.read_bytes()
        truncated = len(data) > _MAX_READ_BYTES
        text = data[:_MAX_READ_BYTES].decode("utf-8", errors="replace")
        return {"path": path, "content": text, "truncated": truncated}

    @router.post("/run-snippet", dependencies=[Depends(_require_sandbox_token)])
    def run_snippet(body: RunSnippetRequest) -> dict:
        if not sandbox_mode_tool.sandbox_mode_allows_write():
            raise HTTPException(
                status_code=403,
                detail=(
                    f"sandbox mode is '{sandbox_mode_tool.CURRENT_SANDBOX_MODE}' — runSnippet needs "
                    "'workspace-write' or wider. Ask the agent to call set_sandbox_mode first."
                ),
            )

        result = python_repl_module.python_repl(
            tool={"toolUseId": "sandbox-ui-run-snippet", "name": "python_repl", "input": {"code": body.code, "interactive": False}}
        )
        return {"status": result.get("status", "error"), "output": _python_repl_text(result)}

    app.include_router(router)

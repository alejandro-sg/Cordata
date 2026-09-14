"""REST surface for frontend/app/page.tsx's openGenerativeUI.sandboxFunctions
— the narrow, named bridges a generateSandboxedUi iframe can call back into
via `Websandbox.connection.remote.<name>(args)` (CopilotKit's Open Generative
UI: docs.copilotkit.ai/generative-ui/open-generative-ui).

These calls never go through the Strands agent's own tool-call loop — they're
plain HTTP requests a `sandboxFunction` handler makes from the host page, not
agent tool_use. That means none of the existing gating applies here:
ToolBehavior(interrupt_on_call=True) (main.py) and
strands.interventions.InterventionHandler (acp_server.py) both hook the
agent's before_tool_call event, which nothing here ever raises. The
approval card the frontend shows before calling these routes (see
SandboxFunctions.tsx) is not a UI nicety layered on top of a real gate — for
this surface, it IS the gate. Don't add a fourth route here without adding
the matching client-side confirmation first.

Three functions, deliberately not one general "run a command" hatch — see
sub_agent_common.py's DENIED_TOOLS comment for the same reasoning applied to
sub-agents: a closed, narrow vocabulary is checkable; an open one isn't.

  queryBackend        — read-only, always available. Same payload /models
                        already serves the chat UI.
  readWorkspaceFile   — read-only, always available, contained to
                        agent_common.WORKSPACE_DIR (the same directory every
                        other tool's relative paths already resolve against).
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
"""

from pathlib import Path

import strands_tools.python_repl as python_repl_module
from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

import sandbox_mode_tool
from agent_common import WORKSPACE_DIR

router = APIRouter(prefix="/sandbox")

_MAX_READ_BYTES = 200_000


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

    @router.get("/agent-config")
    def query_backend() -> dict:
        return agent_config_payload()

    @router.get("/workspace-file")
    def read_workspace_file(path: str) -> dict:
        resolved = _resolve_in_workspace(path)
        if not resolved.is_file():
            raise HTTPException(status_code=404, detail=f"no such file in workspace: {path!r}")

        data = resolved.read_bytes()
        truncated = len(data) > _MAX_READ_BYTES
        text = data[:_MAX_READ_BYTES].decode("utf-8", errors="replace")
        return {"path": path, "content": text, "truncated": truncated}

    @router.post("/run-snippet")
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

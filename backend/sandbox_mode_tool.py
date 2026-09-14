"""Session-wide sandbox mode gating what the generated-UI sandbox functions
(frontend/app/page.tsx's openGenerativeUI.sandboxFunctions, served by
sandbox_functions_api.py) are allowed to do — independent of the top-level
agent's own per-call shell/python_repl/file_write/editor approval gate in
main.py/acp_server.py, which always applies regardless of this setting.

Three modes:
  read-only          — sandbox functions may only read (queryBackend,
                        readWorkspaceFile). Default.
  workspace-write     — also allows runSnippet (Python via python_repl,
                        scoped to backend/workspace/ like every other tool).
  danger-full-access  — same as workspace-write today; reserved for a wider
                        execution backend later (e.g. a remote sandbox) so
                        callers already have a name for "no local
                        restriction" without a second migration.

Process-global, same pattern as agent_common.ACTIVE_MODEL_ID: this module
owns the value and its own tool mutates it; other modules (sandbox_functions_
api.py) read it as sandbox_mode_tool.CURRENT_SANDBOX_MODE. Resets to
read-only on backend restart — deliberately not persisted, since silently
resuming a previous session in a wider mode is a worse default than asking
again.
"""

from typing import Literal

from strands import tool

SandboxMode = Literal["read-only", "workspace-write", "danger-full-access"]

_MODES: tuple[SandboxMode, ...] = ("read-only", "workspace-write", "danger-full-access")

CURRENT_SANDBOX_MODE: SandboxMode = "read-only"


def sandbox_mode_allows_write() -> bool:
    """Whether the current mode permits the mutating/executing sandbox
    function (runSnippet), as opposed to only the read-only ones
    (queryBackend, readWorkspaceFile)."""
    return CURRENT_SANDBOX_MODE in ("workspace-write", "danger-full-access")


@tool
def set_sandbox_mode(mode: str) -> str:
    """Set the mode for generated UI's sandbox functions (generateSandboxedUi).

    Governs only the narrow sandboxFunctions a generated UI's own JavaScript
    can call (queryBackend, readWorkspaceFile, runSnippet) — separate from
    your own shell/python_repl/file_write/editor tools, which stay
    individually approval-gated no matter what this is set to.

    read-only (default): generated UI can read backend config and workspace
    files, nothing else.
    workspace-write: also allows runSnippet — each call still shows the user
    an approval card with the exact code before it runs.
    danger-full-access: same restrictions as workspace-write today.

    Args:
        mode: One of "read-only", "workspace-write", "danger-full-access".
    """
    global CURRENT_SANDBOX_MODE
    if mode not in _MODES:
        return f"Error: mode must be one of {', '.join(_MODES)}."
    CURRENT_SANDBOX_MODE = mode  # type: ignore[assignment]
    return f"Sandbox-function mode set to '{mode}'."

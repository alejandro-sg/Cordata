"""Importing ``agent_common`` has no side effects a caller did not ask for.

It used to ``os.chdir(backend/workspace)`` and connect to every configured MCP server (to list their tool names) at
import time — so a test, a tool or the orchestrator CLI that only wanted a constant moved the whole process and paid
for N subprocess spawns. Now:

* the working directory is unchanged by the import; ``enter_workspace()`` is the explicit opt-in (main.py's);
* ``MCP_TOOL_NAMES`` and ``acp_approval.APPROVAL_REQUIRED_TOOLS`` are lazy: a built-in name is answered without
  listing any MCP server, and the listing happens once, on the first question about an unknown name.

    backend/.venv/bin/python _agent_common_import_smoke_test.py
"""

from __future__ import annotations

import os
import tempfile

from harness_testing import check


def main() -> None:
    start = os.getcwd()
    calls: list[int] = []
    import agent_common

    check(os.getcwd() == start, "importing agent_common leaves the working directory alone")
    check(not agent_common.MCP_TOOL_NAMES.resolved, "…and lists no MCP server")
    import acp_approval

    check("shell" in acp_approval.APPROVAL_REQUIRED_TOOLS and "editor" in acp_approval.APPROVAL_REQUIRED_TOOLS, "a built-in tool still asks")
    check(not agent_common.MCP_TOOL_NAMES.resolved, "…answered from the static part, without spawning anything")

    lazy = agent_common.LazyNames(lambda: calls.append(1) or {"mcp_a", "mcp_b"}, static={"shell"})
    check(not lazy.resolved and "shell" in lazy and not calls, "LazyNames: a static name never forces the computation")
    check("mcp_a" in lazy and "nope" not in lazy and calls == [1], "an unknown name computes once…")
    check(sorted(lazy) == ["mcp_a", "mcp_b", "shell"] and len(lazy) == 3 and calls == [1], "…and is cached for iteration and len")
    check(isinstance(lazy | {"x"}, frozenset) and "x" in (lazy | {"x"}), "set operators work and return a frozenset")

    with tempfile.TemporaryDirectory() as tmp:
        os.chdir(tmp)
        try:
            check(agent_common.enter_workspace() == agent_common.WORKSPACE_DIR and os.getcwd() == str(agent_common.WORKSPACE_DIR.resolve()), "enter_workspace() is the explicit opt-in")
        finally:
            os.chdir(start)
    print("\nagent_common import: all checks passed")


if __name__ == "__main__":
    main()

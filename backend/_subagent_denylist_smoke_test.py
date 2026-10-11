"""Regression check for sub_agent_common.resolve_tools' safety invariant: a
sub-agent (a specialist, the digest, the observer) must never receive a tool that
needs human-in-the-loop approval, because sub-agents have no interrupt path
of their own — see sub_agent_common.py's DENIED_TOOLS/ORCHESTRATOR_ONLY_TOOLS
comments for why. That invariant was previously only as strong as those two
hand-maintained sets; this pins it down as an actual check that fails loudly
if a future tool addition forgets to extend them.

Pure and fast on purpose — no oMLX server, no real Agent, no network. Run
directly:

    backend/.venv/bin/python _subagent_denylist_smoke_test.py
"""

import agent_common
import sub_agent_common as sac


def _tool_names(tools: list) -> set[str]:
    return {tool.tool_name for tool in tools}


def main() -> None:
    # A real (but never-run — the model is constructed, never called)
    # template Agent, exactly like the one a sub-agent builder passes as
    # tool_context.agent in production. Building one instead of hand-rolling
    # a stub registry is what actually catches a naming mismatch: TOOLS mixes
    # @tool-decorated functions (their own .tool_name) with raw strands_tools
    # modules (name comes from the module's own TOOL_SPEC) -- only Strands'
    # own tool_registry normalizes both correctly.
    caller = agent_common.build_template_agent()

    forbidden = sac.DENIED_TOOLS | agent_common.GENERATED_TOOL_NAMES | sac.ORCHESTRATOR_ONLY_TOOLS

    # No "tools" key at all -> the "give every safe tool" default path.
    default_names = _tool_names(sac.resolve_tools(None, caller, "smoke-test"))
    leaked = default_names & forbidden
    assert not leaked, f"resolve_tools(None, ...) leaked forbidden tool(s) to a sub-agent: {sorted(leaked)}"
    assert default_names, "resolve_tools(None, ...) returned no tools at all -- suspicious, check agent_common.TOOLS"

    # Explicitly naming a forbidden tool must still be denied, not honored.
    explicit_names = _tool_names(sac.resolve_tools(["shell", "file_read"], caller, "smoke-test"))
    assert "shell" not in explicit_names, "resolve_tools must deny an explicitly-requested forbidden tool ('shell')"
    assert "file_read" in explicit_names, "resolve_tools must still grant an explicitly-requested safe tool ('file_read')"

    # Explicit empty list means "give this sub-agent nothing" -- not the
    # same code path as "no tools key at all" (see resolve_tools' own
    # comment) -- must not silently fall back to the default set.
    empty_names = _tool_names(sac.resolve_tools([], caller, "smoke-test"))
    assert empty_names == set(), f"resolve_tools([], ...) should grant nothing, got: {sorted(empty_names)}"

    print(f"PASS: resolve_tools never leaks a forbidden tool to a sub-agent ({len(forbidden)} forbidden, {len(default_names)} granted by default)")


if __name__ == "__main__":
    main()

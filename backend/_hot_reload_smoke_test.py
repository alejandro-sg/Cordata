"""Regression check for skill_validation_hooks.py's hot_reload_generated_skill
(AfterToolCallEvent) and its knock-on effect on sub_agent_common.resolve_tools'
denylist: a tool written to skills_generated/ *after* process start must (a)
become callable on the live agent that wrote it, without a restart, and (b)
still be denied to a sub-agent, exactly as strictly as a tool that was there
at process start. Pure and fast — no oMLX/network dependency, writes and
cleans up one real file under backend/skills_generated/ so the actual
directory-scanning code paths (not a mock) are what's under test. Run
directly:

    backend/.venv/bin/python _hot_reload_smoke_test.py
"""

from pathlib import Path

from strands.hooks import AfterToolCallEvent
from strands.tools.decorator import DecoratedFunctionTool

import agent_common
import skill_validation_hooks as svh
import sub_agent_common as sac

_TEST_TOOL_NAME = "_hot_reload_smoke_test_tool"
_TEST_TOOL_PATH = svh.SKILLS_GENERATED_DIR / f"{_TEST_TOOL_NAME}.py"

_TOOL_SOURCE = f'''\
from strands import tool


@tool
def {_TEST_TOOL_NAME}(n: int) -> int:
    """A trivial generated tool, written by this smoke test only."""
    return n + 1
'''


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


def _fake_after_tool_call_event(agent, path: Path) -> AfterToolCallEvent:
    # HookEvent subclasses lock their own attributes (see registry.py's
    # BaseHookEvent.__setattr__) — constructing one directly via its real
    # __init__ (a dataclass) is the supported way to get a realistic
    # instance without needing a full live Strands tool-dispatch to fire it.
    return AfterToolCallEvent(
        agent=agent,
        selected_tool=None,
        tool_use={"toolUseId": "smoke-test", "name": "file_write", "input": {"path": str(path)}},
        invocation_state={},
        result={"toolUseId": "smoke-test", "status": "success", "content": [{"text": "wrote it"}]},
        cancel_message=None,
        duration=0.01,
    )


def main() -> None:
    if _TEST_TOOL_PATH.exists():
        _TEST_TOOL_PATH.unlink()

    caller = agent_common.build_template_agent()

    try:
        check(
            _TEST_TOOL_NAME not in caller.tool_registry.registry,
            "tool is not registered before it's ever written (sanity check)",
        )

        _TEST_TOOL_PATH.write_text(_TOOL_SOURCE)
        hooks = svh.SkillValidationHooks()
        event = _fake_after_tool_call_event(caller, _TEST_TOOL_PATH)
        hooks.hot_reload_generated_skill(event)

        check(
            _TEST_TOOL_NAME in caller.tool_registry.registry,
            "hot_reload_generated_skill registers the tool on the live agent with no restart",
        )
        check(
            isinstance(caller.tool_registry.registry[_TEST_TOOL_NAME], DecoratedFunctionTool),
            "the registered entry is a real @tool-decorated function, not a stub",
        )

        # The knock-on check: sub_agent_common must deny this tool to a
        # sub-agent even though it was never in the process-start
        # GENERATED_TOOL_NAMES snapshot — resolve_tools' own denylist has to
        # notice it was hot-loaded, not just that it existed at import time.
        default_names = {t.tool_name for t in sac.resolve_tools(None, caller, "smoke-test")}
        check(
            _TEST_TOOL_NAME not in default_names,
            "resolve_tools' default-tools path still denies a hot-loaded generated tool to a sub-agent",
        )

        explicit_names = {t.tool_name for t in sac.resolve_tools([_TEST_TOOL_NAME, "think"], caller, "smoke-test")}
        check(
            _TEST_TOOL_NAME not in explicit_names,
            "resolve_tools denies a hot-loaded generated tool even when a sub-agent spec names it explicitly",
        )
        check("think" in explicit_names, "resolve_tools still grants an explicitly-requested safe tool alongside it")

        # A second AfterToolCallEvent for the same tool (e.g. the writer
        # re-wrote it) must be a no-op, not a duplicate-registration error.
        hooks.hot_reload_generated_skill(event)
        check(True, "re-firing for an already-registered tool does not raise")
    finally:
        if _TEST_TOOL_PATH.exists():
            _TEST_TOOL_PATH.unlink()

    print("PASS: hot-loaded generated tools register live and are still denied to sub-agents")


if __name__ == "__main__":
    main()

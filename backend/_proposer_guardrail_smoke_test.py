"""Regression check for proposer_tool.py's deterministic gates — the part
that matters most, since a Proposer-authored change can target core harness
files. Exercises _parse_draft, _run_static_gate, and _run_test_gate
directly with synthetic drafts, without ever invoking the model (the writer/
analyst/critic sub-agents are irrelevant to whether the *gates* actually
catch a bad draft).

_run_test_gate genuinely shells out to run the regression smoke tests
against a sandboxed copy of backend/, so this is slower than the other
_*_smoke_test.py scripts (tens of seconds, not instant) — still no oMLX/
network dependency though. Run directly:

    backend/.venv/bin/python _proposer_guardrail_smoke_test.py
"""

import proposer_tool as pt


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


def main() -> None:
    # _parse_draft
    check(
        pt._parse_draft("FILE: skills_generated/foo.py\n---\ndef foo():\n    pass\n")
        == ("skills_generated/foo.py", "def foo():\n    pass"),
        "_parse_draft extracts path and content from a well-formed reply",
    )
    check(pt._parse_draft("no file marker here") is None, "_parse_draft rejects a reply with no FILE:/--- markers")
    check(pt._parse_draft("FILE: \n---\ncontent") is None, "_parse_draft rejects an empty path")

    # _run_static_gate: guardrail files are refused outright
    for target in ["skill_validation_hooks.py", "proposer_tool.py"]:
        reason = pt._run_static_gate(target, "def x():\n    pass\n")
        check(reason is not None and "off-limits" in reason, f"static gate refuses a draft targeting {target}")

    # _run_static_gate: path traversal refused
    reason = pt._run_static_gate("../../etc/passwd", "junk")
    check(reason is not None and "escapes" in reason, "static gate refuses a path that escapes backend/")

    # _run_static_gate: invalid syntax refused
    reason = pt._run_static_gate("skills_generated/broken.py", "def x(:\n")
    check(reason is not None and "not valid Python" in reason, "static gate refuses unparseable Python")

    # _run_static_gate: banned imports/calls refused (delegates to skill_validation_hooks.find_banned_usage)
    reason = pt._run_static_gate("skills_generated/shelly.py", "import subprocess\nsubprocess.run(['ls'])\n")
    check(reason is not None and "banned" in reason, "static gate refuses a draft importing subprocess")

    reason = pt._run_static_gate("skills_generated/evally.py", "eval('1+1')\n")
    check(reason is not None and "banned" in reason, "static gate refuses a draft calling eval")

    # _run_static_gate: a harness-file replacement missing the guardrail markers is refused
    reason = pt._run_static_gate("main.py", "print('a harness file with no gating left in it')\n")
    check(
        reason is not None and "guardrail text" in reason,
        "static gate refuses a main.py replacement stripped of its HITL guardrail markers",
    )

    # _run_static_gate: a clean, harmless draft passes
    clean_draft = (
        "from strands import tool\n\n\n"
        "@tool\n"
        "def double_it(n: int) -> int:\n"
        "    \"\"\"Doubles a number.\"\"\"\n"
        "    return n * 2\n"
    )
    reason = pt._run_static_gate("skills_generated/double_it.py", clean_draft)
    check(reason is None, "static gate passes a clean, harmless draft")

    # _run_test_gate: same clean draft, added as a genuinely new file, should
    # not break any regression smoke test in the sandboxed copy.
    print("running _run_test_gate against a sandboxed copy of backend/ (this takes a while)...")
    test_result = pt._run_test_gate("skills_generated/double_it.py", clean_draft)
    check(test_result is None, "test gate passes when the drafted change doesn't break anything")

    # _run_test_gate: a drafted agent_common.py replacement that breaks the
    # import entirely must be caught by the regression tests (they all
    # `import agent_common`).
    broken_agent_common = "this is not valid python syntax :::: ((\n"
    test_result = pt._run_test_gate("agent_common.py", broken_agent_common)
    check(test_result is not None and "Refused" in test_result, "test gate catches a drafted agent_common.py that breaks import")

    print("PASS: proposer_tool's static and test gates behave as designed")


if __name__ == "__main__":
    main()

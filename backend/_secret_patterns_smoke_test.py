"""``secret_patterns.scrub``: masks what a credential looks like, leaves code readable — against the
fixture the Rust GUI's own scrubber is held to (``proto/fixtures/redact_cases.json``) — and
``intervention_view.build``: the exact command or diff a person is shown, masked.

    backend/.venv/bin/python _secret_patterns_smoke_test.py
"""

import json
import tempfile
from pathlib import Path

import secret_guard
import secret_patterns
import intervention_view
from harness_testing import check

FIXTURE = Path(__file__).resolve().parent.parent / "proto" / "fixtures" / "redact_cases.json"


def test_fixture() -> None:
    cases = json.loads(FIXTURE.read_text())
    for case in cases["masks"]:
        out, n = secret_patterns.scrub(case["input"])
        check(all(g not in out for g in case["gone"]) and "[REDACTED" in out and n >= 1, f"masks: {case['name']}")
    for case in cases["keeps"]:
        out, n = secret_patterns.scrub(case["input"])
        check(out == case["input"] and n == 0, f"keeps readable: {case['name']}")


def test_registered_secrets() -> None:
    secret_guard.register_secret("correct-horse-battery-staple")
    out, n = secret_patterns.scrub("note: correct-horse-battery-staple is the passphrase")
    check("correct-horse" not in out and n >= 1, "a registered secret is masked wherever it appears, in any format")
    check(secret_patterns.scrub("") == ("", 0) and secret_patterns.scrub(None) == ("", 0), "empty input is fine")


def test_view() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        root = str(Path(tmp).resolve())
        (Path(root) / "a.py").write_text("print('old')\nkeep = 1\n")
        key = "sk-ABCDEF0123456789ABCDEF0123456789"
        shell = intervention_view.build("shell", {"command": f"curl -H 'Authorization: Bearer {key}' x"}, root, interrupt_id="i1", pod="orchestrator", risk={"score": 0.9, "confidence": 0.8, "band": "destructive"}, reason="armed", armed=True)
        check(shell["kind"] == "shell" and key not in shell["command"] and key not in shell["input_json"] and shell["redactions"] >= 2, "shell: the literal command, with the credential masked, counted")
        check(shell["risk_score"] == 0.9 and shell["risk_band"] == "destructive" and shell["armed"] and shell["pod"] == "orchestrator", "…and Laya's reading, the pod and the armed flag")
        edit = intervention_view.build("editor", {"command": "str_replace", "path": "a.py", "old_str": "print('old')", "new_str": f"print('{key}')"}, root, interrupt_id="i2", pod="debugger", risk=None, reason=None, armed=False)
        check(edit["kind"] == "diff" and "-print('old')" in edit["diff"] and "+print('[REDACTED]')" in edit["diff"] and key not in edit["diff"], "editor: a unified diff of what will change, read from disk, secret masked")
        check(edit["diff"].startswith("--- a/a.py\n+++ b/a.py\n"), "…with git-style headers")
        created = intervention_view.build("file_write", {"path": "new.txt", "content": "hello\n"}, root, interrupt_id="i3", pod=None, risk=None, reason=None, armed=False)
        check(created["kind"] == "diff" and "--- /dev/null" in created["diff"] and "+hello" in created["diff"], "file_write of a new file diffs against /dev/null")
        other = intervention_view.build("claude_code_engineer", {"task": "refactor"}, root, interrupt_id="i4", pod="orchestrator", risk=None, reason=None, armed=False)
        check(other["kind"] == "other" and "refactor" in other["input_json"], "any other tool: its input as JSON")
        huge = intervention_view.build("file_write", {"path": "big.txt", "content": "x\n" * 1_500_000}, root, interrupt_id="i5", pod=None, risk=None, reason=None, armed=False)
        check(len(huge["diff"]) <= intervention_view.MAX_DIFF_CHARS + 200 and "diff truncated" in huge["diff"], "an enormous diff is cut with a marker, not dropped")
        broken = intervention_view.build("editor", {"command": "str_replace", "path": "missing.py", "old_str": "a", "new_str": "b"}, root, interrupt_id="i6", pod=None, risk=None, reason=None, armed=False)
        check(broken["kind"] == "other" and "missing.py" in broken["input_json"], "an edit that cannot be previewed still shows its input")


def main() -> None:
    test_fixture()
    test_registered_secrets()
    test_view()
    print("\nsecret patterns + intervention view: all checks passed")


if __name__ == "__main__":
    main()

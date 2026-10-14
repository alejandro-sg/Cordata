"""⌘I edits (inline_edit.py): the prompt carries the selection, bounded context and the instruction; the reply is
reduced to replacement code; bad requests and model failures are reported.

    backend/.venv/bin/python _inline_edit_smoke_test.py
"""

from __future__ import annotations

import inline_edit
from harness_testing import check


def main() -> None:
    seen: list[str] = []

    def model(prompt: str) -> str:
        seen.append(prompt)
        return "Here you go:\n```python\ndef add(a: int, b: int) -> int:\n    return a + b\n```\nDone."

    result = inline_edit.edit({"code": "def add(a, b):\n    return a+b", "instruction": "add type hints", "language": "python", "path": "m.py", "before": "x" * 10_000, "after": "y" * 10_000}, model)
    check(result == {"ok": True, "code": "def add(a: int, b: int) -> int:\n    return a + b"}, f"the fenced code is the replacement ({result})")
    check("add type hints" in seen[0] and "<selection>\ndef add(a, b):" in seen[0], "the prompt names the instruction and the selection")
    check(seen[0].count("x") <= inline_edit.MAX_CONTEXT + 10 and seen[0].count("y") <= inline_edit.MAX_CONTEXT + 10, "context is bounded on both sides")
    check(inline_edit.strip_reply("return 1") == "return 1", "an unfenced reply is used as it is")
    check(not inline_edit.edit({"code": "a", "instruction": " "}, model)["ok"], "no instruction, no call")
    check(not inline_edit.edit({"code": "a" * (inline_edit.MAX_SELECTION + 1), "instruction": "x"}, model)["ok"], "an oversize selection is refused")

    def down(_: str) -> str:
        raise RuntimeError("timeout")

    check("timeout" in inline_edit.edit({"code": "a", "instruction": "x"}, down)["reason"], "a model failure is reported")
    check(not inline_edit.edit({"code": "a", "instruction": "x"}, lambda p: "  ")["ok"], "an empty reply for a real selection is refused")
    print("\nALL OK")


if __name__ == "__main__":
    main()

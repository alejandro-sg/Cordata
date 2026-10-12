"""Shared fixtures for the orchestrator's smoke tests and eval suite: a small project with a failing test suite, the
five-step plan that makes it pass (plain, grounded in a research brief, or in parallel waves), research briefs that
cite the project truthfully or not, and fake workers wired to do exactly that (or to fail in chosen ways)."""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path
from typing import Any

from workers import WorkerFamily

from .executor import Candidate
from .fakes import FakeWorker, StepScript

FILES = {
    "calc/add.py": "def add(a, b):\n    return a + b\n",
    "calc/sub.py": "def sub(a, b):\n    return a - b\n",
    "calc/mul.py": "def mul(a, b):\n    return a * b\n",
    "calc/div.py": "def div(a, b):\n    if b == 0:\n        raise ZeroDivisionError('b is zero')\n    return a / b\n",
    "calc/__init__.py": "from .add import add\nfrom .sub import sub\nfrom .mul import mul\nfrom .div import div\n\n__all__ = ['add', 'sub', 'mul', 'div']\n",
}
WRONG_MUL = "def mul(a, b):\n    return a + b\n"
STEP_FILES = [("s1", "calc/add.py"), ("s2", "calc/sub.py"), ("s3", "calc/mul.py"), ("s4", "calc/div.py"), ("s5", "calc/__init__.py")]

TESTS = '''import unittest

import calc


class TestCalc(unittest.TestCase):
    def test_ops(self):
        self.assertEqual(calc.add(2, 3), 5)
        self.assertEqual(calc.sub(2, 3), -1)
        self.assertEqual(calc.mul(2, 3), 6)
        self.assertEqual(calc.div(6, 3), 2)

    def test_div_zero(self):
        with self.assertRaises(ZeroDivisionError):
            calc.div(1, 0)


if __name__ == "__main__":
    unittest.main()
'''

PYTHON = os.path.realpath(sys.executable)
TEST_COMMAND = f"{PYTHON} -B -m unittest discover -s tests -t . -q"


def make_project(root: Path, name: str = "calcproj") -> Path:
    project = root / name
    (project / "tests").mkdir(parents=True)
    (project / "calc").mkdir()
    (project / "tests" / "__init__.py").write_text("")
    (project / "tests" / "test_calc.py").write_text(TESTS)
    (project / "README.md").write_text("# calc\n")
    (project / ".env").write_text("SECRET=do-not-leak\n")
    for argv in (["git", "init", "-q"], ["git", "add", "-A"], ["git", "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]):
        subprocess.run(argv, cwd=project, check=True, capture_output=True)
    return project


def draft(*, rollback: str = "rollback_checkpoint", extra_criteria: list[dict[str, Any]] | None = None) -> dict[str, Any]:
    return {
        "goal": "Implement the calc package (add, sub, mul, div with a ZeroDivisionError) so the tests pass.",
        "acceptance_criteria": [
            {"id": "tests", "type": "test", "description": "the unit tests pass", "target": TEST_COMMAND, "timeout_s": 60},
            {"id": "readme", "type": "file_not_changed", "description": "README untouched", "target": "README.md"},
            {"id": "div0", "type": "regex_match", "description": "div guards zero", "target": "calc/div.py::ZeroDivisionError"},
            *(extra_criteria or []),
        ],
        "steps": [
            {"step_id": sid, "name": f"write {path}", "description": f"Create {path}.", "file_scope": [path], "rollback_policy": rollback}
            for sid, path in STEP_FILES
        ],
        "verifier_commands": [],
        "assumptions": ["python 3.11+"],
    }


def _line_of(text: str, needle: str) -> int:
    """The 1-based line of ``text`` holding ``needle`` — so the briefs cite the fixture's real lines, whatever they are."""
    for number, line in enumerate(text.splitlines(), 1):
        if needle in line:
            return number
    raise ValueError(f"{needle!r} is not in the text")


def _file_evidence(text: str, path: str, quote: str) -> dict[str, str]:
    return {"kind": "file", "ref": f"{path}:{_line_of(text, quote)}", "quote": quote}


#: The ids of the findings in ``brief()``, the ones a grounded plan may cite.
FINDINGS = ("f-import", "f-ops", "f-div0", "f-readme")


def brief(project: Path) -> dict[str, Any]:
    """A research brief (``research.RESEARCH_SCHEMA``) whose every finding cites real lines of the testkit project."""
    tests = (project / "tests" / "test_calc.py").read_text()
    readme = (project / "README.md").read_text()
    return {
        "findings": [
            {"id": "f-import", "claim": "The tests import a top-level calc package.", "evidence": [_file_evidence(tests, "tests/test_calc.py", "import calc")]},
            {
                "id": "f-ops", "claim": "calc must export add, sub, mul and div with the usual arithmetic.",
                "evidence": [_file_evidence(tests, "tests/test_calc.py", "self.assertEqual(calc.mul(2, 3), 6)"), _file_evidence(tests, "tests/test_calc.py", "self.assertEqual(calc.div(6, 3), 2)")],
            },
            {"id": "f-div0", "claim": "div must raise ZeroDivisionError when b is zero.", "evidence": [_file_evidence(tests, "tests/test_calc.py", "self.assertRaises(ZeroDivisionError)")]},
            {"id": "f-readme", "claim": "The README is the project's only doc and must stay as it is.", "evidence": [_file_evidence(readme, "README.md", "# calc"), {"kind": "doc", "ref": "README.md"}]},
        ],
        "constraints": ["README.md must not change", "the tests run with unittest discover from the project root"],
        "risks": ["mul is easy to get wrong as a + b"],
        "approach": "Write one module per operation, then export them from calc/__init__.py.",
        "open_questions": [],
    }


def fabricated_brief() -> dict[str, Any]:
    """A brief citing mostly invented lines and files: one finding of four survives, so verification refuses it."""
    return {
        "findings": [
            {"id": "f-import", "claim": "The tests import a top-level calc package.", "evidence": [_file_evidence(TESTS, "tests/test_calc.py", "import calc")]},
            {"id": "f-pow", "claim": "The tests expect a pow operation.", "evidence": [{"kind": "file", "ref": "tests/test_calc.py:412", "quote": "self.assertEqual(calc.pow(2, 3), 8)"}]},
            {"id": "f-config", "claim": "calc reads its precision from a config module.", "evidence": [{"kind": "file", "ref": "calc/config.py:7", "quote": "PRECISION = 10"}]},
            {"id": "f-usage", "claim": "The README documents a command-line interface.", "evidence": [{"kind": "file", "ref": "README.md:58", "quote": "python -m calc add 2 3"}]},
        ],
        "constraints": [],
        "risks": [],
        "approach": "Add pow and a config module.",
        "open_questions": [],
    }


def grounded_draft(
    *,
    grounded_in: dict[str, list[str]] | None = None,
    complexity: str | dict[str, str] = "medium",
    depends_on: dict[str, list[str]] | None = None,
    rollback: str = "rollback_checkpoint",
    extra_criteria: list[dict[str, Any]] | None = None,
) -> dict[str, Any]:
    """``draft()`` with each step citing ``brief()`` findings, a complexity (one for all, or per step id), and — when
    given — the steps each step waits for. Steps left out of ``grounded_in`` cite ``f-ops``."""
    out = draft(rollback=rollback, extra_criteria=extra_criteria)
    cites = {"s4": ["f-div0"], "s5": ["f-import", "f-ops"]} if grounded_in is None else grounded_in
    for step in out["steps"]:
        sid = step["step_id"]
        step["grounded_in"] = list(cites.get(sid, ["f-ops"]))
        step["complexity"] = complexity if isinstance(complexity, str) else complexity.get(sid, "medium")
        if depends_on is not None and sid in depends_on:
            step["depends_on"] = list(depends_on[sid])
    return out


def parallel_draft(**kw: Any) -> dict[str, Any]:
    """A grounded draft whose s1–s4 are independent with disjoint scopes and s5 (the package init) waits for all four."""
    return grounded_draft(depends_on={"s5": ["s1", "s2", "s3", "s4"]}, **kw)


def steps_script(*, wrong: set[str] | None = None, tokens: tuple[int, int] = (1000, 300), cost: float = 0.0) -> dict[str, StepScript]:
    wrong = wrong or set()
    scripts = {}
    for sid, path in STEP_FILES:
        good = {path: FILES[path]}
        bad = {path: WRONG_MUL} if path == "calc/mul.py" else {path: "# wrong\n"}
        scripts[sid] = StepScript(writes=bad if sid in wrong else good, tokens=tokens, cost_usd=cost)
    return scripts


def fix_script(n: int = 2, *, correct: bool = True) -> dict[str, StepScript]:
    """What a worker does on the machine's ``fix-<n>`` drift-retry steps: write the whole package right (or not)."""
    files = dict(FILES) if correct else {"calc/mul.py": WRONG_MUL}
    return {f"fix-{i}": StepScript(writes=files) for i in range(1, n + 1)}


def claude_fake(**kw: Any) -> FakeWorker:
    return FakeWorker("claude-fake", WorkerFamily.CLAUDE_CODE, model="sonnet", model_family="anthropic", **kw)


def agy_fake(**kw: Any) -> FakeWorker:
    return FakeWorker("agy-fake", WorkerFamily.ANTIGRAVITY, model="gemini-3.8-flash-medium", model_family="google", **kw)


def ollama_fake(**kw: Any) -> FakeWorker:
    return FakeWorker("ollama-fake", WorkerFamily.OLLAMA, model="glm-5.3-flash", model_family="zhipu", **kw)


def cand(worker: FakeWorker, model: str | None = None) -> Candidate:
    return Candidate(worker, model or worker.model)


def tests_pass(project: Path) -> bool:
    done = subprocess.run(TEST_COMMAND.split(), cwd=project, capture_output=True, text=True, timeout=60)
    return done.returncode == 0

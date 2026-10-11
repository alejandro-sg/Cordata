"""Shared fixtures for the orchestrator's smoke tests and eval suite: a small project with a failing test suite, the
five-step plan that makes it pass, and fake workers wired to do exactly that (or to fail in chosen ways)."""

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

"""Auto Mode: the multi-objective router — the learned dispatcher, plus what a task needs and what a model can hold.

    Utility(w, t) = Q̂(w, role, c) + γ·CapMatch(w, t) − λ·NormCost(w) − μ·QuotaScarcity(w) − ν·ContextPressure(w, t)

``LearnedDispatcher`` (dispatch.py) already decides on quality, cost and quota. Two things it cannot see are added here:

* **The task's profile** (``TaskProfile``): the capabilities a step needs — ``heavy_reasoning``, ``fast_syntax``,
  ``shell_verification``, ``long_context``, ``review`` — read from its role, complexity and wording, and its context
  footprint, estimated as ``bytes(file_scope) / 3.6 + prompt + brief + 2·tool-turn headroom + expected output``.
* **The model's shape** (``model_shape``): its real context window (``ollama_cloud.REGISTRY`` for the cloud roster,
  the ``CATALOG`` below for the CLI workers) and its capability vector. A worker family can take capabilities away:
  ``OllamaWorker`` edits files but runs no shell, so it never offers ``shell_verification`` — a compile-and-test loop
  goes to Claude Code, or to a file-only worker whose result the deterministic Tier-1 verifier then checks.

**The context gate is the one hard rule added, and it cannot empty a role.** A candidate whose window cannot hold the
task (``estimated > 0.85 · window``) is dropped *only if another candidate can hold it*; otherwise everyone stays and
pressure merely orders them. A window is metadata, and metadata is not trusted to remove the last worker standing
(the cross-family critique of the plan, ``~/.cordata/orchestrator/phase2-grounded-research/critique.md``). Below the
gate, pressure is a soft penalty that grows as the task fills the window.

Like the dispatcher it extends, the router only **orders** what the gates let through: it can never add a candidate a
gate removed, and the harden role's model-family exclusion is untouched. ``rerank_step`` is the per-step seam the
executor calls, so each step of a plan is routed by its own profile rather than by the run's goal.
"""

from __future__ import annotations

import os
import re
from collections.abc import Iterable
from dataclasses import dataclass, field
from typing import Any

from .dispatch import LearnedDispatcher, Spec
from .executor import Candidate

#: A token is about 3.6 bytes of source on average (code is denser than prose; this errs on the large side).
BYTES_PER_TOKEN = 3.6
#: A candidate is too small when the task would fill more than this much of its window.
FILL_LIMIT = 0.85
DEFAULT_PROMPT_TOKENS = 2_500
DEFAULT_BRIEF_TOKENS = 3_000
TOOL_TURN_HEADROOM = 12_000
EXPECTED_OUTPUT = {"low": 4_000, "medium": 12_000, "high": 32_000}

#: The CLI workers' models: (context window, capabilities). Planning numbers for routing, not a provider contract.
CATALOG: dict[str, tuple[int, frozenset[str]]] = {
    "haiku": (200_000, frozenset({"fast_syntax", "shell_verification", "review"})),
    "sonnet": (1_000_000, frozenset({"heavy_reasoning", "fast_syntax", "shell_verification", "long_context", "review"})),
    "opus": (1_000_000, frozenset({"heavy_reasoning", "shell_verification", "long_context", "review"})),
    "gemini-3.8-flash": (1_000_000, frozenset({"fast_syntax", "shell_verification", "long_context"})),
    "gemini-3.1-pro": (1_000_000, frozenset({"heavy_reasoning", "shell_verification", "long_context", "review"})),
    "claude-sonnet-5-5": (200_000, frozenset({"heavy_reasoning", "fast_syntax", "shell_verification", "review"})),
}
UNKNOWN_WINDOW = 128_000
#: What a worker family cannot do whatever its model can.
FAMILY_LACKS: dict[str, frozenset[str]] = {"ollama": frozenset({"shell_verification"})}

_SHELL = re.compile(r"\b(test|tests|pytest|cargo|build|compile|make|npm|run the|lint|clippy|benchmark|migrat)", re.I)
_REASON = re.compile(r"\b(design|architect|algorithm|concurren|race|deadlock|security|root cause|debug|invariant|protocol)", re.I)
_SYNTAX = re.compile(r"\b(rename|typo|format|docstring|comment|import|bump|wire|boilerplate|stub)", re.I)


@dataclass(frozen=True)
class TaskProfile:
    estimated_tokens: int
    needs: frozenset[str] = frozenset()
    complexity: str = "medium"

    def to_json(self) -> dict[str, Any]:
        return {"estimated_tokens": self.estimated_tokens, "needs": sorted(self.needs), "complexity": self.complexity}


def scope_bytes(project: str, file_scope: Iterable[str], cap: int = 40_000_000) -> int:
    """How many bytes the files a scope names hold (a directory entry counts what is under it), capped."""
    total = 0
    root = os.path.realpath(project)
    for entry in file_scope:
        path = os.path.realpath(os.path.join(root, entry.rstrip("/").replace("**", "").rstrip("/*")))
        if not path.startswith(root):
            continue
        if os.path.isfile(path):
            total += os.path.getsize(path)
        elif os.path.isdir(path):
            for directory, dirs, files in os.walk(path):
                dirs[:] = [d for d in dirs if d not in {".git", "node_modules", "target", ".venv", "__pycache__"}]
                for name in files:
                    try:
                        total += os.path.getsize(os.path.join(directory, name))
                    except OSError:
                        continue
                    if total >= cap:
                        return cap
        if total >= cap:
            return cap
    return total


def profile_for(role: str, text: str, *, project: str = ".", file_scope: Iterable[str] = (), complexity: str = "medium", prompt_tokens: int | None = None, brief_tokens: int | None = None) -> TaskProfile:
    """A step's (or a phase's) profile: what it needs and how much context it will occupy."""
    scope = list(file_scope)
    files = scope_bytes(project, scope) if scope else 0
    prompt = prompt_tokens if prompt_tokens is not None else max(DEFAULT_PROMPT_TOKENS, len(text) // 4)
    estimated = int(files / BYTES_PER_TOKEN + prompt + (brief_tokens if brief_tokens is not None else DEFAULT_BRIEF_TOKENS) + 2 * TOOL_TURN_HEADROOM + EXPECTED_OUTPUT.get(complexity, 12_000))
    needs: set[str] = set()
    if role in ("harden", "audit", "conformance"):
        needs.add("review")
    if role in ("plan", "research"):
        needs.add("heavy_reasoning")
    if complexity == "high" or _REASON.search(text or ""):
        needs.add("heavy_reasoning")
    if complexity == "low" or _SYNTAX.search(text or ""):
        needs.add("fast_syntax")
    if role in ("execute", "harden") and _SHELL.search(text or ""):
        needs.add("shell_verification")
    if estimated > 150_000 or len(scope) > 25:
        needs.add("long_context")
    return TaskProfile(estimated, frozenset(needs), complexity)


def model_shape(candidate: Candidate) -> tuple[int, frozenset[str]]:
    """(context window, capabilities) for the model a candidate runs."""
    family = candidate.family
    model = (candidate.model or "").lower()
    window, caps = UNKNOWN_WINDOW, frozenset()
    if family == "ollama":
        try:
            import ollama_cloud

            spec = ollama_cloud.resolve(candidate.model or "")
            window, caps = spec.context_window, spec.capabilities
        except Exception:
            pass
    else:
        for prefix, shape in sorted(CATALOG.items(), key=lambda item: -len(item[0])):
            if model.startswith(prefix):
                window, caps = shape
                break
    return window, caps - FAMILY_LACKS.get(family, frozenset())


def capability_match(needs: frozenset[str], caps: frozenset[str]) -> float:
    """The share of what the task needs that the model offers (1.0 when it needs nothing in particular)."""
    return 1.0 if not needs else len(needs & caps) / len(needs)


def context_pressure(estimated: int, window: int) -> float:
    """0 when the task is a small fraction of the window, 1 at the fill limit and beyond (quadratic in between)."""
    fill = estimated / max(1, window * FILL_LIMIT)
    return max(0.0, min(1.0, fill)) ** 2


@dataclass
class AutoRouter(LearnedDispatcher):
    """``LearnedDispatcher`` with the task profile in the score. ``profile`` is the one in force for ``rank`` (a phase's,
    set by the machine; ``rerank_step`` sets a step's for the length of one call)."""

    gamma: float = 0.25
    nu: float = 0.4
    profile: TaskProfile | None = None
    project: str = "."
    routed: list[dict[str, Any]] = field(default_factory=list)

    def __post_init__(self) -> None:
        super().__post_init__()
        if self.mode == "auto":
            self.mode = "live"

    def _shape(self, spec: Spec) -> tuple[int, frozenset[str]]:
        return model_shape(spec.candidate)

    def eligible(self, role: str, exclude_model_families: set[str] | None = None) -> list[Spec]:
        specs = super().eligible(role, exclude_model_families)
        if self.profile is None or len(specs) < 2:
            return specs
        fits = [s for s in specs if self.profile.estimated_tokens <= FILL_LIMIT * self._shape(s)[0]]
        return fits or specs  # metadata never removes the last worker standing

    def adjust(self, spec: Spec, role: str, task_class: str) -> float:
        if self.profile is None:
            return 0.0
        window, caps = self._shape(spec)
        return self.gamma * capability_match(self.profile.needs, caps) - self.nu * context_pressure(self.profile.estimated_tokens, window)

    def rank_for(self, role: str, task_class: str, profile: TaskProfile, *, exclude_model_families: set[str] | None = None) -> list[Candidate]:
        previous, self.profile = self.profile, profile
        try:
            ranked = self.rank(role, task_class, exclude_model_families=exclude_model_families)
        finally:
            self.profile = previous
        self.routed.append({"role": role, "task_class": task_class, "profile": profile.to_json(), "order": [c.key for c in ranked]})
        return ranked

    def rerank_step(self, step: Any, candidates: list[Candidate], task_class: str) -> list[Candidate]:
        """The executor's per-step seam: ``candidates`` reordered by this step's profile. Only reorders — a candidate
        not in ``candidates`` is never added, one the profile would rank is never dropped from the list."""
        profile = profile_for("execute", f"{getattr(step, 'name', '')}\n{getattr(step, 'description', '')}", project=self.project, file_scope=getattr(step, "file_scope", []) or [], complexity=getattr(step, "complexity", "medium") or "medium")
        preferred = [c.key for c in self.rank_for("execute", task_class, profile)]
        position = {key: i for i, key in enumerate(preferred)}
        return sorted(candidates, key=lambda c: position.get(c.key, len(position)))

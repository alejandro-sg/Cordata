"""The plan artifact: the contract the state machine checks the work against.

A planner (any worker, role ``plan``) answers with the *draft* — goal, acceptance criteria, steps with their file
scopes, verifier commands, assumptions — validated against ``DRAFT_SCHEMA`` by the worker itself where it can
(``--json-schema``). ``from_draft`` turns it into a ``Plan``: it adds the id, the budget envelope and per-step state,
and runs the checks a schema cannot express. A plan that fails them is refused, not repaired: a planner that cannot
name the files a step touches has not planned it.

What the checks refuse, and why it matters later:

* a step with **no file scope** (unless it is ``read_only``) — the executor's writes are confined to it by the kernel;
* a scope entry that is absolute, leaves the project, or names a secrets file;
* duplicate step ids, an unknown criterion type, a ``file_not_changed`` criterion on a file a step is allowed to change
  (the plan contradicts itself), a ``regex_match`` whose pattern does not compile;
* **no checkable criterion at all** — a plan whose only acceptance is the model's say-so cannot be audited
  deterministically, which is the whole point of Tier 1;
* a bad ``complexity``, a ``depends_on`` naming an unknown step or the step itself, or a dependency cycle — the
  executor runs independent steps in parallel waves ordered by these edges;
* when a verified research brief exists (``findings`` is given): a ``grounded_in`` id that is not one of its
  findings, or a plan in which no step cites any finding at all.

Persisted as JSON in ``orch_runs.plan_json`` (``state.db``) and rendered on disk for a person to read.
"""

from __future__ import annotations

import copy
import re
import uuid
from dataclasses import asdict, dataclass, field
from typing import Any

import jsonschema

from workers import scope as scope_mod

CRITERION_TYPES = ("command", "test", "file_exists", "file_not_changed", "regex_match")
RISKS = ("read_only", "edits_files", "destructive")
ROLLBACK = ("rollback_checkpoint", "keep_partial")
COMPLEXITIES = ("low", "medium", "high")
STEP_STATUSES = ("pending", "running", "completed", "failed", "partial", "skipped")
MAX_STEPS = 12
MAX_CRITERIA = 20

#: What a planner is asked for. Narrower than ``PLAN_SCHEMA``: the orchestrator owns ids, budgets and state.
DRAFT_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["goal", "acceptance_criteria", "steps"],
    "additionalProperties": True,
    "properties": {
        "goal": {"type": "string", "minLength": 1},
        "acceptance_criteria": {
            "type": "array",
            "minItems": 1,
            "maxItems": MAX_CRITERIA,
            "items": {
                "type": "object",
                "required": ["id", "type", "description"],
                "properties": {
                    "id": {"type": "string"},
                    "type": {"enum": list(CRITERION_TYPES)},
                    "description": {"type": "string"},
                    "target": {"type": "string"},
                    "expected_exit_code": {"type": "integer"},
                    "timeout_s": {"type": "number"},
                },
            },
        },
        "steps": {
            "type": "array",
            "minItems": 1,
            "maxItems": MAX_STEPS,
            "items": {
                "type": "object",
                "required": ["step_id", "name", "description", "file_scope"],
                "properties": {
                    "step_id": {"type": "string"},
                    "name": {"type": "string"},
                    "description": {"type": "string"},
                    "file_scope": {"type": "array", "items": {"type": "string"}},
                    "risk": {"enum": list(RISKS)},
                    "rollback_policy": {"enum": list(ROLLBACK)},
                    "grounded_in": {"type": "array", "items": {"type": "string"}},
                    "complexity": {"enum": list(COMPLEXITIES)},
                    "depends_on": {"type": "array", "items": {"type": "string"}},
                },
            },
        },
        "verifier_commands": {"type": "array", "items": {"type": "string"}},
        "assumptions": {"type": "array", "items": {"type": "string"}},
    },
}

#: The stored artifact (the plan document's §3.2 schema).
PLAN_SCHEMA: dict[str, Any] = {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "title": "OrchestratorPlan",
    "type": "object",
    "required": ["plan_id", "goal", "acceptance_criteria", "steps", "budget_envelope"],
    "properties": {
        "plan_id": {"type": "string"},
        "version": {"type": "integer"},
        "goal": {"type": "string"},
        "acceptance_criteria": DRAFT_SCHEMA["properties"]["acceptance_criteria"],
        "steps": {
            "type": "array",
            "items": {
                "type": "object",
                "required": ["step_id", "name", "description", "file_scope"],
                "properties": {
                    **DRAFT_SCHEMA["properties"]["steps"]["items"]["properties"],
                    "checkpoint_hash": {"type": ["string", "null"]},
                    "status": {"enum": list(STEP_STATUSES)},
                },
            },
        },
        "budget_envelope": {"type": "object", "properties": {"max_usd": {"type": "number"}, "max_tokens": {"type": ["integer", "null"]}}},
        "verifier_commands": {"type": "array", "items": {"type": "string"}},
        "assumptions": {"type": "array", "items": {"type": "string"}},
    },
}


@dataclass
class Criterion:
    id: str
    type: str
    description: str
    target: str = ""
    expected_exit_code: int = 0
    timeout_s: float = 30.0


@dataclass
class Step:
    step_id: str
    name: str
    description: str
    file_scope: list[str]
    risk: str = "edits_files"
    checkpoint_hash: str | None = None
    rollback_policy: str = "rollback_checkpoint"
    status: str = "pending"
    #: Which worker/model ran it last, and how many handoffs it took — filled by the executor.
    worker: str | None = None
    attempts: int = 0
    #: Research finding ids the step rests on, how hard it is (routes the worker), and the steps it waits for.
    grounded_in: list[str] = field(default_factory=list)
    complexity: str = "medium"
    depends_on: list[str] = field(default_factory=list)


@dataclass
class Plan:
    plan_id: str
    goal: str
    acceptance_criteria: list[Criterion]
    steps: list[Step]
    budget_envelope: dict[str, Any] = field(default_factory=lambda: {"max_usd": 2.0, "max_tokens": None})
    verifier_commands: list[str] = field(default_factory=list)
    assumptions: list[str] = field(default_factory=list)
    version: int = 1

    def scope_union(self) -> list[str]:
        seen: list[str] = []
        for step in self.steps:
            for entry in step.file_scope:
                if entry not in seen:
                    seen.append(entry)
        return seen

    def to_json(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> "Plan":
        data = copy.deepcopy(data)
        criteria = [Criterion(**{k: v for k, v in c.items() if k in Criterion.__dataclass_fields__}) for c in data.pop("acceptance_criteria", [])]
        steps = [Step(**{k: v for k, v in s.items() if k in Step.__dataclass_fields__}) for s in data.pop("steps", [])]
        known = {k: v for k, v in data.items() if k in cls.__dataclass_fields__}
        return cls(acceptance_criteria=criteria, steps=steps, **known)

    def render(self) -> str:
        """The plan as a person reads it (written beside the run in the ledger, shown by front doors) —
        Antigravity visual plan artifact with Mermaid workflow graph, status checklists, and scoped alerts."""
        lines = [f"# Plan {self.plan_id} (v{self.version})", "", f"**Goal:** {self.goal}"]

        # Antigravity visual artifact: Mermaid workflow graph
        lines += ["", "```mermaid", "flowchart TD", '    Start(["Start"])']
        prev = "Start"
        for i, step in enumerate(self.steps, 1):
            sid = f"Step_{i}"
            sname = step.name.replace('"', "'")
            lines.append(f'    {prev} --> {sid}["{i}. {sname} ({step.risk})"]')
            prev = sid
        lines.append(f'    {prev} --> Audit(["Audit & Verify"])')
        lines.append("```")

        # Budget envelope & scope alert
        budget = self.budget_envelope or {}
        max_usd = budget.get("max_usd")
        budget_str = f"${max_usd:.2f} USD" if max_usd is not None else "unbounded"
        max_tok = budget.get("max_tokens")
        if max_tok:
            budget_str += f" · {max_tok:,} tokens"
        union_files = self.scope_union()
        lines += [
            "",
            "> [!NOTE]",
            f"> Budget envelope: {budget_str} | Steps: {len(self.steps)} | Target files: {len(union_files)}",
        ]

        lines += ["", "## Steps"]
        for i, step in enumerate(self.steps, 1):
            lines.append(f"{i}. [{step.status}] **{step.name}** (`{step.step_id}`, {step.risk}, {step.complexity}) — {step.description}")
            lines.append(f"   scope: {', '.join(step.file_scope) or '(read only)'}")
            if step.depends_on:
                lines.append(f"   depends on: {', '.join(step.depends_on)}")
            if step.grounded_in:
                lines.append(f"   grounded in: {', '.join(step.grounded_in)}")
        lines += ["", "## Acceptance criteria"]
        lines += [f"- `{c.id}` {c.type}{(' ' + c.target) if c.target else ''} — {c.description}" for c in self.acceptance_criteria]
        if self.verifier_commands:
            lines += ["", "## Verifiers"] + [f"- `{v}`" for v in self.verifier_commands]
        if self.assumptions:
            lines += ["", "## Assumptions"] + [f"- {a}" for a in self.assumptions]
        return "\n".join(lines) + "\n"


class PlanError(ValueError):
    """A draft that cannot become a plan. ``problems`` lists every reason, for the replan prompt."""

    def __init__(self, problems: list[str]) -> None:
        super().__init__("; ".join(problems))
        self.problems = problems


def _cycle(edges: dict[str, list[str]]) -> list[str] | None:
    """A dependency cycle among ``edges`` (step id → the ids it depends on), as the ids along it, or None."""
    state: dict[str, int] = {}  # 1 = on the DFS path, 2 = done
    path: list[str] = []

    def visit(node: str) -> list[str] | None:
        state[node] = 1
        path.append(node)
        for dep in edges.get(node, []):
            if state.get(dep) == 1:
                return path[path.index(dep):] + [dep]
            if dep not in state:
                found = visit(dep)
                if found:
                    return found
        path.pop()
        state[node] = 2
        return None

    for node in edges:
        if node not in state:
            found = visit(node)
            if found:
                return found
    return None


def problems(draft: Any, root: str, findings: set[str] | None = None) -> list[str]:
    """Every reason ``draft`` is not an acceptable plan for the project at ``root``. Empty means acceptable.

    ``findings`` is the set of verified research finding ids: when given, steps may cite only those in
    ``grounded_in`` and at least one step must cite one. ``None`` (no verified brief) leaves citations unchecked."""
    if not isinstance(draft, dict):
        return ["the plan is not a JSON object"]
    out: list[str] = []
    validator = jsonschema.Draft202012Validator(DRAFT_SCHEMA)
    for error in sorted(validator.iter_errors(draft), key=lambda e: list(e.absolute_path)):
        where = "/".join(str(p) for p in error.absolute_path) or "(root)"
        out.append(f"{where}: {error.message[:200]}")
    if out:
        return out
    step_ids: set[str] = set()
    writable: list[str] = []
    for step in draft["steps"]:
        sid = str(step["step_id"])
        if sid in step_ids:
            out.append(f"duplicate step id {sid!r}")
        step_ids.add(sid)
        risk = step.get("risk", "edits_files")
        if risk != "read_only" and not step["file_scope"]:
            out.append(f"step {sid!r} changes files but declares no file_scope")
        for entry in step["file_scope"]:
            reason = scope_mod.valid(entry, root)
            if reason:
                out.append(f"step {sid!r}: {reason}")
        if risk != "read_only":
            writable.extend(step["file_scope"])
        complexity = step.get("complexity", "medium")
        if complexity not in COMPLEXITIES:
            out.append(f"step {sid!r}: complexity {complexity!r} is not one of {', '.join(COMPLEXITIES)}")
    edges: dict[str, list[str]] = {}
    cited = 0
    for step in draft["steps"]:
        sid = str(step["step_id"])
        deps = [str(d) for d in step.get("depends_on") or []]
        for dep in deps:
            if dep == sid:
                out.append(f"step {sid!r} depends on itself")
            elif dep not in step_ids:
                out.append(f"step {sid!r} depends on unknown step {dep!r}")
        edges.setdefault(sid, []).extend(d for d in deps if d != sid and d in step_ids)
        if findings is not None:
            for ref in step.get("grounded_in") or []:
                if str(ref) in findings:
                    cited += 1
                else:
                    out.append(f"step {sid!r} cites {str(ref)!r}, which is not a verified finding")
    cycle = _cycle(edges)
    if cycle:
        out.append(f"the steps' depends_on form a cycle: {' -> '.join(cycle)}")
    if findings is not None and not cited:
        out.append("no step cites a verified research finding in grounded_in")
    checkable = 0
    ids: set[str] = set()
    for criterion in draft["acceptance_criteria"]:
        cid = str(criterion["id"])
        if cid in ids:
            out.append(f"duplicate criterion id {cid!r}")
        ids.add(cid)
        ctype, target = criterion["type"], str(criterion.get("target") or "")
        if ctype in ("command", "test"):
            if not target.strip():
                out.append(f"criterion {cid!r} ({ctype}) has no command in target")
            else:
                checkable += 1
        elif ctype in ("file_exists", "file_not_changed"):
            reason = scope_mod.valid(target, root) if target else "no path in target"
            if reason:
                out.append(f"criterion {cid!r}: {reason}")
            else:
                checkable += 1
                if ctype == "file_not_changed" and scope_mod.matches(target, writable, root):
                    out.append(f"criterion {cid!r} says {target!r} must not change, but a step's scope allows changing it")
        elif ctype == "regex_match":
            path, sep, pattern = target.partition("::")
            if not sep or not path or not pattern:
                out.append(f"criterion {cid!r} (regex_match) needs target 'path::pattern'")
            else:
                try:
                    re.compile(pattern)
                    reason = scope_mod.valid(path, root)
                    if reason:
                        out.append(f"criterion {cid!r}: {reason}")
                    else:
                        checkable += 1
                except re.error as error:
                    out.append(f"criterion {cid!r}: bad regex ({error})")
    if not checkable:
        out.append("no acceptance criterion can be checked without a model")
    for command in draft.get("verifier_commands") or []:
        if not str(command).strip():
            out.append("an empty verifier command")
    return out


def from_draft(
    draft: Any, root: str, *, max_usd: float = 2.0, max_tokens: int | None = None, plan_id: str | None = None, version: int = 1,
    findings: set[str] | None = None,
) -> Plan:
    found = problems(draft, root, findings)
    if found:
        raise PlanError(found)
    steps = [
        Step(
            step_id=str(s["step_id"]),
            name=str(s["name"]),
            description=str(s["description"]),
            file_scope=[scope_mod.normalize(e) for e in s["file_scope"]] if s.get("risk", "edits_files") != "read_only" else [],
            risk=s.get("risk", "edits_files"),
            rollback_policy=s.get("rollback_policy", "rollback_checkpoint"),
            grounded_in=[str(g) for g in s.get("grounded_in") or []],
            complexity=s.get("complexity", "medium"),
            depends_on=[str(d) for d in s.get("depends_on") or []],
        )
        for s in draft["steps"]
    ]
    criteria = [
        Criterion(
            id=str(c["id"]), type=c["type"], description=str(c["description"]), target=str(c.get("target") or ""),
            expected_exit_code=int(c.get("expected_exit_code", 0) or 0), timeout_s=float(c.get("timeout_s", 30.0) or 30.0),
        )
        for c in draft["acceptance_criteria"]
    ]
    plan = Plan(
        plan_id=plan_id or f"plan-{uuid.uuid4().hex[:10]}",
        goal=str(draft["goal"]),
        acceptance_criteria=criteria,
        steps=steps,
        budget_envelope={"max_usd": float(max_usd), "max_tokens": max_tokens},
        verifier_commands=[str(v) for v in draft.get("verifier_commands") or []],
        assumptions=[str(a) for a in draft.get("assumptions") or []],
        version=version,
    )
    jsonschema.validate(plan.to_json(), PLAN_SCHEMA)
    return plan

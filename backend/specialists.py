"""Specialist sub-agents: Architect and Debugger, composed under the
orchestrator (the session's main agent) with Strands' native agents-as-tools.

* **architect** — reads the code (files, LSP) and returns a change plan: files,
  steps, risks, how to verify. Read-only tools; strong model tier.
* **debugger** — given a failure (command, exit code, stderr/test output) finds
  the root cause with read-only tools and LSP diagnostics, then fixes it:
  ``apply_patch`` and re-running the failing command go through the session's
  approval gate exactly like the orchestrator's own calls.

Why ``Agent.as_tool()`` and not a hand-rolled ``@tool`` wrapper: the adapter
propagates a specialist's interrupts to the parent and forwards the parent's
resume back (``_harness_loop_smoke_test.py`` proves it on this SDK), so the one
``_ApprovalHandler`` instance the session uses — shared by reference — gates
the specialists too, mode changes included. Before this, sub-agents could only
get tools with no human gate at all (``sub_agent_common.DENIED_TOOLS``).

Specialists start fresh on every call (``preserve_context=False``): whatever
context they need is in the input the orchestrator — or the self-healing loop —
writes for them.

``diagnose`` is the Debugger in *proposal* mode for the self-healing loop:
read-only tools, returns a root cause and a unified diff that has already been
dry-run against the working tree, never touching disk itself.
"""

from __future__ import annotations

import os
import re
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

from strands import Agent
from strands_tools import file_read, shell, think

import harness_events
import lsp_bridge
import omlx_provider
import patch_tool

ARCHITECT_PROMPT = """You are the Architect specialist in a coding harness. You plan changes; you never edit files.
Work: read the relevant code (file_read, lsp_symbols, lsp_definition, lsp_references), then answer with
1. Goal — one sentence.
2. Files — each file to change and why.
3. Steps — ordered, concrete.
4. Risks — what could break.
5. Verify — the exact commands that prove it works (tests, build, lint).
Be specific to this repository; cite paths and symbols you actually read."""

DEBUGGER_PROMPT = """You are the Debugger specialist in a coding harness. You fix failures.
Work: read the failure, inspect the code it points at (file_read, lsp_diagnostics, lsp_definition), find the root
cause, then fix it with apply_patch (a unified diff) and re-run the failing command with shell to verify.
Change as little as possible; never weaken or delete a test to make it pass.
End with: ROOT CAUSE: <one line>  FIX: <what you changed>  VERIFIED: <command and result>."""

DIAGNOSE_PROMPT = """You are the Debugger specialist in a coding harness's self-healing loop.
You receive a failed command and its output. Investigate with your read-only tools (file_read, lsp_diagnostics,
lsp_definition, lsp_references), then answer in exactly this format:

ROOT CAUSE: <one line>
PATCH:
```diff
<a unified diff (---/+++ headers with paths relative to the project root, @@ hunks with enough context)>
```
VERIFY: <the single command that proves the fix; usually the command that failed>

Rules: fix the code under test, never weaken or delete tests; smallest change that fixes the root cause; if the
failure is environmental (missing tool, network, permissions) and no code change can fix it, answer
ROOT CAUSE: <cause>  and  PATCH: none."""


@dataclass(frozen=True)
class SpecialistSpec:
    name: str
    description: str
    prompt: str
    tier: str | None


ARCHITECT = SpecialistSpec(
    "architect",
    "Architect specialist: reads the code and returns a concrete change plan (files, steps, risks, verification). "
    "Use before multi-file or design-level changes. Input: what should change and why.",
    ARCHITECT_PROMPT,
    "strong",
)
DEBUGGER = SpecialistSpec(
    "debugger",
    "Debugger specialist: given a failing command, test or error, finds the root cause, patches it and verifies. "
    "Input: the failing command and its full error output.",
    DEBUGGER_PROMPT,
    "standard",
)
SPECIALISTS = (ARCHITECT, DEBUGGER)

READ_ONLY_TOOLS = [file_read, think, *lsp_bridge.LSP_TOOLS]


def _model(tier: str | None, role: str | None = None):
    from sub_agent_common import build_sub_agent_model

    return build_sub_agent_model(tier, role=role)


def build_specialist(
    spec: SpecialistSpec,
    *,
    interventions: list | None = None,
    hooks: list | None = None,
    model: Any = None,
) -> Agent:
    tools: list = list(READ_ONLY_TOOLS)
    if spec is DEBUGGER:
        tools += [shell, patch_tool.apply_patch]
    chosen = model if model is not None else _model(spec.tier, spec.name)
    return Agent(
        name=spec.name,
        description=spec.description,
        model=chosen,
        system_prompt=f"{spec.prompt}\n\nProject directory: {os.getcwd()}",
        tools=tools,
        interventions=interventions,
        # A specialist on a Prism model loads it before it runs and unloads it after
        # (omlx_provider.PrismLeaseHooks); any other specialist gets nothing extra.
        hooks=[*(hooks or []), *omlx_provider.hooks_for(chosen)],
        callback_handler=None,
    )


def specialist_tools(
    *,
    interventions: list | None = None,
    hooks_factory: Callable[[], list] | None = None,
    model_factory: Callable[[SpecialistSpec], Any] | None = None,
) -> tuple[list, dict[str, Agent]]:
    """The specialists as tools for the orchestrator, plus the agents by name
    (the router dispatches to them directly)."""
    agents = {
        spec.name: build_specialist(
            spec,
            interventions=interventions,
            hooks=hooks_factory() if hooks_factory else [lsp_bridge.LspEditHooks()],
            model=model_factory(spec) if model_factory else None,
        )
        for spec in SPECIALISTS
    }
    return [agent.as_tool(name=name, description=agent.description) for name, agent in agents.items()], agents


# ------------------------------------------------------- self-heal proposals

_DIFF_FENCE = re.compile(r"```(?:diff|patch)?\s*\n(.*?)```", re.DOTALL)


@dataclass
class Diagnosis:
    root_cause: str
    patch: str | None
    verify: str | None
    raw: str
    patch_error: str | None = None

    @property
    def actionable(self) -> bool:
        return bool(self.patch) and self.patch_error is None


def parse_diagnosis(reply: str) -> Diagnosis:
    root = re.search(r"ROOT CAUSE:\s*(.+)", reply)
    verify = re.search(r"VERIFY:\s*`?([^`\n]+)`?", reply)
    patch = None
    if not re.search(r"PATCH:\s*none", reply, re.IGNORECASE):
        for fence in _DIFF_FENCE.finditer(reply):
            try:
                patch_tool.parse(fence.group(1))  # any format apply_patch understands
            except patch_tool.PatchError:
                continue
            patch = fence.group(1)
            break
    return Diagnosis(
        root_cause=(root.group(1).strip() if root else reply.strip().splitlines()[0] if reply.strip() else "unknown"),
        patch=patch,
        verify=verify.group(1).strip() if verify else None,
        raw=reply,
    )


def diagnose(
    failure: str,
    previous_attempts: list[str] | None = None,
    *,
    model: Any = None,
    root: str | None = None,
    session_id: str | None = None,
    max_rounds: int = 2,
) -> Diagnosis:
    """Ask the Debugger (read-only) for a root cause and a patch for ``failure``.

    The patch is dry-run (``patch_tool.plan``) against the working tree; if it
    doesn't apply, the Debugger gets the error and one more round. Specialists
    are stateless, so earlier failed attempts are passed in explicitly.
    """
    root = root or os.getcwd()
    task = harness_events.Task("self-heal: debugger diagnosing", session_id=session_id, source="heal")
    agent = Agent(
        name="debugger",
        model=model if model is not None else _model(DEBUGGER.tier, DEBUGGER.name),
        system_prompt=f"{DIAGNOSE_PROMPT}\n\nProject directory: {root}",
        tools=list(READ_ONLY_TOOLS),
        callback_handler=None,
    )
    prompt = f"Failure to fix:\n{failure}"
    if previous_attempts:
        prompt += "\n\nEarlier attempts that did NOT fix it (don't repeat them):\n" + "\n---\n".join(previous_attempts)
    diagnosis = Diagnosis("unknown", None, None, "")
    try:
        for _round in range(max_rounds):
            reply = str(agent(prompt)).strip()
            diagnosis = parse_diagnosis(reply)
            if not diagnosis.patch:
                break
            try:
                patch_tool.plan(diagnosis.patch, root)
                diagnosis.patch_error = None
                break
            except patch_tool.PatchError as error:
                diagnosis.patch_error = str(error)
                prompt = f"Your patch does not apply: {error}\nRe-read the file and answer again in the same format."
        task.finish("completed" if diagnosis.actionable else "failed", diagnosis.root_cause[:120])
    except Exception as error:
        task.finish("failed", str(error))
        diagnosis = Diagnosis(f"debugger failed: {type(error).__name__}: {error}", None, None, "")
    return diagnosis

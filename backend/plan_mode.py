"""Plan mode's way out: ``present_plan``.

In plan mode nothing runs, so the agent used to describe a plan in chat and the person had to notice, switch modes
by hand and say "go". ``present_plan`` makes the hand-over a real step, as Claude Code's ExitPlanMode does: the agent
calls it with the finished plan, the approval gate (``acp_approval``) puts the plan to the person as a card with
three answers, and the *person's* answer — never the agent's — decides what the session becomes:

* **approve → build**   tools run after the usual approvals (the default mode);
* **approve → edit**    file edits run without asking, commands still ask;
* **approve → auto**    routine calls run without asking (the person chose it, so this is not a self-grant);
* **keep planning**     the plan is not approved; the agent is told to revise it (with the person's note, if any).

The tool itself runs *after* the answer: ``acp_server`` records the answer in the agent's state
(``record``) and the tool reads it back, so what the model sees as the result matches what the person chose.
Outside plan mode the gate refuses the call, so the tool can never be used to change a mode on the agent's say-so.
"""

from __future__ import annotations

from typing import Any

import acp
from strands import tool
from strands.types.tools import ToolContext

TOOL_NAME = "present_plan"
STATE_KEY = "cordata.plan_decision"

OPTION_BUILD = "approve_build"
OPTION_EDIT = "approve_edit"
OPTION_AUTO = "approve_auto"
OPTION_REVISE = "keep_planning"

#: The mode each approving answer puts the session in.
MODE_FOR_OPTION = {OPTION_BUILD: "build", OPTION_EDIT: "edit", OPTION_AUTO: "auto"}


def options() -> list[Any]:
    """The four answers. Their *kinds* matter to a client that only knows ACP's four — Zed, the GUI's modal, the Discord bot
    map "Allow" to the first allow-once option and "Deny" to the reject one: so the first is the plain approval (build), the
    rest are allow-once as well (a remote door cannot reach edit or auto by a stray "Always allow" click; the TUI picks them
    by id), and the refusal is keep planning."""
    return [
        acp.schema.PermissionOption(option_id=OPTION_BUILD, name="Approve — build (ask before changes)", kind="allow_once"),
        acp.schema.PermissionOption(option_id=OPTION_EDIT, name="Approve — edit (file edits run, commands ask)", kind="allow_once"),
        acp.schema.PermissionOption(option_id=OPTION_AUTO, name="Approve — auto (routine calls run)", kind="allow_once"),
        acp.schema.PermissionOption(option_id=OPTION_REVISE, name="Keep planning", kind="reject_once"),
    ]


def answered(agent: Any) -> bool:
    """Whether the person's answer to this plan card is already recorded: the gate sees the call a second time when the
    paused call resumes, by which time an approval has changed the mode."""
    try:
        return agent.state.get(STATE_KEY) is not None
    except Exception:
        return False


def record(agent: Any, option_id: str | None, note: str = "") -> str | None:
    """Remember what the person answered, for the tool to report. Returns the mode the session should switch to, or
    None when the plan was not approved."""
    mode = MODE_FOR_OPTION.get(option_id or "")
    try:
        agent.state.set(STATE_KEY, {"mode": mode, "note": (note or "").strip()[:500]})
    except Exception:  # an agent without state is a test double; the answer still switches the mode
        pass
    return mode


@tool(context=True, name=TOOL_NAME)
def present_plan(
    tasks: str = "",
    guardrails: str = "",
    done: str = "",
    plan: str = "",
    tool_context: ToolContext = None,
) -> str:
    """Hand the execution contract to the person for approval before making any changes.
    Instead of loose planning, the contract is explicitly structured in three parts:
      1. Describe the task(s): Specific scope, files to edit/create, and step-by-step implementation.
      2. Describe the Guardrails that surround the task(s): What invariants, safety limits,
         security/credential rules, or boundaries MUST NOT be violated (e.g. no secret leaks, preserve API shapes).
      3. Describe what Done looks like: Exact verification command(s), test criteria, and measurable proof.

    Args:
        tasks: Detailed description of the task(s) and steps.
        guardrails: Safety guardrails, invariants, and constraints surrounding the task(s).
        done: Concrete definition of Done (exact test/build commands, expected output, verification criteria).
        plan: Markdown execution contract (legacy or single-string fallback).
    """
    agent = tool_context.agent
    decision: dict[str, Any] = {}
    try:
        decision = agent.state.get(STATE_KEY) or {}
        agent.state.delete(STATE_KEY)
    except Exception:
        decision = {}
    mode = decision.get("mode")
    note = str(decision.get("note") or "").strip()
    if mode in ("build", "edit", "auto"):
        extra = f" The person added: {note}" if note else ""
        return (
            f"Plan approved. The session is now in {mode} mode"
            + {"auto": ": routine calls run without asking.", "edit": ": file edits run without asking, commands still ask."}.get(mode, ": tools run after the usual approvals.")
            + f" Carry the plan out step by step and check each step as you go.{extra}"
        )
    if note:
        return f"The plan was not approved. The person wrote: {note}\nRevise the tasks, guardrails, and definition of done, then call present_plan again."
    return "The plan was not approved yet. Ask what to change, or revise it from what you know, then call present_plan again."

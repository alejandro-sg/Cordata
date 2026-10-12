"""``ask_user``: a multiple-choice question to the person, answered on a card.

The agent used to ask in prose and guess at what the reply meant. This puts the question where the person can answer
it in one key: the approval gate (``acp_approval``) puts the call to the client as a permission request whose options
*are* the choices, the person picks one (or types their own answer, or says "decide for me"), and the tool returns what
they chose. It works in every mode, plan included — a question changes nothing — and it never runs a tool for the
asker: the only thing an answer does is come back as the tool's result.
"""

from __future__ import annotations

from typing import Any

import acp
from strands import tool
from strands.types.tools import ToolContext

TOOL_NAME = "ask_user"
STATE_KEY = "cordata.ask_answer"
OPTION_SKIP = "skip"
MAX_OPTIONS = 6
MAX_LEN = 120


def clean_options(tool_input: Any) -> list[str]:
    raw = tool_input.get("options") if isinstance(tool_input, dict) else None
    items = [str(item).strip()[:MAX_LEN] for item in raw if str(item).strip()] if isinstance(raw, list) else []
    seen: list[str] = []
    for item in items:
        if item not in seen:
            seen.append(item)
    return seen[:MAX_OPTIONS]


def option_id(index: int) -> str:
    return f"opt{index + 1}"


OPTION_DECLINE = "decline"


def options(tool_input: Any) -> list[Any]:
    """The choices as ACP permission options, then "decide for me".

    The choices carry the kind ``reject_always`` on purpose: a front door that only knows ACP's four kinds (the Discord bot's
    Allow / Deny, the GUI's modal) maps "Allow" to the first allow-once option and "Deny" to the reject-once one, and must
    never turn a stray click into an *answer*. So the only allow-once option is "decide for me", the only reject-once option
    is "skip the question", and the numbered choices are reachable by id only — by a client that knows what a question is."""
    out = [acp.schema.PermissionOption(option_id=option_id(i), name=text, kind="reject_always") for i, text in enumerate(clean_options(tool_input))]
    out.append(acp.schema.PermissionOption(option_id=OPTION_SKIP, name="Decide for me", kind="allow_once"))
    out.append(acp.schema.PermissionOption(option_id=OPTION_DECLINE, name="Skip the question", kind="reject_once"))
    return out


def answered(agent: Any) -> bool:
    try:
        return agent.state.get(STATE_KEY) is not None
    except Exception:
        return False


def record(agent: Any, tool_input: Any, chosen: str | None, note: str = "") -> None:
    """Remember the answer for the tool to report: the text of the option chosen, or the person's own words."""
    choices = clean_options(tool_input)
    picked = None
    if chosen and chosen.startswith("opt") and chosen[3:].isdigit() and 1 <= int(chosen[3:]) <= len(choices):
        picked = choices[int(chosen[3:]) - 1]
    try:
        agent.state.set(STATE_KEY, {"choice": picked, "note": (note or "").strip()[:500], "skipped": picked is None and not (note or "").strip()})
    except Exception:
        pass


@tool(context=True, name=TOOL_NAME)
def ask_user(question: str, options: list[str], tool_context: ToolContext) -> str:
    """Ask the person a multiple-choice question and wait for the answer. Use it when the task turns on a decision only
    they can make — which of two designs, which file, whether to keep the old behaviour — and the code cannot tell you.
    Do not use it for things you can find out by reading, or to ask permission for a tool (those ask on their own).

    Args:
        question: One clear question, in a sentence.
        options: Two to six short, distinct answers, most likely first. The person can also type their own or let you decide.
    """
    agent = tool_context.agent
    answer: dict[str, Any] = {}
    try:
        answer = agent.state.get(STATE_KEY) or {}
        agent.state.delete(STATE_KEY)
    except Exception:
        answer = {}
    if answer.get("choice"):
        return f"The person chose: {answer['choice']}"
    if answer.get("note"):
        return f"The person answered in their own words: {answer['note']}"
    return "The person left it to you: decide, and say in your reply which assumption you made."

"""Laya's confident ``team`` route, executed as a harness-level Strands
GraphBuilder pipeline. Before this, a multi-part turn ran through the
orchestrator's own model call, which had to decide both *that* it wanted a
team and *how* to decompose the task into local_graph/local_swarm specs —
two chances for the session's main model to wander. When Laya routes a turn
to ``team`` with ``laya.teamDispatch = direct`` (default), the ACP server
skips that and runs two bounded steps directly:

1. **Decompose** — one ``plan_team`` call: a small planner Agent (``standard``
   tier, no tools) turns the user's task into a strict JSON team spec, each
   node carrying name, role, task, optional tool names, model tier and
   dependencies. This cannot write, run or call anything.
2. **Execute** — the exact same machinery the ``local_graph`` tool uses:
   ``sub_agent_common.build_agent_nodes`` + ``strands.multiagent.Graph``.
   ``local_graph_tool.stream_graph`` is imported rather than copied, so the
   pipeline and the tool can't drift.

Safety, unchanged by skipping the orchestrator: every node's tools go through
the same denylist as local_graph/local_swarm (``sub_agent_common.resolve_tools``
— no shell, python_repl, file_write, editor, generated skills, orchestrator-
only or frontend-proxy tools), and the same UntrustedToolResultHooks /
SecretRedactionHooks. Only the *decision* to delegate is automated; nothing a
sub-agent could do here can pass the human gate that local_graph's own runs
never had either.

Failures fail *open*: an unusable plan (bad JSON, too few nodes, a dependency
name that doesn't exist, a planner error) returns None from ``plan_team`` and
the turn falls back to the advisory hint — the orchestrator's model sees the
turn as an ordinary hint, exactly as ``laya.teamDispatch = hint`` does. The
pipeline must never swallow a turn or hide a routing failure from the user.
"""

from __future__ import annotations

import asyncio
import json
import logging
import re
from collections.abc import AsyncIterator
from typing import Any

from strands import Agent

from local_graph_tool import stream_graph
from sub_agent_common import build_sub_agent_model

logger = logging.getLogger(__name__)

PLANNER_PROMPT = """You are the planner of a coding harness's team pipeline. The user's request is below.
Decide whether it is genuinely a multi-part task that a *team of sub-agents* should run as parallel
Strands Graph nodes, and if so decompose it.

Sub-agents are read-only researchers/planners/analysts: they can read files and search code, but they
CANNOT run shell commands, edit files, or install anything. Split along genuinely independent parts
(each needs no other's output to start); if a part must consume another's output, give it the other's
name in depends_on. Prefer 2-5 narrow agents. Do NOT invent work: if the request is a single coherent
task, answer exactly {"agents": false}.

Answer with ONLY a JSON object, no prose, no fences:
{"task": "one sentence for the whole team",
 "agents": [{"name": "snake_case", "role": "one line", "task": "exact self-contained task for this node",
             "tools": ["optional tool names: file_read, http_request, http_fetch, agent_memory"],
             "tier": "fast"|"standard"|"strong" (optional),
             "profile": "architect"|"digest" (optional: architect = design/planning, digest = condensing bulk
                        context into a short summary; both run on a local model, so use them for that work),
             "depends_on": ["names this node needs first", ...] (optional)}]}

Request:
"""


def parse_team_plan(reply: str) -> dict[str, Any] | None:
    """Extracts and validates the planner's JSON. Returns None (never raises)
    for anything not a usable spec — the caller must fail open."""
    text = (reply or "").strip()
    match = re.search(r"\{.*\}", text, re.DOTALL)
    if not match:
        return None
    try:
        plan = json.loads(match.group(0))
    except json.JSONDecodeError:
        return None
    agents = plan.get("agents")
    if agents is False or agents is None:
        return None  # the planner judged it not a team task — a valid answer
    if not isinstance(agents, list) or len(agents) < 2:
        return None  # one node is not a team: local_graph/local_swarm's own floor is >= 1, but "team" means several
    seen: set[str] = set()
    sanitized: list[dict[str, Any]] = []
    for i, entry in enumerate(agents):
        if not isinstance(entry, dict):
            return None
        name = re.sub(r"[^a-z0-9_]", "_", str(entry.get("name") or f"node_{i + 1}").lower()).strip("_")
        deps = entry.get("depends_on")
        if not name or not str(entry.get("task") or "").strip() or (deps is not None and not isinstance(deps, list)):
            return None
        sanitized.append({**entry, "name": name})
        seen.add(name)
    # dependency names must survive the same sanitization the nodes get — the
    # planner's "Code Review!" refers to the node it just named, whatever it
    # called it
    for entry in sanitized:
        for dependency in (entry.get("depends_on") or []):
            dependency = re.sub(r"[^a-z0-9_]", "_", str(dependency).lower()).strip("_")
            if dependency not in seen:
                return None  # a dangling dependency would silently drop the edge in stream_graph's zip
    return {"task": str(plan.get("task") or "").strip(), "agents": sanitized}


def plan_team(task: str, caller: Any, context: str = "", model: Any = None) -> dict[str, Any] | None:
    """One planner model call → a validated spec dict for ``stream_team``.
    Synchronous; the ACP server runs it via asyncio.to_thread. Returns None
    on any failure (fail open) — never let planning break a turn."""
    planner = Agent(
        name="team-planner",
        model=model if model is not None else build_sub_agent_model("standard"),
        system_prompt=PLANNER_PROMPT,
        tools=[],  # decomposes text only — nothing to call, nothing to gate
        callback_handler=None,
    )
    prompt = task if not context else f"{task}\n\nConversation so far (for context):\n{context}"
    try:
        reply = str(planner(prompt)).strip()
    except Exception as error:
        logger.warning("team pipeline: planner failed: %s: %s", type(error).__name__, error)
        return None
    plan = parse_team_plan(reply)
    if plan is None and reply:
        logger.info("team pipeline: unusable plan (%.80s)", reply.replace("\n", " "))
    return plan


def node_specs(plan: dict[str, Any]) -> list[dict[str, Any]]:
    """The planner's JSON entries → local_graph/stream_graph agent specs:
    each node's system_prompt is role + task (the graph hands every node the
    same overall ``task``, so the per-node task must ride in the prompt)."""
    specs = []
    for entry in plan["agents"]:
        role = str(entry.get("role") or "").strip()
        task_text = str(entry.get("task") or "").strip()
        system_prompt = f"You are {role} on a specialist team. Your part of the work, precisely: {task_text}\n" if role else f"Your part of the work, precisely: {task_text}\n"
        spec = {
            "name": str(entry.get("name") or "").strip(),
            "system_prompt": system_prompt,
            "task_of_node": task_text,  # for debugging/progress display only
        }
        if entry.get("tools"):
            spec["tools"] = list(entry["tools"])
        if entry.get("tier"):
            spec["tier"] = entry["tier"]
        if entry.get("profile") in ("architect", "digest"):
            spec["profile"] = entry["profile"]
        if entry.get("depends_on"):
            spec["depends_on"] = list(entry["depends_on"])
        specs.append(spec)
    return specs


async def stream_team(task: str, plan: dict[str, Any], caller: Any) -> AsyncIterator[dict[str, Any]]:
    """Yields stream_graph's progress states, then its result dict."""
    specs = node_specs(plan)
    async for value in stream_graph(task, specs, caller, label="team_pipeline"):
        yield value


async def plan_team_async(task: str, caller: Any, context: str = "", model: Any = None) -> dict[str, Any] | None:
    return await asyncio.to_thread(plan_team, task, caller, context, model)
"""``subagent`` — hand a self-contained subtask to a fresh agent and get back only its answer.

Built on strands-harness's ``make_subagent`` (``strands_harness.tools.subagent``), not on ``create_harness``: the
session's agent stays the one ``acp_server`` builds, and only the delegation tool comes from the SDK. It is the one way
the orchestrator delegates in-process:

* **Roles the model picks** (``agent_type``): ``generalist`` — any focused subtask, with the full kit below;
  ``explorer`` — read-only search across many files when only the conclusion is wanted; and the two specialists from
  ``specialists.SPECIALISTS`` — ``architect`` (read-only, returns a change plan, strong tier) and ``debugger`` (finds a
  failure's root cause, patches it and re-runs the command, standard tier). The specialists' prompts, tiers and kits
  are defined once, in ``specialists.py``; the Laya router's direct dispatch and the self-heal ``diagnose`` still use
  those agents, the orchestrator reaches them through this tool.
* **Tools the model narrows** (``tools``): a subset of the kit, re-checked at call time, so a child can never hold a
  tool the kit does not offer. The kit is read-only code tools plus ``shell`` and ``apply_patch``; the explorer and
  architect stay read-only whatever is asked for.
* **A tier the model picks** (``model``): ``fast`` / ``standard`` / ``strong`` on the active provider
  (``sub_agent_common.build_sub_agent_model``, with the role, so a role's Ollama Cloud profile or local Prism model
  applies); omitted, the role's own tier.
* **Context the model shares** (``context``): ``none`` (a blank conversation, the default) or ``no_tools`` (this
  conversation's text turns, tool calls and results left out). ``all`` is not offered: it would copy tool output —
  untrusted text — into a second agent's history.

**Watching it.** Every call is a pod of its own in the Lab (``lab.Lab.spawn``: ``debugger-3``, its task, a delegation
link from the orchestrator), kept current by ``LabHooks`` and streamed live by ``LabStream`` — what the child writes and
what it reasons, as it is generated. The tool card in the transcript shows the same thing in the team-progress format
(``acp_protocol._SpecialistProgress``): started, the tools it called, the tail of its reasoning, finished.

Safety, the same as before for the Debugger: every child is built with the session's ``_ApprovalHandler`` as its
intervention, so its ``shell`` and ``apply_patch`` ask (or are refused in plan mode) exactly like the orchestrator's own
calls; the SDK tool propagates the child's interrupt to the parent and resumes the same child on the answer. A child
gets no ``subagent`` tool of its own (and ``max_depth=1`` besides), no ``orchestrate``, no Claude Code CLI, no egress
tool. Its result is model-written text and is fenced as delegated output for the agent that reads it
(``prompt_security``).
"""

from __future__ import annotations

import os
import weakref
from collections.abc import Callable
from typing import Any

from strands import Agent

import omlx_provider
import patch_tool
from core_tools import CoreToolHooks, HarnessSandbox, shell

TOOL_NAME = "subagent"

GENERALIST_PROMPT = (
    "You are a general-purpose subagent in a coding harness, handling one focused subtask for the session's main "
    "agent. You run in your own fresh conversation and cannot ask follow-up questions: work from the task as given, "
    "make reasonable assumptions where it is underspecified, and see it through to a verified result. Find before "
    "reading (code_search, code_outline, the LSP tools); read only the parts you need. Change files only with "
    "apply_patch, as little as the task needs; never weaken or delete a test. Your final message is the only thing "
    "that returns, so put the substance there: what you did, what you found (with paths and symbols), what you "
    "verified and how, and anything the main agent must act on."
)

EXPLORER_PROMPT = (
    "You are a read-only explorer subagent in a coding harness. You answer one question about the codebase for the "
    "session's main agent, which wants the conclusion, not the search. Use code_search, code_outline and the LSP "
    "tools to locate, and file_read with a line range to confirm. You never edit files or run commands. Answer with "
    "the conclusion first, then the evidence as path:line references with a few words each. Say plainly what you "
    "could not find."
)

WRITE_TOOLS = frozenset({"shell", "apply_patch"})
#: Roles that never get a write tool, whatever the call asks for (and so may run in plan mode — acp_approval).
READ_ONLY_ROLES = frozenset({"explorer", "architect"})
ROLES = ("generalist", "explorer", "architect", "debugger")


#: Strands' background execution, for this tool only: the model may set ``_background_execution`` on a `subagent`
#: call and keep working while the child runs; every other tool stays in the foreground (no ``*`` wildcard). The
#: SDK's own wait-for-completion is off — see ``hold_turn_for_background``.
BACKGROUND_TASKS = {"agentic": [TOOL_NAME], "wait_for_completion": False, "max_concurrency": 3}


def hold_turn_for_background(agent: Any) -> None:
    """Keep a turn open until its background `subagent` calls have settled, so their results reach the model in the
    same turn, never as a surprise in the next one.

    Why not the SDK's ``wait_for_completion=True``: it waits at the end of *every* invocation, and with
    ``checkpointing=True`` each pause between tool rounds is an invocation of its own (acp_server resumes it at once)
    — a background child would block the very next pause, which is foreground with extra steps. This waits only when
    the invocation is the turn's real end (not a checkpoint pause, not an interrupt, not a cancel); the SDK's hook,
    which runs after this one, then delivers what settled (or re-invokes to surface a child's approval prompt).
    Reads the SDK's background plugin internals (pinned strands-agents 1.57.2, ``_subagent_tool_smoke_test.py``)."""
    from strands.hooks import AfterInvocationEvent
    from strands.background_tasks._types import is_task_status_terminal

    async def wait(event: AfterInvocationEvent) -> None:
        plugin = getattr(agent, "_background_tasks", None)
        if plugin is None or agent._interrupt_state.activated:
            return
        if event.result is not None and event.result.stop_reason in ("checkpoint", "interrupt", "cancelled"):
            return
        tasks = plugin._task_snapshots()
        while (
            not agent.cancel_signal.is_set()
            and not any(task["status"] == "input_required" for task in tasks)
            and any(not is_task_status_terminal(task["status"]) for task in tasks)
        ):
            await plugin._await_next_settlement(tasks)
            tasks = plugin._task_snapshots()

    # Registered after the SDK's plugin, so it runs first (AfterInvocationEvent callbacks run in reverse).
    agent.add_hook(wait, AfterInvocationEvent)


def _kit() -> dict[str, Any]:
    """The tools a child may be given, by tool name: the specialists' read-only set, plus shell and apply_patch."""
    from specialists import READ_ONLY_TOOLS

    tools = [*READ_ONLY_TOOLS, shell, patch_tool.apply_patch]
    return {_name(tool): tool for tool in tools}


def _name(tool: Any) -> str:
    name = getattr(tool, "tool_name", None)
    if name:
        return name
    spec = getattr(tool, "TOOL_SPEC", None)  # a strands_tools module (file_read)
    if isinstance(spec, dict) and spec.get("name"):
        return spec["name"]
    raise ValueError(f"cannot name tool {tool!r}")


def _lab() -> Any:
    import lab as lab_module

    return lab_module


def subagent_tool(
    *,
    interventions: list | None = None,
    hooks_factory: Callable[[], list] | None = None,
    model_factory: Callable[..., Any] | None = None,
    parent_pod: str | None = "orchestrator",
) -> Any:
    """The ``subagent`` tool for the session's agent. ``interventions`` is the session's approval gate (shared by
    reference); ``hooks_factory`` makes each child's hooks; ``model_factory(tier, role=...)`` its model (tests inject
    one); ``parent_pod`` is the Lab pod each child's delegation link starts from."""
    from strands_harness.tools.subagent import Choice, Fixed, Option, Preset, make_subagent

    from specialists import ARCHITECT, DEBUGGER

    kit = _kit()
    read_only = [name for name in kit if name not in WRITE_TOOLS]
    debugger_kit = [*read_only, "shell", "apply_patch"]
    presets = {
        "generalist": Preset(
            instructions=GENERALIST_PROMPT,
            description="a general-purpose agent for a focused subtask (may run commands and patch files, gated)",
        ),
        "explorer": Preset(
            instructions=EXPLORER_PROMPT,
            tools=read_only,
            model="fast",
            description="read-only search across many files; returns the conclusion with path:line evidence",
        ),
        "architect": Preset(
            instructions=ARCHITECT.prompt,
            tools=read_only,
            model=ARCHITECT.tier,
            description="reads the code and returns a concrete change plan (files, steps, risks, verification); use "
            "before multi-file or design-level changes; read-only",
        ),
        "debugger": Preset(
            instructions=DEBUGGER.prompt,
            tools=debugger_kit,
            model=DEBUGGER.tier,
            description="given a failing command, test or error (put the command and its full output in the task), "
            "finds the root cause, patches it and re-runs the command to verify",
        ),
    }

    def model_for(tier: str | None, role: str) -> Any:
        if model_factory is not None:
            return model_factory(tier, role=role)
        import specialists

        return specialists._model(tier, role)  # sub_agent_common.build_sub_agent_model, the specialists' own path

    def builder(spec: Any) -> Agent:
        role = spec.agent_type or "generalist"
        names = spec.tools if spec.tools is not None else list(kit)
        if role in READ_ONLY_ROLES:
            # A model-supplied `tools` wins over the preset's in the SDK; these roles stay read-only regardless.
            names = [name for name in names if name not in WRITE_TOOLS]
        tools = [kit[name] for name in names if name in kit]
        chosen = model_for(spec.model if isinstance(spec.model, str) else None, role)
        lab_module = _lab()
        lab = lab_module.get()
        pod = lab.spawn(role, role=f"subagent:{role}", parent=parent_pod, task=spec.task)
        sandbox = HarnessSandbox()
        agent = Agent(
            name=pod.name,
            model=chosen,
            sandbox=sandbox,
            system_prompt=f"{spec.instructions or GENERALIST_PROMPT}\n\nProject directory: {os.getcwd()}",
            tools=tools,
            interventions=interventions,
            hooks=[
                *([CoreToolHooks()] if "shell" in names else []),
                *(hooks_factory() if hooks_factory else []),
                *omlx_provider.hooks_for(chosen),
                lab_module.LabHooks(lab, pod),
            ],
            # What the child writes and reasons reaches its pod as it is generated, not only once a call is done.
            callback_handler=lab_module.LabStream(lab, pod.name),
        )
        pod._agent = weakref.ref(agent)
        pod.system_prompt = agent.system_prompt or ""
        sandbox.bind(agent)
        return agent

    return make_subagent(
        builder=builder,
        presets=presets,
        default_preset="generalist",
        instructions=Fixed(None),
        tools=Choice(list(kit), multiple=True),
        mcp_servers=Fixed([]),
        model=Choice(
            [
                Option("fast", description="cheapest; lookups and mechanical work"),
                Option("standard", description="most subtasks"),
                Option("strong", description="hard reasoning or design"),
            ]
        ),
        context=Choice(["none", "no_tools"]),
        max_depth=1,
        name=TOOL_NAME,
    )

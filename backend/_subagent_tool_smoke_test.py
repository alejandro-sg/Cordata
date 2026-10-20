"""The ``subagent`` tool (subagent_tool.py) on the installed strands-harness, with ScriptedModels playing both the
session's agent and the child — no network, no model server.

1. Schema: four roles (generalist, explorer, and the architect/debugger specialists), tiers, context modes; ``all``
   context is not offered; no ``subagent`` in a child's kit.
2. Gate: a child's ``shell`` call is held by the session's intervention, the interrupt reaches the parent, and the
   parent's resume runs the call inside the same child.
3. Roles: explorer and architect are read-only even when ``shell`` is asked for (and so may run in plan mode); the
   debugger gets shell and apply_patch; each role gets its own tier and its role name for the model factory.
4. Watching: every call is a Lab pod of its own — numbered, its task, a delegation link from the orchestrator — and
   its reasoning streams into the pod as it is generated; the tool card's progress uses the team format.
5. Background: with checkpoints on (as the ACP session runs), a backgrounded call does not block the next pause, the
   parent keeps working, and the turn still ends with the child's answer delivered.
6. Containment: ``subagent`` is orchestrator-only for every other sub-agent too, and its result is fenced.

    backend/.venv/bin/python _subagent_tool_smoke_test.py
"""

import asyncio

from strands import Agent
from strands.interventions import Confirm, InterventionHandler, Proceed

import acp_approval
import lab as lab_module
import sub_agent_common
import subagent_tool
from acp_protocol import _SpecialistProgress
from harness_testing import ScriptedModel, check, text, tool as call


class AskForShell(InterventionHandler):
    name = "ask-for-shell"

    def __init__(self) -> None:
        self.asked: list[str] = []

    def before_tool_call(self, event):
        if event.tool_use.get("name") == "shell":
            self.asked.append(str(event.tool_use.get("input")))
            return Confirm(prompt="run?")
        return Proceed()


class Children:
    """A model factory that hands each new child the next script and remembers the tier and role it was asked for."""

    def __init__(self, *scripts: list) -> None:
        self.scripts = list(scripts)
        self.asked: list[tuple[str | None, str]] = []
        self.models: list[ScriptedModel] = []

    def __call__(self, tier: str | None, *, role: str) -> ScriptedModel:
        self.asked.append((tier, role))
        model = ScriptedModel(self.scripts.pop(0))
        self.models.append(model)
        return model


def parent_with(delegate, steps, **kwargs) -> Agent:
    return Agent(model=ScriptedModel(steps), tools=[delegate], callback_handler=None, **kwargs)


def schema() -> None:
    spec = subagent_tool.subagent_tool(model_factory=Children()).tool_spec
    props = spec["inputSchema"]["json"]["properties"]
    check(spec["name"] == "subagent", "the tool is named subagent")
    check(props["agent_type"]["enum"] == ["generalist", "explorer", "architect", "debugger"], "four roles, the specialists among them")
    check("change plan" in spec["description"] and "root cause" in spec["description"], "…each role described for the model")
    check(props["model"]["enum"] == ["fast", "standard", "strong"], "the model picks a tier")
    check(props["context"]["enum"] == ["none", "no_tools"], "context is none or no_tools — never all")
    check({"shell", "apply_patch", "file_read", "code_search"} <= set(props["tools"]["items"]["enum"]), "the kit is offered as a subset choice")
    check("subagent" not in props["tools"]["items"]["enum"], "a child cannot be handed subagent")
    check("mcp_servers" not in props and "instructions" not in props, "no MCP servers and no ad-hoc system prompt")


def gate() -> None:
    gate = AskForShell()
    children = Children([call("shell", {"command": "echo subagent-ok"}), text("ran it")])
    parent = parent_with(
        subagent_tool.subagent_tool(interventions=[gate], model_factory=children),
        [call("subagent", {"task": "run the check", "model": "strong"}), text("parent done")],
    )
    result = parent("delegate it")
    check(result.stop_reason == "interrupt", "the child's shell call pauses the parent")
    check(len(gate.asked) == 1 and "echo subagent-ok" in gate.asked[0], "…through the session's own intervention")
    result = parent([{"interruptResponse": {"interruptId": result.interrupts[0].id, "response": True}}])
    check(str(result).strip() == "parent done", "the parent resumes after approval")
    check(len(children.models) == 1, "the resume reached the same child — no second one was built")
    check("subagent-ok" in (children.models[0].calls[-1]["last_text"] or ""), "the approved command ran inside the child")
    check(children.asked == [("strong", "generalist")], "the tier the model picked reached the child's model factory")
    results = [b["toolResult"] for m in parent.messages for b in m["content"] if "toolResult" in b]
    check(results and "ran it" in results[-1]["content"][0]["text"], "only the child's final answer comes back")


def roles() -> None:
    for role, asks in (("explorer", ["shell", "file_read"]), ("architect", ["apply_patch", "code_search"])):
        children = Children([text("found it at a.py:3")])
        parent_with(subagent_tool.subagent_tool(model_factory=children), [call("subagent", {"task": "where is X", "agent_type": role, "tools": asks}), text("ok")])("go")
        got = set(children.models[0].calls[0]["tools"])
        check(not got & subagent_tool.WRITE_TOOLS and got, f"{role} is read-only even when a write tool is asked for (got {sorted(got)})")
        check(acp_approval._read_only_call("subagent", {"agent_type": role, "task": "x"}), f"…so {role} runs in plan mode")
    check(not acp_approval._read_only_call("subagent", {"task": "x"}) and not acp_approval._read_only_call("subagent", {"task": "x", "agent_type": "debugger"}), "a generalist or debugger does not")

    children = Children([text("plan")], [text("fixed")], [text("done")])
    delegate = subagent_tool.subagent_tool(model_factory=children)
    parent_with(delegate, [call("subagent", {"task": "plan it", "agent_type": "architect"}), text("ok")])("a")
    parent_with(delegate, [call("subagent", {"task": "fix pytest", "agent_type": "debugger"}), text("ok")])("b")
    parent_with(delegate, [call("subagent", {"task": "anything"}), text("ok")])("c")
    check(children.asked == [("strong", "architect"), ("standard", "debugger"), (None, "generalist")], "each role on its own tier, under its own role name")
    debugger_tools = set(children.models[1].calls[0]["tools"])
    check({"shell", "apply_patch", "code_search"} <= debugger_tools, "the debugger can patch and re-run")
    check("ROOT CAUSE" in (children.models[1].calls[0]["system_prompt"] or ""), "…under the Debugger's own prompt (specialists.py)")
    check({"shell", "apply_patch", "file_read"} <= set(children.models[2].calls[0]["tools"]), "a generalist gets the whole kit")


def watching() -> None:
    lab = lab_module.get()
    lab.reset()
    children = Children([text("the import path is wrong", thinks="the test imports calc from the root so run it there")])
    parent_with(subagent_tool.subagent_tool(model_factory=children), [call("subagent", {"task": "why does pytest fail", "agent_type": "debugger"}), text("ok")])("go")
    pods = [p for p in lab.pods.values() if p.ephemeral]
    check(len(pods) == 1 and pods[0].name.startswith("debugger-") and pods[0].role == "subagent:debugger", "the call is a pod of its own, numbered, with its role")
    pod = pods[0]
    check(pod.task == "why does pytest fail" and pod.parent == "orchestrator", "…its task and its parent recorded")
    check("run it there" in pod.reasoning, f"…its reasoning streamed into the pod live ({pod.reasoning!r})")
    check(pod.stream and "import path" in pod.stream, "…and what it wrote")
    check(pod.status == "idle", "…and it is idle once the call is over")
    link = lab.links.get(("orchestrator", pod.name))
    check(link is not None and link["active"] is False and link["count"] == 1, "a delegation link from the orchestrator, closed when it finished")
    row = pod.to_dict(detail=False)
    check(row["ephemeral"] and "run it there" in row["reasoning"] and row["task"], "the snapshot carries task, reasoning and ephemeral")

    for i in range(10):  # a long session: only the last few finished per-call pods are kept
        lab.spawn("explorer", role="subagent:explorer", parent="orchestrator", task=f"q{i}").status = "idle"
    kept = [p for p in lab.pods.values() if p.ephemeral]
    check(len(kept) <= lab_module._KEEP_FINISHED_EPHEMERAL + 1, f"finished per-call pods are pruned ({len(kept)} kept)")

    progress = _SpecialistProgress()
    progress.feed({"type": "tool_use_stream", "current_tool_use": {"toolUseId": "t1", "name": "code_search"}})
    progress.feed({"reasoningText": "the test imports calc " * 3, "reasoning": True})
    rendered = progress.render("debugger")
    check(rendered.startswith("started: debugger\ntool: debugger - code_search\nthinking: debugger - "), f"the tool card speaks the team format ({rendered!r})")
    check("imports calc" in rendered, "…with the reasoning tail")
    lab.reset()


def background() -> None:
    children = Children([text("child answer: 42")])
    delegate = subagent_tool.subagent_tool(model_factory=children)
    parent = parent_with(
        delegate,
        [
            call("subagent", {"task": "count the files", "agent_type": "explorer", "_background_execution": True}),
            text("kept working meanwhile"),
            text("final: the child said 42"),
        ],
        checkpointing=True,
        background_tasks=subagent_tool.BACKGROUND_TASKS,
    )
    subagent_tool.hold_turn_for_background(parent)
    policy = parent._background_tasks._policy
    check(policy == {"subagent": "agentic"}, f"the model may background a subagent call, and nothing else ({policy})")

    async def turn():
        agent_input, pauses = "go", 0
        while True:  # acp_server's loop: a checkpoint pause is resumed at once, inside the same turn
            result = await parent.invoke_async(agent_input)
            if result.stop_reason != "checkpoint":
                return result, pauses
            pauses += 1
            agent_input = {"checkpointResume": {"checkpoint": result.checkpoint.to_dict()}}

    result, pauses = asyncio.run(turn())
    check(pauses >= 1, f"the turn went through checkpoint pauses ({pauses})")
    check(str(result).strip() == "final: the child said 42", f"the turn ends with the child's answer in hand ({str(result)!r})")
    delivered = [c.get("text", "") for m in parent.messages for b in m["content"] if "toolResult" in b for c in b["toolResult"].get("content", []) if isinstance(c, dict)]
    check(any("child answer: 42" in t for t in delivered), "the background result was delivered into the conversation")
    uses = [b["toolUse"]["name"] for m in parent.messages for b in m["content"] if "toolUse" in b]
    check("strands_manage_background_task" in uses, f"…as a background task's result, not a foreground one ({uses})")
    order = [m["content"][0].get("text", "") for m in parent.messages if m["role"] == "assistant" and m["content"] and "text" in m["content"][0]]
    check(order and order[0].startswith("kept working"), f"the parent kept working before the result came in ({order})")


def containment() -> None:
    check("subagent" in sub_agent_common.ORCHESTRATOR_ONLY_TOOLS, "subagent is orchestrator-only for every sub-agent")
    import prompt_security

    check("subagent" in prompt_security._SUBAGENT_TOOLS, "its result is fenced as delegated output")


def main() -> None:
    schema()
    gate()
    roles()
    watching()
    background()
    containment()
    print("\nPASS: subagent delegates through the session's gate, roles hold their kits, every call is watchable, background calls finish inside the turn")


if __name__ == "__main__":
    main()

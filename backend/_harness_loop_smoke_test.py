"""Phase-0 validation: the Strands primitives the agentic harness is built on
behave the way the harness relies on, on the installed SDK (no network, no
model server — a ScriptedModel plays the model).

1. Agent loop: tool call -> result -> reply.
2. Checkpoints: ``checkpointing=True`` pauses after the model asks for tools and
   resumes from ``{"checkpointResume": ...}``.
3. Interventions: ``Confirm`` raises an interrupt; the resume payload decides.
4. Agents-as-tools: a specialist's interrupt surfaces in the *parent* and the
   parent's resume reaches the specialist (how specialists stay HITL-gated).
5. ``AfterInvocationEvent.resume``: a hook re-invokes the agent on its own (the
   self-healing loop's continuation primitive).
6. MCP stdio transport: a real MCP server process, its tools called by the
   agent, MCP progress notifications delivered, and tools hot-added to / removed
   from a live agent's registry.

    backend/.venv/bin/python _harness_loop_smoke_test.py
"""

import asyncio
import sys
from pathlib import Path

from mcp import StdioServerParameters, stdio_client
from strands import Agent, tool
from strands.hooks import AfterInvocationEvent
from strands.interventions import Confirm, InterventionHandler, Proceed
from strands.tools.mcp import MCPClient

from harness_testing import ScriptedModel, check, text, tool as call

HERE = Path(__file__).parent.resolve()


@tool
def add(a: int, b: int) -> str:
    """Add two numbers."""
    return str(a + b)


@tool
def danger(path: str) -> str:
    """Pretend to delete a path."""
    return f"deleted {path}"


class AskForDanger(InterventionHandler):
    name = "ask-for-danger"

    def before_tool_call(self, event):
        if event.tool_use.get("name") == "danger":
            return Confirm(prompt="delete?")
        return Proceed()


def loop() -> None:
    agent = Agent(model=ScriptedModel([call("add", {"a": 2, "b": 3}), text("five")]), tools=[add], callback_handler=None)
    result = agent("add")
    check(str(result).strip() == "five", "agent loop: tool call, result, reply")
    tool_results = [b["toolResult"] for m in agent.messages for b in m["content"] if "toolResult" in b]
    check(tool_results and tool_results[0]["content"][0]["text"] == "5", "tool result is in the conversation")


def checkpoints() -> None:
    agent = Agent(
        model=ScriptedModel([call("add", {"a": 1, "b": 1}), text("two")]),
        tools=[add],
        callback_handler=None,
        checkpointing=True,
    )
    result = agent("go")
    check(result.stop_reason == "checkpoint", f"checkpointing pauses at a cycle boundary (got {result.stop_reason})")
    positions = [result.checkpoint.position]
    while result.stop_reason == "checkpoint":
        result = agent({"checkpointResume": {"checkpoint": result.checkpoint.to_dict()}})
        if result.stop_reason == "checkpoint":
            positions.append(result.checkpoint.position)
    check(positions == ["after_model", "after_tools"], f"both boundaries fire in order ({positions})")
    check(str(result).strip() == "two", "resuming from checkpoints finishes the turn")


def interventions() -> None:
    agent = Agent(
        model=ScriptedModel([call("danger", {"path": "/tmp/x"}), text("ok")]),
        tools=[danger],
        callback_handler=None,
        interventions=[AskForDanger()],
    )
    result = agent("delete it")
    check(result.stop_reason == "interrupt" and len(result.interrupts) == 1, "Confirm pauses with one interrupt")
    interrupt = result.interrupts[0]
    result = agent([{"interruptResponse": {"interruptId": interrupt.id, "response": False}}])
    denied = [b["toolResult"] for m in agent.messages for b in m["content"] if "toolResult" in b]
    check(denied and "deleted" not in denied[-1]["content"][0]["text"], "a False response denies the call")


def specialist_interrupts_reach_parent() -> None:
    specialist = Agent(
        name="debugger",
        description="fixes things",
        model=ScriptedModel([call("danger", {"path": "build/"}), text("cleaned")]),
        tools=[danger],
        callback_handler=None,
        interventions=[AskForDanger()],
    )
    parent = Agent(
        model=ScriptedModel([call("debugger", {"input": "clean build"}), text("specialist finished")]),
        tools=[specialist.as_tool()],
        callback_handler=None,
    )
    result = parent("fix the build")
    check(result.stop_reason == "interrupt", "a specialist's Confirm pauses the parent")
    interrupt = result.interrupts[0]
    result = parent([{"interruptResponse": {"interruptId": interrupt.id, "response": True}}])
    check(str(result).strip() == "specialist finished", "the parent resumes after approval")
    inner = [b["toolResult"] for m in specialist.messages for b in m["content"] if "toolResult" in b]
    check(inner and inner[-1]["content"][0]["text"] == "deleted build/", "the approved call ran inside the specialist")


def autonomous_resume() -> None:
    seen: list[str] = []

    def again(event: AfterInvocationEvent) -> None:
        seen.append(str(event.result).strip())
        if len(seen) == 1:
            event.resume = "continue: verify"

    model = ScriptedModel([text("attempt"), text("verified")])
    agent = Agent(model=model, callback_handler=None, hooks=[again])
    result = agent("start")
    check(seen == ["attempt", "verified"], f"AfterInvocationEvent.resume re-invokes the agent ({seen})")
    check(str(result).strip() == "verified" and model.calls[-1]["last_text"] == "continue: verify", "…with the hook's input")


def mcp_transport() -> None:
    progress: list[tuple] = []

    async def on_progress(value, total, message):  # MCP awaits it: must be async
        progress.append((value, total, message))

    def make_client(prefix: str | None = None) -> MCPClient:
        params = StdioServerParameters(command=sys.executable, args=[str(HERE / "fixtures" / "echo_mcp_server.py")])
        return MCPClient(lambda: stdio_client(params), prefix=prefix, progress_callback=on_progress)

    with make_client() as client:
        names = [t.tool_name for t in client.list_tools_sync()]
        check(sorted(names) == ["echo", "slow_count"], f"stdio MCP server lists its tools ({names})")
        agent = Agent(
            model=ScriptedModel([call("echo", {"text": "hi"}), call("slow_count", {"n": 3}), text("done")]),
            tools=client.list_tools_sync(),
            callback_handler=None,
        )
        agent("use the server")
        results = [b["toolResult"]["content"][0]["text"] for m in agent.messages for b in m["content"] if "toolResult" in b]
        check(results[:2] == ["echo: hi", "counted to 3"], f"the agent calls MCP tools over stdio ({results})")
        check([p[0] for p in progress] == [1, 2, 3] and progress[-1][1] == 3, f"MCP progress notifications arrive ({progress})")

    # Hot-swap: a running agent gains a server's tools and later loses them.
    agent = Agent(model=ScriptedModel([text("x")]), tools=[add], callback_handler=None)
    with make_client(prefix="hot") as client:
        before = set(agent.tool_names)
        added = agent.tool_registry.process_tools(client.list_tools_sync())
        check(set(agent.tool_names) - before == {"hot_echo", "hot_slow_count"}, f"tools hot-added with a prefix ({added})")
        for name in added:
            agent.tool_registry.registry.pop(name, None)
            agent.tool_registry.dynamic_tools.pop(name, None)
        check(set(agent.tool_names) == before, "…and removed again from the live registry")
        specs = [spec["name"] for spec in agent.tool_registry.get_all_tool_specs()]
        check(specs == ["add"], "the model no longer sees removed tools")


def main() -> None:
    loop()
    checkpoints()
    interventions()
    specialist_interrupts_reach_parent()
    autonomous_resume()
    mcp_transport()
    print("\nPASS: Strands agent loop, checkpoints, interventions, agents-as-tools, resume and MCP stdio behave as the harness assumes")


if __name__ == "__main__":
    main()

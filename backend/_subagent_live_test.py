"""LIVE check of the ``subagent`` tool against the signed-in local Ollama daemon (needs network + ``ollama signin``;
costs a few thousand cloud tokens). Not part of the offline suite (``run_tests.py`` runs ``*_smoke_test.py``).

A real orchestrator model delegates a read-only question to an ``explorer`` child, with checkpoints and background
execution on as the ACP session runs them, and the Lab watches the child: its pod, task, tool trace, the text it
writes and — when the model streams any — its reasoning, live.

    backend/.venv/bin/python _subagent_live_test.py
"""

import asyncio
import os
import time

os.environ.setdefault("MODEL_PROVIDER", "ollama_cloud")

from strands import Agent  # noqa: E402

import agent_common  # noqa: E402
import lab as lab_module  # noqa: E402
import subagent_tool  # noqa: E402
from harness_testing import check  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))


async def main() -> None:
    os.chdir(HERE)
    lab = lab_module.get()
    lab.reset()
    seen_reasoning: list[str] = []
    seen_stream: list[str] = []
    real_emit = lab.emit

    def watch(*, force: bool = False) -> None:  # what a client would have seen, frame by frame
        for pod in lab.pods.values():
            if pod.ephemeral:
                if pod.reasoning and (not seen_reasoning or seen_reasoning[-1] != pod.reasoning):
                    seen_reasoning.append(pod.reasoning)
                if pod.stream and (not seen_stream or seen_stream[-1] != pod.stream):
                    seen_stream.append(pod.stream)
        real_emit(force=force)

    lab.emit = watch
    parent = Agent(
        model=agent_common.build_model(),
        tools=[subagent_tool.subagent_tool()],
        system_prompt=(
            "You coordinate work. For any question about the codebase, delegate it with the subagent tool "
            "(agent_type explorer) and answer from its report. Be brief."
        ),
        checkpointing=True,
        background_tasks=subagent_tool.BACKGROUND_TASKS,
        callback_handler=None,
    )
    subagent_tool.hold_turn_for_background(parent)
    started = time.monotonic()
    agent_input = (
        "Which Python file in this directory defines the constant TOOL_NAME = \"subagent\", and what are the four "
        "values in its ROLES tuple? Use the subagent tool."
    )
    pauses = 0
    while True:
        result = await parent.invoke_async(agent_input)
        if result.stop_reason != "checkpoint":
            break
        pauses += 1
        agent_input = {"checkpointResume": {"checkpoint": result.checkpoint.to_dict()}}
    elapsed = time.monotonic() - started
    answer = str(result)
    print(f"\n--- answer ({elapsed:.1f}s, {pauses} checkpoint pauses) ---\n{answer.strip()}\n")

    calls = [b["toolUse"] for m in parent.messages for b in m["content"] if "toolUse" in b and b["toolUse"]["name"] == "subagent"]
    check(bool(calls), f"the real model delegated through subagent ({[c.get('input') for c in calls]})")
    pods = [p for p in lab.pods.values() if p.ephemeral]
    check(bool(pods), f"each call got a Lab pod ({[p.name for p in pods]})")
    pod = pods[-1]
    print(f"pod {pod.name}: role={pod.role} model={pod.model} effort={pod.effort} calls={pod.calls} status={pod.status}")
    print(f"  task: {pod.task[:160]}")
    print(f"  trace: {[(row['tool'], row['status']) for row in pod.trace]}")
    print(f"  reasoning frames seen live: {len(seen_reasoning)}; last: {seen_reasoning[-1][-200:] if seen_reasoning else '(model streamed none)'}")
    print(f"  stream frames seen live: {len(seen_stream)}")
    check(pod.trace and all(row["status"] in ("completed", "failed") for row in pod.trace), "the child's tool calls are in its pod's trace")
    check(len(seen_stream) > 1 or len(seen_reasoning) > 1, "the child's output reached the Lab while it ran, not only at the end")
    check("subagent_tool.py" in answer, "the answer names the file")
    check(all(role in answer for role in subagent_tool.ROLES), "…and the four roles")
    print("\nPASS (live)")


if __name__ == "__main__":
    asyncio.run(main())

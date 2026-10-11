"""The web app's door into the conversation hub (`hub_client.py`), against a real hub.

A real daemon `State` (supervisor + conversation hub) and gRPC bridge on a free port, driving the
scripted ACP child; `HubAgent.run` is fed real AG-UI `RunAgentInput`s and its events are checked —
what the CopilotKit frontend would receive. No model, no network, no tunnel (the bridge is local).

1. **configured or not** — env parsing, the token file, and a repr that never prints the token.
2. **a web thread is a hub conversation** — the first message opens one (origin `web`), the reply
   streams as AG-UI text, the thread remembers it, and the next message continues it.
3. **a conversation from another door** — a thread whose id is a handle continues that conversation;
   its history is readable for the "earlier in this conversation" panel.
4. **the laptop asleep** — a hub that cannot be reached is a sentence, not a stack trace.

    backend/.venv/bin/python _hub_web_smoke_test.py
"""

import asyncio
import json
import os
import socket
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
_ROOT = Path(tempfile.mkdtemp(prefix="sierralab-hub-web-"))
os.environ["SIERRALAB_HOME"] = str(_ROOT / "home")
os.environ["SIERRALAB_DAEMON_CONFIG"] = str(_ROOT / "home" / "daemon.json")
os.environ["SIERRALAB_STATE_DB"] = str(_ROOT / "home" / "state.db")
os.environ["SIERRALAB_DISCORD"] = "off"
os.environ["FAKE_ACP_STATE"] = str(_ROOT / "fake-acp-state.json")
os.environ["SIERRALAB_ACP_ENTRYPOINT"] = f"{sys.executable} {HERE / 'fixtures' / 'fake_acp_chat_child.py'}"

from ag_ui.core import RunAgentInput, UserMessage  # noqa: E402

import hub_client  # noqa: E402
from daemon.grpc_bridge import GrpcBridge  # noqa: E402
from daemon.server import State  # noqa: E402
from harness_testing import check  # noqa: E402


def run_input(thread_id: str, text: str) -> RunAgentInput:
    return RunAgentInput(thread_id=thread_id, run_id=f"r-{thread_id}-{len(text)}", messages=[UserMessage(id="m1", role="user", content=text)], state={}, tools=[], context=[], forwarded_props={})


async def collect(agent: hub_client.HubAgent, thread_id: str, text: str) -> tuple[list[str], str]:
    kinds, text_out = [], []
    async for event in agent.run(run_input(thread_id, text)):
        kind = str(getattr(event, "type", ""))
        kinds.append(kind.rsplit(".", 1)[-1])
        if hasattr(event, "delta"):
            text_out.append(event.delta)
    return kinds, "".join(text_out)


def test_configuration() -> None:
    check(hub_client.configured({}) is None, "no SIERRALAB_HUB_GRPC: the web backend keeps its own agent")
    check(hub_client.configured({"SIERRALAB_HUB_GRPC": "h:1"}) is None, "an address without a token is not enough")
    token_file = _ROOT / "hub.json"
    token_file.write_text(json.dumps({"token": "s3cret-token-value", "port": 8765}))
    config = hub_client.configured({"SIERRALAB_HUB_GRPC": "host.docker.internal:18766", "SIERRALAB_HUB_TOKEN_FILE": str(token_file), "SIERRALAB_HUB_CWD": "/p"})
    check(config is not None and config.token == "s3cret-token-value" and config.cwd == "/p", "the token is read from the copied daemon.json")
    check("s3cret" not in repr(config), "and never printed")
    check(hub_client.ThreadMap(_ROOT / "m.json").get("c3f9a1e") == "c3f9a1e", "a thread named like a handle is that conversation")


async def test_web_turns() -> None:
    state = State(port=0)
    state.chats.start()
    bridge = GrpcBridge(state, port=0)
    port = await bridge.start()
    project = _ROOT / "project"
    project.mkdir()
    config = hub_client.HubConfig(target=f"127.0.0.1:{port}", token=state.token, cwd=str(project))
    threads = hub_client.ThreadMap(_ROOT / "sessions" / "hub_threads.json")
    agent = hub_client.HubAgent(config, threads)
    try:
        kinds, said = await collect(agent, "web-thread-1", "hello from the web")
        check(kinds[0] == "RUN_STARTED" and kinds[-1] == "RUN_FINISHED", f"an AG-UI run, start to finish ({kinds[:3]}…)")
        check("TEXT_MESSAGE_START" in kinds and "TEXT_MESSAGE_CONTENT" in kinds and "TEXT_MESSAGE_END" in kinds, "the reply arrives as streamed AG-UI text")
        check("You said: hello from the web" in said, f"the agent on the laptop answered it ({said[:80]})")
        handle = threads.get("web-thread-1")
        conv = state.chats.resolve(handle or "")
        check(conv is not None and conv.origin == "web" and conv.cwd == str(project.resolve()), "the thread opened a hub conversation from the web app, in the configured folder")
        check(oct((_ROOT / "sessions" / "hub_threads.json").stat().st_mode & 0o777) == "0o600", "the thread map is private")

        _, said = await collect(agent, "web-thread-1", "and a second thing")
        check("You said: and a second thing" in said and state.chats.resolve(handle).turns == 2, "the next message continues the same conversation")

        terminal = state.chats.create(cwd=str(project), title="from the terminal", origin="terminal")

        class Quiet:
            def text(self, delta):  # noqa: ANN001
                pass

            def activity(self, label):  # noqa: ANN001
                pass

        await state.chats.send(terminal.key, "the terminal asked first", Quiet(), origin="terminal")
        _, said = await collect(agent, terminal.handle, "the web follows up")
        check("You said: the web follows up" in said and state.chats.resolve(terminal.handle).turns == 2, "a thread named by a handle continues a conversation another door started")
        turns = await hub_client.history(config, terminal.handle)
        check([t["origin"] for t in turns] == ["terminal", "web"] and "the terminal asked first" in turns[0]["asked"], f"its history shows both doors' turns ({[t['origin'] for t in turns]})")
        listed = await hub_client.list_conversations(config)
        check({row["handle"] for row in listed} >= {handle, terminal.handle}, "the sidebar can list every door's conversations")
    finally:
        await bridge.stop(0.5)
        await state.chats.stop()
        await state.supervisor.stop_all(grace=1.0)

    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        dead = probe.getsockname()[1]
    asleep = hub_client.HubAgent(hub_client.HubConfig(target=f"127.0.0.1:{dead}", token="x"), hub_client.ThreadMap(_ROOT / "asleep.json"))
    kinds, said = await collect(asleep, "web-thread-2", "anyone there?")
    check(kinds[-1] == "RUN_FINISHED" and "not reachable" in said, f"with the laptop asleep the web app says so and finishes the run ({said[:70]})")


async def main() -> None:
    test_configuration()
    await test_web_turns()
    print("\nhub web: all checks passed")


if __name__ == "__main__":
    asyncio.run(main())

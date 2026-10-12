"""Regression check: nothing but ACP frames may reach the protocol's stdout.

A real bug: Strands' default callback handler printed the streamed reply to the
same stdout the ACP transport writes JSON-RPC frames to. Text printed without a
trailing newline glued itself onto the next frame ("pong{"jsonrpc":...}"); the
client could not parse the line and dropped the frame, so agent_message_chunk
updates vanished (deterministically under PYTHONUNBUFFERED=1).

No LLM: a child process applies acp_server's stdout protection, then does every
kind of stray write, then writes one frame the way acp.stdio's transport does.

    backend/.venv/bin/python _acp_stdout_smoke_test.py
"""

import os
os.environ.setdefault("CORDATA_REFLECT", "off")  # scripted/live test turns are not lessons (reflect.py)
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).parent.resolve()

CHILD = r"""
import os, subprocess, sys
sys.path.insert(0, %r)
import acp_server
acp_server._protect_protocol_stdout()

print("stray print without newline", end="")                 # Strands' PrintingCallbackHandler
print("a rich-style panel line")
sys.stdout.write("raw text write")
os.write(1, b"C-level write to fd 1\n")                       # anything that bypasses sys.stdout
subprocess.run(["echo", "child process inheriting stdout"])   # e.g. a tool's subprocess

# The real transport: acp.stdio.stdio_streams() — on POSIX it hands sys.stdout to
# loop.connect_write_pipe, i.e. uses sys.stdout.fileno(). (A first version of the
# protection forwarded fileno() to stderr and silently sent every frame there; a
# test that only wrote to sys.stdout.buffer missed it.)
import asyncio, acp.stdio

async def serve():
    reader, writer = await acp.stdio.stdio_streams()
    print("stray after the streams exist", end="")
    writer.write(b'{"jsonrpc":"2.0","method":"session/update","params":{}}\n')
    await writer.drain()
    print("after the frame", end="")
    sys.stdout.flush()

asyncio.run(serve())
"""


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


def main() -> None:
    for unbuffered in ("1", "0"):
        env = {**os.environ, "PYTHONUNBUFFERED": unbuffered}
        out = subprocess.run([sys.executable, "-c", CHILD % str(HERE)], capture_output=True, env=env, cwd=HERE, timeout=60, input=b"")
        stdout, stderr = out.stdout.decode(), out.stderr.decode()
        tag = f"(PYTHONUNBUFFERED={unbuffered})"
        assert out.returncode == 0, f"FAIL: the child crashed {tag}: {stderr[-400:]}"
        check(True, f"the child ran {tag}")
        check(stdout == '{"jsonrpc":"2.0","method":"session/update","params":{}}\n', f"stdout carries exactly the protocol frame and nothing else {tag}: {stdout!r}")
        for stray in ("stray print without newline", "a rich-style panel line", "raw text write", "C-level write to fd 1", "child process inheriting stdout", "stray after the streams exist", "after the frame"):
            check(stray in stderr, f"stray output {stray!r} went to stderr {tag}")

    # The ACP agent must not own a printing callback handler.
    import agent_common

    agent_common.ACTIVE_PROVIDER = "local"  # this test is about stdout, not about which provider/key is configured
    agent = agent_common.build_agent_for_session("stdout-smoke")
    check(type(agent.callback_handler).__name__ != "PrintingCallbackHandler", "the ACP agent has no printing callback handler")
    print("\nPASS: only ACP frames reach the protocol stdout; stray writes go to stderr")


if __name__ == "__main__":
    main()

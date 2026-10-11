"""Tiny MCP stdio server used by the harness tests (plugin host, transports).

Two tools: ``echo`` returns its input, ``slow_count`` reports MCP progress
notifications while it counts — what the TUI turns into a progress bar. One
resource (``notes://readme``) and one resource template (``notes://{topic}``),
for ``mcp_resources``.
"""

import asyncio
import os

from mcp.server.mcpserver import Context, MCPServer

server = MCPServer(os.environ.get("ECHO_SERVER_NAME", "echo"))


@server.tool()
def echo(text: str) -> str:
    """Return the text unchanged."""
    return f"echo: {text}"


@server.tool()
async def slow_count(n: int, ctx: Context) -> str:
    """Count to n, reporting progress after each step."""
    for i in range(1, n + 1):
        await ctx.report_progress(i, n, f"step {i}/{n}")
        await asyncio.sleep(0.01)
    return f"counted to {n}"


@server.resource("notes://readme", name="readme", description="what this server is")
def readme() -> str:
    return "The echo server: echo and slow_count."


@server.resource("notes://{topic}", name="topic", description="a note about one topic")
def topic(topic: str) -> str:
    return f"note about {topic}"


if __name__ == "__main__":
    server.run()

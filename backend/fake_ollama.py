"""A scriptable stand-in for the local Ollama daemon, for end-to-end tests.

Serves just what ``ollama_cloud`` talks to — ``GET /api/version``,
``GET /api/tags`` and ``POST /v1/chat/completions`` (streamed SSE) — on a
loopback port, answers each model from its own script, and writes down every
request (path, auth header, JSON body). Anything else is a 404 and is recorded
too, so a test can assert that nothing but chat completions was ever requested.

A script is a list of ``harness_testing`` steps (``text(...)`` / ``tool(name,
input)``) per *wire* model id (``kimi-k2.7-code:cloud``); once a model's script is
spent it keeps answering ``(done)``. Not imported by the server.
"""

from __future__ import annotations

import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

from harness_testing import Step


def _chunk(delta: dict, finish: str | None = None, model: str = "fake") -> dict:
    return {"id": "chatcmpl-fake", "object": "chat.completion.chunk", "created": 0, "model": model, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}


def _sse(*chunks: dict) -> bytes:
    return b"".join(b"data: " + json.dumps(chunk).encode() + b"\n\n" for chunk in chunks) + b"data: [DONE]\n\n"


USAGE = {"id": "chatcmpl-fake", "object": "chat.completion.chunk", "created": 0, "model": "fake", "choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}}


def render(step: Step, call_id: str) -> bytes:
    if step.kind == "tool":
        call = {"index": 0, "id": call_id, "type": "function", "function": {"name": step.name, "arguments": json.dumps(step.input)}}
        return _sse(_chunk({"role": "assistant", "tool_calls": [call]}), _chunk({}, "tool_calls"), USAGE)
    return _sse(_chunk({"role": "assistant", "content": step.text}), _chunk({}, "stop"), USAGE)


class FakeOllama:
    def __init__(self, scripts: dict[str, list[Step]], *, delay: float = 0.0) -> None:
        self.scripts = {model: list(steps) for model, steps in scripts.items()}
        self.delay = delay
        self.requests: list[dict[str, Any]] = []
        self._lock = threading.Lock()
        self._calls = 0
        fake = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.0"  # the stream ends when the connection does

            def log_message(self, *args: Any) -> None:
                pass

            def _record(self, body: dict | None) -> None:
                with fake._lock:
                    fake.requests.append({"method": self.command, "path": self.path, "auth": self.headers.get("Authorization"), "body": body})

            def do_GET(self) -> None:  # noqa: N802
                self._record(None)
                if self.path == "/api/version":
                    self._json({"version": "fake"})
                elif self.path == "/api/tags":
                    self._json({"models": []})
                else:
                    self._json({"error": "not found"}, 404)

            def do_POST(self) -> None:  # noqa: N802
                length = int(self.headers.get("Content-Length") or 0)
                try:
                    body = json.loads(self.rfile.read(length) or b"{}")
                except ValueError:
                    body = {}
                self._record(body)
                if self.path != "/v1/chat/completions":
                    self._json({"error": "not found"}, 404)
                    return
                if fake.delay:
                    time.sleep(fake.delay)
                step, call_id = fake._next(body.get("model", ""))
                payload = render(step, call_id)
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.wfile.write(payload)

            def _json(self, payload: dict, status: int = 200) -> None:
                data = json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        self._server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self._thread = threading.Thread(target=self._server.serve_forever, name="fake-ollama", daemon=True)

    def _next(self, model: str) -> tuple[Step, str]:
        with self._lock:
            self._calls += 1
            script = self.scripts.get(model) or []
            step = script.pop(0) if script else Step(kind="text", text="(done)")
            return step, f"call_{self._calls}"

    # ------------------------------------------------------------ lifecycle

    def start(self) -> "FakeOllama":
        self._thread.start()
        return self

    def stop(self) -> None:
        self._server.shutdown()
        self._server.server_close()

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self._server.server_address[1]}/v1"

    def __enter__(self) -> "FakeOllama":
        return self.start()

    def __exit__(self, *exc: object) -> None:
        self.stop()

    # ---------------------------------------------------------------- asks

    def chats(self, model: str | None = None) -> list[dict[str, Any]]:
        """The decoded chat-completion request bodies, optionally for one wire model."""
        rows = [r["body"] for r in self.requests if r["path"] == "/v1/chat/completions"]
        return [body for body in rows if model is None or body.get("model") == model]

    def paths(self) -> list[str]:
        return [f"{r['method']} {r['path']}" for r in self.requests]

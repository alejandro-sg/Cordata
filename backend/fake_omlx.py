"""A scriptable stand-in for the oMLX server, for tests and the Phase 7 end-to-end run.

Serves what ``omlx_manager`` and ``omlx_provider`` talk to, on a loopback port:

* ``GET  /health``, ``GET /v1/models``, ``GET /v1/models/status``
* ``POST /v1/models/{id}/load`` and ``/unload`` (state is real: ``status`` reflects them)
* ``POST /v1/chat/completions`` (streamed SSE, answered from a per-model script)

Every request is recorded with the model's loaded state *at that moment*
(``chats()[i]["loaded"]``) — that is how a test proves "the model was loaded before the
completion, and unloaded after". Real oMLX loads an inactive model on a chat request as
a side effect (``autoload=True``, the default here too); ``autoload=False`` answers 409
instead, for tests that want a request to an unloaded model to be an error.

Sizes default to the real on-disk footprints from the spec (8.41 GB / 8.30 GB for the two
Prism models). Not imported by the server.
"""

from __future__ import annotations

import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.parse import unquote

from fake_ollama import render
from harness_testing import Step

GB = 1024**3

DEFAULT_MODELS: dict[str, float] = {
    "Ternary-Bonsai-2-27B-mlx-2bit": 8.41,
    "Ternary-Bonsai-27B-mlx-2bit": 8.30,
    "Qwen2.5-Coder-32B-Instruct-4bit": 19.35,
    "DeepSeek-R1-Distill-Qwen-32B-4bit": 19.35,
}


class FakeOmlx:
    def __init__(
        self,
        scripts: dict[str, list[Step]] | None = None,
        *,
        models: dict[str, float] | None = None,
        ceiling_gb: float = 37.44,
        autoload: bool = True,
        load_delay: float = 0.0,
        pinned: set[str] | None = None,
        loaded: set[str] | None = None,
        broken: set[str] | None = None,
    ) -> None:
        self.scripts = {model: list(steps) for model, steps in (scripts or {}).items()}
        self.sizes = {name: int(gb * GB) for name, gb in (models or DEFAULT_MODELS).items()}
        self.ceiling = int(ceiling_gb * GB)
        self.autoload = autoload
        self.load_delay = load_delay
        self.pinned = set(pinned or ())
        self.broken = set(broken or ())  # load answers 409, as oMLX does for a model type it cannot run
        self.loaded: dict[str, float] = {name: time.time() for name in (loaded or ())}
        self.requests: list[dict[str, Any]] = []
        self.events: list[str] = []  # "load X" / "unload X" / "chat X", in order
        self._lock = threading.Lock()
        self._calls = 0
        fake = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.0"

            def log_message(self, *args: Any) -> None:
                pass

            def _record(self, body: Any = None) -> None:
                with fake._lock:
                    fake.requests.append({"method": self.command, "path": self.path, "auth": self.headers.get("Authorization"), "body": body})

            def do_GET(self) -> None:  # noqa: N802
                self._record()
                if self.path == "/health":
                    self._json({"status": "healthy", "engine_pool": {"model_count": len(fake.sizes), "loaded_count": len(fake.loaded)}})
                elif self.path == "/v1/models":
                    self._json({"object": "list", "data": [{"id": name, "object": "model", "owned_by": "omlx", "max_model_len": 262144} for name in fake.sizes]})
                elif self.path == "/v1/models/status":
                    self._json(fake.status())
                else:
                    self._json({"detail": "not found"}, 404)

            def do_POST(self) -> None:  # noqa: N802
                length = int(self.headers.get("Content-Length") or 0)
                raw = self.rfile.read(length) if length else b""
                try:
                    body = json.loads(raw or b"{}")
                except ValueError:
                    body = {}
                self._record(body)
                path = self.path
                if path.startswith("/v1/models/") and path.endswith(("/load", "/unload")):
                    model, action = unquote(path.removeprefix("/v1/models/")).rsplit("/", 1)
                    if model not in fake.sizes:
                        self._json({"detail": f"model {model} not found"}, 404)
                        return
                    if action == "load" and model in fake.broken:
                        self._json({"error": {"message": f"Model '{model}' is unavailable after a previous load failure: Model type not supported.", "type": "invalid_request_error"}}, 409)
                        return
                    if action == "load":
                        if fake.load_delay:
                            time.sleep(fake.load_delay)
                        with fake._lock:
                            fake.loaded.setdefault(model, time.time())
                            fake.events.append(f"load {model}")
                        self._json({"status": "loaded", "model_id": model})
                    else:
                        if model in fake.pinned:
                            self._json({"detail": f"model {model} is pinned"}, 409)
                            return
                        with fake._lock:
                            fake.loaded.pop(model, None)
                            fake.events.append(f"unload {model}")
                        self._json({"status": "unloaded", "model_id": model})
                    return
                if path == "/v1/chat/completions":
                    model = body.get("model", "")
                    if model not in fake.sizes:
                        self._json({"error": {"message": f"model {model} not found"}}, 404)
                        return
                    was_loaded = model in fake.loaded
                    if not was_loaded and not fake.autoload:
                        self._json({"error": {"message": f"model {model} is not loaded"}}, 409)
                        return
                    with fake._lock:
                        fake.loaded.setdefault(model, time.time())
                        fake.loaded[model] = time.time()
                        fake.events.append(f"chat {model}")
                        fake.requests[-1]["loaded"] = was_loaded
                        fake._calls += 1
                        script = fake.scripts.get(model) or []
                        step = script.pop(0) if script else Step(kind="text", text="(done)")
                        call_id = f"call_{fake._calls}"
                    payload = render(step, call_id)
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.end_headers()
                    self.wfile.write(payload)
                    return
                self._json({"detail": "not found"}, 404)

            def _json(self, payload: dict, status: int = 200) -> None:
                data = json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        self._server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self._thread = threading.Thread(target=self._server.serve_forever, name="fake-omlx", daemon=True)

    def status(self) -> dict[str, Any]:
        with self._lock:
            rows = []
            for name, size in self.sizes.items():
                is_loaded = name in self.loaded
                rows.append(
                    {
                        "id": name,
                        "loaded": is_loaded,
                        "is_loading": False,
                        "estimated_size": size,
                        "actual_size": size if is_loaded else None,
                        "pinned": name in self.pinned,
                        "last_access": self.loaded.get(name),
                        "model_context_length": 262144,
                        "engine_type": "batched",
                    }
                )
            return {
                "final_ceiling": self.ceiling,
                "current_model_memory": sum(self.sizes[name] for name in self.loaded),
                "model_count": len(self.sizes),
                "loaded_count": len(self.loaded),
                "models": rows,
            }

    # ------------------------------------------------------------ lifecycle

    def start(self) -> "FakeOmlx":
        self._thread.start()
        return self

    def stop(self) -> None:
        self._server.shutdown()
        self._server.server_close()

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self._server.server_address[1]}/v1"

    def __enter__(self) -> "FakeOmlx":
        return self.start()

    def __exit__(self, *exc: object) -> None:
        self.stop()

    # ---------------------------------------------------------------- asks

    def chats(self, model: str | None = None) -> list[dict[str, Any]]:
        """Chat-completion requests: ``{"body", "loaded"}`` (``loaded`` = was the model
        resident when the request arrived)."""
        rows = [r for r in self.requests if r["path"] == "/v1/chat/completions"]
        return [r for r in rows if model is None or (r["body"] or {}).get("model") == model]

    def paths(self) -> list[str]:
        return [f"{r['method']} {r['path']}" for r in self.requests]

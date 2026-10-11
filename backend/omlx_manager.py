"""The oMLX server's model lifecycle: what is in unified memory, and loading/unloading it.

oMLX serves many models from one process but keeps every one of them *inactive*
until asked ("0 loaded · 0.0 B"); a chat request to an inactive model makes the
server load it as a side effect, which is the one way a local model ends up in
the 48 GB of unified memory without anyone deciding it should. This module is the
deliberate path: ``load`` and ``unload`` are explicit, verified against the
server's own status, and refused when they would not fit.

Talks to oMLX's public API (verified against oMLX 0.6.4's ``/openapi.json``):

* ``GET  /v1/models/status``       — every model: ``loaded``, ``estimated_size``,
  ``actual_size``, ``pinned``, ``last_access``; plus ``final_ceiling`` (the pool oMLX
  will not exceed) and ``current_model_memory``.
* ``POST /v1/models/{id}/load``    — blocks until the weights are resident.
* ``POST /v1/models/{id}/unload``  — drops them.
* ``GET  /health``                 — is it up.

Stdlib only, and no ``strands`` import: the daemon's gRPC bridge calls it for the
GUI's emergency [Unload Memory] button, and the daemon never imports the agent
stack (``daemon/supervisor.py``). The Strands tool that wraps it is
``omlx_resource_tool.py``; the Strands model that *talks* to a loaded model is
``omlx_provider.py``, whose transport cannot reach these endpoints — loading is
this module's job and nothing a model's own request can trigger.
"""

from __future__ import annotations

import json
import logging
import os
import time
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

logger = logging.getLogger(__name__)

#: 127.0.0.1, not "localhost": on this machine localhost resolves to ::1 first and
#: nothing answers there (settings.py's note on QDRANT_HOST / OMLX_BASE_URL).
DEFAULT_BASE_URL = "http://127.0.0.1:8000/v1"

GB = 1024**3

#: Loading blocks until the weights are resident: ~8 GB of 2-bit weights is seconds,
#: a 32 GB 4-bit model is a minute or more from a cold disk.
LOAD_TIMEOUT = 300.0
REQUEST_TIMEOUT = 10.0
#: How long an unload waits for the server's memory figure to fall after the model reads as unloaded. A 27B Prism model is
#: ~7 GB of Metal buffers; a server that flips the flag but keeps the buffers (an MLX cache that was not cleared) still
#: holds them, and that is a leak only the pool figure shows.
UNLOAD_SETTLE = 3.0


class OmlxError(RuntimeError):
    """oMLX refused, was unreachable, or answered something unexpected."""


class OmlxUnreachable(OmlxError):
    pass


class OmlxUnknownModel(OmlxError, KeyError):
    pass


def default_base_url() -> str:
    """The *Prism* endpoint when one is configured, else the harness's oMLX.

    ``OMLX_PRISM_BASE_URL`` is the secondary provider's address (docs/meta-harness.md);
    unset it follows ``OMLX_BASE_URL`` — the same server on this machine, which is why
    the default is not the spec's ``:8080``: nothing is listening there, oMLX is on
    ``:8000`` (``scripts/dev-up.sh`` and ``settings.OMLX_BASE_URL`` agree)."""
    return (os.getenv("OMLX_PRISM_BASE_URL") or os.getenv("OMLX_BASE_URL") or DEFAULT_BASE_URL).rstrip("/")


@dataclass(frozen=True)
class OmlxModelStatus:
    id: str
    loaded: bool
    loading: bool
    size_bytes: int  # resident size when loaded, else the server's estimate
    estimated_bytes: int
    pinned: bool
    last_access: float | None
    max_context: int | None = None
    engine: str | None = None

    @property
    def size_gb(self) -> float:
        return self.size_bytes / GB

    def to_json(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "loaded": self.loaded,
            "loading": self.loading,
            "size_gb": round(self.size_gb, 2),
            "estimated_gb": round(self.estimated_bytes / GB, 2),
            "pinned": self.pinned,
            "last_access": self.last_access,
        }


@dataclass(frozen=True)
class OmlxStatus:
    models: tuple[OmlxModelStatus, ...]
    ceiling_bytes: int
    memory_bytes: int  # what the loaded models hold right now

    @property
    def loaded(self) -> list[OmlxModelStatus]:
        return [model for model in self.models if model.loaded]

    @property
    def ceiling_gb(self) -> float:
        return self.ceiling_bytes / GB

    @property
    def memory_gb(self) -> float:
        return self.memory_bytes / GB

    def get(self, model_id: str) -> OmlxModelStatus | None:
        return next((model for model in self.models if model.id == model_id), None)

    def to_json(self) -> dict[str, Any]:
        return {
            "ceiling_gb": round(self.ceiling_gb, 2),
            "memory_gb": round(self.memory_gb, 2),
            "loaded": [model.to_json() for model in self.loaded],
            "models": [model.to_json() for model in self.models],
        }


# (method, url, body, headers, timeout) -> (status, parsed JSON | None). A test seam.
Transport = Callable[[str, str, bytes | None, dict[str, str], float], tuple[int, Any]]


def _urllib_transport(method: str, url: str, body: bytes | None, headers: dict[str, str], timeout: float) -> tuple[int, Any]:
    request = urllib.request.Request(url, data=body, headers=headers, method=method)
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:  # noqa: S310 - the local model server
            raw = response.read()
            status = response.status
    except urllib.error.HTTPError as error:
        raw, status = error.read(), error.code
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        raise OmlxUnreachable(f"cannot reach oMLX at {url}: {error}") from error
    try:
        return status, json.loads(raw) if raw else None
    except ValueError:
        return status, None


def _bytes(value: Any) -> int:
    try:
        return int(value or 0)
    except (TypeError, ValueError):
        return 0


class OmlxManager:
    def __init__(self, base_url: str | None = None, *, transport: Transport | None = None, api_key: str | None = None) -> None:
        self.base_url = (base_url or default_base_url()).rstrip("/")
        self.root = self.base_url.removesuffix("/v1")
        self._transport = transport or _urllib_transport
        key = api_key if api_key is not None else os.getenv("OMLX_API_KEY", "")
        self._headers = {"Accept": "application/json", **({"Authorization": f"Bearer {key}"} if key else {})}
        #: model id -> bytes the pool still held after the model was unloaded (see ``unload``). Empty is healthy.
        self.unreclaimed: dict[str, int] = {}

    def _call(self, method: str, path: str, *, timeout: float = REQUEST_TIMEOUT) -> tuple[int, Any]:
        return self._transport(method, f"{self.root}{path}", b"" if method == "POST" else None, self._headers, timeout)

    # -------------------------------------------------------------- reads

    def healthy(self, *, timeout: float = 1.5) -> bool:
        try:
            status, _ = self._call("GET", "/health", timeout=timeout)
        except OmlxError:
            return False
        return status == 200

    def status(self) -> OmlxStatus:
        code, payload = self._call("GET", "/v1/models/status")
        if code != 200 or not isinstance(payload, dict):
            raise OmlxError(f"oMLX /v1/models/status answered {code}")
        models = []
        for row in payload.get("models") or []:
            loaded = bool(row.get("loaded"))
            estimated = _bytes(row.get("estimated_size"))
            actual = _bytes(row.get("actual_size"))
            accessed = row.get("last_access")
            models.append(
                OmlxModelStatus(
                    id=str(row.get("id")),
                    loaded=loaded,
                    loading=bool(row.get("is_loading")),
                    size_bytes=(actual or estimated) if loaded else estimated,
                    estimated_bytes=estimated,
                    pinned=bool(row.get("pinned")),
                    last_access=float(accessed) if isinstance(accessed, (int, float)) else None,
                    max_context=row.get("model_context_length"),
                    engine=row.get("engine_type"),
                )
            )
        return OmlxStatus(
            models=tuple(models),
            ceiling_bytes=_bytes(payload.get("final_ceiling")),
            memory_bytes=_bytes(payload.get("current_model_memory")),
        )

    def resolve(self, model: str, status: OmlxStatus | None = None) -> OmlxModelStatus:
        """The server's row for ``model`` — by exact id, or by id with a registry
        org prefix stripped (``prism-ml/Ternary-Bonsai-27B-mlx-2bit``)."""
        status = status or self.status()
        for candidate in (model, model.rsplit("/", 1)[-1]):
            row = status.get(candidate)
            if row is not None:
                return row
        raise OmlxUnknownModel(f"oMLX has no model {model!r}; it serves: {', '.join(m.id for m in status.models) or 'nothing'}")

    # ------------------------------------------------------------ changes

    def load(self, model: str, *, timeout: float = LOAD_TIMEOUT) -> OmlxModelStatus:
        """Make ``model`` resident. Idempotent. Does not decide whether it *fits* —
        that is ``memory_budget.ensure_headroom``'s job, run before this by every
        caller in this repo — but verifies the result against the server's own
        status rather than trusting a 200."""
        row = self.resolve(model)
        if row.loaded:
            return row
        code, payload = self._call("POST", f"/v1/models/{urllib.parse.quote(row.id, safe='')}/load", timeout=timeout)
        if code != 200:
            raise OmlxError(f"oMLX refused to load {row.id}: HTTP {code} {self._detail(payload)}")
        return self._await(row.id, loaded=True, timeout=min(timeout, 60.0))

    def unload(self, model: str, *, settle: float = UNLOAD_SETTLE) -> OmlxModelStatus:
        """Drop ``model`` from memory. Idempotent; a pinned model is refused by oMLX
        itself and reported here, not retried around.

        "Unloaded" is checked twice: the model's own flag, and then the pool — the server's
        ``current_model_memory`` must fall by at least half the model's resident size within
        ``settle`` seconds. A server that reports the model gone but still holds its buffers is
        recorded in ``unreclaimed`` (and logged) rather than trusted: that memory is not back in
        macOS's hands until the server releases it or restarts."""
        status = self.status()
        row = self.resolve(model, status)
        if not row.loaded and not row.loading:
            return row
        before, size = status.memory_bytes, row.size_bytes
        code, payload = self._call("POST", f"/v1/models/{urllib.parse.quote(row.id, safe='')}/unload")
        if code != 200:
            raise OmlxError(f"oMLX refused to unload {row.id}: HTTP {code} {self._detail(payload)}")
        row = self._await(row.id, loaded=False, timeout=30.0)
        self._verify_reclaimed(row.id, before, size, settle)
        return row

    def _verify_reclaimed(self, model_id: str, before: int, size: int, settle: float) -> None:
        if size <= 0 or before <= 0:
            self.unreclaimed.pop(model_id, None)
            return
        deadline = time.monotonic() + max(0.0, settle)
        while True:
            try:
                now = self.status().memory_bytes
            except OmlxError:
                return  # cannot tell; the flag already said unloaded
            if before - now >= size // 2:
                self.unreclaimed.pop(model_id, None)
                return
            if time.monotonic() >= deadline:
                break
            time.sleep(0.2)
        held = max(0, size - max(0, before - now))
        self.unreclaimed[model_id] = held
        logger.warning("omlx: %s reads as unloaded but the pool still holds %.1f GB of it (restart oMLX to reclaim)", model_id, held / GB)

    def unload_all(self) -> list[OmlxModelStatus]:
        """Every loaded, unpinned model. The emergency path: keeps going past a
        failure so one stuck model cannot keep the rest resident."""
        dropped: list[OmlxModelStatus] = []
        for row in self.status().loaded:
            if row.pinned:
                continue
            try:
                dropped.append(self.unload(row.id))
            except OmlxError as error:
                logger.warning("omlx: could not unload %s: %s", row.id, error)
        return dropped

    def _await(self, model_id: str, *, loaded: bool, timeout: float) -> OmlxModelStatus:
        deadline = time.monotonic() + timeout
        row = self.resolve(model_id)
        while row.loaded != loaded or (loaded and row.loading):
            if time.monotonic() >= deadline:
                raise OmlxError(f"oMLX did not report {model_id} {'loaded' if loaded else 'unloaded'} within {timeout:g}s")
            time.sleep(0.1)
            row = self.resolve(model_id)
        return row

    @staticmethod
    def _detail(payload: Any) -> str:
        if isinstance(payload, dict):
            error = payload.get("error")
            if isinstance(error, dict):  # OpenAI-shaped: {"error": {"message": ...}}
                error = error.get("message")
            return str(payload.get("detail") or error or payload.get("message") or payload)[:400]
        return ""


_DEFAULT: OmlxManager | None = None


def default_manager() -> OmlxManager:
    global _DEFAULT
    if _DEFAULT is None or _DEFAULT.base_url != default_base_url():
        _DEFAULT = OmlxManager()
    return _DEFAULT

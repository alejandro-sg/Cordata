"""Local Apple-silicon inference through oMLX: the Prism models as a second Strands provider.

``MODEL_PROVIDER=local`` already points the *orchestrator* at oMLX. This is the other
thing: a **secondary provider instance** — ``strands.models.openai.OpenAIModel`` pointed
at the oMLX server (``OMLX_PRISM_BASE_URL``, falling back to ``OMLX_BASE_URL``) — that
the Meta-Harness routes *sub-tasks* to, whatever the orchestrator itself runs on, to save
cloud spend and latency on work a 2-bit 27B model does well:

* **Architect Planning** — the architect specialist's default engine;
* **Local Context Digest** — condensing bulk context before a cloud model sees it
  (``context_digest_tool``), and any A2A-graph node whose profile is ``digest``.

Registry (the spec's identifiers, with the server's own ids as the wire names — oMLX serves
``Ternary-Bonsai-2-27B-mlx-2bit``, not ``prism-ml/…``; the org prefix is the registry's):

    prism-ml/Ternary-Bonsai-2-27B-mlx-2bit   8.41 GB   architect
    prism-ml/Ternary-Bonsai-27B-mlx-2bit     8.30 GB   digest

Three guarantees, each enforced mechanically rather than asked of the model:

1. **A request never loads a model.** ``EndpointGuard`` lets exactly ``POST
   <base>/chat/completions`` through, so nothing built here can reach oMLX's load/unload
   routes; and ``PrismModel`` checks the model is *resident* before every request — oMLX
   would otherwise load an inactive model as a side effect, bypassing the memory budget.
   A request to a model that is not loaded raises ``ModelNotLoaded``.
2. **Load before delegating, unload after** — ``PrismLeaseHooks``: a refcounted lease taken
   (``memory_budget.load_omlx``: room first) when a Prism agent starts and released when it
   finishes; the last release unloads, *if this lease was what loaded it* (a model that was
   already resident — the orchestrator's own — is never unloaded out from under it).
3. **Fail over, don't fail.** Prism candidates sit in a Strands ``ModelRouter`` ahead of the
   provider the harness would have used anyway; a lease that could not be taken (oMLX down,
   no room) makes the Prism request refuse, and the router moves on to the cloud candidate.

Gate: ``omlx.prism`` (default on) — and oMLX must answer ``/health``; otherwise
``build_sub_agent_model`` returns exactly what it returned before this module existed.
"""

from __future__ import annotations

import asyncio
import logging
import os
import threading
import time
import weakref
from contextlib import asynccontextmanager
from dataclasses import dataclass
from typing import Any
from urllib.parse import urlparse

import httpx
import openai
from strands.hooks import AfterInvocationEvent, BeforeInvocationEvent, HookProvider, HookRegistry
from strands.models.openai import OpenAIModel
from strands.models.routing import FallbackStrategy, ModelRouter, RoutingCandidate, RoutingContext

import harness_events
import memory_budget
import omlx_manager

logger = logging.getLogger(__name__)

#: oMLX needs no key; the OpenAI client requires a non-empty one.
DUMMY_API_KEY = "not-needed"
DEFAULT_MAX_TOKENS = 16384
PRISM_ENV = "SIERRALAB_PRISM"  # on | off — wins over the omlx.prism setting (tests, CI)


class ModelNotLoaded(RuntimeError):
    """A Prism request was made for a model that is not resident in memory."""


class ForbiddenEndpoint(RuntimeError):
    """A request tried to reach anything but the chat-completions endpoint."""


class UnknownPrismModel(KeyError):
    pass


# ------------------------------------------------------------------ registry


@dataclass(frozen=True)
class PrismSpec:
    alias: str  # as the spec names it: org/name
    wire_id: str  # as the oMLX server names it
    disk_gb: float
    summary: str


REGISTRY: dict[str, PrismSpec] = {
    spec.alias: spec
    for spec in (
        PrismSpec("prism-ml/Ternary-Bonsai-2-27B-mlx-2bit", "Ternary-Bonsai-2-27B-mlx-2bit", 8.41, "Ternary Bonsai 2 27B, 2-bit — architect planning"),
        PrismSpec("prism-ml/Ternary-Bonsai-27B-mlx-2bit", "Ternary-Bonsai-27B-mlx-2bit", 8.30, "Ternary Bonsai 27B, 2-bit — local context digest"),
    )
}

#: Which model each local role runs on by default, and what the other is for.
ROLE_DEFAULTS: dict[str, str] = {
    "architect": "prism-ml/Ternary-Bonsai-2-27B-mlx-2bit",
    "digest": "prism-ml/Ternary-Bonsai-27B-mlx-2bit",
}
ROLES = tuple(ROLE_DEFAULTS)


def resolve(model: str) -> PrismSpec:
    """An alias (``prism-ml/…``), a bare server id, or a wire id → its spec."""
    if model in REGISTRY:
        return REGISTRY[model]
    for spec in REGISTRY.values():
        if model in (spec.wire_id, spec.alias.rsplit("/", 1)[-1]):
            return spec
    raise UnknownPrismModel(f"{model!r} is not a registered Prism model; known: {', '.join(REGISTRY)}")


def default_base_url() -> str:
    return omlx_manager.default_base_url()


# --------------------------------------------------------- availability gate


_probe: tuple[float, bool] | None = None
_PROBE_TTL = 5.0


def enabled(preferences: Any = None, manager: omlx_manager.OmlxManager | None = None) -> bool:
    """Should local roles run on Prism right now? The setting (or env override) says
    yes *and* oMLX answers. Cached for a few seconds: this runs once per sub-agent build."""
    global _probe
    forced = (os.environ.get(PRISM_ENV) or "").strip().lower()
    if forced == "off":
        return False
    if forced != "on":
        if preferences is None:
            try:
                import settings_store

                preferences = settings_store.current(os.getcwd())
            except Exception:
                preferences = None
        getter = getattr(preferences, "get", None)
        if getter is not None and getter("omlx.prism", True) is False:
            return False
    now = time.monotonic()
    if manager is None and _probe is not None and now - _probe[0] < _PROBE_TTL:
        return _probe[1]
    up = (manager or omlx_manager.OmlxManager()).healthy()
    if manager is None:
        _probe = (now, up)
    return up


# --------------------------------------------------------- guarded transport


class EndpointGuard(httpx.AsyncBaseTransport):
    """Exactly ``POST <base>/chat/completions``. oMLX's ``/v1/models/{id}/load`` is a
    different path, so no model built here can load or unload anything."""

    def __init__(self, base_url: str, inner: httpx.AsyncBaseTransport | None = None):
        parsed = urlparse(base_url)
        self.netloc = parsed.netloc
        self.allowed_path = parsed.path.rstrip("/") + "/chat/completions"
        self._inner = inner or httpx.AsyncHTTPTransport()

    async def handle_async_request(self, request: httpx.Request) -> httpx.Response:
        if request.method != "POST" or request.url.path != self.allowed_path or request.url.netloc.decode() != self.netloc:
            raise ForbiddenEndpoint(f"blocked {request.method} {request.url}: only POST {self.allowed_path} on {self.netloc} is allowed")
        return await self._inner.handle_async_request(request)

    async def aclose(self) -> None:
        await self._inner.aclose()


_listeners: list[Any] = []
_listeners_lock = threading.Lock()


def subscribe_requests(listener: Any) -> Any:
    """Be told what every Prism request carries: ``{"alias", "wire_id", "effort": None, "runtime": "omlx"}``."""
    with _listeners_lock:
        _listeners.append(listener)

    def unsubscribe() -> None:
        with _listeners_lock:
            if listener in _listeners:
                _listeners.remove(listener)

    return unsubscribe


def _notify(info: dict[str, Any]) -> None:
    with _listeners_lock:
        listeners = list(_listeners)
    for listener in listeners:
        try:
            listener(info)
        except Exception:
            logger.debug("prism request listener failed", exc_info=True)


# --------------------------------------------------------------------- model


class PrismModel(OpenAIModel):
    """A Prism model on the oMLX server, over ``/v1/chat/completions``.

    ``transport`` replaces the network layer (still behind ``EndpointGuard``): how a test
    intercepts the exact request. ``require_loaded`` (default) is guarantee 1 above.
    """

    def __init__(
        self,
        alias: str,
        *,
        base_url: str | None = None,
        transport: httpx.AsyncBaseTransport | None = None,
        manager: omlx_manager.OmlxManager | None = None,
        require_loaded: bool = True,
        **model_config: Any,
    ) -> None:
        self.spec = resolve(alias)
        self.base_url = (base_url or default_base_url()).rstrip("/")
        self._transport = transport
        self._manager = manager or omlx_manager.OmlxManager(self.base_url)
        self.require_loaded = require_loaded
        params = {"max_tokens": DEFAULT_MAX_TOKENS, **(model_config.pop("params", None) or {})}
        super().__init__(
            client_args={"api_key": DUMMY_API_KEY, "base_url": self.base_url},
            model_id=self.spec.wire_id,
            params=params,
            **model_config,
        )

    def _resident(self) -> bool:
        try:
            return self._manager.resolve(self.spec.wire_id).loaded
        except omlx_manager.OmlxError:
            return False

    async def stream(self, *args: Any, **kwargs: Any):  # type: ignore[override]
        if self.require_loaded and not await asyncio.to_thread(self._resident):
            raise ModelNotLoaded(
                f"{self.spec.alias} is not loaded in oMLX; load it first (omlx_resource_manager, or a PrismLease) — "
                "a chat request would make oMLX load it without checking the memory budget"
            )
        async for event in super().stream(*args, **kwargs):
            yield event

    def format_request(self, *args: Any, **kwargs: Any) -> dict[str, Any]:
        request = super().format_request(*args, **kwargs)
        _notify({"alias": self.spec.alias, "wire_id": self.spec.wire_id, "effort": None, "runtime": "omlx"})
        return request

    @asynccontextmanager
    async def _get_client(self):  # type: ignore[override]
        http_client = httpx.AsyncClient(
            transport=EndpointGuard(self.base_url, self._transport),
            follow_redirects=False,
            timeout=httpx.Timeout(600.0, connect=5.0),
        )
        async with openai.AsyncOpenAI(api_key=DUMMY_API_KEY, base_url=self.base_url, http_client=http_client) as client:
            yield client


# ------------------------------------------------------------------- routing


class PrismStrategy:
    """Opening candidate: ``invocation_state["prism_model"]``, else the pinned model, else the
    role's default — *unless that one is not resident and another Prism model is*: a lease
    that could not load the preferred model (this oMLX build may not support it) loaded the
    next one, and that is the one to ask. After a failed call: the healthiest remaining
    candidate (the cloud one)."""

    def __init__(self, role: str | None = None, pinned: str | None = None):
        self.role = role
        self.pinned = pinned
        self._fallback = FallbackStrategy()

    async def select(self, context: RoutingContext, **kwargs: Any) -> RoutingCandidate | None:
        if context.attempts:
            return await self._fallback.select(context, **kwargs)
        wanted = context.invocation_state.get("prism_model") or self.pinned or ROLE_DEFAULTS.get(self.role or "")
        if not wanted:
            return None
        try:
            alias = resolve(wanted).alias
        except UnknownPrismModel:
            return None
        ordered = sorted(context.candidates, key=lambda candidate: candidate.name != alias)
        prism = [candidate for candidate in ordered if isinstance(candidate.model, PrismModel)]
        for candidate in prism:
            if await asyncio.to_thread(candidate.model._resident):
                return candidate
        return prism[0] if prism and prism[0].name == alias else None


@dataclass
class PrismModels:
    """What ``build_models`` returns: hand ``router`` to ``Agent(model=...)``; ``aliases``
    are the Prism candidates in preference order (what a lease should hold)."""

    router: ModelRouter
    models: dict[str, PrismModel]
    role: str | None
    has_fallback: bool

    @property
    def primary(self) -> str:
        return next(iter(self.models))

    def lease_hooks(self, table: "LeaseTable | None" = None) -> "PrismLeaseHooks":
        return PrismLeaseHooks(list(self.models), table=table)


_BUILT: "weakref.WeakKeyDictionary[Any, PrismModels]" = weakref.WeakKeyDictionary()


def build_models(
    role: str | None = None,
    *,
    model: str | None = None,
    fallback: Any = None,
    base_url: str | None = None,
    transport: httpx.AsyncBaseTransport | None = None,
    manager: omlx_manager.OmlxManager | None = None,
) -> PrismModels:
    """Candidates for one agent: the role's Prism model first, the other Prism model next,
    then ``fallback`` (the model the harness would have used without Prism — a ``Model`` or
    a nested ``ModelRouter``) as the last resort."""
    preferred = resolve(model).alias if model else ROLE_DEFAULTS.get(role or "", next(iter(REGISTRY)))
    order = [preferred] + [alias for alias in REGISTRY if alias != preferred]
    models = {alias: PrismModel(alias, base_url=base_url, transport=transport, manager=manager) for alias in order}
    candidates = [RoutingCandidate(m, name=alias, description=m.spec.summary) for alias, m in models.items()]
    if fallback is not None:
        candidates.append(RoutingCandidate(fallback, name="fallback", description="the provider this role used before Prism"))
    router = ModelRouter(candidates, strategy=PrismStrategy(role, pinned=preferred if model else None))
    built = PrismModels(router, models, role, fallback is not None)
    _BUILT[router] = built
    return built


def hooks_for(model: Any) -> list[HookProvider]:
    """The lease hooks a Prism router needs (none for any other model). Whoever builds an
    Agent from ``build_sub_agent_model`` adds these to it."""
    built = _BUILT.get(model)
    return [built.lease_hooks()] if built is not None else []


# --------------------------------------------------------------------- leases


@dataclass
class Lease:
    alias: str
    wire_id: str
    holders: int = 0
    loaded_by_us: bool = False
    error: str | None = None


class LeaseTable:
    """Refcounted loads. Two graph nodes sharing one Prism model load it once; the last
    one out unloads it — but only if the lease is what loaded it."""

    def __init__(self, budget: memory_budget.MemoryBudget | None = None) -> None:
        self._budget = budget
        self._lock = threading.Lock()
        self._leases: dict[str, Lease] = {}

    @property
    def budget(self) -> memory_budget.MemoryBudget:
        return self._budget or memory_budget.default_budget()

    def acquire(self, alias: str) -> Lease:
        """Blocking (a load takes seconds). Never raises: a lease that could not be taken
        comes back with ``error`` set, and the Prism model then refuses its requests."""
        spec = resolve(alias)
        with self._lock:
            lease = self._leases.setdefault(spec.alias, Lease(spec.alias, spec.wire_id))
            lease.holders += 1
            if lease.holders > 1:
                return lease
            try:
                row = self.budget.omlx.resolve(spec.wire_id)
                if row.loaded:
                    lease.loaded_by_us = False  # someone else's (the orchestrator's own model, say): not ours to unload
                else:
                    loaded, room = self.budget.load_omlx(spec.wire_id)
                    lease.loaded_by_us = True
                    harness_events.emit(
                        "memory",
                        {"event": "lease_load", "runtime": "omlx", "model": spec.alias, "size_gb": round(loaded.size_gb, 2), "freed": [r.name for r in (room.freed if room else [])]},
                    )
                lease.error = None
            except (memory_budget.MemoryBudgetExceeded, omlx_manager.OmlxError) as error:
                lease.error = str(error)
                logger.warning("prism: could not lease %s: %s", spec.alias, error)
                harness_events.emit("memory", {"event": "lease_failed", "runtime": "omlx", "model": spec.alias, "reason": str(error)[:300]})
            return lease

    def release(self, alias: str) -> None:
        spec = resolve(alias)
        with self._lock:
            lease = self._leases.get(spec.alias)
            if lease is None or lease.holders == 0:
                return
            lease.holders -= 1
            if lease.holders > 0:
                return
            owned, lease.loaded_by_us = lease.loaded_by_us, False
            if not owned:
                return
            try:
                self.budget.omlx.unload(spec.wire_id)
                harness_events.emit("memory", {"event": "lease_unload", "runtime": "omlx", "model": spec.alias})
            except omlx_manager.OmlxError as error:
                logger.warning("prism: could not unload %s: %s", spec.alias, error)

    def held(self) -> dict[str, int]:
        with self._lock:
            return {alias: lease.holders for alias, lease in self._leases.items() if lease.holders}


_TABLE: LeaseTable | None = None
_TABLE_LOCK = threading.Lock()


def default_table() -> LeaseTable:
    global _TABLE
    with _TABLE_LOCK:
        if _TABLE is None:
            _TABLE = LeaseTable()
        return _TABLE


class PrismLeaseHooks(HookProvider):
    """Load a Prism model before this agent runs; unload it after (guarantee 2).

    ``aliases`` are the candidates in preference order: the first that can be leased is the
    one held, so a model this oMLX build cannot load (or that does not fit) falls through to
    the next Prism model before the router ever reaches the cloud. Per *invocation*, not per
    model call: a tool-using loop of ten model calls is one lease. Async, so a multi-second
    load does not stall the event loop the ACP server shares."""

    def __init__(self, aliases: "str | list[str]", *, table: LeaseTable | None = None) -> None:
        self.aliases = [aliases] if isinstance(aliases, str) else list(aliases)
        self._table = table
        self._held: list[str] = []
        self._lock = threading.Lock()

    @property
    def alias(self) -> str:
        return self.aliases[0]

    @property
    def table(self) -> LeaseTable:
        return self._table or default_table()

    def register_hooks(self, registry: HookRegistry, **kwargs: Any) -> None:
        registry.add_callback(BeforeInvocationEvent, self._before)
        registry.add_callback(AfterInvocationEvent, self._after)

    def _lease_first(self) -> str | None:
        """The first alias that leases cleanly. A failed attempt is released again (it
        counted as a holder) and the next is tried; None when none could be leased."""
        for alias in self.aliases:
            lease = self.table.acquire(alias)
            if lease.error is None:
                return alias
            self.table.release(alias)
        return None

    async def _before(self, event: BeforeInvocationEvent) -> None:
        if getattr(event, "cancel", False):
            return
        held = await asyncio.to_thread(self._lease_first)
        with self._lock:
            self._held.append(held or "")

    async def _after(self, event: AfterInvocationEvent) -> None:
        with self._lock:
            if not self._held:
                return
            alias = self._held.pop()
        if alias:
            await asyncio.to_thread(self.table.release, alias)

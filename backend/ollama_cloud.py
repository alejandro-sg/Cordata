"""Ollama Cloud models through the local Ollama daemon's OpenAI-compatible proxy.

The daemon (``ollama serve``, signed in with ``ollama signin``) exposes
``/v1/chat/completions`` on localhost:11434 and forwards ``<model>:cloud`` tags to
Ollama's hosted inference. This module is the one place that knows how to talk to
it from Strands:

* **Registry** — the model tags this harness routes between, with the *wire id*
  the daemon wants. Verified live against ollama 0.35: the bare tag
  (``glm-5.3-flash``) is a 404 on the daemon; only ``glm-5.3-flash:cloud`` is
  offloaded to the cloud. ``laguna-xs-2.1`` has no ``:cloud`` tag at all — it is a
  20 GB local-weights model — so it is registered as ``local`` and is only usable if
  it is *already* on disk. This module never pulls.
* **OllamaCloudModel** — ``strands.models.openai.OpenAIModel`` pointed at the daemon
  with a dummy key, whose HTTP transport refuses anything but
  ``POST <base>/chat/completions`` (so no code path here can reach ``/api/pull``)
  and which stamps ``reasoning_effort`` into every request via ``extra_body``.
* **Reasoning throttle** — ``ReasoningControl`` is the mutable level a model reads
  at request time; ``ReasoningThrottle`` is the Strands hook that re-sets it before
  each model call from a policy, so a sub-agent's test-time compute follows its role.
* **Routing** — ``build_models`` wraps the candidates in Strands' native
  ``ModelRouter`` with ``RoleStrategy``: the opening candidate is the one the
  invocation (or the agent's role) asks for, failures fall through to the others.

Not a secret store: the key is the literal string ``"ollama"`` (the daemon ignores
it and authenticates to the cloud with its own signed-in identity), so nothing here
touches the Keychain or ``secret_guard``.
"""

from __future__ import annotations

import json
import logging
import os
import threading
import urllib.error
import urllib.request
from collections.abc import Callable, Mapping
from contextlib import asynccontextmanager
from dataclasses import dataclass
from typing import Any, Literal
from urllib.parse import urlparse

import httpx
import openai
from strands.hooks import BeforeModelCallEvent, HookProvider, HookRegistry
from strands.models.openai import OpenAIModel
from strands.models.routing import FallbackStrategy, ModelRouter, RoutingCandidate, RoutingContext

logger = logging.getLogger(__name__)

DEFAULT_BASE_URL = "http://localhost:11434/v1"
#: The daemon ignores the bearer token; the OpenAI client just requires a non-empty one.
DUMMY_API_KEY = "ollama"
#: Same reasoning as agent_common._DEFAULT_MAX_TOKENS — thinking tokens count against it.
DEFAULT_MAX_TOKENS = 16384
#: See `agent_common.UNKNOWN_CONTEXT_WINDOW`; `OLLAMA_CLOUD_CONTEXT_WINDOW` overrides it.
DEFAULT_CONTEXT_WINDOW = 128_000

REASONING_LEVELS = ("low", "medium", "high", "max")
ReasoningEffort = Literal["low", "medium", "high", "max"]


def default_base_url() -> str:
    return (os.getenv("OLLAMA_BASE_URL") or DEFAULT_BASE_URL).rstrip("/")


# ------------------------------------------------------------------ registry


@dataclass(frozen=True)
class ModelSpec:
    alias: str
    wire_id: str
    hosting: Literal["cloud", "local"]
    summary: str

    @property
    def pulls_weights(self) -> bool:
        """True when serving this tag needs weights on the machine."""
        return self.hosting == "local"


REGISTRY: dict[str, ModelSpec] = {
    spec.alias: spec
    for spec in (
        ModelSpec("glm-5.3-flash", "glm-5.3-flash:cloud", "cloud", "GLM 5.3 Flash — fast general model (18B active)"),
        ModelSpec("kimi-k2.7-code", "kimi-k2.7-code:cloud", "cloud", "Kimi K2.7 Code — long-horizon coding"),
        ModelSpec(
            "laguna-xs-2.1",
            "laguna-xs-2.1",
            "local",
            "Laguna XS 2.1 — 33B MoE / 3B active; local weights only (no :cloud tag), never pulled by this harness",
        ),
    )
}
CLOUD_ALIASES = tuple(alias for alias, spec in REGISTRY.items() if spec.hosting == "cloud")


class UnknownModel(KeyError):
    pass


class ModelUnavailable(RuntimeError):
    """The tag cannot be served right now. Raised instead of pulling weights."""


class ForbiddenEndpoint(RuntimeError):
    """A request tried to reach anything but the chat-completions endpoint."""


def resolve(model: str) -> ModelSpec:
    """An alias (``glm-5.3-flash``) or a wire id (``glm-5.3-flash:cloud``) → its spec."""
    if model in REGISTRY:
        return REGISTRY[model]
    for spec in REGISTRY.values():
        if model == spec.wire_id:
            return spec
    raise UnknownModel(f"{model!r} is not a registered Ollama model; known: {', '.join(REGISTRY)}")


def installed_local_tags(base_url: str | None = None, *, timeout: float = 3.0) -> set[str]:
    """Names the daemon already has on disk (``GET /api/tags``). Read-only — the one
    non-chat request this module makes, and it never goes through the model's guarded
    transport. Empty set when the daemon is unreachable."""
    root = (base_url or default_base_url()).removesuffix("/v1")
    try:
        with urllib.request.urlopen(f"{root}/api/tags", timeout=timeout) as response:  # noqa: S310 - local daemon
            payload = json.load(response)
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError, OSError):
        return set()
    names: set[str] = set()
    for entry in payload.get("models", []):
        name = entry.get("name") or entry.get("model") or ""
        if name:
            names.add(name)
            names.add(name.removesuffix(":latest"))
    return names


def daemon_problem(base_url: str | None = None, *, timeout: float = 3.0) -> str | None:
    """Why the local daemon can't serve chat right now, or None when it answers."""
    root = (base_url or default_base_url()).removesuffix("/v1")
    try:
        with urllib.request.urlopen(f"{root}/api/version", timeout=timeout):  # noqa: S310 - local daemon
            return None
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        return f"the Ollama daemon is not reachable at {root} ({error}). Start it with `ollama serve` and sign in with `ollama signin`."


Availability = Callable[[ModelSpec], str | None]


def routable(base_url: str | None = None, availability: "Availability | None" = None) -> list[ModelSpec]:
    """The registry models usable right now, in registry order: every cloud tag,
    plus a local-weights tag only if it is already installed. What a model picker
    should offer."""
    check = availability or default_availability(base_url)
    return [spec for spec in REGISTRY.values() if check(spec) is None]


def default_availability(base_url: str | None = None) -> Availability:
    """Cloud tags are always routable; a local-only tag only if the daemon already has
    it. Returns a reason string when a tag is not usable, ``None`` when it is."""
    installed: set[str] | None = None

    def check(spec: ModelSpec) -> str | None:
        nonlocal installed
        if spec.hosting == "cloud":
            return None
        if installed is None:
            installed = installed_local_tags(base_url)
        if spec.wire_id in installed:
            return None
        return (
            f"{spec.alias} has no :cloud tag and is not installed locally; this harness never runs `ollama pull`. "
            f"Pull it yourself (`ollama pull {spec.wire_id}`, ~20 GB) to enable it."
        )

    return check


# ------------------------------------------------------- reasoning throttle


def normalize_effort(level: str | None) -> ReasoningEffort | None:
    if level is None:
        return None
    value = str(level).strip().lower()
    if value not in REASONING_LEVELS:
        raise ValueError(f"reasoning_effort must be one of {REASONING_LEVELS}, got {level!r}")
    return value  # type: ignore[return-value]


class ReasoningControl:
    """The reasoning_effort a model stamps into its next request. Shared by every
    candidate of one router so a throttle applies whichever model gets picked.
    ``None`` omits the field (provider default)."""

    def __init__(self, level: str | None = None, *, on_change: Callable[[str | None, str | None], None] | None = None):
        self._lock = threading.Lock()
        self._level = normalize_effort(level)
        self.on_change = on_change

    @property
    def level(self) -> ReasoningEffort | None:
        with self._lock:
            return self._level

    def set(self, level: str | None) -> ReasoningEffort | None:
        new = normalize_effort(level)
        with self._lock:
            old, self._level = self._level, new
        if old != new and self.on_change:
            try:
                self.on_change(old, new)
            except Exception:  # telemetry must never break a model call
                logger.debug("reasoning on_change failed", exc_info=True)
        return new


# --------------------------------------------------------- guarded transport


class EndpointGuard(httpx.AsyncBaseTransport):
    """Lets exactly ``POST <base>/chat/completions`` through. A weights download is
    ``POST /api/pull`` (or an implicit pull behind another route); neither can leave
    this process through a model built here."""

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


# ---------------------------------------------------------- request listeners

_request_listeners: list[Callable[[dict[str, Any]], None]] = []
_listeners_lock = threading.Lock()


def subscribe_requests(listener: Callable[[dict[str, Any]], None]) -> Callable[[], None]:
    """Be told what every request is about to carry: ``{"alias", "wire_id", "effort"}``.
    That is the model that actually runs — behind a router it is not ``agent.model``."""
    with _listeners_lock:
        _request_listeners.append(listener)

    def unsubscribe() -> None:
        with _listeners_lock:
            if listener in _request_listeners:
                _request_listeners.remove(listener)

    return unsubscribe


def _notify_request(info: dict[str, Any]) -> None:
    with _listeners_lock:
        listeners = list(_request_listeners)
    for listener in listeners:
        try:
            listener(info)
        except Exception:  # telemetry must never fail a model call
            logger.debug("request listener failed", exc_info=True)


# --------------------------------------------------------------------- model


class OllamaCloudModel(OpenAIModel):
    """An Ollama model behind the local daemon's OpenAI proxy.

    ``transport`` replaces the network layer (still behind ``EndpointGuard``) — how
    tests intercept the exact HTTP request without a daemon.
    """

    def __init__(
        self,
        alias: str,
        *,
        base_url: str | None = None,
        reasoning: ReasoningControl | None = None,
        reasoning_effort: str | None = None,
        transport: httpx.AsyncBaseTransport | None = None,
        **model_config: Any,
    ) -> None:
        self.spec = resolve(alias)
        self.base_url = (base_url or default_base_url()).rstrip("/")
        self.reasoning = reasoning if reasoning is not None else ReasoningControl()
        if reasoning_effort is not None:
            self.reasoning.set(reasoning_effort)
        self._transport = transport
        params = {"max_tokens": DEFAULT_MAX_TOKENS, **(model_config.pop("params", None) or {})}
        # The window is set by the hosted model, not by this process. A planning number rather than a fact (see
        # `agent_common.UNKNOWN_CONTEXT_WINDOW`) so Strands' proactive compression has something to measure against.
        model_config.setdefault("context_window_limit", int(os.getenv("OLLAMA_CLOUD_CONTEXT_WINDOW") or DEFAULT_CONTEXT_WINDOW))
        super().__init__(
            client_args={"api_key": DUMMY_API_KEY, "base_url": self.base_url},
            model_id=self.spec.wire_id,
            params=params,
            **model_config,
        )

    # --- reasoning throttle
    @property
    def reasoning_effort(self) -> ReasoningEffort | None:
        return self.reasoning.level

    def set_reasoning_effort(self, level: str | None) -> ReasoningEffort | None:
        """Takes effect on the next request — between agent-loop iterations included."""
        return self.reasoning.set(level)

    # --- model switching
    def switch_model(self, alias: str, *, availability: Availability | None = None) -> ModelSpec:
        """Point this instance at another registered model for its next request."""
        spec = resolve(alias)
        reason = (availability or default_availability(self.base_url))(spec)
        if reason:
            raise ModelUnavailable(reason)
        self.spec = spec
        self.update_config(model_id=spec.wire_id)
        return spec

    @property
    def current_spec(self) -> ModelSpec:
        """The model the next request names — read from the live config, because
        ``update_config(model_id=...)`` (the model picker, ``set_active_model``)
        changes it without going through ``switch_model``."""
        try:
            return resolve(str(self.config.get("model_id") or self.spec.wire_id))
        except UnknownModel:
            return self.spec

    # --- request shape
    def format_request(self, *args: Any, **kwargs: Any) -> dict[str, Any]:
        request = super().format_request(*args, **kwargs)
        effort = self.reasoning.level
        if effort is not None:
            # extra_body is the SDK's escape hatch for fields its enum doesn't know
            # ("max"); it lands at the top level of the JSON body.
            request["extra_body"] = {**(request.get("extra_body") or {}), "reasoning_effort": effort}
        spec = self.current_spec
        _notify_request({"alias": spec.alias, "wire_id": spec.wire_id, "effort": effort})
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


class ReasoningThrottle(HookProvider):
    """Re-sets the reasoning effort before every model call.

    ``policy(event)`` returns the level for this step, or ``None`` to leave it
    unchanged. A plain string is a constant level (the role's budget).
    """

    def __init__(self, control: ReasoningControl, policy: str | Callable[[BeforeModelCallEvent], str | None]):
        self.control = control
        self.policy = (lambda _event: policy) if isinstance(policy, str) else policy

    def register_hooks(self, registry: HookRegistry, **kwargs: Any) -> None:
        registry.add_callback(BeforeModelCallEvent, self._before_model)

    def _before_model(self, event: BeforeModelCallEvent) -> None:
        level = self.policy(event)
        if level is not None:
            self.control.set(level)


# --------------------------------------------------------------------- roles


@dataclass(frozen=True)
class RoleProfile:
    role: str
    model: str
    effort: ReasoningEffort


#: Test-time-compute budget by job. The Debugger gets the most (it has to find a root
#: cause), the Observer the least (it runs unattended on a timer, read-only).
DEFAULT_ROLES: dict[str, RoleProfile] = {
    profile.role: profile
    for profile in (
        RoleProfile("orchestrator", "glm-5.3-flash", "low"),
        RoleProfile("architect", "glm-5.3-flash", "high"),
        RoleProfile("debugger", "kimi-k2.7-code", "max"),
        RoleProfile("observer", "glm-5.3-flash", "low"),
    )
}


class RoleTable:
    """Role → (model, effort), changeable at runtime. A delegation can override either
    for one call (``build_models(role, model=..., effort=...)``) without touching this."""

    def __init__(self, profiles: Mapping[str, RoleProfile] | None = None):
        self._lock = threading.Lock()
        self._profiles = dict(profiles or DEFAULT_ROLES)
        for role, text in _env_overrides().items():
            self._profiles[role] = _parse_override(role, text, self._profiles.get(role))

    def get(self, role: str | None) -> RoleProfile | None:
        with self._lock:
            return self._profiles.get((role or "").lower())

    def set(self, role: str, *, model: str | None = None, effort: str | None = None) -> RoleProfile:
        with self._lock:
            current = self._profiles.get(role.lower()) or RoleProfile(role.lower(), CLOUD_ALIASES[0], "medium")
            profile = RoleProfile(
                current.role,
                resolve(model).alias if model else current.model,
                normalize_effort(effort) or current.effort,
            )
            self._profiles[profile.role] = profile
            return profile

    def snapshot(self) -> dict[str, RoleProfile]:
        with self._lock:
            return dict(self._profiles)


def _env_overrides() -> dict[str, str]:
    prefix = "OLLAMA_ROLE_"
    return {key[len(prefix):].lower(): value for key, value in os.environ.items() if key.startswith(prefix) and value.strip()}


def _parse_override(role: str, text: str, current: RoleProfile | None) -> RoleProfile:
    """``OLLAMA_ROLE_DEBUGGER=kimi-k2.7-code:max`` — ``model``, ``model:effort`` or ``:effort``
    (aliases never contain a colon, so the first one splits)."""
    model, _, effort = text.strip().partition(":")
    base = current or RoleProfile(role, CLOUD_ALIASES[0], "medium")
    return RoleProfile(role, resolve(model).alias if model else base.model, normalize_effort(effort or None) or base.effort)


ROLES = RoleTable()


# ------------------------------------------------------------------- routing


class RoleStrategy:
    """Picks the opening candidate and handles failover.

    Opening choice, first match wins: ``invocation_state["ollama_model"]`` (one call
    asks for a model), ``pinned`` (this agent was built with an explicit model), the
    agent's role profile (``agent.name``, read live so ``RoleTable.set`` applies), else
    decline — the router then serves its first declared candidate. After a failed call
    it defers to ``FallbackStrategy``: the healthiest remaining candidate.
    """

    def __init__(self, roles: RoleTable | None = None, pinned: str | None = None):
        self.roles = roles or ROLES
        self.pinned = pinned
        self._fallback = FallbackStrategy()

    async def select(self, context: RoutingContext, **kwargs: Any) -> RoutingCandidate | None:
        if context.attempts:
            return await self._fallback.select(context, **kwargs)
        wanted = context.invocation_state.get("ollama_model") or self.pinned
        if not wanted:
            agent = context.invocation_state.get("agent")
            profile = self.roles.get(getattr(agent, "name", None))
            wanted = profile.model if profile else None
        if not wanted:
            return None
        try:
            alias = resolve(wanted).alias
        except UnknownModel:
            return None
        return next((candidate for candidate in context.candidates if candidate.name == alias), None)


@dataclass
class OllamaModels:
    """What ``build_models`` returns: hand ``router`` to ``Agent(model=...)``;
    ``control`` throttles reasoning for whichever candidate runs."""

    router: ModelRouter
    control: ReasoningControl
    models: dict[str, OllamaCloudModel]
    unavailable: dict[str, str]
    role: str | None = None

    @property
    def default(self) -> OllamaCloudModel:
        return self.router.default_model  # type: ignore[return-value]

    def throttle(self, policy: str | Callable[[BeforeModelCallEvent], str | None]) -> ReasoningThrottle:
        return ReasoningThrottle(self.control, policy)


def build_models(
    role: str | None = None,
    *,
    model: str | None = None,
    effort: str | None = None,
    aliases: tuple[str, ...] | None = None,
    base_url: str | None = None,
    transport: httpx.AsyncBaseTransport | None = None,
    availability: Availability | None = None,
    roles: RoleTable | None = None,
    on_effort_change: Callable[[str | None, str | None], None] | None = None,
) -> OllamaModels:
    """Candidates for one agent: the registered models that are usable right now,
    wrapped in Strands' ``ModelRouter``.

    ``model``/``effort`` override the role's profile for this agent only — the hook for
    swapping a specialist's model when it is created or when work is delegated to it.
    The preferred model is declared first so it is also the router's default (and
    ``agent.model``). A candidate that is not usable (a local-only tag that is not on
    disk) is left out and listed in ``unavailable`` rather than failing the build —
    unless it is the one explicitly asked for, which raises ``ModelUnavailable``.
    """
    table = roles or ROLES
    profile = table.get(role)
    preferred = resolve(model).alias if model else (profile.model if profile else CLOUD_ALIASES[0])
    level = normalize_effort(effort) or (profile.effort if profile else None)
    check = availability or default_availability(base_url)

    order = [preferred] + [alias for alias in (aliases or tuple(REGISTRY)) if alias != preferred]
    control = ReasoningControl(level, on_change=on_effort_change)
    models: dict[str, OllamaCloudModel] = {}
    unavailable: dict[str, str] = {}
    for alias in order:
        spec = resolve(alias)
        reason = check(spec)
        if reason:
            if alias == preferred:
                raise ModelUnavailable(reason)
            unavailable[alias] = reason
            continue
        models[alias] = OllamaCloudModel(alias, base_url=base_url, reasoning=control, transport=transport)

    router = ModelRouter(
        [RoutingCandidate(m, name=alias, description=m.spec.summary) for alias, m in models.items()],
        strategy=RoleStrategy(table, pinned=resolve(model).alias if model else None),
    )
    return OllamaModels(router, control, models, unavailable, role)


def build_single(
    role: str | None = None,
    *,
    model: str | None = None,
    effort: str | None = None,
    base_url: str | None = None,
    roles: RoleTable | None = None,
    **model_config: Any,
) -> OllamaCloudModel:
    """One model, no router — for callers that hold ``agent.model`` and mutate it
    (``update_config``, the ``/model`` switch). Checks nothing over the network."""
    table = roles or ROLES
    profile = table.get(role)
    alias = resolve(model).alias if model else (profile.model if profile else CLOUD_ALIASES[0])
    level = normalize_effort(effort) or (profile.effort if profile else None)
    return OllamaCloudModel(alias, base_url=base_url, reasoning_effort=level, **model_config)

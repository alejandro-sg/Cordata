"""Reasoning effort that follows the work: Ollama Cloud's ``reasoning_effort``, chosen per model call.

The orchestrator's role runs at ``low`` effort (``ollama_cloud.DEFAULT_ROLES``) because most turns are small. A hard
change in a large monorepo is not, and a fixed level is wrong for one of the two. This hook re-reads two things before
every model call and sets the level the request will carry (``ollama_cloud.ReasoningControl``, through the SDK's own
``BeforeModelCallEvent``):

* the **project's size class** (``env_profile.py``: small / medium / large, cached), and
* **Laya's reading of this turn's complexity** (the router's ``RouteDecision.tier``: fast / standard / strong — or no
  reading, when Laya abstained or is not loaded).

    ==========  ========  ==========  ========
    complexity  small     medium      large
    ==========  ========  ==========  ========
    fast        base      base        base
    standard    base      medium      medium
    strong      medium    high        high
    unknown     base      base        medium
    ==========  ========  ==========  ========

``base`` is the role's level as it stands *now* (``ollama_cloud.ROLES``) — which is where the Lab's effort dial and an
``OLLAMA_ROLE_ORCHESTRATOR=…:high`` override land — and the table only ever **raises** it: a person who chose
``high`` is never moved to ``medium``, and nothing here reaches ``max`` unless the person chose it. A model without the
knob (local, Anthropic, OpenAI-compatible) is left alone. ``reasoning.adaptive: false`` keeps the role's level.
"""

from __future__ import annotations

import logging
from typing import Any, Callable

from strands.hooks import BeforeModelCallEvent, HookProvider, HookRegistry

logger = logging.getLogger(__name__)

ORDER = ("low", "medium", "high", "max")
TABLE: dict[tuple[str, str], str | None] = {
    ("fast", "small"): None, ("fast", "medium"): None, ("fast", "large"): None,
    ("standard", "small"): None, ("standard", "medium"): "medium", ("standard", "large"): "medium",
    ("strong", "small"): "medium", ("strong", "medium"): "high", ("strong", "large"): "high",
    ("unknown", "small"): None, ("unknown", "medium"): None, ("unknown", "large"): "medium",
}


def choose(base: str | None, complexity: str | None, size: str) -> str | None:
    """The level for this call: the table's suggestion, never below ``base``. None leaves the request as it is."""
    suggestion = TABLE.get((complexity or "unknown", size))
    if base is None and suggestion is None:
        return None
    if base is None:
        return suggestion
    if suggestion is None or ORDER.index(suggestion) <= ORDER.index(base):
        return base
    return suggestion


def _controls(model: Any) -> list[Any]:
    """Every ReasoningControl behind a model (a router's candidates share one)."""
    try:
        from lab import _reasoning_controls  # noqa: PLC0415 — the Lab already knows how to find them

        return list(_reasoning_controls(model))
    except Exception:
        control = getattr(model, "reasoning", None)
        return [control] if control is not None and hasattr(control, "set") else []


class AdaptiveEffort(HookProvider):
    """One per orchestrator agent (module docstring)."""

    def __init__(
        self,
        root: str,
        *,
        complexity: Callable[[Any], str | None],
        enabled: Callable[[], bool],
        role: str = "orchestrator",
        notify: Callable[[dict], None] | None = None,
    ) -> None:
        self.root = root
        self._complexity = complexity
        self._enabled = enabled
        self.role = role
        self._notify = notify
        self.last: str | None = None

    def register_hooks(self, registry: HookRegistry, **kwargs: Any) -> None:
        registry.add_callback(BeforeModelCallEvent, self._before_model)

    def _base(self) -> str | None:
        try:
            import ollama_cloud  # noqa: PLC0415

            profile = ollama_cloud.ROLES.get(self.role)
            return profile.effort if profile is not None else None
        except Exception:
            return None

    def level_for(self, agent: Any) -> str | None:
        import env_profile  # noqa: PLC0415

        return choose(self._base(), self._complexity(agent), env_profile.size_class(self.root))

    def _before_model(self, event: BeforeModelCallEvent) -> None:
        try:
            if not self._enabled():
                return
            controls = _controls(getattr(event.agent, "model", None))
            if not controls:
                return
            level = self.level_for(event.agent)
            if level is None:
                return
            for control in controls:
                control.set(level)
            if level != self.last:
                self.last = level
                if self._notify is not None:
                    self._notify({"event": "effort", "role": self.role, "effort": level, "source": "adaptive"})
        except Exception:
            logger.debug("adaptive effort: could not set the level", exc_info=True)

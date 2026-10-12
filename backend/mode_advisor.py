"""Adaptive mode: when a request should start in plan, decided before any tool runs.

The session has three modes (plan / build / auto, ``acp_approval._MODES``). Adaptive mode lets the harness *tighten*
the one in force, never loosen it:

* a request that is a design question, an explicit "plan this", or a large cross-cutting change → **plan** first;
* nothing here ever moves a session toward *auto*. Leaving plan is the person's answer to the plan card
  (``plan_mode.py``), and ``auto`` is only ever entered by the person (Tab, ``/mode``, an approved plan).

The advisor is a pure function of the message and Laya's route, so it is cheap (no model call, no tokens) and testable.
It is deliberately conservative: a short message, a continuation ("yes", "go ahead") or a plain question gets no advice,
and the caller leaves the session alone after the person has changed the mode by hand (``HOLD_TURNS``).
"""

from __future__ import annotations

import re
from dataclasses import dataclass

#: After the person picks a mode themselves, adaptive mode stays out of the way for this many turns.
HOLD_TURNS = 8
#: Advice below this is not acted on.
MIN_CONFIDENCE = 0.7

#: Laya routes that mean "a design or multi-part job".
PLAN_ROUTES = frozenset({"architect", "team"})

_WORDS = lambda text: len(text.split())  # noqa: E731

_CONTINUATION = re.compile(r"^\s*(yes|yep|y|ok(ay)?|go( ahead| on)?|do it|continue|proceed|sure|approved?|lgtm|ship it|build it|run it|next|keep going)\b[\s.!]*$", re.I)

#: The person asked for a plan, or asked that nothing change yet.
_EXPLICIT = re.compile(
    r"\bplan (this|it|that|out|first)\b|\bplan mode\b|\b(make|write|draft|create|give me|come up with|outline|propose)\b.{0,24}\b(a |the |an )?(plan|proposal|roadmap|design|strategy)\b"
    r"|\bbefore (you |we )?(change|edit|touch|write|implement|run|start|build|code)\b|\bdon'?t (change|edit|touch|modify|write|run|implement) anything\b"
    r"|\bwithout (changing|editing|modifying|touching)\b|\bthink (it |this |that )?(through|over)\b|\bhow (should|would|could) (we|i|you) (approach|go about|structure|design|architect|tackle)\b"
    r"|\bwhat('s| is) the best (way|approach)\b|\b(trade-?offs?|pros and cons)\b",
    re.I,
)

#: A large or cross-cutting change.
_BIG = re.compile(
    r"\b(refactor|rewrite|re-?architect|redesign|overhaul|migrate|migration|port (it|this|the)|convert (the|all|every)|restructure|rework)\b"
    r"|\b(across|throughout|every|all)\b.{0,30}\b(files?|modules?|repo|repository|codebase|project|services?|tests?|crates?|packages?)\b"
    r"|\bfrom scratch\b|\bend[- ]to[- ]end\b"
    r"|\bimplement\b.{0,40}\b(system|framework|engine|pipeline|service|platform|dashboard|subsystem)\b",
    re.I,
)


@dataclass(frozen=True)
class Advice:
    mode: str  # only "plan" today
    reason: str
    confidence: float

    def to_event(self) -> dict:
        return {"mode": self.mode, "reason": self.reason, "confidence": round(self.confidence, 2)}


def advise(text: str, *, mode: str, route: str | None = None) -> Advice | None:
    """Advice for a turn that opens in ``mode``, or None to leave things as they are."""
    if mode == "plan":
        return None  # already as tight as it gets
    message = (text or "").strip()
    if not message or message.startswith(("/", "!")) or _CONTINUATION.match(message):
        return None
    if _EXPLICIT.search(message):
        return Advice("plan", "you asked for a plan (or for nothing to change yet)", 0.9)
    big = len(_BIG.findall(message))
    long = _WORDS(message) >= 30
    if route in PLAN_ROUTES and (big or long):
        return Advice("plan", f"Laya routed this as a {route} job — design before editing", 0.8)
    if big >= 2 or (big and long):
        return Advice("plan", "a large change across the project — plan before editing", 0.75)
    return None


def should_apply(advice: Advice | None, *, mode: str, held: int) -> bool:
    """Whether to move the session to ``advice.mode`` now. Never loosens, never overrides a recent manual choice."""
    return advice is not None and advice.mode == "plan" and mode != "plan" and held <= 0 and advice.confidence >= MIN_CONFIDENCE

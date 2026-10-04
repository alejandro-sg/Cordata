"""A continuous 0.0-1.0 destructiveness score on top of Laya's risk reading.

Laya answers `risk` as a *choice* over three levels — `read_only`, `edits_files`,
`destructive` (`laya_router.RISK_LEVELS`) — plus a confidence in whichever it
picked. That is an ordinal label, not a number, and the approval gate already
uses it that way: `RouteDecision.armed` is exactly `destructive` at >= 0.8
(`laya_router.py:155-156`), and that gate stays authoritative. Nothing in this
module changes it.

What this adds is a single float in the same units as the gate's implicit
thresholds, so a policy can express "below this, let it run" and "above this,
always ask" without re-deriving the same comparison in three places, and so the
score can be logged and compared across turns.

The mapping. The ordinal is normalised to 0.0 / 0.5 / 1.0, then shrunk toward
0.5 by Laya's confidence in that reading:

    score = 0.5 + (ordinal/2 - 0.5) * confidence

so confidence is what moves the score *away* from "no information". A confident
`read_only` lands near 0.0 and a confident `destructive` near 1.0, which is the
intuitive reading; an unconfident anything lands near 0.5, which is the honest
one — with a flat answer distribution Laya has not actually said this is safe,
and the score should not claim it did. That is the same failure-closed posture
the router takes when it abstains.

Confidence is Laya's raw `answer_confidence`, calibrated through the `risk`
question's binning map when a `Calibrator` is supplied (`laya_calibration.py`).
The router calibrates `route` but reads `risk` raw; passing the calibrator here
means a user whose destructive calls keep turning out fine stops tripping the
gate on the same confidence, without touching the router's own decisions.

This module never raises and never decides. It scores, and it says which band
the score fell in.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from laya_router import RISK_LEVELS

#: Where the confidence-shrunk score sits when Laya has said nothing useful.
NEUTRAL = 0.5

#: The directive's thresholds, and the defaults for the settings keys below.
DEFAULT_SAFE = 0.2
DEFAULT_DESTROY = 0.8

BANDS = ("safe", "elevated", "destructive")


@dataclass
class RiskAssessment:
    score: float
    band: str
    label: str | None
    confidence: float
    ordinal: int | None
    reason: str = ""

    def to_event(self) -> dict[str, Any]:
        return {
            "event": "risk",
            "score": self.score,
            "band": self.band,
            "label": self.label,
            "confidence": round(self.confidence, 3),
            "reason": self.reason,
        }


def thresholds(preferences: Any = None) -> tuple[float, float]:
    """`(safe, destroy)` from settings, falling back to the directive's 0.2/0.8.

    Read through `settings_store.Settings.get`, which already applies the
    user/project/local precedence and validated the range at write time.
    """
    safe, destroy = DEFAULT_SAFE, DEFAULT_DESTROY
    if preferences is not None:
        try:
            safe = float(preferences.get("laya.riskSafe", DEFAULT_SAFE))
            destroy = float(preferences.get("laya.riskDestroy", DEFAULT_DESTROY))
        except (TypeError, ValueError, AttributeError):
            safe, destroy = DEFAULT_SAFE, DEFAULT_DESTROY
    # A crossed pair would make `band` nonsense (both branches true, or neither).
    # Fail back to the defaults rather than silently inverting the gate.
    if not 0.0 <= safe < destroy <= 1.0:
        return DEFAULT_SAFE, DEFAULT_DESTROY
    return safe, destroy


def band(score: float, *, safe: float = DEFAULT_SAFE, destroy: float = DEFAULT_DESTROY) -> str:
    if score < safe:
        return "safe"
    if score > destroy:
        return "destructive"
    return "elevated"


def destructiveness(decision: Any, *, calibrator: Any = None) -> float:
    """Score one `RouteDecision` in 0.0 (clearly safe) .. 1.0 (clearly destructive).

    Returns `NEUTRAL` when Laya produced no risk answer at all — an abstained
    turn has not been assessed, and scoring it 0.0 would read as a clearance.
    """
    label = getattr(decision, "risk", None)
    if label not in RISK_LEVELS:
        return NEUTRAL
    ordinal = RISK_LEVELS.index(label)
    base = ordinal / (len(RISK_LEVELS) - 1)

    confidence = float(getattr(decision, "risk_confidence", 0.0) or 0.0)
    if calibrator is not None:
        try:
            confidence = calibrator.calibrated("risk", label, confidence)
        except Exception:
            pass  # an unusable calibrator must not cost us the score
    confidence = min(1.0, max(0.0, confidence))

    return round(NEUTRAL + (base - NEUTRAL) * confidence, 4)


def assess(decision: Any, *, preferences: Any = None, calibrator: Any = None) -> RiskAssessment:
    """Score, band, and explain — the form the ledger and the TUI both want."""
    label = getattr(decision, "risk", None)
    score = destructiveness(decision, calibrator=calibrator)
    safe, destroy = thresholds(preferences)
    place = band(score, safe=safe, destroy=destroy)

    if label not in RISK_LEVELS:
        reason = "Laya returned no risk reading for this turn"
    elif place == "safe":
        reason = f"Laya read this as {label} (score {score:.2f} < {safe:.2f})"
    elif place == "destructive":
        reason = f"Laya read this as {label} (score {score:.2f} > {destroy:.2f})"
    else:
        reason = f"Laya read this as {label} (score {score:.2f} between {safe:.2f} and {destroy:.2f})"

    return RiskAssessment(
        score=score,
        band=place,
        label=label if label in RISK_LEVELS else None,
        confidence=float(getattr(decision, "risk_confidence", 0.0) or 0.0),
        ordinal=RISK_LEVELS.index(label) if label in RISK_LEVELS else None,
        reason=reason,
    )


def auto_approvable(decision: Any, *, preferences: Any = None, calibrator: Any = None) -> bool:
    """Whether a confident `safe` reading may skip the approval prompt.

    Opt-in: `laya.riskAutoApprove` defaults to false, and an absent key reads as
    false. The caller is responsible for having checked that the tool would have
    been prompted at all — this says nothing about tools that were already going
    to run, or about any `deny` rule, which outranks everything here.

    **Deliberately not wired into the approval gate.** The reading scored here is
    Laya's of the *user's message*; the call under approval is chosen by a model
    that may since have read a web page, a file or a tool result. A turn that was
    confidently `read_only` ("summarise this README") would then wave through
    whatever the README talked the model into running. The score is safe to
    *show* a person deciding (it rides on the prompt) and unsafe to decide *for*
    them, so until the risk is read per call this stays a function with no
    caller, and `laya.riskAutoApprove` is a reserved key that changes nothing.
    """
    if preferences is None:
        return False
    try:
        if not preferences.get("laya.riskAutoApprove", False):
            return False
    except (AttributeError, TypeError):
        return False
    safe, _ = thresholds(preferences)
    return assess(decision, preferences=preferences, calibrator=calibrator).score < safe

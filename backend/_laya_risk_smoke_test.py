"""Regression check for laya_risk.py — the 0.0-1.0 destructiveness score, its
banding, and the guarantee that it does not disturb the existing `armed` gate.

No model, no network: `RouteDecision` is constructed directly, so this runs in
well under a second and cannot be affected by Laya's weights.

    backend/.venv/bin/python _laya_risk_smoke_test.py
"""

from laya_router import RISK_LEVELS, RouteDecision
import laya_risk


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


class Prefs:
    """Stands in for settings_store.Settings, which is just a validated dict."""

    def __init__(self, **values) -> None:
        self.values = values

    def get(self, key, default=None):
        return self.values.get(key, default)


def decision(risk, confidence):
    return RouteDecision(target=None, risk=risk, risk_confidence=confidence)


def main() -> None:
    # ------------------------------------------------------------- mapping
    # Confidence moves the score away from "no information" (0.5), so a
    # confident read_only is a clearance and a confident destructive is not.
    table = [
        ("read_only", 1.0, 0.0),
        ("read_only", 0.9, 0.05),
        ("read_only", 0.5, 0.25),
        ("edits_files", 1.0, 0.5),
        ("edits_files", 0.6, 0.5),
        ("destructive", 0.8, 0.9),
        ("destructive", 0.9, 0.95),
        ("destructive", 1.0, 1.0),
    ]
    for risk, confidence, expected in table:
        got = laya_risk.destructiveness(decision(risk, confidence))
        check(abs(got - expected) < 1e-9, f"{risk} @ {confidence} scores {expected} (got {got})")

    check(
        laya_risk.destructiveness(RouteDecision(target=None)) == laya_risk.NEUTRAL,
        "a decision with no risk reading scores NEUTRAL, not 0.0",
    )
    check(
        laya_risk.destructiveness(decision("nonsense", 0.9)) == laya_risk.NEUTRAL,
        "an unknown risk label scores NEUTRAL",
    )

    # Monotone in the ordinal at fixed confidence: read_only < edits_files <
    # destructive, which is the whole point of a comparable score.
    for confidence in (0.2, 0.5, 0.8, 1.0):
        by_ordinal = [laya_risk.destructiveness(decision(risk, confidence)) for risk in RISK_LEVELS]
        check(by_ordinal == sorted(by_ordinal), f"score orders with the ordinal at confidence {confidence}")
        check(len(set(by_ordinal)) == 3, f"the three levels stay distinct at confidence {confidence}")

    # And within a label, confidence moves the score *away* from NEUTRAL —
    # down toward a clearance for read_only, up toward a danger for destructive.
    for risk in RISK_LEVELS:
        scores = [laya_risk.destructiveness(decision(risk, c)) for c in (0.1, 0.3, 0.5, 0.7, 0.9, 1.0)]
        base = RISK_LEVELS.index(risk) / (len(RISK_LEVELS) - 1)
        expected = sorted(scores, reverse=base < laya_risk.NEUTRAL)
        check(scores == expected, f"{risk} score moves away from NEUTRAL as confidence rises")
        check(
            all(abs(score - laya_risk.NEUTRAL) <= abs(base - laya_risk.NEUTRAL) + 1e-9 for score in scores),
            f"{risk} never overshoots its own level's score",
        )

    # ------------------------------------------------------------- banding
    check(laya_risk.band(0.1) == "safe", "0.1 is safe")
    check(laya_risk.band(0.19) == "safe", "just under 0.2 is safe")
    check(laya_risk.band(0.2) == "elevated", "0.2 itself is elevated, not safe")
    check(laya_risk.band(0.5) == "elevated", "0.5 is elevated")
    check(laya_risk.band(0.8) == "elevated", "0.8 itself is elevated, not destructive")
    check(laya_risk.band(0.81) == "destructive", "just over 0.8 is destructive")

    # ----------------------------------------------------------- thresholds
    check(laya_risk.thresholds() == (0.2, 0.8), "defaults match the directive's 0.2 / 0.8")
    check(
        laya_risk.thresholds(Prefs(**{"laya.riskSafe": 0.1, "laya.riskDestroy": 0.9})) == (0.1, 0.9),
        "thresholds are read from settings",
    )
    # A crossed pair would make band() nonsense, so it must not be honoured.
    check(
        laya_risk.thresholds(Prefs(**{"laya.riskSafe": 0.9, "laya.riskDestroy": 0.3})) == (0.2, 0.8),
        "a crossed threshold pair falls back to the defaults",
    )

    # ------------------------------------------------------------- assess()
    assessed = laya_risk.assess(decision("destructive", 0.9))
    check(assessed.band == "destructive" and assessed.ordinal == 2, "assess() bands a destructive turn")
    check("destructive" in assessed.reason, "assess() explains itself")
    check(assessed.to_event()["score"] == 0.95, "the event form carries the score")

    # ------------------------------------------- the armed gate is untouched
    # laya_risk adds a signal; it must not change what the approval gate does.
    check(decision("destructive", 0.9).armed is True, "armed is unchanged for destructive @ 0.9")
    check(decision("destructive", 0.5).armed is False, "armed is unchanged for destructive @ 0.5")
    check(decision("edits_files", 1.0).armed is False, "armed ignores even a confident edits_files")
    check(decision("read_only", 1.0).armed is False, "armed ignores read_only")

    # -------------------------------------------------------- auto-approve
    destructive = decision("destructive", 0.95)
    read_only = decision("read_only", 0.95)
    check(
        laya_risk.auto_approvable(read_only) is False,
        "auto-approve is off when there is no settings object",
    )
    check(
        laya_risk.auto_approvable(read_only, preferences=Prefs()) is False,
        "auto-approve defaults to off when the key is absent",
    )
    enabled = Prefs(**{"laya.riskAutoApprove": True})
    check(laya_risk.auto_approvable(read_only, preferences=enabled) is True, "a confident read_only is auto-approvable when enabled")
    check(laya_risk.auto_approvable(destructive, preferences=enabled) is False, "a destructive turn is never auto-approvable")
    check(
        laya_risk.auto_approvable(decision("edits_files", 1.0), preferences=enabled) is False,
        "an edits_files turn is never auto-approvable (0.5 > the 0.2 safe threshold)",
    )

    # ------------------------------------------------- a broken calibrator
    class Broken:
        def calibrated(self, *args, **kwargs):
            raise RuntimeError("no calibration file")

    check(
        laya_risk.destructiveness(decision("destructive", 0.9), calibrator=Broken()) == 0.95,
        "an unusable calibrator falls back to the raw confidence",
    )


if __name__ == "__main__":
    main()
    print("\nlaya_risk: all checks passed")

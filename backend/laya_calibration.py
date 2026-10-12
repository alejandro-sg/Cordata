"""Online calibration and reinforcement for Laya's decisions, from execution
outcomes, scored with strictly proper scoring rules.

Laya reports ``answer_confidence`` (max p of its answer). Whether that number
means "right that often" *for this user's work* is an empirical question, and
the answer drifts. Every routed turn and every executed command produces an
outcome (stdout success / stderr failure, tests passing, an approval rejected),
and this module turns those outcomes into three things:

1. **Binning maps** (Bayesian histogram binning) per ``(question, label)``:
   raw confidence falls in one of 10 bins; each bin holds a Beta posterior
   whose prior is centred on the raw confidence itself (pseudo-counts), so
   with no data the map is the identity and with data it converges to the
   bin's observed success rate. ``calibrated(q, label, raw)`` is what the
   abstention gate reads. The prior is worth 8 outcomes: "agreement" labels are
   noisy (the LLM's own tool choice is not ground truth), so one miss moves a
   0.99 route to 0.88, two to 0.79 — not straight under the gate.
2. **Decision weights**: an additive logit bias ``β`` per ``(question, label)``
   applied when choosing among options (``argmax log p + β``). After each
   outcome ``β += lr · (y − p̂)`` — the gradient of the log loss of the
   calibrated prediction, so it only moves while predictions are miscalibrated
   and settles when they are right. Clipped to ±2.
3. **Scores**: prequential log loss and Brier score — each outcome scores the
   prediction made *before* it was learned from — for raw vs calibrated
   probabilities. Both rules are strictly proper; if the calibrated column
   isn't lower, the calibration isn't helping, and ``/laya stats`` shows it.

What this is not: it does not change Laya's transformer weights (fine-tuning
is the offline loop in ``docs/laya-triage.md``). It is the cheap, local, online
layer on top, keyed per user (``~/.cordata/laya/calibration.json``).
"""

from __future__ import annotations

import json
import math
import os
import threading
from pathlib import Path
from typing import Any

import settings_store

BINS = 10
PRIOR_STRENGTH = 8.0
LEARNING_RATE = 0.1
BIAS_LIMIT = 2.0
_EPS = 1e-6


def _clip(p: float) -> float:
    return min(1 - _EPS, max(_EPS, float(p)))


def _bin(p: float) -> int:
    return min(BINS - 1, max(0, int(p * BINS)))


def default_path() -> Path:
    return settings_store.user_dir() / "laya" / "calibration.json"


class Calibrator:
    def __init__(self, path: str | os.PathLike | None = None, autosave: bool = True) -> None:
        self.path = Path(path) if path else default_path()
        self.autosave = autosave
        self._lock = threading.Lock()
        # key "question/label" -> {"bins": [[successes, trials], ...], "bias": float}
        self.maps: dict[str, dict[str, Any]] = {}
        # question -> running proper scores
        self.scores: dict[str, dict[str, float]] = {}
        self._load()

    # ------------------------------------------------------------ storage

    def _load(self) -> None:
        try:
            data = json.loads(self.path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return
        if isinstance(data, dict):
            self.maps = {k: v for k, v in (data.get("maps") or {}).items() if isinstance(v, dict) and len(v.get("bins", [])) == BINS}
            self.scores = data.get("scores") or {}

    def save(self) -> None:
        with self._lock:
            payload = json.dumps({"version": 1, "maps": self.maps, "scores": self.scores}, indent=1)
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            tmp = self.path.with_suffix(".tmp")
            tmp.write_text(payload, encoding="utf-8")
            os.replace(tmp, self.path)
        except OSError:
            pass

    def _entry(self, question: str, label: str) -> dict[str, Any]:
        key = f"{question}/{label}"
        entry = self.maps.get(key)
        if entry is None:
            entry = self.maps[key] = {"bins": [[0.0, 0.0] for _ in range(BINS)], "bias": 0.0}
        return entry

    # ------------------------------------------------------------- reading

    def calibrated(self, question: str, label: str, raw: float) -> float:
        """The binning map: Beta posterior mean of the raw confidence's bin,
        prior centred on ``raw`` itself (identity until outcomes arrive)."""
        raw = _clip(raw)
        with self._lock:
            entry = self.maps.get(f"{question}/{label}")
            if entry is None:
                return raw
            successes, trials = entry["bins"][_bin(raw)]
        return _clip((successes + PRIOR_STRENGTH * raw) / (trials + PRIOR_STRENGTH))

    def bias(self, question: str, label: str) -> float:
        with self._lock:
            entry = self.maps.get(f"{question}/{label}")
            return float(entry["bias"]) if entry else 0.0

    def choose(self, question: str, probabilities: dict[str, float]) -> tuple[str, float]:
        """Pick an option with the learned decision weights: argmax log p + β.
        Returns (label, renormalised probability of that label after weighting)."""
        if not probabilities:
            raise ValueError("no options")
        scores = {label: math.log(_clip(p)) + self.bias(question, label) for label, p in probabilities.items()}
        top = max(scores.values())
        total = sum(math.exp(s - top) for s in scores.values())
        label = max(scores, key=scores.get)
        return label, math.exp(scores[label] - top) / total

    # ------------------------------------------------------------- learning

    def update(self, question: str, label: str, raw: float, outcome: bool) -> dict[str, float]:
        """Learn from one outcome of a prediction made with confidence ``raw``.
        Returns the prequential scores of that prediction (before learning)."""
        raw = _clip(raw)
        y = 1.0 if outcome else 0.0
        calibrated = self.calibrated(question, label, raw)
        with self._lock:
            entry = self._entry(question, label)
            bucket = entry["bins"][_bin(raw)]
            bucket[0] += y
            bucket[1] += 1
            entry["bias"] = max(-BIAS_LIMIT, min(BIAS_LIMIT, entry["bias"] + LEARNING_RATE * (y - calibrated)))
            score = self.scores.setdefault(
                question, {"n": 0, "log_loss_raw": 0.0, "log_loss_cal": 0.0, "brier_raw": 0.0, "brier_cal": 0.0, "successes": 0}
            )
            point = {
                "log_loss_raw": -(y * math.log(raw) + (1 - y) * math.log(1 - raw)),
                "log_loss_cal": -(y * math.log(calibrated) + (1 - y) * math.log(1 - calibrated)),
                "brier_raw": (raw - y) ** 2,
                "brier_cal": (calibrated - y) ** 2,
            }
            score["n"] += 1
            score["successes"] += int(y)
            for key, value in point.items():
                score[key] += value
        if self.autosave:
            self.save()
        return {**point, "calibrated": calibrated}

    def stats(self) -> dict[str, dict[str, float]]:
        """Mean proper scores per question (lower is better)."""
        with self._lock:
            out = {}
            for question, score in self.scores.items():
                n = max(1, score["n"])
                out[question] = {
                    "n": score["n"],
                    "success_rate": round(score["successes"] / n, 3),
                    "log_loss_raw": round(score["log_loss_raw"] / n, 4),
                    "log_loss_calibrated": round(score["log_loss_cal"] / n, 4),
                    "brier_raw": round(score["brier_raw"] / n, 4),
                    "brier_calibrated": round(score["brier_cal"] / n, 4),
                }
            return out

    def weights(self) -> dict[str, float]:
        with self._lock:
            return {key: round(entry["bias"], 3) for key, entry in self.maps.items() if abs(entry["bias"]) > 1e-9}

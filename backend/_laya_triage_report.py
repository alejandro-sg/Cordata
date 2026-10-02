"""Agreement report for laya_triage.py's shadow log.

    backend/.venv/bin/python _laya_triage_report.py [LOG.jsonl] [--thresholds 0.5,0.6,0.7,0.8,0.9]

Reads one JSON line per user turn (Laya's would-be route + confidence, and the
route the orchestrator LLM actually took, derived from the tools it called) and
reports how often they agree, overall and at each confidence threshold.

Read it for what it is: *agreement with the orchestrator's behaviour*, not
accuracy against ground truth — the LLM is the policy being measured and makes
mistakes too. The decision it supports: "above confidence τ, would routing by
Laya have picked what the LLM picked, and on how much of the traffic?" The
majority-class baseline is printed beside accuracy because a classifier that
always says "chat" can look good on chat-heavy traffic.
"""

import json
import sys
from collections import Counter, defaultdict
from pathlib import Path

DEFAULT_THRESHOLDS = (0.5, 0.6, 0.7, 0.8, 0.9)


def percentile(sorted_values: list[float], pct: float) -> float | None:
    if not sorted_values:
        return None
    rank = max(1, -(-int(pct * len(sorted_values)) // 100))  # ceil(pct% of n), nearest-rank
    return sorted_values[min(rank, len(sorted_values)) - 1]


def load(path: Path) -> list[dict]:
    records = []
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip():
            continue
        try:
            records.append(json.loads(line))
        except json.JSONDecodeError:
            print(f"warning: skipping malformed line {number}", file=sys.stderr)
    return records


def build_report(records: list[dict], thresholds: tuple[float, ...] = DEFAULT_THRESHOLDS) -> str:
    usable = [
        r for r in records
        if "route" in (r.get("laya") or {}) and not (r.get("actual") or {}).get("incomplete")
    ]
    errored = sum(1 for r in records if "error" in (r.get("laya") or {}))
    incomplete = sum(1 for r in records if (r.get("actual") or {}).get("incomplete"))
    lines = [
        f"turns logged: {len(records)}   usable: {len(usable)}   laya errors: {errored}   incomplete turns: {incomplete}",
    ]
    if not usable:
        lines.append("nothing to compare yet.")
        return "\n".join(lines)

    actual = [r["actual"]["route"] for r in usable]
    predicted = [r["laya"]["route"] for r in usable]
    agree = sum(a == p for a, p in zip(actual, predicted))
    majority, majority_n = Counter(actual).most_common(1)[0]
    lines += [
        "",
        f"agreement with orchestrator: {agree}/{len(usable)} = {agree / len(usable):.1%}",
        f"majority-class baseline    : {majority_n}/{len(usable)} = {majority_n / len(usable):.1%}   (always '{majority}')",
        f"actual routes: {dict(Counter(actual).most_common())}",
    ]
    if len(usable) < 100:
        lines.append(f"note: only {len(usable)} usable turns — treat every percentage here as anecdotal.")

    # per-route precision/recall
    labels = sorted(set(actual) | set(predicted))
    lines += ["", "per route (laya's pick vs actual):", f"  {'route':<16}{'support':>8}{'precision':>11}{'recall':>9}"]
    for label in labels:
        tp = sum(a == p == label for a, p in zip(actual, predicted))
        support = sum(a == label for a in actual)
        picked = sum(p == label for p in predicted)
        precision = f"{tp / picked:.0%}" if picked else "-"
        recall = f"{tp / support:.0%}" if support else "-"
        lines.append(f"  {label:<16}{support:>8}{precision:>11}{recall:>9}")

    # confusion
    confusion: dict[str, Counter] = defaultdict(Counter)
    for a, p in zip(actual, predicted):
        confusion[a][p] += 1
    wrong = [(a, p, n) for a, row in confusion.items() for p, n in row.items() if a != p]
    if wrong:
        lines += ["", "most common disagreements (actual -> laya said):"]
        for a, p, n in sorted(wrong, key=lambda item: -item[2])[:6]:
            lines.append(f"  {a} -> {p}: {n}")

    # confidence gating: the table that decides whether a fast path is safe
    lines += ["", "confidence gate (route_confidence >= threshold):", f"  {'threshold':<11}{'coverage':>10}{'agreement':>11}{'would misroute':>16}"]
    for threshold in thresholds:
        covered = [(a, p) for a, p, r in zip(actual, predicted, usable) if r["laya"]["route_confidence"] >= threshold]
        if not covered:
            lines.append(f"  {threshold:<11.2f}{'0%':>10}{'-':>11}{'-':>16}")
            continue
        right = sum(a == p for a, p in covered)
        lines.append(
            f"  {threshold:<11.2f}{len(covered) / len(usable):>10.0%}{right / len(covered):>11.0%}{len(covered) - right:>16}"
        )

    # needs_team vs actual team usage
    team_turns = [r for r in usable if r["actual"]["route"] == "team"]
    other_turns = [r for r in usable if r["actual"]["route"] != "team"]
    if team_turns and other_turns:
        mean = lambda rows: sum(r["laya"]["needs_team"] for r in rows) / len(rows)  # noqa: E731
        lines += [
            "",
            f"needs_team: mean P(team) {mean(team_turns):.2f} on turns where the LLM used a team "
            f"({len(team_turns)}) vs {mean(other_turns):.2f} on the rest ({len(other_turns)})",
        ]

    latencies = sorted(r["laya"]["latency_ms"] for r in usable if "latency_ms" in r["laya"])
    turn_ms = sorted(r["actual"]["turn_ms"] for r in usable if r["actual"].get("turn_ms") is not None)
    if latencies:
        lines += [
            "",
            f"laya latency   p50 {percentile(latencies, 50):.0f} ms   p95 {percentile(latencies, 95):.0f} ms",
            f"whole turn     p50 {percentile(turn_ms, 50):.0f} ms   p95 {percentile(turn_ms, 95):.0f} ms",
        ]
    return "\n".join(lines)


def main(argv: list[str]) -> int:
    thresholds = DEFAULT_THRESHOLDS
    paths = []
    args = iter(argv)
    for arg in args:
        if arg == "--thresholds":
            thresholds = tuple(float(x) for x in next(args).split(","))
        else:
            paths.append(arg)
    if paths:
        path = Path(paths[0])
    else:
        from settings import LAYA_TRIAGE_LOG

        path = Path(LAYA_TRIAGE_LOG)
    if not path.exists():
        print(f"no log at {path} — run the backend with LAYA_TRIAGE_MODE=shadow first", file=sys.stderr)
        return 1
    print(f"log: {path}")
    print(build_report(load(path), thresholds))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

"""Live view of laya_triage.py's log — for any front door (the web UI, Zed, a script);
acp-tui shows the same thing inline, no second terminal needed.

    backend/.venv/bin/python _laya_triage_watch.py [LOG.jsonl] [--all]     (or: make laya-watch)

Follows the log and prints one line per turn the moment it ends: what Laya said and how
confident it was, whether a hint was sent, what the LLM then actually did, and whether
they agreed. Starts at the end of the file (new turns only); --all replays it first.
The log is written when a turn finishes, so this lags the live line in acp-tui by one turn.
"""

import json
import sys
import time
from pathlib import Path

DIM, GREEN, YELLOW, CYAN, RESET = "\033[2m", "\033[32m", "\033[33m", "\033[36m", "\033[0m"


def format_record(record: dict, color: bool = True) -> str:
    c = (lambda code, text: f"{code}{text}{RESET}") if color else (lambda _code, text: text)
    actual, laya = record.get("actual") or {}, record.get("laya") or {}
    when = time.strftime("%H:%M:%S", time.localtime(record.get("ts", 0)))
    text = (record.get("text") or "").replace("\n", " ")
    prompt = text if len(text) <= 48 else text[:47] + "…"
    head = f"{c(DIM, when)} {prompt!r:<52}"
    if "route" not in laya:
        return f"{head} laya: no decision — {laya.get('error', '?')}   LLM used {actual.get('route')}"
    confidence = laya["route_confidence"]
    said = f"laya {laya['route']} {confidence:.0%}"
    if record.get("hint"):
        said += c(CYAN, " ▸hint")
    elif record.get("mode") == "shadow":
        said += c(DIM, " (shadow)")
    agreed = laya["route"] == actual.get("route")
    verdict = c(GREEN, "✓ agreed") if agreed else c(YELLOW, "✗ disagreed")
    tools = f" ({', '.join(actual.get('tools') or [])})" if actual.get("tools") else ""
    tiers = f"  tiers {actual['tiers']}" if actual.get("tiers") else ""
    ms = f"  {laya.get('latency_ms', 0):.0f}ms"
    return f"{head} {said:<28} LLM used {actual.get('route')}{tools}  {verdict}{c(DIM, ms)}{tiers}"


def main(argv: list[str]) -> int:
    replay = "--all" in argv
    paths = [a for a in argv if not a.startswith("--")]
    if paths:
        path = Path(paths[0])
    else:
        from settings import LAYA_TRIAGE_LOG

        path = Path(LAYA_TRIAGE_LOG)
    color = sys.stdout.isatty()
    print(f"watching {path} — Ctrl+C to stop", file=sys.stderr)
    position = 0 if replay else (path.stat().st_size if path.exists() else 0)
    try:
        while True:
            if path.exists() and path.stat().st_size > position:
                with path.open(encoding="utf-8") as handle:
                    handle.seek(position)
                    for line in handle:
                        if line.endswith("\n") and line.strip():
                            try:
                                print(format_record(json.loads(line), color), flush=True)
                            except json.JSONDecodeError:
                                pass
                            position += len(line.encode("utf-8"))
                        elif not line.endswith("\n"):
                            break  # a half-written line: pick it up next round
            elif path.exists() and path.stat().st_size < position:
                position = 0  # rotated/truncated
            time.sleep(0.3)
    except KeyboardInterrupt:
        return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

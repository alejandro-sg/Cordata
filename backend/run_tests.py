"""Run the hermetic smoke suites in parallel: ``backend/.venv/bin/python run_tests.py``.

    run_tests.py                 every hermetic suite, -j from the CPU count
    run_tests.py -j 4            four at a time
    run_tests.py -k orchestr     only suites whose name contains "orchestr" (repeatable)
    run_tests.py --live          also the suites that call a real model (_acp_smoke_test.py)
    run_tests.py --list          what would run, longest first

Each suite runs in its own process with its own throwaway ``CORDATA_HOME`` and ``state.db`` (so ledgers, checkpoint
stores and settings never collide), ``CORDATA_DISCORD=off`` (a test turn never pages the operator), and a timeout.
Suites are started longest-first from the durations recorded by the last run (``.test-durations.json``), which keeps
the slow ACP/eval suites from all landing at the end. Output goes to one log per suite; a failure prints its tail.
Exit status is non-zero when any suite failed.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

HERE = Path(__file__).resolve().parent
DURATIONS = HERE / ".test-durations.json"
#: Suites that need a real model (they are named *_smoke_test but are live checks; see .github/workflows/ci.yml).
LIVE = {"_acp_smoke_test.py", "_acp_settings_smoke_test.py"}
DEFAULT_TIMEOUT = 900.0
UNKNOWN_DURATION = 30.0


def discover(live: bool, patterns: list[str]) -> list[str]:
    names = sorted(p.name for p in HERE.glob("_*_smoke_test.py"))
    if not live:
        names = [n for n in names if n not in LIVE]
    if patterns:
        names = [n for n in names if any(p in n for p in patterns)]
    return names


def load_durations() -> dict[str, float]:
    try:
        data = json.loads(DURATIONS.read_text())
        return {k: float(v) for k, v in data.items()}
    except (OSError, ValueError):
        return {}


def save_durations(new: dict[str, float]) -> None:
    merged = {**load_durations(), **new}
    try:
        DURATIONS.write_text(json.dumps(dict(sorted(merged.items())), indent=1) + "\n")
    except OSError:
        pass


def run_one(name: str, logs: Path, timeout: float) -> tuple[str, int, float, Path]:
    home = Path(tempfile.mkdtemp(prefix=f"suite-{name.strip('_').split('_smoke')[0]}-"))
    env = {
        **os.environ,
        "CORDATA_HOME": str(home),
        "CORDATA_STATE_DB": str(home / "state.db"),
        "CORDATA_DISCORD": "off",
        "PYTHONUNBUFFERED": "1",
        # Hermetic means no network: Laya's loader asks the Hugging Face hub whether its cached weights are current,
        # and a suite that points HTTPS_PROXY at a fake vault would wait on that request forever.
        "HF_HUB_OFFLINE": os.environ.get("HF_HUB_OFFLINE", "1"),
    }
    log = logs / f"{name}.log"
    started = time.monotonic()
    with open(log, "w") as out:
        try:
            done = subprocess.run([sys.executable, "-u", name], cwd=HERE, env=env, stdout=out, stderr=subprocess.STDOUT, timeout=timeout)
            code = done.returncode
        except subprocess.TimeoutExpired:
            out.write(f"\n[run_tests] timed out after {timeout:g}s\n")
            code = 124
    return name, code, time.monotonic() - started, log


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("-j", "--jobs", type=int, default=max(2, min(8, (os.cpu_count() or 4) // 2)))
    parser.add_argument("-k", dest="patterns", action="append", default=[])
    parser.add_argument("--live", action="store_true")
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT)
    args = parser.parse_args(argv)

    durations = load_durations()
    suites = sorted(discover(args.live, args.patterns), key=lambda n: -durations.get(n, UNKNOWN_DURATION))
    if args.list:
        for name in suites:
            print(f"{durations.get(name, UNKNOWN_DURATION):7.1f}s  {name}")
        return 0
    if not suites:
        print("no suites selected")
        return 2
    logs = Path(tempfile.mkdtemp(prefix="smoke-logs-"))
    lock = threading.Lock()
    results: list[tuple[str, int, float, Path]] = []
    started = time.monotonic()
    serial = sum(durations.get(n, UNKNOWN_DURATION) for n in suites)
    print(f"{len(suites)} suites, {args.jobs} at a time (about {serial / 60:.0f} min if run one by one); logs in {logs}", flush=True)
    with ThreadPoolExecutor(max_workers=args.jobs) as pool:
        futures = [pool.submit(run_one, name, logs, args.timeout) for name in suites]
        for future in as_completed(futures):
            name, code, seconds, log = future.result()
            with lock:
                results.append((name, code, seconds, log))
                mark = "ok  " if code == 0 else "FAIL"
                print(f"{mark} {seconds:6.1f}s  {name}  [{len(results)}/{len(suites)}]", flush=True)
    save_durations({name: round(seconds, 1) for name, code, seconds, _ in results if code == 0})
    failed = [r for r in results if r[1] != 0]
    for name, code, _, log in failed:
        tail = log.read_text(errors="replace").splitlines()[-25:]
        print(f"\n--- {name} (exit {code}) — {log}\n" + "\n".join(tail))
    wall = time.monotonic() - started
    print(f"\n{len(results) - len(failed)}/{len(results)} passed in {wall / 60:.1f} min (one by one: {sum(r[2] for r in results) / 60:.1f} min)")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())

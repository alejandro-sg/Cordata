#!/usr/bin/env bash
# Deterministic Verification Script for Cordata
# Usage:
#   scripts/verify.sh          # fast check (compilation, secrets, core smoke test)
#   scripts/verify.sh --all    # full check (all smoke tests + rust tests)

set -e

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PYTHON="${ROOT}/backend/.venv/bin/python"

if [ ! -x "$PYTHON" ]; then
    PYTHON="$(which python3)"
fi

echo "=== 1. Checking syntax and compilation ==="
"$PYTHON" -m compileall -q "$ROOT/backend" "$ROOT/scripts"
echo "✓ Python bytecode compilation clean."

echo "=== 2. Checking secrets and credentials ==="
"$PYTHON" "$ROOT/scripts/check-secrets.py"
echo "✓ Zero credential leaks detected."

echo "=== 3. Running core hermetic smoke tests ==="
(cd "$ROOT/backend" && "$PYTHON" _background_tasks_smoke_test.py)
echo "✓ Core smoke tests passed."

if [ "$1" = "--all" ]; then
    echo "=== 4. Running full test suite ==="
    (cd "$ROOT" && make test)
    (cd "$ROOT" && make tui-test)
    echo "✓ All test suites passed."
fi

echo "======================================"
echo "VERIFICATION PASSED (exit code 0)"
echo "======================================"
exit 0

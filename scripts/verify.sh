#!/usr/bin/env bash
# Deterministic verification for Cordata.
#   scripts/verify.sh          # fast: secrets, then the TUI workspace's tests
#   scripts/verify.sh --all    # also the native app's tests

set -e

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "=== 1. Secrets and credentials ==="
(cd "$ROOT/tui" && cargo run -q -p cordata-guard --bin cordata-secrets)

echo "=== 2. TUI workspace tests ==="
(cd "$ROOT" && make tui-test)

if [ "$1" = "--all" ]; then
    echo "=== 3. Native app tests ==="
    (cd "$ROOT" && make app-test)
fi

echo "VERIFICATION PASSED (exit code 0)"

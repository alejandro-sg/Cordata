#!/usr/bin/env bash
# The Experimental Lab against a scripted, self-contained daemon (backend/lab_demo.py): every
# feature of the window can be tried without a model, a key or a token spent, and nothing touches
# ~/.sierralab or a daemon you already run.
#
#   make lab-demo
#   make lab-demo WATCH=~/some/project        # the window's observer watches that workspace
#   make lab-menubar-demo WATCH=~/some/project # the menu bar instead of the window (--menubar)
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

PYTHON="$ROOT/backend/.venv/bin/python"
if ! [[ -x "$PYTHON" ]]; then
  err "backend/.venv not found — run scripts/setup.sh first."
  exit 1
fi

DEMO_HOME="$(mktemp -d "${TMPDIR:-/tmp}/sierralab-demo.XXXXXX")"
WORKSPACE="$DEMO_HOME/workspace"
mkdir -p "$WORKSPACE"
DEMO_PID=""

cleanup() {
  if [[ -n "$DEMO_PID" ]]; then
    kill "$DEMO_PID" 2>/dev/null || true
    wait "$DEMO_PID" 2>/dev/null || true
  fi
  rm -rf "${DEMO_HOME:?}"
}
trap cleanup EXIT INT TERM

(cd "$ROOT/backend" && exec "$PYTHON" lab_demo.py --home "$DEMO_HOME" --workspace "$WORKSPACE") >"$DEMO_HOME/demo.log" 2>&1 &
DEMO_PID=$!

log "Starting the demo daemon…"
for _ in $(seq 1 100); do
  if grep -q '"grpcPort"' "$DEMO_HOME/daemon.json" 2>/dev/null && grep -q "demo daemon ready" "$DEMO_HOME/demo.log" 2>/dev/null; then
    break
  fi
  if ! kill -0 "$DEMO_PID" 2>/dev/null; then
    err "the demo daemon exited:"
    cat "$DEMO_HOME/demo.log" >&2
    exit 1
  fi
  sleep 0.2
done
if ! grep -q "demo daemon ready" "$DEMO_HOME/demo.log" 2>/dev/null; then
  err "the demo daemon did not come up:"
  cat "$DEMO_HOME/demo.log" >&2
  exit 1
fi
ok "$(grep 'demo daemon ready' "$DEMO_HOME/demo.log" | head -1)"
dim "Approvals appear on their own every ~15 s. Close the window (cmd-q) to stop the demo."

# The menu bar is the same client against the same demo daemon; only the front door differs. The
# window's --watch is meaningless there (it runs no observer), so it is not passed.
if [[ "${1:-}" == "--menubar" ]]; then
  dim "Watch the menu bar: the icon is idle, turns cyan while the demo works, and amber when a"
  dim "destructive command is held. cmd-q in a terminal, or quit the process, to stop."
  cd "$ROOT/lab"
  exec env SIERRALAB_HOME="$DEMO_HOME" cargo run -q -p harness-menubar
fi

cd "$ROOT/lab"
SIERRALAB_HOME="$DEMO_HOME" cargo run -q -p harness-lab -- --watch "${WATCH:-$WORKSPACE}"

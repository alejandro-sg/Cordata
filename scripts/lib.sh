#!/usr/bin/env bash
# Shared helpers for the scripts/dev-*.sh family. Sourced, never run
# directly — every dev-*.sh starts with `source "$(dirname "$0")/lib.sh"`.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_DIR="$ROOT/.dev"
mkdir -p "$RUN_DIR"

QDRANT_CONTAINER="sierralab-agent-qdrant"
QDRANT_VOLUME="sierralab-agent-qdrant-data"

# --- output -----------------------------------------------------------

if [[ -t 1 ]]; then
  C_GREEN=$'\033[32m'; C_YELLOW=$'\033[33m'; C_RED=$'\033[31m'; C_DIM=$'\033[2m'; C_RESET=$'\033[0m'
else
  C_GREEN=""; C_YELLOW=""; C_RED=""; C_DIM=""; C_RESET=""
fi

log()  { printf '%s\n' "$*"; }
ok()   { printf '%s✓%s %s\n' "$C_GREEN" "$C_RESET" "$*"; }
warn() { printf '%s!%s %s\n' "$C_YELLOW" "$C_RESET" "$*"; }
err()  { printf '%s✗%s %s\n' "$C_RED" "$C_RESET" "$*" >&2; }
dim()  { printf '%s%s%s\n' "$C_DIM" "$*" "$C_RESET"; }

# --- container runtime --------------------------------------------------

# Echoes "docker" or "podman", whichever is actually installed — prefers
# docker since it's the more commonly pre-installed of the two, but this
# repo was originally built against podman, so both are supported rather
# than assuming one.
container_cmd() {
  if command -v docker >/dev/null 2>&1; then
    echo docker
  elif command -v podman >/dev/null 2>&1; then
    echo podman
  else
    return 1
  fi
}

# --- pidfile-managed background processes --------------------------------

# start_bg <name> <logfile> <command...> — runs a command in the
# background and records its pid via $! (captured immediately, so this is
# exact — no guessing which process was "the one just started"). Call from
# inside a `(cd "$dir" && start_bg ...)` subshell to control its working
# directory; the backgrounded process inherits the subshell's cwd.
start_bg() {
  local name="$1" logfile="$2"; shift 2
  local pidfile="$RUN_DIR/$name.pid"
  if is_running "$name"; then
    warn "$name already running (pid $(cat "$pidfile"))"
    return 0
  fi
  "$@" >"$logfile" 2>&1 &
  local pid=$!
  disown
  echo "$pid" >"$pidfile"
  sleep 1
  if ! kill -0 "$pid" 2>/dev/null; then
    err "$name exited immediately — see $logfile"
    rm -f "$pidfile"
    return 1
  fi
  ok "$name started (pid $pid) — logs: $logfile"
}

is_running() {
  local pidfile="$RUN_DIR/$1.pid"
  [[ -f "$pidfile" ]] && kill -0 "$(cat "$pidfile")" 2>/dev/null
}

stop_bg() {
  local name="$1"
  local pidfile="$RUN_DIR/$name.pid"
  if ! [[ -f "$pidfile" ]]; then
    dim "$name: not running (no pidfile)"
    return 0
  fi
  local pid
  pid="$(cat "$pidfile")"
  if ! kill -0 "$pid" 2>/dev/null; then
    dim "$name: pidfile stale (pid $pid already gone)"
    rm -f "$pidfile"
    return 0
  fi
  log "Stopping $name (pid $pid)..."
  pkill -P "$pid" 2>/dev/null || true   # children first: npm's own next dev, uvicorn's reload worker
  kill "$pid" 2>/dev/null || true
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.3
  done
  if kill -0 "$pid" 2>/dev/null; then
    warn "$name (pid $pid) didn't stop cleanly, forcing"
    pkill -9 -P "$pid" 2>/dev/null || true
    kill -9 "$pid" 2>/dev/null || true
  fi
  rm -f "$pidfile"
  ok "$name stopped"
}

port_open() {
  local port="$1"
  (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && exec 3>&- 3<&-
}

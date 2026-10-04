#!/usr/bin/env bash
# The always-on orchestrator daemon (backend/daemon/). Subcommands:
#
#   daemon.sh run         foreground, for watching it work
#   daemon.sh status      is it up, where, and what is it holding
#   daemon.sh stop        stop a daemon started by `run`
#   daemon.sh install     register the macOS LaunchAgent so it starts at login
#   daemon.sh uninstall   unregister it and stop the daemon
#
# The daemon owns its own pidfile (backend/daemon/main.py), so `run` and
# `stop` agree with `install` about which process is which without this script
# tracking a second source of truth.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

LABEL="dev.sierralab.daemon"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
PYTHON="$ROOT/backend/.venv/bin/python"
DAEMON_LOG="$RUN_DIR/daemon.log"
DAEMON_PID="$RUN_DIR/daemon.pid"

require_venv() {
  if ! [[ -x "$PYTHON" ]]; then
    err "backend/.venv not found — run scripts/setup.sh first."
    exit 1
  fi
}

# launchd starts a process with a minimal PATH, and the daemon spawns
# `acp_server.py` children which in turn run shells — a PATH without
# /usr/local/bin or Homebrew is the difference between `git` working and not.
launchd_path() {
  printf '%s' "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
}

case "${1:-status}" in
run)
  require_venv
  shift # drop "run" so the rest of the line reaches argparse
  if [[ -f "$DAEMON_PID" ]] && kill -0 "$(cat "$DAEMON_PID")" 2>/dev/null; then
    err "a daemon is already running (pid $(cat "$DAEMON_PID")) — scripts/daemon.sh stop first"
    exit 1
  fi
  log "Starting the daemon in the foreground (Ctrl-C to stop)..."
  dim "Sessions: one acp_server.py child per project. Logs: $DAEMON_LOG when installed."
  # `-m daemon.main` rather than a path: it is what the LaunchAgent runs too,
  # so the two ways of starting it exercise the same entry point.
  cd "$ROOT/backend" && exec "$PYTHON" -m daemon.main "$@"
  ;;

status)
  require_venv
  if [[ -f "$DAEMON_PID" ]]; then
    pid="$(cat "$DAEMON_PID")"
    if kill -0 "$pid" 2>/dev/null; then
      ok "daemon running (pid $pid, pidfile $DAEMON_PID)"
    else
      warn "stale pidfile (pid $pid is gone) — it will be replaced on next start"
    fi
  else
    dim "no pidfile at $DAEMON_PID"
  fi
  # Ask it, rather than trusting the pidfile: the daemon's own answer includes
  # the port it actually bound and the sessions it is holding.
  (cd "$ROOT/backend" && "$PYTHON" -m daemon.main --check) || true
  if [[ -f "$PLIST" ]]; then
    dim "LaunchAgent: $PLIST"
    launchctl print "gui/$UID/$LABEL" >/dev/null 2>&1 && ok "registered with launchd" || warn "plist present but not loaded"
  fi
  ;;

stop)
  if [[ ! -f "$DAEMON_PID" ]]; then
    dim "no daemon pidfile — nothing to stop"
    exit 0
  fi
  pid="$(cat "$DAEMON_PID")"
  if ! kill -0 "$pid" 2>/dev/null; then
    dim "pid $pid is already gone"
    rm -f "$DAEMON_PID"
    exit 0
  fi
  # SIGTERM, not SIGKILL: main.py's handler stops every child process group on
  # the way out. A killed daemon would leave `acp_server.py` processes holding
  # pipes nobody reads.
  log "Stopping daemon (pid $pid)..."
  kill "$pid" 2>/dev/null || true
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.3
  done
  if kill -0 "$pid" 2>/dev/null; then
    warn "daemon (pid $pid) didn't stop cleanly — children may still be running"
    kill -9 "$pid" 2>/dev/null || true
  fi
  rm -f "$DAEMON_PID"
  ok "daemon stopped"
  ;;

install)
  require_venv
  mkdir -p "$(dirname "$PLIST")" "$RUN_DIR"
  cat >"$PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$PYTHON</string>
    <string>-m</string>
    <string>daemon.main</string>
  </array>
  <key>WorkingDirectory</key><string>$ROOT/backend</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>$(launchd_path)</string>
  </dict>
  <!-- Restart on exit (including a crash) and at login. A restart that raced
       a previous process still holding the port is what the daemon's pidfile
       check resolves by removing a stale one. -->
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>$DAEMON_LOG</string>
  <key>StandardErrorPath</key><string>$DAEMON_LOG</string>
  <key>ProcessType</key><string>Background</string>
</dict>
</plist>
PLIST_EOF
  # Unload first: `bootstrap` on an already-loaded label fails, and the usual
  # reason to re-run install is that something changed.
  launchctl bootout "gui/$UID/$LABEL" 2>/dev/null || true
  if launchctl bootstrap "gui/$UID" "$PLIST" 2>/dev/null; then
    ok "daemon installed and started ($LABEL)"
  else
    err "launchctl bootstrap failed — try: launchctl bootstrap gui/$UID $PLIST"
    exit 1
  fi
  dim "log: $DAEMON_LOG   stop: scripts/daemon.sh uninstall"
  ;;

uninstall)
  if launchctl bootout "gui/$UID/$LABEL" 2>/dev/null; then
    ok "daemon unloaded from launchd"
  else
    dim "not registered with launchd"
  fi
  rm -f "$PLIST"
  # The daemon is gone with the service, but a `run`-started one would not be.
  "$0" stop || true
  ok "LaunchAgent removed"
  ;;

*)
  err "unknown subcommand: $1"
  dim "usage: scripts/daemon.sh [run|status|stop|install|uninstall]"
  exit 1
  ;;
esac

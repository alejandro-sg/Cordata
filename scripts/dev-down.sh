#!/usr/bin/env bash
# Stops backend + frontend. Leaves Qdrant running by default (it's a
# persistent data store, not something you want to tear down every time) —
# pass --with-qdrant to stop that too.
#
# Usage: scripts/dev-down.sh [--with-qdrant]
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

WITH_QDRANT=0
for arg in "$@"; do
  case "$arg" in
    --with-qdrant) WITH_QDRANT=1 ;;
    *) err "Unknown option: $arg"; exit 1 ;;
  esac
done

stop_bg frontend
stop_bg backend

if [[ "$WITH_QDRANT" == "1" ]]; then
  if runtime="$(container_cmd)"; then
    if "$runtime" ps --format '{{.Names}}' 2>/dev/null | grep -qx "$QDRANT_CONTAINER"; then
      log "Stopping Qdrant container..."
      "$runtime" stop "$QDRANT_CONTAINER" >/dev/null
      ok "Qdrant stopped (data volume kept — $QDRANT_VOLUME)"
    else
      dim "Qdrant: not running"
    fi
  fi
else
  dim "Qdrant left running (pass --with-qdrant to stop it too)"
fi

ok "Down."

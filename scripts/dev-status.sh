#!/usr/bin/env bash
# Usage: scripts/dev-status.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

check() {
  local name="$1" url="$2"
  if is_running "$name"; then
    if curl -fsS --max-time 2 "$url" >/dev/null 2>&1; then
      ok "$name: running (pid $(cat "$RUN_DIR/$name.pid")), $url responding"
    else
      warn "$name: process running (pid $(cat "$RUN_DIR/$name.pid")) but $url not responding yet"
    fi
  else
    dim "$name: not running"
  fi
}

check backend "http://localhost:8001/models"
check frontend "http://localhost:3000"

if port_open 6333; then
  ok "qdrant: reachable on :6333"
else
  dim "qdrant: not reachable on :6333"
fi

OMLX_BASE_URL="http://127.0.0.1:8000/v1"
if [[ -f "$ROOT/backend/.env" ]]; then
  env_value="$(grep -E '^OMLX_BASE_URL=' "$ROOT/backend/.env" 2>/dev/null | tail -1 | cut -d= -f2-)" || true
  [[ -n "$env_value" ]] && OMLX_BASE_URL="$env_value"
fi
if curl -fsS --max-time 2 "${OMLX_BASE_URL%/}/models" >/dev/null 2>&1; then
  ok "oMLX: reachable at $OMLX_BASE_URL"
else
  dim "oMLX: not reachable at $OMLX_BASE_URL"
fi

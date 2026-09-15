#!/usr/bin/env bash
# Starts the local dev environment: Qdrant (container), backend, frontend.
# Does NOT start your oMLX model server — that's an external process this
# repo doesn't manage (see README.md) — but checks it's reachable and warns
# if not, since nothing else here will work without it.
#
# Usage: scripts/dev-up.sh [--no-qdrant]
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

SKIP_QDRANT=0
for arg in "$@"; do
  case "$arg" in
    --no-qdrant) SKIP_QDRANT=1 ;;
    *) err "Unknown option: $arg"; exit 1 ;;
  esac
done

# --- prerequisites -------------------------------------------------------

if ! [[ -x "$ROOT/backend/.venv/bin/python" ]]; then
  err "backend/.venv not found — run scripts/setup.sh first."
  exit 1
fi
if ! [[ -d "$ROOT/frontend/node_modules" ]]; then
  err "frontend/node_modules not found — run scripts/setup.sh first."
  exit 1
fi

# oMLX base URL: read from backend/.env if set, else the same default
# settings.py itself falls back to.
OMLX_BASE_URL="http://127.0.0.1:8000/v1"
if [[ -f "$ROOT/backend/.env" ]]; then
  env_value="$(grep -E '^OMLX_BASE_URL=' "$ROOT/backend/.env" 2>/dev/null | tail -1 | cut -d= -f2-)" || true
  [[ -n "$env_value" ]] && OMLX_BASE_URL="$env_value"
fi
if curl -fsS --max-time 2 "${OMLX_BASE_URL%/}/models" >/dev/null 2>&1; then
  ok "oMLX reachable at $OMLX_BASE_URL"
else
  warn "oMLX not reachable at $OMLX_BASE_URL — start your model server first, or the backend will hang on every request."
fi

# --- Qdrant ---------------------------------------------------------------

if [[ "$SKIP_QDRANT" == "1" ]]; then
  dim "Qdrant: skipped (--no-qdrant)"
elif port_open 6333; then
  ok "Qdrant already reachable on :6333"
else
  if runtime="$(container_cmd)"; then
    if "$runtime" ps -a --format '{{.Names}}' 2>/dev/null | grep -qx "$QDRANT_CONTAINER"; then
      log "Starting existing Qdrant container..."
      "$runtime" start "$QDRANT_CONTAINER" >/dev/null
    else
      log "Creating Qdrant container..."
      [[ "$runtime" == "podman" ]] && podman machine start >/dev/null 2>&1 || true
      "$runtime" run -d --name "$QDRANT_CONTAINER" -p 6333:6333 -p 6334:6334 \
        -v "$QDRANT_VOLUME:/qdrant/storage:z" docker.io/qdrant/qdrant:latest >/dev/null
    fi
    ok "Qdrant starting (container: $QDRANT_CONTAINER, runtime: $runtime)"
  else
    warn "Neither docker nor podman found — skipping Qdrant. agent_memory will fail until one's installed and reachable at :6333 (see README.md)."
  fi
fi

# --- backend ---------------------------------------------------------------

(cd "$ROOT/backend" && start_bg backend "$RUN_DIR/backend.log" .venv/bin/python main.py)

# --- frontend ---------------------------------------------------------------

(cd "$ROOT/frontend" && start_bg frontend "$RUN_DIR/frontend.log" npm run dev)

log ""
ok "Up. Frontend: http://localhost:3000  Backend: http://localhost:8001"
dim "scripts/dev-status.sh to check, scripts/dev-down.sh to stop, tail -f .dev/*.log for logs."

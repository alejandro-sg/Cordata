#!/usr/bin/env bash
# One-time setup: backend venv + pinned deps, frontend deps, .env files
# from their .example templates (never overwrites an existing .env).
# Re-running is safe — every step is idempotent.
#
# Usage: scripts/setup.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

log "== backend =="
cd "$ROOT/backend"
if ! [[ -x .venv/bin/python ]]; then
  log "Creating venv..."
  python3 -m venv .venv
fi
log "Installing pinned dependencies..."
.venv/bin/pip install -q --upgrade pip
.venv/bin/pip install -q -r requirements.txt
ok "backend deps installed"

if ! [[ -f .env ]]; then
  cp .env.example .env
  ok "backend/.env created from .env.example — edit it if your oMLX/Qdrant setup isn't the default"
else
  dim "backend/.env already exists, left untouched"
fi

log ""
log "== frontend =="
cd "$ROOT/frontend"
log "Installing dependencies (npm ci)..."
npm ci --silent
ok "frontend deps installed"

if ! [[ -f .env.local ]]; then
  cp .env.local.example .env.local
  ok "frontend/.env.local created from .env.local.example"
else
  dim "frontend/.env.local already exists, left untouched"
fi

log ""
ok "Setup complete."
dim "Next: make sure your oMLX model server is running, then scripts/dev-up.sh (or: make up)."

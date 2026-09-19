#!/usr/bin/env bash
# Usage: scripts/deploy-mini-frontend.sh
#
# Syncs frontend/ to the mini (ssh alias "sierralab-mini" — see ~/.ssh/config)
# and rebuilds its frontend container. Also invoked by deploy-mini.sh, which
# deploys both frontend and backend together.
#
# First run on a fresh mini: bootstraps docker-compose.yml from
# deploy/mini/frontend/ and stops to let you set BACKEND_URL before
# building. Every run after that leaves the remote docker-compose.yml
# alone, so a BACKEND_URL/SANDBOX_API_TOKEN you've customized there
# survives redeploys.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

MINI_HOST="${MINI_HOST:-sierralab-mini}"
MINI_DIR="${MINI_DIR:-~/docker/sierralab-agent-frontend}"

log "Syncing frontend/ to $MINI_HOST:$MINI_DIR ..."
ssh "$MINI_HOST" "mkdir -p $MINI_DIR"
rsync -az --delete \
  --exclude node_modules \
  --exclude .next \
  --exclude .env.local \
  --exclude tsconfig.tsbuildinfo \
  --exclude docker-compose.yml \
  "$ROOT/frontend/" "$MINI_HOST:$MINI_DIR/"

if ! ssh "$MINI_HOST" "test -f $MINI_DIR/docker-compose.yml"; then
  warn "No docker-compose.yml on $MINI_HOST yet — bootstrapping from deploy/mini/frontend/."
  rsync -az "$ROOT/deploy/mini/frontend/docker-compose.yml" "$MINI_HOST:$MINI_DIR/docker-compose.yml"
  err "Set BACKEND_URL in $MINI_DIR/docker-compose.yml on $MINI_HOST (edit there, not here), then re-run this script."
  exit 1
fi

log "Building + restarting container on $MINI_HOST ..."
ssh "$MINI_HOST" "cd $MINI_DIR && docker compose up -d --build"

ok "Deployed. Status:"
ssh "$MINI_HOST" "docker ps --filter name=sierralab-agent-frontend --format 'table {{.Names}}\t{{.Status}}\t{{.Ports}}'"

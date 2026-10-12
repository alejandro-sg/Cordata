#!/usr/bin/env bash
# Usage: scripts/deploy-mini-backend.sh
#
# Syncs backend/ to the mini (ssh alias "mini" — see
# ~/.ssh/config) and rebuilds its backend container. Also invoked by
# deploy-mini.sh, which deploys both frontend and backend together.
#
# Inference itself is NOT deployed here — OMLX_BASE_URL in the remote
# docker-compose.yml points back at whichever machine actually runs oMLX
# (this laptop, by default). This script only moves the agent loop + tool
# execution onto the mini so the web UI doesn't depend on this laptop being
# awake.
#
# First run on a fresh mini: bootstraps docker-compose.yml from
# deploy/mini/backend/ and the state/ dirs its volumes mount, including
# state/mcp_servers.json seeded as `{"mcpServers": {}}` — bind-mounting a
# file that doesn't exist yet on the host creates a directory there instead
# (breaking the mount), and an empty file is present but isn't valid JSON,
# which crashes mcp_manager.load_configured_mcp_clients() at import time
# (MCPClient.load_servers has no empty-file guard, only a missing-file one)
# — so this has to write real (if empty) JSON before the first
# `docker compose up`. Then stops to let you set OMLX_BASE_URL and
# SANDBOX_API_TOKEN before building. Every run after that leaves the remote
# docker-compose.yml alone, so values you've customized there survive
# redeploys. State dirs persist across redeploys either way — never synced
# from here, never deleted.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

MINI_HOST="${MINI_HOST:-mini}"
MINI_DIR="${MINI_DIR:-~/docker/cordata-agent-backend}"

log "Syncing backend/ to $MINI_HOST:$MINI_DIR ..."
ssh "$MINI_HOST" "mkdir -p $MINI_DIR/state/sessions $MINI_DIR/state/workspace $MINI_DIR/state/repl_state $MINI_DIR/state/skills_generated $MINI_DIR/state/hf-cache && [ -s $MINI_DIR/state/mcp_servers.json ] || echo '{\"mcpServers\": {}}' > $MINI_DIR/state/mcp_servers.json"
rsync -az --delete \
  --exclude .venv \
  --exclude __pycache__ \
  --exclude .env \
  --exclude sessions \
  --exclude workspace \
  --exclude repl_state \
  --exclude mcp_servers.json \
  --exclude docker-compose.yml \
  --exclude state \
  "$ROOT/backend/" "$MINI_HOST:$MINI_DIR/"

if ! ssh "$MINI_HOST" "test -f $MINI_DIR/docker-compose.yml"; then
  warn "No docker-compose.yml on $MINI_HOST yet — bootstrapping from deploy/mini/backend/."
  rsync -az "$ROOT/deploy/mini/backend/docker-compose.yml" "$MINI_HOST:$MINI_DIR/docker-compose.yml"
  err "Set OMLX_BASE_URL and SANDBOX_API_TOKEN in $MINI_DIR/docker-compose.yml on $MINI_HOST (edit there, not here — and match SANDBOX_API_TOKEN in the frontend's own compose file), then re-run this script."
  exit 1
fi

log "Building + restarting container on $MINI_HOST ..."
ssh "$MINI_HOST" "cd $MINI_DIR && docker compose up -d --build"

ok "Deployed. Status:"
ssh "$MINI_HOST" "docker ps --filter name=cordata-agent-backend --format 'table {{.Names}}\t{{.Status}}\t{{.Ports}}'"

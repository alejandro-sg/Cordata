#!/usr/bin/env bash
# Usage: scripts/deploy-mini.sh
#
# Deploys both halves of the mini-hosted deployment: backend first (the web
# frontend's own container depends on it being reachable), then frontend.
# Qdrant on the mini is a bare `qdrant/qdrant:latest` image
# (deploy/mini/qdrant/docker-compose.yml) with no source of its own, so
# there's nothing to redeploy there.
#
# Run scripts/deploy-mini-backend.sh or scripts/deploy-mini-frontend.sh
# directly to deploy just one side.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

./deploy-mini-backend.sh
./deploy-mini-frontend.sh

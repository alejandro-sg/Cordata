#!/usr/bin/env bash
# Usage: scripts/deploy-mini.sh
#
# Deploys the mini-hosted backend. Qdrant on the mini is a bare `qdrant/qdrant:latest` image
# (deploy/mini/qdrant/docker-compose.yml) with no source of its own, so there's nothing to redeploy there.
# (The Next.js web UI that used to deploy alongside it was retired.)
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

./deploy-mini-backend.sh

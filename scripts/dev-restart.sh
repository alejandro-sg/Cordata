#!/usr/bin/env bash
# Restarts backend + frontend (Qdrant untouched — it doesn't need to
# restart alongside the app, and stopping it would be needlessly slow).
#
# Usage: scripts/dev-restart.sh
DIR="$(dirname "${BASH_SOURCE[0]}")"
"$DIR/dev-down.sh"
"$DIR/dev-up.sh" --no-qdrant

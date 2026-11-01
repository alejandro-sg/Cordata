#!/usr/bin/env bash
# The pre-tool hook (.cordata/settings.json): exit 2 blocks the call and returns stderr to the model.
set -e
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
for kind in release debug; do
  bin="$DIR/../tui/target/$kind/cordata-guard"
  [[ -x "$bin" ]] && exec "$bin"
done
exit 0

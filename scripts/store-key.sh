#!/usr/bin/env bash
# Stores your Ollama API key in the macOS Keychain (where the backend reads it) and
# then VERIFIES it, so a mistyped or truncated paste is caught now rather than as a
# 401 mid-conversation.
#
#     make key        (or scripts/store-key.sh)
#
# The key is typed or pasted at a hidden prompt — it is never echoed, never put on a
# command line, and never written to a file. Re-running replaces the stored value.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

if ! command -v security >/dev/null 2>&1; then
  err "the macOS 'security' tool isn't available — export OLLAMA_API_KEY in your shell instead."
  exit 1
fi

log "Paste your Ollama API key (hidden), press Enter, then paste it again to confirm."
# `security -w` with no value prompts twice, with echo off; -U updates an existing item.
security add-generic-password -U -a "$USER" -s cordata-ollama-api-key -w || { err "not stored."; exit 1; }

echo
# A stale exported variable would still win over what was just stored, so the check
# must run the way the backend resolves the key; show it if one is set.
if [[ -n "${OLLAMA_API_KEY:-}${CLOUD_OPENAI_API_KEY:-}" ]]; then
  warn "OLLAMA_API_KEY / CLOUD_OPENAI_API_KEY is exported in this shell and OVERRIDES the Keychain — unset it (fish: set -e OLLAMA_API_KEY) or it will keep being used."
fi
exec "$(dirname "${BASH_SOURCE[0]}")/check-key.sh"

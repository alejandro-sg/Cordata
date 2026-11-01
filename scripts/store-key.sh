#!/usr/bin/env bash
# Stores a model provider's API key in the macOS Keychain, where Cordata reads it. For Ollama it then
# verifies the key, so a mistyped paste is caught now rather than as a 401 mid-conversation.
#
#     make key                    (Anthropic, the default model's provider)
#     make key PROVIDER=ollama
#
# The key is typed or pasted at a hidden prompt — it is never echoed, never put on a
# command line, and never written to a file. Re-running replaces the stored value.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

PROVIDER="${1:-anthropic}"
case "$PROVIDER" in
  anthropic) SERVICE=cordata-anthropic-api-key; VAR=ANTHROPIC_API_KEY ;;
  ollama)    SERVICE=cordata-ollama-api-key;    VAR=OLLAMA_API_KEY ;;
  *) err "unknown provider '$PROVIDER' (anthropic or ollama)"; exit 2 ;;
esac

if ! command -v security >/dev/null 2>&1; then
  err "the macOS 'security' tool isn't available — export $VAR in your shell instead."
  exit 1
fi

log "Paste your $PROVIDER API key (hidden), press Enter, then paste it again to confirm."
# `security -w` with no value prompts twice, with echo off; -U updates an existing item.
security add-generic-password -U -a "$USER" -s "$SERVICE" -w || { err "not stored."; exit 1; }

echo
# A stale exported variable would still win over what was just stored.
if [[ -n "${!VAR:-}" ]]; then
  warn "$VAR is exported in this shell and OVERRIDES the Keychain — unset it (fish: set -e $VAR) or it will keep being used."
fi
ok "stored in the macOS Keychain as $SERVICE"
[[ "$PROVIDER" == ollama ]] && exec "$(dirname "${BASH_SOURCE[0]}")/check-key.sh"
exit 0

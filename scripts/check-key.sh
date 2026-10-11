#!/usr/bin/env bash
# Diagnoses "401 Unauthorized" from the cloud model: finds the Ollama key exactly the
# way the backend does (exported variable first, then the macOS Keychain item
# "sierralab-ollama-api-key"), checks it for the usual mistakes, and asks Ollama
# whether it is accepted. NEVER prints the key — only its source, length and a short
# fingerprint (first 8 hex of its SHA-256) to compare against another copy.
#
#     scripts/check-key.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

key=""; source_name=""
if [[ -n "${OLLAMA_API_KEY:-}" ]]; then
  key="$OLLAMA_API_KEY"; source_name="exported OLLAMA_API_KEY (beats the Keychain)"
elif [[ -n "${CLOUD_OPENAI_API_KEY:-}" ]]; then
  key="$CLOUD_OPENAI_API_KEY"; source_name="exported CLOUD_OPENAI_API_KEY (beats the Keychain)"
elif command -v security >/dev/null 2>&1 && key="$(security find-generic-password -s cordata-ollama-api-key -w 2>/dev/null)" && [[ -n "$key" ]]; then
  source_name="macOS Keychain item cordata-ollama-api-key"
elif command -v security >/dev/null 2>&1 && key="$(security find-generic-password -s sierralab-ollama-api-key -w 2>/dev/null)" && [[ -n "$key" ]]; then
  source_name="macOS Keychain item sierralab-ollama-api-key"
else
  err "no key found: nothing exported and no Keychain item. Store it once:"
  err '    security add-generic-password -a "$USER" -s cordata-ollama-api-key -w'
  exit 1
fi

fingerprint="$(printf %s "$key" | shasum -a 256 | cut -c1-8)"
log "source      : $source_name"
log "length      : ${#key} characters"
log "fingerprint : $fingerprint"

problems=0
if [[ "$key" =~ [[:space:]] ]];        then err "contains whitespace or a newline — re-paste it without trailing spaces/newline"; problems=1; fi
if [[ "$key" == \"* || "$key" == \'* ]]; then err "starts with a quote character — store the bare key, without quotes"; problems=1; fi
if [[ "$key" =~ ^(YOUR_API_KEY|\.\.\.|changeme|xxx+)$ || ${#key} -lt 40 ]]; then err "looks like a placeholder or truncated key (real Ollama keys are ~57 characters)"; problems=1; fi

# /api/tags is public (answers 200 to anyone), so it can't validate a key. An empty
# POST to the chat endpoint can: a valid key gets 400 (bad request), a bad/missing
# one 401 — and it costs no tokens.
code="$(curl -s -m 15 -o /dev/null -w '%{http_code}' -X POST -H "Authorization: Bearer $key" -H 'Content-Type: application/json' -d '{}' https://ollama.com/v1/chat/completions || true)"
case "$code" in
  400|200) ok "Ollama accepted the key (HTTP $code)";;
  401|403) err "Ollama REJECTED the key (HTTP $code)"; problems=1
           log "  → a stale exported variable? run:  env | grep -i -E 'OLLAMA|CLOUD_OPENAI'   (fish: set -x | grep -i -E 'ollama|cloud_openai')"
           log "  → otherwise re-store the right key:  security add-generic-password -U -a \"\$USER\" -s sierralab-ollama-api-key -w";;
  000) warn "could not reach ollama.com (network?) — key not verified";;
  *)   warn "unexpected HTTP $code from ollama.com";;
esac
exit $problems

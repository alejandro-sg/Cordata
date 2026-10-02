#!/usr/bin/env bash
# Runs a command with the cloud-model profile — Ollama Cloud, cheapest model
# that does reliable tool calling — so you don't export anything by hand:
#
#     scripts/cloud-env.sh <command> [args...]
#     make tui-cloud            make stress-cloud SCENARIOS=...
#
# Provider, endpoint and model are not secrets and are set here (override any
# of them in your shell: CLOUD_OPENAI_MODEL_ID=nemotron-3-super make tui-cloud).
# The API key is the one thing that must NOT live in a project file — the
# agent's own file tools can read backend/.env — so it comes from, in order:
#   1. OLLAMA_API_KEY / CLOUD_OPENAI_API_KEY already exported in your shell
#   2. the macOS Keychain item "sierralab-ollama-api-key" (one-time setup below)
# and is exported only into the child process this script execs. The backend
# then removes it from its own environment and masks it in tool output
# (backend/secret_guard.py; THREAT_MODEL.md, "Provider API keys").
#
# One-time Keychain setup (prompts for the key; nothing is echoed or stored in a file):
#     security add-generic-password -a "$USER" -s sierralab-ollama-api-key -w
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

if [[ $# -eq 0 ]]; then
  err "usage: scripts/cloud-env.sh <command> [args...]"
  exit 2
fi

# Cheapest verified: gpt-oss:20b ($0.07 in / $0.30 out per 1M tokens), ~1 s, tool
# calls work. nemotron-3-super is cheaper on input ($0.015) — see docs/laya-triage.md.
export MODEL_PROVIDER=openai_compatible
export CLOUD_OPENAI_BASE_URL="${CLOUD_OPENAI_BASE_URL:-https://ollama.com/v1}"
export CLOUD_OPENAI_MODEL_ID="${CLOUD_OPENAI_MODEL_ID:-gpt-oss:20b}"

if [[ -z "${OLLAMA_API_KEY:-}" && -z "${CLOUD_OPENAI_API_KEY:-}" ]]; then
  if command -v security >/dev/null 2>&1 \
     && key="$(security find-generic-password -s sierralab-ollama-api-key -w 2>/dev/null)" \
     && [[ -n "$key" ]]; then
    export OLLAMA_API_KEY="$key"
    unset key
  else
    err "no Ollama API key found. Either export OLLAMA_API_KEY in this shell, or store it once in the macOS Keychain:"
    err '    security add-generic-password -a "$USER" -s sierralab-ollama-api-key -w'
    err "(it prompts for the key; it is never written to a project file)"
    exit 1
  fi
fi

dim "cloud profile: $CLOUD_OPENAI_MODEL_ID @ $CLOUD_OPENAI_BASE_URL (key from $([[ -n "${OLLAMA_API_KEY:-}" ]] && echo environment/Keychain || echo CLOUD_OPENAI_API_KEY); not shown)"
exec "$@"

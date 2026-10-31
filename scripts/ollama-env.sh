#!/usr/bin/env bash
# Runs a command with the Ollama Cloud *through the local daemon* profile — the
# Experimental Lab's provider (backend/ollama_cloud.py):
#
#     scripts/ollama-env.sh <command> [args...]
#     make tui-lab
#
# Cloud models (mistral-large-4, minimax-m3.1-flash, gemma4:31b, kimi-k2.7-code, …) are reached at the signed-in local
# Ollama daemon's OpenAI-compatible proxy, http://localhost:11434/v1, with a dummy
# key — so, unlike scripts/cloud-env.sh, there is no API key to find, store or leak.
# The daemon authenticates to Ollama with its own `ollama signin` identity.
#
# Nothing is pulled: every registered model is a `<name>:cloud` tag (`ollama list` stays empty).
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

if [[ $# -eq 0 ]]; then
  err "usage: scripts/ollama-env.sh <command> [args...]"
  exit 2
fi

export MODEL_PROVIDER=ollama_cloud
export OLLAMA_BASE_URL="${OLLAMA_BASE_URL:-http://localhost:11434/v1}"

if ! curl -fsS -m 3 "${OLLAMA_BASE_URL%/v1}/api/version" >/dev/null 2>&1; then
  err "the Ollama daemon is not answering at ${OLLAMA_BASE_URL%/v1}."
  err "    start it:   ollama serve        sign in:   ollama signin"
  exit 1
fi

dim "ollama_cloud profile: $OLLAMA_BASE_URL (orchestrator gemma4:31b @ medium, architect mistral-large-4 @ high, debugger mistral-large-4 @ max, observer minimax-m3.1-flash @ low; override with OLLAMA_ROLE_<ROLE>=<model>[:low|medium|high|max])"
dim "F2 or /lab opens the Experimental Lab"
exec "$@"

#!/usr/bin/env bash
# One-time setup: the backend's Python environment with pinned dependencies, backend/.env from its template (never
# overwritten), and a release build of the terminal client. Re-running is safe — every step is idempotent.
#
#   scripts/setup.sh           # what the terminal client and `cordata run` need
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

log "== backend =="
cd "$ROOT/backend"
if ! [[ -x .venv/bin/python ]]; then
  log "Creating venv..."
  python3 -m venv .venv
fi
log "Installing pinned dependencies..."
.venv/bin/pip install -q --upgrade pip
.venv/bin/pip install -q -r requirements.txt
ok "backend deps installed"

if ! [[ -f .env ]]; then
  cp .env.example .env
  ok "backend/.env created from .env.example — choose your model provider in it (README: \"Pick a model\")"
else
  dim "backend/.env already exists, left untouched"
fi

log ""
log "== terminal client =="
if command -v cargo >/dev/null 2>&1; then
  (cd "$ROOT/tui" && cargo build --release -p acp-tui 2>&1 | tail -1)
  ok "acp-tui built"
else
  warn "cargo not found — install Rust (https://rustup.rs), then re-run this script"
fi

log ""
ok "Setup complete."
dim "Next: make cli   (puts \`cordata\` on your PATH), then cordata doctor."

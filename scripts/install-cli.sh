#!/usr/bin/env bash
# Install the `orchestrator`, `sierra`, and `acp-tui` CLIs to ~/.local/bin
# so they can be invoked from any project directory or terminal.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="${HOME}/.local/bin"

mkdir -p "$TARGET_DIR"

if [[ ! -x "$REPO_ROOT/tui/target/release/acp-tui" ]]; then
  echo "Building release acp-tui binary..."
  (cd "$REPO_ROOT/tui" && cargo build --release -p acp-tui)
fi

ln -sf "$REPO_ROOT/tui/target/release/acp-tui" "$TARGET_DIR/acp-tui"
ln -sf "$REPO_ROOT/bin/orchestrator" "$TARGET_DIR/orchestrator"
ln -sf "$REPO_ROOT/bin/orchestrator" "$TARGET_DIR/sierra"
ln -sf "$REPO_ROOT/bin/orchestrator" "$TARGET_DIR/cordata"

chmod +x "$REPO_ROOT/bin/orchestrator"
chmod +x "$TARGET_DIR/acp-tui"
chmod +x "$TARGET_DIR/orchestrator"
chmod +x "$TARGET_DIR/sierra"
chmod +x "$TARGET_DIR/cordata"

echo "✓ Installed cordata, orchestrator, sierra, and acp-tui to $TARGET_DIR"
echo ""
echo "Interactive Assistant (Claude Code & Antigravity style):"
echo "  cordata                             # Start interactive TUI session"
echo "  cordata \"what is this project\"      # Start TUI with initial query"
echo ""
echo "Autonomous Multi-Worker Orchestration:"
echo "  cordata status                      # Headroom & recent runs"
echo "  cordata run \"Goal\"                  # Multi-worker batch run"
echo ""
if [[ ":$PATH:" != *":$TARGET_DIR:"* ]]; then
  echo "! Note: $TARGET_DIR is not yet in your PATH."
  echo "  Add it to your shell profile (~/.zshrc):"
  echo "    export PATH=\"\$HOME/.local/bin:\$PATH\""
fi

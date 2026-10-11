#!/usr/bin/env bash
# Install the `orchestrator` and `sierra` CLIs to ~/.local/bin
# so they can be invoked from any project directory or terminal.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="${HOME}/.local/bin"

mkdir -p "$TARGET_DIR"

ln -sf "$REPO_ROOT/bin/orchestrator" "$TARGET_DIR/orchestrator"
ln -sf "$REPO_ROOT/bin/orchestrator" "$TARGET_DIR/sierra"

chmod +x "$REPO_ROOT/bin/orchestrator"
chmod +x "$TARGET_DIR/orchestrator"
chmod +x "$TARGET_DIR/sierra"

echo "✓ Installed orchestrator and sierra to $TARGET_DIR"
echo ""
echo "Try running:"
echo "  orchestrator --help"
echo "  orchestrator status"
echo "  orchestrator \"Add retries to the HTTP client and test them\""
echo ""
if [[ ":$PATH:" != *":$TARGET_DIR:"* ]]; then
  echo "! Note: $TARGET_DIR is not yet in your PATH."
  echo "  Add it to your shell profile (~/.zshrc):"
  echo "    export PATH=\"\$HOME/.local/bin:\$PATH\""
fi

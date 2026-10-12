#!/usr/bin/env bash
# Put `cordata` on your PATH: one symlink in ~/.local/bin to bin/cordata, and a release build of the terminal client.
#
#   make cli                       # or: scripts/install-cli.sh
#   scripts/install-cli.sh --uninstall
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="${HOME}/.local/bin"
LINK="$TARGET_DIR/cordata"

if [[ "${1:-}" == "--uninstall" ]]; then
  rm -f "$LINK"
  echo "removed $LINK"
  exit 0
fi

mkdir -p "$TARGET_DIR"
chmod +x "$REPO_ROOT/bin/cordata"

if [[ ! -x "$REPO_ROOT/tui/target/release/acp-tui" ]]; then
  echo "Building the terminal client (release)…"
  (cd "$REPO_ROOT/tui" && cargo build --release -p acp-tui)
fi

ln -sf "$REPO_ROOT/bin/cordata" "$LINK"
echo "✓ cordata → $LINK"

if [[ ":$PATH:" != *":$TARGET_DIR:"* ]]; then
  echo ""
  echo "! $TARGET_DIR is not on your PATH. Add it:"
  echo "    zsh/bash:  echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.zshrc"
  echo "    fish:      fish_add_path ~/.local/bin"
fi

echo ""
echo "  cordata                     interactive session in the current folder"
echo "  cordata \"fix the bug\"       …starting with a prompt"
echo "  cordata run \"goal\"          autonomous multi-worker run"
echo "  cordata doctor              check that everything is in place"

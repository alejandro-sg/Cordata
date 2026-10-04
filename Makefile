.PHONY: setup up down restart status logs daemon daemon-install daemon-uninstall daemon-status deploy-mini deploy-mini-frontend deploy-mini-backend tui tui-build tui-test stress tui-cloud stress-cloud key check-key laya-watch settings

setup:
	@scripts/setup.sh

up:
	@scripts/dev-up.sh

down:
	@scripts/dev-down.sh

restart:
	@scripts/dev-restart.sh

status:
	@scripts/dev-status.sh

logs:
	@tail -f .dev/backend.log .dev/frontend.log

deploy-mini:
	@scripts/deploy-mini.sh

deploy-mini-frontend:
	@scripts/deploy-mini-frontend.sh

deploy-mini-backend:
	@scripts/deploy-mini-backend.sh

# The always-on orchestrator daemon (backend/daemon/). `daemon` runs it in the
# foreground; `daemon-install` registers the LaunchAgent that keeps it up.
# Sessions are one acp_server.py child per project; the TUI attaches with
# `make tui ARGS="--transport ws"`.
daemon:
	@scripts/daemon.sh run $(ARGS)

daemon-status:
	@scripts/daemon.sh status

daemon-install:
	@scripts/daemon.sh install

daemon-uninstall:
	@scripts/daemon.sh uninstall

# Rust ACP clients (tui/) — see tui/README.md.
tui:
	@cd tui && cargo run -q -p acp-tui -- $(ARGS)

tui-build:
	@cd tui && cargo build --release

tui-test:
	@cd tui && cargo test --workspace

stress:
	@cd tui && cargo run -q -p acp-stress -- $(or $(SCENARIOS),scenarios/smoke.json) $(ARGS)

# Same, on the cloud-model profile (scripts/cloud-env.sh): Ollama Cloud,
# cheapest tool-calling model; key from your shell or the macOS Keychain.
tui-cloud:
	@scripts/cloud-env.sh $(MAKE) --no-print-directory tui

stress-cloud:
	@scripts/cloud-env.sh $(MAKE) --no-print-directory stress

# Store the Ollama API key in the macOS Keychain (hidden prompt) and verify it.
key:
	@scripts/store-key.sh

# Where is the key coming from, and does Ollama accept it? (never prints the key)
check-key:
	@scripts/check-key.sh

# Live view of Laya's per-turn decisions (any front door). acp-tui shows them inline.
laya-watch:
	@backend/.venv/bin/python backend/_laya_triage_watch.py $(ARGS)

# What the backend will use for a project: merged settings, which file set each,
# permission rules, warnings (docs/settings.md).   make settings DIR=~/some/project
settings:
	@backend/.venv/bin/python backend/settings_store.py $(or $(DIR),.)

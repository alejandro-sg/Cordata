.PHONY: setup up down restart status logs deploy-mini deploy-mini-frontend deploy-mini-backend tui tui-build tui-test stress tui-cloud stress-cloud key check-key laya-watch

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

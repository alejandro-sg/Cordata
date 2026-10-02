.PHONY: setup up down restart status logs deploy-mini deploy-mini-frontend deploy-mini-backend tui tui-build tui-test stress

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

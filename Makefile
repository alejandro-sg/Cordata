.PHONY: setup up down restart status logs deploy-mini deploy-mini-frontend deploy-mini-backend

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

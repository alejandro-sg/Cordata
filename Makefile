.PHONY: app app-run app-test app-check app-selftest phase12-test gate-live setup up down restart status logs daemon daemon-install daemon-uninstall daemon-status deploy-mini deploy-mini-backend tui tui-build tui-test proto evals evals-validate learning-test laya-test laya-export laya-eval stress tui-cloud tui-lab test orchestrator-test evals-orchestration stress-cloud key check-key discord-key laya-watch settings install-cli cli secrets hooks

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
	@tail -f .dev/backend.log

deploy-mini:
	@scripts/deploy-mini.sh

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

# Cordata.app, the native workspace (app/, docs/app.md). `app` builds, signs and self-tests the bundle into
# app/target/Cordata.app (SIGN_IDENTITY="Cordata Local" keeps macOS privacy grants across rebuilds; INSTALL=1 copies it
# to ~/Applications); `app-run` runs it from the tree against the daemon's hub (ARGS="--transport stdio" for a child).
app:
	@scripts/package-app.sh $(if $(INSTALL),--install,)

app-run:
	@cd app && cargo run -q -p cordata-app -- $(ARGS)

# The shared state (cordata-view, also the terminal's), the palette's WCAG checks, the app's headless tests.
app-test:
	@cd app && cargo test --workspace --features cordata-view/crossterm

# A scripted turn through the real window against acp-core's fake agent: connect, stream, approve, finish.
app-selftest:
	@cd app && cargo build -q -p cordata-app && ACP_TUI_BACKEND_ROOT=$(CURDIR) CORDATA_APP_SERVER_SCRIPT=$(CURDIR)/tui/crates/acp-core/tests/fixtures/fake_agent.py \
		target/debug/cordata-app --transport stdio --prompt "harness" --selftest

# Lints and the supply chain: clippy (warnings are errors), cargo-deny (licences, bans, sources, advisories) and the
# proof that the gpui-pre crates are zed@93d7456 (network once).
app-check:
	@cd app && cargo clippy --workspace --all-targets -- -D warnings
	@cd app && cargo deny check
	@app/scripts/verify-gpui-snapshot.sh

# Rust ACP clients (tui/) — see tui/README.md.
tui:
	@cd tui && cargo run -q -p acp-tui -- $(ARGS)

tui-build:
	@cd tui && cargo build --release

tui-test:
	@cd tui && cargo test --workspace

# Regenerate the Python gRPC stubs after editing proto/harness.proto.
proto:
	@scripts/gen-proto.sh

# The eval suite (backend/evals/, docs/evals.md): 42 tasks with verifiers, a held-out third, and the
# attack suite that guards Laya Guard. Default driver is the oracle — offline, deterministic, no model.
#   make evals                      the whole suite, oracle driver
#   make evals ARGS="--driver replay --suite heldout"
#   make evals-validate             every task fails before the work and passes after the oracle
evals:
	@cd backend && PYTHONPATH=. .venv/bin/python -m evals run --suite all $(ARGS)

evals-validate:
	@cd backend && PYTHONPATH=. .venv/bin/python -m evals validate

# The learning loop (backend/bandit.py, backend/reflect.py, backend/promote.py, backend/prompts.py,
# backend/optimize.py — docs/agentic-harness.md): the contextual bandit that learns which harness choice to make
# from turn outcomes, and the reflection/promotion/optimization loop over learned skills and prompt overlays.
# Local only — the gate runs the eval suite, which imports the ACP server and loads the local embedding model at
# import, so it is not in CI (same reason as _acp_smoke_test.py).
learning-test:
	@backend/.venv/bin/python backend/_bandit_smoke_test.py
	@backend/.venv/bin/python backend/_learning_smoke_test.py
	@backend/.venv/bin/python backend/_optimize_smoke_test.py

# Phase 11/12 (system_audit_report.md): the audit's fixes, code navigation, preflight, rejection memory and
# environment awareness. Hermetic (scripted models), also in CI.
phase12-test:
	@backend/.venv/bin/python backend/_phase11_hardening_smoke_test.py
	@backend/.venv/bin/python backend/_code_nav_smoke_test.py
	@backend/.venv/bin/python backend/_preflight_smoke_test.py
	@backend/.venv/bin/python backend/_rejection_memory_smoke_test.py
	@backend/.venv/bin/python backend/_environment_awareness_smoke_test.py

# Fails if a credential is in a tracked or not-ignored file (scripts/check-secrets.py). `make secrets ARGS=--history`
# also reads every commit. CI runs it on every push; `make hooks` runs the staged-change version before each commit.
secrets:
	@python3 scripts/check-secrets.py $(ARGS)

hooks:
	@git config core.hooksPath .githooks && echo "pre-commit secret check enabled (.githooks/pre-commit)"

# Every hermetic smoke suite, in parallel (backend/run_tests.py): each in its own throwaway CORDATA_HOME, longest
# first, Discord off.   make test ARGS="-k orchestr"   make test ARGS="-j 4"   make test ARGS="--live"
test:
	@backend/.venv/bin/python backend/run_tests.py $(ARGS)

# The orchestrator (docs/orchestrator-plan.md): the Worker interface and its adapters, the quota ledger and safe-point
# handoffs, the plan → execute → harden → audit machine, and the dispatch policy. Hermetic: fake CLIs under the real
# Seatbelt sandbox, scripted models, a throwaway CORDATA_HOME.
orchestrator-test:
	@backend/.venv/bin/python backend/_worker_conformance_smoke_test.py
	@backend/.venv/bin/python backend/_quota_ledger_smoke_test.py
	@backend/.venv/bin/python backend/_orchestrator_machine_smoke_test.py
	@backend/.venv/bin/python backend/_dispatch_smoke_test.py
	@backend/.venv/bin/python backend/_orchestration_eval_smoke_test.py
	@backend/.venv/bin/python backend/_orchestrate_tool_smoke_test.py
	@backend/.venv/bin/python backend/_front_door_status_smoke_test.py

# The orchestration eval suite: the whole machine on every coding task of the eval catalog, simulated providers.
#   make evals-orchestration ARGS="--policy learned --world ceiling"
evals-orchestration:
	@cd backend && PYTHONPATH=. .venv/bin/python -m evals orchestrate $(ARGS)

# The verification gate against the configured REAL model (spends cloud tokens; Discord off, Laya off, a throwaway
# CORDATA_HOME). Prints what the model did.   make gate-live ARGS="1 3"
gate-live:
	@cd backend && CORDATA_DISCORD=off .venv/bin/python _phase12_gate_live_test.py $(ARGS)

# The trajectory export and the checkpoint registry. Hermetic — no model, no network, so unlike learning-test
# this one runs in CI.
laya-test:
	@backend/.venv/bin/python backend/_trajectories_smoke_test.py

# The trajectory pipeline (backend/trajectory.py, backend/laya_checkpoints.py — docs/laya-triage.md): the
# ledger and Laya's triage log joined into the four row shapes a trainer reads, and the registry that gates a
# fine-tuned checkpoint exactly like a staged skill.   make laya-export ARGS="--project ~/proj --validate"
laya-export:
	@cd backend && PYTHONPATH=. .venv/bin/python trajectory.py export $(ARGS)

# The A/B gate over candidate checkpoints. Default is shadow: it records the verdict on each and activates
# nothing.   make laya-eval ARGS="--mode live"
laya-eval:
	@cd backend && PYTHONPATH=. .venv/bin/python laya_checkpoints.py evaluate $(ARGS)

stress:
	@cd tui && cargo run -q -p acp-stress -- $(or $(SCENARIOS),scenarios/smoke.json) $(ARGS)

# Same, on the cloud-model profile (scripts/cloud-env.sh): Ollama Cloud,
# cheapest tool-calling model; key from your shell or the macOS Keychain.
tui-cloud:
	@scripts/cloud-env.sh $(MAKE) --no-print-directory tui

stress-cloud:
	@scripts/cloud-env.sh $(MAKE) --no-print-directory stress

# The Experimental Lab (docs/experimental-lab.md): Ollama Cloud models through the
# *local* signed-in Ollama daemon — no API key, nothing pulled. F2 / /lab in the TUI.
tui-lab:
	@scripts/ollama-env.sh $(MAKE) --no-print-directory tui

# Store the Ollama API key in the macOS Keychain (hidden prompt) and verify it.
key:
	@scripts/store-key.sh

# Where is the key coming from, and does Ollama accept it? (never prints the key)
check-key:
	@scripts/check-key.sh

# Store the Discord bot token in the macOS Keychain (hidden prompt) and verify it against
# Discord. Enables the daemon's remote front door once ALLOWED_USER_ID and
# TELEMETRY_CHANNEL_ID are set too — see backend/.env.example and THREAT_MODEL.md.
discord-key:
	@scripts/store-discord-key.sh

# Live view of Laya's per-turn decisions (any front door). acp-tui shows them inline.
laya-watch:
	@backend/.venv/bin/python backend/_laya_triage_watch.py $(ARGS)

# What the backend will use for a project: merged settings, which file set each,
# permission rules, warnings (docs/settings.md).   make settings DIR=~/some/project
settings:
	@backend/.venv/bin/python backend/settings_store.py $(or $(DIR),.)

# Put `cordata` on your PATH (a symlink in ~/.local/bin)
install-cli cli:
	@scripts/install-cli.sh


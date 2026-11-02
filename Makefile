.PHONY: install app app-run app-test app-check tui tui-build tui-test key check-key secrets hooks verify install-cli cli

# One step for a fresh clone: release binaries, `cordata` on your PATH, then the environment check.
install: tui-build cli
	@bin/cordata doctor

# Cordata.app, the native workspace (app/, docs/app.md). `app` builds, signs and self-tests the bundle into
# app/target/Cordata.app (SIGN_IDENTITY="Cordata Local" keeps macOS privacy grants across rebuilds; INSTALL=1 copies it
# to ~/Applications).
app:
	@scripts/package-app.sh $(if $(INSTALL),--install,)

app-run:
	@cd app && cargo run -q -p cordata-app -- $(ARGS)

# The shared state (cordata-view, also the terminal's), the palette's WCAG checks, the app's headless tests.
app-test:
	@cd app && cargo test --workspace --features cordata-view/crossterm

# Lints and the supply chain: clippy (warnings are errors), cargo-deny (licences, bans, sources, advisories) and the
# proof that the gpui-pre crates are zed@93d7456 (network once).
app-check:
	@cd app && cargo clippy --workspace --all-targets -- -D warnings
	@cd app && cargo deny check
	@app/scripts/verify-gpui-snapshot.sh

# Rust clients and tools (tui/) — see tui/README.md.
tui:
	@cd tui && cargo run -q -p acp-tui -- $(ARGS)

tui-build:
	@cd tui && cargo build --release

tui-test:
	@cd tui && cargo test --workspace

# Store a provider's API key in the macOS Keychain (hidden prompt): PROVIDER=anthropic (default) or ollama.
key:
	@scripts/store-key.sh $(PROVIDER)

# Where is the Ollama key coming from, and does Ollama accept it? (never prints the key)
check-key:
	@scripts/check-key.sh

# Fails if a credential is in a tracked or not-ignored file (tui/crates/cordata-guard). `make secrets ARGS=--history`
# also reads every commit. CI runs it on every push; `make hooks` runs the staged-change version before each commit.
secrets:
	@cd tui && cargo run -q -p cordata-guard --bin cordata-secrets -- $(ARGS)

hooks:
	@git config core.hooksPath .githooks && echo "pre-commit secret check enabled (.githooks/pre-commit)"

verify:
	@scripts/verify.sh $(ARGS)

# Put `cordata` on your PATH (a symlink in ~/.local/bin)
install-cli cli:
	@scripts/install-cli.sh

# Verifying Cordata (100% Pure Rust Architecture)

Cordata runs as a pure Rust workspace with zero Python or daemon dependencies:
- **Terminal Workspace (`tui/`)**: `acp-tui` (interactive chat), `cordata-run` (batch orchestrator), `cordata-doctor` (diagnostics), `cordata-guard` (secrets and safety hooks).
- **Native macOS Workspace (`app/`)**: `cordata-app` (GPUI native application), `cordata-view`, `cordata-theme`.
- **Token Economizer (`tui/crates/cordata-economizer`)**: Dynamic tool pruning, micro-skills, loopguard, and cache alignment.

---

## 1. Deterministic Verification

Always run the unified deterministic verification script:

```bash
# Standard fast verification (secrets scan + all TUI workspace tests)
scripts/verify.sh

# Comprehensive verification (secrets scan + TUI workspace tests + native GPUI app tests)
scripts/verify.sh --all
```

Exit code `0` is the only acceptable proof of completion.

---

## 2. Granular Verification Targets

| Target | Command | Purpose |
|---|---|---|
| **Secret Scan** | `make secrets` | Scans workspace for exposed API keys, tokens, or credentials |
| **TUI Tests** | `make tui-test` | Runs all 10 crates in `tui/` (340+ unit/integration tests) |
| **Native App Tests** | `make app-test` | Runs GPUI native app and view tests (`app/`) |
| **Release Build** | `make tui-build` | Builds release binaries (`acp-tui`, `cordata-run`, `cordata-doctor`, `cordata-guard`) |
| **System Diagnostics**| `bin/cordata doctor` | Verifies environment, CLIs, Keychain credentials, and sandbox |
| **Orchestrator Status**| `bin/cordata status` | Verifies quota pools and recent autonomous runs in `state.db` |

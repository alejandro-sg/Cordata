# AGENTS.md — Cordata Workspace Index

Index and operational invariants for coding agents working on Cordata. Keep this file under 200 lines; load details on demand via progressive disclosure.

---

## 1. Non-Guessable Commands

| Command | Purpose |
|---|---|
| `make tui-build` | Builds the release binaries `cordata` runs (`acp-tui`, `cordata-run`, `cordata-doctor`, `cordata-guard`) |
| `make cli` | Symlinks `cordata` binary to `~/.local/bin/cordata` |
| `make tui-test` | Runs the Rust tests for the terminal workspace (`tui/`) |
| `make app-test` | Runs Rust tests for native GPUI macOS application (`cordata-app`) |
| `cordata doctor` | Inspects environment, CLIs, model providers, and Seatbelt sandbox |
| `make secrets` | Scans workspace for exposed API keys, tokens, or credentials (`cordata-secrets`) |

---

## 2. Invariants & Persistent Gotchas

1. **Rust only**: the agent, orchestrator, ledger and checkpoints are Rust (`tui/`, `app/`). No Python, no daemon, no Discord. `proto/` holds only a shared test fixture.
2. **Zero Plaintext Secrets**: Never write API keys or tokens into `.env`, code, or tests. Keys reside exclusively in macOS Keychain via `make key` or `scripts/store-key.sh`.
3. **No Unfenced File Reads**: Large files over 500 lines trigger outline-only views (`context.outlineLines = 500`). Over-limit tool outputs (>6,000 chars) are offloaded to `~/.cordata/offloaded/`.
4. **Hermetic Testing**: `cargo test` must pass offline with no model and no provider key. Tests that need a real CLI or sandbox skip when it is absent.
5. **No Hallucinated Binary Behavior**: Use the `rea` MCP server (`inspect_macho`, `inspect_signature`) to verify native arm64 Mach-O binaries, load commands, and entitlements. Do not decompile whole multi-hundred-megabyte binaries with Hopper (`search_strings` times out at 330s).

---

## 3. Autonomous Execution Rules

### The 3-Part Brief
Format initial work units into three strict components:
- **Task**: The exact functional requirement.
- **Guardrails**: Hard invariants and forbidden edits (e.g., no new workspace dependencies).
- **Definition of Done**: A deterministic command that exits 0 upon success.
After completion, deliver an artifact or visual diff report detailing changes and assumptions.

### Verification Scripts over Prompting
Never instruct the model to "double-check" its work. Translate acceptance criteria into deterministic scripts:
- Run `scripts/verify.sh` or `make tui-test`.
- Hard Pass (exit 0) or Fail is the only acceptable proof of completion.

### Autonomous Looping with `/goal`
To run iterative remediation without interactive prompt loops:
- Run `/goal "<condition>" --check "<command>" --turns N` (e.g. `/goal "tests pass" --check "make tui-test" --turns 8`).
- The harness runs `--check` natively without spending tokens.
- Turns are hard-capped at 30 (`DEFAULT_MAX_TURNS = 8`).

### Deterministic Hooks, No Shouting
Do not rely on ALL CAPS or prompt pleadings. Invariants are enforced physically:
- Pre-tool lifecycle hooks (`hooks.preToolUse`) execute `scripts/guard.sh`.
- Exit code `2` blocks the tool execution and returns `stderr` directly to the model as the blocker reason.

### Sub-Agent Orchestration & The Goldfish Review
- Main chat context must remain clean. Heavy research, external fetch, and binary analysis should run in sub-agents.
- Sub-agents are capped at depth 1 with no recursive egress (`orchestrate` disallowed for child pods).
- Parallel wave syntheses run in isolated APFS clonefile sandboxes (`tui/crates/cordata-orchestrator/src/synthesizer.rs`).
- **The Goldfish Pattern**: The primary agent accumulates conversational bias ("Elephant"). For post-implementation review, dispatch a fresh, stateless sub-agent ("Goldfish") with zero conversation history to audit the diff against invariants and surface unhandled edge cases. Run via `/goldfish` or on-demand subagent.

---

## 4. Progressive Disclosure Directory

Only read these documents when their specific domain is relevant:

| Domain | Document Reference |
|---|---|
| Architecture & Layers | [ARCHITECTURE.md](file:///Users/asierra-llm-mbp/Development/Cordata/ARCHITECTURE.md) |
| Threat Model & Sandbox | [THREAT_MODEL.md](file:///Users/asierra-llm-mbp/Development/Cordata/THREAT_MODEL.md) |
| Configuration & Settings | [docs/settings.md](file:///Users/asierra-llm-mbp/Development/Cordata/docs/settings.md) |
| Multi-Agent Orchestrator | [docs/orchestrator.md](file:///Users/asierra-llm-mbp/Development/Cordata/docs/orchestrator.md) |
| TUI & Command Deck | [tui/README.md](file:///Users/asierra-llm-mbp/Development/Cordata/tui/README.md) |
| Native App Architecture | [docs/app.md](file:///Users/asierra-llm-mbp/Development/Cordata/docs/app.md) |

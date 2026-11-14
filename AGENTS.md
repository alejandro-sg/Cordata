# AGENTS.md — Cordata Workspace Index

Index and operational invariants for coding agents working on Cordata. Keep this file under 200 lines; load details on demand via progressive disclosure.

---

## 1. Non-Guessable Commands

| Command | Purpose |
|---|---|
| `make tui-build` | Builds the release binaries `cordata` runs (`acp-tui`, `cordata-run`, `cordata-doctor`) |
| `make cli` | Symlinks `cordata` binary to `~/.local/bin/cordata` |
| `make tui-test` | Runs the Rust tests for the terminal workspace (`tui/`) |
| `make app-test` | Runs Rust tests for native GPUI macOS application (`cordata-app`) |
| `cordata doctor` | Inspects environment, CLIs, model providers, and Seatbelt sandbox |
| `cordata doctor --docs` | Lists `ARCHITECTURE.md` / `docs/*.md` sections whose `<!-- covers: paths @ rev -->` files changed since review; `--stamp` marks them reviewed at HEAD, `--suggest` proposes `covers:` paths for unmarked sections from the repo map |
| `make secrets` | Scans workspace for exposed API keys, tokens, or credentials (`cordata-secrets`) |

---

## 2. Invariants & Persistent Gotchas

1. **Rust only**: the agent, orchestrator, ledger and checkpoints are Rust (`tui/`, `app/`). No Python, no daemon, no Discord. `proto/` holds only a shared test fixture.
2. **Zero Plaintext Secrets**: Never write API keys or tokens into `.env`, code, or tests. Keys reside exclusively in macOS Keychain via `make key` or `scripts/store-key.sh`.
3. **No Unfenced File Reads**: Large files over 1,200 lines trigger outline-only views (`context.outlineLines = 1200`). Over-limit tool outputs (>6,000 chars) are offloaded to `~/.cordata/offloaded/`. Colocated self-contained pods under 1,200 lines are preferred for maximum LLM attention locality.
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

### Deterministic Guards, No Shouting
Do not rely on ALL CAPS or prompt pleadings. Invariants are enforced physically:
- The chat engine's built-in guard (`tui/crates/cordata-guard`) refuses `.env` and `.git/` writes, recursive deletes and `.env` redirects before any approval prompt.
- Every edit and command asks first; the sandbox scopes what an approved command can touch.

### Sub-Agents & Fresh-Context Review
- Keep the main chat context clean: run heavy research, external fetch and binary analysis in your own harness's sub-agents.
- Cordata's chat engine has no sub-agent: its `delegate` tool is not implemented and returns an error. There is no built-in `/goldfish`.
- Parallel orchestrator waves run in isolated APFS clonefile sandboxes (`tui/crates/cordata-orchestrator/src/synthesizer.rs`).
- For post-implementation review, use a fresh sub-agent with no conversation history (from your own harness) to audit the diff against these invariants. `.cordata/commands/goldfish.md` is only a review prompt run in the current conversation.

---

## 4. Progressive Disclosure Directory

Only read these documents when their specific domain is relevant:

| Domain | Document Reference |
|---|---|
| Architecture & Layers | [ARCHITECTURE.md](ARCHITECTURE.md) |
| Threat Model & Sandbox | [THREAT_MODEL.md](THREAT_MODEL.md) |
| Configuration & Settings | [docs/settings.md](docs/settings.md) |
| Multi-Agent Orchestrator | [docs/orchestrator.md](docs/orchestrator.md) |
| TUI & Command Deck | [tui/README.md](tui/README.md) |
| Native App Architecture | [docs/app.md](docs/app.md) |

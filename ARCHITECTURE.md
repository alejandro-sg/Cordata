# Architecture

A map, not a manual. Safety assumptions are in [`THREAT_MODEL.md`](THREAT_MODEL.md); settings are in
[`docs/settings.md`](docs/settings.md).

Everything is Rust. There is no daemon, no Python and no chat-bot front door: a front end runs the agent in its own
process.

```text
 acp-tui (ratatui)                     Cordata.app (GPUI)
        │ SessionHandle / AppEvent             │
        ▼                                      ▼
 ┌──────────────────────────── acp-core ───────────────────────────────┐
 │ native engine: genai streaming · tools (read_file, edit_file,        │
 │   run_command, code_search, code_outline) · approval gate            │
 │ Seatbelt confinement (sandbox-exec), scrubbed env, fail closed       │
 └──────────────────────────────────────────────────────────────────────┘
        │ providers: Anthropic · Gemini · OpenAI-compatible · Ollama
        ▼
 ~/.cordata  (settings, sessions, state.db ledger, checkpoints)

 cordata run / status / show / resume ──► cordata-run ──► cordata-orchestrator
```

## Crates

| crate | what |
|---|---|
| `tui/crates/acp-core` | the session API both front ends use: `SessionHandle`, `AppEvent`, the permission round-trip, and the native engine (`native/`) |
| `tui/crates/cordata-economizer` | token economizer: dynamic tool pruning, micro-skills, loopguard failure interception, prefix cache alignment, head/tail output compactor |
| `tui/crates/acp-tui` | the inline terminal client |
| `tui/crates/cordata-ledger` | quota pools, usage, handoffs, events, run records in `~/.cordata/state.db` |
| `tui/crates/cordata-checkpoints` | the shadow-git checkpoint store behind the orchestrator's rollbacks |
| `tui/crates/cordata-orchestrator` | plan → dispatch → execute → harden → audit, workers (`claude`, `agy`), research, the synthesizer |
| `tui/crates/cordata-run` | the `cordata run` / `status` / `show` / `resume` command |
| `tui/crates/cordata-doctor` | `cordata doctor` |
| `tui/crates/cordata-guard` | `cordata-secrets` (no credential in git) and `cordata-guard` (the pre-tool hook) |
| `app/crates/cordata-view`, `cordata-theme`, `cordata-app` | state and palette shared by both front ends; the native window |

## A turn

1. The prompt goes to the engine with the project's instructions and any attachments.
2. The provider streams text and tool calls. Every tool call passes the approval gate (mode, rules, trust).
3. Commands run under a Seatbelt profile that scopes writes and drops the environment's secrets.
4. A checkpoint is taken before anything that may change files, so a turn can be undone.

## The orchestrator

```text
 cordata run "goal"
        │
 Machine (state.db: orch_runs) ── dispatch (learned bandit over the roster) ◄── quota ledger
   RESEARCH ─► PLAN ─► EXECUTE (waves, checkpoints, Seatbelt write scope) ─► HARDEN (other family) ─► AUDIT ─► SYNTHESIZE
        │                 │ quota line / provider limit → handoff packet → same step, another pool
        ▼
 claude -p · agy
```

[`docs/orchestrator.md`](docs/orchestrator.md) has the detail.

## Tests

`make tui-test` and `make app-test`, offline. Anything that needs a real CLI or the sandbox skips when it is absent.
CI runs the same, plus the secrets scan (`.github/workflows/ci.yml`).

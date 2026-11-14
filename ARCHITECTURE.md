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
 │   run_command, code_search, code_map, code_outline) · approval gate  │
 │ Seatbelt confinement (sandbox-exec), scrubbed env, fail closed       │
 └──────────────────────────────────────────────────────────────────────┘
        │ providers: Anthropic · Gemini · OpenAI-compatible · Ollama
        ▼
 ~/.cordata  (settings, state.db ledger, checkpoints)

 cordata run / status / show / resume ──► cordata-run ──► cordata-orchestrator
```

## Crates
<!-- covers: tui/Cargo.toml, app/Cargo.toml @ b3a342a -->

| crate | what |
|---|---|
| `tui/crates/acp-core` | the session API both front ends use: `SessionHandle`, `AppEvent`, the permission round-trip, the person's rules (`policy`), saved sessions (`history`), and the native engine (`native/`) |
| `tui/crates/cordata-economizer` | token economizer: dynamic tool pruning, micro-skills, loopguard failure interception, prefix cache alignment, head/tail output compactor |
| `tui/crates/acp-tui` | the inline terminal client |
| `tui/crates/cordata-ledger` | quota pools, usage, handoffs, events, run records in `~/.cordata/state.db` |
| `tui/crates/cordata-checkpoints` | the shadow-git checkpoint store behind the orchestrator's rollbacks |
| `tui/crates/cordata-orchestrator` | plan → dispatch → execute → harden → audit, workers (`claude`, `agy`; Ollama Cloud runs through `cordata-run`'s `native_worker`), research, the synthesizer, step tapes, and `evolution` (the gated self-improvement cycle) |
| `tui/crates/cordata-run` | the `cordata run` / `status` / `show` / `resume` / `export` / `evolve` command |
| `tui/crates/cordata-bus` | the per-run message bus workers use to talk to each other (`cordata-bus mcp` is the shim a worker's CLI starts) |
| `tui/crates/cordata-repomap` | the ranked repository map behind the `code_map` tool and the workers' task packets |
| `tui/crates/cordata-floorplan` | `cordata floorplan`: an architectural map of a project with the files being worked on, in the terminal or as HTML |
| `tui/crates/cordata-doctor` | `cordata doctor`, including `--docs` (which doc sections are stale) |
| `tui/crates/cordata-guard` | `cordata-secrets` (no credential in git), the pre-tool guard the chat engine calls, and credential masking (`redact`) |
| `app/crates/cordata-view`, `cordata-theme`, `cordata-app` | state and palette shared by both front ends; the native window, its field view and its menu bar item (`cordata-view`'s `field`, `runs` and `tray` are the pure models) |

## A turn

1. The prompt goes to the engine with the project's instructions and any attachments.
2. The provider streams text and tool calls. The guard refuses what it must, then your `chat.deny` / `chat.allow` rules (`acp_core::policy`), then every other edit and command passes the approval gate.
3. Commands run under a Seatbelt profile that runs them offline (unless the call asked for the network and you approved), scopes writes to the project and caches, and drops the environment's secrets.
4. The conversation is saved, credentials masked, to `~/.cordata/sessions` (`acp_core::history`) for `/resume`.
5. Hooks (`.cordata/hooks.json`, [`docs/settings.md`](docs/settings.md#tool-hooks-hooksjson)) run around each tool call. Context is compacted when it nears the window. In auto mode ([`docs/auto-mode.md`](docs/auto-mode.md)) fixed rules replace the final ask. The chat takes no checkpoints: only `cordata run` does, so only a run can be rolled back.

## The orchestrator
<!-- covers: tui/crates/cordata-orchestrator, tui/crates/cordata-run, tui/crates/cordata-ledger @ b3a342a -->

```text
 cordata run "goal"
        │
 Machine (state.db: orch_runs) ── dispatch (learned bandit over the roster) ◄── quota ledger
   RESEARCH ─► PLAN ─► EXECUTE (waves, checkpoints, Seatbelt write scope) ─► HARDEN (other family) ─► AUDIT ─► SYNTHESIZE
        │                 │ quota line / provider limit → handoff packet → same step, another pool
        ▼
 claude -p · agy · native (Ollama Cloud)
```

[`docs/orchestrator.md`](docs/orchestrator.md) has the detail.

## Tests

`make tui-test` and `make app-test`, offline. Anything that needs a real CLI or the sandbox skips when it is absent.
`make secrets` scans for exposed keys.

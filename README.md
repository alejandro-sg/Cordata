# Cordata

A coding agent for your terminal, and an orchestrator that puts several coding agents to work on one job.

- **`cordata`** opens a terminal chat with an agent that reads, edits and runs things in your project. You approve what is
  risky and see what each turn cost.
- **`cordata run "goal"`** hands a bigger job to real coding CLIs — Claude Code and Antigravity (`agy`). A
  cheap model researches and plans, a strong one implements, another model family reviews, and the planner checks the
  result against its own plan. Every pool's quota is metered, and work moves to another pool before one runs out.

It runs on your machine. The only network calls are to the model providers you configure. It is a single-developer
project: macOS is the supported platform (the workers run under the macOS Seatbelt sandbox), and it changes often.

## Install

You need macOS, [Rust](https://rustup.rs), Git and [ripgrep](https://github.com/BurntSushi/ripgrep)
(`brew install ripgrep`).

```bash
git clone https://github.com/alejandro-sg/Cordata.git cordata && cd cordata
make install          # release build, `cordata` linked into ~/.local/bin, then `cordata doctor`
```

`cordata doctor` checks what is in place and prints the fix for what is not. The native window is optional:
`make app INSTALL=1` (see [`docs/app.md`](docs/app.md)).

## Pick a model

The chat agent runs in the terminal client's own process and needs one model provider. Keys are read from the
environment or the macOS Keychain, never from a file:

| provider | needs |
|---|---|
| Anthropic (the default, `claude-sonnet-5-5`; change with `CORDATA_NATIVE_MODEL`) | `ANTHROPIC_API_KEY` exported in your shell, or the Keychain item `cordata-anthropic-api-key` |
| Ollama's cloud (the default when it is the only key; `CORDATA_NATIVE_MODEL=ollama_cloud::<tag>` picks another) | `make key PROVIDER=ollama` (stores the key in the Keychain) |
| a local Ollama | the server running |

`cordata run` uses the coding CLIs you already have, each signed in once by running `claude` or `agy` yourself. A
CLI that is not installed is left out; `cordata doctor` says which are found.

## Use it

```bash
cd ~/your/project
cordata                              # chat; Ctrl+K opens the command deck
cordata "why does the build fail?"   # …starting with a prompt
```

In the prompt: `/` lists commands, `@` attaches a file, `!git status` runs a command and shows the agent its output, `?`
shows every key. Typing while the agent works queues your message. The agent reads and searches freely, and asks before
it edits a file or runs a command. Commands run under a macOS Seatbelt sandbox. There are no modes, undo, goal loops or saved sessions; see [`tui/README.md`](tui/README.md).

```bash
cordata run "Add retries to the HTTP client and test them"   # research → plan → implement → review → check
cordata status                                               # quota left in each pool, recent runs (spends nothing)
cordata show <run_id>                                        # a run's plan, who did what, the audit
```

`cordata run` needs `--allow-direct` unless you run an Agent Vault proxy: without one the coding CLIs reach the network directly
(still inside the Seatbelt sandbox). Details in [`docs/orchestrator.md`](docs/orchestrator.md).
Set `orchestrator.pools` to your real plans there so handoffs happen at the right moment.

## Where things live

| | |
|---|---|
| `~/.cordata/` | your settings, run history, quota ledger (`state.db`), checkpoints |
| `<project>/.cordata/` | project settings and custom commands; `CORDATA.md` in a project is read as instructions |
| `tui/` | the terminal client, the agent engine, the orchestrator, the ledger and checkpoints (Rust) |
| `app/` | Cordata.app, the native macOS workspace (Rust/GPUI), sharing `cordata-view` / `cordata-theme` with the terminal |

## More

- [`docs/orchestrator.md`](docs/orchestrator.md) — the orchestrator: roles, dispatch, quota ledger, handoffs
- [`docs/settings.md`](docs/settings.md) — every setting
- [`ARCHITECTURE.md`](ARCHITECTURE.md) · [`THREAT_MODEL.md`](THREAT_MODEL.md)

## Develop

```bash
make tui-test        # the terminal workspace's tests
make app-test        # the native app's tests
make secrets         # no credential in the tree
```

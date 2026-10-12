# Cordata

A coding agent for your terminal, and an orchestrator that puts several coding agents to work on one job.

- **`cordata`** opens a terminal chat with an agent that reads, edits and runs things in your project. You approve what is
  risky, undo what you did not like, and see what each turn cost.
- **`cordata run "goal"`** hands a bigger job to real coding CLIs — Claude Code, Antigravity (`agy`) and Ollama models. A
  cheap model researches and plans, a strong one implements, another model family reviews, and the planner checks the
  result against its own plan. Every pool's quota is metered, and work moves to another pool before one runs out.

It runs on your machine. The only network calls are to the model providers you configure. It is a single-developer
project: macOS is the supported platform (the workers run under the macOS Seatbelt sandbox), and it changes often.

## Install

You need macOS, Python 3.12+, [Rust](https://rustup.rs), Git and [ripgrep](https://github.com/BurntSushi/ripgrep)
(`brew install ripgrep`).

```bash
git clone <repo-url> cordata && cd cordata
scripts/setup.sh      # Python environment, dependencies, and the terminal client
make cli              # puts `cordata` on your PATH (a symlink in ~/.local/bin)
cordata doctor        # checks what is in place and prints the fix for what is not
```

## Pick a model

The chat agent needs one model provider. Put one of these in `backend/.env` (created by `setup.sh`):

| provider | `backend/.env` | needs |
|---|---|---|
| Ollama Cloud through your local Ollama | `MODEL_PROVIDER=ollama_cloud` | `ollama signin` once; no API key |
| Ollama's API directly | `MODEL_PROVIDER=openai_compatible`, `CLOUD_OPENAI_BASE_URL=https://ollama.com/v1` | `make key` |
| Anthropic | `MODEL_PROVIDER=anthropic` | `ANTHROPIC_API_KEY` exported in your shell, or the Keychain item `cordata-anthropic-api-key` |
| a local server (oMLX, any OpenAI-compatible) | the default; set `OMLX_BASE_URL` and `OMLX_MODEL_ID` | the server running |

API keys never go in `.env` — the agent can read that file. `make key` stores an Ollama key in the macOS Keychain.

`cordata run` uses the coding CLIs you already have, each signed in once by running `claude` or `agy` yourself. A
CLI that is not installed is left out; `cordata doctor` says which are found.

## Use it

```bash
cd ~/your/project
cordata                              # chat; Tab cycles plan → build → edit → auto; Ctrl+K opens the command deck
cordata "why does the build fail?"   # …starting with a prompt
cordata -c                           # continue the newest session in this folder
```

In the prompt: `/` lists commands, `@` attaches a file, `!git status` runs a command and shows the agent its output, `?`
shows every key. Typing while the agent works queues your message. `/goal tests pass --check make test` keeps the agent
going until your command passes. `/undo` takes back a turn's file changes. The full list is in [`tui/README.md`](tui/README.md).

```bash
cordata run "Add retries to the HTTP client and test them"   # research → plan → implement → review → check
cordata status                                               # quota left in each pool, recent runs (spends nothing)
cordata show <run_id>                                        # a run's plan, who did what, the audit
```

Until the Agent Vault is set up, `cordata run` needs `--allow-direct`; see [`docs/orchestrator.md`](docs/orchestrator.md).
Set `orchestrator.pools` to your real plans there so handoffs happen at the right moment.

## Where things live

| | |
|---|---|
| `~/.cordata/` | your settings, sessions, run history, quota ledger (`state.db`), checkpoints |
| `<project>/.cordata/` | project settings and custom commands; `CORDATA.md` in a project is read as instructions |
| `backend/` | the agent, the orchestrator (`backend/orchestrator/`), the workers, the daemon (Python) |
| `tui/` | the terminal client (Rust) |
| `lab/`, `frontend/` | a native monitoring window and a web UI — kept working, not developed |

## More

- [`docs/orchestrator.md`](docs/orchestrator.md) — the orchestrator: roles, dispatch, quota ledger, handoffs
- [`docs/settings.md`](docs/settings.md) — every setting, permission rules, trust
- [`docs/guide.md`](docs/guide.md) — the long reference: daemon, Discord, memory, skills, editors (ACP), the web UI
- [`ARCHITECTURE.md`](ARCHITECTURE.md) · [`THREAT_MODEL.md`](THREAT_MODEL.md) · [`docs/competitive-gap.md`](docs/competitive-gap.md)

## Develop

```bash
make test            # the hermetic Python suites, in parallel (~3 minutes)
make tui-test        # the Rust tests
```

# Cordata

## What is Cordata?

A coding agent for your Mac, in three pieces:

1. **`cordata`** is a chat agent that reads, edits and runs things in your project. It asks before every edit and every command, and runs commands in a macOS sandbox.
2. **`cordata run "goal"`** is an orchestrator. It hands a bigger job to coding CLIs you already have (Claude Code and Antigravity): one model researches and plans, another implements, a model from a different family reviews, and the planner checks the result against its own plan.
3. **Cordata.app** is an optional native window on the same engine.

Everything runs on your machine. The only network calls go to the model providers you configure. It is a single-developer project, macOS only (Apple Silicon for the app), and it changes often.

## Why use it

**Nothing happens without your say-so.** Every edit and command stops at an approval card. A built-in guard refuses `.env` and `.git/` writes and recursive deletes before the prompt even appears.

![Cordata.app asking approval before editing a file](docs/img/app-approval.png)

**Quota-aware handoffs.** `cordata run` meters each plan's rolling and weekly quota. When a pool nears its limit, the step is rolled back and continues on another pool. `cordata status` shows what is left and spends nothing.

![cordata status showing quota pools](docs/img/quota-pools.png)

**One-screen setup check.** `cordata doctor` says what is installed, which key it found, and the fix for what is missing.

![cordata doctor output](docs/img/doctor.png)

**Keys stay out of files.** API keys live in the macOS Keychain or your environment. A secrets scan (`make secrets`) fails the build if one lands in the tree.

## Quick start

You need macOS, [Rust](https://rustup.rs), Git and [ripgrep](https://github.com/BurntSushi/ripgrep) (`brew install ripgrep`).

```bash
git clone https://github.com/alejandro-sg/Cordata.git cordata && cd cordata
make install                        # builds, links `cordata` into ~/.local/bin, runs `cordata doctor`
make key                            # stores your Anthropic key in the Keychain (hidden prompt)
cd ~/your/project && cordata        # start chatting
```

No Anthropic key? Use Ollama instead: `make key PROVIDER=ollama`, or just run a local Ollama server. Other settings are in [docs/settings.md](docs/settings.md).

For the native window: `make app INSTALL=1` (see [docs/app.md](docs/app.md)).

## Use it

```bash
cordata                                   # chat in the current project; Ctrl+K opens the command deck
cordata "why does the build fail?"        # start with a prompt
cordata run "Add retries to the HTTP client and test them" --allow-direct
cordata show <run_id>                     # a run's plan, who did what, the audit
cordata resume <run_id>                   # continue a paused or interrupted run
```

In the chat prompt: `/` lists commands, `@` attaches a file, `!git status` runs a command and shows the agent its output, `?` shows every key.

`cordata run` needs `--allow-direct` unless you run an Agent Vault proxy (Cordata does not ship one). Without it, the coding CLIs reach the network directly, still inside the Seatbelt sandbox. Set `orchestrator.pools` to your real plans so handoffs happen at the right moment. Details: [docs/orchestrator.md](docs/orchestrator.md).

## Documentation

- [Orchestrator](docs/orchestrator.md): roles, dispatch, quota, handoffs
- [Terminal client](tui/README.md): keys, prompt box, what the chat does not have
- [Native app](docs/app.md): Cordata.app
- [Settings](docs/settings.md): every setting
- [Architecture](ARCHITECTURE.md) · [Threat model](THREAT_MODEL.md) · [Security](SECURITY.md)

## Development

```bash
make tui-test        # terminal workspace tests (offline; no model or key needed)
make app-test        # native app tests
make secrets         # no credential in the tree
```

## License

MIT

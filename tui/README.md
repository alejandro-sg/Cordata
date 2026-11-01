# Cordata terminal client

Rust workspace (`tui/`): the terminal client, the orchestrator and the tools around them. The agent runs inside the
client's own process (`acp-core`'s native engine); there is no backend process. Crate layout is in
[`ARCHITECTURE.md`](../ARCHITECTURE.md).

## Run

`make install` builds the release binaries and links `cordata` into `~/.local/bin`.

```sh
cordata                              # new session in the current directory
cordata "what is this project"       # start with a prompt
cordata --cwd ~/some/project         # work in another folder
cordata run "<goal>"                 # orchestrated multi-worker run (docs/orchestrator.md)
```

From a checkout without installing: `make tui ARGS="--cwd ~/some/project"`. `--debug-log FILE` appends the raw protocol
traffic to a file.

## Model keys

Keys come from the environment (`ANTHROPIC_API_KEY`, `OLLAMA_API_KEY`) or the macOS Keychain, never from a project file
the agent's tools could read. `make key` stores the Anthropic key (`make key PROVIDER=ollama` the Ollama one) at a hidden prompt, and `make check-key`
shows where the Ollama key is found and whether Ollama accepts it, without printing it. The engine masks keys in every tool result.

## The client

Output goes into the terminal's own scrollback: no alternate screen, so the wheel, selection and copy work and the
conversation is still there after you quit. Only the bottom of the screen is
live: the unfinished reply, running tool cards, the prompt box (or an approval box) and a one-line footer.

- **Tool calls** are bordered cards with a collapsed result (Ctrl+E expands). Edits and commands always ask first.
- **Reasoning** shows as a live tail while the model thinks, then `· reasoned (N words)`. Ctrl+R expands it.
- **Orchestrator runs** get one live row per worker.
- **Untrusted content** (web pages, MCP tools, sub-agent output) gets a magenta double border and its source. Agent text
  is stripped of terminal escape sequences before drawing.

### The prompt box

| you type | what happens |
|---|---|
| `/` | command menu: ↑↓ choose, Tab completes |
| `@` | fuzzy file menu; the files' contents go with the prompt. Keys, `.env*` and credentials are never attached |
| `!git status` | runs in the project on your authority; the next prompt carries the output to the agent |
| anything, while the agent works | queues as the next turn; Esc stops the turn and puts the queue back in the box |

`/diff`, `/copy`, `/export [file]` and `/cost` do what they say. Any `*.md` in `.cordata/commands/` (or `.claude/commands/`) becomes `/name`, with `$ARGUMENTS`.

| key | action |
|---|---|
| Enter · Ctrl+J / Alt+Enter | send · newline |
| `a` / `r` | allow / reject a pending tool call; `s` allows and saves the rule; `w` rejects and says why |
| Esc / Ctrl+C | cancel the turn (inert while the approval box is open: answer it first) |
| Ctrl+K | command deck: every model, view and command, searchable |
| F3 | session HUD: context, usage, tasks, agents, the run, recent activity |
| F4 | diff review: files, hunks, line numbers |
| Ctrl+O / Ctrl+T | model / temperature picker |
| `?` or F1 | all keys and commands |
| Ctrl+Q | quit |

### Not available in the chat yet

The chat engine does not implement modes (plan, build, edit, auto) or permission rules: every edit and command asks,
always. There is no undo, no goal loop, no context compaction and no saved sessions (`--resume` and `-c` start a fresh
one), and `/config` changes are not read by the chat. The pre-tool hook in `.cordata/settings.json` (`scripts/guard.sh`,
see [`docs/settings.md`](../docs/settings.md)) still blocks writes to `.env` and `.git/`. `cordata run` has its own
checkpoints and rollback.

## Tests

```sh
make tui-test
```

Hermetic: no model, no provider key and no network. Tests that need a real CLI or the sandbox skip when it is absent.

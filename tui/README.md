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
live: the unfinished reply, running tool cards, the next-line composer (or an approval gate) and a one-line footer.
Shared visual identity with the native app ("The Rope"):

- **Cord in the gutter**: `│` with `●` nodes, `▪` for your anchors, and at a gate the line breaks `╵ ● ╷` in ochre.
- **Rope header**: `── ● fix-tests ──── ○ docs ──── ◐ refactor ──` across active threads.
- **Next line**: `›` caret, a `─` hairline above, and right-aligned footer metadata.
- **The gate**: top and bottom rules only, with the two-column "If allowed / If denied" ledger.
- **Tool calls** are bordered cards with a collapsed result (Ctrl+E expands). Edits and commands always ask first.
- **Reasoning** shows as a live tail while the model thinks, then `│ · reasoned (N words)`. Ctrl+R expands it.
- Agent text is stripped of terminal escape sequences before drawing.

### The prompt box (Next line)

| you type | what happens |
|---|---|
| `/` | command menu: ↑↓ choose, Tab completes |
| `@` | fuzzy file menu; the files' contents go with the prompt. Keys, `.env*` and credentials are never attached |
| `!git status` | runs in the project on your authority; the next prompt carries the output to the agent |
| anything, while the agent works | queues as the next turn; Esc stops the turn and puts the queue back in the box |

`/diff`, `/copy` and `/export [file]` do what they say. `/history` lists this project's saved sessions,
`/resume [id]` continues one (`acp-tui --resume` at start-up), `/forget` deletes them, and `/permissions` shows the rules,
network and write scope commands run under. Any `*.md` in `.cordata/commands/` (or `.claude/commands/`) becomes `/name`, with `$ARGUMENTS`.

| key | action |
|---|---|
| Enter · Ctrl+J / Alt+Enter | send · newline |
| `a` / `r` | allow / reject a pending tool call; `s` allows that tool for the session; `w` rejects and says why |
| Esc / Ctrl+C | cancel the turn (inert while the approval box is open: answer it first) |
| Ctrl+K | command deck: views, sessions and every command, searchable |
| Ctrl+N · Ctrl+←/→ · Alt+1…9 | new session · previous/next · session n |
| Ctrl+G | the session that needs you: an approval first, then one that finished unseen (`✓` in the footer) |
| F4 | diff review: files, hunks, line numbers |
| `?` or F1 | all keys and commands |
| Ctrl+Q | quit |

### Knowing when a session needs you

Each session in the footer is working (`●`), waiting for an approval (`⚠`), finished while you were elsewhere (`✓`,
until you look) or idle. Outside the conversation:

- **Title.** The window or tab title is the thread with a spinner while working, `⚠` when blocked, `✓` when done
  (the terminal's own title comes back on exit). `CORDATA_TITLE=off` leaves it alone.
- **Progress.** Ghostty and Windows Terminal show a progress indicator while a turn runs (OSC 9;4).
- **Notifications.** An approval, a failed turn or a long turn rings the bell; in iTerm2, WezTerm and Ghostty a
  desktop notification says which session and why, when the terminal is in the background or the session is not the
  one shown. `CORDATA_NOTIFY=off` silences both.
- **herdr.** Inside a [herdr](https://herdr.dev) pane Cordata reports its state (and why it is blocked) to herdr, so
  it shows in herdr's agent list and `herdr agent wait`, and gives the command that resumes the session
  (`cordata --resume <id>`) after a herdr restart. `CORDATA_HERDR=off` turns it off.

### Approvals, rules and the sandbox

Every edit and command asks (`a` allows once, `s` allows that tool for the session) unless a rule in
`~/.cordata/settings.json` says otherwise: `chat.allow` runs matching calls without asking, `chat.deny` refuses them
(`docs/settings.md`). Commands run offline and write only the project, temporary folders and package caches; one that
needs the internet asks for it, and the card says so. A built-in guard refuses `.env` and `.git/` writes, recursive
deletes and `.env` redirects before any prompt.

### What the chat does not have

No undo, goal loop or context compaction, and no settings panel: the model comes from `CORDATA_NATIVE_MODEL`. The one mode is `chat.mode` (`ask` or `auto`, `docs/auto-mode.md`).
`cordata run` has its own checkpoints and rollback.

**Instructions.** The agent reads `CORDATA.md` (or `.cordata/CORDATA.md`, `AGENTS.md`, `CLAUDE.md`) from the project, plus
`~/.cordata/CORDATA.md` and `CORDATA.local.md`, at the first prompt. `/memory` lists them and `/init` drafts one.

## Tests

```sh
make tui-test
```

Hermetic: no model, no provider key and no network. Tests that need a real CLI or the sandbox skip when it is absent.

# Gap analysis: Claude Code and Antigravity, read from the binaries

What the two tools Cordata is measured against actually ship, found by reverse-engineering the installed builds with REA
rather than from their marketing — and what Cordata did about each gap. Observed on this machine: Claude Code 2.1.296
(`~/.local/share/claude/versions/`, a Bun-compiled Mach-O), Antigravity CLI `agy` 1.3.3 (a 180 MB Go binary), and
Antigravity.app 2.22.0 (an Electron shell around a Go `language_server`).

## How it was read

| target | tool | what it gave |
|---|---|---|
| `agy` (Go) | `strings`, `agy --help`, `agy changelog` | the command surface, the embedded release notes (1.2.16 → 1.3.3), the protobuf service names (`exa.cortex_pb.*`, `exa.hooks_pb.*`, `exa.language_server_pb.*`) |
| `claude` (Bun, JS embedded in a Mach-O) | `strings`, the command registry (`type:"local",name:"…"`) | every slash command, the hook event names, the headless flags |
| Antigravity.app | REA `open_binary` + `extract_artifact` on `app.asar` | `package.json` (Electron + `@connectrpc` + `chrome-devtools-mcp`), the main-process modules; the agent itself lives in `Resources/bin/language_server`, not in JavaScript |

`analyze_javascript_application` on the asar overflows the MCP response budget (10 MB); extracting the archive and reading
it directly was the workable route. The findings are observations of shipped behaviour, not copies of any code.

## What the references have that matter

| capability | Claude Code | Antigravity | Cordata before | now |
|---|---|---|---|---|
| Type while the agent works; messages queue | ✓ | ✓ (`Queued Messages`) | input locked mid-turn | ✓ type-ahead queue; a cancelled turn hands the queue back |
| `@file` mentions with completion | ✓ | ✓ | backend could read them; the TUI never sent any | ✓ fuzzy menu, contents attached, secrets refused |
| `/` command menu | ✓ | ✓ | Tab-cycling through names | ✓ live menu with descriptions; Enter runs argument-less commands |
| Custom commands from files | `.claude/commands/*.md` | workflows, skills | none | ✓ reads `.claude/commands` and `.cordata/commands` |
| `!shell` escape | ✓ | ✓ | none | ✓ shown, and handed to the agent with the next prompt |
| `/goal` — work until a condition holds | ✓ (`/goal`) | plan-mode loops | none | ✓ `--check <command>` decides it for free; or a tool-less judge |
| `/btw` side question | ✓ | ✓ | none | ✓ no tools, nothing written back, works mid-turn |
| `/compact`, context meter | ✓ | context usage view | automatic only, invisible | ✓ gauge in the footer, `/compact [focus]`, `/context` |
| Visible task list | `TodoWrite` | `task.md` artifact | none | ✓ `todo_write` + a plan panel above the prompt |
| Prompt suggestions | `--prompt-suggestions` | — | none | ✓ zero-token, from what the turn did |
| `/diff`, `/copy`, `/export`, `/cost` | ✓ | `/diff`, `/rewind` | `/undo` only | ✓ |
| Terminal notification when it needs you | ✓ | ✓ (`Notifications`) | none | ✓ bell on approval, and on a long turn finishing |
| User hooks (Pre/PostToolUse, Stop, …) | 9 events | 5 hook types | internal only | not yet — see below |
| Worktrees for parallel agents | `EnterWorktree` | `CreateWorktree`, jj | none | partly — the orchestrator's next step |
| Plugins/marketplace | ✓ | ✓ | MCP plugins | unchanged |
| Remote control | ✓ | ✓ | the daemon + Discord | unchanged |

## What Cordata has that neither reference does

Laya's local triage in front of the model; the learned, shadow-gated dispatch policy; per-pool quota accounting with
provider-reported utilisation; handoff packets between coding CLIs; self-healing with sandbox-verified patches;
checkpoints that undo a turn's file changes. The orchestrator is the part neither tool has at all: it chooses *which*
model and CLI does each step, from evidence, and moves the work when a quota runs low.

## Design choices worth defending

* **Queued, not interleaved.** A message typed mid-turn waits for the turn to finish and goes out as a turn of its own.
  It is not injected into the running turn (Claude Code's behaviour): an interjection the agent half-reads is how a
  plan quietly changes. `Esc` stops the turn and puts what was queued back in the box.
* **`/goal` is checked by the harness, not by the agent.** `--check make test` is run by Cordata after each turn; the
  agent cannot talk its way to "done". With no command, a model with *no tools* judges from what the turn did, and is told
  that the agent's own claim is not evidence. Both are bounded (`--turns`, default 8, hard cap 30), stop on cancel, and
  announce every step. `/goal` is parsed in the backend, so it works from Zed or Discord the same way.
* **`/btw` has no tools.** Whatever the conversation contains — including untrusted tool output — the aside cannot act
  on it. Nothing is written to the history.
* **`@file` never attaches a secret.** `.env*`, private keys, `.netrc`, `credentials*` are refused by name, whatever was
  typed, and said so on screen.
* **`!command` is the person's own.** It does not pass the agent's approval gate (it is not an agent action), and what it
  prints is shown and sent along with the next prompt so nothing needs copy-and-paste.
* **The palette degrades.** Truecolor terminals get the Cordata palette; 256-colour ones the nearest cube colours;
  `CORDATA_THEME=terminal`, a light background (`COLORFGBG`) or `NO_COLOR` keep the terminal's own colours.

## Not done, on purpose or for later

* **User hooks.** The backend's hooks are internal (secret masking, the untrusted-output guard). Letting a project
  register commands that run on every tool call is code execution from a settings file; it needs the trust gate the
  permission rules already use, and a design for blocking vs. advisory hooks. Left for its own change.
* **Background shell tasks and `/tasks`.** `shell` is synchronous; a dev server needs the agent to background it by hand.
* **Image paste.** The path exists for the menu bar's screenshot (`os_context`); the terminal client does not attach files
  other than as text.
* **Live parallel steps across CLIs in separate worktrees** — see `docs/orchestrator.md`.

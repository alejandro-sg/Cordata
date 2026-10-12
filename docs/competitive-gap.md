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

## Second pass (2026-10-12): modes, plans, questions and the full-screen views

Re-read the same two binaries for the interaction model rather than the command list, and added two references: **DeepSeek
Harness** (`dsh`, `@deepseek-ai/dsh`) and **DeepSeek-TUI** — both read from their public descriptions (third-party write-ups
and the repository's front page), not from source, so what follows is what they *say* they do.

| idea | where it comes from | Cordata now |
|---|---|---|
| A real hand-over from plan to work: the agent presents the plan, the person answers, a rejection comes back as a revision request | Claude Code's `ExitPlanMode` (the binary's own text: on rejection "revise the plan… call ExitPlanMode again") · Antigravity `/plan` (plans wait for review) | `present_plan` → the **plan card** (`b` build · `e` edit · `a` auto · `k` keep planning · `w` say what to change); the person's answer switches the mode |
| A mode between "ask for everything" and "ask for nothing" | Claude Code *accept edits* · `agy --mode accept-edits` | **edit** mode: file edits run after preflight, commands still ask |
| Mode *and* posture as separate axes (Plan/Work/Operate × Ask/Auto-review/Full access) | DeepSeek-TUI | not copied: Cordata's four modes already fold the two; Laya's per-call gate plays the auto-review part |
| Clarifying questions with suggested options | `dsh`'s planning mode · Claude Code's question tool | `ask_user`: numbered options, `w` to type your own, `s` to leave it to the agent |
| The harness choosing the mode | `dsh` describes planning before editing; Claude Code has an auto-mode classifier set up with `/auto-mode-setup` | **adaptive mode**: a pure function of the message and Laya's route that only *tightens* (→ plan); never selects `auto` |
| A fullscreen renderer next to the inline one | Claude Code `/tui` (`default` \| `fullscreen`) | **hybrid**: inline conversation; the command deck (Ctrl+K), HUD (F3) and diff review (F4) take the alternate screen with the mouse on, then hand it back |
| Interactive diff: files, hunks, line numbers | Antigravity `/diff` (Commit Tree view, `n`/`N`, per-file cursor) | `/diffview`: file list, hunk jumps, old/new line numbers, untracked files, mouse |
| Command palette | Antigravity `/plugin` pickers · DeepSeek-TUI `/provider` (F3) | Ctrl+K: modes, models, views, sessions, commands, custom commands, one filter |
| One screen for "what is happening" | `dsh`'s *Trajectory* view (every step with its cost) · Claude Code `/context` grid · Antigravity `/tasks` | the HUD: session, context gauge, usage, tasks, agents, the orchestrator's run board (with per-step cost / tokens), activity |
| Subagents / workflows visible while they run | Antigravity `/agents` (shows `Error:` for a stopped subagent) | the HUD's AGENTS and RUN panels (steps, workers, handoffs) |

**The look.** A night-time interface rather than boxes and emoji: sodium amber for Cordata itself, electric teal for the other
voice (agents, the orchestrator), cold mint / vermilion for safe and danger, thin rules, `⌜ ⌝` brackets, `▰▱` gauges and one
scanning light while the agent works. Everything is a semantic colour remapped in one place (`theme.rs`), so
`CORDATA_THEME=terminal` and `NO_COLOR` still work.

**Not copied, on purpose.** `/ultraplan` / `/ultrareview` (a cloud session drafting or reviewing for you): they need a service
Cordata does not run. `/advisor` (consult a stronger model "at key moments"): Cordata's dispatcher already routes by evidence and
the orchestrator escalates between workers; a second, user-visible escalation switch would duplicate it.

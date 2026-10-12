# Cordata terminal clients (Rust, ACP)

Two binaries over one protocol library, all talking to `backend/acp_server.py`
(the same Agent Client Protocol server Zed uses):

| crate | what |
|---|---|
| `acp-core` | all protocol logic, no UI: spawn backend → `SessionHandle` → `AppEvent` stream; permission round-trip; untrusted-output detection |
| `acp-tui` | interactive client (Claude-Code-style): inline in your terminal's own scrollback, streaming chat, tool cards, approval box with "always allow", layered settings (`/config`, `/permissions`), mode/model/temperature, `--resume` |
| `acp-stress` | scriptable concurrent runner: scenarios in, one JSONL line per prompt-turn + a summary out |

Built on the official [`agent-client-protocol`](https://crates.io/crates/agent-client-protocol)
crate (pinned `=2.2.0`, default features only).

## Run

Using the global `cordata` command (`make cli` links it into `~/.local/bin`):

```sh
cordata                              # new interactive session in current directory
cordata "what is this project"       # launch with initial prompt (Claude Code style)
cordata -c                           # resume newest session in current directory
cordata --resume <session-id>        # resume session by ID
cordata --cwd ~/some/project         # launch in specified project folder
cordata run "<goal>"                 # autonomous multi-worker batch run
```

Or running directly via cargo / make:

```sh
cd tui
cargo run -p acp-tui                         # new session in the current directory
cargo run -p acp-tui -- --cwd ~/some/project # agent works in that project
cargo run -p acp-tui -- --resume <session-id>
cargo run -p acp-stress -- scenarios/smoke.json -c 4 -o results.jsonl
```

`make tui`, `make tui-test`, `make stress SCENARIOS=tui/scenarios/stress.json` from the repo root do the same.

The backend is found automatically (`backend/.venv/bin/python` +
`backend/acp_server.py`, walking up from the binary, then the cwd); override
with `--backend-root` or `ACP_TUI_BACKEND_ROOT`.

Startup is near-instant (<1.5s cold start): heavy dependencies (`SentenceTransformer`, Qdrant)
are lazily initialized on first use, and MCP client initialization connects asynchronously in the background
without blocking the ACP handshake.

### Using a cloud model (and keeping its key from the agent)

The backend is wired to Ollama Cloud (`gpt-oss:20b`, the cheapest model verified to call tools
reliably) in `backend/.env` — plain `cargo run -p acp-tui` uses it, nothing to export. The only
thing that can't live in a project file is the key (the agent's file tools can read `.env`), so
store it **once** in the macOS Keychain. `make key` prompts for it (hidden — never echoed, never
on a command line, never in a file), stores it, and then *verifies* Ollama accepts it, so a bad
paste is caught immediately:

```sh
make key
```

`make check-key` re-checks later: it shows where the backend finds the key (an exported variable
beats the Keychain), its length and a short fingerprint — never the key — and whether Ollama
accepts it. At startup the backend does the same check itself and reports a rejected key with the
cause, instead of a bare `401` mid-conversation.

The backend reads it from there at startup (or from `OLLAMA_API_KEY` / `CLOUD_OPENAI_API_KEY` if
you export one, which wins), removes it from its own environment, and masks it in every tool
result — see `THREAT_MODEL.md`, "Provider API keys", including what that does *not* cover
(`python_repl` in `auto` mode). With no key you get the fix on screen instead of a mystery error.
Back to local oMLX: set `MODEL_PROVIDER=local` in `backend/.env`. Neither client ever reads the
key itself, and `--debug-log` records ACP traffic and the server's stderr, neither of which
carries it.

## acp-tui

**It prints into your terminal like any command-line program — there is no alternate screen.**
Finished messages are written once and become ordinary scrollback, so the mouse wheel, trackpad,
Shift+PageUp, text selection and copy/paste all work, a long reply flows into scrollback as it
streams, and what you saw is still there after you quit (`cordata --resume <id>` is printed on exit).
Only the bottom of the screen is live: the unfinished part of the reply, running tool cards, then
the prompt box (or the approval box / `/config` panel in its place) and a one-line footer
(connection dot, mode, model · temperature, directory — and, while the agent works,
`⠋ working 12s · 3 tool calls · Esc cancels`). The footer sheds the least important parts on a
narrow terminal. Multi-line paste goes into the box as one paste; it doesn't submit.

- **Reasoning** is shown as a live two-line tail while the model thinks, then settles to
  `· reasoned (304 words)`. Ctrl+R (or `/thinking`, or `tui.thinking` in settings) shows it in full
  for reasoning that finishes afterwards.
- **Tool calls** are bordered cards: status glyph, the command or input, and a collapsed result
  (Ctrl+E / `/tools` expands). `shell`, `python_repl`, `file_write` ask first in `build` mode.
- **Sub-agents and orchestrator runs** get one row per worker, live: `⠙ architect
  thinking: …` while it runs, `✓ debugger <summary>` when done, `→ a → b` for hand-offs, and
  the final result under `result`.
- **Replies** render basic markdown: headings, bullets, numbered lists, quotes, `code`, **bold**
  and fenced code blocks.
- **Laya, live.** The moment you send a prompt, a dim `◆ laya` line appears under it (~70 ms):
  the route Laya picked and how sure it is, and whether a routing hint was sent to the model —
  `files_and_code 98% ▸ hint sent`, or why not (`below the 85% gate, no hint`, `no hint for this
  route`, `no decision — timed out…`). When the turn ends a second line says what the LLM then
  actually did and whether it agreed (`↳ LLM used files_and_code (file_read) ✓ agreed`, plus the
  tiers any sub-agents used). `/laya off` hides it. It is pushed by the backend as an ACP extension
  notification (`_cordata/laya`), which other clients (Zed) ignore.
- **Untrusted content** (web pages, memory recalls, MCP tools, sub-agent output) gets a magenta
  double border, `⚠ UNTRUSTED CONTENT`, and its source — the backend's guard boilerplate is
  stripped for display; a classifier flag, if any, is shown.

### Typing, and what the prompt box understands

| you type | what happens |
|---|---|
| `/` | a menu of commands with their arguments and descriptions — ↑↓ choose, Tab completes, Enter runs a command that needs no argument |
| `@` | a menu of the project's files (fuzzy: `@appr` finds `src/app.rs`; a folder keeps the menu open so you can walk in). On send, the files' contents go with the prompt, so the agent need not spend a turn reading them. Keys, `.env*` and credentials are never attached |
| `!git status` | runs in the project, on your authority, and shows the output; the next prompt carries it to the agent |
| anything, **while the agent works** | it queues (`⏵ next …` above the box) and goes out as a turn of its own when the agent is done. Esc stops the turn and puts the queue back in the box |
| nothing, after a turn | a grey suggestion of what usually comes next ("run the tests") — Tab takes it, typing replaces it |

`/goal <condition> [--check <command>] [--turns N]` keeps the agent working until the condition holds. With `--check`
the *harness* runs your command after each turn and the goal is met when it exits 0; without, a model with no tools
judges from what the turn did. `/btw <question>` is answered from the conversation without touching it, even mid-turn.
`/compact [focus]` summarises older turns; the footer's `ctx ▰▰▱▱▱▱ 31%` shows how full the window is. `/diff`, `/copy`,
`/export [file]`, `/cost` do what they say. Your own commands: any `*.md` in `.cordata/commands/` (or Claude's
`.claude/commands/`) is `/name`, with `$ARGUMENTS`. When the agent keeps a checklist (`todo_write`), it is drawn above the
prompt.

Colour: Cordata's palette on truecolor and 256-colour terminals; `CORDATA_THEME=terminal` (or a light background, or
`NO_COLOR`) keeps your terminal's own. `CORDATA_NOTIFY=off` silences the bell rung when an approval is waiting or a long
turn finishes.

| key | action |
|---|---|
| Enter | send · `Ctrl+J` / `Alt+Enter` newline |
| `a` / `r` (or Esc) | allow / reject a pending tool call (approval box) — `y` / `n` work too |
| `s` | allow and **save the rule** ("Always allow shell(git status)"), when the box offers it |
| Esc / Ctrl+C | cancel the running turn (not while the approval box is open) · Esc/Ctrl+C clears the box when idle |
| Up / Down | earlier / later prompts (on the first / last line of the box) |
| Tab / Shift+Tab | cycle mode: plan → build → edit → auto (or autocomplete slash commands when typing `/...`) |
| Ctrl+K or `/palette` | the **command deck**: every mode, model, view, session and command in one searchable list — see below |
| F3 or `/hud` | the **session HUD**: context, usage, tasks, agents, the orchestrator's run and recent activity on one screen |
| F4 or `/diffview` | the **diff review**: changed files, hunks, line numbers; mouse and keys |
| `w` (any approval card) | reject **and say why**, or — on a plan or a question — answer in your own words |
| Ctrl+O (also Ctrl+M where the terminal distinguishes it) | model picker |
| Ctrl+T | temperature picker |
| Ctrl+R / Ctrl+E | expand reasoning / tool results |
| `?` (empty box) or F1 | print the help (all keys and commands) |
| F2 or `/lab` | the **Experimental Lab**: live agent pods, A2A links, Laya threat gauge, E-Stop, dial, guidance — see below |
| Ctrl+Q | quit |

### Modes, plans and questions

| mode | what runs without asking |
|---|---|
| **plan** | nothing runs; the agent investigates with read-only tools and hands you a plan |
| **build** | reads and edits ask only where the approval rules say (the default) |
| **edit** | file edits run (after preflight); commands and everything else still ask |
| **auto** | routine calls run; anything Laya rates destructive, deny rules and model pulls still ask |

**Adaptive mode** (`modes.adaptive`, on by default) starts a request that wants a plan — "make a plan…", a design question,
a large change across the project — in **plan**, says why in one line, and ends with a **plan card**: `b` approve → build,
`e` approve → edit, `a` approve → auto, `k`/Esc keep planning, `w` say what to change (the agent revises and presents again).
It only ever *tightens*: nothing moves a session toward `auto` except your answer to a card or your own Tab, and after you
pick a mode yourself it stays out of the way for 8 turns.

When the work turns on a decision only you can make the agent calls `ask_user`: a **question card** with numbered options —
press the number, `w` to type your own answer, `s` to leave it to the agent.

### Full-screen views (hybrid)

The conversation stays inline in your terminal's scrollback. The views below take the screen while they are open — with the
mouse on (hover highlights, click chooses, the wheel scrolls) — and hand it back exactly as it was when you close them.

| view | open | keys |
|---|---|---|
| command deck | Ctrl+K · `/palette` | type to filter · ↑/↓ or wheel · Enter or click runs · a command that needs an argument is left in the prompt · Esc |
| session HUD | F3 · `/hud` | Tab or click focus a panel · ↑/↓ or wheel scroll it · `r` diff review · Ctrl+K deck · Esc |
| diff review | F4 · `/diffview` | ↑/↓ · ←/→ pane · `[` `]` file · `n` `N` hunk · wheel/click · `r` reload · Esc |

### The Experimental Lab (F2 / `/lab`)

A live view of every agent in the session (`docs/experimental-lab.md` has the design and the
protocol). It is drawn in the live region like the other panels, so the transcript stays in your
scrollback. With it open, keys go to the Lab; Ctrl+C still cancels a turn.

| key | does |
|---|---|
| ←/→, `h`/`l`, Tab | select a pod |
| Enter | inspect: system prompt, tool trace, live stream (Enter/Esc collapses) |
| `K` | **E-Stop**: isolate the pod and its sub-agents — every model/tool call is refused from then on |
| `R` | release an isolated pod |
| `[` / `]` (or `-` / `+`) | the pod's Laya destructiveness dial −/+ 0.05 (it only ever *tightens* the approval gate) |
| `I` | inject guidance: type, Enter queues it for the pod's next model call, Esc cancels |
| `Y` / `N` | at the **HITL Intervention Station** (a pending approval, drawn with its diff): approve & execute / reject & discard |
| `K` at the station | E-Stop: reject the call *and* isolate the pod that asked |
| Esc, F2 | collapse the inspector, then close |

Run it on the signed-in local Ollama daemon (no API key): `make tui-lab`.

Scrolling is your terminal's: wheel, trackpad, Shift+PageUp/PageDown, or its scrollback search.

**Slash commands** (type in the prompt box; they never reach the model, except `/init`): `/help`,
`/mode [plan|build|edit|auto]`, `/model [name]`, `/temp [focused|balanced|creative]`, `/thinking`,
`/tools`, `/laya [on|off]`, `/config`, `/permissions`, `/trust`, `/memory`, `/init`, `/status`,
`/clear`, `/quit`. A path like `/usr/bin/env is missing` is still a normal prompt.

**Preferences and permissions** are layered JSON files — `~/.cordata/settings.json`,
`<project>/.cordata/settings.json` (or `.cordata/`, commit it), `<project>/.cordata/settings.local.json`
(this machine) — plus instruction files (`CORDATA.md`, `CORDATA.md`, or `AGENTS.md` / `CLAUDE.md`), the way
Claude Code and Kilo do it:

```text
/config                                  interactive panel; Tab picks which file a change is written to
/config set laya.mode shadow project
/permissions allow shell(git status)     deny always wins, in every mode
/permissions deny  file_write(*.env) project
/trust                                   let this folder's project settings loosen permissions
/init                                    have the agent draft CORDATA.md
```

Everything — keys, rule syntax, the trust model, what applies when — is in
[`docs/settings.md`](../docs/settings.md); what is and isn't at parity with Claude Code, Kilo and
DeepSeek-TUI is in [`docs/harness-parity.md`](../docs/harness-parity.md).

Deliberate behaviours worth knowing:

- **Ctrl+C is inert while the approval box is open.** `session/cancel` is only
  checked between streamed events and the server is parked on your permission
  request, so cancelling then would do nothing. Answer the request first.
  *Rejecting a tool call does not abort the turn* — the model sees the denial and continues.
- **Mode colours are risk-ordered:** plan = green (nothing runs), build = yellow
  (dangerous tools ask), auto = red (nothing asks).
- **Ctrl+M can't be the model key in most terminals** (it is byte-identical to
  Enter), hence Ctrl+O.
- **`--resume`** shows text history only (the backend doesn't replay tool calls)
  and comes back in your default mode (`build` unless `permissions.defaultMode` says otherwise); a divider
  says so. The protocol has no session listing, so you need the id (printed when you quit, shown in the
  banner, and in `backend/sessions/session_<id>/`).
- **Untrusted output**: results the backend wraps in `<<<UNTRUSTED_TOOL_OUTPUT>>>`
  (http_request, agent_memory reads, MCP tools) get a magenta double-line card
  labelled `⚠ UNTRUSTED CONTENT`. All agent text is stripped of terminal escape
  sequences before drawing.

`--debug-log FILE` appends the raw JSON-RPC traffic and the server's stderr to a
file (never stdout — the TUI owns the terminal).

### `--transport ws`: attaching to the daemon

`--transport stdio` (the default) spawns `backend/acp_server.py` for this
terminal, and the session ends with it. `--transport ws` attaches to the
always-on daemon instead (`make daemon`, or `make daemon-install` for a
LaunchAgent):

```bash
make daemon                          # foreground supervisor
make tui ARGS="--transport ws"       # attaches to the session in this cwd, or starts one
make tui ARGS='--transport ws --continue'
```

The daemon takes the **directory**, not a session id: the id is the daemon's to
mint, and a client that just started does not have one. Attaching to a directory
that already has a live session reattaches to it and replays what you missed —
including an approval prompt the child is already blocked on, which it will not
re-emit (output a previous client already received is not replayed). One client
per session: a second terminal on the same directory is refused, and `/new` is
unavailable over `ws` for the same reason. The banner's `link` row says which
transport you are on.

The token and port come from `~/.cordata/daemon.json` (or `~/.cordata/daemon.json`, 0600); `--daemon-url`
overrides the URL, for a daemon whose config you don't have on disk. Nothing
else changes: the daemon proxies ACP frames verbatim, so `--resume`,
`--continue`, permissions and Laya behave identically either way.

## acp-stress

```jsonc
{ "scenarios": [{
  "name": "shell-roundtrip",
  "prompts": ["Call the shell tool to run exactly: echo hi"],
  "cwd": ".",                       // default: runner's cwd
  "mode": "build",                  // plan | build | auto
  "model": "…", "temperature": "balanced",   // focused | balanced | creative
  "approval_policy": "auto_allow",  // auto_allow | auto_reject (default) | {"shell":"allow","*":"reject"}
  "repeat": 4                       // independent copies
}]}
```

- **One backend process per session**, always. The backend does a process-wide
  `os.chdir` per session, so sessions sharing a process would corrupt each
  other's working directory. Each process loads its own models: size `-c` to your RAM.
- `build` + `auto_allow` exercises the real permission request/response
  round-trip; `auto` mode exercises the model's tool calling without it. They
  stress different things.
- Output: one JSONL line per prompt-turn (`stop_reason`, `ttft_ms`, `latency_ms`,
  `tool_calls`, `permission_requests`, `permissions`, `error`, best-effort
  `context_usage`), then a summary (p50/p95, stop reasons, most common tool,
  grouped problems). Exit code is non-zero if any session errored or any turn
  ended in anything but `end_turn`. A session that dies still gets a line for
  every prompt (later ones are `skipped: …`).
- **Memory:** with Laya on (`LAYA_TRIAGE_MODE=route`, the backend default, and/or
  `LAYA_GUARD_ENABLED`), every backend process also loads its own ~650 MB Laya checkpoint.
  For big concurrent runs that only stress the harness, set `LAYA_TRIAGE_MODE=off
  LAYA_GUARD_ENABLED=false` in the launching shell. Per-turn triage and sub-agent tiers
  show up in `backend/laya_triage.jsonl` — see `docs/laya-triage.md`.
- Hung turns are cancelled after `--turn-timeout` and reported; Ctrl+C shuts
  every backend down cleanly. No token/cost accounting — ACP has no structured usage push.
- `scenarios/smoke.json` is the five cases from `backend/_acp_smoke_test.py`.
  Reading it back: approved → `completed` + 1 permission request · denied →
  `failed` + 1 · plan → `failed` + 0 · auto → `completed` + 0.

## Tests

```sh
cargo test --workspace
```

Protocol paths (streaming, permission allow/reject/drop, plan/auto, cancel,
replay, config options, crash, timeouts, concurrency, orphan-process check) are
tested end-to-end against `crates/acp-core/tests/fixtures/fake_agent.py`, a
scripted ACP agent, so they need no model server. That includes a regression test that
notifications keep flowing while an approval is open (the Lab's telemetry depends on it).

The Lab is also covered end to end, with this binary in a pseudo-terminal against a real backend
child and a scripted stand-in for the Ollama daemon: `backend/_e2e_lab_tui_smoke_test.py`
(`docs/experimental-lab.md`, "Tests").

## Undo

Before any call that may change files the backend snapshots the project (`docs/agentic-harness.md`,
"Checkpoints"), so `auto` mode can be taken back. `/undo` previews the latest turn's changes — files, what
each would do, and any you edited since — and `/undo apply` reverts them; `/undo <turn> [apply]` names an
earlier turn and `/checkpoints` lists them. A change that could not be snapshotted, or that writes outside the
project, is said once (`⚠ not checkpointed …`). Undo is refused while a turn is running.

`/back` is the other undo: it takes the *conversation* back, so the agent forgets the last turn (`/back 2` the last two) and the
screen drops them with it; files are not touched. `/back list` shows the checkpoints (one is taken as each turn starts) and
`/back to <number>` restores one — including the state a `/back` left, so a `/back` can itself be undone.

## Not done

Provider switching (it's environment on purpose), free-float temperature, skills/MCP management,
session enumeration in the TUI picker (add `list_sessions` to `acp_server.py` first — don't read `backend/sessions/`
off disk behind the protocol's back), `/compact`, and `@file` mentions. The full list, with the reference tool for each, is in
[`docs/harness-parity.md`](../docs/harness-parity.md).

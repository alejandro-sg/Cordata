# sierralab-agents terminal clients (Rust, ACP)

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
with `--backend-root` or `ACP_TUI_BACKEND_ROOT`. You need `scripts/setup.sh`
done and your oMLX model server running — without it every turn fails with
`Connection error`. The backend takes ~8 s to start (it loads models), so the
footer shows `connecting` at first.

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
streams, and what you saw is still there after you quit (`acp-tui --resume <id>` is printed on exit).
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
- **Sub-agent teams** (`local_swarm` / `local_graph`) get one row per agent, live: `⠙ cat_agent
  thinking: …` while it runs, `✓ dog_agent <its summary>` when done, `→ a → b` for hand-offs, and
  the team's final result under `result`. The agents' own system prompts aren't dumped into the card.
- **Replies** render basic markdown: headings, bullets, numbered lists, quotes, `code`, **bold**
  and fenced code blocks.
- **Laya, live.** The moment you send a prompt, a dim `◆ laya` line appears under it (~70 ms):
  the route Laya picked and how sure it is, and whether a routing hint was sent to the model —
  `files_and_code 98% ▸ hint sent`, or why not (`below the 85% gate, no hint`, `no hint for this
  route`, `no decision — timed out…`). When the turn ends a second line says what the LLM then
  actually did and whether it agreed (`↳ LLM used files_and_code (file_read) ✓ agreed`, plus the
  tiers any sub-agents used). `/laya off` hides it. It is pushed by the backend as an ACP extension
  notification (`_sierralab/laya`), which other clients (Zed) ignore.
- **Untrusted content** (web pages, memory recalls, MCP tools, sub-agent output) gets a magenta
  double border, `⚠ UNTRUSTED CONTENT`, and its source — the backend's guard boilerplate is
  stripped for display; a classifier flag, if any, is shown.

| key | action |
|---|---|
| Enter | send · `Ctrl+J` / `Alt+Enter` newline |
| `a` / `r` (or Esc) | allow / reject a pending tool call (approval box) |
| `s` | allow and **save the rule** ("Always allow shell(git status)"), when the box offers it |
| Esc / Ctrl+C | cancel the running turn (not while the approval box is open) · Esc/Ctrl+C clears the box when idle |
| Up / Down | earlier / later prompts (on the first / last line of the box) |
| Tab / Shift+Tab | cycle mode forward / back: plan → build → auto |
| Ctrl+O (also Ctrl+M where the terminal distinguishes it) | model picker |
| Ctrl+T | temperature picker |
| Ctrl+R / Ctrl+E | expand reasoning / tool results |
| `?` (empty box) or F1 | print the help (all keys and commands) |
| Ctrl+Q | quit |

Scrolling is your terminal's: wheel, trackpad, Shift+PageUp/PageDown, or its scrollback search.

**Slash commands** (type in the prompt box; they never reach the model, except `/init`): `/help`,
`/mode [plan|build|auto]`, `/model [name]`, `/temp [focused|balanced|creative]`, `/thinking`,
`/tools`, `/laya [on|off]`, `/config`, `/permissions`, `/trust`, `/memory`, `/init`, `/status`,
`/clear`, `/quit`. A path like `/usr/bin/env is missing` is still a normal prompt.

**Preferences and permissions** are layered JSON files — `~/.sierralab/settings.json`,
`<project>/.sierralab/settings.json` (commit it), `<project>/.sierralab/settings.local.json`
(this machine) — plus instruction files (`SIERRALAB.md`, or `AGENTS.md` / `CLAUDE.md`), the way
Claude Code and Kilo do it:

```text
/config                                  interactive panel; Tab picks which file a change is written to
/config set laya.mode shadow project
/permissions allow shell(git status)     deny always wins, in every mode
/permissions deny  file_write(*.env) project
/trust                                   let this folder's project settings loosen permissions
/init                                    have the agent draft SIERRALAB.md
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
re-emit. The banner's `link` row says which transport you are on.

The token and port come from `~/.sierralab/daemon.json` (0600); `--daemon-url`
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
scripted ACP agent, so they need no model server.

## Not done

Provider switching (it's environment on purpose), free-float temperature, skills/MCP management,
session enumeration (add `list_sessions` to `acp_server.py` first — don't read `backend/sessions/`
off disk behind the protocol's back), `/compact`, `/new`, snapshots / `/undo`, `@file` mentions,
a non-interactive `-p` mode. The full list, with the reference tool for each, is in
[`docs/harness-parity.md`](../docs/harness-parity.md).

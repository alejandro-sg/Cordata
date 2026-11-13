# Settings

What Cordata reads, where, and what it does with it. Only what the Rust code actually reads is listed.

## Files

Three JSON files; a later one wins on a conflict. Objects merge key by key.

| scope | file |
|---|---|
| user | `~/.cordata/settings.json` (`CORDATA_HOME` moves the folder) |
| project | `<project>/.cordata/settings.json` |
| local | `<project>/.cordata/settings.local.json` |

Plain JSON, no comments. A file that is not valid JSON is ignored and reported (`cordata doctor` says which); it is never
overwritten. A key can be nested (`{"vault": {"enabled": true}}`) or flat (`{"vault.enabled": true}`).

**Only the user file may set `orchestrator.*` and `vault.*`** (and `app.*`). A repository's own files cannot raise the
spend cap, change the pools or point worker traffic at another proxy; those keys are ignored there and reported.

## Chat agent

The terminal client and the app run the agent in their own process. The model and keys come from the environment.

| variable | meaning |
|---|---|
| `CORDATA_NATIVE_MODEL` | model name, any `genai` name (`claude-…`, `gemini-…`, `gpt-…`, or a local Ollama tag); `ollama_cloud::<tag>` (e.g. `ollama_cloud::kimi-k3`, `ollama_cloud::qwen3-coder:480b`, `ollama_cloud::deepseek-v4.1-flash`, `ollama_cloud::glm-5.3-flash`) for Ollama's cloud — without the prefix, `kimi-…`, `glm-…`, `deepseek-…`, `minimax-…` and `mimo-…` tags go to that vendor's own API. Unset: `claude-sonnet-5-5`, or `ollama_cloud::kimi-k3` when only an Ollama key exists |
| `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `OLLAMA_API_KEY` | provider key; wins over the Keychain |
| Keychain `cordata-anthropic-api-key`, `cordata-ollama-api-key` | provider key (`make key` stores one: `PROVIDER=anthropic` by default, or `ollama`) |

Edits and commands ask first unless a rule says otherwise; "Always allow" lasts for the session and is per tool (a
command that asks for the network is asked about on its own). Reads and searches do not ask. `/permissions` shows what is
in effect.

| key | values | default | what |
|---|---|---|---|
| `chat.allow` | list of rules | none | calls that run without asking. **User file only** |
| `chat.mode` | `ask`, `auto` | `ask` | `ask` asks before each edit or command; `auto` lets fixed rules (`auto::judge`, no model call) allow the ones they can show are safe and asks about the rest (with its reason). **Read only from the user file**: a project or local value is ignored with a warning. Switch per session with ⇧⇥. See `docs/auto-mode.md`. The guard, `chat.deny`, `chat.network = off` and the sandbox still apply |
| `chat.deny` | list of rules | none | calls refused without asking; any file may add them |
| `chat.network` | `ask`, `off` | `ask` | commands run offline; `ask` lets a command request the network (you are asked), `off` refuses it. A project may set `off` |
| `chat.writable` | list of absolute paths (`~/` allowed) | none | folders commands may write besides the project, temporary folders and package caches. **User file only** |
| `chat.history` | `on`, `off` | `on` | save sessions to `~/.cordata/sessions` for `/resume`. A project may set `off` |
| `chat.ollamaNumCtx` | integer ≥ 2048 | `65536` | the context window asked of a local Ollama model on every request (`options.num_ctx`); without it Ollama uses its small server default and drops the start of long chats. **User file only**. Ollama Cloud is unaffected. A bigger window needs more memory for the model's cache (at 65536, several GB for a 7-8B model); lower it on a small Mac. A model name no provider prefix claims (not `claude…`, `gemini…`, `gpt…`, `ollama_cloud::…`) is treated as a local Ollama model |
| `mcp.approved` | list of `<folder>\|<name>\|sha256:<digest>` | none | project MCP servers you approved, each pinned to its exact command, arguments and environment; written by Settings → MCP → Approve. **User file only** |

MCP servers: `~/.cordata/mcp.json` (`{"mcpServers": {name: {command, args, env}}}`) applies everywhere; a project's
`.mcp.json` server only as approved (THREAT_MODEL.md, "MCP servers").

`code_map` (the repo map tool) ranks definitions by the reference graph and the question's words; with a Model2Vec model
in `~/.cordata/models/potion-code-16M` (or `CORDATA_EMBED_MODEL=<folder>`) it adds meaning. Nothing is downloaded for you:
`config.json`, `tokenizer.json` and `model.safetensors` from huggingface.co/minishlab/potion-code-16M (64 MB).

A rule is `tool` or `tool(pattern)`, for `read_file`, `edit_file`, `run_command`, `code_search`, `code_outline`. For
`run_command` the pattern is a glob over the whole command (`*` matches anything); for the file tools a glob over the
project-relative path (`*` stays in one folder, `**` crosses folders). Deny beats allow. An allow rule never matches a
chained, piped, redirected or substituted command, nor one that asks for the network (that card has no "always"
either). Path patterns ignore case and match where the path lands after symlinks. A deny rule is checked against every
simple command in the line (`env git push`, `(git push)`, `sh -c 'git push'` all match `run_command(git push*)`), but it
matches spellings, not meaning: treat it as a convenience, not a boundary. If `~/.cordata/settings.json` does not parse,
the chat fails closed (no allow rules, no network, no history) and says so.

An allow rule for a build or test tool runs code the repository controls: `run_command(cargo test*)` together with
`edit_file(src/**)` lets the agent write a test and run it with no card; `npm run *` and `make *` run whatever the
project's scripts say. The sandbox still applies. Write such rules for projects you trust:

```json
{"chat": {"allow": ["run_command(cargo test*)", "run_command(cargo clippy*)", "edit_file(src/**)"],
          "deny": ["run_command(git push*)"]}}
```

**Sessions.** Saved after every turn, credentials masked (`THREAT_MODEL.md`). `/history` lists this project's,
`/resume [id]` continues one (the last by default; an id prefix is enough), `acp-tui --resume [id]` does it at start-up,
and `/forget` deletes them all (it asks you to type `/forget confirm`).

**Instructions.** At the first prompt the agent reads, broad to specific: `~/.cordata/CORDATA.md`, the first of
`CORDATA.md`, `.cordata/CORDATA.md`, `AGENTS.md`, `CLAUDE.md` in the project, then `CORDATA.local.md` (this machine).
Each is capped at 20,000 characters. `/memory` lists what was found and `/init` has the agent draft a `CORDATA.md`.

**Display preferences** in the user, project or local file: `tui.thinking` and `tui.toolOutput` (`collapsed` or
`expanded`); `/thinking` and `/tools` flip them for the session.

## Orchestrator (`cordata run`)

| key | values | default | what |
|---|---|---|---|
| `orchestrator.workers` | comma list of `claude_code`, `antigravity`, `ollama` | all three | which worker families may work; `ollama` runs Ollama Cloud models through Cordata's own engine and needs an Ollama key |
| `orchestrator.models` | list of model ids | unset | a family with a model named here runs only the named models (for example `["gemini-3.8-flash-medium"]` keeps antigravity on that one model); other families keep their roster |
| `orchestrator.pools` | list of pools | conservative caps | your real plans, so handoffs happen at the right moment (`docs/orchestrator.md`) |
| `orchestrator.maxUsd` | number | 2 | spend cap for one run |
| `orchestrator.maxTokens` | number | 0 (none) | token cap for one run |
| `orchestrator.harden` | bool | true | a second model family reviews each step |
| `orchestrator.tier2` | bool | true | the planner audits the result against its plan |
| `orchestrator.stepCheck` | bool | true | after each step that changed Rust sources, `cargo check` runs where the step worked; a change that does not build fails the step and the next attempt gets the compiler errors. Off for a run whose project did not build before its first step |
| `orchestrator.keepForReview` | bool | true | when retries and the replan are spent but the change is in scope, builds, and at least two checks in three pass, the run ends in `to_review` with the work left in the project and the failing checks named, instead of restoring the project |
| `orchestrator.screen` | bool | true | a reviewer model screens the audit's commands before any runs (`--no-screen` for one run); the pattern guard runs either way |
| `orchestrator.dispatch`, `.lambda`, `.mu` | | | the learned dispatch policy and its cost and risk weights |

A family whose CLI is not installed (or, for `ollama`, whose key is missing) is left out. `CORDATA_CLAUDE_BIN` and `CORDATA_AGY_BIN` name a binary explicitly.

## Agent Vault (`vault.*`)

The coding CLIs' traffic goes through the Agent Vault proxy, or the run is refused unless you pass `--allow-direct`.

| key | what |
|---|---|
| `vault.enabled` | route worker traffic through an Agent Vault proxy you run (Cordata does not ship one; default off, so `cordata run` needs `--allow-direct`) |
| `vault.addr` | the proxy address (default `http://127.0.0.1:14322`) |
| `CORDATA_VAULT_DIRECT` | environment; allow direct egress for orchestrator runs |

## Tool hooks (`hooks.json`)

The chat engine runs hooks around each tool call. It reads `.cordata/hooks.json` in the project when you have approved
that exact file, otherwise `~/.cordata/hooks.json` (`$CORDATA_HOME/hooks.json`; the two are not merged). Code: `tui/crates/acp-core/src/hooks.rs`.

```json
{ "hooks": {
    "PreToolUse":  [{ "matcher": "run_command", "hooks": [{ "type": "command", "command": "./scripts/check.sh" }] }],
    "PostToolUse": [{ "matcher": "*", "hooks": [{ "type": "checksyntax" }, { "type": "checkassertions" }] }]
} }
```

- `matcher` is a tool name or `*`.
- `command` gets `{event, tool, args, cwd}` (plus `output` and `success` after the call) as JSON on stdin. Stdout of
  `{"decision":"block","reason":"…"}` (or, after the call only, `{"decision":"feedback","reason":"…"}`) decides; otherwise a non-zero exit
  blocks with stderr as the reason, and exit 0 allows. A hook that runs past `timeout_secs` (default 30) is killed and blocks the call.
- `checksyntax` (JSON, TOML, Rust) and `checkassertions` (refuses edits that strip test assertions) run after edits only.
- Hook commands run through `/bin/sh` without a per-call prompt; your own hooks run unconfined, a project's run inside
  the same Seatbelt sandbox as `run_command` (offline, writing only the project and temp folders, no `.env` or
  credential files). A project `hooks.json` is code
  the repository would run on your machine, so it runs only once approved: the chat's notice gives the entry
  (`<folder>|.cordata/hooks.json|sha256:<digest>`) to add to `mcp.approved` in `~/.cordata/settings.json`. The digest
  covers the file and every file in the project that a hook command names (`./scripts/check.sh`), so editing either
  holds it back again. A script reached some other way (sourced by a named script, found on `PATH`) is not pinned, but
  runs inside the sandbox all the same. A
  malformed project file does not run, and the chat says so.

## Built-in guard

Before the approval prompt, the chat engine refuses (without asking) edits to `.env` files and inside `.git/`,
recursive deletes of `/`, `~` or `..`, and shell redirects into `.env`. It is not configurable. The same rules live in
`tui/crates/cordata-guard`.

## The native app (`app.*`)

Kept in the window's own preferences, `~/Library/Application Support/Cordata/prefs.toml` (`CORDATA_APP_PREFS_DIR`
moves it), and changed from Settings (⌘,): `theme` (`system`, `light` = Paper, `2049` = Ink, `graphite`, `dim`),
`density`, `code_font_size`, `send_with`, `open_on_launch`, `default_mode` (`canvas`, `transcript`, `sheet`),
`restore_threads`, `notify_approval`, `notify_done`, `menu_bar` (the menu bar item: on by default; while it is up the
close button hides the app and ⌘Q quits), `sound_on_completion` (the two sounds), `quick_ask_hotkey` and
`quick_ask_key`, plus what the window remembers (`sidebar_open`, `panel_open`, `model`, recent projects). A file that does not parse is kept beside it as `prefs.toml.damaged-<time>` before the defaults
replace it.

## Environment

| variable | meaning |
|---|---|
| `CORDATA_HOME` | state folder (default `~/.cordata`). `cordata` ignores a relative value or one inside the repository you run it from (a direnv `.envrc` could set it), with a warning |
| `CORDATA_STATE_DB` | the ledger database (default `$CORDATA_HOME/state.db`) |
| `CORDATA_ROOT` | the checkout, for `cordata doctor` |
| `CORDATA_SANDBOX_SEATBELT` | how workers are confined; unset means on |
| `CORDATA_THEME` | terminal colour theme |

## Checking

`cordata doctor` lists what is in place, what is not, and the command that fixes it.

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
| `CORDATA_NATIVE_MODEL` | model name, any `genai` name (`claude-…`, `gemini-…`, `gpt-…`, or a local Ollama tag); `ollama_cloud::<tag>` (e.g. `ollama_cloud::glm-5.3-flash`) for Ollama's cloud — without the prefix, `kimi-…`, `glm-…`, `deepseek-…`, `minimax-…` and `mimo-…` tags go to that vendor's own API. Unset: `claude-sonnet-5-5`, or `ollama_cloud::kimi-k3` when only an Ollama key exists |
| `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `OLLAMA_API_KEY` | provider key; wins over the Keychain |
| Keychain `cordata-anthropic-api-key`, `cordata-ollama-api-key` | provider key (`make key` stores one: `PROVIDER=anthropic` by default, or `ollama`) |

Every edit and command asks first; "Always allow" lasts for the session and is per tool. Reads and searches do not ask.
There are no modes or permission rules, and sessions are not saved.

**Instructions.** At the first prompt the agent reads, broad to specific: `~/.cordata/CORDATA.md`, the first of
`CORDATA.md`, `.cordata/CORDATA.md`, `AGENTS.md`, `CLAUDE.md` in the project, then `CORDATA.local.md` (this machine).
Each is capped at 20,000 characters. `/memory` lists what was found and `/init` has the agent draft a `CORDATA.md`.

**Display preferences** in the user, project or local file: `tui.thinking` and `tui.toolOutput` (`collapsed` or
`expanded`); `/thinking` and `/tools` flip them for the session.

## Orchestrator (`cordata run`)

| key | values | default | what |
|---|---|---|---|
| `orchestrator.workers` | comma list of `claude_code`, `antigravity` | both | which CLI families may work |
| `orchestrator.pools` | list of pools | conservative caps | your real plans, so handoffs happen at the right moment (`docs/orchestrator.md`) |
| `orchestrator.maxUsd` | number | 2 | spend cap for one run |
| `orchestrator.maxTokens` | number | 0 (none) | token cap for one run |
| `orchestrator.harden` | bool | true | a second model family reviews each step |
| `orchestrator.tier2` | bool | true | the planner audits the result against its plan |
| `orchestrator.dispatch`, `.lambda`, `.mu` | | | the learned dispatch policy and its cost and risk weights |

A CLI that is not installed is left out. `CORDATA_CLAUDE_BIN` and `CORDATA_AGY_BIN` name a binary explicitly.

## Agent Vault (`vault.*`)

The coding CLIs' traffic goes through the Agent Vault proxy, or the run is refused unless you pass `--allow-direct`.

| key | what |
|---|---|
| `vault.enabled` | route worker traffic through an Agent Vault proxy you run (Cordata does not ship one; default off, so `cordata run` needs `--allow-direct`) |
| `vault.addr` | the proxy address (default `http://127.0.0.1:14322`) |
| `CORDATA_VAULT_DIRECT` | environment; allow direct egress for orchestrator runs |

## Built-in guard

Before the approval prompt, the chat engine refuses (without asking) edits to `.env` files and inside `.git/`,
recursive deletes of `/`, `~` or `..`, and shell redirects into `.env`. It is not configurable. The same rules live in
`tui/crates/cordata-guard`.

## The native app (`app.*`)

Kept in the app's own preferences file (`CORDATA_APP_PREFS_DIR` moves it): `app.theme`, `app.density`,
`app.notifications`, `app.restoreWindows`, `app.showReasoning`.

## Environment

| variable | meaning |
|---|---|
| `CORDATA_HOME` | state folder (default `~/.cordata`) |
| `CORDATA_STATE_DB` | the ledger database (default `$CORDATA_HOME/state.db`) |
| `CORDATA_ROOT` | the checkout, for `cordata doctor` |
| `CORDATA_SANDBOX_SEATBELT` | how workers are confined; unset means on |
| `CORDATA_THEME` | terminal colour theme |

## Checking

`cordata doctor` lists what is in place, what is not, and the command that fixes it.

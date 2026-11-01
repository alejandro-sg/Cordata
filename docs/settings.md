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

## Chat agent (environment)

The terminal client and the app run the agent in their own process. It does not read the settings files; the model and
keys come from the environment.

| variable | meaning |
|---|---|
| `CORDATA_NATIVE_MODEL` | model name, any `genai` name (`claude-…`, `gemini-…`, `gpt-…`, or an Ollama tag). Default `claude-sonnet-5-5` |
| `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `OLLAMA_API_KEY` | provider key; wins over the Keychain |
| Keychain `cordata-anthropic-api-key`, `cordata-ollama-api-key` | provider key (`make key` stores the Ollama one) |

Every tool that changes files or runs a command asks first; "Always allow" lasts for the session. Reads and searches do
not ask. Sessions are not saved yet, so `--resume` and `-c` start a fresh one.

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

Coding CLIs never run with direct network access: their traffic goes through the Agent Vault proxy, or the run is refused.

| key | what |
|---|---|
| `vault.enabled` | route worker traffic through the proxy (default off, so `cordata run` needs `--allow-direct` until it is on) |
| `vault.addr` | the proxy address (default `http://127.0.0.1:14322`) |
| `CORDATA_VAULT_DIRECT` | environment; allow direct egress for orchestrator runs |

## Hooks

`hooks.preToolUse` names a command run before a tool call; `hooks.matcher` selects tools. Exit code 2 blocks the call and
sends stderr to the model. `.cordata/settings.json` points it at `scripts/guard.sh`, which runs `cordata-guard`: no edits
to `.env` or `.git/`, no `rm -rf` of `/`, `~` or `..`, no shell redirect into `.env`.

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

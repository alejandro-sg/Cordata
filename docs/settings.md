# Settings, permissions and instructions

How to set up your preferences for the agent harness and `acp-tui`, in the same layered
shape Claude Code (`settings.json`, `CLAUDE.md`) and Kilo CLI (`kilo.jsonc`) use.

The contract lives here; two implementations follow it and are tested against the same files
(`tui/fixtures/settings/`): `backend/settings_store.py` (enforces permissions, applies
model / mode / Laya / tier preferences, reads instructions) and `tui/crates/acp-tui/src/settings.rs`
(shows what's in effect, applies display preferences, writes changes).

## Files

Three JSON files; a later one wins on a conflict.

| scope | file | for |
|---|---|---|
| `user` | `~/.sierralab/settings.json` (`SIERRALAB_HOME` moves the directory) | your own defaults everywhere (written with mode `0600`) |
| `project` | `<project>/.sierralab/settings.json` | shared with the team — commit it |
| `local` | `<project>/.sierralab/settings.local.json` | this machine only; created with a `.gitignore` next to it |

Plain JSON, no comments. A file that isn't valid JSON is ignored and reported (the TUI shows a
notice at connect; `/config path` lists the files) — and **never overwritten** by a write.

Objects merge key by key. `permissions.allow`, `permissions.ask` and `permissions.deny`
**accumulate** across the three files (a project can add a `deny` without erasing yours); every
other value is replaced by the higher scope.

## Settings

| key | values | applies | what |
|---|---|---|---|
| `permissions.defaultMode` | `plan` · `build` · `auto` | next session | mode a session starts in (default `build`) |
| `permissions.allow` / `ask` / `deny` | list of rules | live | see [Permission rules](#permission-rules) |
| `model` | model id | now | model on the active provider; ignored (with a log line) if the provider doesn't offer it |
| `temperature` | `focused` · `balanced` · `creative` | now | sampling preset |
| `laya.mode` | `off` · `shadow` · `route` | next session | Laya triage (see `docs/laya-triage.md`) |
| `laya.threshold` | 0–1 | next session | confidence needed before Laya adds a routing hint |
| `laya.dispatch` | `direct` · `hint` | next session | confident specialist routes: run the specialist directly, or only hint |
| `laya.teamDispatch` | `direct` · `hint` | next session | confident team routes: run the decompose+graph pipeline directly (see `docs/agentic-harness.md`), or only hint |
| `laya.minConfidence` | 0–1 | next session | below this `answer_confidence` Laya abstains (fail closed) |
| `laya.onAbstain` | `escalate` · `llm` | next session | when Laya abstains: escalate to `tiers.strong`, or let the LLM decide |
| `selfHeal.enabled` | `true` · `false` | next session | diagnose and fix failing commands autonomously (see `docs/self-healing-architecture.md`) |
| `selfHeal.maxAttempts` | 1–10 | next session | debugger rounds per failure before escalating to you |
| `selfHeal.verify` | `sandbox` · `workspace` · `off` | next session | verify patches in a throwaway copy first, or only in the workspace |
| `selfHeal.autoApply` | `true` · `false` | next session | apply sandbox-verified fixes without asking (needs a trusted folder; see `THREAT_MODEL.md`) |
| `lsp.enabled` | `true` · `false` | next session | start the project's language servers for diagnostics after edits |
| `tiers.fast` / `standard` / `strong` | model id | next session | sub-agent model tiers |
| `tui.thinking` | `collapsed` · `expanded` | now | reasoning as one line or in full |
| `tui.toolOutput` | `collapsed` · `expanded` | now | tool results trimmed or in full |
| `tui.showLaya` | `true` · `false` | now | show Laya's triage lines |
| `trustedProjects` | list of paths | live | **user file only** — see [Trust](#trust) |

A value from a settings file beats the `.env` / environment default for the same thing
(`LAYA_TRIAGE_MODE`, `MODEL_TIER_*`). Unset keys leave the environment alone.

**Not settings:** the model *provider*, its base URL and its credentials. Those are deployment
configuration (`backend/.env`, environment, macOS Keychain — `make key`). A settings key named
like a credential (`apiKey`, `token`, `password`…) or a value shaped like one (`sk-…`, a JWT-ish
string, `ghp_…`) is refused and reported, so a key can't be pasted into a file the agent can read.
Unknown keys are ignored with a warning.

## Changing them

In the TUI:

```text
/config                              interactive panel (↑↓ select · Enter change · ⌫ reset · Tab choose the file · Esc)
/config set tui.thinking expanded    write to the user file
/config set laya.mode shadow project …or to the project / local file
/config unset tui.thinking           remove it from that file
/config path                         the files, which exist, which instruction files are read
/permissions                         every rule, its file, and whether it's active
/permissions allow shell(git status) local
/permissions deny  file_write(*.env) project
/permissions remove shell(git status)
/trust                               trust this folder (below)
/memory                              instruction files in the system prompt
/init                                ask the agent to draft SIERRALAB.md for this project
/status                              session, mode, model, Laya, rule counts
```

…or edit the JSON by hand — the backend re-reads the files whenever they change (three `stat`
calls per tool call), so permission edits made in your editor apply mid-session.

Display preferences apply immediately but only to output that hasn't been printed yet: finished
output lives in your terminal's scrollback and can't be restyled.

## Permission rules

```json
{
  "permissions": {
    "defaultMode": "build",
    "allow": ["shell(git status)", "shell(npm test *)", "file_read"],
    "ask":   ["file_write(src/*)"],
    "deny":  ["shell(rm *)", "http_request(https://evil.test/*)"]
  }
}
```

A rule is a tool name, optionally with a pattern: `shell`, `shell(git status)`, `shell(git *)`.
`*` matches any text. What the pattern is matched against:

| tool | matched against |
|---|---|
| `shell` | the command |
| `python_repl` | the code |
| `file_read` · `file_write` · `editor` | the path (relative paths and relative patterns resolve against the project directory; `~` expands; `..` is normalised first) |
| `http_request` | the URL |
| anything else (MCP tools, generated tools) | the tool name only |

How rules combine with the mode:

- **`deny` beats `ask` beats `allow`**, and **`deny` holds in every mode, `auto` included**. The
  agent is told which rule blocked the call, and the call never reaches an approval prompt.
- `plan` mode runs nothing, whatever the rules say.
- `build`: an `allow` rule skips the prompt; an `ask` rule prompts even for a tool that normally
  wouldn't; with no matching rule the usual gate applies (`shell`, `python_repl`, `file_write`,
  `editor`, generated and MCP tools ask).
- `auto`: nothing asks; only `deny` rules still block.

Shell commands are split on `;  &&  ||  |  &` and newlines, because `allow shell(git *)` must not
approve `git status && rm -rf ~`:

- a **deny** pattern matches if any part (or the whole line) matches;
- an **allow** pattern *without* `*` matches only that exact command;
- an **allow** pattern *with* `*` has to match **every** part, and never applies to a command that
  contains `$( )`, backticks, `<( )`, or a redirect (other than to `/dev/null` / `2>&1`);
- `shell(git status *)` also matches a bare `git status`.

Built-in denies, always on and shown by `/permissions` as `built-in`: reading a credential out of
the macOS Keychain from the agent's shell (`security … find-generic-password`, `find-internet-password`,
`dump-keychain`). The backend also masks the provider key in every tool result
(`THREAT_MODEL.md`); this refuses the call outright.

**These rules are a convenience on top of the approval prompt, not a sandbox.** A command a pattern
doesn't anticipate can do something it didn't intend (`allow shell(npm *)` lets `npm exec` run
anything). Prefer exact commands.

### "Always allow"

When the backend can state a narrow rule for a call, the approval box offers it:

```text
 a  Allow     r  Reject (Esc)
 s  Always allow shell(git status)
```

`s` allows the call and saves that exact rule to `settings.local.json` (this machine, this
project), effective immediately. It also marks the folder trusted, since the rule would otherwise
be ignored (next section). No rule is offered for `python_repl`, for commands with wildcards or
several lines, or for a pattern containing `)`.

## Trust

A cloned repository can ship its own `.sierralab/settings*.json`. Its `deny` and `ask` rules only
restrict, so they always apply. But `allow` rules and a more permissive default mode would let a
stranger's repo switch off the approval prompts, so **project and local `allow` rules, and
`permissions.defaultMode: "auto"` from those files, are ignored until the folder is trusted**: listed in the
*user* file's `trustedProjects`. `/trust` does that; so does choosing "Always allow". The user file
itself is always trusted. `/permissions` marks ignored rules `← ignored: folder not trusted`.

Instruction files from a folder you haven't looked at are still read (see below) — the banner tells
you which — but they can only advise the model; every gated tool call still needs your approval.

## Instructions (the `CLAUDE.md` equivalent)

Read into the system prompt when a session starts, broad to specific, capped at 16 000 characters
in total:

1. `~/.sierralab/SIERRALAB.md` — yours, everywhere
2. the project's: the first that exists of `SIERRALAB.md`, `.sierralab/SIERRALAB.md`, `AGENTS.md`,
   `CLAUDE.md` — so a repo set up for another tool still works
3. `SIERRALAB.local.md` — this machine only (gitignored by the `.gitignore` next to the local settings)

`/init` has the agent read the project and write `SIERRALAB.md` (it asks before writing, like any
file write). `/memory` lists what's being read. Changes apply to the next session.

## Where each thing is applied

| | read by | when |
|---|---|---|
| permission rules, trust | backend (`_ApprovalHandler`) | every tool call |
| `defaultMode`, `model`, `temperature` | backend (`new_session` / `load_session`) | session start; `/config` also sets model/temperature on the running session |
| `laya.*`, `tiers.*` | backend (`_apply_startup_preferences`) | session start |
| instruction files | backend (system prompt) | session start |
| `tui.*` | acp-tui | startup and on change |

## Checking the result

```sh
backend/.venv/bin/python backend/settings_store.py ~/some/project   # merged values, sources, rules, warnings
```

Tests: `backend/_settings_store_smoke_test.py` (no model), `backend/_acp_settings_smoke_test.py`
(real server and model: defaults, instructions, deny/allow, always-allow), `cargo test -p acp-tui`
(the Rust reader against the same fixture).

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
| `laya.callGuard` | `off` · `rules` · `full` | next session | Laya Guard: score **every tool call, by every agent**, and ask at or above that agent's gate in every mode, `auto` included (`docs/agentic-harness.md`). `full` (default) adds Laya's own reading, which can only raise a score |
| `checkpoints.enabled` | `true` · `false` | next session | snapshot the project before any call that may change files, so `/undo` can reverse it (default `true`; trust-gated — a project file cannot turn it off; see `docs/agentic-harness.md`) |
| `checkpoints.maxFileMb` | 0.5–512 | next session | files larger than this are left out of snapshots (default 8; trust-gated) |
| `laya.mode` | `off` · `shadow` · `route` | next session | Laya triage (see `docs/laya-triage.md`) |
| `laya.threshold` | 0–1 | next session | confidence needed before Laya adds a routing hint |
| `laya.dispatch` | `direct` · `hint` | next session | confident specialist routes: run the specialist directly, or only hint |
| `laya.teamDispatch` | `direct` · `hint` | next session | confident team routes: run the decompose+graph pipeline directly (see `docs/agentic-harness.md`), or only hint |
| `laya.minConfidence` | 0–1 | next session | below this `answer_confidence` Laya abstains (fail closed) |
| `laya.onAbstain` | `escalate` · `llm` | next session | when Laya abstains: escalate to `tiers.strong`, or let the LLM decide |
| `selfHeal.enabled` | `true` · `false` | next session | diagnose and fix failing commands autonomously (see `docs/self-healing-architecture.md`) |
| `selfHeal.maxAttempts` | 1–10 | next session | debugger rounds per failure before escalating to you |
| `selfHeal.verify` | `sandbox` · `workspace` · `off` | next session | verify patches in a throwaway copy first, or only in the workspace |
| `selfHeal.memory` | `true` · `false` | next session | remember sandbox-verified fixes (failure signature → patch, per project) and try a remembered one in the sandbox before asking the Debugger; a fix that stops working retires itself (`docs/self-healing-architecture.md`) |
| `selfHeal.autoResume` | `true` · `false` | next session | continue a turn a crash interrupted on its own when the session reloads (twice at most per checkpoint, never over 30 minutes old; approvals still ask) |
| `selfHeal.autoApply` | `true` · `false` | next session | apply sandbox-verified fixes without asking (needs a trusted folder; see `THREAT_MODEL.md`) |
| `lsp.enabled` | `true` · `false` | next session | start the project's language servers for diagnostics after edits |
| `vault.enabled` | `true` · `false` | next session | run tool subprocesses through the Agent Vault credential proxy (see below) |
| `vault.addr` | url | next session | where the vault's proxy listens (default `http://127.0.0.1:14322`) |
| `vault.vault` | name | next session | which vault credentials are drawn from |
| `vault.egressPolicy` | `deny` · `proxy` | next session | vault unreachable: refuse egress, or route through the address without credentials |
| `observer.enabled` | `true` · `false` | next cycle | the daemon's background observer (see below); off by default |
| `observer.intervalMinutes` | 1–1440 | next cycle | how long between observations (default 30) |
| `memory.traces` | `true` · `false` | next session | record execution traces to the local LanceDB corpus |
| `learning.reflect` | `true` · `false` | next turn | draft a lesson from a failed or reworked turn into staging (see `docs/agentic-harness.md`) |
| `learning.promotion` | `off` · `shadow` · `live` | next turn | promote a staged lesson, skill or prompt overlay only when it raises the eval score: `shadow` records the verdict and promotes nothing (trust-gated — a project file cannot turn it on) |
| `learning.evalTasks` | 3–60 | next turn | how many eval tasks a promotion candidate is judged on |
| `learning.optimizePrompts` | `true` · `false` | next turn | draft a sharper variant of a live lesson or prompt overlay whose turns scored below par, into staging for the same gate (trust-gated — a project file cannot turn it on) |
| `learning.bandit` | `off` · `shadow` · `live` | next turn | learn which harness choice to make (route · engine · ask) from turn outcomes; `shadow` records the choice and acts on nothing (trust-gated) |
| `learning.banditExplore` | 0.0–0.5 | next turn | ε: how often `live` takes a non-best arm so it keeps learning (default 0.1) |
| `learning.banditMinPulls` | 1–100 | next turn | a choice is not moved until every arm has this many outcomes behind it (default 5) |
| `learning.banditLambda` | 0.0–1.0 | next turn | λ in `verifier success − λ·cost − μ·correction` — how much tokens, time and Claude Code dollars count against a turn |
| `learning.banditMu` | 0.0–1.0 | next turn | μ in the same reward — how much refusals, reverts and a cancelled turn count against it |
| `kg.enabled` | `true` · `false` | next session | record graph entities and edges from each turn (see below) |
| `kg.sink` | `lance` · `cognee` | next session | which graph backend keeps the rows (default `lance`) |
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

The same gate covers the keys that decide how much the harness protects you or where data goes:
`selfHeal.autoApply`, `vault.enabled`, `vault.addr`, `vault.vault`, `vault.egressPolicy` and
`kg.sink`. A repo that could set them could switch off the egress proxy you turned on, make its
refusal fail open, or point its address at a host of its own. Set in the user file, or in a folder you
trusted, they apply; from an untrusted project or local file they are ignored and reported as a
warning.

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
| `vault.*` | backend (`vault_proxy.install`) | session start |
| `observer.*` | daemon (`Observer`) | every cycle, so a change lands without a restart |
| `memory.traces`, `kg.*` | backend (`_ingest_turn`) | session start, applied when the turn finishes |
| `learning.*` | backend (`reflect` / `optimize` / `bandit`, `_maintain`) | every turn, read fresh, so a change lands without a restart |
| instruction files | backend (system prompt) | session start |
| `tui.*` | acp-tui | startup and on change |

## Agent Vault (`vault.*`)

Off by default. Turned on, tool subprocesses do not hold real credentials —
they hold placeholders (`OPENAI_API_KEY=__openai_api_key__`) and the process is
pointed at the vault's local proxy, which substitutes the real value on the way
out. The vault token lives in the Keychain (`sierralab-agent-vault-token`),
never in a settings file, and the vault's CA is named by `SIERRALAB_VAULT_CA`
when one has been exported.

With `vault.egressPolicy: "deny"` (the default), a vault that cannot be reached
at `vault.addr` means `shell`, `python_repl` and `http_request` are *refused*
for the session — in `auto` mode too, with no prompt offered — and no proxy
variable is left set, so nothing can quietly egress directly. `"proxy"` keeps
work going through the configured address without vault credentials. This closes
credential exposure on egress; it is **not** a filesystem sandbox — see
`THREAT_MODEL.md`, "Agent Vault is egress-only". Tests:
`backend/_vault_proxy_smoke_test.py`.

## Knowledge graph (`kg.*`)

Off by default. Turned on, the turn that just finished is fed to a graph as
entities and edges: the project and session, each tool that ran, each file a
path-bearing tool named, and each error signature. It is **derived, not
summarised** — no model is asked what the entities were, so the same turn always
produces the same graph, and a node id is stable across turns so a file touched
fifty times is one node.

No prompts and no command lines are stored: those are `state.db`'s and
`memory.traces`' job, and the graph is the structure over them (which tool
touched which file, what failed where). Storage is `backend/memory.kg/`
(gitignored, `SIERRALAB_KG_DIR` moves it): `graph.lance` for queries, plus
`nodes.jsonl` / `edges.jsonl` mirrors written even when `lancedb` is not
installed — so a checkout without it still has a graph readable with `jq`.

`kg.sink` picks the backend. `lance` is the default and needs nothing beyond the
pinned `lancedb`. `cognee` hands the same rows to Cognee and is for a machine
that has it installed; **Cognee is not in `requirements.txt` and this path has
not been run against a real Cognee install** — with it missing, `kg.sink:
"cognee"` logs once and still writes the mirrors. Reads: `LanceGraphSink.search`
(substring over labels) and `.neighbours` (the edges touching a node). Tests:
`backend/_kg_smoke_test.py`.

## Observer (`observer.*`)

Off by default, and only meaningful under `make daemon`: it is the one part of
the harness that runs with no user turn behind it. Turned on, the daemon reads
each known project (the newest five in `backend/sessions/index.json`) with a
fixed `git` command list — `status`, `diff --stat`, `log`, `diff` — plus the
tails of any log files named by `SIERRALAB_OBSERVER_LOGS`, hands that to a
read-only agent, and writes `nightly_briefing.md` (override:
`SIERRALAB_OBSERVER_BRIEFING`) in `~/.sierralab/`.

The observer holds `file_read` and `think`, and nothing else: the daemon runs
the commands, not the model, and the briefing is written by the daemon from the
text the child prints. A client attaching kills any cycle in flight, and no
cycle starts while one is attached — a briefing written over a project someone
is working in is the one thing this must not do. `SIERRALAB_OBSERVER_QUIET`
(`22:00-07:00`) stops it running at night; `SIERRALAB_OBSERVER_TIER` picks the
model tier.

Proposals are staged on the ledger with status `proposed`, not `staged` — they
are not approvals somebody is waiting on, so they never appear in the
`--resume` pending-approval notice. The briefing is the thing to read; the
ledger keeps the record (`interventions.kind = 'observation'` plus an
`observation` event, readable with `state_store.recent_events(kind="observation")`).
Tests: `backend/_observer_smoke_test.py`.

## Discord remote control (environment only, not settings)

There is deliberately no `discord.*` settings key. Enabling the daemon's Discord
front door is environment-only, so nothing about it lands in a layered,
printed, shareable `settings.json` — in particular not the bot token, which
follows the same Keychain path as the provider keys (see `.env.example` and
`THREAT_MODEL.md`, "Discord remote control").

| Variable | Meaning |
| --- | --- |
| `DISCORD_BOT_TOKEN` | The bot token. Prefer `make discord-key` (Keychain service `sierralab-discord-bot-token`) or export it; **not** a file value. Read once at startup, then removed from the process environment and masked everywhere. |
| `ALLOWED_USER_ID` | The one Discord user id allowed to approve, reject or steer (Developer Mode → Copy ID). Required — with no allowlist the bot stays off. |
| `TELEMETRY_CHANNEL_ID` | The channel the bot announces in, opens task threads under, and reads steering replies from. |
| `SIERRALAB_DISCORD` | `0`/`off`/`false`/`no` disables the bot even when the three above are set (kill switch, checked before the token is read). |

All three of the first row's names must be present and parse (the ids as
integers) for the bot to start; otherwise it logs the reason and is a no-op.
`GET /health` reports `discord: {enabled, connected, allowedUser, channel,
reason}` — never the token. Message Content Intent must be enabled for the
application in the Discord Developer Portal, or `discord.PrivilegedIntentsRequired`
is logged as the actionable message it is rather than crashing the daemon.
Tests: `backend/_discord_remote_smoke_test.py` (hermetic, fake `discord`).

## Checking the result

```sh
backend/.venv/bin/python backend/settings_store.py ~/some/project   # merged values, sources, rules, warnings
```

Tests: `backend/_settings_store_smoke_test.py` (no model), `backend/_acp_settings_smoke_test.py`
(real server and model: defaults, instructions, deny/allow, always-allow), `cargo test -p acp-tui`
(the Rust reader against the same fixture).

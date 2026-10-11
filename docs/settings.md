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
| `lsp.enabled` | `true` · `false` | next session | start the project's language servers for diagnostics after edits (in an untrusted folder rust-analyzer runs without build scripts, proc-macros or `cargo check` — the repo's own code) |
| `preflight.enabled` | `true` · `false` | next session | check every `apply_patch` / `file_write` / `editor` change **before** it is asked about or applied — syntax, `ruff` (on PATH), the language server's errors on the unsaved text — and send what it newly breaks back to the agent instead (`docs/agentic-harness.md`; trust-gated) |
| `preflight.maxRetries` | 0–10 | next session | failed checks sent back per file per turn before the turn is told to stop editing (default 3; trust-gated) |
| `preflight.tests` | a command | next session | also run this (e.g. `pytest -x -q`) in a throwaway copy with the edit applied; a failure the copy *without* the edit shows too is not blamed. Runs the project's code, so it is trust-gated, and skipped while the vault refuses egress (default empty) |
| `preflight.rollback` | `true` · `false` | next session | when the agent runs out of attempts, put back the files the turn changed (from its first checkpoint; undoable) (default `true`; trust-gated) |
| `reasoning.adaptive` | `true` · `false` | next session | Ollama Cloud: the orchestrator's `reasoning_effort` follows the project's size and Laya's reading of each turn's complexity — never below the role's level (the Lab dial, `OLLAMA_ROLE_ORCHESTRATOR`), never `max` unless chosen (default `true`) |
| `learning.rejections` | `true` · `false` | next session | turn each change you reject into a rule (the ReflectionAgent, one tool-less model call; your note, if you gave one, is the reason) that later turns in this project are held to — `python backend/rejection_memory.py list` to read them, `forget <id>` to drop one (default `true`; trust-gated) |
| `vault.enabled` | `true` · `false` | next session | run tool subprocesses through the Agent Vault credential proxy (see below) |
| `vault.addr` | url | next session | where the vault's proxy listens (default `http://127.0.0.1:14322`) |
| `vault.vault` | name | next session | which vault credentials are drawn from |
| `vault.egressPolicy` | `deny` · `proxy` | next session | vault unreachable: refuse egress, or route through the address without credentials |
| `capture.enabled` | `true` · `false` | next launch | the menu-bar deck may read screen context at all (see below); off by default |
| `capture.allowlist` | list of bundle ids | next launch | apps the deck may read, e.g. `["com.apple.Safari"]`; empty reads nothing |
| `capture.denylist` | list of bundle ids | next launch | apps the deck must not read, on top of the built-in never-read list |
| `capture.screenshot` | `true` · `false` | next launch | the deck may capture one window as an image (Screen Recording) |
| `capture.screenshotCloud` | `true` · `false` | next launch | a screenshot may go to a cloud vision model; off: local only |
| `capture.visionModel` | model tag | next turn | the vision model a screenshot turn runs on (Ollama); unset: screenshots are never sent |
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
`selfHeal.autoApply`, `vault.enabled`, `vault.addr`, `vault.vault`, `vault.egressPolicy`,
`kg.sink`, Laya's own lines (`laya.mode`, `laya.callGuard`, `laya.riskSafe`, `laya.riskDestroy`),
`preflight.*`, `learning.rejections` and `capture.*`. A repo that could set them could switch off the egress proxy you turned on, make its
refusal fail open, point its address at a host of its own, raise the destructive line past every score, or switch
off the checks in front of its own edits. The full list is `TRUST_GATED_KEYS` in `backend/settings_store.py`
(mirrored in the TUI; `_settings_schema_parity_smoke_test.py` holds the two to it). Set in the user file, or in a folder you
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

## Screen context (`capture.*`)

What the menu-bar deck (`make lab-menubar`, `lab/crates/harness-menubar`) may read off the screen.
Stricter than any other key here: the menubar reads `capture.*` **only from the user file** — a
project's file is ignored with a warning even in a trusted folder, so a cloned repository can never
grant itself a window. You rarely edit these by hand: the deck's **Always allow <app>** button adds
the app and switches capture on, and **Add screenshot** switches `capture.screenshot` on.

```json
{ "capture": { "enabled": true, "allowlist": ["com.apple.Safari", "com.microsoft.VSCode"],
               "screenshot": true, "screenshotCloud": false, "visionModel": "qwen2.5vl:7b" } }
```

- Password managers, Keychain Access and Passwords are **never read**, whatever these say — a constant
  (`harness_client::policy`, mirrored in `backend/os_context.py`), not a setting.
- A screenshot is sent only when `visionModel` names a model the session's Ollama daemon serves: a
  local tag that is installed (nothing is ever pulled), or a `:cloud` tag when `screenshotCloud` is on.
  The turn runs on it once, then the session's model is back and the image is removed from the history.
- Accessibility (title, selection) and Screen Recording (screenshot) are macOS permissions you grant in
  System Settings; the deck's Grant buttons open the prompt. `make lab-menubar-app` builds a signed
  `Sierralab.app` so the grant belongs to it.

See `THREAT_MODEL.md`, "OS context capture".

## Observer (`observer.*`)

Off by default, and only meaningful under `make daemon`: it is the one part of
the harness that runs with no user turn behind it. Turned on, the daemon reads
each known project (the newest five in `backend/sessions/index.json`) with a
fixed `git` command list — `status`, `diff --stat`, `log`, `diff` — plus the
tails of any log files named by `SIERRALAB_OBSERVER_LOGS`, hands that to a
read-only agent, and writes `nightly_briefing.md` (override:
`SIERRALAB_OBSERVER_BRIEFING`) in `~/.sierralab/`.

The observer holds `file_read`, and nothing else: the daemon runs
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

## Token economy

What a model call costs is mostly what it re-sends: the system prompt, every tool's schema, and the whole
conversation so far. `context_diet.py` trims the last two without removing a capability, and the shell and editor are the
SDK's own vended tools (`core_tools.py`), a fifth of the size of the ones they replaced. Measured through the real ACP
server, a plain turn carries 9.0 KB of tool schema, down from 44.1 KB. `SHELL_DEFAULT_TIMEOUT` (seconds, default 900)
is how long a `shell` command may run when the model names no timeout. Defaults are on;
every `context.*` and `claudeCode.*` key here is trust-gated (a cloned repo cannot change what you spend).

| Key | Default | Meaning |
| --- | --- | --- |
| `context.coldTools` | `true` | Hide rarely-used tools (`http_request`, the model/resource managers, `claude_code_engineer`, …) unless the turn mentions their subject, they were used in the last three turns, or Laya routes the turn to `web`. The prompt lists what is hidden; the agent loads one with `enable_tools` and has it on its next step. Measured through the real ACP server: 43.6 KB → 15.1 KB of tool schema per call (−65%). |
| `context.toolOutputChars` | `6000` | The most characters of one tool result the model reads (Strands' own `context_manager="auto"` uses 1,500 tokens, the default its docs report as best on ContextBench). Over it, the result is **offloaded** (`context.offload`) — or, with offloading off, cut keeping its head *and* tail (an error or exit code is at one end) and saying how much was left out. |
| `context.offload` | `true` | Set an over-limit tool result aside whole instead of cutting it — Strands' `ContextOffloader`. The full result is written to `~/.sierralab/offloaded/<session>/` (dir `0700`, files `0600`, registered credentials masked first, deleted after 7 days); the model reads a head+tail preview and a reference, and `retrieve_offloaded_content` returns only the slice it asks for (a regex with context lines, or a line range). Nothing is lost, and a command is never run twice just to see its middle. The read-back is fenced as untrusted data. A specialist's report, the Claude Code CLI's result and the swarm/graph tools are the answer itself and are never offloaded. `false` cuts instead. |
| `context.summaryModel` | `fast` | Who writes the summary when a long chat is compacted (it is a model call you pay for): the **fast model tier** (`MODEL_TIER_FAST`) when one is configured, or `main` for the session's own model. With no fast tier set, both are the same model and nothing changes. |
| `context.webFetch` | `agentic` | What `web_fetch` returns: `agentic` — an analyst model reads the page and answers your prompt about it, so the page never enters the conversation (the analyst runs on the fast model tier when one is configured) — or `markdown`, the page itself (a long one is set aside by the offloader). |
| `context.maxSteps` | `50` | Model calls one turn may make before it pauses (Strands' per-invocation `limits`). The turn stops with ACP's `max_turn_requests`, says why, keeps its history valid, and carries on at "continue" with a fresh budget. A loop that will not converge cannot run on spending your tokens. `0` = no cap. |
| `context.maxTurnUsage` | `0` | Tokens one turn may spend (input + output, all calls) before it pauses the same way. `0` = no cap. |
| `context.skillSearch` | `true` | List only the few skills that fit the request — embedding search, Laya deciding which, if any, clearly fits — instead of all 49 on every call. The agent has a `search_skills` tool for the rest. `false` lists them all (19 KB). |
| `context.skillsListed` | `4` | How many skills are listed per turn. |
| `context.outlineLines` | `500` | A whole-file read (`file_read` mode `view`, `editor` `view` with no range) of a source file over this many lines comes back as its **outline** — classes, functions, methods with line ranges — plus how to read what is needed (`code_outline` with a symbol, a line range, `code_search`). Ahead of the offloader, so the file is never in context or on disk twice. Data files (`.json`, `.log`, …) are left to the offloader. |
| `context.agedOutputChars` | `1200` | Tool results older than the newest 8 messages are shortened to this many characters; so are huge old tool *inputs* (a whole file the agent wrote). The first message (the task) is never touched, every tool call keeps its result. `0` turns ageing off. |
| `claudeCode.model` | `sonnet` | Model alias for Claude Code CLI runs (`""` = the CLI's own default, which is its largest model). |
| `claudeCode.effort` | `medium` | `default` · `low` · `medium` · `high` · `xhigh` · `max`. |
| `claudeCode.maxBudgetUsd` | `5` | Dollar cap per run. |

Not settings, but worth knowing: the observer (`observer.enabled`) reuses its last reading of a project whose
evidence has not changed — no model call at all on an unchanged tree. On Ollama Cloud the **debugger runs at
reasoning effort `max`** (thinking tokens count); `OLLAMA_ROLE_DEBUGGER=kimi-k2.7-code:high` in `backend/.env`
is the cheaper setting. Tests: `backend/_context_diet_smoke_test.py` (the hook through a real Strands loop) and
`backend/_diet_acp_smoke_test.py` (through the real ACP server, with the byte measurement).

## Discord remote control (environment only, not settings)

There is deliberately no `discord.*` settings key. Enabling the daemon's Discord
front door is environment-only, so nothing about it lands in a layered,
printed, shareable `settings.json` — in particular not the bot token, which
follows the same Keychain path as the provider keys (see `.env.example` and
`THREAT_MODEL.md`, "Discord remote control").

| Variable | Meaning |
| --- | --- |
| `DISCORD_BOT_TOKEN` | The bot token. Prefer `make discord-key` (Keychain service `sierralab-discord-bot-token`) or export it; **not** a file value. Read once at startup, then removed from the process environment and masked everywhere. |
| `ALLOWED_USER_ID` | The one Discord user id allowed to chat, approve, deny or steer (Developer Mode → Copy ID). Required — with no allowlist the bot stays off. |
| `TELEMETRY_CHANNEL_ID` | The channel the bot announces in and takes new chats from; chats and task threads open under it. |
| `SIERRALAB_DISCORD` | `0`/`off`/`false`/`no` disables the bot even when the three above are set (kill switch, checked before the token is read). |
| `SIERRALAB_DISCORD_CWD` | The folder a new chat works in. Unset: your newest terminal/GUI session's folder, else `~/.sierralab/discord-workspace`. `!new <folder>` picks one per chat. |

All three of the first rows' names must be present and parse (the ids as
integers) for the bot to start; otherwise it logs the reason and is a no-op.
`GET /health` reports `discord: {enabled, connected, allowedUser, channel,
reason, threads, chats, liveChats}` — never the token. Message Content Intent must be enabled for the
application in the Discord Developer Portal, or `discord.PrivilegedIntentsRequired`
is logged as the actionable message it is rather than crashing the daemon. The bot also needs
*Send Messages*, *Create Public Threads*, *Send Messages in Threads*, *Add Reactions* and *Embed Links* in
the channel (without thread permission, the channel itself becomes the one chat).

Commands (plain text, anywhere the bot listens): `!status` · `!stop` · `!mode auto|build|plan` ·
`!new <folder>` · `!usage` · `!help`. Chats are persisted in `~/.sierralab/discord_chats.json` (0600, newest 40);
at most three agent processes run at once and an idle one is stopped after 20 minutes — the next reply in its
thread starts a new process and `session/load`s the conversation.

Tests: `backend/_discord_remote_smoke_test.py` — hermetic: a fake `discord` module, and the real `Supervisor`
driving `fixtures/fake_acp_chat_child.py`, a scripted agent that speaks the ACP wire.

## Checking the result

```sh
backend/.venv/bin/python backend/settings_store.py ~/some/project   # merged values, sources, rules, warnings
```

Tests: `backend/_settings_store_smoke_test.py` (no model), `backend/_acp_settings_smoke_test.py`
(real server and model: defaults, instructions, deny/allow, always-allow), `cargo test -p acp-tui`
(the Rust reader against the same fixture).

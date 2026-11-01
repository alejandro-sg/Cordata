# Threat Model

What Cordata defends against, where the boundaries are, and what it does not cover. Cordata is one developer's tool
that runs on a Mac, with no server: the only network traffic is to the model providers and the coding CLIs you configure.

## What is trusted

| | trusted? |
|---|---|
| The person at the keyboard | yes: decides every approval |
| The model's output | no: it can be steered by anything it reads |
| File contents, command output, web pages the agent reads | no: prompt-injection carriers |
| The project folder | partly: its files are data, its settings files cannot loosen what you set |
| The coding CLIs (`claude`, `agy`) | as far as their sandbox profile allows |

## Boundary 1: tool calls need a person

In the chat agent, `edit_file` and `run_command` ask before they run; `read_file`, `code_search` and `code_outline` do
not. "Always allow" lasts for one session and is per tool. A pending approval that is never answered is denied when its
sender drops. The pre-tool hook (`scripts/guard.sh` → `cordata-guard`) blocks, without asking, writes to `.env`, writes
inside `.git/`, recursive deletes of `/`, `~` or `..`, and shell redirects into `.env`.

Not covered: the permission-rule files (`permissions.allow`/`deny`) and the plan/edit/auto modes are not enforced by
the current engine.

## Boundary 2: commands run confined

`run_command` runs under a macOS Seatbelt profile (`sandbox-exec`) with a scrubbed environment: only `PATH`, `HOME`,
`USER`, `LOGNAME`, `SHELL`, `TMPDIR`, `TERM`, `LANG`, `TZ`, `LC_*` and colour variables are inherited, so a provider key
in the parent's environment never reaches a child. The profile denies reading or writing `.env` files and `~/.aws`,
`~/.netrc`, `~/.config/gh`, `~/.docker/config.json`, `~/.kube/config`. Fail closed: when `sandbox-exec` cannot be applied,
commands do not run. Tool output over 6,000 characters is written to `~/.cordata/offloaded/` and replaced by a summary.

Not covered: the profile otherwise allows everything, so a command you approve can reach any host and write anywhere you can.

## Boundary 3: file edits stay in the project

`edit_file` resolves the path, refuses anything outside the project folder and refuses secret files (`.env`,
`.env.*`, except `.env.example`/`.sample`/`.template`/`.dist`). An edit needs a unique target and shows its diff in the
approval card. `read_file` and `code_search` skip secret files.

## Provider API keys

Keys are read from the environment or the macOS Keychain at the moment a request is made. They are never written to a
file by Cordata, never put on a command line beyond the Keychain service name, and `make key` prompts with a hidden
input. Do not put a key in `.env` or a project file: the agent's tools can read those (the sandbox denies `.env`, but
other files are not covered). `cordata-secrets` (CI, `make secrets`, the pre-commit hook) refuses a commit that adds a
credential-shaped token, and `--history` scans every blob.

## The orchestrator (coding CLIs as workers)

`cordata run` hands work to `claude` and `agy`. Each step runs as a child under Seatbelt with:

- **a write scope**: the step's declared files only, plus tool caches (`.pytest_cache`, `__pycache__`); `.env` and
  credential directories unreadable;
- **a scrubbed environment** and no provider key;
- **egress only through the Agent Vault proxy** (`vault.enabled`); with no proxy the run is refused unless you pass
  `--allow-direct`, which you do knowingly;
- **checkpoints** before each step in a shadow git repository, and copy-on-write sandboxes for parallel waves, harvested
  only after the step's output is checked.

A worker's answer is parsed against a schema, verified by a different model family (HARDEN), then audited: tier 1 runs
the plan's acceptance commands under a profile that denies project writes and outbound network (a pattern guard refuses
destructive commands and redacts secrets in the output); tier 2 has the planner compare the result with its plan.
Research findings are kept only when their quoted evidence is found in the file they cite.

Not covered: a worker is a full coding agent. Inside its write scope it can write anything, including code you will later
run. Read the diff.

## Checkpoints are recovery, not a boundary

Shadow-git snapshots (`~/.cordata/checkpoints/<hash>/git`) let `/undo` and the orchestrator roll files back. They protect
against a bad edit, not against a hostile command: a command can also touch things a snapshot does not hold.

## Prompt injection

Anything the agent reads can contain instructions. The defences are structural, not textual: approvals for anything that
changes state, confined commands, no keys in reach, an egress gate for workers. The model is not asked to detect attacks.

## The native app

The app runs the same engine in its own process. Its preferences live in its own file. It stores no key; the Keychain
holds the one you set (`Settings → Account`, service `cordata-*-api-key`). Screen capture is not implemented.

## Known gaps

- Chat commands have no network or write restriction beyond the secret paths above.
- Permission rules and modes are not enforced by the native engine (see Boundary 1).
- Sessions are not persisted, so there is no session history to protect, or to clear.
- Laya Guard is gone; the orchestrator audit uses a conservative pattern guard.
- Seatbelt is macOS-only and `sandbox-exec` is deprecated by Apple; if it disappears, commands stop running (fail closed).

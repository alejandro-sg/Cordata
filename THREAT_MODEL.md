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
not. "Always allow" lasts for one session and is per tool; for `run_command` it never covers a call that asks for the
network, which asks on its own. A pending approval that is never answered is denied when its sender drops. A built-in guard
(`cordata-guard`) refuses, without asking, edits to `.env` files and inside `.git/`, recursive deletes of `/`, `~` or `..`,
and shell redirects into `.env`.

**Rules you write.** `chat.allow` lets named calls run without asking (`run_command(cargo test*)`, `edit_file(src/**)`);
`chat.deny` refuses named calls without asking. The order is guard → deny → allow → ask. Only `~/.cordata/settings.json`
may hold `chat.allow` (and `chat.writable`): a repository's own settings can add deny rules and switch the network or
history off, never grant its agent anything; such keys are ignored and reported. An allow rule never matches a command
that chains, pipes, redirects or substitutes (`; & | $ \` < > ( )`, backslash, newline), so `cargo test && curl … | sh`
asks even under `run_command(cargo test*)`. A deny rule is checked against every simple command inside the line (split at
operators, subshells, substitutions and quotes, with `env`/`sudo`/`then`-style prefixes, `VAR=…` and the binary's folder
dropped), but a shell can spell a command many ways, so deny rules are a convenience in front of the card and the
sandbox, not a boundary. File rules match the resolved, case-insensitive path. A user file that does not parse fails
closed: no allow rules, no network, no history. A call that ran under a rule says so in the transcript, and
`/permissions` lists what is in effect.

An allow rule for a toolchain runs code the repository controls: with `run_command(cargo test*)` and `edit_file(src/**)`,
the agent can write a test and run it without a card (`npm run *` and `make *` likewise). The sandbox still bounds what
that code can do; write such rules for projects you trust.

## Boundary 2: commands run confined

`run_command` runs under a macOS Seatbelt profile (`sandbox-exec`) with a scrubbed environment: only `PATH`, `HOME`,
`USER`, `LOGNAME`, `SHELL`, `TMPDIR`, `TERM`, `LANG`, `TZ`, `LC_*` and colour variables are inherited, so a provider key
in the parent's environment never reaches a child. The profile:

- **denies writes** everywhere except the project (and the repository it sits in, when it is a subfolder; never `$HOME`
  itself, even when it is a dotfiles repository), `/private/tmp`,
  `$TMPDIR`, `/dev`, package caches (`~/Library/Caches`, `~/.cache`, `~/.cargo/registry` and `git`, `~/.npm`, pnpm, yarn,
  Go modules, Gradle, Maven) and the absolute folders you list in `chat.writable`. Executable locations such as
  `~/.cargo/bin`, `~/.local/bin`, `~/.npm/_npx` or shell start-up files are not writable, so an approved command cannot
  plant something the next shell runs;
- **makes no direct connection out**: outbound sockets go only to localhost, and name lookups (mDNSResponder) are
  closed, so data cannot leave in a DNS query. A call sets `network: true` to go online; its approval card says "with
  network access", offers no "always", no rule covers it, and `chat.network: "off"` refuses it outright;
- **reaches only its own Unix sockets**, online or not: those under the folders it may write (a test's own server), never
  launchd's per-login sockets (ssh-agent) or any daemon elsewhere. Local daemons act outside the sandbox — Docker runs
  containers, herdr and other multiplexers start programs in their panes, Cordata.app's control socket opens threads — so
  reaching one would be a way out;
- **cannot change what git runs later**: `.git/config`, `.git/hooks/`, `.git/info/attributes` (and a submodule's) are
  read-only, so `core.fsmonitor`, `core.hooksPath` and filter drivers cannot be set from a command. Commits still work;
  `git init`, `git remote add` and `git config` inside the sandbox do not;
- denies reading or writing `.env` files, `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.netrc`, `~/.config/gh`, `~/.docker/config.json`,
  `~/.kube/config`, `~/.cordata` (offloaded output excepted) and the other credential files listed in `sandbox.rs`.

Fail closed: when `sandbox-exec` cannot be applied, commands do not run. Tool output over 6,000 characters is written
to `~/.cordata/offloaded/` and replaced by a summary.

Not covered: inside the project a command can write anything else, including code you will later run, and reads
outside the secret paths are allowed. "Offline" means no direct sockets: a command can still ask a local
service to reach out for it (`open https://…` hands a URL to your browser, a local model server or proxy on localhost
forwards what it is sent). A command you approve with network access can reach any host.

## Boundary 3: file edits stay in the project

`edit_file` resolves the path, refuses anything outside the project folder and refuses secret files (`.env`,
`.env.*`, except `.env.example`/`.sample`/`.template`/`.dist`) and `.git/`, judged on where the path lands after symlinks,
so `src/k -> ../.env` is refused like `.env`. An edit needs a unique target and shows its diff in the
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

A worker's answer is parsed against a schema, verified by a different model family (HARDEN), then audited. Before any
acceptance command runs, a reviewer model (another worker than the planner that wrote them, when one has quota) screens
them all, fenced as data, and scores each command's risk; at 0.8 or above the command is refused and its criterion fails.
The screen can only add refusals: a conservative pattern guard scores every command too and the higher score wins, so a
reviewer talked into passing `rm -rf ~` does not let it through. Tier 1 then runs the commands under a profile that denies
project writes and outbound network, with secrets redacted from the output; tier 2 has the planner compare the result
with its plan. When the screen is off (`orchestrator.screen`, `--no-screen`) or cannot answer, the pattern guard decides
alone and the run records a warning.
Research findings are kept only when their quoted evidence is found in the file they cite.

`~/.gemini` is denied to every worker. Only the `agy` worker's own profile reopens what that CLI needs — its state folder
read-write, `~/.gemini/config` read-only — and keeps its `bin/`, `builtin/`, `updater/`, `mcp/` and `settings.json`
unwritable, so no worker can plant code or an MCP server that agy later runs unsandboxed.

Not covered: a worker is a full coding agent. Inside its write scope it can write anything, including code you will later
run. Read the diff.

## MCP servers

A chat starts the servers in `~/.cordata/mcp.json`. A project's `.mcp.json` names programs to run on your Mac, so each
of its servers starts only as you approved it (Settings → MCP → "Approve", which shows the exact command and the names of
the variables it sets). The approval is `mcp.approved` in `~/.cordata/settings.json`: the folder, the server's name and a
SHA-256 of its command, arguments and environment values. A `git pull` or checkout that changes any of them holds the
server back again ("changed since you approved it"); a nested checkout or worktree is a different folder and asks on its
own; a repository's own settings cannot approve anything, and a project server never replaces one of yours with the same
name. Servers
start with an allowlisted environment (`PATH`, `HOME`, locale; no provider keys) plus their own `env` block, and stop
with the session. Every MCP tool call asks first, like an edit.

Not covered: a started server runs with your rights, outside the command sandbox.

## Tool hooks

A project's `.cordata/hooks.json` names shell commands the chat runs around tool calls, outside the sandbox. Like a
project MCP server it runs only as approved: an `mcp.approved` entry pins the folder and a SHA-256 of the file and of
every file a hook command names by path, so a change to any of them holds it back. Code those scripts reach indirectly
(a sourced file, a program on `PATH`) is not pinned. Each hook is killed after its timeout (default 30 s). Your own `~/.cordata/hooks.json` needs no approval. An approved
hook runs with your rights.

## Saved sessions

Each chat session is saved after every turn to `~/.cordata/sessions/<project hash>/<id>.json` so `/resume` (or
`acp-tui --resume [id]`) can continue it. The folder is `0700` and each file `0600`, written atomically. Credential-shaped
text (provider keys, tokens, bearer headers, private keys, `password = …`) is masked before it is written, in prompts,
replies, tool arguments and tool output alike, so a key pasted into the chat never lands on disk. Commands cannot read the
files: the sandbox denies `~/.cordata`. A project keeps its newest 50 sessions. `/forget confirm` deletes a project's
sessions, even ones saved before history was switched off, and stops saving every open session in the terminal client
(in Cordata.app, other windows keep saving until they close); `chat.history: "off"` (which a project may set) keeps
nothing new.

Not covered: masking is pattern-based, so a secret with no recognisable shape is stored as typed; the files are not
encrypted beyond FileVault.

## Checkpoints are recovery, not a boundary

Shadow-git snapshots (`~/.cordata/checkpoints/<hash>/git`) let the orchestrator roll files back. They protect
against a bad edit, not against a hostile command: a command can also touch things a snapshot does not hold.

## Prompt injection

Anything the agent reads can contain instructions. The defences are structural, not textual: approvals for anything that
changes state, confined commands, no keys in reach, an egress gate for workers. The model is not asked to detect attacks.

## The native app

The app runs the same engine in its own process. Its preferences live in its own file. It stores no key; the Keychain
holds the one you set (`Settings → Account`, service `cordata-*-api-key`). Screen capture is not implemented.

The menu bar item (`Settings → Show in the menu bar`, on by default) lets you answer an approval without the window.
It offers Allow only for a call the engine does not rate high risk; a high-risk call says "Review in Cordata…". A click
carries a hash of the call the menu showed and does nothing if the thread has since asked for a different one. The menu
shows the command line (credential-shaped spans masked, as in saved sessions), so anyone who can see your menu bar can
read it; turn the item off when sharing your screen. While it is up, closing the window hides the app instead of
ending it, so a turn in progress keeps running until you quit with ⌘Q.

## Known gaps

- Inside the project, an approved command can write anything, `.git/` included; read the diff.
- Offline commands can still reach the internet through a local service that does it for them (a browser via `open`).
- Command deny rules match spellings, not meaning; the approval card and the sandbox are the boundary.
- Session masking is pattern-based; a secret with no recognisable shape is saved as typed.
- The command screen is a model and can be wrong; the pattern guard and the no-network, no-write sandbox stay underneath it.
- Seatbelt is macOS-only and `sandbox-exec` is deprecated by Apple; if it disappears, commands stop running (fail closed).

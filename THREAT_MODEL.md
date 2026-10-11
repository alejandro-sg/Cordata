# Threat Model

This document states the trust boundary and the real gates behind it, so a
contributor (or anyone deciding whether to run this) doesn't have to
reconstruct that picture from scattered code comments first.

## Trust boundary

This is a **local-only, single-user agent harness with real host access** —
not a hardened multi-tenant service. There is no login, no accounts, no
per-user permission model. Whoever can reach the frontend origin *is* the
user, with the same capability the person running it has. This is
deliberate — the whole design (README's "no cloud calls, everything runs on
localhost") assumes one person, one machine or small home LAN, not a
service exposed to strangers.

The threat model does not try to stop the person running this from using
its own tools (`shell`, `file_write`, etc.) — that's the product. It does
try to stop:

- A request that skips the frontend entirely (a raw `curl`, another device
  on the LAN) from reaching a tool that's supposed to require a human's
  approval click.
- Content the model reads from outside the conversation — a fetched web
  page, a recalled memory that itself originated from one — being treated
  as an instruction instead of data.
- A model-generated tool (`skill_forge_tool.py`'s output under
  `backend/skills_generated/`) running before a human has actually looked
  at it, twice over.
- A sub-agent (`local_swarm`/`local_graph`) reaching a capability with no
  human-in-the-loop gate of its own, since sub-agents have no approval UI
  to pause into at all.
- An external process this harness runs on the model's behalf — the Claude
  Code CLI (`claude_code_tool.py`) — egressing to the network without passing
  the Agent Vault, or reading a credential file while it does.

New surfaces added with the Meta-Harness (`docs/meta-harness.md`) — the gRPC telemetry bridge and
the sandboxed Claude Code child — are analyzed under "The orchestrator daemon" and "Harness
autonomy" below, and their accepted residuals are in "Known gaps".

## Human-in-the-loop gates

Both front doors gate the same four tools before they run for real —
`shell`, `python_repl`, `file_write`, `editor` — plus any tool loaded from
`backend/skills_generated/`, by name, dynamically:

- **AG-UI (web)** — `main.py`'s `ToolBehavior(interrupt_on_call=True)`,
  implemented by `ag_ui_strands`'s `StrandsInterruptHook`. The call pauses
  server-side; the frontend renders an Approve/Deny card
  (`frontend/app/AgentTools.tsx`) and the tool only runs once a human clicks
  Approve. Verified live, repeatedly, that this cannot be bypassed by a
  denied click, a stale approval, or a tool call racing ahead of the card.
- **ACP (editor)** — `acp_server.py`'s own `_ApprovalHandler`, wired through
  Strands' `interventions` system, surfaced as a native Zed permission
  prompt. Same four tools, same "the call genuinely does not execute until
  approved" property, independent implementation from the AG-UI path (no
  shared trust between the two front doors — each enforces its own). The
  client's answer only approves if the selected `option_id` maps to an
  allow-kind option the server offered (`_permission_approved`); a selected
  "Reject", an unknown id, or a cancelled outcome all deny.

**Every ACP approval is recorded on the ledger, and a recorded one can be
replayed.** `_resolve_interrupts` stages the pending approval in
`~/.sierralab/state.db` (`interventions`, keyed by the Strands interrupt id)
before the user is asked, and closes it with what they chose; both transitions
are also appended to the append-only `events` table, with Laya's risk score and
band. Two properties matter for this document:

- **The ledger is evidence, not authority.** `_ApprovalHandler` never consults
  it to decide whether a call may run. The one thing read back is a decision
  this harness itself wrote, and only for the *same* call in the *same*
  session and the *same turn* (`_settled_intervention`; Strands derives the
  interrupt id from the tool-use id, which another session's model could in
  principle mint as well — and a local server that numbers its calls reuses an
  id every turn, which is why a decision from an earlier turn is never
  replayed). A row that is unreadable, still staged, stale, or whose tool,
  input or session disagrees re-prompts. Replay exists because `_stream` reports a turn's
  interrupts from two places, and without it the same call would be put to the
  user twice in one turn. A turn restarted after a crash re-issues a *new*
  tool call, so it is asked again rather than inheriting an old answer.
- **An unattended prompt is a denial, attributed as such.** With no client
  attached the outcome is recorded as `decided_by: "no-client"` rather than as
  a user decision, so the ledger never credits a fail-closed default to a
  person. Nothing approves a call that nobody saw.

It lives in the user's private harness directory (`~/.sierralab/`, forced to
0700, with `state.db` and its `-wal`/`-shm` at 0600 — `state_store._make_private`;
a default umask would leave both world-readable) rather than beside the source,
because a row records the tool input and the settings snapshot a session ran
with.

**Generated tools get a second, independent gate on top**, not just the
`file_write` approval that put them on disk:
`skill_validation_hooks.py`'s `SkillValidationHooks` AST-parses any
`skills_generated/*.py` tool for a narrow banned list (`subprocess` in any
form, `eval`/`exec`, `os.system`) and dry-runs it with synthetic arguments
before its first real call is allowed to proceed — a human approving the
*file write* is not the same as a human having verified the *tool's
behavior*, and this closes that gap. Trust is in-memory only and resets on
restart, on purpose — `skills_generated/` is an audit trail, not an
unconditional allowlist.

**Sub-agents get none of the above** — `local_swarm`/`local_graph` build
plain `strands.Agent` instances directly in Python
(`sub_agent_common.py`), with no AG-UI/ACP transport wrapping them and
nowhere to pause for a human. `sub_agent_common.DENIED_TOOLS` (`shell`,
`python_repl`, `file_write`, `editor`) and `ORCHESTRATOR_ONLY_TOOLS`
(`local_swarm`, `local_graph`, `skill_forge`, `proposer`, the
model/sandbox-mode control tools, `open_game`) are refused to every
sub-agent regardless of what a spec asks for, or omits — the default
(`"tools"` unset) hands a sub-agent every tool *not* on either list rather
than erring toward "give it everything." `resolve_tools` also excludes
every tool `ag_ui_strands.client_proxy_tool.registered_proxy_names()`
reports (`toggle_theme`, `get_game_state`, `apply_game_move`, and any
future `useFrontendTool`) — these get proxied into the top-level agent's
own tool registry, and their proxy body unconditionally returns a fake
`"success"` with no real effect; a sub-agent given one wouldn't just lack a
capability, it would silently report doing something that never happened.

### Settings files, permission rules and project trust (ACP front door)

`settings_store.py` layers `~/.sierralab/settings.json`, `<project>/.sierralab/settings.json` and
`settings.local.json` over the approval gate (`docs/settings.md`). The new attack surface is that
the project files arrive with a cloned repository:

- **A stranger's repo cannot loosen the gate.** Project and local `allow` rules, and
  `permissions.defaultMode: "auto"`, are ignored until the folder is in the *user* file's
  `trustedProjects` (`/trust`, or an explicit "Always allow"). `deny` and `ask` rules only restrict,
  so they apply regardless. `/permissions` and the connect banner show what was ignored.
- **`deny` is checked first, in every mode** (`auto` included), and an `ask` rule can force a prompt
  on a tool that wouldn't normally ask. Shell commands are split on `; && || | &` and newlines
  before matching, and a wildcard `allow` never applies to a command with `$( )`, backticks or a
  redirect — so `allow shell(git *)` does not approve `git status && rm -rf ~`. Built-in denies
  refuse reading a credential out of the macOS Keychain from the agent's shell.
- **No credentials in settings.** A key named like a credential, or a value shaped like one, is
  refused on read and on write; provider, URL and key stay environment/Keychain configuration. The
  files are written atomically; the user file is `0600`; a file that fails to parse is never
  overwritten.
- **"Always allow" is an explicit decision about a folder.** It saves the exact command (never a
  wildcard, never for `python_repl`) to `settings.local.json` and marks the folder trusted — which
  also activates any `allow` rules a cloned repo already ships in that folder. Review them with
  `/permissions`.

- **A stranger's repo cannot lower Laya's lines either** (Phase 11 audit). `laya.callGuard`,
  `laya.riskDestroy`, `laya.riskSafe` and `laya.mode` used to be settable from an untrusted
  project file: `callGuard: off` or `riskDestroy: 1.0` removed every per-call ask that bounds
  `auto`, and a prompt-injected agent that wrote that file (a 0.4-scored shell redirect, see
  below) could do it for the next session. They are trust-gated now, with `preflight.*`,
  `learning.rejections` and `context.outlineLines` (`TRUST_GATED_KEYS`; the TUI's mirror is held to
  it by `_settings_schema_parity_smoke_test.py`).
- **A shell write to a policy file is scored like `file_write` to it.** `printf … > .sierralab/settings.json`,
  `tee` / `cp` / `mv` / `sed -i` into a settings file or any MCP declaration (`.mcp.json`, `mcp.json`,
  `mcp.local.json`, `.cursor/` / `.vscode/mcp.json`, `backend/mcp_servers.json`) scores 0.9 and asks in
  every mode; it used to score 0.4 ("overwrites a file") and run in `auto`.

**What this does not do.** Rules are not a sandbox: a pattern can't anticipate everything an
allowed program can do (`allow shell(npm *)` includes `npm exec`). Instruction files
(`SIERRALAB.md`, and — as a fallback — a repo's own `AGENTS.md` / `CLAUDE.md`) are read into the
system prompt from folders that have *not* been trusted, because they cannot change the gate; a
hostile one can still steer the model toward a request you then have to refuse, so the connect
banner names the files being read and `/memory` lists them. Trust is per-folder-path, not
per-content: a trusted folder whose settings later change (a `git pull`) keeps its trust.

## Harness autonomy: the new surfaces

The agentic harness layer (`docs/agentic-harness.md`) added code that runs without a new user
approval on every individual call. Four surfaces, each with its own containment:

- **Project MCP servers (`backend/mcp-servers/`, `plugin_host.py`)**. The config file a project
  ships (`.mcp.json`, `.sierralab/mcp.json` / `mcp.local.json`, plus the backend's own list)
  tells the *backend itself* which MCP servers to spawn — this is deliberately not behind the
  per-call approval gate, because starting a plugin is a launch-time action, not a tool call. The
  mitigations that make this survivable: a project's own plugin declarations are auto-started
  **only when the folder is trusted** — the same `trustedProjects` check the settings files use,
  the same principle (a stranger's repo cannot activate anything of its own); plugins this
  backend or the user's own file declared start regardless, but those are the user's choices.
  Plugin tool *calls* always land in `APPROVAL_REQUIRED_TOOLS` / `MCP_TOOL_NAMES`, so they
  prompt like any gated call whatever the source; and a confident Laya route only *narrows* the
  visible toolset to one plugin — it can never widen it. The residual risk to know: the
  auto-started server's command runs with the backend user's full privileges at session start,
  same as any dev tooling the user runs on the project.
- **Language servers running locally (`lsp_bridge.py`)**. The LSP manager spawns whatever the
  project type implies (a Rust server for Rust files, etc.) at session start, also outside the
  per-call gate. A language server executes code paths of its own; an attacked one is an
  interpreter for its project's files. Contained the same way as plugin servers: nothing
  *sends* a command from the model through it (the LSP tools are read-only queries); the edit
  hooks only *read* diagnostics back. Off with `lsp.enabled: false`. The residual risk is the
  standard one for running dev infrastructure on unreviewed code — treat a cloned repo you don't
  trust like you'd treat opening it in a full IDE: the servers are running on your machine.
- **The throwaway project copies (`self_heal.py`'s `ShadowWorkspace`)**. Self-heal verification
  copies tracked + untracked-not-ignored files (and symlinks heavy dirs) into a temp directory
  and applies candidate patches *there*. The copy is a full copy of real project files — secrets
  and all (`.venv` is symlinked, but `.env`-style files the project carries are copied). It
  lives in the user's own temp dir (`$TMPDIR`), cleaned up on exit; commands run there with the
  session's own user, exactly as they would in the real tree (`NotASandboxLocalEnvironment` —
  by design, it is not a sandbox). Attack surface to know about: the temp copy briefly holds
  project data outside the project (on the same machine, same user); the stale-`.pyc` trap is
  handled (caches are excluded from the copy and `apply_patch` invalidates the patched module's
  `.pyc` — a verification pass can't silently run pre-patch byte-code).
- **`selfHeal.autoApply`**. Off by default, and pre-approval is deliberately narrow: it may
  auto-approve exactly (a) an `apply_patch` whose text *equals* the sandbox-verified patch, and
  (b) a re-run of the exact command that already failed once. Every other tool still prompts;
  `deny` rules still win; plan mode still blocks everything; the sandbox verification step is
  not skippable for a pre-approved patch. What this buys the attacker model: an adversary who
  controls the failing *output* (e.g. a repo whose build failure text is prompt-injected) can
  shape the diagnosis, but the patch still has to pass the sandbox-verified re-run of the
  original command before it lands, and the patch application is still visible in the TUI as a
  diff. The residual risk is accepted explicitly once, when the flag is turned on in a trusted
  folder — it is not a per-turn decision.

## Network-edge gate: SANDBOX_API_TOKEN

`main.py` binds `0.0.0.0`, not `127.0.0.1` — this deployment is meant to
support frontend and backend running on separate LAN machines, which means
"only reachable from localhost" was never actually true for
`/sandbox/*` (`sandbox_functions_api.py` — the `openGenerativeUI` bridge
functions) or `/games/*` (`games_static_api.py` — served game files).
Anything on the same LAN could call `runSnippet` (arbitrary Python)
directly and skip the frontend's client-side approval card entirely — the
card is real UI state, but on its own it was never a network-level gate.

`SANDBOX_API_TOKEN` closes that: every request to either route family must
carry the matching `X-Sandbox-Token` header. It is **never unset in
practice** — `settings.py` auto-generates a fresh `secrets.token_hex(32)`
at process start whenever it isn't explicitly configured, so a checkout
with zero setup is still gated, not silently open. The frontend discovers
that auto-generated value itself, at runtime, via `GET
/sandbox/_local-token` — a route gated not by the token (asking for it
implies not having it yet) but by `_require_localhost`, which only answers
a caller whose own connection originates from `127.0.0.1`/`::1`. A
same-machine caller (the Next.js server process, in the common
single-machine setup) could already read `backend/.env` directly if it
wanted to, so hopping to `_local-token` instead of a shared file gives it
nothing new; a different machine on the LAN — exactly who this token
exists to keep out — gets a plain 403 with no token revealed, same as
every other gated route. Set `SANDBOX_API_TOKEN` explicitly instead once
frontend and backend really are on separate hosts; auto-discovery cannot
cross that hop by design.

## The orchestrator daemon (loopback WebSocket)

`backend/daemon/` listens on `127.0.0.1` only — the bind address is a constant,
not a setting, because this process spawns shells in arbitrary project
directories, so reachability from a network is categorically wrong rather than a
tunable. Three properties hold it:

* **Loopback is not an authorization boundary.** Any local process — and any
  page in a local browser — can open a connection to it. So every route and the
  socket require the token in `~/.sierralab/daemon.json`, written mode 0600 in
  the user's private harness directory. The token is deliberately *not* in
  `settings.json`: that file is layered, printed by `/config` and shareable.
  It is read from the file rather than passed as a flag, because `argv` is
  visible to every process on the machine.
* **No browser origins.** A WebSocket handshake carries an `Origin` header a
  script cannot forge, and any request bearing one is refused (close 4403).
  This is what makes a query-parameter token acceptable — the browser
  `WebSocket` constructor cannot set headers, so the token has to travel in the
  URL, and the Origin check is what stops a malicious page from using it.
* **One client per session.** A second attach is refused (4409). Two clients on
  one stdin would interleave answers to prompts only one of them can see.

The daemon **proxies frames verbatim** — it reads only the session id and the
method name from a line, never decoding and re-encoding a message. The security
consequence is the point: the daemon cannot inspect a tool call or rewrite a
permission prompt, so the approval gate, the Laya routing and (below) the egress
policy all stay in the child, where they already are. A daemon that could
rewrite a frame would be a place where a compromised daemon silently weakens
every gate at once.

Sessions outlive their client: a child parked on an approval prompt stays parked,
and a reconnecting client is replayed the prompt from the daemon's backlog. The
backlog holds only output no client has been handed yet — a frame a client already
received is never replayed to the next one, which would re-show answered prompts.
That is a durability property with a disclosure edge — a client attaching to a
directory that already has a live session receives that session's buffered
output, including tool results. It is gated by the same token, and the backlog
is bounded (`Supervisor.BACKLOG_LIMIT`), so it is not a growing store.

### The Experimental Lab: what its controls are, and are not

`backend/lab.py` exposes a live view of every agent and five controls
(`_sierralab/lab/{select,isolate,release,dial,inject}`, ACP extension requests on the
session's own connection — there is no REST verb for any of them).

* **E-Stop is a refusal, not a process kill.** The pods are Strands agents inside one
  `acp_server.py` process; there is no OS process per agent to kill, and the vault proxy
  holds credential *placeholders* per process, not a ticket per agent, so there is nothing
  to revoke. `isolate` marks the pod (and everything it spawned) isolated, `agent.cancel()`s
  it if it is mid-flight, and its hooks then refuse every model call and every tool call.
  That stops the loop at the next boundary and cannot be argued with by a prompt; it does
  **not** interrupt a tool already executing in the OS (a running `shell` command finishes —
  reject it at the station instead), and it does not make an *approved* call unhappen.
* **The Laya dial only tightens.** A pod's dial arms the approval gate (asks even in `auto`
  mode) when the turn's score exceeds it. It can be lowered to 0.30 and cannot be raised
  above the router's own 0.8 gate, which stays authoritative — a control that could
  loosen the gate would be a way to switch off a safety check from a keypress. The score is
  Laya's reading of the *user's message* for the turn, not of the specific call
  (`laya_risk.py`), and that is what the gauge shows; treat it as context for a human
  deciding, never as a clearance.
* **Guidance is attributed and outranks nothing it should not.** Injected text is merged
  into the model's next request as `[operator directive — …]`. It is operator input (the
  person running the session), not tool output, so it is *not* wrapped as untrusted; it
  cannot widen a permission rule or skip an approval — those are enforced in code, not in
  the prompt.
* **Every control is in the ledger** (`state.db` events, kind `lab`) before it takes effect.
* **Nothing reaches a snapshot unmasked.** Stream text, tool inputs, system prompts, memory
  snippets and guidance pass through `secret_guard.redact` first.
* **The daemon's `/lab` and `/lab/ws` are read-only** and follow the daemon's rules
  (loopback, token, no browser origins — so a browser page cannot be the canvas). The
  snapshots include tool inputs and approval prompts, which is why they carry the same gate
  as the session socket. A canvas cannot approve, isolate or inject: controls travel only on
  the session's own connection, where the driving client already holds them.
* **Resource numbers are process-wide** (CPU %, peak RSS): agents share a process, so a
  per-pod figure would be a guess.

### The gRPC telemetry bridge (the native Lab)

`backend/daemon/grpc_bridge.py` is a second daemon front door, added for the native GPUI window
(`docs/meta-harness.md`). It relays and reports; it decides nothing. What holds it:

* **Loopback-only, token-gated, no TLS.** The bind is a constant (`127.0.0.1`), not a setting, for
  the same reason as the WebSocket: this process answers questions about shells running in
  arbitrary projects. Every RPC requires the `x-daemon-token` metadata — the same 0600
  `daemon.json` secret — compared in constant time. There is no TLS because there is no network:
  a loopback socket that never leaves the machine would only be paying for a certificate, and the
  token, not the transport, is the boundary. The one thing a native window does *not* inherit is
  the WebSocket's `Origin` check; that check exists to defeat a browser page, and a GUI is not
  one — but it is precisely why the token is required here rather than relying on loopback.
* **It cannot make a decision the human didn't.** An approval forwards the answer into the paused
  ACP request, and the options it may send are the ones the *child offered* — the bridge cannot
  invent a choice. A knob call is the child's own `_sierralab/lab/*` control invoked on the user's
  behalf; the unload is the same `memory_budget.unload_all` the agent's tools call. Nothing here
  can widen a permission rule or skip a gate.
* **Two front doors, first answer wins** (`Supervisor._admit_client_frame`). A TUI keypress and a
  GUI click cannot both reach the child: the first is admitted, the second is told it lost. Without
  this, two clients on one blocked prompt would each think they resolved it.
* **The bridge only ever tightens a gate.** It cannot be asked for a looser one. The dial is clamped
  in the bridge as well as in the child — a scripted client need not run the GPUI view's own clamp —
  and the clamp is the child's own range, so the most a client can ask for is "no dial", never a gate
  above the router's. The thresholds are mirrored rather than imported (the daemon is stdlib-only;
  `lab`/`laya_risk` reach the agent stack) and CI asserts the mirror equals the originals, because a
  drifted `DIAL_DEFAULT` would be exactly the loosening this rule forbids. Likewise a request the
  child left unlabelled at or above the destructive gate is shown **armed** with a derived band: the
  disagreement is one-directional — it can make a human look, never stop one looking.
* **A refused control is not a dead bridge.** `SetKnob` reports a bad argument as `KnobReply.ok=false`
  with the reason in the body, which is what the Rust client renders inline; a gRPC `Status` is how it
  learns the daemon is unreachable. Keeping them distinct is why a mistyped pod shows a refusal rather
  than turning the window read-only.
* **An intervention id is namespaced by session** (`<session_id>/<child id>`); the bridge reads only
  the prefix and lets the supervisor resolve the rest, so one session's approval cannot be applied to
  another's pending request.
* **Every RPC is bounded**, because a client is the untrusted side of this wire: telemetry is
  latest-wins per subscriber, the intervention fan-out queue is capped, `ProactiveIngest` is capped
  and deduplicated on content hash, snippet and diff bodies have size limits, and gRPC messages are
  capped at 16 MiB. Ingest is fed by the Rust observer through `try_send` — it drops and counts
  rather than blocking a UI thread, so a hostile or merely broken workspace cannot stall the app.
* **Everything is audited** (`state.db`, kind `gui`) before it takes effect: who approved a
  destructive command, who unloaded the models. The approval record is the one a post-mortem needs.
* **The diff body is masked at three layers** (child, daemon, client) before the modal renders it —
  the same `secret_guard` chain the Lab view uses, so the third party to touch it adds no trust.
* **A stale approval cannot be answered.** A request whose originating connection is gone is shown
  but not approvable, because the agent may already have been resumed by the other client — the
  window does not get to act on a picture of the past. See "Known gaps".
* **`SubmitTurn` is the one RPC that makes the bridge a participant.** Every other RPC relays or
  reports; this one *starts* something — a turn from the menu-bar deck, carrying the screen context
  the person attached. It is bounded the way the rest are: token-gated, it decides nothing about the
  turn itself (a daemon-owned conversation gets a real turn through `Chats.send`, the primitive
  Discord uses; a session another client drives only gets text *guidance* through the existing
  steer path, because the daemon never issued its `session/new` and a second turn would be a second
  writer to one stdin), and it accepts and returns rather than holding a turn that may run for hours.
  The context is re-checked here before it is forwarded — see "OS context capture" below.

### OS context capture (the menu-bar deck)

`lab/crates/harness-menubar` reads what the person is looking at — the frontmost app, its window
title, the selected text, optionally a screenshot of one window — and sends it with a prompt. Screen
content is the most sensitive input this harness has, and the requirement is the person's own:
*"I should be able to dictate what the agent can see. I can't have it view my secrets."*

* **Deny by default, per app.** `capture.enabled` is off and `capture.allowlist` empty on a fresh
  install, and an empty allowlist reads nothing. An app is added only by the person — the deck's
  "Always allow <app>" button, or their own settings file. Bundle-id-shaped values only (no names,
  paths or patterns), checked in Rust, Python and the TUI alike.
* **User scope only.** `capture.*` is read by the menubar from the *user* settings file; a project's
  `.sierralab/settings.json` that sets it is ignored with a warning even when the project is trusted —
  a cloned repository must never grant itself the ability to read windows. The keys are also
  trust-gated in `settings_store.py` and the TUI.
* **A never-read list no setting can lift** — password managers, Keychain Access, Passwords — matched
  on bundle id, case-insensitively, including helpers under an entry (`me.proton.pass.electron`). It
  is a constant in `harness_client::policy`, mirrored in `backend/os_context.py`, and a test holds the
  two equal. Rust refuses to read such an app; the daemon refuses to forward one (`SubmitTurn` →
  refused and audited), so a scripted gRPC client that skips the Rust policy gets nowhere.
* **Never in the background.** Nothing is read until the person clicks Capture (or presses `c`) in the
  deck; summoning the deck only records *which* app was frontmost, which costs no permission. Secure
  event input (a password field focused anywhere) and a focused secure text field each mean the app is
  named and nothing else is read.
* **Masked before it leaves the process, previewed before it is sent.** Titles and selections go
  through `harness_client::redact::scrub` (the patterns `backend/secret_patterns.py` is tested
  against) in Rust, so the checklist shows exactly what would be sent; the daemon masks again before
  rendering. Partial success is shown per field with its reason (`✗ selected text — Accessibility is
  not granted`), never as a blank.
* **Wrapped as data.** The context reaches the model through a `ContextInjector` for one turn, with the
  selection inside `prompt_security.wrap_untrusted` guard markers — a web page's selection can say
  "ignore your instructions" — and it is cleared when the turn ends.
* **Screenshots are narrower still.** One window (never the screen), only for an allowed app, never
  under secure input, at most 2048 px on the long side and 4 MiB of JPEG, captured with
  ScreenCaptureKit under its own Screen Recording permission. Pixels cannot be masked, so their
  defences are policy: a screenshot reaches a model only when `capture.visionModel` names one, a
  cloud one only with `capture.screenshotCloud`, only on the Ollama provider (where the tag says
  whether it is local) and never by pulling weights; the turn runs on that model once, the session's
  model is put back, and the image is stripped from the history and the saved snapshot when the turn
  ends. Mid-turn checkpoints are off for that turn so no copy is left on disk. Guidance to a session
  another client drives is text only.
* **Audited by size, never by content.** `submit_turn` rows record the app, which fields were present
  and how long, the redaction count, the screenshot's byte count and which model (if any) received
  it. The daemon's log line carries a short masked excerpt so a person can confirm what was sent.
* **The OS is the real boundary.** Accessibility and Screen Recording are TCC grants the person makes
  in System Settings; the deck only ever asks when its Grant button is clicked. A bundled, signed
  build (`make lab-menubar-app`) is what keeps those grants attached to this app rather than to a
  terminal.

### The Claude Code CLI runs as a sandboxed child, egress only through the vault

`backend/claude_code_tool.py` runs the official CLI headless as a subprocess. Four properties are
the whole containment, and each is enforced by the tool, not asked of the CLI:

* **Its CLI is driven by an allowlist and a denylist, not by prompts.** `--permission-prompts none`
  means anything not allowed is *denied*, not asked — a task cannot stall on a dialog and cannot
  argue its way to a permission. `DENIED_TOOLS` names the network paths a proxy cannot see into
  (`WebFetch`, `WebSearch`, `curl`, `wget`, `nc`, `ssh`, `scp`) and the credential files
  (`**/.env`, `**/.env.*` on read, edit and write) so the CLI says no before the kernel has to.
* **Its environment is an allowlist, and on macOS it is inside a Seatbelt profile.** The child is
  spawned with a scrubbed, allowlisted environment (`scrubbed_sandbox`) — not the parent's — so no
  provider key, vault placeholder or daemon token is in it. Seatbelt additionally makes `.env`
  files unreadable to it at the kernel, whatever the CLI's own tools or a script it writes try.
  The residual: an *approved* shell call can still read anything else this user can (gap 1 below).
* **Egress is through the Agent Vault or not at all.** `HTTPS_PROXY` (and the CA bundle variables)
  point at the vault proxy, and with the vault off or unreachable the tool **refuses** — the same
  fail-closed posture as the `shell` gate. There is no setting that lets this CLI reach the network
  directly. What that buys: the proxy holds *placeholders*, so a credential the CLI managed to read
  still isn't one it can use, and the substitution happens in the proxy.
* **The LanceDB priming is untrusted data.** Retrieved traces are log text — past commands,
  outcomes, errors — wrapped as data, never instructions, and redacted before they enter the
  prompt. An attacker who can write a log line can write text the CLI's model reads; that is the
  same injection surface as the observer's evidence, and it does not widen the tool's own rules.

### The background observer writes nothing

The observer (`daemon/observer.py`) is the only thing here that acts with no
user turn behind it, and it is off by default (`observer.enabled`). Its
zero-write property is structural, not a prompt instruction — an observer that
is *asked* not to write is one injection away from writing:

* The agent is built with `OBSERVER_TOOLS` (`file_read`, `think`) and nothing
  else, the list is asserted disjoint from `sub_agent_common`'s denylists **at
  import** (adding `shell` there fails loudly rather than widening silently),
  and a `BeforeToolCallEvent` hook cancels any call outside the allowlist even
  if a tool reached the registry another way.
* The read-only commands are run by the *daemon*, with a fixed `git` argv list
  (`status`, `diff --stat`, `log`, `diff`) and `GIT_OPTIONAL_LOCKS=0` — the
  latter is what makes "changed nothing" true for a git command, since
  `git status` otherwise rewrites `.git/index`. The model never chooses a
  command, which is also why `shell` is absent: it does not need one.
* The briefing is written by the daemon, in its own process, from the text the
  child prints on stdout. There is no code path from the observer agent to the
  filesystem.
* The agent runs as a **child process** (`observer_runner.py`), both because
  `agent_common` chdirs at import and because preemption has to be a process
  kill: a client attaching mid-cycle kills the child's process group rather
  than asking it to stop.

What this does not do: the evidence it reads — `git log` messages, diffs, log
files, and any text posted to `POST /observe` — is attacker-influenceable in the
sense that anyone who can write a commit message or a log line can write text a
model will read. That text can steer a *proposal*. It cannot make the observer
write, run or fetch anything, and proposals are staged with status `proposed`
and never applied by anything: they are advice in a file, not an action. A
webhook body is rendered into the briefing's evidence block verbatim and
attributed, never promoted into the model's prose.

Disclosure: the briefing retains whatever the working tree and logs contained at
the time, so it lives in the private user directory (`~/.sierralab/`), not in a
project. `POST /observe` takes its token as a header rather than a query
parameter, so a cross-origin page cannot reach it (a custom header needs a
preflight this app does not answer) — and it is a *source*, not a trigger: text
queued there is read by the next cycle, and the response says whether the
observer is even on rather than implying anything was acted on.

### Discord remote control (outbound only)

The daemon can carry the approvals conversation to Discord (`daemon/discord_bot.py`) so an unattended run is
not parked forever on a prompt nobody can see, and — since it is the one place a person is reliably
reachable — lets that person *talk to an agent* from it (`daemon/chat.py`). The direction is what makes it
tolerable:

* **Outbound WebSocket only.** discord.py opens a client connection to Discord's
  gateway. The daemon listens on no new port, registers no webhook, and exposes
  no callback URL. The loopback socket above stays the only inbound surface —
  this adds a second *way out*, not a second way in, and the network-edge
  reasoning for `bind 127.0.0.1` is untouched.
* **Fail-closed, twice.** The bot is off unless `DISCORD_BOT_TOKEN`,
  `ALLOWED_USER_ID` and `TELEMETRY_CHANNEL_ID` are all present and parse — a
  missing allowlist disables it rather than opening it to everyone, and
  `SIERRALAB_DISCORD=0` is a kill switch checked before the token is even read.
  An unparseable id disables the bot with a logged reason and never raises into
  daemon startup.
* **Zero-trust click and message.** Button callbacks (`_click`) and `on_message` check the author against
  `ALLOWED_USER_ID` as their **first** statement and return before touching any state; anyone else's press is
  a no-op that gets an ephemeral refusal, and anyone else's message is dropped in silence (a reply would tell a
  stranger the bot is listening). A click is not a credential: the id is compared to a configured operator, and
  Discord's own interaction signing is just what delivers it. Approving relays through
  `Supervisor.answer_permission` — the *same* arbitration the GUI and TUI use, so the second front door to
  answer loses harmlessly (`_settle`; a late terminal answer is dropped whichever door settled first), and the
  decision is attributed (`decided_by="discord"`) in `state.db` and the `_sierralab/resolved` frame.
* **A chat is an agent the operator can start — the new capability, and what bounds it.** A message from the
  operator opens a daemon-owned `acp_server.py` child (`Session.owner`). It is the *same* agent with the *same*
  gate: every shell/edit approval, every Laya-armed destructive call (in every mode, `auto` included), the vault
  egress rule and the checkpoints apply unchanged, and the approval prompts reach Discord and nowhere else —
  so a compromised Discord operator account can start agents and approve what they ask, exactly as a compromised
  GUI session could, and no more. Bounds: at most three such processes at once (`chat.MAX_LIVE`; a fourth first stops the least-recently-used *idle* one, and is
  refused with a message — not queued — only while all three are mid-turn), an idle one is stopped after 20 minutes, the conversation store is
  `~/.sierralab/discord_chats.json` mode 0600 holding ids, folders, token counts and a short title (the first 80 characters of the opening message, secret-masked — never the conversation, which lives in the agent's own session files), a chat starts in
  the folder named by `SIERRALAB_DISCORD_CWD` / your newest session / a private workspace and **says which in its
  first line along with its mode**, and `!mode` can only pick among the three modes the TUI offers. An owned
  session is invisible to a terminal's directory lookup and refused by the socket (`4409`): one writer per stdin.
* **Offloaded tool output is a new place data rests — and what keeps it harmless.** A tool result over
  `context.toolOutputChars` is written whole to `~/.sierralab/offloaded/<session>/` (`context_offload.py`, Strands'
  `ContextOffloader`) so the model can read a slice back instead of re-running a command. It can hold anything
  a tool printed, so: the directory is `0700` and each file `0600`; the write is on the host, never through the
  agent's sandbox, and registered credentials are masked *before* it (the plugin runs ahead of `SecretRedactionHooks`
  on `AfterToolCallEvent`, so it masks for itself); files are deleted after seven days; and the read-back
  (`retrieve_offloaded_content`) is wrapped in the untrusted-data guard, because the stored copy predates that
  wrap and a pattern slice may not include the lines the guard sat around. The model supplies only a reference
  (a tool-call id the SDK sanitises: no `..`, no backslash, one namespace per session) — not a path. The same
  secrets the session files already hold are all it adds; `context.offload: false` stops it.
* **The agent's shell and editor are the SDK's vended tools, on the host — not a new boundary.** `core_tools.py` swaps the
  deprecated `strands_tools.shell` / `editor` for `strands.vended_tools.shell` / `file_editor`, routed through
  `HarnessSandbox` (the SDK's `NotASandboxLocalEnvironment` — `sh -c` on the host with the full environment, exactly the
  authority the old tool had). Nothing is added or removed on the gate: the names (`shell`, `editor`) and parameters that
  the approval handler, Laya Guard, the settings rules, checkpoints and the diff preview key on are unchanged, and a hook
  ordered ahead of them only (a) defaults the timeout, (b) resolves a relative editor path to an absolute one *before*
  the gate sees it, so a path rule is matched against the real path, and (c) turns the SDK's JSON result into text with an
  exit code. A cancelled turn now kills the running command's process group instead of abandoning it.
* **The web tools are the SDK's, and keep the same fences.** `http_request` and `web_fetch` (`core_tools.py`) replace the
  deprecated `strands_tools.http_request`. Both are in `vault_proxy.EGRESS_TOOLS`, so an unreachable vault refuses them
  (fail closed); both results pass through the untrusted-data wrap; and the HTTP client is built *per call* so the vault's
  proxy and CA environment, which can appear after a session began, is always the one in force. A 4xx/5xx body — text
  from outside that the SDK drops and the harness now returns — is fenced before the model reads it. `web_fetch` adds a
  model reading a whole page: an analyst with **no tools**, so an injection in the page can colour its answer but cannot
  act, and the answer is fenced as derived-from-untrusted. What goes: the old tool's auth modes (the model never held
  credentials; the secret guard still blocks a registered one in a request's input), and `calculator`, `think` and
  `current_time` (a model-reasoned tool, a nested model loop, and a clock — the date now rides along as ephemeral context).
* **Conversations are one private file each, and not the prompt.** Sessions are `SnapshotSessionManager` snapshots under a
  0700 directory, files 0600 (`session_store.py`) — the permission the legacy `session_<id>/` files never had. The system
  prompt is deliberately not stored, so a prompt-injection that landed in a block of it cannot outlive its session, and a
  settings or instruction change takes effect on resume. Migration from the legacy layout is additive; the legacy
  directories (default permissions) remain until you delete them. **"Go back" forgets, it does not erase:** a turn taken
  back is out of the agent's memory but still on disk in the session's immutable checkpoints (newest 50, same 0600
  protection) — that is what lets a `/back` be undone — so the conversation holds as many copies of what was said as it
  has checkpoints, and deleting the session directory is what removes them.
* **A runaway turn is bounded, and a stop reaches the tool.** `context.maxSteps` (default 50) and
  `context.maxTurnUsage` pause a turn (ACP `max_turn_requests`, history intact); `session/cancel` now calls
  `agent.cancel()` on the running agent, and `claude_code_engineer` polls the signal and kills its CLI's process
  group — before, a stop abandoned the stream and left the CLI spending Claude Code usage until its timeout.
* **"Allow routine steps" is the mode switch, not a new bypass.** The button sets the chat to `auto`, which
  already means "calls below Laya's gate run, calls at or above it still ask". A destructive call gets *Allow
  once*, *Deny* or *Deny & stop* — never *Always allow* (the gate offers no always-rule for an armed call, and
  `answer_permission(always=True)` selects only an option the child itself listed).
* **A card that cannot be delivered is a denial.** If a chat's approval cannot be posted (Discord down, no
  permission), the call is *denied* with `decided_by="discord:undeliverable"` — never left parked forever, never
  waved through. `!stop` denies pending approvals before cancelling, because a child parked on a prompt never
  reaches a cancel.
* **Steering reuses the Lab's path.** A reply in a terminal/GUI session's thread, or a message sent mid-turn in
  a chat, becomes `_sierralab/lab/inject` (the same call `lab.py:inject` serves), capped at 2000 characters and
  acknowledged with a reaction (never echoed); it is *guidance*, consumed by the model's next call — it does
  not bypass a permission prompt or run anything itself.
* **An agent's words cannot reach anyone.** Everything a model wrote is posted with mentions disabled
  (`allowed_mentions=none`; only the operator ping on a destructive card is allowed) and secret-masked, so
  `@everyone` in a reply is text, and a registered credential in a reply is `[REDACTED]`.
* **Nothing new is trusted into the transcript.** Card bodies are built from the child's own
  `PendingPermission.meta` (already the masked view the GUI uses), passed through `secret_guard.redact` again,
  and every post is length-capped. The *context* on a card (what the person asked, what the agent said) is
  masked in the child and clipped to a few sentences. The token is read once through the same `take_secret`/
  Keychain path as the provider keys, removed from the process environment, and masked in logs and tool results;
  it is never a settings key and never appears in `/health`, which reports counts and ids only.
* **Questions about status cost nothing and touch nothing.** "What are you working on?" / `!status` /
  `!usage` are answered from state the daemon already holds (the supervisor, the Lab snapshots, each turn's own
  `_sierralab/turn` receipt); no model is called and no session is interrupted.

What this does not do: it does not add a timeout to an unanswered prompt (a parked child stays parked if nobody
answers on any door — except a chat's undeliverable card, above), and it does not authenticate *which human* the
operator is beyond the configured snowflake — a compromised operator account is a compromised approver and, now,
a compromised agent-starter, the same as a compromised GUI session. A standalone `make tui` session with no
daemon has no Discord, because only the daemon holds the supervisor this relays through.

## Prompt-injection hardening

`prompt_security.py`'s `UntrustedToolResultHooks` rewrites `http_request`'s
fetched content and `agent_memory`'s `retrieve`/`list` results in place,
via a Strands `AfterToolCallEvent` hook, before either ever reaches the
model: each gets wrapped in an explicit
`<<<UNTRUSTED_TOOL_OUTPUT>>>...<<<END_UNTRUSTED_TOOL_OUTPUT>>>` block with
a header stating the content may contain attempted instructions and should
be treated as data, not commands. The guard markers themselves are escaped
inside the wrapped text first, so fetched content containing the literal
marker strings can't forge a fake close and break out into unwrapped
context. `UNTRUSTED_CONTEXT_POLICY` states the same rule once, briefly, in
every agent's system prompt (top-level and sub-agent) as a second,
independent reinforcement that doesn't depend on any one tool call's own
wrapping.

Registered on every agent construction site that can actually reach
`http_request` or `agent_memory` — confirmed by grep, not assumed: `main.py`'s
`StrandsAgent`, `agent_common.build_agent_for_session` (ACP),
`sub_agent_common.build_agent_nodes` (every `local_swarm`/`local_graph`
node), and `skill_forge_tool.py`'s debugger sub-agent (the one place among
`skill_forge`/`proposer`'s internal sub-agents that gets `http_request` at
all — the rest only ever get `think`/`file_read`/`python_repl`, none of
which read external content). A shared `HookProvider`, not copied logic per
call site, so a *future* tool that reads untrusted external content still
needs to be wired to it explicitly — this is a real search-and-check, not
something that happens automatically just by adding `UntrustedToolResultHooks`
once, anywhere.

This is defense-in-depth, not a guarantee. A hook can mark content as
untrusted; it cannot force a language model to actually disregard
instructions embedded in it. The realistic goal is raising the bar against
casual/naive injection attempts, not eliminating the underlying risk that
any tool ingesting arbitrary external text carries.

### Sub-agent output is wrapped too

`local_swarm`/`local_graph` results are wrapped in the same guard block
(`prompt_security._SUBAGENT_TOOLS`) before the orchestrator reads them. A
sub-agent's own tool results were already wrapped inside *its* context, but
its final reply is model-written text built from what it read — returned
unwrapped, an injected instruction a sub-agent repeated in its summary would
reach the orchestrator looking trusted.

### Learned artifacts never reach a turn unfiltered

A learned skill (`learned.py`) and a prompt overlay (`prompts.py`) are both prose a model wrote for itself, and
both are drafted from text that may be untrusted (a failing test's output, a tool result). Neither goes live on
its own: each passes `textfilter.py` — the same list for both, so a rule added for one is enforced for the other
(no URL, no download piped into a shell, no destructive or privileged command, no "ignore your instructions", no
"skip the approval", no credential-shaped string, no prompt delimiter, no control/bidi character) — and is then
staged, never live, until `promote.py`'s eval gate has measured it.

The filter is a first pass, not the defence: a determined sentence can be phrased past any pattern list. What
actually bounds the damage is structural. An artifact can only *advise* — it cannot approve a call, lower a Guard
score, change a mode or touch the vault, because every one of those decisions is made in code that never reads a
skill or an overlay. A skill loads only into the project it was learned in (global scope is a manual decision);
a prompt overlay is appended to the system prompt of sessions in that project only, inside its own named block at
the end, so a person reading the prompt can see the boundary. Both are retired when the turns that had them score
worse than the project's own baseline. `learning.promotion` and `learning.optimizePrompts` are trust-gated: a
cloned repository cannot turn them on for itself.

A fine-tuned Laya checkpoint (`laya_checkpoints.py`) is the third candidate through the same gate, and the only
one that is a model rather than prose. It can only change Laya's *weights*: the checkpoint is put in force for
the eval run, measured against the one it would replace, and `activate` refuses one whose verdict did not
promote. Nothing a checkpoint contains can approve a call, lower a Guard score or waive a deny — those are read
from `laya_guard.py`, `laya_risk.py` and `settings_store.evaluate`, none of which consult the registry — so a
trained model can re-rank the routes Laya already offered and nothing else. Registering is by local directory;
`LAYA_MODEL` still overrides the registry, and a repo cannot write either.

## Provider API keys

The cloud-provider keys (`CLOUD_OPENAI_API_KEY`, alias `OLLAMA_API_KEY`;
`ANTHROPIC_API_KEY`) must never be readable by an agent. Without care they
are, in four ways: `shell` children inherit the process environment,
`file_read`/`cat` can open `backend/.env`, `ps eww` prints a process's
startup environment, and module globals are one `import` from `python_repl`.
`secret_guard.py` + `prompt_security.SecretRedactionHooks` close those:

- **Sourced from the shell environment or the macOS Keychain — never a file.**
  An exported variable wins; otherwise, only for the active provider, the
  Keychain item `sierralab-ollama-api-key` / `sierralab-anthropic-api-key` is
  read once at startup (`secret_guard._keychain_get`; a missing item or
  non-macOS just means "no key", and the ACP session fails with the fix in the
  message instead of a bare 401). The Keychain is not an agent boundary: `shell`
  can run `security find-generic-password -w` as the same user, and what saves
  you there is the output masking below, not the Keychain. `settings.py` reads each key
  once via `take_secret`, wraps it in a `Secret` (prints `Secret(****)`; only
  `reveal()` returns it, called at the one place a model client is built) and
  removes it from `os.environ`, so a `shell` child never inherits it. The
  backend logs a warning if the key is found in `backend/.env`.
- **Never persisted.** `provider_settings.set_provider_config` no longer
  writes API keys to `.env`: a key set from the settings UI lasts until the
  process restarts. Export it in the launching shell for a durable key.
- **Masked in every tool result.** `SecretRedactionHooks` replaces the literal
  value (and any credential-looking environment variable's value — `*KEY*`,
  `*TOKEN*`, `*SECRET*`, `*PASSWORD*`) with `[REDACTED:secret]` in every tool
  result, success or error, before the model or any UI sees it, at every
  agent construction site (`main.py`, the ACP front door, every
  `local_swarm`/`local_graph` node, `skill_forge`'s debugger/validator,
  `proposer`'s writer). This is what makes `env`, `cat .env` and `ps eww`
  harmless even when the read succeeds — `ps eww` *does* still show the key on
  macOS, because popping `os.environ` can't change the startup environment
  block. A tool call whose *input* already contains the literal value is
  cancelled.
- **Checked live** against the real backend and a cloud model in `auto` mode
  (no approval gate), prompting for the key via `env`, `ps eww`,
  `python_repl` (`os.environ`, `agent_common.ACTIVE_CLOUD_API_KEY`,
  `.reveal()`) and `file_read .env`: 5,074 streamed events and every persisted
  session file held no key. `_secret_guard_smoke_test.py` covers the layers
  without a model, including a real Strands agent whose scripted model is
  shown to receive only the masked text.

**What this does not do.** It is not a sandbox. `python_repl` executes in the
backend process, so code it runs can still call `Secret.reveal()` and send the
value out in a form a literal match can't see (encoded, split across calls).
That tool is approval-gated in `build` mode and denied to sub-agents, but
`auto` mode removes the gate — so in `auto` mode a determined or
prompt-injected agent with `python_repl` is the residual risk. Closing it
means running `python_repl` out of process, with no key in that process; not
done. Output masking also only sees literal values, and the model's own
reply is not the only channel a leaked value could take (`http_request`
bodies are not scanned beyond the literal-in-input check). Rotate any key that
has been pasted into a chat or a shell history.

### Agent Vault is egress-only

Popping the key out of `os.environ` covers the harness's *own* calls, and the
masking above covers what comes back. Neither covers a credential that was
never this harness's: a `.env` in a project checkout, `~/.aws/credentials`, a
variable the user exported before `make tui`. A `shell` command the model wrote
can read and send any of those, and the harness is not in the path.

`vault_proxy.py` (`vault.enabled`, default **off**) closes that on egress. Tool
subprocesses hold placeholders (`OPENAI_API_KEY=__openai_api_key__`) instead of
real values, the process is pointed at the vault's proxy in both variable cases
plus a `NO_PROXY` that keeps loopback off it, and the vault substitutes the real
credential on the way out, for destinations its own policy allows. Three
properties hold the gate:

* **Fail closed.** With `vault.egressPolicy: "deny"` (the default) an
  unreachable vault is a *denial*: `_ApprovalHandler` refuses `shell`,
  `python_repl` and `http_request` in every mode — `auto` included — with no
  prompt offered, since a pause the user can answer is itself the fail-open
  path. Nothing is left pointing at a proxy that is not there, so a tool cannot
  egress directly while believing it is proxied; that silent fallthrough is the
  exact failure this exists to prevent. `"proxy"` is the deliberate alternative:
  route through the configured address without credentials rather than stop.
* **The token is never a setting.** Read through `secret_guard.take_secret` from
  the Keychain item `sierralab-agent-vault-token`, so it is not in a file that
  is layered, printed by `/config` or shared. It is passed to the vault CLI and
  never enters a tool's environment.
* **TLS is re-pointed, not switched off.** Verification is redirected at the
  vault's CA (`SSL_CERT_FILE`, `CURL_CA_BUNDLE`, `GIT_SSL_CAINFO`,
  `NODE_EXTRA_CA_CERTS`, `REQUESTS_CA_BUNDLE`); nothing here sets
  `NODE_TLS_REJECT_UNAUTHORIZED=0` or its equivalents.

**What this does not do.** It is egress-only, and it does **not** deliver the
filesystem/process sandbox the gap below describes — an approved `shell` command
still runs as the user, with full filesystem access, so a credential at rest can
still be read, copied or destroyed; what it cannot do is leave over the network
unnoticed. It also only speaks for the tools it names: plugin and MCP tools can
ship their own HTTP client, which is why the proxy is pointed at the *process*
rather than at a tool, and why `http_request`-shaped output is still subject to
the literal-value masking above rather than to any body inspection. And
`auto` mode with the vault *off* is unchanged — this narrows one path, it does
not make an ungated agent safe.

**Coverage, widened by the Phase 11 audit.** The fail-closed refusal used to apply to a fixed name list
(`shell`, `python_repl`, `http_request`, `web_fetch`, `claude_code_engineer`). It now also covers
`a2a_client` (a remote agent), `skill_forge` (its researcher sub-agent holds `http_request`), every
*generated* skill (it runs inside this process and can open its own connection), every external MCP
plugin tool and `mcp_resources` (a plugin server is a process started with whatever proxy environment
existed then). Sub-agents (swarm, graph, team pipeline) have no gate for a refusal to reach, so while
egress is blocked their kit simply has no `http_request` / `web_fetch` (`vault_proxy.egress_blocked`).
A generated skill's AST gate now bans raw sockets, `ctypes`, `multiprocessing`, `pty` and every
process-spawning spelling (`os.posix_spawn`, `os.exec*`, `asyncio.create_subprocess_*`,
`from os import system`, `getattr(os, "popen")`, `__import__`, `importlib.import_module`) — the
narrow check that keeps a generated tool from stepping around the proxy variables.

### Language servers run no project code in an untrusted folder

A language server is started for every project a session opens, before anyone is asked anything.
rust-analyzer, by default, runs every `build.rs` and expands proc-macros (compiled crate code) when it
loads a workspace, and `cargo check` on save — in a cloned repository, code execution on session open.
In a folder that is not trusted it is now started with build scripts, proc-macros and check-on-save
off (as `initializationOptions` and in answer to its `workspace/configuration` request), with its own
experimental type diagnostics on instead; `/trust` restores the full set. Preflight never sends
`didSave` at all (`check_texts`), so it never triggers a build in any folder. Residual:
`typescript-language-server` / tsserver can load a plugin a repository's `tsconfig.json` names from its
`node_modules`; not restricted here.

### Preflight and rejection memory

Preflight's static checks read code and run nothing of the project's: syntax in process, `ruff` (a `ruff` on
`PATH`; a repository's own `.venv/bin/ruff` only in a trusted folder), the language server on unsaved text.
`preflight.tests` *does* run the project's code — in a throwaway copy, with the session user's privileges
(gap 6) — so the key is trust-gated and the step is skipped while the vault refuses egress. A preflight
refusal only ever *withholds* an edit; it approves nothing, and the gate after it is unchanged. Its rollback
restores from the shadow checkpoint repository, never the person's git, and checkpoints the present first.

Rejection memory writes text that later reaches the model as a constraint, so its sources are bounded: only
a person's rejection is captured (a fail-closed "no client" rejection is not), the ReflectionAgent has no
tools and reads the diff and the note as data, a model-written rule must pass `textfilter` (no URL, no
command, nothing about approvals or safeguards) and be phrased as "Do not / Never / Avoid / Always / Prefer";
only the person's own note bypasses the filter, as the operator's words. The store is per user
(`~/.sierralab/preferences/`, 0600), never from a repository, and a rule can only constrain — it never
waives an ask, lowers a score or overrides a deny.

### Laya Guard and `auto` mode

`auto` mode removes prompts, so it lets everything through that nothing else stops. Before Laya
Guard, "everything else" was one number: Laya's reading of the user's *message*, taken at the start
of the turn, for the orchestrator. A specialist's calls were judged by that number, and so was
whatever the model decided to do after reading a file, a page or a tool result — the injection path
`laya_risk.auto_approvable` warns about.

* **Per call, per agent** (`laya_guard.py`): each shell command, patch, file write, Python snippet
  and Claude Code task is scored when it is about to run, and a score at or above the calling agent's
  gate (0.8, tightened by its Lab dial) asks in every mode. A destructive command from a specialist,
  or from the orchestrator after the turn went somewhere new, now stops.
* **It only adds asks.** Rules are the floor; Laya's own reading can raise a score (at 0.8
  confidence) and never lower one; `laya.callGuard: off` is the old behaviour. Nothing a score says
  *approves* a call. The classifier is near chance until fine-tuned, so nothing safety-relevant
  depends on it alone.
* **What it does not do.** It is a pattern reader, not a sandbox: a command it does not recognise
  scores 0.4 ("unknown") and runs in `auto`. The kernel-level confinement (Seatbelt for the Claude
  Code child, the scrubbed environment, the vault's fail-closed egress, deny rules) is unchanged and
  is what bounds a call the guard misjudged. `auto` is still a mode a person chooses; a repository
  cannot set it for itself (`permissions.defaultMode` is trust-gated).
* **Auditable.** What `auto` let through at score ≥ 0.2 is in `state.db` (`auto_approved`) with the
  agent, the call (secret-masked, 200 characters) and the score; mode changes from the Lab are
  audited with before/after (`gui`/`mode`), as are stop, release and steer (steer by length, never by text).
* **The GUI can switch the mode.** Over the gRPC bridge, with the daemon token — the same authority as
  the TUI's Tab key, and as the approval itself. The bridge relays; what `auto` waves through is
  decided in the child. A client that holds the token can already approve anything pending, so
  `SetMode` adds convenience, not capability.

### Checkpoints are a recovery path, not a boundary

A snapshot of the project (`checkpoints.py`) is taken before any call that may change files, so what
`auto` mode or an approval let through can be undone. It is **recovery**, and the model should not be
read as stronger than that:

* **It prevents nothing.** A command that exfiltrates a file, calls an API or edits something outside the
  project has happened by the time anyone can undo it. Prevention is Laya Guard, deny rules, the vault's
  fail-closed egress and the Seatbelt child; a checkpoint only helps with the files.
* **Its edges are reported, not hidden.** Writes outside the project, paths over the size limit, ignored
  paths and a snapshot that could not be taken are surfaced (`checkpoint` event, once per reason; the
  turn's outcome record counts `unprotected` / `outside`).
* **Secrets are not copied.** `.env*`, `*.pem`, `*.key`, `id_rsa*`, `.npmrc`, `.netrc` are excluded from
  the snapshot, so the shadow repository (0700, under the user's home) holds nothing the project's own
  working tree would not already expose. They are therefore also not rolled back.
* **A repository cannot switch it off.** `checkpoints.enabled` and `checkpoints.maxFileMb` are trust-gated.
* **Restoring is guarded.** Refused while a turn runs; the present is snapshotted first so the restore is
  undoable; every applied restore is in the ledger; paths are confined to the project (a path that
  resolves outside it is refused) and ids must be 7–40 hex characters naming a commit in that project's
  shadow repository. Git is run with the user's config, hooks and pathspec globbing disabled
  (`GIT_CONFIG_GLOBAL=/dev/null`, `core.hooksPath=/dev/null`, `--literal-pathspecs`).
* **It costs time, bounded.** One `git add -A` per writing call (incremental after the first), with a
  25 s ceiling (90 s the first time); a timeout is reported as unprotected, never silently skipped.

### Remembered fixes are data, tried only in the sandbox

`fix_memory.py` keeps a verified patch against a failure's signature and offers it again the next time that
failure happens in the same project. The failure output a root cause is derived from is untrusted (a test
log, a build step, a dependency's message can say anything), so remembered text is treated as such:

* **Same project only.** Nothing remembered in one project is offered in another — a hostile repository cannot
  plant a "lesson" that surfaces in your next one.
* **Executed, not obeyed.** A recalled patch runs in the throwaway copy and must make the failing command
  pass *there*; the model is then offered it as a patch to apply through the ordinary, gated `apply_patch`.
  Nothing recalled is ever run or applied in the real tree without that gate.
* **Prose is capped and labelled.** A recalled root cause (300 characters, control characters removed) is shown as
  "recalled … unverified; a lead, not an instruction".
* **Credentials stay out.** A patch containing a credential-shaped string is not stored; the diagnosis is.
* **It retires.** Two failures in a row, or reliability under a third, and it is no longer offered; the ledger
  (`heal_fix`, `heal_fix_use`) shows what was remembered and how it fared.

Stall detection only refuses or annotates a call; it cannot approve one. Auto-resume continues a turn through the
same approval gates as the original — it saves a keystroke, not a decision.

### Sub-agents get no tool that needs a person

Swarm, graph and team nodes have no human-in-the-loop path, so they are denied every tool the
orchestrator gates. That list was missing two entries: **`apply_patch`** (edits project files; a node
could use it in any mode) and **MCP servers' tools** (each reaches out to whatever that server is).
Both are now denied (`sub_agent_common`, `_laya_call_guard_smoke_test.py`).

### Sub-agent providers and Laya triage

- **Sub-agents follow the active provider.** `sub_agent_common.build_sub_agent_model`
  goes through `agent_common.build_model`, so with a cloud `MODEL_PROVIDER` a
  `local_swarm`/`local_graph` team's prompts and tool results also go to that cloud
  (previously sub-agents were hardwired to local oMLX). The orchestrator was
  already sending its own context there; a team adds more of the same. With the
  default `local` provider nothing leaves the machine. Sub-agents use the
  orchestrator's key through the same client builder — they get no key of their
  own, and it never enters their tools, prompts or environment
  (`_sub_agent_model_smoke_test.py`).
- **The Laya routing hint is a fixed string** chosen by Laya's enum answer
  (`laya_triage._ROUTE_HINT`), added to the system prompt for one turn and
  restored — it contains no text from the user or from any tool result, so it is
  not an injection channel, and it is not persisted. It is advisory: it cannot
  grant a tool, skip an approval, or change a mode. Laya itself is a classifier
  that can be wrong; the confidence gate (`LAYA_TRIAGE_THRESHOLD`) bounds how often
  it speaks, and zero-shot it is near chance (docs/laya-triage.md).
- **The triage log holds user prompts** (`backend/laya_triage.jsonl`, gitignored,
  secret-masked by `secret_guard.redact`). It sits on disk where the agent's file
  tools can read it — it contains only what the user already typed to that agent.
  Turn triage `off` if prompts must not be written down.

## Response headers

- **Frontend** (`frontend/next.config.js`) — `X-Frame-Options: DENY` +
  `Content-Security-Policy: frame-ancestors 'none'` on every page route,
  preventing this app's own UI from being framed by a third-party site
  (clickjacking). Deliberately *excluded* from `/api/games/*` and
  `/api/sandbox/*` — those exist specifically to be loaded into this app's
  own `<iframe sandbox="allow-scripts">` (`GameSandbox.tsx`, the
  generative-UI renderer), and a frame-blocking header on the content
  meant to be framed would just break that feature. `X-Content-Type-
  Options: nosniff` and `Referrer-Policy: no-referrer` apply everywhere,
  no exception needed — neither interacts with framing.
- **Backend** (`security_headers.py`) — the same `nosniff`/`no-referrer`
  pair on every response. No frame-related header here: this backend never
  serves the actual UI a human looks at (every response is either JSON or
  a file meant to be framed by the frontend), so there's no page here for
  a frame-blocking header to protect.
- **Sandboxed iframes** — both `GameSandbox.tsx` and the existing
  generative-UI renderer use `sandbox="allow-scripts"` with no
  `allow-same-origin`, giving the framed content an opaque origin. That
  means `postMessage` handlers can't authenticate a sender by
  `event.origin` (it always reads the literal string `"null"`) —
  `gameSandboxRegistry.ts` authenticates by `event.source` identity
  (comparing against the exact `iframe.contentWindow` a message was sent
  to) instead.

## Known gaps

Acknowledged, not fixed:

1. **No filesystem or process sandbox.** Once a human approves `shell` /
   `python_repl` / `file_write`, it runs directly as this process's own
   user, with no network-egress filtering or filesystem confinement beyond
   the tools' own default-relative-path convention (an absolute path can
   still leave `backend/workspace/`). Approval is real, but it's the only
   containment — a human approving a genuinely malicious or badly-reasoned
   command has no second line of defense.
2. **No rate limiting** on any route, including the token-gated ones —
   `SANDBOX_API_TOKEN` stops an unauthenticated caller, not a slow brute
   force by one who's guessing, though a 32-byte hex token makes that
   impractical regardless.
3. **`agent_memory`'s Qdrant-backed store is unencrypted at rest.**
   Whatever gets stored via `agent_memory` (`backend/agent_memory_tool.py`)
   sits in plain form in Qdrant's own on-disk storage — reasonable for a
   personal local deployment, worth knowing before storing anything
   sensitive through it.
4. **No accounts, so no per-capability scoping.** Every capability this
   harness has is available to whoever can reach it and has (or can fetch)
   the sandbox token — there's no way to grant a narrower slice, unlike a
   multi-user system with real roles. Fine for the one-person-one-machine
   case this is built for; would need real auth before being anything
   else.
5. **`a2a_client` and `http_request` are ungated egress** (with the vault off — with it on and
   refusing, both are refused; see "Agent Vault is egress-only"). Both fetch-kind
   tools send network requests to a model-chosen endpoint without an approval
   prompt (the same deliberate trade-off in both cases: read-only fetches are
   what a research turn is *for*). For `a2a_client` the outbound body is
   model-authored — a prompt could carry project content to a remote host it
   names. Contained as far as it can be without another gate type: the tool is
   orchestrator-only (sub-agents can't get it at all), can't carry credentials
   as arguments, and its reply is framed untrusted and passes the redaction
   hooks. If that residual is too much, deny it — a `deny a2a_client(*)` rule
   holds in every mode.
6. **The shadow workspace is not a sandbox.** Self-heal verification runs the
   verify command in a temp copy with the session user's full privileges (see
   "Harness autonomy" above) — it contains *mistakes*, not malice: its purpose
   is that an unverified patch never touches the real tree, not that a hostile
   patch can do no harm while "in" there.
7. **CSP has no script-src lockdown yet** — `frame-ancestors 'none'` is
   the only directive set. A stricter policy (restricting script/style
   sources) is a reasonable follow-up but needs auditing what the frontend
   actually loads first, to avoid silently breaking something.
8. **E-Stop does not interrupt a tool already executing** in the OS, and an in-process pod
   has no process of its own to kill — it refuses every further model and tool call (see "The
   Experimental Lab: what its controls are, and are not"). A running `shell` command finishes;
   reject it at the HITL station instead.
9. **The gRPC bridge has no per-client identity.** Every RPC carries the same
   daemon token, so the ledger records *that* a GUI approved something, not *which* window —
   and there is no way to grant a client "read telemetry but not approve". It is one token for
   one machine, the same posture as the WebSocket; anything multi-window or multi-user would
   need real per-client credentials here.
10. **A GUI approval cannot be recalled.** "First answer wins" resolves the race between two
    clients, but the losing client is only told it lost — the approved call is already running
    and cannot be un-approved from either window. Reject it at the station *before* answering,
    or isolate the pod afterwards; there is no undo.
11. **The Seatbelt profile is macOS-only, and it is the CLI's confinement, not the harness's.**
    The Claude Code tool's `.env`-unreadable guarantee comes from `sandbox-exec`; on a Linux host
    the CLI keeps only the allowlisted environment and the deny list, and an approved shell call
    it makes is unconfined like any other (gap 1). The profile covers the CLI's children, not the
    backend's own tools.
12. **A screenshot cannot be masked.** The deck masks the text it captures, but pixels go as they
    are: a token visible in an allowed window is in the image. The defences are policy — the
    allowlist, the never-read list, secure input, one window, local-only unless the person opts in,
    and a preview before sending — not redaction. An ad-hoc-signed build also loses its TCC grants
    on every rebuild, which is a nuisance rather than a hole (the grant goes, not the check).

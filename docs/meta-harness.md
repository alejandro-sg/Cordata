# The Meta-Harness

The Claude Code CLI as a tool, local Prism inference under a unified-memory budget, and a native
window onto the daemon's telemetry bridge — the three pieces that turn the harness from "a
Strands session with specialists" into something that delegates, spends memory deliberately, and
can be watched and answered by a person who is not in a terminal.

[`docs/experimental-lab.md`](experimental-lab.md) ends at Phase 5 (the Lab view inside the TUI);
[`docs/agentic-harness.md`](agentic-harness.md) covers the harness the specialists run on. This
document covers what came next.

```
make lab-demo         # try the window with no model, key or token: a scripted daemon + the window
make daemon           # the real front door, its gRPC bridge and its observer
make lab              # the native GPUI window against that daemon (it can be opened first: it keeps looking)
make lab WATCH=~/proj # …watching that workspace for changed symbols
make lab-test         # lab/ cargo test --workspace  (the client, observer and view; no Python)
make lab-live         # the Rust client against the *real* Python bridge, then the audit ledger it left
make proto            # regenerate the Python gRPC stubs after editing proto/harness.proto
```

Contents: [ClaudeCodeEngineer](#claudecodeengineer) · [The unified-memory budget](#the-unified-memory-budget) ·
[Local inference: oMLX and the Prism registry](#local-inference-omlx-and-the-prism-registry) ·
[The gRPC telemetry bridge](#the-grpc-telemetry-bridge) · [The GPUI Lab](#the-gpui-lab) ·
[What is and isn't true](#what-is-and-isnt-true) · [Tests](#tests)

## ClaudeCodeEngineer

`backend/claude_code_tool.py`. For a large, many-file coding task the orchestrator does not write
the code itself: it calls `claude_code_engineer`, which runs the official CLI as a **child
process** and judges the outcome from a structured result.

Four properties make that a default path rather than a demo:

* **Headless.** `claude -p --output-format stream-json --permission-prompts none`. The task goes in
  on stdin; nothing can wait on a TTY or a permission dialog (anything not on the allowlist is
  denied, not asked). The event stream is parsed live, so progress reaches the Lab and the TUI
  while it runs — `{"progress": …}` tool-stream updates, a task bar, and the `claude_code_engineer`
  pod's trace.
* **Memory-primed.** The prompt carries the top matches from the LanceDB trace corpus
  (`trace_store.search`): past commands, outcomes and errors in this project, retrieved by
  similarity to the task, wrapped as untrusted **data** (they are log text, never instructions) and
  secret-redacted. Empty when `memory.traces` is off, and the result says so rather than pretending.
* **Egress through the vault, or not at all.** The child is spawned with `HTTPS_PROXY` (and the CA
  bundle variables) pointing at the Agent Vault proxy — the zero-trust MITM that substitutes
  credentials on the way out (`backend/vault_proxy.py`). With the vault off or unreachable the tool
  **refuses** (fail closed, like the `shell` gate); there is no setting that lets this CLI egress
  directly. Its environment is *allowlisted*, not inherited (`scrubbed_sandbox`), so no provider
  key, vault placeholder or token is in it; and on macOS Seatbelt makes `.env` files unreadable to
  it at the kernel, whatever its tools or a script it writes try.
* **Memory-aware.** Before spawning the CLI it calls `memory_budget.release_for_heavy_task`:
  local model weights go before the CLI's own footprint needs the room.

## The unified-memory budget

`backend/memory_budget.py`. The host is a 48 GB Apple-silicon machine where CPU, GPU and the
Neural Engine share one pool. Local weights live in that pool next to everything else the harness
keeps resident — the LanceDB trace corpus, the Laya checkpoint, the Python agent stack, the Claude
Code CLI. Out-of-memory there is not an exception to catch; it is the whole machine thrashing.

```
ceiling      48 GB   SIERRALAB_MEMORY_CEILING_GB
− reserve    10 GB   SIERRALAB_MEMORY_RESERVE_GB  (macOS, LanceDB, Laya, the harness, the CLI)
= budget     38 GB   for model weights
```

`ensure_headroom(needed_gb)` is the one operation everything else uses. It sums what is resident
(oMLX's status + Ollama's `/api/ps`, via `omlx_manager` / `ollama_cli`) and, when `needed` does not
fit, **stops the least recently used unpinned models until it does** — and says so. If it cannot
make room it loads nothing and says why; it never evicts for a request bigger than the whole
budget, because the eviction would not have helped.

Callers, each one a place a prompt would otherwise have to be trusted:

| caller | runs it when |
|---|---|
| `omlx_resource_tool` (`load`) | before loading a Prism model |
| `ollama_resource_tool` (`ensure_headroom`) | the LLM-visible handle for the same thing |
| `claude_code_tool` | `release_for_heavy_task`, before spawning the CLI |
| `daemon/grpc_bridge.py` | the GUI's emergency **[Unload Memory]** → `unload_all` |

Stdlib plus the two light managers; no `strands` import — the daemon never imports the agent stack.

## Local inference: oMLX and the Prism registry

`backend/omlx_manager.py` (lifecycle), `backend/omlx_provider.py` (the Strands provider),
`backend/omlx_resource_tool.py` (the tool), `backend/context_digest_tool.py` (the caller).

`MODEL_PROVIDER=local` points the *orchestrator* at oMLX. The Prism module is the other thing: a
**secondary provider instance** the Meta-Harness routes sub-tasks to, whatever the orchestrator
itself runs on, to save cloud spend on work a 2-bit 27B model does well.

Registry (the spec's identifiers; the org prefix is the registry's, oMLX serves the bare name):

| registry id | size | used for |
|---|---|---|
| `prism-ml/Ternary-Bonsai-2-27B-mlx-2bit` | 8.41 GB | architect planning |
| `prism-ml/Ternary-Bonsai-27B-mlx-2bit` | 8.30 GB | local context digest |

Three guarantees, each enforced mechanically rather than asked of the model:

1. **A request never loads a model.** `EndpointGuard` lets exactly `POST <base>/chat/completions`
   through, so nothing built here can reach oMLX's load/unload routes; and `PrismModel` checks the
   model is *resident* before every request — oMLX would otherwise load an inactive model as a side
   effect, which is the one way weights end up in unified memory without anyone deciding they
   should.
2. **Load before delegating, unload after.** `PrismLeaseHooks`: a refcounted lease is taken when a
   Prism agent starts and released when it finishes; the last release unloads — but only if that
   lease is what loaded it, so a model the orchestrator itself holds is never unloaded out from
   under it.
3. **Fail over, don't fail.** Prism candidates sit in a Strands `ModelRouter` *ahead of* the
   provider the harness would have used anyway; a lease that could not be taken (oMLX down, no room)
   makes the Prism request refuse and the router moves on to the next Prism model, then the cloud.

Gate: `omlx.prism` (default on), and oMLX must answer `/health`; otherwise `build_sub_agent_model`
returns exactly what it returned before the module existed.

The digest role (`context_digest`) is the clearest case for all of it: 40 KB of build log goes to
the **local** model, a short faithful digest comes out, and only the digest reaches a cloud model's
context. The digest agent has **no tools** — the text it reads is untrusted, and there is nothing
for an instruction hidden in it to call.

## The gRPC telemetry bridge

`proto/harness.proto` (one schema), `backend/daemon/grpc_bridge.py` (server), `lab/crates/harness-proto`
and `lab/crates/harness-client` (Rust, generated client + supervisor).

One service, `sierralab.harness.v1.Harness`, on loopback:

| RPC | kind | what it is |
|---|---|---|
| `StreamTelemetry` | server streaming | daemon state as it changes, latest-wins per subscriber |
| `InterventionStream` | bidirectional | HITL requests down, explicit approve/reject back |
| `ProactiveIngest` | client streaming | AST snippets up, never blocking the UI thread |
| `SetKnob` | unary | `reasoning_effort` / Laya dial, mid-execution |
| `UnloadMemory` | unary | the emergency unload — oMLX/Ollama weights out |
| `GetDiff` | unary | the body of a diff a frame only referenced |
| `SubmitTurn` | unary | a turn from the menu-bar deck, with the screen context the person attached |
| `Ping` | unary | liveness + schema version, what the reconnect loop probes |

What it is and is not:

* It **relays and reports**; it never decides anything the harness decides. An approval is a
  human's click forwarded into the paused ACP request, restricted to the options the *child
  offered*; a knob is the child's own `_sierralab/lab/*` control called on the user's behalf; the
  unload is the same `memory_budget` code the agent's tools use.
  **`SubmitTurn` is the one exception**, and a deliberate one: it *starts* a turn. It still decides
  nothing about it — a daemon-owned conversation gets a real turn through `Chats.send`, a session
  another client drives gets text guidance on the steer path (the daemon never issued its
  `session/new`), and no session opens a `menubar:<cwd>` conversation. It accepts and returns (a
  turn may run for hours), re-checks the app against the never-read list before forwarding, and
  audits the context by size only. `THREAT_MODEL.md`, "OS context capture", has the policy.
* It is **loopback-only and token-gated**: `127.0.0.1` is not configurable, and every RPC needs the
  `x-daemon-token` metadata (the 0600 secret in `daemon.json`) compared in constant time. Loopback
  is not an authorization boundary, which is why the token is there even without TLS.
* It is **bounded everywhere a client could grow it**: telemetry is latest-wins, the intervention
  fan-out queue is capped, ingest is capped and deduplicated, snippets and diffs have size limits,
  and messages are capped at 16 MiB.
* **A memory read rides `LabState`, it does not get its own event.** `LabState.memory` carries each
  pod's most recent reads (`pod`, `source`, `query`, `count`, a few hits with an *optional* score — a
  label-only kg row has none, and the wire does not coerce it to 0.0), so the canvas animates a packet
  without a second stream to lose. The client enqueues a hit only when its `ts_ms` is newer than that
  pod's last, so a replayed or re-sent frame does not re-animate, and the flight is timed off the
  client's own clock rather than the daemon's. Known limit: the frame is *latest state, not a stream*,
  and a non-focused pod ships only its last four reads — a burst on a background pod can drop one
  before the client sees it. That is fine for an animation; anything that must see every read needs a
  one-off event instead.
* Everything a GUI exercises is **audited** (`state.db`, kind `gui`): who approved the destructive
  diff, who unloaded the models.
* Two front doors can answer the same question; **the first to answer wins** and the other is told
  (`Supervisor._admit_client_frame`) — a GUI click and a TUI keypress cannot both reach the child.
* **A refused control is a reply, not a transport error.** `SetKnob` answers `KnobReply.ok=false`
  with the reason in the body (an unknown pod, a level that is not one of the four, a NaN dial). That
  is the schema's own partial-failure channel and the Rust client renders it as an inline notice
  ("refused — …"); a gRPC `Status` means the *daemon* is unreachable and the window goes offline. The
  two must not be conflated: making a bad argument a status would show a live daemon as down.
* **The bridge only ever tightens.** It mirrors the child's thresholds rather than importing them —
  `lab` and `laya_risk` both reach the agent stack, and the daemon is stdlib-only on purpose. A
  mirrored constant that drifted would be a gate a client could *loosen*, so the copy is asserted
  equal to the originals in CI, the dial is clamped in the bridge as well as in the child (a
  scripted client need not clamp), and a request the child left unlabelled at or above the router's
  destructive gate arrives **armed** with a derived band. Both directions can make a human look,
  neither can stop one.
* **An intervention id is `<session_id>/<child JSON-RPC id>`**, and the session prefix is the only
  part the bridge reads: `_decide` partitions on the first `/` and hands the remainder to
  `Supervisor.answer_permission`, which is what keeps one session's approval from being applied to
  another. The child's id is opaque — a client echoes it verbatim and never constructs one.

## The GPUI Lab

`lab/` — a Rust workspace: `harness-lab` (the window), `harness-client` (connect, reconnect with
jittered backoff, degrade to offline), `harness-proto` (the generated wire types),
`harness-observer` (the proactive observer).

The window draws what the daemon reports and forwards a human's clicks, keystrokes and file saves.
Nothing in it decides anything about an agent. Threading is the shape of the whole crate:

* the gRPC client runs on its own tokio runtime and talks to the UI only through an `Inbox`
  (latest-wins) and bounded `try_send` channels — the UI thread never awaits the network, and a slow
  frame cannot stall a stream;
* the observer runs on its own OS thread and hands snippets to the client's ingest channel with a
  `try_send`, dropping and counting rather than blocking;
* one GPUI foreground task wakes on either the inbox or a 100 ms tick, drains, applies and
  `notify()`s — the only place the model is mutated, so there is no lock to contend.

**Trying it without spending anything.** `make lab-demo` starts `backend/lab_demo.py` — the real
`Supervisor` and the real gRPC bridge, with a scripted session (`fixtures/demo_acp_child.py`) and a
fake oMLX — in a throwaway home, and opens the window against it. The scripted agents work, a
destructive shell command is held at the gate (Laya 0.93, armed, with a token-shaped string in it so
you can see the masking — it pops the modal), a file edit is held (Laya elevated — it waits in the
tray), a 10,000-line diff is published and a 1,000-update burst runs; then it is quiet for ~100 s
before it plays again. The knobs really change the pod you are looking at;
**Unload Memory** unloads the fake oMLX's models (they come back after 20 s so you can do it
again). It never touches `~/.sierralab` or a daemon you already run.

**The window** answers four questions, in the order a person asks them, and keeps everything else one
click away:

1. **What is the team doing, and is it all right?** The *header* (the connection) and the *mission
   banner*: the task in the person's own words, one word for the whole team's state (**Working** ·
   **Needs you** · **Idle** · **Stopped** · **Offline**), counts, the mode in plain words, and the two
   numbers that say whether to worry — Laya's last reading on a painted gauge, and the **memory ring**
   (loaded models, other residents, free space) with **Unload** on hover.
2. **What is each agent doing?** *The canvas*: a pod per agent on a 2D surface, laid out from the
   delegation tree — the orchestrator on the top row, each tier below it, siblings spread evenly — with
   **conduits** between pods for the daemon's handoffs and tool calls and **memory orbitals**: a packet
   flying into a pod the moment it reads LanceDB or Cognee. A pod is *stateful* — dim when idle, a
   pulsing cyan border while synthesizing, solid cyan with a busy strip while executing, a pulsing amber
   border when contained or waiting on you, solid crimson when isolated — and still carries the status
   word, **a sentence saying what it is doing right now** ("Reading
   backend/daemon/supervisor.py"), its ticks and its risk meter. The sentence comes from
   `harness_client::words` (pure, tested), never from raw telemetry.
3. **What needs me, and what can I do about it?** *The side column*, in priority order: **Needs you**
   (approvals, a nearly-full memory pool, an offline daemon — present only when something does), then
   **the selected agent** — what it's doing, **Stop** / **Release**, how hard it thinks
   (`reasoning_effort`), the **radial Laya dial** for the score at which Laya asks you about *its*
   calls, Laya Guard's tally for it and its recent work — then tabs, one open at a time: **Activity** (a
   plain-language feed of what just happened), **Outputs** (diffs) and **System** (everything
   diagnostic: the daemon, the full gauge, memory models, the observer, decisions, the keys).
4. **What do I want next?** *The floating command deck*, one panel at the foot of the canvas: the
   **mode dial** (Plan · Build · Auto), the **steer input** (it goes to the orchestrator, which steers
   the whole team, unless a pod is selected), and the **global E-Stop**. A directive the agent reads
   arrives as an operator directive before its next model call and outranks earlier plans; the card
   shows "1 directive queued" until it has.

* **Agents** — each is one `AgentNode` (`ui/canvas.rs`), absolutely placed by the pure
  `layout_pods`, which sizes the surface it needs rather than being clipped by it. What makes an agent
  type look different is *data*: `role_style` is the single table naming roles; adding an agent type is
  one row, and an unknown role gets the generic card instead of going unrendered.
* **Conduits and orbitals** are drawn from telemetry, not decoration: a link is a `LabState.links`
  entry (or the parent edge when the daemon sent none), and a packet is a real memory read. `ui/dial.rs`
  and `ui/canvas.rs` paint their arcs and glows as rings of small quads driven by `with_animation` —
  gpui 0.2.2 has no arc primitive and no backdrop filter, and the window only repaints when telemetry
  arrives, so nothing animates off the wall clock.
* **Laya threat gauge** — a `canvas` that paints 50 segments (green below 0.5, amber to the gate,
  red above), a needle at the live score and a hairline on the router's own **0.80 gate**. It follows
  the *latest call by any agent*. No reading paints an empty track — never a green 0.0, which would
  read as clearance.
* **Unified memory** — the ring shows what `OmlxState` actually reports: the models the oMLX server
  knows, the other residents of its pool, and the free space. It does **not** invent an OS/LanceDB
  segment, because telemetry carries no such split. **Unload** / **Unload + Ollama** sit in System and
  the OOM card as well, offered even when no telemetry has arrived (an out-of-memory emergency is the
  worst moment to hide the control). Below 6 GB free it appears in *Needs you*, unasked.
* **Controls act on the agent whose values are on screen**, and are offered only when they can act:
  offline, Stop is not drawn rather than drawn dead. The dial only ever **tightens** the router's 0.80
  gate, and it is clamped in the bridge as well as the child. Stopping an agent stops what it spawned
  ("Stop + sub-agents"). The global **E-Stop** isolates every agent of the session — it **refuses, it
  does not kill**: pods are agents inside the daemon's process, so there is no sandbox to signal (see
  `THREAT_MODEL.md`).
* **Diffs** are listed in Outputs (never forced open); click one, or press `d`. The viewer is
  virtualised: a 10,000-line diff opens in a few milliseconds and redraws in about one.

**Autonomy.** *Plan*: agents think and propose, nothing runs. *Build*: agents work; risky steps ask.
*Auto*: agents work without asking — routine calls only. The selector is the session's own
`session/set_mode`, the same switch as the TUI's Tab; the window reports the mode the daemon says is
in force (so a change made from the TUI shows up here). **Auto never means "approve anything"** —
see [Laya Guard](agentic-harness.md#laya-guard-every-call-by-every-agent). Switching to it is flagged in
the activity feed; the audit log records who changed the mode and to what.

**The proactive observer** (`harness-observer`) watches the workspace with `notify`, parses each
saved file with tree-sitter (Rust, Python, TypeScript/TSX, JavaScript) and sends the daemon *what
changed*, at the granularity of the **symbol** — a function, a method, a class, an `impl` — not the
file. A symbol carries its own text with the symbols nested inside it cut out, which is what keeps a
one-line change to a method from also reporting its class and its module as modified. It is
optional in both directions: a workspace that cannot be watched costs a line in the header, not a
failure to start.

**Keys** (System tab → keyboard lists the same):

| key | does |
|---|---|
| `/` (or `⌘K`) | steer — the deck's input, or the focused agent: type, `enter` sends, `esc` gives the keyboard back |
| `m` | autonomy in the deck: plan → build → auto |
| `y` / `n` / `k` | answer the review on screen: approve · reject · reject and isolate |
| `r` | review the next approval waiting in *Needs you* |
| `↑` `↓` / `tab` | select a pod · switch session |
| `1`–`4` | `reasoning_effort` low/medium/high/max |
| `[` `]` | the selected pod's Laya dial — drag it, or nudge (only ever tightens) |
| `d` / `esc` | open the newest diff · close it (on the modal, `esc` = decide later) |
| `u` / `shift-u` | [Unload memory] · the same, Ollama included |
| `cmd-q` | quit |

**What interrupts, and what doesn't.** Interruption is tiered, because a window that takes itself
hostage for every question gets muted, and a muted approval gate is no gate:

| the request | where it goes |
|---|---|
| Laya at or above the router's 0.80 gate — or armed and unscored | the **containment modal**, by itself |
| anything else the daemon asks about (an elevated reading, a tool that merely always asks) | a card in **Needs you**, in the side column: answer in place (**Approve** / **Reject**), **Review** it in full, or carry on — the rest of the window stays usable |
| `esc` / **Decide later** on the modal | *Needs you* — nothing is answered, and it does not pop back up |

The agent is suspended until someone answers, whichever surface the question is on; *Needs you*
and "Decide later" only decide whether the *window* is held. `y` / `n` / `k` answer **only the review
on screen** (never a card nobody opened, never an older question hidden behind the modal) — and while
the deck's steer input has the keyboard every key goes into the text, so typing "yes, but not that one" can
never approve anything. `r` opens the next card. The window title carries `● n approvals waiting`
either way.

**The containment modal** is the one element that may take over the window. It is a full-window scrim —
absolutely positioned, so it takes no room in the layout, and occluding, so a click on it never
reaches the controls under it — with the modal centred on it, at the highest deferred priority:
`[Approve & Execute]`, `[Reject & Discard]` and `[Reject & Isolate]`, the pod, the tool, the Laya
score on the same gauge, why it stopped, and how many secrets were masked. It is split in two: the
left pane summarises **what the change touches** — a diff read as files, hunks and added/removed
counts, or a shell command as its head program and flags — and the right pane holds the command or
diff exactly as the daemon sent it, masked three times over (child, daemon, client). The summary is
read from the payload, **not** from the repository: the lab never opens the files it is shown, so a diff
that lies about its own impact is summarised exactly as it lies, and the pane says so. The right pane
is the one that decides; the left is a reading aid that never hides it. A shell command is **wrapped**,
never cut off behind a scrollbar — an approval whose tail is hidden is an approval of something you did
not read. A request whose connection is gone is shown but cannot be answered — the agent may already
have been resumed elsewhere.

**Offline mode.** The client re-reads `daemon.json` on *every* attempt, so a window opened before the
daemon exists — or across a daemon restart that moved its port or rotated its token — shows
`OFFLINE · RETRY n` with the reason, keeps the last picture (flagged, read-only) and comes back by
itself.

## What is and isn't true

* **"Blur" is a scrim.** GPUI 0.2.2 has no element-level backdrop filter, so the containment modal
  dims the UI behind it rather than blurring it. The intent — the modal is un-ignorable — is met;
  the pixels are not what the spec drew.
* **The look was checked by geometry, not by eye.** The layout tests (a populated window at the
  980×640 minimum: nothing off the right edge, the modal centred and taking no layout room, every
  control inside its column) are real; a screenshot was never taken, because capturing the screen
  needs a macOS permission that is the user's to grant. Run `make lab-demo` and look.
* **The GUI cannot act on a stale picture.** An offline daemon turns the window into a read-only
  view of how things were. Buttons that would send something still report that nothing was sent.
* **`reasoning_effort` is on the wire, its effect is the provider's.** Same caveat as
  `docs/experimental-lab.md`; the harness guarantees what it sends.
* **The gate model may not load.** This machine's oMLX cannot load one Prism id
  (`Model type prism_hadamard_qwen35 not supported`, oMLX 0.6.4), so the gate runs on the first
  registered Prism model the server *can* load; `backend/_omlx_prism_live_test.py` says so and
  `backend/_omlx_prism_smoke_test.py` covers the failover between Prism models.
* **Verified live, not only in the smoke tests.** Against the real oMLX 0.6.4 (`_omlx_prism_live_test.py`):
  the unsupported id is refused with exactly that message, a supported Prism model loads and answers
  (`'pong'`), asking for a *different* Prism id that is not resident is refused rather than silently
  reloaded, and a lease loads then unloads so the weights leave unified memory. The failure the spec
  named is real and unchanged; what the harness does about it — stay on a model that loads, fail over
  between Prism models, fall back to the cloud — is what the live run exercised.
* **E-Stop refuses; it doesn't kill.** Pods are agents inside one process. See
  `THREAT_MODEL.md`, "The Experimental Lab".
* **A stale pod status is possible.** A pod isolated mid-loop shows `ISOLATED` (the GUI prefers a
  frame's `isolated` flag over its `status`) because the child only learns on its next loop turn.
* **The window is untested against a real display in CI.** The tests below drive a headless GPUI
  test context; nothing here asserts what the window looks like on a screen. `make lab-live` does
  run the *client* against the real Python bridge (no display needed).

## Tests

All offline unless marked live.

| file | covers |
|---|---|
| `backend/_claude_code_smoke_test.py` | the CLI tool: headless flags, event-stream parsing, LanceDB priming, vault-or-refuse, allowlisted env, Seatbelt |
| `backend/_ollama_cloud_smoke_test.py` | registry, request shape, endpoint guard, `reasoning_effort` low→max, role budgets, router, picker |
| `backend/_omlx_prism_smoke_test.py` | the Prism registry, `EndpointGuard`, residency check (`ModelNotLoaded`), leases, failover between Prism models, cloud fallback |
| `backend/_memory_budget_smoke_test.py` | the ceiling/reserve/budget arithmetic, LRU eviction, refusal when a request cannot fit |
| `backend/_grpc_bridge_smoke_test.py` | the bridge: token gate, every RPC, latest-wins telemetry, capped ingest, audit rows, first-answer-wins |
| `backend/_meta_harness_acp_smoke_test.py` | the same through the real ACP server |
| `backend/_gui_controls_acp_smoke_test.py` | a GUI knob / approval reaching the child's control surface |
| `backend/_laya_call_guard_smoke_test.py` | **Laya Guard**: a table of commands, paths, patches and tasks and the score each gets; Laya's reading can only raise a score, only at 0.8 confidence, never loads weights, never lowers a rule; in `auto` a destructive call asks — from the orchestrator *and* from a specialist — each agent's dial tightens its own gate, `laya.callGuard: off` restores the old behaviour, what auto let through is in the ledger; sub-agents are denied `apply_patch` and MCP tools |
| `backend/_secret_patterns_smoke_test.py` | the masking the modal's third layer relies on |
| `lab/` `cargo test --workspace` | the client (reconnect, offline, a window opened before the daemon, inbox conflation, diff fetch), the observer (symbol extraction, real filesystem watching), the view (headless render, the modal overlays instead of taking layout room, a populated window fits at the minimum size, a 10,000-line diff opens and redraws quickly, `esc` closes a diff and not an approval, the unload path) |
| `backend/_lab_gui_live_smoke_test.py` (`make lab-live`) | the Rust client against the **real Python bridge** and the demo daemon: the first frame, knobs (applied, refused inline, clamped), **autonomy** (build → auto → build; an invalid mode is a refusal reply), **steering** (a directive queued, then read by the agent before its next call), **stop / release** (and refusal while stopped), a masked destructive request rejected + isolated, a diff request approved, a 10,000-line diff fetched, **1,000 back-to-back updates** (inbox backlog, drain and apply times on the "UI thread"), the real observer (save → tree-sitter → gRPC → `state.db`), the unload — then the audit rows are read back |
| `backend/_e2e_lab_smoke_test.py`, `_e2e_lab_tui_smoke_test.py`, `_daemon_smoke_test.py` | Phase 5 and the daemon surfaces the bridge sits beside |
| `backend/_omlx_prism_live_test.py`, `_ollama_cloud_live_test.py`, `_e2e_lab_live_test.py` | **live**: the real daemon, real models, real Laya; asserts nothing was pulled |

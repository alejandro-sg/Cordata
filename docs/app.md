# Cordata.app — the native macOS workspace

A Rust/GPUI app that is the front door to the harness (Strands → ACP server → daemon → orchestrator over Claude Code,
Antigravity and Ollama). Agent-first with a light editor; personal use, signed locally; the Blade Runner 2049 look.
It takes Claude Desktop's sessions, approvals and isolation; Antigravity's Mission Control, artifacts and
trajectories; Ollama's model manager; Gemini's live voice and screen context; Zed's GPU-native speed and ACP interop.

## Status

| Phase | What | State |
|---|---|---|
| **B0** | ten backend defects a GUI exposes (§6.0) | **done** — `backend/_backend_hardening_smoke_test.py` |
| **P0b** | GPUI dependency gate (§3 D1) | **done — GO on option A** — `app/spike/GATE.md` |
| **P0c** | `app/` workspace, `cordata-view` extraction, `cordata-theme`, window shell, packaging, CI | **done** |
| **B1** | session/transcript surface (§6.1) | **done** — `backend/_session_surface_smoke_test.py` |
| **P1** | chat parity (§4) | **core done** — see the parity table; remaining: search, the HUD and diff-review views (P3), worktree badges (B3) |
| P0a | register Cordata as an ACP agent server in Zed | waiting on the owner's call (`disable_ai: true` in their Zed config) |
| **B2** | orchestrator control (§6.2) | **done** — `backend/_orchestrator_control_smoke_test.py`; `orchestrator/control.py`, detached runs, control file, `GET/POST /orchestrator/runs…` |
| **P2** | Mission Control | **done** — runs list, a run's steps / pods (pool gauges with soft/hard lines, live activity) / budget / handoffs / artifacts, pause · resume · cancel, start a run (`--run GOAL` too). Verified live with the scripted orchestrator through a real daemon; a run on real Claude Code/agy/Ollama workers was not made (it spends shared quota) |
| **B3** | daemon surface (§6.3) | **done** — `backend/_daemon_surface_smoke_test.py`; `GET /projects` (git worktrees, trust, live, waiting approvals), `GET /history`, `/ws?role=observer`, approval options + `option_id` over gRPC, atomic `daemon.json`. Decision: worktrees stay separate projects for trust and checkpoints (safety by realpath); grouping learning stores by repository is deferred |
| **B4** | settings & trust (§6.4) | **done** — `backend/_settings_trust_smoke_test.py`; `app.*` (both schemas, user file only, parity-tested), `_cordata/settings/{get,schema,set,rules/add,rules/remove}`, `_cordata/trust/{status,grant}` (grant needs `confirmed`), `_cordata/doctor`, `_cordata/auto/readiness`, `auto`/`edit` need `_meta.cordata.confirmed` from clients that declare `confirmModes` |
| **P3** | light editor & diff review (§3 D4) | **done** — dock tabs (Mission · Files · Editor · Review, wider for Editor/Review), per-hunk Keep/Revert review pane (`review.rs`), file tree and editor tabs (`editor.rs`), ⌘1–4, ⌘P, ⌘S, ⌘W closing editor tabs, trust-gated diagnostics on unsaved text (`_editor_surface_smoke_test.py` in CI), disk reload, Open in Zed |
| P4–P7 | §9 | not started |

**Run it.** `make app-run` (the daemon's hub; `ARGS="--transport stdio"` for a private child from the checkout).
`make app` builds, signs (`SIGN_IDENTITY` optional) and self-tests `app/target/Cordata.app`; `INSTALL=1` copies it to
`~/Applications`. `make app-test` runs the shared state, the palette and the app's tests; `make app-selftest` drives
one scripted turn through the real window; `make app-check` is clippy, `cargo deny` and the gpui snapshot proof.

**Keys — parity with the terminal** (`tui/README.md`):

| terminal | app | |
|---|---|---|
| Enter · Ctrl+J newline | Enter | Shift+Enter newline — later (multi-line composer) |
| `a`/`y` · `r`/`n`/Esc · `s` · `w` on an approval | same keys while the composer is empty; or the card's buttons | plan card: `b` build · `e` edit; question: the numbered buttons |
| Esc / Ctrl+C cancel | Esc · ⌘. | Esc first closes a menu |
| ↑/↓ prompt history | ↑/↓ (one-line box) | |
| Tab / Shift+Tab mode | Shift+Tab, or click the pill | skips `auto` (a plan-card answer or `/mode auto`) |
| `/` and `@` menus, Tab completes | same | ↑/↓ choose · Tab complete · Enter run · Esc close |
| Ctrl+K deck | ⌘K / Ctrl+K | the command menu (every command and custom command) |
| Ctrl+O model picker | click the model in the header | Ctrl+T temperature: `/temp` |
| Ctrl+R / Ctrl+E expand reasoning / tools | Ctrl+R / Ctrl+E, or click a block | |
| `?` / F1 help | F1 | |
| Ctrl+N / Alt+N / Ctrl+←→ sessions | ⌘N · ⌘W · ⌘[ ⌘] · the rail | |
| F3 HUD · F4 diff review · F2 Lab | the dock (Mission Control · Files · Editor · Review) | ⌘1–4 dock tabs; ⌘P files, ⌘S save, ⌘W closes editor tab |

**Crates** (`app/`):

* `cordata-view` — the front-end state the terminal and the app share: the conversation reducer (`App::on_event`),
  slash commands, input, menus, `@` mentions, `!` shell, custom commands, the settings schema and editor, the Lab and
  run board, neutral keys (`key.rs`, crossterm-shaped; `From` conversions behind the `crossterm` feature), overlays as
  a trait (`overlay::View`, the terminal installs its own), the secret-masking table (`redact.rs`) and `shown()`.
  Moved verbatim from acp-tui; acp-tui re-exports each module at its old path.
* `cordata-theme` — colour roles (`NIGHT`, `NIGHT_HIGH_CONTRAST`) with WCAG tests, mode and risk roles, HUD vocabulary.
  The terminal's palette reads it.
* `cordata-app` — the window: `session.rs` (acp-core on a tokio runtime → the reducer → effects), `workspace.rs` (rail,
  transcript, approval card, composer, gauges, dock), `transcript.rs`, `hud.rs`, `theme.rs` (the component theme from
  the roles), `platform.rs` (Liquid Glass, accessibility display flags).

**Deviations from the plan, and why.**

* The shared **markdown AST** (§3 D3 cut 4) was not built: gpui-component renders markdown natively, so the app has no
  consumer for one. The terminal keeps its renderer.
* The TUI-only fields (`committed*`, `spinner`) stayed in `cordata_view::App`: plain data, harmless to the app, and
  moving them would have touched every renderer for no behaviour change.
* `App::answer(decision)` and `App::submit_text(text)` were added so a front end with buttons and its own text box
  drives the same reducer the keys do.
* The TUI's dim text `#606C7A` failed WCAG AA on the ground; the shared palette raises it to `#7D8A99`.

---

## 1. Decisions & environment

Agent-first + light editor; personal use, no notarization or auto-update. macOS 26, Apple Silicon, Command Line Tools
only (GPUI builds with `runtime_shaders`). Repo MIT; Zed's editor crates are GPL and are never copied; gpui-pre and
gpui-component are Apache-2.0. `app/` is Core tier (default CI); `lab/` stays frozen and what the app reuses from it
is lifted, not depended on.

## 2. Evidence from the reference apps

Static reverse engineering of Claude, Antigravity, Ollama, Gemini and Zed (nothing launched). Consequences: per-
capability helper processes under Seatbelt and a UI stall sampler (Claude); runs as shareable trajectories, MCP and
workflow templates (Antigravity); a model manager and Claude Code on Ollama models (Ollama); live voice/screen
context and an MCP tool-attribution header on every tool card (Gemini); ACP interop, per-hunk agent-edit review, edit
prediction on local models (Zed).

## 3. Technology decisions

### D1 — GPUI dependency gate
Option A — `gpui-pre =0.3.9` (zed@93d7456) + `gpui-component =0.7.3` — passed (`app/spike/GATE.md`). Controls: exact
pins, committed lock, `app/scripts/verify-gpui-snapshot.sh` (every shipped file of the 14 gpui-pre crates vs Zed;
only three labelled facade edits differ), `app/deny.toml`, no network stack in the graph.

### D2 — Process model
The UI thread only draws. acp-core and the daemon client run on a tokio runtime; their events reach GPUI through
channels a task drains. One ACP child per session (the backend chdirs per process); one live writer per conversation
(the `sessions` lease, B0). Later: capture and voice helpers as separate TCC-disclaimed processes; language servers
only in trusted folders; a UI stall sampler.

### D3 — Shared crates
`cordata-view` and `cordata-theme` (above). acp-core is reused as-is; a Finder-launched bundle reaches the harness
through `~/.cordata/daemon.json` (Hub transport), not by walking up to a checkout. Channel per feature: ACP for the
transcript, tool cards and approvals; gRPC for telemetry, the conversation list and fan-out.

### D4 — Editor (light)
gpui-component's editor (tree-sitter, LSP diagnostics/hover/definition, large files): tabs, tree, find, git gutter,
per-hunk agent-edit review, "Open in Zed". Language servers only in trusted folders; never auto-installed.

### D5 — Terminal
`alacritty_terminal` + `portable-pty` with our own element (Zed's terminal crates are GPL).

### D6 — Signing
Ad-hoc by default; `SIGN_IDENTITY` for a self-signed "Cordata Local" certificate so privacy grants survive rebuilds.
Hardened runtime, empty entitlements, `scripts/package-app.sh`, `app/scripts/check-bundle.sh`.

## 4. UX

* **Left rail** — projects → sessions (worktree badge, running / needs-you / done), full-text search, a "needs you"
  inbox across sessions.
* **Centre** — transcript + composer. Blocks: user, assistant markdown, thought, tool card (status, MCP attribution,
  input, output), diff (per-hunk review), plan card, ask card, approval card (exact command/diff, Laya band, "always"
  rules, reject-with-reason), orchestrator run card, goal/checklist strip, context gauge, mode pill (plan green, build
  amber, edit gold, auto red), model and effort pickers. Composer: `/` `@` `!`, queue, paste image, drag files, voice.
* **Right dock** — Mission Control (default while a run exists), Artifacts, Editor, Terminal, Diff Review, Browser,
  Models, Memory/Skills.
* **Mission Control** — wave timeline; a pod per worker (role, status, tokens, $, pool gauge with soft/hard lines,
  handoffs); plan.md / walkthrough.md; conformance verdict; pause / resume / cancel at safe points; replay.
* ⌘K deck, HUD, diff review as overlays (the terminal's keys); menu-bar extra and quick-ask hotkey; `cordata://`
  deep links; `cordata .`.

### 4b. Design system
One token source (`cordata-theme`): sodium amber accent, electric teal for agents and untrusted content, gold,
green, red, steel on near-black. Pills `▐ PLAN ▌`, segbars `▰▰▱▱`, `⌜ C O R D A T A ⌝`, hairline frames. Atmosphere
within GPUI's means: two-stop gradient haze, glass via `NSGlassEffectView` behind a transparent window (the rail is
translucent over it). Reduce Motion / Reduce Transparency turn the atmosphere off; Increase Contrast swaps the palette;
colour is never the only signal; all text WCAG AA.

## 5. Orchestrator work the UI exposes
Control surface (§6.2); a new worker lane — Claude Code on Ollama models through Ollama's Anthropic-compatible
endpoint — gated by a bake-off; `agy` options (`--sandbox` as defence in depth inside Seatbelt, `--effort`,
`--json-schema`); trajectory export/replay; scheduled runs later.

## 6. Backend change spec

### 6.0 Pre-GUI defects (B0 — done)
1. busy guard on `session/prompt` (refused before any side effect); 2. ext methods resolve daemon ids (`_known`);
3. conversations in `state.db`'s `sessions` table (no lost writes, paged `session/list`, absolute cwd required);
4. Stop during a goal's judge ends the goal; 5. a cancelled orchestrator run is `cancelled` (and `_keep_discarded` is
defined once); 6. a turn's diff is published as `_cordata/diff` (masked); 7. approvals stranded by a dead process are
announced once and expired, never for a live session; 8. `quota_usage.role/.worker` in the schema; 9. one live writer
per conversation (`host_pid` lease); 10. `CORDATA_SESSIONS_DIR`.

### 6.1 Session & transcript (B1)
Native ACP `plan` and `usage_update`; replay of tool calls and diffs on `session/load` (`toolUse`↔`toolResult` from
the snapshot, diffs from inputs and checkpoints); `_cordata/session/history` paging without an agent; fork from a turn;
image prompt blocks built server-side (only for vision-capable models); mode/config changes echoed to other doors.

### 6.2 Orchestrator control (B2)
`orchestrator/control.py` (list/show/artifacts, listing-bound and realpath-confined reads); `_cordata/orchestrator/
{status,runs,show,start,pause,resume,cancel}`; `start` applies the gates itself (plan mode, vault fail-closed,
`allowDirect` trust); runs as detached processes owned by a `RunSupervisor` (NDJSON events tee'd per run, `host.json`,
dead pid ⇒ `interrupted`); per-pod `worker` events and `usage` with pods/pools/envelope; pause/cancel at safe points.

### 6.3 Daemon (B3)
`GET /projects` (git worktrees), `GET /history` from the `sessions` table, an observer role on `/ws`, approval
options in the gRPC proto, the `daemon.json` write race.

### 6.4 Settings & trust (B4)
`app.*` settings group (both schemas, parity test, user-file only); `_cordata/settings/*`, `_cordata/trust/*`,
`_cordata/doctor`, `_cordata/auto/readiness`; `auto`/`edit` require confirmation from clients that declare it.

### 6.5 Client side
acp-core: handle `plan`/`usage_update`; parse `effort, skills, vault, preflight, preference, conversation`; type the
raw-`Value` events; an ACP cross-version contract test; MCP elicitation through `ask_user`.

## 7. Security invariants
In THREAT_MODEL.md ("The native app"): loopback + token; the GUI only tightens; first answer wins and stale approvals
are not answerable; GUI actions audited; scrub before draw; capture deny-by-default; project MCP servers and LSPs only
after trust; vault egress fail-closed; supply-chain controls; deep links navigate only; token and vault-token handling.

## 8. Reliability & performance budgets
Stale banner within 1 s of a daemon drop, keep state, reconnect with backoff; cold start < 800 ms; scroll 10k blocks at
refresh rate (the gate measured p99 9.3 ms at 120 Hz); typing < 16 ms; idle CPU < 1 %; RSS < 400 MB with 5 sessions;
a UI stall > 200 ms writes a diagnostic. 4-hour soak against a scripted agent.

## 9. Phases
B0 ∥ P0b/P0c (done) → B1 → P1 (chat parity, 3 wk) → B2 → P2 (Mission Control, 2 wk) ≈ a daily driver; then B3/B4,
P3 (light editor), P4 (terminal + browser agent), P5 (system: menu bar, hotkey, capture, notifications, LaunchAgent,
deep links), P6 (models + voice: local-first speech, opt-in Gemini Live), P7 (hardening, soak, perf, REA of our own
bundle). Cut lines after P2, P3, P5; P4 and P6 are independently droppable.

## 10–15
Execution: each phase a branch; B-phases are good first customers for `cordata run`. Risks: gpui-pre churn (pins,
snapshot proof, fallback to official gpui), accessibility gaps (measured by hand with VoiceOver), extraction
regressions (mechanical moves, every suite green at each step), quota burn during self-build. Out of scope for v1:
notarization, a full editor, Finder/Share extensions, Windows/Linux, collaboration, cloud sync.

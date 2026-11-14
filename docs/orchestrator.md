# The orchestrator

`cordata run "goal"` hands a coding task to real coding CLIs (Claude Code `claude`, Antigravity `agy`) and moves the work
between them when a quota runs low. A cheap model researches and plans, a strong one implements, a different model
family reviews, and the planner checks the result against its own plan.

```text
 goal ─► RESEARCH ─► PLAN ─► EXECUTE ─► HARDEN ─► AUDIT ─► CONFORMANCE ─┬─► done
                                waves of    other family    tier 1:      ├─► gaps: fix pass, then HARDEN ► AUDIT ► CONFORMANCE
                                independent reviews         deterministic│      (up to --max-rounds, default 3)
                                steps                                    └─► replan (once), or fail with rollback
```

## Using it
<!-- covers: tui/crates/cordata-run/src/main.rs, tui/crates/cordata-run/src/cli.rs, tui/crates/cordata-run/src/receipt.rs @ b3a342a -->

```bash
cordata run "Add retries to the HTTP client and test them" --cwd ~/proj --allow-direct
cordata status            # quota left in each pool, recent runs and handoffs; spends nothing
cordata show <run_id>     # plan, history, audit, verdicts, usage per role and worker
cordata resume <run_id>   # continue a paused or interrupted run (refused if your settings now turn off a safety guard
                          # the run started with — harden, tier2, screen, conformance, isolate_waves —
                          # unless you pass --allow-weaker-guards; other changed settings are recorded as a warning)
```

`--allow-direct` is needed unless an Agent Vault proxy is configured (`vault.enabled`, see
[`settings.md`](settings.md)). Cordata does not ship that proxy; without it the coding CLIs reach the network directly,
still inside the Seatbelt sandbox described in [`THREAT_MODEL.md`](../THREAT_MODEL.md). `CORDATA_VAULT_DIRECT=1` is the
environment form of the flag.

`cordata show <run_id> --step <id>` prints that step's tape, the scrubbed, size-capped event log each worker attempt writes
to `~/.cordata/orchestrator/<run_id>/steps/<step>-<attempt>.ndjson`; add `--attempt N` for an earlier attempt and
`--follow` to tail it while the step runs. `show <run_id> --timeline` draws the run as a waterfall of spans, and
`cordata export <run_id> --otlp` writes the run as OpenTelemetry GenAI spans (a local file; Cordata sends nothing).

`run --land review` finishes the run in `to_review` instead of `done`, with the work left in the project for you to read
(the default, `--land all`, is the old behavior). `run --detach` prints the run id and starts the run in its own session (`setsid`), with console output in
`~/.cordata/orchestrator/<run_id>/run.log`, so it survives the terminal that launched it. `caffeinate` keeps the Mac awake
while it runs, `cordata attach <run_id>` follows the log until the run stops, and a macOS notification says when it
finishes, needs you (`paused`, `usage_low`), is ready to review or stops. The notification names only the run id and
state. `cordata resume` picks up a run whose process is gone. Keep Antigravity to attended runs: Google does not support
unattended background use on consumer plans. `--session-id` ties a run
to the chat thread that started it (`/run` in the chat passes it).

`cordata evolve [repomap|economizer|guard|custom] [--threshold N] [--dry-run] [--mutate CMD | --propose [TEXT]]` is a
self-improvement cycle for Cordata's own subsystems: a candidate change is made in a copy-on-write sandbox and harvested
only if it passes four gates (it compiles, the hermetic tests pass, the evaluator files are untouched, and a fitness
score improves by at least the threshold).

`run` also takes `--workers claude_code,antigravity,ollama` (`ollama` is Ollama Cloud through Cordata's own engine and needs a key rather than a CLI), `--run-id ID`, `--max-usd N`, `--max-rounds N`, `--max-parallel N` (default 3),
`--no-research`, `--no-harden`, `--no-tier2`, `--no-step-check`, `--no-screen`, `--no-conformance` and `--json`. With no installed worker, `run` and
`resume` exit 78 before doing anything. Ctrl+C stops the run where it is and records it `cancelled`. Every run has a control file, `~/.cordata/orchestrator/<run_id>/control.ndjson`: a line `{"cmd":"pause"}` pauses the run at its next safe point (never mid-call), and the window's Hold button writes exactly that. `cordata resume <run_id> --direction "…"` adds your words to the next unfinished step's task and notes it on the run; `--workers` picks who continues. Every run writes
`~/.cordata/orchestrator/<run_id>/plan.md` and `walkthrough.md`. Every run (and resume) also writes `receipt.json` there:
criteria results, diffstat, cost, wall time and audit findings, and prints a one-line summary (not with `--json`). Its
`logic` list is the run read as a logic diff: for each plan step, the behavior it was to bring and why (`behavior`,
`because`, which the planner writes in plain words; a step without them falls back to its description), how the step
ended, and the changed files inside its scope. It comes from the plan and the checkpoint store, never from a model.

`run --review-plan` (or `orchestrator.reviewPlan`) stops after the first plan is written: the run is `paused`, no worker
has built anything, and the plan with each step's expected behavior is printed and saved as `plan.md`. Read it, then
`cordata resume <run_id>` builds it; to change course, cancel and run again with a better goal. A replan after a failed
audit is held the same way, so every plan is read before it is built. The window shows such a run as "plan awaits you",
with each step's expected behavior under its name and an Approve and build button in place of Resume.

Set `orchestrator.pools` to your real plans so handoffs happen at the right moment. The defaults are conservative caps:

```json
[
  {"pool_id": "claude_code", "family": "claude_code", "unit": "usd", "window_limit": 20, "weekly_limit": 150},
  {"pool_id": "antigravity", "family": "antigravity", "unit": "tokens", "window_limit": 8000000}
]
```

Default pools your list does not name are added after it (the run prints a warning, and `cordata status` lists them),
so what your list leaves out runs under the default caps, not uncapped. To keep a family out of a run, pass
`--workers`. Each step of a parallel wave reserves its share of the run's `--max-usd` and of each pool's headroom when
it starts; its retries spend from that share, and a step that spends it waits for the next wave. A CLI learns a
request's cost only after it, so every worker is given its budget less a hold for one more request: twice the costliest
request this run has made (before its first, the costliest in the ledger), at least a tenth of the budget and at most
half. Each request re-sends a longer conversation, hence the doubling. Claude Code steps also stream their token counts
(`--include-partial-messages`): Cordata prices every request as it streams, sub-agents' included, from Anthropic's list
prices per model (a version the table does not name takes its family's dearest price, an unknown family the dearest of
all; hour-long cache writes at 2x, the long-context tiers), and stops the CLI at the first update past the budget. Checked
against a real Claude Code 2.1.317 call (`tests/fixtures/claude_haiku_5_5_partial_stream.jsonl`): the estimate came
within 1% of the reported cost. What a request can still add after the line is its input (known when it starts) and the
output of one update. The prices are a table in `workers/claude.rs` (from 2026-11-03); a price rise Anthropic makes later
would make the estimate low until it is updated. Antigravity and Ollama Cloud steps are metered by the hold alone.

Other `orchestrator.*` settings are in [`settings.md`](settings.md).

## How a run goes
<!-- covers: tui/crates/cordata-orchestrator/src/machine.rs, tui/crates/cordata-orchestrator/src/plan.rs @ b3a342a -->

1. **Research** (on by default). The planner, read-only, writes a brief of findings with evidence (`path:line` plus a
   quote). A deterministic check, no model, verifies each file reference; unverifiable findings are dropped, and the
   brief is refused if fewer than half survive.
2. **Plan.** Steps name the files they may write, carry acceptance criteria that need no judgement (`command`, `test`,
   `file_exists`, `file_not_changed`, `regex_match`, and `regex_absent` for text that must be gone from a file or a whole
   directory), a `complexity` and `depends_on`. A plan with an unknown dependency,
   a cycle, or no step grounded in a finding is refused.
3. **Execute.** Steps whose dependencies are done and whose write scopes are disjoint run together in copy-on-write
   sandboxes; a rollback restores only that step's scope. A step that defeats every attempt is retried with the strongest
   worker before the run replans. Before a replan or a failure restores the project, the discarded work is saved as
   `discarded-NNN.patch` beside the plan.
4. **Harden.** A worker from another model family runs the acceptance commands and fixes real failures inside the scope.
5. **Audit.** First a reviewer model (not the planner, when another worker has quota) screens every command the audit is
   about to run and scores its risk; a command at 0.8 or above is refused. Its score is combined with a pattern guard's
   by taking the higher, so the model can refuse more but never less. Commands are screened once per run. Tier 1 is
   deterministic: scope, criteria, verifier commands run with no network and no project writes. Tier 2 has the planner
   compare the result with its plan.
6. **Conformance.** The planner judges the final diff against its own brief: `accept`, `gaps` or `replan`.

After each step the sandboxed output is checked (syntax of changed Python, JSON, TOML and YAML files, and imports between
project modules; Python is parsed with tree-sitter) before it is harvested into the project.

## Quota and handoffs

Each pool has a rolling window (5 h) and a weekly window with a soft line at 80 % and a hard line at 95 %. Usage is
metered by Cordata and corrected by the provider's own readings when it reports them (Claude Code emits its five-hour
and seven-day utilisation on every run). When a pool crosses its soft line, or a provider limit hits mid-step, the step is
rolled back, a handoff packet (step, checkpoint, partial diff, failed attempts, open assumptions, scrubbed and fenced as
untrusted data) is saved, and the same step continues on another pool.

## Dispatch
<!-- covers: tui/crates/cordata-orchestrator/src/dispatch.rs, tui/crates/cordata-orchestrator/src/bandit.rs @ b3a342a -->

Hard gates come first and are never learned: the role, the CLI being installed, the pool being under its hard line, and
the reviewer coming from a family that wrote none of the change. Then the order: the cheapest competent worker per role.
With no evidence, planning and research prefer the cheapest standard-tier worker, execution and harden the strongest.
`--dispatch off|shadow|live|auto` controls learning: `shadow` records what the learned policy would have chosen and
acts as `off`; `live` acts on it. `auto` (default) starts in shadow and goes live on its own once the learned policy has
earned it: after 30 logged outcomes, trained on the older 70% and replayed on the newest 30%, it must pass as often as
the always-strongest order (within 2 points) at no more cost per success. The run's warnings say which way it went and
why; a policy that drifts (a model that got worse) falls back to shadow on the next run.

Each candidate is a model at an effort (`claude_code:opus@high`): the bandit learns model and effort together, so a
cheaper effort that passes as often wins its role. The agy models the installed CLI does not list (`agy models`) are
left out of a run, and planning tries each family's best before any family's second.

Before each step the executor checks the step's pool against what a step there usually costs (the 75th percentile of
recent steps). With no pool able to fit one, the run stops at that step boundary (`usage_low`) with each pool's room
and when it frees up, rather than being cut off mid-step; `cordata status` shows about how many steps each pool has
left.

## What the CLIs do headless

- **Claude Code** is run with `--setting-sources project --disable-slash-commands --strict-mcp-config`, because user hooks,
  plugins and skills otherwise add ~29k cache tokens before the first word. Its `total_cost_usd` is list price, notional
  on a subscription.
- **agy** cannot run commands headless: anything needing a grant is denied and listed in `denied_actions`. A write step
  that changes nothing after a refused action counts as a failure, and the prompt tells the worker it has no shell. Its
  `--mode plan` is its own planning workflow, not read-only, so plan and audit roles use the default mode. It reports
  tokens, never dollars.
- The audit sandbox denies writes to anything named `.env`, so a test fixture that writes one fails as an acceptance
  criterion; prefer `command` or `file_exists` criteria for such suites.

## The run bus (workers talking to each other)
<!-- covers: tui/crates/cordata-bus @ b3a342a -->

Parallel steps coordinate through one message bus per run (`tui/crates/cordata-bus`), modelled on herdr's socket API
but built for untrusted workers:

- The orchestrator owns a unix socket at `~/.cordata/bus/<random>/bus.sock` for the life of the run (no daemon). The
  worker Seatbelt profile still denies `~/.cordata`. It gets one `path-literal` connect allowance for its own run's
  socket, so it cannot replace that socket, bind a fake one, or reach another run's.
- Each Claude Code step gets a 256-bit token in its environment (`CORDATA_BUS_TOKEN`, never argv) and
  `--mcp-config` naming `cordata-bus mcp`. `--strict-mcp-config` keeps every other MCP server out. The bus serves a
  token only to the worker process it was issued to, or one of that process's descendants. It stamps the sender itself.
- Tools: `read` (long-poll up to 30 s), `post` (note/question/answer/finding, ≤4 KiB, scrubbed, 20/min), `status`,
  `peers`, `claim`/`release`. Claims are advisory and only allowed inside the step's own scope.
  Plan and audit steps may only read and set status.
- Read results are fenced as peer data. Peer messages cannot grant permission, widen a scope or approve anything.
  No tool can type into another worker; only the orchestrator starts workers.
- Antigravity has no per-run MCP flag. Steps run there (and any retry) get a digest of the bus in their prompt instead.
  Each wave's messages are emitted as a `bus_messages` event. `CORDATA_BUS=off` disables the bus.

## Code
<!-- covers: tui/crates/cordata-orchestrator, tui/crates/cordata-run @ b3a342a -->

`tui/crates/cordata-orchestrator`: `machine.rs` (the durable state machine in `state.db`), `research.rs`, `plan.rs`,
`executor.rs`, `audit.rs`, `handoff.rs`, `dispatch.rs` and `bandit.rs`, `synthesizer.rs`, `scope.rs`, `checkpoints.rs`,
and `workers/` (one streaming contract, adapters for `claude` and `agy`, the Seatbelt profile and the vault gate). The `ollama` worker is `tui/crates/cordata-run/src/native_worker.rs`: it drives `acp-core` and answers the engine's approvals for the step (edits inside the step's files, offline commands only for roles that change files, nothing else).
`tui/crates/cordata-run` is the command (`detach.rs`: the detached run's log, `caffeinate`, notification and `attach`); `cordata-ledger` holds the quota pools and run records.

## What a project teaches

Each run ends by adding what it taught to `~/.cordata/knowledge/<project key>.jsonl` (Antigravity's knowledge items in
miniature): a worker that gave up on refused commands, models the installed agy does not offer, the check commands that
passed on a finished run, and why a run failed or stopped. Lessons are read off the run record, with no model call. A
lesson seen again counts up; at most 40 are kept. The planner and every worker get the eight most-seen as "What earlier
runs on this project learned". Delete the file to start over.

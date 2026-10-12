# The orchestrator

Research → plan → execute (waves) → harden → audit → conformance for coding tasks, across three kinds of worker —
the Claude Code CLI, Google Antigravity's `agy` CLI and Ollama models — choosing the cheapest one that does the job
and moving the work between them when a quota runs low. The design is [`orchestrator-plan.md`](orchestrator-plan.md);
this page is how it is built, how to use it, and where the build departed from the plan and why.

```text
 goal ─► RESEARCH ─► PLAN ─► EXECUTE ─► HARDEN ─► AUDIT ─► CONFORMANCE ─┬─► done
          cheapest    cites     waves of    other        Tier 1 only      ├─► gaps: a fix pass, then HARDEN ► AUDIT ► CONFORMANCE
          standard    verified  independent family        (free,           │          (≤ max_rounds, default 3)
          planner,    findings  steps, by   reviews       deterministic)   └─► replan, or past the rounds: replan (≤ 1) / fail
          read-only                complexity
          │                       │ quota soft/hard line, provider limit
          │                       ▼
          └─ the same planner ─ safe-point handoff (packet) ─► same step, another pool
             judges conformance
```

## Using it

From a session (the ACP front doors — TUI, Zed, Discord): the agent calls **`orchestrate(goal)`** for multi-step
work. It asks first (build mode), like `claude_code_engineer`; a confident Laya `team` route only hints at it.

From a shell (from any directory, once `make cli` has put `cordata` on your PATH):

```bash
cordata run "Add retries to the HTTP client and test them" --cwd ~/proj
```

```bash
cordata status
```

To run without the Agent Vault proxy in development/testing, pass `--allow-direct` or set `CORDATA_VAULT_DIRECT=1`:

```bash
cordata run "Add retries to the HTTP client" --allow-direct
```

`run` has three knobs for the research loop, as flags only (a setting would need a Rust-side schema entry):

* `--max-rounds N` — conformance fix rounds before the run replans or fails (default 3).
* `--max-parallel N` — independent steps that may run at once (default 3).
* `--no-conformance` / `--no-research` — fall back to the earlier PLAN → EXECUTE → HARDEN → AUDIT (Tier 2) loop.

`status` shows every quota pool's headroom, recent runs and handoffs — from `state.db`, spending nothing. `show
RUN_ID` prints a run's plan, history, audit, conformance verdicts, per-role and per-worker usage, and the final
state. `resume RUN_ID [--allow-direct] [--max-usd N]` continues a paused or interrupted one (a resumed run gets the
default budget unless you pass `--max-usd`); `doctor` checks the CLIs, services and settings this depends on; `replay`
judges the learned dispatch policy (below). The TUI's `/status` and Discord's `!status` render the same pools (zero
model tokens).
Every run also writes `~/.cordata/orchestrator/<run_id>/plan.md` and `walkthrough.md` (what each step did, who did
it, the audit, the recovery) — Antigravity's artifact model, rendered from the record rather than written by a model.

Settings (`orchestrator.*`, all trust-gated): see [`settings.md`](settings.md). The pools default to conservative
caps — set `orchestrator.pools` to your plans:

```json
[
  {"pool_id": "claude_code", "family": "claude_code", "unit": "usd", "window_limit": 20, "weekly_limit": 150},
  {"pool_id": "antigravity_claude", "family": "antigravity", "model_regex": "claude-.*", "unit": "tokens", "window_limit": 3000000},
  {"pool_id": "antigravity", "family": "antigravity", "unit": "tokens", "window_limit": 8000000},
  {"pool_id": "ollama_cloud", "family": "ollama", "unit": "requests", "window_limit": 600, "window_seconds": 3600}
]
```

## The pieces

| module | what it does |
|---|---|
| `backend/workers/` | One streaming contract (`run`/`resume`/`cancel`/`quota_snapshot` → events → a typed `WorkerResult`) and three adapters. The two CLIs share `cli_worker.py`: vault-only egress, allowlisted env, a Seatbelt profile that denies every write in the project outside the step's file scope, budgets enforced mid-stream, provider limits recognised. Conformance: `_worker_conformance_smoke_test.py`. |
| `orchestrator/research.py` | RESEARCH: asks the planner pool for a short brief, then calls grounded tools (`read_file`, `search`, `list_files`) to verify every claim. The brief is discarded if any claim cannot be anchored to a project file. |
| `orchestrator/plan_artifact.py` | The plan contract: each step names its files; every criterion is checkable without judgement (`command`/`test`, `file_exists`, `file_not_changed`, `regex_match`). A draft that cannot be audited is refused, not repaired. |
| `orchestrator/quota_ledger.py` | Per-plan envelope plus rolling pool windows (5 h, weekly), soft 80 % / hard 95 % lines. Self-metered, and corrected by the provider's own readings (below). Now records `role` and `worker` on every usage row for per-role/per-worker attribution. |
| `orchestrator/handoff.py` | The packet one worker leaves the next: step, checkpoint, partial diff, failed attempts, open assumptions — scrubbed, bounded, persisted, fenced as untrusted data. |
| `orchestrator/executor.py` | EXECUTE: complexity routing, parallel waves of independent steps, checkpoints, per-step budgets (the smaller of the envelope's and the pool's room), rollback or keep-partial on a stop, handoff to another pool, pause when none is left. |
| `orchestrator/audit.py` | Tier 1 (free): scope, criteria, verifiers — model-written commands scored by Laya Guard, run with no network and no writes but caches. Tier 2: a reviewer's accept / drift / replan. |
| `orchestrator/machine.py` | The durable state machine (`orch_runs` in `state.db`) with `research`, `plan`, `execute`, `harden`, `audit`, `conformance` states; bounded retries, cross-family harden, restore on failure, resume. |
| `orchestrator/dispatch.py` | Who does what: hard gates, then the cheapest competent tier (learned), shadow mode, replay. Also exposes a role-specific prior order for `research`/`plan`, `execute`, `harden`, `audit` and `conformance`. |
| `orchestrator/evalsuite.py`, `sim.py` | The orchestration eval suite and the simulated providers it and replay are judged on. |
| `orchestrate_tool.py` | The session tool. |

## The research loop

**Who does what.** The cheapest *competent* planner does the thinking — by default the cheapest standard-tier worker,
not the strongest (`dispatch.ROLE_PRIOR`: plan · research · conformance → standard, fast, strong; audit → standard,
strong, fast; execute and harden → strong first, harden always from another model family). Before this, "no evidence
yet" meant strongest-first for *every* role, so a plan cost as much as the implementation. Learned order in `live` mode
is unchanged and still overrides the prior.

1. **RESEARCH** (`research.py`, on by default, `--no-research` to skip). The planner, read-only, writes a brief:
   findings with evidence (`file` = `path:line` + a quote, `doc`, `command`). A *deterministic* check — no model —
   verifies each `file` reference: the file is inside the project, the line exists, the quote occurs within 3 lines. Findings
   with no verifiable evidence are dropped; if fewer than half survive the brief is refused and asked for again once.
2. **PLAN** — the same worker plans from the verified brief when it can. Steps may carry `grounded_in` (finding ids),
   `complexity` (`low` · `medium` · `high`) and `depends_on`; the plan is refused for an unknown finding or dependency,
   a dependency cycle, or when no step cites a finding.
3. **EXECUTE** — steps whose dependencies are done and whose write scopes are disjoint run together (`--max-parallel`);
   a rollback inside a wave restores only that step's own scope. A `high` step takes the strongest usable worker, a
   `low` one the cheapest. A step that defeats every attempt is retried up to `max_step_recoveries` (2) times —
   strongest worker first, told to work in small edits — **without** discarding the steps before it.
4. **HARDEN** — a worker from another model family runs the acceptance commands, fixes real failures inside the scope,
   closes gaps against the brief, and does not widen the scope.
5. **AUDIT** — Tier 1 (deterministic) only, when conformance is on.
6. **CONFORMANCE** (`audit.conformance`) — the planner that did the research reads the final diff and the Tier-1 results
   against its own brief and plan and answers `accept` · `gaps` · `replan`. Gaps go to a fix pass by the execute role and
   round again, at most `max_rounds` times; then the existing replan/fail with rollback applies. If the planner has no
   quota, the next-cheapest standard-tier worker stands in and the record says so.

**Usage.** `QuotaLedger.run_usage(run_id)` reports calls, tokens and cost per (role, worker); the machine emits a `usage`
event after every worker call carrying running totals and each pool's percentage; the totals are stored on the run and
shown by `show` and in `walkthrough.md`.

**A restore never loses work silently.** Before a replan or a failure restores the project to the start of the run, the
discarded changes are saved as `discarded-NNN.patch` beside the run's plan (and the record says where).

**What is and is not evidenced.** The loop is tested end to end with fake workers (`_orchestrator_loop_smoke_test.py`:
role priors, the grounding check on fabricated lines, plan refusals, the conformance loop and its bound, planner
fallback, parallel waves and rollback isolation, usage totals). It was *built* by this orchestrator on real workers
(Claude Code, Antigravity, Ollama), which exercised quota handoffs and the recovery path for real. The research and
conformance phases have run live once, on an Ollama-only roster against a small repo: kimi-k2.7-code researched (four
findings, one dropped by the grounding check), planned, repaired a failed regex criterion and judged conformance;
deepseek-v4.1-flash executed the three `low` steps; the run finished `done` in 27 calls and 67k tokens at $0. That is
one small task, not an evaluation of the loop.

## What the CLIs actually do (observed, not documented)

The plan's REA rule was "observable behaviour first". What observing showed, and what the adapters do about it:

* **Claude Code** (`claude -p --output-format stream-json`): user hooks, plugins and skills load into a headless
  run — ~29k cache-creation tokens before the first word. The worker runs with `--setting-sources project
  --disable-slash-commands --strict-mcp-config`. `--bare` would go further but reads no OAuth credential, so a
  subscription user cannot use it. `--json-schema` makes the CLI call a `StructuredOutput` tool; the result carries
  `structured_output`. Its `total_cost_usd` is list price (`costBasis: list`) — notional on a subscription.
* **Claude Code reports its real quota.** Every run emits `rate_limit_event` with `unifiedWindows.five_hour` and
  `seven_day` utilisation and reset times. The ledger stores these (`quota_signals`) and uses them as the pool's
  level whenever they are higher than the meter — the provider sees usage from the web app or another machine
  that the meter cannot.
* **agy**: the prompt goes on stdin as `{"event": "user", "message": {"content": …}}` with `-p=` (a bare `-p`
  swallows the next flag as the prompt). Events are `init`, `step_update` (`user_input`, `agent_response` with
  `text_delta`, `tool`, `system_message`) and `result`. Headless mode auto-denies anything that needs a grant —
  commands always, edits unless `--mode accept-edits` — and lists them in `denied_actions`. **`--mode plan` is not
  a read-only mode**: it is agy's own planning workflow (writes a `plan.md` artifact under `~/.gemini` and asks for
  its approval before doing the task), so plan/audit roles use the default mode instead — the plan document had
  proposed `--mode plan`. `--json-schema` is enforced through its `finish` tool (`structured_output`, plus that
  tool's `toolAction`/`toolSummary`, dropped); schema properties named `function` or `args` collide with the tool's
  envelope. agy reports tokens, never dollars. Its model list (`agy models`) on this machine: Gemini 3.6–3.8 Flash,
  3.1 Pro, Claude Opus/Sonnet 5.5, GPT-OSS 120B — not the "Claude 3.5/3.7/4.x" the plan listed.

* **What the first real multi-step run taught** (building this loop with this orchestrator, on real workers):
  * agy cannot run commands headless. Asked to "run the tests first" it was refused, then *ended its turn with success
    and no edit* — two steps "completed" having changed nothing. Now the prompt tells such a worker it has no shell,
    and a write step that changes nothing after a refused action is a failure (`workers/cli_worker._finish`).
  * One step that defeated three weak workers triggered a replan, which restored the whole project to the start of the
    run and discarded nine good steps. Now: recovery passes first (strongest worker, small edits), and any restore
    saves the work it discards as a patch beside the plan.
  * Quota handoffs fired for real: Antigravity hit its token cap mid-step (rolled back, packet saved, step finished on
    Claude Code); Claude Code's own window crossed the soft line (next step went to Ollama).
  * The audit sandbox denies writes to anything named `.env`, wherever it is, so a test suite whose fixture writes one
    fails as an acceptance criterion (exit 1 in 0.3 s) while passing outside the sandbox. Prefer `command`/`file_exists`
    criteria for such suites, or give the fixture another name.

## Where Ollama sits

With the role priors, Ollama is the default for the thinking roles and for cheap work, and the CLIs take the hard
implementation: **kimi-k2.7-code** (standard tier, the cheapest standard worker) researches, plans, judges conformance
and audits; **deepseek-v4.1-flash** (fast tier) takes `low`-complexity steps, harden and audit; **glm-5.3-flash** is the
fast-tier fallback. Claude Code and Antigravity take `medium`/`high` steps and anything the Ollama workers fail.

Measured on this machine, same tasks, through `OllamaWorker`:

| model | a 250-line module | research brief over this repo |
|---|---|---|
| deepseek-v4.1-flash | 84 s · 159k tokens in · 334 lines | read 2.5M+ tokens in 4 minutes without converging — **not** used for `plan`/research |
| kimi-k2.7-code | 127 s · 299k in · 413 lines | 86 s · 840k in · 5 findings |
| glm-5.3-flash | 374 s · 1.3M in · 320 lines | timed out at 300 s |

The roster encodes that: deepseek has no `plan` role, and is priced below glm because it was cheaper in effect (8× fewer
tokens for the same file). A second cheap worker also helps a cheap-only policy: in the simulation the always-cheapest
policy passes 0.91–1.0 of hard tasks with it and 0.73–0.91 without, which is why the promotion-gate test now shows the
gate refusing a one-cheap-arm roster.

## Dispatch: the cheapest competent worker

Gates first, never learned: the role, the CLI being installed, the pool not past its hard line, and the harden
reviewer coming from a model family that wrote none of the change. Then the order:

1. **Tiers first.** Every arm of a tier (fast · standard · strong) is pooled per role and task class — the
   bandit's `tier` (and `effort`) decision points, which the in-session harness reserved and never acted on.
2. A tier is **competent** when its pooled success rate is within `z` (1.5) standard errors of the difference from
   the best tier's (at least one point). The competent arms go first, cheapest first (plus quota scarcity).
3. An arm's own estimate is shrunk toward its tier's, so two equals are not separated by luck (the winner's curse
   an earlier per-arm version fell into: it picked opus over an equally good, cheaper arm by noise).

With no evidence the order is strongest-first. `shadow` (the default) computes the learned order, logs it as a
`bandit` decide event and acts as `off`; each run's final verdict is written as an `orch_outcome` event. The policy
goes `live` only through `python -m promote dispatch`: on dev and held-out tasks, at most one point of pass rate
lost, cheaper per success, no worse at surviving handoffs, no less plan-conformant.

**What the evidence says** (simulated providers on the eval catalog's 36 coding tasks, two explicit worlds — cheap
tiers as good on easy and medium work, or not; `_dispatch_smoke_test.py`, `_orchestration_eval_smoke_test.py`):

* trained on 8,000 outcomes, the learned policy was never more expensive than always-strongest and stayed within
  one pass-rate point of it in both worlds (exact expected values, three seeds);
* where cheaper tiers are as good it cut cost per success by 47–94 %; by replay on held-out logs, 0.954 vs 0.945
  pass rate at 0.34 vs 0.63 cost per success;
* end to end through the machine (one task per class), the same pass rate at half the cost per success, and the
  gate promoted it; the always-cheapest policy on hard work lost a task in nine and was held.

The claim is conditional on the world: it shows the policy finds out which world it is in and behaves accordingly,
not that cheap models are as good as strong ones on your tasks. Real outcomes (collected in `shadow`) decide that.
With few outcomes the policy stays at strongest-first by construction.

## Evidence and scope of current verification

The new loop (research, grounded planning, parallel waves, harden, audit, conformance, usage attribution) is
verified only with in-process fakes in `backend/_orchestrator_loop_smoke_test.py`. Those fakes exercise the control
flow, the grounding check, the wave scheduler, the fix/replan counters, the per-role and per-worker usage columns,
and the `show`/`status` rendering — all without real CLI calls or network egress.

What is **not** measured yet: any quality or cost gain from research or conformance on real tasks, any throughput
improvement from parallel waves with real workers, and any calibration of the complexity bands against real-world
step difficulty. Research is off in the bare `RunConfig` default and is enabled through `config.run_config()` (and
therefore the CLI and `orchestrate_tool`); `--no-research` disables it.

## Lean: what was removed

Measured from the ledger (75 turns over a week): the tools used were `shell`, `file_read`, `code_search`,
`file_search`, skill search and offload retrieval; Laya never routed to `team`; `local_graph`, `local_swarm` and the
game tools were never called. Removed: the in-process teams (`local_graph`, `local_swarm`, the Laya team pipeline —
replaced by `orchestrate`), the browser game demo (`open_game`, `/games/*`, its skill and frontend panel) and the
web UI's swarm panel. Frozen (kept, out of the default CI path): the Next.js frontend, the GPUI Lab and menu bar,
oMLX.

`acp_server.py` (3,055 lines) was split by responsibility — `acp_approval.py` (the approval gate and session modes),
`acp_protocol.py` (tool kinds, rendering, the stdout guard), `acp_session.py` (session record, prompts, startup
preferences), `acp_learning.py` (bandit and maintenance hooks) — leaving the ACP agent itself at ~1,870 lines. Every
moved name is re-exported from `acp_server`, so callers and tests are unchanged (one test now patches
`acp_approval.current_generated_tool_names`, where the gate reads it). The split needed one root-cause fix:
`agent_common` now pins relative `sys.path` entries before it `chdir`s, so a bare-module import after it works under
`python -m` too — the import-order rule `acp_server` used to carry in a comment is gone.

Measured, baseline (`d6a0a26`) → now: backend Python outside tests 40,255 → 44,908 lines, of which the orchestrator,
the workers and the tool are 5,651 new lines — the rest of the backend shrank by ~1,000; the web UI 4,208 → 2,936
lines (−30 %); `acp_server.py` 3,055 → 1,868.

Not done from the plan, and why:

* **Memory consolidation** (retire Qdrant for LanceDB): agent memory lives in Qdrant on `mini` today;
  moving it is a data migration of the person's memories, not a refactor — left for a decision.
* **`backend/workspace/*`**: gitignored runtime scratch on this machine (nothing tracked); not deleted.

# The orchestrator

Plan → execute → harden → audit for coding tasks, across three kinds of worker — the Claude Code CLI, Google
Antigravity's `agy` CLI and Ollama models — choosing the cheapest one that does the job and moving the work
between them when a quota runs low. The design is [`orchestrator-plan.md`](orchestrator-plan.md); this page is
how it is built, how to use it, and where the build departed from the plan and why.

```text
 goal ─► PLAN ─► EXECUTE ─► HARDEN ─► AUDIT ─┬─► done
          ▲        │ step by step,            ├─► drift: a fix pass (≤ 2), audit again
          │        │ each in a checkpoint     └─► replan (≤ 1): back to the start checkpoint
          │        ▼ quota soft/hard line, provider limit
          │     safe-point handoff (packet) ─► same step, another pool
          └────────────── replan ─────────────────
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

`status` shows every quota pool's headroom, recent runs and handoffs — from `state.db`, spending nothing. `show
RUN_ID` prints a run's plan, history and audit; `resume RUN_ID [--allow-direct] [--max-usd N]` continues a paused or
interrupted one (a resumed run gets the default budget unless you pass `--max-usd`); `doctor` checks the CLIs, services
and settings this depends on; `replay` judges the learned dispatch policy (below). The TUI's `/status` and Discord's `!status` render the same pools (zero model tokens).
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
| `orchestrator/plan_artifact.py` | The plan contract: each step names its files; every criterion is checkable without judgement (`command`/`test`, `file_exists`, `file_not_changed`, `regex_match`). A draft that cannot be audited is refused, not repaired. |
| `orchestrator/quota_ledger.py` | Per-plan envelope plus rolling pool windows (5 h, weekly), soft 80 % / hard 95 % lines. Self-metered, and corrected by the provider's own readings (below). |
| `orchestrator/handoff.py` | The packet one worker leaves the next: step, checkpoint, partial diff, failed attempts, open assumptions — scrubbed, bounded, persisted, fenced as untrusted data. |
| `orchestrator/executor.py` | EXECUTE: checkpoints, per-step budgets (the smaller of the envelope's and the pool's room), rollback or keep-partial on a stop, handoff to another pool, pause when none is left. |
| `orchestrator/audit.py` | Tier 1 (free): scope, criteria, verifiers — model-written commands scored by Laya Guard, run with no network and no writes but caches. Tier 2: a reviewer's accept / drift / replan. |
| `orchestrator/machine.py` | The durable state machine (`orch_runs` in `state.db`), bounded retries, cross-family harden, restore on failure, resume. |
| `orchestrator/dispatch.py` | Who does what: hard gates, then the cheapest competent tier (learned), shadow mode, replay. |
| `orchestrator/evalsuite.py`, `sim.py` | The orchestration eval suite and the simulated providers it and replay are judged on. |
| `orchestrate_tool.py` | The session tool. |

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

# Orchestrator plan: plan → execute → harden → audit across Claude Code, Antigravity and Ollama

Status: refined engineering specification and implementation plan, approved 2026-10-10.
Baseline measurements recorded from checkout commit `1817ee9` (`feat/acp-tui-laya-secrets`).

## 1. The mission, restated

Make sierralab-agents a **lean, headless-first orchestrator** whose primary job is to get a coding task done at the lowest cost that reliably passes its own verification checks, by delegating between three worker families:

| Worker | What it is here | Headless surface (measured on this machine) |
| --- | --- | --- |
| **Claude Code** | `claude` CLI, wrapped by `ClaudeCodeWorker` | `-p --output-format stream-json --permission-prompts none`, `--max-budget-usd`, `--fallback-model`, `--model`, `--permission-mode` |
| **Antigravity** | `agy` CLI, wrapped by `AgyWorker` | `-p --output-format stream-json --input-format stream-json`, `--json-schema`, `--model`, `--effort`, `--mode plan\|accept-edits`, `--conversation <id>`, `--sandbox`; models include Gemini 3.x, **Claude Opus/Sonnet 3.5/3.7/4.x/5.5**, and GPT-OSS 120B |
| **Ollama** | local + `:cloud` models through `OllamaWorker` | In-process Strands `ModelRouter` and OpenAI-compatible proxy on `:11434` |

The core execution loop per task:

```text
 ┌──────────────┐     ┌──────────────┐     ┌──────────────┐     ┌──────────────┐
 │   1. PLAN    │ ──► │  2. EXECUTE  │ ──► │  3. HARDEN   │ ──► │   4. AUDIT   │ ──► DONE
 └──────────────┘     └──────────────┘     └──────────────┘     └──────┬───────┘
        ▲                    │                                         │
        │                    ▼ (Quota soft/hard limit)                 │
        │             SAFE HANDOFF (at step boundary)                  │
        │                    │                                         │
        │                    ▼                                         │
        │             RESUME IN NEXT WORKER                            │
        │                                                              │
        └────────────── Replan (bounded <= 1) ─────────────────────────┤
        └────────────── Drift retry (bounded <= 2) ────────────────────┘
```

1. **Plan**: the cheapest model that can produce a sound, structured plan outputs a machine-checkable plan artifact.
2. **Execute**: an execution worker carries out ordered steps inside shadow git checkpoints.
3. **Harden**: a worker from an alternate model family reviews the diff, patches issues, and executes security/safety checks.
4. **Audit**: deterministic checks first (file scope, acceptance commands, verifiers), followed by residual planner review (`accept`, `drift`, or `replan`).

Laya (System 1 fast router) plus learned bandit outcomes (System 2) select worker assignments. A **quota ledger** monitors pool headroom and enforces **handoffs at safe points** before provider limits are hit.

Brief:
> Make the orchestrator the best coding-agent dispatcher we can build. Keep what serves routing, safety, learning and handoff; freeze or prune the rest using measured usage rather than taste. Add a common `Worker` interface with Claude Code, Antigravity and Ollama adapters; a dual-envelope quota ledger with soft/hard thresholds; a portable, secret-scrubbed handoff packet; a durable plan→execute→harden→audit state machine; and a dispatch policy (Laya + bandit) that picks the cheapest competent worker. Every behavior change must pass the eval gate and preserve the invariant that nothing learned may lower a safety gate.

---

## 2. Where the project stands (scrutiny)

### What is already strong, and is reused rather than rebuilt

- **Laya router** (`laya_router.py`): route, risk and complexity in one ~18 ms local pass, with fail-closed abstention.
- **Reward and learning**: `outcomes.py` scores every turn; `bandit.py` learns discrete harness choices; `promote.py` and `backend/evals/` gate anything learned (42 tasks, held-out third). The invariant "nothing learned may lower a gate" is maintained.
- **Safety spine**: approval gate, Laya Guard, preflight, checkpoints (shadow git in `checkpoints.py`), self-heal with sandbox-verified patches, vault proxy (`vault_proxy.py`), scrubbed sandbox with Seatbelt (`scrubbed_sandbox.py`), and `secret_guard`.
- **Execution pieces**: `claude_code_tool.py` (headless, vault egress, allowlisted env, budget cap, stream parsing), `specialists.py` (architect, debugger), `team_pipeline.py` (planner + Strands graph), `stall_guard.py`.
- **Front doors**: daemon with approvals fanned out to the TUI, Discord, menu bar and Lab; hub conversations.

### Gaps against the goal

1. **No common worker abstraction.** Claude Code is a bespoke tool; Antigravity is absent; Ollama is only a model backend for the in-process agent, not something that can be handed an isolated task.
2. **No usage or limit accounting.** The only cap is a per-run `claudeCode.maxBudgetUsd`. `docs/harness-parity.md` records "no usage/cost" in `/status`. Without unified accounting, there is no proactive handoff point.
3. **No portable handoff.** Claude's session id, an `agy` conversation id, and an in-process Strands session cannot continue each other. Nothing captures "what is done, what is left" in a format another worker can resume.
4. **The loop exists in pieces, not as one durable machine.** Architect, team pipeline, debugger, and verifier run independently, but no artifact lets the planner check output *against the original plan*. `team_pipeline.py` decomposes, but nothing audits.
5. **The bandit cannot yet choose what matters.** Its `tier` and `effort` points are marked "not acted on in v1"; those are the cost/quality knobs required for multi-worker routing. It has no notion of worker pool or remaining quota headroom.
6. **Heavy footprint for one purpose.** Measured: backend 38.3k lines of Python (non-test) plus 76 smoke tests; 32.1k lines of Rust (`tui` 17.8k, `lab` 14.3k across Lab, menu bar, capture, observer); a 3.1k-line Next.js frontend; `acp_server.py` alone is 3,055 lines; three overlapping multi-agent mechanisms (`local_graph`, `local_swarm`, `team_pipeline`); two vector stores (Qdrant for agent memory, LanceDB for traces and the knowledge graph) plus optional Cognee; two local-model runtimes (oMLX and Ollama); scratch files in `backend/workspace/`, `game_tool.py`, and `games_static_api.py`.
7. **Uncommitted baseline.** `git status` lists 148 modified or untracked entries on `feat/acp-tui-laya-secrets` and the repo has no remote. Refactoring directly on top of that is unsafe.

---

## 3. Target architecture

```text
 Front doors (TUI · Discord · Headless CLI) ── approvals, status, handoff notices
                    │
         daemon: supervisor · state.db ledger
                    │
 ┌──────────────────▼───────────────────────────────────────────────────────────┐
 │ Orchestrator Engine (Session)                                                │
 │  Laya: route · risk · complexity ─► Dispatch policy ◄─ Quota ledger (state.db)│
 │                                      │   ▲ learned reward per (role,worker,   │
 │                                      │   │ model, task class) from outcomes   │
 │  Durable 4-Role State Machine (state.db):                                     │
 │   PLAN ─► EXECUTE ─► HARDEN ─► AUDIT ─┬─► DONE                                │
 │     ▲        │                          └─► REPLAN (<=1)                      │
 │     │        ▼ (Quota soft/hard)        └─► DRIFT RETRY (<=2)                 │
 │     │     SAFE-POINT HANDOFF ──┐                                              │
 │     └──────────────────────────┘                                              │
 └──────────────┬──────────────────┬───────────────────┬────────────────────────┘
                ▼                  ▼                   ▼
          ClaudeCodeWorker     AgyWorker          OllamaWorker
          (claude -p)          (agy -p)           (cloud/local, Strands)
          ─────────────────────────────────────────────────────────────
          Shared Spine: Vault Proxy Egress · Seatbelt Sandbox · Shadow Git Checkpoint
```

### 3.1 `Worker` interface (`backend/workers/`)

All workers implement an asynchronous, streaming interface supporting cancellation, quota introspection, and resumption.

#### Abstract Interface (`backend/workers/base.py`)
```python
from __future__ import annotations
from abc import ABC, abstractmethod
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from enum import Enum
from typing import Any

class WorkerFamily(str, Enum):
    CLAUDE_CODE = "claude_code"
    ANTIGRAVITY = "antigravity"
    OLLAMA = "ollama"

@dataclass(frozen=True)
class WorkerCapabilities:
    family: WorkerFamily
    models: list[str]
    supports_streaming: bool = True
    supports_structured_output: bool = True
    supports_sandbox: bool = True
    supports_resume: bool = True

@dataclass
class TaskPacket:
    task_id: str
    role: str  # "plan", "execute", "harden", "audit"
    prompt: str
    cwd: str
    file_scope: list[str] = field(default_factory=list)
    budget_usd: float | None = None
    budget_tokens: int | None = None
    timeout_s: float = 1800.0
    model: str | None = None
    effort: str | None = None
    checkpoint_hash: str | None = None
    handoff_context: dict[str, Any] | None = None

@dataclass
class WorkerEvent:
    event_type: str  # "init", "progress", "tool_use", "tool_result", "text", "error", "quota", "done"
    payload: dict[str, Any]
    raw: str | None = None

@dataclass
class WorkerResult:
    worker_id: str
    status: str  # "completed", "failed", "cancelled", "quota_limit"
    exit_code: int
    duration_s: float
    cost_usd: float
    tokens_in: int
    tokens_out: int
    files_changed: list[str]
    diff_stat: str
    raw_output: str
    error_message: str | None = None

class Worker(ABC):
    @property
    @abstractmethod
    def capabilities(self) -> WorkerCapabilities: ...

    @abstractmethod
    async def quota_snapshot(self) -> dict[str, Any]: ...

    @abstractmethod
    async def run(self, task: TaskPacket) -> AsyncIterator[WorkerEvent]: ...

    @abstractmethod
    async def resume(self, handle: str, packet: TaskPacket) -> AsyncIterator[WorkerEvent]: ...

    @abstractmethod
    async def cancel(self, handle: str) -> bool: ...
```

#### Worker Implementations
- **`ClaudeCodeWorker` (`backend/workers/claude_worker.py`)**:
  Wraps `claude -p --output-format stream-json --permission-prompts none`. Runs inside `scrubbed_sandbox` under macOS Seatbelt with `HTTPS_PROXY` routed through `vault_proxy`. Parses stream events, intercepts `--max-budget-usd`, and returns `WorkerResult` with cost and diff telemetry.
- **`AgyWorker` (`backend/workers/agy_worker.py`)**:
  Wraps `agy -p --output-format stream-json --input-format stream-json --conversation <session_id>`.
  - Roles `plan` and `audit`: run with `--mode plan --json-schema <schema_path>`.
  - Role `execute`: runs with `--mode accept-edits`.
  - Serves Gemini 3.x, Claude (Opus/Sonnet 3.5/3.7/4.x/5.5), and GPT-OSS 120B under Google Antigravity quota. Operates as an independent quota pool for the same model families.
- **`OllamaWorker` (`backend/workers/ollama_worker.py`)**:
  Wraps local models and Ollama Cloud models via Strands `ModelRouter`. Headless execution for cheap planning, summarization, and small-scope execution tasks.
- **Conformance Suite (`backend/workers/conformance.py` & `backend/_worker_conformance_smoke_test.py`)**:
  A standardized test suite executed against fakes (`fake_claude`, `fake_agy`, `fake_ollama`) asserting streaming contract compliance, budget cutoff, clean cancellation, resumption, and secret-boundary enforcement.

---

### 3.2 Plan artifact (`backend/orchestrator/plan_artifact.py`)

A schema-validated JSON document persisted in `state.db` and rendered on disk as the contract checked by the state machine:

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "OrchestratorPlan",
  "type": "object",
  "required": ["plan_id", "goal", "acceptance_criteria", "steps", "budget_envelope"],
  "properties": {
    "plan_id": { "type": "string" },
    "version": { "type": "integer", "default": 1 },
    "goal": { "type": "string" },
    "acceptance_criteria": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["id", "type", "description"],
        "properties": {
          "id": { "type": "string" },
          "type": { "enum": ["command", "test", "file_exists", "file_not_changed", "regex_match"] },
          "description": { "type": "string" },
          "target": { "type": "string" },
          "expected_exit_code": { "type": "integer", "default": 0 },
          "timeout_s": { "type": "number", "default": 30.0 }
        }
      }
    },
    "steps": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["step_id", "name", "description", "file_scope"],
        "properties": {
          "step_id": { "type": "string" },
          "name": { "type": "string" },
          "description": { "type": "string" },
          "file_scope": { "type": "array", "items": { "type": "string" } },
          "risk": { "enum": ["read_only", "edits_files", "destructive"], "default": "edits_files" },
          "checkpoint_hash": { "type": "string" },
          "rollback_policy": { "enum": ["rollback_checkpoint", "keep_partial"], "default": "rollback_checkpoint" },
          "status": { "enum": ["pending", "running", "completed", "failed", "partial", "skipped"], "default": "pending" }
        }
      }
    },
    "budget_envelope": {
      "type": "object",
      "properties": {
        "max_usd": { "type": "number", "default": 5.0 },
        "max_tokens": { "type": "integer" }
      }
    },
    "verifier_commands": {
      "type": "array",
      "items": { "type": "string" }
    },
    "assumptions": {
      "type": "array",
      "items": { "type": "string" }
    }
  }
}
```

The executor's tool permissions are derived directly from the active step's `file_scope`. Any mutation outside the declared paths is blocked at the sandbox/tool gate.

---

### 3.3 Audit, in cheap-first order (`backend/orchestrator/audit.py`)

Audit operates in two tiers:

1. **Tier 1 (Deterministic, zero model cost)**:
   - **Scope validation**: assert no files were modified outside the union of the plan's step `file_scope`s.
   - **Acceptance criteria verification**: execute all criteria commands/tests in `acceptance_criteria`.
   - **Static checks & verifiers**: run language-server diagnostics, linting, and verifier test commands.
   - **Diff integrity**: verify shadow git diff metrics against expected thresholds.
2. **Tier 2 (Residual model audit)**:
   - Sent to the planner model only when Tier 1 succeeds or produces ambiguous outputs: "Evaluate the diff against the original goal and acceptance criteria. Report verdict: `accept` or `drift` with specific drift items."

**Bounded Loop Policy**:
- Verdict `accept` $\rightarrow$ proceed to `DONE`.
- Verdict `drift` $\rightarrow$ retry execution (bounded: maximum 2 retries).
- Unresolvable failure or broken assumptions $\rightarrow$ trigger `replan` (bounded: maximum 1 replan).
- Exceeding bounds terminates with `FAILED` and restores the safe checkpoint.

The **Harden** role runs on an alternate model family from **Execute** (e.g., Gemini reviews Claude Code's diff, or Claude reviews Gemini's diff), counteracting single-model blind spots.

---

### 3.4 Quota ledger and safe-point handoff (`backend/orchestrator/quota_ledger.py`)

#### Dual-Envelope Accounting Model
- **Per-Plan Envelope**: dollar or token cap limiting single-task expenditure (e.g., \$2.00 default cap per plan).
- **Rolling Pool Window**: rolling time-window tracking (e.g., 5-hour rolling request/token cap for Claude Code / Antigravity, and weekly subscription limits).

#### SQLite Schema (`state.db`)
```sql
CREATE TABLE IF NOT EXISTS quota_pools (
    pool_id             TEXT PRIMARY KEY,
    family              TEXT NOT NULL,
    model_regex         TEXT NOT NULL,
    unit                TEXT NOT NULL, -- "usd", "tokens", "requests"
    soft_threshold_pct  REAL NOT NULL DEFAULT 0.80,
    hard_threshold_pct  REAL NOT NULL DEFAULT 0.95,
    window_seconds      INTEGER NOT NULL DEFAULT 18000 -- 5 hours
);

CREATE TABLE IF NOT EXISTS quota_usage (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    ts                  REAL NOT NULL,
    session_id          TEXT NOT NULL,
    step_id             TEXT,
    pool_id             TEXT NOT NULL,
    model               TEXT NOT NULL,
    tokens_in           INTEGER NOT NULL DEFAULT 0,
    tokens_out          INTEGER NOT NULL DEFAULT 0,
    cost_usd            REAL NOT NULL DEFAULT 0.0,
    is_error_limit      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS quota_usage_pool_ts ON quota_usage(pool_id, ts);

CREATE TABLE IF NOT EXISTS handoff_packets (
    handoff_id          TEXT PRIMARY KEY,
    session_id          TEXT NOT NULL,
    ts                  REAL NOT NULL,
    step_id             TEXT NOT NULL,
    from_worker         TEXT NOT NULL,
    to_worker           TEXT,
    reason              TEXT NOT NULL,
    packet_json         TEXT NOT NULL
);
```

#### Safe-Point Handoff Protocol
- **Soft Threshold (80%)**: Active worker completes its current step. At the step boundary, the orchestrator initiates a clean handoff to the next ranked worker.
- **Hard Threshold (95%) or Provider Rate-Limit Error**:
  - Worker execution is immediately halted.
  - Per step `rollback_policy`:
    - `rollback_checkpoint`: git shadow checkpoint restores the checkout to the step's clean initial state.
    - `keep_partial`: partial diff is preserved, marked `partial`, and transferred.
- **Handoff Packet**:
  Strictly typed JSON data (never instructions; secret-masked and fenced in `<<<UNTRUSTED_HANDOFF_DATA>>>`):
  ```json
  {
    "handoff_id": "uuid-str",
    "session_id": "sess-xyz",
    "timestamp": 1775876543.2,
    "reason": "quota_soft_limit_reached",
    "plan_id": "plan-123",
    "step_index": 2,
    "step_status": "partial",
    "checkpoint_hash": "a1b2c3d4...",
    "diff_stat": "3 files changed, 45 insertions, 12 deletions",
    "diff_patch": "...",
    "files_touched": ["backend/workers/base.py"],
    "verifier_results": { "test_conformance": "passed" },
    "failed_attempts": ["Attempt 1: TypeError in ..."],
    "open_assumptions": ["Assumed python 3.11 compatibility"]
  }
  ```

---

### 3.5 Dispatch policy (`backend/orchestrator/dispatch.py`)

Chooses the cheapest worker whose expected quality satisfies the threshold:

$$\text{Score}(w) = \hat{Q}(w, \text{role}, \text{task\_class}) - \lambda \cdot \text{NormalizedCost}(w) - \mu \cdot \text{QuotaScarcity}(w)$$

1. **System 1 (Laya)**: Provides instantaneous (~18 ms) route, risk band, and complexity scoring.
2. **Hard Security Gates**: Filters permitted options. Vault egress, risk limits, and the family diversity rule (Harden reviewer family $\ne$ Execute worker family) eliminate ineligible candidates.
3. **System 2 (Contextual Bandit)**:
   - Activates previously reserved decision points in `bandit.py`: `tier` (`fast`, `standard`, `strong`) and `effort` (`low`, `medium`, `high`, `max`).
   - Adds new discrete points: `planner_pool`, `executor_pool`, `reviewer_pool`.
4. **Shadow Mode First**: New dispatch policies log candidate choices and evaluate against historical ledger replay before live activation.

---

## 4. Making it lean (measure first, then cut)

### Component Categorization & Action Plan

| Subsystem | Planned Action | Concrete Target |
| --- | --- | --- |
| **Core Orchestrator & Safety** (`daemon/`, `laya_*`, `outcomes`, `bandit`, `promote`, `evals/`, `checkpoints`, `self_heal`, `preflight`, approval gate, `scrubbed_sandbox`, `vault_proxy`, `tui/`) | **Keep & Sharpen** | Core headless dispatch runtime |
| **Multi-Agent Mechanisms** (`team_pipeline.py`, `local_graph`, `local_swarm`) | **Consolidate** | Subsume into unified `OrchestratorMachine` |
| **Memory / Vector Stores** (Qdrant + LanceDB + optional Cognee) | **Consolidate** | Retain LanceDB as single local vector store; decouple Qdrant |
| **ACP Server** (`acp_server.py`, 3,055 lines) | **Refactor & Split** | Decompose into `acp_protocol.py`, `acp_session.py`, `acp_approval.py`, `acp_dispatch.py` |
| **Web Frontend** (`frontend/`, Next.js 3.1k lines) | **Freeze** | Kept out of default headless path; excluded from default CI |
| **Experimental Lab & Menubar** (`lab/`, 14.3k lines Rust) | **Freeze** | Kept behind optional `--lab` flag; separate crate CI tier |
| **Local Runtimes** (`omlx_manager.py`) | **Freeze** | Retained for local mini deployment; excluded from standard worker path |
| **Demo & Scratch Artifacts** (`game_tool.py`, `games_static_api.py`, `backend/workspace/*`) | **Delete** | Prune completely in Phase 6 |

---

## 5. Reverse engineering & provider intelligence (REA)

We apply an **observable runtime behavior first** methodology:

1. **Ollama**: Open-source reference; inspect code directly.
2. **Claude Code & Antigravity**:
   - Inspect `--help`, JSON stream outputs (`stream-json`), schemas, exit codes, and public documentation.
   - Analyze error payloads for undocumented rate-limit and quota headers.
   - Use REA binary reverse-engineering tools *only* if observable behavior leaves critical signal gaps AND licensing explicitly permits analysis.
3. **Artifact Integration**: Adopt the best elements of Antigravity's artifact model (plans, walkthroughs) into `PlanArtifact`.

---

## 6. Phased implementation roadmap

| # | Phase | Scope | Exit Test & Verification Gate |
| --- | --- | --- | --- |
| 0 | **Baseline & Clean Commit** | S | Working tree cleanly committed in logical reviewable chunks; full hermetic smoke suite (76 tests) passing: `make phase12-test && make learning-test` |
| 1 | **`Worker` Interface & Adapters** | M | `Worker` ABC, `ClaudeCodeWorker`, `AgyWorker`, `OllamaWorker`, and fake adapters pass `backend/_worker_conformance_smoke_test.py` |
| 2 | **Quota Ledger & Safe Handoff** | M–L | Fault injection test: artificially trigger limit at step 2 of 5; verify clean checkpoint, handoff packet generation, resume on second worker, and test pass: `backend/_quota_ledger_smoke_test.py` |
| 3 | **Plan Artifact & 4-Role State Machine** | L | Planted-drift test caught by Audit; plan-conformant test accepted; state machine bounded loops verified: `backend/_orchestrator_machine_smoke_test.py` |
| 4 | **Dispatch Policy & Bandit Activation** | M | Shadow-mode ledger replay demonstrates equal/higher pass rate at lower cost than "always strongest" baseline; activate `tier`, `effort`, and pool points |
| 5 | **Orchestration Eval Suite** | M | Formal eval benchmark measuring cost-per-success, handoff survival, and plan conformance wired into `backend/promote.py` gate: `make evals ARGS="--suite all"` |
| 6 | **Lean Consolidation & Cleanup** | M | Subsume `team_pipeline`/`local_graph`/`local_swarm`; freeze frontend/Lab/oMLX; split `acp_server.py`; prune dead files; confirm LOC reduction and test speedup |
| 7 | **Front-Door Telemetry** | S | `/status` in `acp-tui` and Discord renders real-time quota headroom gauges and handoff events with zero model token expenditure |

---

## 7. Risks and mitigations

- **Terms of Service**: Headless automation of subscription CLIs (`claude`, `agy`) must respect usage limits. *Mitigation*: The quota ledger enforces proactive caps and gracefully falls back to Ollama and local models.
- **Cross-Worker Prompt Injection**: Worker outputs become inputs for subsequent stages. *Mitigation*: Handoff packets carry strictly validated structured data; all inter-agent text is secret-scrubbed and fenced in `<<<UNTRUSTED_HANDOFF_DATA>>>`.
- **Planner Blind Spots & Collusion**: A planner model might generate trivial acceptance criteria. *Mitigation*: The Harden role runs on an alternate model family and validates acceptance criteria against the prompt before execution completes.
- **Cost Runaway**: Uncontrolled retry loops. *Mitigation*: Hard budget caps per plan envelope; state machine retry counts bounded (max 2 drift retries, max 1 replan); `stall_guard.py` active.
- **Refactoring on Dirty Baseline**: *Mitigation*: Phase 0 enforces clean baseline commits before any orchestrator refactoring begins.

---

## 8. Resolved decisions & rationale

1. **Web frontend, GPUI Lab, menubar, and oMLX**:
   - **Decision**: **Freeze** behind feature boundaries.
   - **Rationale**: The orchestrator's mission is headless-first. Freezing these components decouples them from default testing and execution paths without introducing immediate breaking regressions. Dead code will be pruned in Phase 6 based on measured usage.
2. **Reverse-Engineering Scope (REA)**:
   - **Decision**: **Observable behavior first**.
   - **Rationale**: CLI flags, JSON stream events, return codes, and public documentation provide all necessary integration signals. Binary reverse-engineering is avoided unless an essential capability is opaque and explicitly permitted by license terms.
3. **Quota Accounting Ground Truth**:
   - **Decision**: **Provider execution signals + local SQLite self-metering**.
   - **Rationale**: Combining CLI return records (`total_cost_usd`, tokens), rate-limit error signals, and deterministic local metering in `state.db` provides 100% reliable tracking with zero web-scraping fragility.
4. **Budget Envelope Model**:
   - **Decision**: **Dual-envelope model**.
   - **Rationale**: Per-plan dollar caps enforce immediate blast-radius containment, while rolling 5-hour and weekly window tracking prevents subscription quota exhaustion.

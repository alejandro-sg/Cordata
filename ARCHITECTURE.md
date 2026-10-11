# Architecture

A map, not a manual: what runs where, how a turn flows, and which document holds the detail. Safety assumptions are in
[`THREAT_MODEL.md`](THREAT_MODEL.md); every setting is in [`docs/settings.md`](docs/settings.md); the latest audit is
[`system_audit_report.md`](system_audit_report.md).

## Processes

```text
 acp-tui (Rust, ratatui)      Zed / any ACP client      web UI (Next.js)        Discord (operator only)
        │ ACP over stdio or the daemon socket                │ AG-UI / HTTP              │ outbound bot
        ▼                                                     ▼                           │
 ┌───────────────────────── orchestrator daemon (backend/daemon, `make daemon`) ─────────┴──────┐
 │  supervisor: one acp_server.py child per session · approvals fanned out to every front door  │
 │  gRPC bridge (tonic client in lab/) · observer (read-only, off by default) · telemetry hub   │
 └───────────────────────────────┬──────────────────────────────────────────────────────────────┘
                                 ▼
 ┌──────────────────────── acp_server.py (one session's agent process) ─────────────────────────┐
 │ Laya router (route · risk · complexity, local model) ──► orchestrator Strands Agent          │
 │   tools · specialists (as_tool) · orchestrate (→ orchestrator/) · MCP plugins · LSP bridge    │
 │   _ApprovalHandler: preflight → deny rules → vault → mode → Laya Guard → ask / proceed        │
 │   checkpoints (shadow git) · self-heal · outcomes / learning · rejection memory · KG sink     │
 └──────────────────────────────────────────────────────────────────────────────────────────────┘
        │ models                                   │ memory / stores
        ▼                                          ▼
 Ollama Cloud (via the local daemon) · Anthropic   Qdrant (agent_memory, on sierralab-mini) · LanceDB
 · OpenAI-compatible · oMLX local (Prism models,   (traces, knowledge graph; Cognee optional) ·
   loaded/unloaded through memory_budget)          ~/.sierralab (settings, sessions, checkpoints,
                                                   preferences, learned skills, state.db ledger)
 Native Lab (lab/, Rust GPUI): telemetry, the containment modal, E-Stop, effort dials — over gRPC (proto/harness.proto).
 Menu bar (lab/crates/harness-menubar): status icon + ⌘⇧Space deck; screen context (harness-capture, deny-by-default)
   → SubmitTurn → os_context.py injector for one turn; a screenshot only to capture.visionModel, then forgotten.
```

## A turn

1. **Route** — Laya answers route / risk / complexity in one local pass (`laya_router.py`); a confident specialist route
   skips the orchestrator's own model call, a team route hints at `orchestrate`. Abstention fails closed.
2. **Context** — the system prompt (project, workspace profile, instruction files, prompt overlays) plus per-message
   injections: the clock, the MCP servers running now, the rules this person's rejections taught. Cold tools are
   hidden until needed; skills are found by search (`context_diet.py`, `skill_search.py`).
3. **Find before reading** — `code_search` (ripgrep), `code_outline`, the LSP tools; a long file read whole comes back
   as an outline (`code_search.py`, `code_nav.py`).
4. **Act** — every tool call passes `_ApprovalHandler`. An edit is preflighted first (syntax, ruff, the language
   server on the unsaved text, optional tests in a copy); what it newly breaks goes back to the agent, not to a person
   (`preflight.py`). Then deny rules, the vault's egress gate, the mode, Laya Guard's per-call score — and a
   checkpoint before anything that may change files (`checkpoints.py`).
5. **Heal** — a failing command is diagnosed by the Debugger and its patch verified in a throwaway copy before the
   agent is told to apply it (`self_heal.py`, `fix_memory.py`).
6. **Learn** — the turn is scored (`outcomes.py`); a costly turn may stage a lesson (`reflect.py` → `promote.py`'s
   eval gate); a rejected change becomes a constraint (`rejection_memory.py`); the bandit and Laya's calibrators learn
   from outcomes, and nothing learned may lower a gate.

## The orchestrator (coding CLIs as workers)

```text
 orchestrate tool / python -m orchestrator
        │
 OrchestratorMachine (state.db: orch_runs) ── dispatch (gates → cheapest competent tier) ◄── quota ledger
   PLAN ─► EXECUTE (steps in checkpoints, Seatbelt write scope) ─► HARDEN (other model family) ─► AUDIT (tier 1 → tier 2)
        │                 │ quota line / provider limit → handoff packet → same step, another pool
        ▼
 ClaudeCodeWorker (claude -p) · AgyWorker (agy) · OllamaWorker (Strands, local or :cloud) — backend/workers/
```

[`docs/orchestrator.md`](docs/orchestrator.md) has the detail; [`docs/orchestrator-plan.md`](docs/orchestrator-plan.md)
the plan it was built from.

## Status: core and frozen

| Tier | What | In default CI |
| --- | --- | --- |
| Core | daemon, `acp_server`, the orchestrator and workers, Laya, outcomes/bandit/promote/evals, checkpoints, self-heal, preflight, vault, sandbox, `acp-tui` | yes |
| Frozen | the Next.js web UI (`frontend/`, its own workflow on `frontend/**` changes), the GPUI Lab and menu bar (`lab/`, `make lab-test`), oMLX (`omlx_*`, local Prism models on the mini) | no |

Frozen means kept working and built, not developed; nothing core depends on a frozen piece.

## Where the detail is

| Topic | Document |
| --- | --- |
| The agentic harness: routing, Guard, specialists, preflight, self-heal, learning, token economy, code navigation, plugins, MCP | [`docs/agentic-harness.md`](docs/agentic-harness.md) |
| Self-healing internals | [`docs/self-healing-architecture.md`](docs/self-healing-architecture.md) |
| Laya triage | [`docs/laya-triage.md`](docs/laya-triage.md) |
| The orchestrator: workers, quota ledger, handoffs, the state machine, dispatch | [`docs/orchestrator.md`](docs/orchestrator.md) |
| The meta-harness (Claude Code CLI, memory managers, Prism models) | [`docs/meta-harness.md`](docs/meta-harness.md) |
| The Experimental Lab and its gRPC bridge | [`docs/experimental-lab.md`](docs/experimental-lab.md) |
| Evals | [`docs/evals.md`](docs/evals.md) |
| Strands SDK: what is used, what was declined, and why | [`docs/strands-sdk-audit.md`](docs/strands-sdk-audit.md) |
| Parity with Claude Code / Kilo / DeepSeek-TUI | [`docs/harness-parity.md`](docs/harness-parity.md) |
| Settings, permissions, trust | [`docs/settings.md`](docs/settings.md) |
| Threat model | [`THREAT_MODEL.md`](THREAT_MODEL.md) |
| The TUI | [`tui/README.md`](tui/README.md) |
| Scripts and deployment | [`scripts/README.md`](scripts/README.md) |

## Tests

Every mechanism has a hermetic smoke test in `backend/_*_smoke_test.py` (scripted models, the real ACP server object,
no network); CI runs the ones that need no local service (`.github/workflows/ci.yml`). Rust: `make tui-test`,
`make lab-test`. Live checks (`_acp_smoke_test.py`, `_ollama_cloud_live_test.py`, `make lab-live`) need the real
providers and are run by hand — with `SIERRALAB_DISCORD=off`, so a test turn does not page the operator.

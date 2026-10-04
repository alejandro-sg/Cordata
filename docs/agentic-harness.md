# The agentic harness

How one user turn runs through this backend: the fast typed router in front of the LLM, the
specialists around it, the autonomous loops (self-heal, the team pipeline), the supporting
machinery (language servers, plugins, patches, the event bus), and what all of it can and cannot
touch. The parity view (what Claude Code / Kilo / DeepSeek-TUI do and whether this matches) is in
[`harness-parity.md`](harness-parity.md); safety assumptions are in [`../THREAT_MODEL.md`](../THREAT_MODEL.md).

## A turn, end to end

```text
acp-tui / Zed / web UI
   │  prompt turn
   ▼
ACP server (acp_server.py) ── Laya triage+router (laya_triage.py, laya_router.py)
   │                              route · risk · complexity, one ~18 ms pass
   │                          ├── dispatch → specialist (architect/debugger) directly
   │                          ├── dispatch → team pipeline (planner → GraphBuilder)
   │                          ├── scope   → plugin's tools only this turn
   │                          ├── hint    → advisory line in the system prompt
   │                          └── escalate→ tiers.strong for this turn (abstain → fail closed)
   ▼
Orchestrator (Strands Agent, agent_common.py) — model loop with tools
   ├── human-gated: shell, python_repl, file_write, editor, apply_patch, generated skills
   ├── read-only:  file_read, http_request, agent_memory, think, lsp_*, time, calculator
   ├── multi-agent: local_swarm, local_graph (sub-agents, denylisted toolset)
   ├── remote:      a2a_client (A2A protocol)
   ├── plugins:     per-project MCP plugin tools
   └── hooks:       secret redaction, untrusted-result guard, Laya outcome learning,
                    self-healing loop, skill validation, LSP edit hooks
   ▼
Responses stream back as agent messages + tool cards; heal/plugin/LSP/tier events
ride the harness event bus (harness_events.py) as ACP extension notifications.
```

## Laya: routing, not reasoning (laya_triage.py, laya_router.py)

Laya is a local decision model — it never generates text. Per turn, one forward pass answers
three typed questions (route, risk, complexity); a calibrated policy then acts or abstains.
Abstention always fails closed (the turn runs on the strong tier or lets the LLM decide alone),
and Laya can only *add* a prompt (arm the approval gate), never remove one. Every turn's route
and every shell command's predicted outcome is scored against reality afterward and feeds the
online calibrators (`laya_calibration.py`), logged per user in `laya_triage.jsonl` — inspect with
`_laya_triage_report.py`.

Routing targets: the orchestrator itself (`answer`/`code`/`web`/`memory`/`team`), the two
specialists, or a running plugin. `docs/laya-triage.md` has the triage details and thresholds.

## Direct specialist dispatch (specialists.py)

With `laya.dispatch: direct` (default), a confident `architect` or `debugger` route skips the
orchestrator's model call — the turn's text goes straight to the specialist, whose (read-only,
or patch+shell for the debugger) output is written back into the conversation as if the
orchestrator had delegated. Specialists are stateless: context is passed in explicitly, and each
exchange is persisted to the session file. `laya.dispatch: hint` keeps only the advisory line.

Specialists are Strands `Agent.as_tool()` instances — the same object is (a) an orchestrator
tool the model can call on its own and (b) a dispatch target Laya can reach directly. Their
interventions share the session's `_ApprovalHandler` by reference, so a specialist's `shell` or
`apply_patch` call prompts exactly like the orchestrator's own — including interrupts, which
propagate up to the parent (`_harness_loop_smoke_test.py`).

## Self-healing (self_heal.py)

A `shell` call that exits non-zero becomes an autonomous loop: the Debugger (read-only) is
asked for a root cause and a unified diff; the patch is applied in a throwaway copy of the
project (verified with the same failing command); if it passes, the note handed back to the
orchestrator says so and tells it to `apply_patch` (a gated call) and re-run. If verification
fails, the Debugger sees the new output and tries again — bounded by `selfHeal.maxAttempts`,
after which the model is told to stop and report. Files edited by the loop are rendered as diffs
in acp-tui. Full detail: [`self-healing-architecture.md`](self-healing-architecture.md).

Two subtle traps this code now defends against, both found live:

- The shadow copy is built with `__pycache__` (and the other tool caches) excluded, and every
  `apply_patch` write deletes the patched module's `.pyc`. A `.pyc` header records the source's
  mtime as whole seconds, so a patch rewritten in the same wall-clock second as the failing run
  would otherwise validate stale byte-code — a broken patch could look verified, or a good one
  look failed, depending on caching luck rather than the code.
- With `selfHeal.autoApply`, pre-approval is *exact-match* only: the `apply_patch` text must
  equal the sandbox-verified patch, and the re-run command must equal the one that failed.
  `deny` rules and plan mode still win; see `THREAT_MODEL.md`.

## Team pipeline (team_pipeline.py)

Before the team pipeline, a multi-part turn went through the orchestrator's own model call,
which had to decide to build a team and how to decompose it. Now a confident `team` route with
`laya.teamDispatch: direct` (default) runs the pipeline directly:

1. **Plan** — one small model call (no tools) converts the task into a validated JSON team spec
   (name, role, task, optional tools/tier/dependencies). Anything malformed fails *open*: the
   turn falls back to the advisory hint and the orchestrator decides normally — never hidden,
   never swallowed.
2. **Run** — the same machinery the `local_graph` tool uses (`stream_graph` in
   `local_graph_tool.py` — literally imported, not copied): Strands `GraphBuilder`, nodes built
   by `sub_agent_common.build_agent_nodes`, progress streamed into one tool card.

Safety is unchanged by skipping the orchestrator: every node's tools go through
`sub_agent_common.resolve_tools` (no shell, no python_repl, no file_write, no editor, no
generated skills, no orchestrator-only or frontend-proxy tools) — the same denylist local_graph
and local_swarm runs have. The pipeline automates only the *decision* to delegate; it does not
widen what a delegated agent can do. `local_swarm` / `local_graph` stay available as orchestrator
tools for the case the model wants a team Laya didn't route — sub-agents run on the session's
provider, same tiers, `SwarmProgress`-style progress plus per-node reasoning tails.

## Remote agents over A2A (a2a_client_tool.py)

`a2a_client` (Strands' `A2AAgent`, backed by `a2a-sdk` — installed as
`strands-agents[a2a]`) lets the orchestrator ask a remote agent served over the
[Agent-to-Agent protocol](https://a2aproject.github.io/A2A/) for one self-contained task:
`A2AAgent(endpoint).invoke_async(prompt)` behind a `@tool`, with http/https enforced and errors
returned as tool results. Deliberate limits:

- **Orchestrator-only** (`sub_agent_common.ORCHESTRATOR_ONLY_TOOLS`): the call is unreviewed
  network egress, and a sub-agent has no approval gate. A `local_graph`/`local_swarm` spec
  asking for `a2a_client` by name is refused like any other denied tool.
- **No credential surface.** Authenticated remote agents would take a `ClientConfig` in *code*;
  the tool has no parameter that could carry a key a prompt injection might exfiltrate.
- The remote reply is third-party output — labelled untrusted in the tool result, subject to the
  orchestrator's secret-redaction hooks.
- Serving this harness's own agent *as* an A2A server is not wired up (Strands supports it);
  that would be a deliberate, separately-reviewed decision, not a side effect of this tool.

`strands-agents[a2a]` is pinned in `backend/requirements.txt` (a2a-sdk 0.3.26 with 1.55.1).

## Around the LLM

- **Language servers (`lsp_bridge.py`)** — per-project language servers started off the request
  path (`lsp.enabled`, default on): diagnostics, symbols, definitions, references as read-only
  tools and post-edit diagnostic hooks, so the model checks its edits against the real compiler
  instead of guessing. Status rides the event bus.
- **Plugins (`plugin_host.py`)** — per-project MCP plugin servers (`backend/mcp-servers/`,
  `mcp_servers.json`), started in the backend's process, their tools reaching the orchestrator;
  a confident `plugin_<name>` route scopes the turn's toolset to that plugin.
- **Patches (`patch_tool.py`, `edit_preview.py`)** — unified-diff `apply_patch` (atomic,
  root-confined, context-repairing) through the normal edit-approval gate, plus ACP diff
  previews of edits *before* they happen.
- **Secrets (`secret_guard.py`, `prompt_security.py`)** — env/Keychain values are masked in tool
  output, redacted before anything reaches Laya or the logs, and shell denies for Keychain
  reads are built in. Untrusted tool results get the injection-hardening treatment.
- **Event bus (`harness_events.py`)** — the producers (plugins, LSP, self-heal, sub-agent tiers)
  emit thread-safe events; the ACP server forwards them as `_sierralab/<kind>` extension
  notifications. Unknown extensions are ignorable per the ACP spec, so Zed is unaffected; the
  TUI renders them as progress bars.

## Where things are applied and tested

Settings keys for all of this (`laya.*`, `selfHeal.*`, `lsp.enabled`, model/temperature) are in
[`settings.md`](settings.md), applied at session start by `_apply_startup_preferences`. Each
piece has a hermetic smoke test — see `_self_heal_smoke_test.py`, `_team_pipeline_smoke_test.py`,
`_a2a_client_smoke_test.py`, `_laya_router_smoke_test.py`, `_harness_loop_smoke_test.py`,
`_plugins_lsp_smoke_test.py` and friends; the denylist and graph-structure assumptions are
regression-tested in `_subagent_denylist_smoke_test.py` and
`_local_graph_structure_smoke_test.py`. Web-UI and TUI behavior is the same backend — the
AG-UI front door (`main.py`) gets the same tools; only the specialist/laya dispatch is ACP-side.
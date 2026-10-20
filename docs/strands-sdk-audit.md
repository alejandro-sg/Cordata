# Strands SDK audit — what it offers, what the harness uses

Method: every page of the Strands docs that bears on this harness was read through the docs MCP (agent loop,
lifecycle controls, hooks and events, interventions — HITL, Cedar, steering —, plugins — skills, context
injector, context offloader, goal loop —, context management and its modes/presets/estimation, conversation
management, sessions, snapshots, storage, memory, model providers incl. Ollama and routing, retry, tools —
vended, executors, results, MCP —, multi-agent, sandbox, Strands Shell, observability, safety, evals, and the
`strands-harness` product docs), the installed SDK (1.55.1) was read where a doc was ambiguous, and the
1.56 – 1.57.2 release notes were checked. Each claim below was either run (the tests named) or read in source.

Installed: **`strands-agents 1.57.2`** (the latest), with `strands-agents-tools 0.8.9` and `ag_ui_strands 0.4.1` — all three
bumped together in `requirements.txt`; every hermetic suite passes on them. (The audit started on 1.55.1.)

## Adopted

| SDK feature | Where | Why it earns its place |
| --- | --- | --- |
| `ContextOffloader` (plugins/context-offloader) | `context_offload.py`, `acp_server._build_session` | A big tool result is written whole to a private store; the model reads a head+tail preview and a reference and pulls only the slice it needs with `retrieve_offloaded_content`. Replaces *cutting* (lossless; a command is never re-run to see its middle). Tested: `_offload_acp_smoke_test.py`. |
| Per-invocation `limits` (agent-loop → Invocation limits) | `acp_server._stream_turn`, `context.maxSteps` / `context.maxTurnUsage` | A turn that will not converge pauses (ACP `max_turn_requests`, history valid, "continue" resumes) instead of spending. Enforced at the `after_tools` checkpoint, because a limit is per invocation and a checkpointed turn is many. |
| `agent.cancel()` + `tool_context.cancel_signal` (agent-loop → Cancellation) | `acp_server.cancel`, `claude_code_tool.run_engineer` | A stop now reaches the running agent and, through the signal, the tool: the Claude Code CLI is killed within ~0.4 s instead of running to its timeout on your usage. Never called on an idle agent (the signal outlives the call and would cancel the next turn). |
| `proactive_compression` (conversation-management) | `agent_common` / `sub_agent_common.build_conversation_manager` | History is summarised before the call that would overflow, not after the provider rejects it — a whole wasted round trip. Every model path now carries a context window (`UNKNOWN_CONTEXT_WINDOW`, 128k when nothing says; `OLLAMA_CLOUD_CONTEXT_WINDOW`). |
| `strands.vended_tools.shell` / `file_editor` (tools → vended tools) | `core_tools.py` | Replace the deprecated `strands_tools.shell` / `editor` (an error log in tools 0.9.0). Schema 8.6 KB → 1.8 KB (~1,700 tokens off every call), routed through `agent.sandbox`, killed as a process tree on timeout. Names and parameters kept (`shell`, `editor`) so the approval gate, Laya Guard, saved rules and checkpoints still match; hooks ordered ahead of everything else fit the result (text with an exit code), the timeout default (900 s) and relative paths. A `HarnessSandbox` makes a running command end when the turn is cancelled. Tested: `_core_tools_smoke_test.py`. |
| `SnapshotSessionManager` (session-management) | `session_store.py` | One atomic, versioned JSON file per conversation (0600) instead of a directory of per-message files; the SDK's recommended path. The system prompt is *not* persisted (this harness rebuilds it every session — `_capture` is overridden), hand-appended messages are persisted by capturing the agent, and the 420 existing sessions migrate on first use or in bulk (`python session_store.py migrate --all`): exact (messages, state, conversation-manager state identical for all 420) and additive (the legacy directory is untouched). Tested: `_session_store_smoke_test.py`. |
| Immutable snapshots / `restore_snapshot` (session-management → time travel) | `session_store.begin_turn` / `go_back`, `/back`, `!back` | One checkpoint per turn start, labelled with the prompt; going back restores messages, state and the conversation manager's state (files untouched) and is itself undoable. Explicit `save_snapshot(is_latest=False)` rather than the `snapshot_trigger` callback, which only sees the agent and cannot say *which turn*. |
| `strands.vended_tools.http_request` / `web_fetch` | `core_tools.py` | `http_request` 4.1 KB → 1.0 KB of schema, redirects followed, a per-call client so the vault's proxy environment is read at call time, error bodies kept (fenced); `web_fetch` is new — an analyst answers about the page so it never enters the main context (`context.webFetch`; fast model tier when configured). |
| `ContextInjector` for the clock | `core_tools.clock` | Replaces `current_time` (the deprecation's own advice): ~20 tokens per user turn, ephemeral, instead of a tool. |
| `SummarizingConversationManager(summarization_agent=…)` (conversation-management) | `agent_common._summarizer_agent`, `context.summaryModel` | Compaction is a model call you pay for; the fast tier (`MODEL_TIER_FAST`) writes it when one is configured. No fast tier set → unchanged. |
| The SDK's model table (`strands.models._defaults`) | `agent_common._with_window` | Anthropic/OpenAI context windows come from the SDK (1.57.2 knows Sonnet 5.5, Opus 5.5, Fable 5.1) instead of a hand-kept prefix table that said 200k for all of them. |
| `ContextInjector`, `AgentSkills`, `Plugin` / `@hook` (plugins) | `skill_search.py` | Semantic skill listing with Laya deciding (earlier change). |
| Interventions (`InterventionHandler`, `Confirm` / `Deny` / `Proceed`) | `acp_server._ApprovalHandler` | The approval gate — a superset of `HumanInTheLoop` (see below). |
| strands-harness `make_subagent` (0.3.1; the tool only, not `create_harness`) | `subagent_tool.py` | The orchestrator's one in-process delegation tool: `subagent` with a role — `generalist`, a read-only `explorer`, and the specialists as presets (`architect` read-only, `debugger` with shell + apply_patch; prompts, tiers and kits still defined once in `specialists.py`, whose agents the router's direct dispatch and self-heal keep using) — a tier, a tool subset and `none` / `no_tools` context. Explorer and architect run in plan mode. Children carry the session's `_ApprovalHandler`, so their `shell` / `apply_patch` ask like the orchestrator's own; depth 1, no `orchestrate`, no egress. Each call is a Lab pod of its own (`debugger-3`: task, delegation link, trace, live output and reasoning via `lab.LabStream`), the tool card renders it in the team format in the TUI and the app, and the Lab window's `Pod` carries `reasoning_tail` / `task`. `_subagent_tool_smoke_test.py`, `_lab_acp_smoke_test.py`, both e2e Lab suites; live: `_subagent_live_test.py`. |
| `background_tasks` (`BackgroundTasksConfig`), for `subagent` only | `subagent_tool.BACKGROUND_TASKS`, `hold_turn_for_background` | The model may set `_background_execution` on a `subagent` call and keep working (`agentic: ["subagent"]`, no wildcard; `subagents.background`, default on). The SDK's `wait_for_completion` is off because with `checkpointing=True` every pause is an invocation and it would block the next one; instead a hook waits only at the turn's real end, so results arrive inside the turn and a child's approval prompt still surfaces as an interrupt. Reads the plugin's internals — pinned SDK. |
| `ModelRouter` / `FallbackStrategy`, `GraphBuilder`, `Swarm`, `as_tool()`, A2A, `Sandbox`, checkpoints, `FileSessionManager`, `SummarizingConversationManager`, `MCPClient`, hooks | throughout | Already in use. `as_tool()` also means cancelling the parent now cancels the delegated specialist (docs: "receives the parent's signal automatically"). |
| Concurrent tool executor | SDK default | Used implicitly. |

## Looked at and deliberately not adopted

| Feature | Verdict |
| --- | --- |
| `HumanInTheLoop` (allow-list, LLM risk classifier, trust mode) | The gate is already a superset: modes, rules, Laya's per-call risk gate, "Always allow", three front doors. The SDK's classifier spends a model call per tool call; Laya is local and ~25 ms. |
| `LLMSteeringHandler` / tool ledger | One extra model call per tool call. `stall_guard` already stops repeated identical steps, Laya rates the risk. |
| Cedar authorization | Declarative policy files + `cedarpy`. The settings rules (`allow` / `ask` / `deny` patterns) cover the same ground for one operator; revisit for a team policy. |
| `GoalLoop` | Judge model call(s) per attempt. Self-heal verifies with commands and costs nothing when it passes. |
| `context_manager="auto"` / `"agentic"` | Experimental. `auto` is `ContextOffloader(1500, 750)` + `SummarizingConversationManager(0.3, 0.85)`, i.e. what the harness now composes by hand — with a tail-keeping preview, a private store, a veto list and masking it lacks. Note: SDK 1.57.2's own harness *removed* the offloader plugin in favour of the context manager's stash (#4701) — the plugin remains public and supported; revisit when the strategy API leaves experimental. |
| `pin_first` | Would pin the *first* message of a long chat forever — often "hi". |
| SDK `MemoryManager` + vended stores | Duplicates `agent_memory` (Qdrant on the mini), `memory.lance` and the KG, and its background extraction spends a model call every few turns. |
| `background_tasks` for every other tool | Adopted for `subagent` only (above). `claude_code_engineer` and `orchestrate` stream their own progress and are long by nature; making them backgroundable is the next candidate. |
| `strands.vended_tools` — `notebook`, `sleep`, `handoff_to_user`, `mcp_router` | `shell`, `file_editor`, `http_request` and `web_fetch` are adopted (above). `calculator`, `current_time` and `think` — the other `strands_tools` deprecations — are dropped (the clock became a context injector; arithmetic is the model's or `python_repl`'s; reasoning is the provider's). The rest have no use here yet. `strands_tools` itself stays for `file_read`, `file_write` and `python_repl`, which are not deprecated. |
| `programmatic_tool_caller` (strands-harness) | Model-written code calling tools in one step — saves tokens on multi-tool work, but Monty isolates the code, not the tools it calls; needs a sandbox decision first. |
| OpenTelemetry traces / OTLP | The harness has `trace_store`, `state.db` and the per-turn receipt (`_cordata/turn`). An OTLP exporter behind a setting is a small later addition. |
| Guardrails, PII redaction | Bedrock-oriented; `secret_guard` + `secret_patterns` cover this harness's threat model. |
| Structured output, `idempotency_token`, `concurrent_invocation_mode` | Nothing here needs them: one writer per session, `Conversation.busy` already rejects a double submit. |
| Strands Shell (in-process sandboxed Bourne shell, credential injection, SSRF guard) | A real alternative to `ScrubbedSandbox` for the CLI egress story, but a separate project. |
| `Agent.shutdown()` (1.57.1) | Only flushes an SDK `MemoryManager`, which the harness does not use. A no-op here. |
| Strands Evals (`strands-agents-evals` 1.4.0) | Spiked — `backend/evals/spikes/strands_evals_skill_routing.py`. Its deterministic evaluators (`SkillInvoked`, `ToolCalled`) read OpenTelemetry trajectories offline and for free, and a retrieval `Experiment` runs 54 judgements in 0.14 s; but the recall check already exists (`_skill_search_smoke_test.py`, now 27 requests: 27/27 in the nearest four, 22/27 first), and the LLM-judged evaluators default to a Bedrock judge (AWS credentials, tokens per case). Adopt the trajectory checks the day a routing or skill-activation regression needs catching that recall cannot see; the dependency is just `rich`. |

## Checked and fine

* **Trusted message history** (safety-security): a caller-shaped `toolUse` as the last message runs the tool with no
  model call. The ACP prompt is always flattened to a text string (`_extract_prompt_text`), so a Discord or TUI
  caller cannot forge one; checkpoint resume payloads come from local disk.
* **Retry**: SDK default (5 retries, exponential from 4 s) on throttling, left as is.
* **Hook ordering**: `AfterToolCallEvent` runs in reverse registration order and plugins register after hooks, so a
  plugin's hook runs first. This is why `HarnessOffloader` masks credentials itself rather than relying on
  `SecretRedactionHooks`, and why `ContextDiet`'s cap is the natural fallback when the offloader declines.

## Phase 11: conformance pass over the harness's own constructs

Every custom tool and hand-off was re-read against the SDK. What was already native stays as it is; what was not is
now, and the new pieces are built from SDK surfaces only:

| Construct | Verdict |
| --- | --- |
| `claude_code_engineer` | Native: `@tool(context=True)` async generator, cooperative stop through `tool_context.cancel_signal`. No change. |
| `omlx_resource_manager` / `ollama_resource_manager` | Native `@tool` returning a `ToolResult` (`status` + `content` text/json). The Prism lease hooks are a `HookProvider` on `Before/AfterInvocationEvent`; fixed: an invocation whose `AfterInvocationEvent` never fires (the SDK skips it when a later `BeforeInvocation` hook raises — `agent.py`, `_run_loop`) no longer strands its lease. |
| Specialists, team pipeline, swarm, graph | Native `as_tool()`, `GraphBuilder`, `Swarm` — the SDK's own multi-agent hand-offs. No change. |
| `skill_forge` researcher | Was on `strands_tools.http_request` (deprecated in tools 0.8.9, an error log in 0.9.0); now the SDK's vended `http_request` (`core_tools`), like the rest of the harness. |
| Approval gate | `InterventionHandler`; preflight uses the documented async override (the registry awaits an awaitable `before_tool_call`), so an edit's checks run off the event loop. |
| Per-message context | `ContextInjector` for the running MCP servers (`plugin_host.capabilities_injector`) and the rejection rules (`rejection_memory.constraints_injector`) — ephemeral, never stored, the docs' pattern for just-in-time context. |
| Large-file reads | A `HookProvider` on `AfterToolCallEvent` at the same explicit `HookOrder` as `core_tools`' result shaping, ahead of the `ContextOffloader` plugin. |
| Adaptive effort | A `HookProvider` on `BeforeModelCallEvent` setting the model's `ReasoningControl` — the hook `ollama_cloud.ReasoningThrottle` was written for and never wired. |
| MCP | Servers load through `MCPClient.load_servers` (the SDK's own `mcpServers` parser — VS Code's `type` is mapped to its `transport`). **Resources and resource templates** were never exposed (tools only); `mcp_resources` now reads them through `list_resources_sync`, `list_resource_templates_sync` and `read_resource_sync`. MCP *prompts* (`list_prompts_sync` / `get_prompt_sync`) remain unexposed: a server-authored prompt is instructions, and nothing here should take those from a server. |

## Still open

* **Strands Shell**, and **`background_tasks`** beyond `subagent`, as above.

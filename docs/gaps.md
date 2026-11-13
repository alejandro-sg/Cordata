# Cordata gap analysis: orchestration, context, visibility

Date: 2026-11-10. Baseline: `main` at `0a290bb`.

**Status 2026-11-13 (grep of the code, not live-verified).** Commit 6dac406 claimed to close G1–G13; the code says
otherwise for several. Read the sections below as the original analysis, not as current state.

| Gap | Now |
|---|---|
| G1 live activity | Partial: per-step tapes (`cordata-orchestrator/src/tape.rs`), `cordata show --step/--follow` |
| G2 steering | Partial: `say`/`interrupt` in `control.ndjson`, forwarded to Claude steps; agy can only be cancelled |
| G3 blocked worker asks person | Open: `blocked_reason` exists in `plan.rs` but nothing sets it |
| G4 full context window | Partial: heuristic `compact_history_if_needed` in `engine.rs`, no model summary |
| G5 context visibility | Done: context breakdown event and `/context` |
| G6 chat vs orchestrator | Partial: `/run` from chat; `delegate` is not implemented and `/goldfish` was removed |
| G7 hung worker | Open: `Tape::is_stalled` was never called and has been deleted |
| G8 single trace | Partial: tape trace ids, `show --otlp` (unverified) |
| G9 terminal agent state | Partial: OSC 7501 parsing plus screen rules |
| G10 work lands unseen | Partial: `--land review` |
| G11 repo map for workers | Done: executor adds the repo map to step prompts |
| G12 run tied to one process | Partial: `--detach` (setsid); no `attach`, no `caffeinate` |
| G13 unwired modules | Done: `skills.rs`, `library.rs`, `recursion.rs` deleted |

The question this answers: when a person hands Cordata a problem, can they see what every agent is doing, what it
knows, what it cost, and step in at the right moment? Each gap below names the evidence, why it is a real problem
(not a missing feature for its own sake), and a fix grounded in something that exists and works elsewhere.

Evidence labels: **[code]** read or grepped in this repository; **[herdr]** observed in the installed Herdr binaries;
**[web]** from a web search (secondary sources are marked; check the primary before relying on exact wording);
**[not verified]** a reasonable reading that I did not confirm.

---

## 1. How herdr was examined

| artifact | REA result |
|---|---|
| `/Applications/Herdr.app/Contents/MacOS/Herdr` (59.5 MB, v0.1.12) | `inspect_signature`: `dev.herdr.app`, Developer ID "Herdr, Inc. (2W9652Q7QR)", hardened runtime, entitlements `allow-jit`, `audio-input`, `camera`, `apple-events`. `inspect_macho`: arm64 thin, min macOS 11.0, SDK 15.5, links AppKit, QuartzCore, CoreText, UserNotifications, Security, Carbon, CoreLocation. |
| `~/.local/bin/herdr-core` (`hc`, 6.1 MB) | `inspect_signature`: `dev.herdr.core`, same team, no entitlements, hardened runtime off. |

No decompilation was done (AGENTS.md invariant 5). Behaviour comes from the binaries' own documentation surfaces:
`hc --help`, `hc --skill`, `hc schema` (OpenRPC, 32 methods), `herdr-app --skill`, and `strings`.
`allow-jit` plus the JavaScript and `.d.ts` text embedded in the app binary show the window's chrome is built from
JavaScript "mods" over a GPUI base.

### What herdr is, in one paragraph

Two layers. **herdr-core** runs programs in terminals that outlive the window (a core process plus per-terminal
holders, started through a one-shot launchd job on macOS), on this machine or others over SSH. **herdr-app** only
arranges what the person sees (spaces, tabs, panes, a changes/diff view, git worktrees as spaces, an urgency-sorted
agent list). Agents are ordinary interactive CLIs; nothing is run in one-shot print mode.

### The ideas worth taking

| herdr idea | evidence |
|---|---|
| Agent state is **declared by the program**, not guessed: OSC 7501 reports with states `idle, working, done, blocked, error` and kinds `permission, question, auth`. "Do not guess status from screen text." | `hc --skill`; string `7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth` |
| Every agent stays **watchable, steerable and available for another turn**: `attach`, `read`, `send`, `keys`, `interrupt`. "Run agents interactively, never `pi -p`." | `hc --skill` |
| **Event-driven waiting**: `wait <t>`, `wait --children any|all`, `events.subscribe`. "Never sleep or poll." | `hc --skill`, schema |
| **Parent/child tree with reports**: a child is told how to reach its parent; `<hc from="t7 review">…</hc>` arrives as the parent's next input; hc itself notifies the parent when a child is blocked, errored or exited. | `hc --skill`, `Terminal.parent`, `Terminal.notify` |
| A `turn` counter and `status_ms` per terminal, so "what changed since I looked" is a number. | schema `Terminal` |
| **Templates**: a role is a Markdown file (name, description, command with model; body is instructions). | `hc --skill` |
| The app can **render itself to a PNG** without screen capture (`herdr-app shot`), and `check` loads a mod and renders it once. | `herdr-app --skill` |
| One schema for the whole API (`hc schema`), so other tools can be built against it. | schema |

### What herdr does not do (Cordata's ground)

No sandbox, no write scopes, no approval gate of its own, no quota ledger or cost accounting, no plan, no
deterministic acceptance criteria, no cross-family review, no checkpoints or rollback, no handoff between providers.
Its mods are "trusted JavaScript with command and network access". herdr gives visibility and control over agents it
does not understand; Cordata understands the work but shows less of it. The gaps below are mostly the second half of
that sentence.

---

## 2. Gaps

Ordered by how much each one hides from the person.

### G1. A worker's live activity is thrown away

**Evidence [code].** `Machine::emit` (`tui/crates/cordata-orchestrator/src/machine.rs:630-642`) writes every event to
the ledger *except* `worker`, `state` and `usage`. `worker` events (each streamed tool call and text chunk from
`claude`/`agy`) go only to the in-process listener of the `cordata run` that started the run. The window reads runs by
polling `cordata-run status --json` and `show <id> --json` (`app/crates/cordata-view/src/runs.rs:1`, `:165`: "A run is
read every few seconds, not heard").

**Why it matters.** During a step the field shows "running" and nothing else: not which file is being read, not the
command that has hung, not the reasoning that is going wrong. After the run, the only record of a step is its result.
"Why did step 3 take four attempts?" has no answer. This is the largest visibility hole, and it is exactly what herdr's
`read`/`attach` give for free.

**Fix.**
1. Append worker events to a per-step file, `~/.cordata/orchestrator/<run>/steps/<step>-<attempt>.ndjson`, scrubbed with
   the existing `secrets::scrub`, size-capped (head and tail kept, the rest counted). A file, not `state.db`: the stream
   is high-volume and append-only, and the "no SQLite migration" finding from the 2026-11-03 audit still holds.
2. `cordata show <run> --step s3 [--follow]` tails it. The window tails the same file with a filesystem watch
   (kqueue through the `notify` crate, or GPUI's own watcher) instead of the status poll. This also retires the
   "poll-diff" motion workaround in `runs::Pulse`.
3. Clicking a step node in the field opens its tape.

Grounding: this is event sourcing with the log the run already produces; herdr's `events.subscribe` and `read` are the
same idea [herdr]. LangGraph's checkpointer keeps per-step state for the same reason: replay and fork need it [web].

### G2. A running step cannot be steered

**Evidence [code].** The only mid-run control is `{"cmd":"pause"}` in `control.ndjson`, honoured "at its next safe
point (never mid-call)" (`docs/orchestrator.md`). Direction is added with `cordata resume --direction`, which applies
to the *next unfinished step*. Workers are one-shot (`claude -p`, agy headless).

**Why it matters.** The person watches a worker head the wrong way and can only wait for the step to finish or
fail, then pay for a retry. herdr's `send` and `interrupt` make the correction cost one message [herdr].

**Fix.** Run Claude Code steps with bidirectional streaming (`--input-format stream-json` with
`--output-format stream-json`) so the orchestrator can write a user message into a live step. Add control commands
`{"cmd":"say","step":"s3","text":"…"}` and `{"cmd":"interrupt","step":"s3"}`. Person-originated messages are stamped as
such and recorded in the step tape; bus messages from peers stay fenced as data. For agy (no such channel), `interrupt`
stops the step and the text becomes the retry's direction.

Grounding: the search did not return the stream-json input documentation, so confirm the flags against
`claude --help` before building [not verified]. The Agent Client Protocol offers the same shape as a standard
(`session/update` notifications, `session/cancel`) [web].

### G3. A blocked worker cannot ask the person

**Evidence [code].** agy "cannot run commands headless: anything needing a grant is denied and listed in
`denied_actions`", and a step that changes nothing after a refusal counts as a failure (`docs/orchestrator.md`). Bus
message kinds are `note, question, answer, finding` (`tui/crates/cordata-bus/src/lib.rs:41`), and only peer workers
read them; I found no path from a `question` to the person. The run's states have `paused` and `usage_low` but no
"waiting on you".

**Why it matters.** Today an unanswerable question becomes a failed attempt, a retry on the strongest worker, then a
replan: three paid steps for something one sentence from the person would settle. The window already has the right
surface (the "Needs you" lane and ⌘J) but orchestrated steps never enter it.

**Fix.** A step-level `blocked` state with a kind (`permission`, `question`), mirroring herdr's 7501 kinds. Sources:
a bus `question` addressed to `person`; a denied action that matches no rule. The run parks that step (others in the
wave continue), emits `needs_you`, and the window lists it in "Needs you". The answer is written to `control.ndjson`
and delivered through the G2 channel or as retry direction. A timeout falls back to today's behaviour so unattended
runs still finish.

Grounding: ACP's `session/request_permission` is exactly this request/response, and Gemini CLI implements it [web].
LangGraph's `interrupt()` plus `Command(resume=…)` is the same pattern, with the documented caveat that the node
re-runs from its start on resume, so side effects before the question must be idempotent [web]. Cordata's step
rollback already gives that property.

### G4. The chat engine has no answer to a full context window

**Evidence [code].** `cordata_economizer::compactor` is referenced nowhere outside its own module (grep for
`compactor::` across `tui/crates` returns nothing). The engine sends `history.clone()` whole on every round
(`tui/crates/acp-core/src/native/engine.rs:425`). There is no handling of a context-length error (grep for context
limit / window / "too long" in `acp-core` and the economizer: no hits). ARCHITECTURE.md lists the "head/tail output
compactor" as a feature of the economizer.

**Why it matters.** A long thread ends in a provider error with no way forward except `/new`, losing the working
state. Before that, quality degrades silently as the window fills. The meter shows the fill but nothing acts on it.

**Fix, in order of cost.**
1. Wire the existing compactor: age old tool outputs into digests once they fall out of the recent window. Tool results
   are the bulk of an agent transcript, and this needs no model call.
2. Compaction at a threshold (for example 80 % of the model's window from `cordata_view::models::window`): one model
   call summarises the thread into goal, decisions, files touched, open questions and next step; history becomes
   system + summary + the last few turns. Emit `AppEvent::Compacted { before, after }` and show the summary in the
   transcript as a foldable entry so the person can read and correct what the agent now "remembers".
3. Keep the cache prefix stable across a compaction (`cache_align` already tracks it), so the summary lands after the
   stable prefix.
4. On a context-length error, compact and retry once instead of failing the turn.

Grounding: Anthropic's context engineering post names three techniques: compaction, structured note-taking outside
the window, and sub-agents returning condensed summaries (1,000 to 2,000 tokens) [web, secondary summaries]. Claude
Code exposes a `PreCompact` hook with `trigger: manual|auto`, which is the event shape to copy [web, third-party].

### G5. Nobody can see what is in the context

**Evidence [code].** `Telemetry::usage` sets `context_used = usage.input + usage.output`
(`app/crates/cordata-view/src/telemetry.rs:71`); `context_share` divides by the model's window. That is the only
context figure. The system prompt is assembled from `CORDATA.md`, `.cordata/CORDATA.md`, `AGENTS.md`, `CLAUDE.md`, the
micro-skills and the pruned tool schemas (`engine.rs:49-57`, `:504`), none of it itemised anywhere.

**Why it matters.** "Why did it ignore my instruction?" and "why is this thread expensive?" are both questions about
context composition. The tool pruner makes this worse: a tool the model needed may have been left out, and the person
cannot tell.

**Fix.** Emit `AppEvent::Context` once per request with estimated tokens per part: instructions (by source file),
micro-skills injected, tool schemas (sent and pruned, by name), MCP tools, conversation, tool results, attachments.
`estimate_tokens` exists; the provider's reported input total calibrates it. Surface as `/context` and as a breakdown
on the meter's hover. For orchestrated steps, write the composed prompt (`prompting::compose` output) to the step tape
from G1 so the person can read exactly what a worker was told.

Grounding: Claude Code's `/context` command is the precedent. The context engineering post's framing, the smallest
high-signal set of tokens, cannot be pursued without measuring the set [web, secondary].

### G6. Chat and the orchestrator are two products

**Evidence [code].** No slash command starts a run (`app/crates/cordata-view/src/commands.rs:90-110`; no `/run`). The
window only resumes (`app/crates/cordata-app/src/workspace/field.rs:431`). The chat engine's tools are `read_file`,
`edit_file`, `run_command`, `code_search`, `code_map`, `code_outline` (`acp-core/src/native/tools.rs:257-285`): no
delegate or sub-agent tool. AGENTS.md describes sub-agents "capped at depth 1", `orchestrate` disallowed for child
pods and a `/goldfish` review; `goldfish` exists only as `.cordata/commands/goldfish.md`, and nothing in the engine
spawns a child.

**Why it matters.** The natural flow is: explore a problem in a thread, then say "go build that". Today that means
leaving the window, retyping the goal into `cordata run`, and losing everything the thread established. And the chat
agent cannot protect its own context: every exploratory read lands in the main thread.

**Fix.**
1. `/run [goal]` in both front ends. It starts `cordata-run` with a brief compiled from the thread (goal, decisions,
   files discussed) as the research seed, and records `parent_session` on the run. The field draws the tether from
   thread to run; the run's result comes back to the thread as one entry.
2. A `delegate` tool in the native engine: a child session with fresh history, read-only tools by default, depth 1,
   a token budget, returning a condensed report. The child's transcript is kept and foldable under the tool call
   (visibility is the point), and its usage rolls up to the parent's meter. `/goldfish` becomes a `delegate` with no
   history and the diff as input.

Grounding: the orchestrator-worker pattern in Anthropic's multi-agent write-up, with the measured cost that
multi-agent runs use about 15x the tokens of chat [web]. That number argues for the budget and the depth cap, and for
making delegation explicit rather than automatic. herdr's parent/child terminals with reports are the terminal-level
version [herdr].

### G7. A hung worker is found only by its deadline

**Evidence [code].** The worker loop has one timer: `tokio::time::sleep(task.timeout_s)`
(`tui/crates/cordata-orchestrator/src/workers/cli.rs:482`), then "timed out after Ns" (`:560`). No heartbeat, stall or
no-output detection (grep: only those two lines).

**Why it matters.** A worker waiting on a prompt it cannot see, or looping on a dead network call, burns the whole
step timeout. With three in parallel the person sees three "running" nodes and cannot tell a busy one from a dead one.

**Fix.** Track `last_event_at` per step from the stream. Expose it in `show --json` and draw "quiet for 2m" on the
node. After a quiet threshold (per worker family, learned from the 95th percentile of gaps in past tapes), mark the step
`stalled`; after a second threshold, interrupt and retry with the tail of the tape in the handoff packet. herdr's
`status_ms` and `turn` fields are this signal [herdr].

### G8. No single trace ties a problem to everything done about it

**Evidence [code].** Chat sessions are files under `~/.cordata/sessions`; runs, usage, handoffs and events are tables
in `state.db` (`cordata-ledger`: `quota_usage`, `handoff_packets`, `orch_runs`, `events`); bus messages are a
`bus_messages` event per wave; checkpoints are a shadow git store. No shared id links a thread, the run it led to, the
steps, their attempts, the model calls and the checkpoints. No tracing dependency exists (grep for `opentelemetry`,
`trace_id`, `span` in the manifests: none).

**Why it matters.** "What did this cost end to end, and where did the time go?" needs a join across four stores by
hand. It also blocks any timeline view, which is the standard way people debug agent systems.

**Fix.** Adopt the OpenTelemetry GenAI span model as the *shape* of Cordata's own records, without adding a network
exporter (the app's `deny.toml` bans a network stack, and the product is local-first): a run or turn is an
`invoke_agent` span; each model request a `chat` span carrying `gen_ai.request.model` and
`gen_ai.usage.input_tokens`/`output_tokens`; each tool call an `execute_tool` span with `gen_ai.tool.name` and
`gen_ai.tool.call.id`. Store `trace_id`, `span_id`, `parent_span_id` on ledger events and tape lines. Then
`cordata show <run> --timeline` and a waterfall in the window are queries, and an optional OTLP file export
(`cordata export --otlp`) lets people load a run into any trace viewer.

Grounding: OpenTelemetry GenAI semantic conventions, agent spans section. Still marked Development, and moved to a
dedicated repository in mid-2026 per one source, so pin a version and keep the mapping in one file [web].

### G9. Terminal agents' state is guessed from the screen

**Evidence [code + herdr].** The dock terminal recognises agents by foreground process and decides
working/blocked/idle by "rules over the title and the bottom rows" (`docs/app.md`, `detect.toml`). The string `7501`
appears nowhere in the workspace. herdr's protocol exists precisely because scraping is unreliable.

**Why it matters.** A missed "blocked" means a silent agent waiting on the person. A false one is a wrong
notification. Rules-as-data helps, but each CLI release can break them.

**Fix.** Parse OSC 7501 in `vt.rs` and prefer a declared status over the rules whenever the program has reported one
(herdr's `reported` flag). Emit 7501 from `acp-tui` so Cordata's own client is exact in any host that understands it.
For Claude Code in the dock, ship an optional hook snippet: `Notification` (matcher `permission_prompt`,
`idle_prompt`) and `Stop` map directly to blocked and done [web, third-party hook references; confirm field names
against the official hooks page]. Keep the screen rules as the fallback only.

### G10. Work lands in the project before the person has seen it

**Evidence [code, partly not verified].** A step's sandbox output is syntax-checked and then "harvested into the
project" (`docs/orchestrator.md`; `synthesizer::harvest`). I found no flag in `cordata-run`'s `cli.rs` or `config.rs`
that holds a run's result for review before it touches the working tree. Rollback exists (checkpoints,
`discarded-NNN.patch`), but that is undo, not review. Whether `show --json` carries a per-step diff was not confirmed.

**Why it matters.** Full visibility includes "what exactly changed, by which step, and do I accept it". herdr-app has
a changes/diff view per space and worktrees as first-class places [herdr]; Cordata has `/diffview` for chat but the
orchestrator's output is one merged working tree.

**Fix.** Save each harvested step as `steps/<step>.patch` beside the tape (the harvest already knows the changed
files). Add `--land review`: the run finishes in a `to_review` state with the result on a branch or kept in the
synthesis sandbox, the window's review pane shows it grouped by step with each step's acceptance results, and the
person lands all, some, or none. The "To review" lane on the work board already exists for threads.

### G11. Workers do not get the repository map

**Evidence [code].** `cordata-repomap`/`code_map` is used by the chat engine only; nothing under
`cordata-orchestrator` or `cordata-run` references it. A worker's prompt is role brief, task, scope, handoff, bus note
and schema (`workers/prompting.rs:113-131`), plus the eight most-seen lessons.

**Why it matters.** The bench recorded on 2026-11-03 had the map answering 8/8 questions in about 1K tokens against
3/8 at about 52K for grep-and-read. Every worker currently rediscovers the layout at full price, in every step, on the
most expensive models.

**Fix.** Render a map scoped to the step (query = step task, boosted by the step's `file_scope` and its
dependencies' changed files) and include it in the task packet under a token cap. Record the map in the tape (G5).
Measure with the dispatch log: tokens-in per successful step before and after.

### G12. A run lives and dies with one process

**Evidence [code + herdr].** "No daemon" is an invariant; the bus socket lives "for the life of the run"; a run
continues only by `cordata resume`. herdr keeps terminals alive through window quit, core restart and (with `keep`)
host shutdown, and reaches other machines over SSH.

**Why it matters.** Closing the terminal that ran `cordata run` stops the work. This was deferred on 2026-11-02 as
needing the user's decision, and it still does.

**Options.** (a) Keep the invariant and make interruption cheap: detach `cordata-run` into its own session
(`setsid`, output to the tape from G1) so it survives the launching terminal, and have the window offer Resume for any
run whose process is gone. No long-lived service, so the invariant holds in spirit. (b) A one-shot launchd job per
run, as herdr does for its core [herdr]: survives app quit, exits when the run ends. (c) Run workers inside `hc`
terminals when herdr is installed, and get attach/read/keep for free at the cost of a dependency. Recommendation: (a)
now, (b) if people ask for runs that survive logout.

### G13. Modules that are documented or present but not wired

**Evidence [code].**

| module | status |
|---|---|
| `cordata-economizer::compactor` | no caller (see G4), yet listed in ARCHITECTURE.md |
| `cordata-orchestrator::recursion` (propose-probe-compose) | zero references outside its file |
| `cordata-orchestrator::taste` | zero references outside its file |
| `acp-core::acp_registry` | a catalogue is read by one tool; `BackendTransport` has a single variant, `Native`, so no ACP agent can actually be driven |
| AGENTS.md sub-agent rules, `/goldfish` | no engine support (see G6) |

**Why it matters.** For a product whose promise is that the person can trust what they see, documentation that
describes behaviour the binary does not have is itself a visibility gap. It also misleads the agents that read
AGENTS.md.

**Fix.** For each: wire it, or delete it and correct the document. Add a `scripts/verify.sh` check that every
`pub mod` in the orchestrator and economizer has a caller outside its own file and tests.

---

## 3. One design that closes most of these

G1, G2, G3, G5, G7, G8 and G10 are one piece of work seen from seven sides: **a durable, typed event stream per
step, with a channel back in.**

```text
 worker (claude / agy / native)
      │ stream                         ▲ say / interrupt / answer
      ▼                                │
 steps/<step>-<attempt>.ndjson   ◄── control.ndjson
   (trace_id, span_id, kind, at, payload — scrubbed, capped)
      │ fs watch                 │ query
      ▼                          ▼
 window: tape, "quiet for 2m",   cordata show --step/--timeline/--follow
 Needs you, review by step       cordata export --otlp
```

No daemon, no socket beyond the bus, no new store: files under the run's existing folder, written by the process
that already owns the run.

## 4. Suggested order

| # | work | closes | why first |
|---|---|---|---|
| 1 | Step tape + `show --step --follow` + window tail | G1, part of G5, G7 | everything else reads from it |
| 2 | Wire compactor, threshold compaction, retry on overflow | G4 | a thread that dies is the worst chat failure |
| 3 | `needs_you` state and answers through `control.ndjson` | G3 | turns three paid failures into one question |
| 4 | Context breakdown event and `/context` | G5 | cheap once the estimates exist |
| 5 | `/run` from a thread; `delegate` tool | G6 | joins the two halves of the product |
| 6 | Live `say`/`interrupt` on Claude steps | G2 | needs the stream-json flags confirmed |
| 7 | Per-step patches and `--land review` | G10 | builds on the tape and the review pane |
| 8 | Span ids on events, timeline view | G8 | a schema change; do once tapes are stable |
| 9 | Repo map in task packets | G11 | measurable through the dispatch log |
| 10 | OSC 7501 in and out; hook snippet | G9 | small, removes a class of false states |
| 11 | Unwired module sweep | G13 | an afternoon; stops the drift |
| 12 | Detached runs | G12 | needs your decision on the invariant |

Each item should enter as a three-part brief (task, guardrails, definition of done) with a deterministic check, per
AGENTS.md section 3.

## 5. Not verified

- The exact Claude Code flags for streaming input in headless mode (G2).
- Whether `show --json` already carries per-step changed files or diffs (G10).
- Hook field names and whether `PreCompact` can block: the sources found were third-party and disagree (G4, G9).
- OpenTelemetry attribute names beyond those quoted: the agent conventions are still in development (G8).
- herdr's internals beyond its documented surface: nothing was decompiled.

## Sources

- [OpenTelemetry: semantic conventions for GenAI agent and framework spans](https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-agent-spans)
- [Dash0: OpenTelemetry GenAI semantic conventions explained](https://www.dash0.com/knowledge/opentelemetry-genai-semantic-conventions-explained)
- [Honeycomb: instrumenting AI agents](https://docs.honeycomb.io/send-data/agents)
- [Anthropic: how we built our multi-agent research system](https://www.anthropic.com/engineering/multi-agent-research-system)
- [Anthropic context engineering, secondary summary](https://agentic-ai.readthedocs.io/en/latest/ContextEngineering/anthropic/)
- [Claude Code hooks reference](https://code.claude.com/docs/en/hooks.md)
- [Agent Client Protocol: tool calls and permission requests](https://agentclientprotocol.com/protocol/tool-calls)
- [Phil Schmid: the Agent Client Protocol overview](https://www.philschmid.de/acp-overview)
- [Copilot CLI issue comparing ACP permission behaviour across agents](https://github.com/github/copilot-cli/issues/845)
- [LangGraph: time travel (replay and fork)](https://docs.langchain.com/oss/python/langgraph/use-time-travel)
- [LangGraph: checkpointers](https://docs.langchain.com/oss/python/langgraph/checkpointers)

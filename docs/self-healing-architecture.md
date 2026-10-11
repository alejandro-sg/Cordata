# Self-Healing Multi-Agent Architecture for sierralab-agents

**Status:** design document (no code in this repo implements this yet).
**Scope:** extends the existing Strands + AG-UI + CopilotKit skeleton with (1) a self-learning/self-healing loop that writes and validates new `@tool`s, and (2) a hierarchical Agents-as-Tools sub-agent layer, both gated by Human-in-the-Loop (HITL) at the same points the app already uses.

Every Strands/CopilotKit API named below is grounded in a live MCP doc query against `strandsagents.com` (Strands MCP) and `docs.copilotkit.ai` / `docs.ag-ui.com` (CopilotKit MCP) run for this document — sources are cited inline. Every file/function path is grounded by reading the actual repo. Nothing here is invented from training-data memory of these SDKs.

---

## 0. What already exists (don't rebuild this)

| Concern | Current implementation |
|---|---|
| Agent definition | `backend/agent_common.py` — `build_template_agent()` (AG-UI, session-less) / `build_agent_for_session()` (ACP, session-bound). Single `TOOLS` list: `shell, editor, file_read, file_write, python_repl, http_request, agent_memory, local_swarm`. |
| Model | `build_model()` → `OpenAIModel(base_url=OMLX_BASE_URL, model_id="DeepSeek-R1-Distill-Qwen-32B-4bit")` — 100% local oMLX, no cloud calls. |
| AG-UI transport | `backend/main.py` — `StrandsAgent(agent=build_template_agent(), config=StrandsAgentConfig(...))` served by `create_strands_app(agui_agent, "/")`. This is exactly the `ag_ui_strands` integration path CopilotKit's docs describe for AWS Strands. |
| HITL (backend) | `main.py:27,48` — `_APPROVAL_REQUIRED_TOOLS = ["shell", "python_repl", "file_write"]`, each wrapped `ToolBehavior(interrupt_on_call=True)` in `StrandsAgentConfig.tool_behaviors`. |
| HITL (frontend) | `frontend/app/AgentTools.tsx` — three `useHumanInTheLoop({name, description, render})` calls (from `@copilotkit/react-core/v2`) rendering a shared `ApprovalCard`; `respond({approved: true|false})` unblocks the paused tool call. |
| Skills | `backend/skills/{project-memory,skill-authoring,sub-agent-team}/SKILL.md`, loaded by `AgentSkills(skills=[str(SKILLS_DIR)])` (`strands.vended_plugins.skills`) — **only wired into `main.py`'s `StrandsAgent(..., plugins=build_plugins())`**, not into `build_template_agent()`, because `ag_ui_strands` does not forward `plugins=` when it derives a per-thread agent from the template (documented in code comments at `agent_common.py:94-97` and `main.py:43-45`). |
| Multi-agent | `backend/local_swarm_tool.py` — a `@tool local_swarm(task, agents)` that wraps `strands_tools.swarm.swarm`, injecting `model_provider="openai"` + oMLX settings server-side. |

This document adds a **fourth skill file** (self-healing conventions), a **new hook**, **three new sub-agents**, and **one new tool**, on top of the above — nothing above changes.

---

## 1. Step-by-step implementation plan

1. **Add a generated-skills directory**: `backend/skills_generated/` (distinct from `backend/skills/`, which holds hand-written `SKILL.md` guidance, not executable code). Every file the self-healing loop writes lands here as `<tool_name>.py`, one `@tool`-decorated function per file, importable as a normal Python module.

2. **Add the reflection-gated skill-forge tool**: `backend/skill_forge_tool.py`, a `@tool skill_forge(stack_trace: str, failing_tool: str, attempted_args: str) -> str` function. Internally it does *not* call the model directly — it delegates to the three sub-agents defined in §3, in sequence, per the **Functional Wrapping** pattern Strands' own multi-agent lesson recommends: *"wrap the sub-agent inside a Python function decorated with `@tool`... Because the agent is created inside the function, it starts completely fresh on every call... set the sub-agent's callback handler to `None`"* (strandsagents.com, Lesson 10: Multi-Agent Patterns: Agents as Tools).

3. **Add the validation hook**: `backend/skill_validation_hooks.py`, a class implementing Strands' `HookProvider` protocol:
   ```python
   from strands.hooks import HookProvider, HookRegistry, BeforeToolCallEvent

   class SkillValidationHooks(HookProvider):
       def register_hooks(self, registry: HookRegistry) -> None:
           registry.add_callback(BeforeToolCallEvent, self.validate_generated_skill)

       def validate_generated_skill(self, event: BeforeToolCallEvent) -> None:
           ...  # see §2 "interception" below
   ```
   This is the documented `HookProvider`/`registry.add_callback(EventType, callback)` pattern (strandsagents.com, *hooks* concept page, §2.1/§9), and the mutation surface (`event.selected_tool`, `event.tool_use["name"]`) is the same one shown in the hooks page's own "Tool Interception" cookbook entry (§5.2), which is literally: intercept a `BeforeToolCallEvent`, check `event.tool_use.name`, and either let it through or set `event.selected_tool` to a different (safer) tool.

4. **Wire both into the agent**: in `agent_common.py`, extend `TOOLS` with `skill_forge`, and dynamically append any already-approved tools discovered under `backend/skills_generated/` at process start (import-and-register, not `AgentSkills` — see the hot-reload limitation in §5). In `main.py`, add `hooks=[SkillValidationHooks()]` next to the existing `StrandsAgentConfig(...)` construction, and extend `_APPROVAL_REQUIRED_TOOLS`-style gating to cover any tool name loaded from `skills_generated/` on its first real (non-dry-run) call.

5. **Extend the frontend**: add one more `useHumanInTheLoop({name: "review_generated_skill", ...})` in `AgentTools.tsx`, rendering the generated source (not just a one-line summary like the existing `ApprovalCard`) so a human reads the actual code before approving — same `respond({approved, reason})` contract already used for `shell`/`python_repl`/`file_write`.

6. **Add a fourth `SKILL.md`**: `backend/skills/self-healing/SKILL.md`, documenting for the agent itself the sequence it must follow (think → skill_forge → wait for approval → retry), mirroring how `backend/skills/skill-authoring/SKILL.md` already documents manual skill-writing conventions.

None of steps 1–6 touch `main.py`'s existing `_APPROVAL_REQUIRED_TOOLS` list or `AgentTools.tsx`'s existing three hooks — this is additive.

---

## 2. Lifecycle of a self-healing event

```
 1. ERROR DETECTION
    A tool call in the running agent loop fails (raises, or returns
    ToolResult status="error"). Strands fires AfterToolCallEvent with
    event.result["status"] == "error" — this is the same field the SDK's
    own "Tool Call Retry" cookbook recipe checks (strandsagents.com,
    hooks §8.5) to decide whether to set event.retry = True.
    The orchestrator's system prompt (backend/skills/self-healing/SKILL.md)
    instructs it: on a tool error, do not just retry blindly — call
    skill_forge with the captured stack trace.

 2. REFLECTION (Thinking Tool)
    skill_forge's first internal action is a fresh Agent(tools=[think, ...])
    — the debugger_agent (§3) — invoked with the stack trace + failing
    tool name + args as its one string input. Its system prompt forces it
    to call `think` before producing a diagnosis: think is already an
    installed, working tool in this repo (agent_common.py:28, imported
    from strands_tools and present in TOOLS since before this design).
    Because sub-agent data flow is string-in/string-out with the
    specialist "start[ing] completely fresh on every call" and having
    "absolutely no access to the orchestrator's conversation history"
    (strandsagents.com, Lesson 10), the full trace + args must be passed
    explicitly — nothing is inherited.

 3. SKILL DRAFTING (corrected — see "Correction" note below)
    debugger_agent's diagnosis (a string) is handed to skill_writer_agent
    (§3), a second fresh Agent(tools=[think]) — no file_write/editor. It
    thinks again (validate the fix against the diagnosis), then drafts the
    new backend/skills_generated/<new_tool_name>.py content as its text
    response (FILE: <path>, then the full content) — it does not write
    anything itself. skill_forge returns that draft to the *top-level*
    orchestrator, whose own next file_write call (one of main.py's
    _APPROVAL_REQUIRED_TOOLS, ToolBehavior(interrupt_on_call=True)) is what
    actually pauses for human approval via useHumanInTheLoop("file_write")
    in AgentTools.tsx — this is the first HITL hand-off.

    Correction (found reading the actual ag_ui_strands source, not assumed):
    ToolBehavior(interrupt_on_call=True) is implemented by StrandsInterruptHook,
    which ag_ui_strands' StrandsAgent attaches only to the one Agent instance
    it wraps — never to an Agent constructed elsewhere. A sub-agent built
    inside skill_forge_tool.py (a plain strands.Agent, no AG-UI transport)
    calling file_write directly would therefore write to disk with *no*
    human approval at all — the original version of this document, and of
    skill_forge_tool.py itself, incorrectly assumed that call was gated. It
    was not; both have been corrected to draft-then-orchestrator-writes,
    per sub_agent_common.py's DENIED_TOOLS reasoning (a sub-agent can never
    be given a tool it has no way to be interrupted on).

 4. INTERCEPTION (Hooks)
    On the *next* turn the orchestrator (or the validator_agent, §3)
    attempts to actually call the newly-written tool. Because it is
    registered in the agent's tool registry, Strands fires
    BeforeToolCallEvent before invocation. SkillValidationHooks
    .validate_generated_skill(event) runs:
      a. AST-parses the source file for banned imports/calls
         (no `os.system`, no `subprocess`, no `eval`/`exec` unless the
         generated tool's own contract explicitly needs one — flagged
         for extra human scrutiny either way).
      b. Confirms the function is @tool-decorated with a parseable
         signature and non-empty docstring (Strands requires this to
         expose the tool to the model at all).
      c. Runs a sandboxed dry-run of the function against synthetic args by
         calling strands_tools.python_repl's underlying function directly
         (skill_validation_hooks.py:182) — a bare Python call, not a real
         agent tool-dispatch, so (same correction as §2 step 3) it is NOT
         itself HITL-gated. Its only real protection is (a)/(b) above
         already having screened the source for banned imports/calls before
         this runs — flagged here, not fixed, since sandboxing this
         properly is a separate, larger change than this document covers.
      d. On failure at (a) or (b): sets event.selected_tool to a fixed
         "skill_rejected" stub tool and rewrites event.tool_use so the
         model receives a structured rejection reason instead of running
         the untrusted code — the exact substitution pattern shown in
         the hooks doc's "Tool Interception" example (§5.2: mutate
         event.selected_tool / event.tool_use["name"] inside the
         callback).
      e. On failure at (c): does not swap the tool, but the resulting
         AfterToolCallEvent will show status="error", which the
         orchestrator's own retry logic (below) picks up.
      f. On success at all three: lets the call proceed unmodified.

 5. EXECUTION (+ bounded self-correction)
    The (now-intercepted-and-passed, or rejected) tool executes.
      - If AfterToolCallEvent.result["status"] == "error" and the
        orchestrator's own AfterToolCallEvent hook has retried fewer
        than N times for this tool_use_id, it sets event.retry = True
        (strandsagents.com hooks §8.5's exact mechanism) to re-run the
        SAME already-written code once, in case the failure was
        transient (e.g. a flaky HTTP call inside the generated tool).
      - If it fails again after N retries, or failed step 4's static
        checks, the orchestrator escalates: it does NOT try to write a
        third variant unsupervised. It surfaces the situation as a
        second HITL hand-off (§4) asking a human to approve, edit, or
        discard the generated skill outright.
      - Only a human-approved, hook-passed, successfully-dry-run tool
        is ever promoted to "trusted" (i.e. exempted from the
        interception dry-run on subsequent calls, though it stays
        listed under skills_generated/ for audit).
```

---

## 3. Sub-agent schema (Agents-as-Tools)

All three follow Strands' **functional wrapping** pattern: a `@tool`-decorated Python function that constructs `Agent(model=build_model(), tools=[...], callback_handler=None)` fresh on every call (per strandsagents.com Lesson 10 — fresh context, silent streaming, string in/string out). All three sit *underneath* `skill_forge`, which is the tool the orchestrator actually sees; the orchestrator never calls them directly, preserving the documented "orchestrator is a bottleneck / specialists cannot talk to each other" constraint deliberately (§5).

| Sub-agent | Wrapped as | Toolset | Input (string) | Output (string) | Notes |
|---|---|---|---|---|---|
| `debugger_agent` | `@tool` inside `skill_forge_tool.py` | `think`, `file_read`, `http_request` | Stack trace + failing tool name + args, verbatim | Structured diagnosis: root cause, whether it's transient vs. code-level, proposed fix sketch | `http_request` lets it consult local docs/README if needed; no shell/file_write — it never mutates anything. |
| `skill_writer_agent` | `@tool` inside `skill_forge_tool.py` | `think` only | The diagnosis string from `debugger_agent` | Drafted `FILE: <path>` + full content, as text — nothing written | No file_write/editor: a sub-agent built here has no AG-UI transport, so it has no mechanism to be interrupted (see correction in §2 step 3). The orchestrator's own file_write call on the returned draft is the real, gated write. |
| `validator_agent` | `@tool` inside `skill_forge_tool.py`, invoked by `skill_forge` after `skill_writer_agent` returns | `python_repl`, `file_read` | Path to the generated file | Pass/fail + reasoning, independent of the `BeforeToolCallEvent` hook's own checks | A second opinion by design: the hook (§2 step 4) is the *enforced* gate; `validator_agent` is a *pre-flight* opinion the orchestrator can request before ever attempting the real call, cheaper than tripping the hook's rejection path. |

(The `local_swarm` tool this section once pointed to for open-ended parallel work has since been replaced by `orchestrate` — `docs/orchestrator.md`.)

---

## 4. HITL hand-off points (exact map)

| # | Point | Layer | Mechanism | Existing or new |
|---|---|---|---|---|
| 1 | Any `shell` / `python_repl` / `file_write` call (incl. `skill_writer_agent`'s write, and `validator_agent`'s sandboxed dry-run) | Strands backend | `ToolBehavior(interrupt_on_call=True)` in `StrandsAgentConfig.tool_behaviors` (`main.py:48`) | **Existing** |
| 1 | Same, rendered | CopilotKit frontend | `useHumanInTheLoop({name: "shell"|"python_repl"|"file_write", render})` + `ApprovalCard` (`AgentTools.tsx`) | **Existing** |
| 2 | First real (post-hook-pass) invocation of a `skills_generated/` tool | Strands backend | Extend `_APPROVAL_REQUIRED_TOOLS`-style gating to any tool name resolved from `skills_generated/` | **New** |
| 2 | Same, rendered with full source shown | CopilotKit frontend | New `useHumanInTheLoop({name: "review_generated_skill", render: <shows source>})` | **New** |
| 3 | Retry budget exhausted or static validation failed | Strands backend | Orchestrator raises an explicit `Interrupt` (via the `HumanInTheLoop` intervention handler's interrupt/resume mode, or a raw `interrupt`) asking whether to discard/retry-with-edits/escalate | **New** |
| 3 | Same, rendered | CopilotKit frontend | Reuse the `review_generated_skill` card with a "failed validation" state | **New** |

The SDK-native primitive underneath hand-off #3 is Strands' `HumanInTheLoop` intervention handler (strandsagents.com, *human-in-the-loop* concept page): *"pauses agent execution before tool calls to request human approval... Pass it to `interventions`... configurable allow-lists, trust mode, and collection mode."* Its config surface — `allowed_tools` (bypass list, supports `"*"`), a custom `evaluate`/risk-classifier callback, and Interrupt/Resume vs. Stdio vs. Custom-UI-callback collection modes — is the natural place to encode "skills_generated/* always requires approval regardless of trust mode" as a risk-classifier rule, distinct from the simpler `ToolBehavior(interrupt_on_call=True)` flag the repo currently uses for its three built-in dangerous tools. Underneath both, `strands.interrupt.Interrupt` (`id`, `name`, `reason`, `response`) and `InterruptException`/`PendingToolExecution` are what actually suspend and resume the agent loop without re-calling the model.

On the frontend, `respond()`'s payload shape is free-form JSON (the existing cards use `{approved: boolean}`; the new `review_generated_skill` card should use `{approved: boolean, reason?: string}` so a rejection can carry structured feedback back into the orchestrator's next turn, exactly as `strands.interrupt.Interrupt.response` is designed to carry the human's answer back into the resumed agent).

---

## 5. Edge cases and limitations (grounded in this repo + MCP research)

- **Local model tool-calling reliability is the dominant risk, by explicit design decision to stay on-device.** The repo's own README documents that the local oMLX model (`DeepSeek-R1-Distill-Qwen-32B-4bit`) sometimes *narrates* tool calls in prose instead of invoking them. The self-healing loop is a four-step chain (`think` → `file_write` → hook validation → execution) where every step depends on the previous one actually firing as a real tool call, not prose. Mitigations that don't require a different model: keep `debugger_agent`/`skill_writer_agent`/`validator_agent` system prompts extremely short and directive (the existing `SYSTEM_PROMPT` in `agent_common.py` is deliberately short for the same reason — "a longer prompt measurably hurt this small model's tool-calling reliability"); cap retries low (N=1–2) since a model that narrates instead of calling will not self-correct by repetition; and treat any turn where the expected tool call didn't fire as an immediate escalation to the HITL layer rather than a silent no-op.

- **Fixed: a generated tool no longer needs a restart to become callable within the same conversation.** This used to say `AgentSkills` can't hot-reload one into the running session (still true — `AgentSkills`' "next conversation" semantics never apply here, same reasoning as `backend/skills/skill-authoring/SKILL.md` warns for hand-written skills). The actual fix isn't `AgentSkills` though: `skill_validation_hooks.py`'s `SkillValidationHooks.hot_reload_generated_skill` (an `AfterToolCallEvent` hook) imports a just-written `skills_generated/*.py` file and calls `agent.tool_registry.process_tools([...])` on it directly, right after a human-approved `file_write`/`editor` call succeeds — a real, direct `strands.Agent` API (confirmed reading `strands/tools/registry.py`), not `AgentSkills` at all. Scope, precisely: the *current* AG-UI thread (ag_ui_strands caches one live `Agent` per thread and reuses it across turns — `self._agents_by_thread`, confirmed reading its source) gets it immediately; a different or brand-new AG-UI thread still needs a restart, since `ag_ui_strands` derives every thread's agent by deep-copying the one template `Agent` built once at process start. ACP has no such limitation — `agent_common.build_agent_for_session()` now re-scans `skills_generated/` fresh on every call (it's called once per session anyway), so every new ACP session picks up anything written since the process started, restart or not. One real, documented gating gap this introduces: a hot-loaded tool's first real call still gets `validate_generated_skill`'s AST-scan + sandboxed dry-run (unconditional on tool name, independent of the rest of this), but *not* the interrupt-based human-approval pause every restart-registered generated tool gets, because that gate is driven by a name list (`_APPROVAL_REQUIRED_TOOLS`/`APPROVAL_REQUIRED_TOOLS`) computed once, statically, before the hot-loaded tool's name existed — closing that too would mean mutating that live list safely, which wasn't verified carefully enough to ship in the same change. Separately: `sub_agent_common.resolve_tools`' denylist was fixed to use `agent_common.current_generated_tool_names()` (a fresh directory scan) instead of the same static snapshot, specifically because a sub-agent has *no* human-in-the-loop gate at all — a hot-loaded generated tool reaching one via the old static-set denylist would have been a real, silent escalation, not just a missing-approval-once gap.

- **Sub-agents are stateless by design, not by accident.** Strands' own guidance: *"the specialist's context resets between calls... If the orchestrator calls the researcher agent twice during a conversation, the researcher does not remember the first call."* `debugger_agent` cannot recall a previous failed fix on retry #2 unless `skill_forge`'s Python code explicitly concatenates prior attempts into the new prompt string — this is a design obligation on `skill_forge_tool.py`, not something the SDK gives you for free.

- **No guaranteed execution order between sub-agents.** Because coordination is model-driven ("you cannot guarantee a strict execution order between specialists" — Lesson 10), the debug→write→validate sequence is enforced by `skill_forge`'s own imperative Python control flow (call debugger_agent, then skill_writer_agent, then validator_agent, in that fixed order inside one `@tool` function), *not* by giving the top-level orchestrator all three as separate tools and hoping it calls them in the right sequence. The `BeforeToolCallEvent` hook is the actual security backstop if this ordering is ever bypassed — not the model's cooperation.

- **`AfterToolCallEvent.retry` replays the identical tool_use_id with identical code.** It's documented for "transient errors, timeouts, or flaky external services" (hooks §8.5) — appropriate for `validator_agent`'s sandboxed dry-run hitting a flaky dependency, inappropriate for "the generated tool is logically wrong." Retrying a logically-broken tool via `event.retry = True` just reproduces the same failure; a logic failure must route back through `skill_forge` (new diagnosis → new file) or to a human, never through the retry hook alone.

- **A pending browser-side approval can be orphaned by a backend restart.** `FileSessionManager` already persists conversation/session state to `backend/sessions/` across restarts, but an in-flight `useHumanInTheLoop` interrupt is live state in the running process. If a self-healing loop's own generated tool crashes the backend process before a human responds to a `review_generated_skill` card, the frontend's pending approval has nothing to resume into — the next request against that thread starts a fresh agent loop with no memory of the unapproved skill file sitting on disk. Any production version of this needs the pending-approval state itself written to disk (e.g. alongside the generated `.py` file, a sidecar `.pending.json`) so a restarted process can detect and re-surface it, rather than relying solely on in-memory interrupt state.

- **AG-UI shared state (`useCoAgent`-equivalent) is available but unused in this repo.** `StrandsAgentConfig(state_context_builder=...)` and the AG-UI protocol's State Management Events exist specifically to stream structured, non-chat state (e.g. "self-healing in progress: step 2/4, awaiting validation") from a Strands agent to the CopilotKit frontend. This repo currently has zero `useCoAgent` usage — every signal is either chat text or a HITL tool-call pause. Surfacing the self-healing lifecycle (§2) as live state rather than buried in chat transcript would need this wiring added; it's a real gap for observability, not a blocker for correctness.

---

## Sources consulted (MCP)

- `strandsagents.com/docs/user-guide/concepts/agents/hooks` — event lifecycle table, `HookProvider`, Tool Interception (§5.2), Tool Call Retry (§8.5).
- `strandsagents.com/docs/user-guide/concepts/agents/interventions/human-in-the-loop` — `HumanInTheLoop` intervention handler, config surface.
- `strandsagents.com/docs/api/python/strands.interrupt` — `Interrupt`, `InterruptException`, `PendingToolExecution`, `_InterruptState`.
- `strandsagents.com/docs/learning/multi-agent-patterns-agents-as-tools` (Lesson 10) — direct delegation vs. functional wrapping, string-only data flow, statelessness, orchestrator-as-bottleneck.
- `docs.copilotkit.ai/integrations/aws-strands/quickstart`, `.../shared-state/in-app-agent-read`, `.../shared-state/in-app-agent-write`, `.../frontend-tools` — `ag_ui_strands`, `StrandsAgent`, `StrandsAgentConfig`, `create_strands_app`.
- `docs.copilotkit.ai/reference/hooks/useHumanInTheLoop`, `.../reference/v1/hooks/useHumanInTheLoop` — tool-based HITL, `respond`, `ToolCallStatus`.
- `docs.ag-ui.com/sdk/python/core/overview` and `.../events` — `RunAgentInput`, State Management Events.

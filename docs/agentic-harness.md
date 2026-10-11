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
   │                (every edit preflighted first: syntax · ruff · LSP · optional tests)
   ├── read-only:  code_search, code_outline, file_read, lsp_*, http_request, agent_memory,
   │               environment_profile, mcp_resources (list)
   ├── multi-agent: local_swarm, local_graph (sub-agents, denylisted toolset)
   ├── remote:      a2a_client (A2A protocol)
   ├── plugins:     per-project MCP plugin tools (.mcp.json, .cursor/, .vscode/, mcp.json)
   ├── injected:    the clock, the MCP servers running now, this person's rejection rules
   └── hooks:       secret redaction, untrusted-result guard, Laya outcome learning,
                    self-healing loop, skill validation, LSP edit hooks, large-file outline,
                    adaptive reasoning effort
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

## Laya Guard: every call, by every agent (laya_guard.py)

The router's risk reading is **per turn**: Laya reads the user's message once, and the
orchestrator's reading arms the approval gate for everything that turn then does. That cannot be
allowed to *waive* a prompt (a turn that began "summarise this README" would wave through whatever the
README talked the model into running — `laya_risk.auto_approvable`'s docstring says so), but it was
also all that stood between `auto` mode and a destructive call, for every agent: a specialist was
judged by the *orchestrator's* turn, and a model that drifted mid-turn was not judged again.

Laya Guard scores **each tool call, at the moment it is about to run, for the agent making it**:

* **Rules** — a deterministic reading of the concrete command, path or patch: `rm -rf` (a build
  directory is routine, `src` is not), `git clean -fdx`, `git push --force`, `curl … | sh`, `sudo`,
  a write outside the project or to `~/.ssh`, `.env` or the harness's own settings, a patch that deletes
  a file or thousands of lines, a Claude Code task that asks for destruction. These are the floor,
  because the shipped Laya checkpoints are near chance until fine-tuned.
* **Laya** (`laya.callGuard: full`, the default) — the same `risk` question the router asks about a
  turn, asked about this one call on weights that are already loaded (it never triggers a load). Its
  reading can only **raise** a score, and only when it calls the call destructive at the confidence the
  router itself needs to arm a turn (0.8). It cannot talk a rule's score down.

The score is compared with **that agent's gate**: the router's destructive threshold (0.8), tightened
by the agent's own Lab dial. At or above it the call asks a person — **in every mode, `auto` included**.
Below it, nothing is added and nothing is waived: whether a call asks at all is still the mode, the
permission rules, the vault and the forced asks. What auto mode lets through that was not trivially
safe (score ≥ 0.2) is written to the ledger (`auto_approved`, with the agent, the call and the score).

Each agent's last reading, peak, and counts of calls auto-approved and held ride on the Lab pod and
show on the team board. `laya.callGuard: rules` keeps only the deterministic half; `off` restores the
turn-level behaviour exactly. Covered by `_laya_call_guard_smoke_test.py`.

Sub-agents in a swarm, graph or team have no approval gate, so they are kept away from every tool that
needs one: `sub_agent_common.DENIED_TOOLS` (now including `apply_patch`, which edits project files)
plus the orchestrator-only tools and every MCP server's tools.

## Learning from turns (reflect.py, promote.py, learned.py, prompts.py, optimize.py, bandit.py, trajectory.py, laya_checkpoints.py)

The harness records every turn (`outcomes.py`, `docs/evals.md`). Eight pieces read that record and change
behaviour in a bounded way; the governing rule is the same for all of them — **nothing learned may lower a Guard
score, remove a prompt, or waive a deny.** They can only add a hint, re-rank choices that were already allowed,
and change prose that a person can read in full.

- **Reflection (`reflect.py`)** — a turn that failed or that the person reworked becomes a drafted lesson: a
  tool-less model call writes a `SKILL.md` into `~/.sierralab/learned/<project>/staging/`, which is *not* live.
  The goal and error lines it is shown are wrapped as data and secret-masked first, and the drafting call has no
  tools at all. `learning.reflect` (default on) is the switch.
- **Promotion (`promote.py`)** — a staged candidate is run through the eval suite and promoted only if it raises
  the score on the dev split **without breaking anything** there *and* leaves the held-out third clean. Two kinds
  go through the same gate: a **skill** (read on demand — the model sees one line and opens the body only when it
  applies) and a **prompt overlay** (`prompts.py` — prose appended to a session's system prompt on *every* turn).
  `learning.promotion: off | shadow | live` — `shadow` writes the verdict and promotes nothing, so a person can
  read what the gate would have done. Trust-gated: a cloned repository cannot turn it on for itself.
- **Prompt overlays (`prompts.py`)** — the same four places as a skill (staging / live / retired / rejected) at
  `~/.sierralab/prompts/<project>/`, private, and in the ledger. A live overlay is appended to the system prompt
  as its own named block (`# How this project is worked on`), after the project prompt, the workspace context and
  the person's instructions, so the boundary is unambiguous to a person reading it. Because an overlay is in front
  of the model on every turn it is the more powerful lever, so it is filtered by exactly the same rules a skill is
  (`textfilter.py`: no URL, no download piped into a shell, no destructive or privileged command, no "ignore your
  instructions", no "skip the approval", no credential, no prompt delimiter, no control character) and its turns
  are scored against the project baseline the same way.
- **Optimization (`optimize.py`)** — the other loop: an artifact that is *already* live and whose turns scored
  below par gets one rewrite. It is shown the current text plus the summaries of those turns, and the variant is
  **staged, never swapped in place** — it faces the same gate, and the live one keeps working until the variant
  has measurably beaten it. `learning.optimizePrompts` (default off, trust-gated) is the switch; a store's own
  daily draft limit bounds it.
- **Artifact health (`learned.py`, `prompts.py`)** — the stores. A live skill's or overlay's turns are scored
  against the project's own baseline (`_baseline_reward`); one whose turns score worse is retired.
- **The contextual bandit (`bandit.py`)** — *which* harness choice to make, learned from turn outcomes. Three
  points are acted on in v1: **route** (dispatch · hint · none, among the ones the base policy already allowed),
  **engine** (the Claude Code CLI path vs. the orchestrator's own tools) and **ask** (whether to add an approval
  on a call the Guard already scored at or above its gate). The reward is the turn's own `outcomes.score`
  re-weighted as `verifier success − λ·cost − μ·correction`, with cost counting tokens, time and Claude Code
  dollars (`learning.banditLambda`, `learning.banditMu`). The table is coarse and per-project, in
  `~/.sierralab/bandit/`, keyed by mode × risk × tier × target kind so a person's few turns still fill a bucket;
  `learning.banditMinPulls` keeps a choice on its default until every arm has enough outcomes, and
  `learning.banditExplore` is ε. `learning.bandit: off | shadow | live` — `shadow` records what it *would* have
  chosen and acts on nothing.
  - **It is a bandit, not a policy.** It never decides what is allowed, only which of a set of already-permitted
    options tends to turn out better. `bandit.py` deliberately does not import `laya_guard`, `laya_risk` or
    `secret_guard`; the one read inside the approval handler (`acp_server._ApprovalHandler._bandit_ask`) is an
    OR-term that can only *add* a `Confirm` and whose precondition is the Guard's own gate, so the effective
    threshold stays `max(guard_floor, bandit_floor) = guard_floor`. An abstained decision is left untouched —
    fail-closed is not the bandit's to reopen.
  - **The ledger is the trajectory.** The chosen arms ride on the turn's `route["choices"]`; the reward arrives
    late through the existing `outcome_update`, exactly like kept/reverted, so a turn teaches nothing until it
    has been reconciled (`learning.bandit` is checked fresh each turn, so it lands without a restart).

- **Trajectory (`trajectory.py`)** — the ledger and Laya's triage log joined into the four row shapes a trainer
  reads: off-policy rows `(context, arm, propensity, reward)` for a model that would replace the bandit's tabular
  scorer behind `choose`, Laya's behaviour-cloning rows in the exact schema its training script reads, and
  `(request, tools, reward)` SFT / preference pairs for the small local models. Nothing is trained here; this is
  the export. A row exists only for a turn whose reward is known, and the train / eval split is a hash of the
  request text, so adding turns never moves an existing one. `make laya-export`; row shapes in `docs/evals.md`.
- **Checkpoints (`laya_checkpoints.py`)** — a fine-tuned Laya checkpoint is a **candidate like a skill**: a
  registry (`~/.sierralab/laya/checkpoints.json`) maps an id to its directory and its measured numbers, the same
  A/B gate judges it on the same tasks, and only a pass may go live — `activate` refuses a checkpoint whose last
  verdict did not promote. `LAYA_MODEL` resolves through the registry (a live checkpoint wins over the stock repo;
  an explicit `LAYA_MODEL` still overrides both). This is the one artifact that changes a *model's weights*
  rather than prose — and still nothing structural: it can re-rank routes Laya already offered and nothing else.
  The shipped checkpoints are near chance (`docs/laya-triage.md`), so until one is trained on real turns and
  clears the gate, the bandit's Laya-derived points are weak features; its reward is outcomes-based, so it is not
  blocked by that, only less informed.

Covered by `_bandit_smoke_test.py`, `_learning_smoke_test.py` and `_optimize_smoke_test.py`
(`make learning-test`) and `_trajectories_smoke_test.py` (`make laya-test`, and in CI — it is hermetic). The
bandit test asserts the Guard-floor property by import and behaviour introspection rather than review; the
optimize test proves a staged candidate really is in force for the eval run it is judged on (the `config` →
`run_task` → `sandbox` path), and that a prompt naming "skip the approval" is refused by the same filter a skill
gets. The trajectories test proves the Laya rows are byte-for-byte the exporter's, that no request lands in both
splits, and that the registry refuses to make a regressing checkpoint live.

## Checkpoints: `auto` you can undo (checkpoints.py)

`auto` removes the pause before a change; a checkpoint puts back the ability to take the change back.
Before any tool call that may change files — `file_write`, `editor` (not `view`), `apply_patch`,
`python_repl`, `claude_code_engineer`, a generated tool, and any shell command that is not recognisably
read-only — the harness snapshots the project. It does so in every mode and for every agent, after the
decision and before the call: a call held for approval is snapshotted before the prompt appears, a call
refused (plan mode, a deny rule, the vault) is not snapshotted at all.

* **Where.** A *shadow* git repository under `~/.sierralab/checkpoints/<project hash>/` (0700), with the
  project as its work tree. Your own `.git` is never read for this or written: no commits, branches, index
  changes or hooks of yours are involved, and it works in a folder that is not a repository. One commit per
  call, its message carrying the session, turn, agent, tool and a redacted one-line summary; one more after
  each turn (`end`).
* **What it covers.** Everything not git-ignored, not a dependency/build directory, not a secrets file, not
  over `checkpoints.maxFileMb` (default 8). Edits are reverted, created files removed (and the directories
  they leave empty), deleted files recreated. Executable bits and symlinks survive.
* **What it does not.** Ignored paths (`node_modules`, `.venv`, `target`, your `.gitignore`); `.env*`, keys
  and `.npmrc`, which are deliberately never copied; the inside of a nested git repository; anything outside
  the project directory (reported as `outside`, once, so you know); non-file side effects (a database, a
  network call). A project over 50,000 files, your home directory and system directories are not
  checkpointed. A snapshot that fails never stops the turn — but the turn's record and the client both say
  it was **not protected**, once per reason, and the ledger has it.
* **Undoing.** `/undo` previews the latest turn that has not been undone — the files, what each would do, and
  any that were edited *after* the turn ended (their changes would be overwritten); `/undo apply` does it;
  `/undo <turn> [apply]` names an earlier one; `/checkpoints` lists turns. Restoring snapshots the present
  first, so an undo can itself be undone, and is refused while a turn is running (two writers, one tree). Every
  applied restore is audited (`checkpoint_restore`). Over ACP: `_sierralab/checkpoints/{list,undo,restore}`.
* **Bounded.** History past 800 snapshots is rebuilt from the newest 400 and garbage-collected, off the turn's path.
* **Not a permission.** A checkpoint approves nothing and lowers no gate; it only makes what got through
  recoverable. Laya Guard still asks first. `checkpoints.enabled` is trust-gated: a cloned repository cannot
  turn the undo off.

Covered by `_checkpoints_smoke_test.py` (real git: snapshots, restore, your own `.git` untouched, ignored
files, odd names, refusals, upkeep) and `_checkpoints_acp_smoke_test.py` (an `auto` turn, undo, refusal while
running, off, unprotected).

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

### Approvals on the ledger (`state.db`)

An interrupt is in-memory state in Strands, which means the harness's own two-place report of a
turn's interrupts (the `tool_interrupt_event` and `AgentResult.interrupts`) can put the *same*
call in front of the user twice, and nothing outlives the process that was parked on a prompt.
`_resolve_interrupts` therefore stages each prompt in `~/.sierralab/state.db` before showing it
and closes the row with the answer — plus Laya's risk score and band, where the turn was scored —
and appends both transitions to the append-only `events` table under kind `intervention`. A row
is looked up before a second prompt is raised, and an identical one is replayed rather than
re-asked (`_settled_intervention`; unmatched rows re-prompt). `--resume` reads the same table and
says which approvals were left unanswered when the process died, instead of dropping the question
silently. Detail and the fail-closed reasoning: `THREAT_MODEL.md`, "Human-in-the-loop gates".

A prompt is a *front door*, not a GUI modal. Under `make daemon`, `session/request_permission`
is tapped into a `PendingPermission` and fanned out to whoever is listening; the TUI, the Lab's
Containment Modal and — when the environment configures one — the Discord bot are all equal doors,
and `Supervisor._settle` makes the second answer to a prompt harmless (the child is written the
first, the loser gets a "already decided"). That is what closes the unattended gap: with no client
attached, a destructive call used to leave the child blocked forever, because nothing was ever
going to answer. The Discord door (`daemon/discord_bot.py`) posts a card built from the same masked
`PendingPermission.meta` the GUI modal uses, led by *what, where and why* — the child adds what the person
asked and what the agent said just before the call (`intervention_view.build`: `goal`, `said`, `project`) —
with the fenced command or diff, Laya's reading and reason as evidence beneath. Its buttons say what they do
(Allow / Deny / Deny & stop, plus Always allow when the child offered a rule and "Allow routine steps in this
chat" on a routine ask), the callback checks the configured operator id *before* it touches any state, and the
card collapses to one line when answered — by Discord or by another door. It is outbound-only (no inbound port,
no webhook) and off unless the token and both ids are set. What it is and isn't: `THREAT_MODEL.md`, "Discord
remote control".

Discord is also a way to *talk* to an agent. `daemon/chat.py` is a minimal ACP client: a conversation is one
`acp_server.py` child the daemon owns (`Session.owner`), driven with `initialize` / `session/new` /
`session/prompt`, its reply read off the `session/update` frames the supervisor taps, its cost off the
`_sierralab/turn` receipt the child sends when a turn ends. An owned session has no subscriber (nothing is
backlogged for a reader that will never come), is never returned by a terminal's directory lookup, and is
refused by the socket — one writer per stdin. Idle children are reaped and the conversation resumes from disk.

## Token economy (context_diet.py)

Every model call re-sends the system prompt, all tool schemas and the whole conversation. Measured on a real
session (`acp_server._build_session`, no plugins): 29 tools, ~43 KB of schema, ≈10.8k tokens per call.
`ContextDiet` is one hook provider with three levers, none of which removes a capability:

1. **Cold tools** are hidden at the start of a turn unless the user's message mentions their subject (a regex per
   tool), they were used in the last three turns, or the Laya route is `web`; the prompt carries one line saying
   what is hidden, and `enable_tools` loads one for the agent's next step. A plain turn: 43.6 KB → 15.1 KB (−65%).
2. **One result is set aside, not thrown away** (`context.toolOutputChars`). Strands' `ContextOffloader` writes the whole
   result to `~/.sierralab/offloaded/<session>/` and the model reads a head+tail preview plus a reference;
   `retrieve_offloaded_content` returns the slice it asks for. `context_offload.HarnessOffloader` adds what the SDK's
   plugin lacks: a preview that keeps the *tail* (exit codes live there), a private store (0700/0600, written on the
   host, never routed through the agent's sandbox), credentials masked before anything is written, a veto list
   (a specialist's report is the answer, not output to be skimmed), thresholds re-read per result, and age-based
   cleanup instead of the SDK's loop-cycle eviction. The retrieval tool is cold until something is offloaded, then
   loads itself. With `context.offload: false` — or if the store cannot be written — the result is cut head+tail by
   `ContextDiet`, as before. The read-back passes through the untrusted-data fence.
3. **Old output ages**: at `BeforeModelCallEvent`, tool results (and huge tool inputs) older than the newest 8
   messages are shortened in the copy the model is about to read — idempotent, structure-preserving (every call
   keeps its result), never the first message.

A fourth lever is a plugin set, not a hook — see "Skills: semantic search with Laya deciding" below.

Three more come straight from the Strands SDK (`docs/strands-sdk-audit.md` lists everything it offers and what was
adopted): **per-turn limits** (`context.maxSteps`, default 50; `context.maxTurnUsage`) pass Strands' `limits` and pause
a runaway turn instead of letting it spend — a limit is per invocation and a checkpointed turn is many, so the budget
is enforced at the `after_tools` checkpoint, the one pause where every tool call has its result; **native
cancellation** — `session/cancel` calls `agent.cancel()` on the running agent (never an idle one: the signal outlives
the call), so the loop stops at its next checkpoint and a tool that polls `tool_context.cancel_signal` stops
mid-flight — `claude_code_engineer` kills its CLI within a fraction of a second, instead of running to its timeout
on your Claude Code usage; and **proactive compression** — the history is summarised before the call that would
overflow the window, not after the provider rejects it (every model path now carries a window, assumed 128k when
nothing says).

Specialists get levers 2 and 3 (their kit is fixed). Elsewhere: the observer reuses its last reading when a
project's evidence fingerprint is unchanged (`evidence_fingerprint`), `claude_code_engineer` defaults to a
mid-size model at medium effort and is hidden unless the turn is about a large change or names Claude, and
Discord shows each turn's *actual* tokens rather than an estimate of what was saved.

## The shell, the editor and the web tools (core_tools.py)

`strands_tools.shell`, `editor` and `http_request` are deprecated (strands-agents-tools 0.8.9 — "an error log in v0.9.0");
the SDK ships their replacements — `strands.vended_tools.shell`, `file_editor`, `http_request` — and a new `web_fetch`.
The agent now uses those. Model-facing names stay `shell`, `editor` and `http_request` (and `editor` keeps its
parameters), because the approval gate, Laya Guard, the settings rules people have written, the checkpoints, the diff
preview, the LSP hook and the vault's egress rule all key on them. What changed, and what `core_tools.CoreToolHooks`
fits around the SDK's tools:

* **Size**: shell 2,901 → 535 bytes of schema, editor 5,708 → 1,273, `http_request` 4,109 → 1,042 — ~2,000 tokens off the
  full kit. Together with the cold-tool diet, a plain turn carries 9.0 KB of tool schema (it was 44 KB).
* **A shell result is text with an exit code.** The SDK reports a JSON string with status `success` even for exit 3, which
  would read as a pass to the router's p(ok), outcomes, self-heal and the stall guard. A hook ordered ahead of every other
  `AfterToolCallEvent` hook (explicit `HookOrder`) rewrites it to `Exit Code: N` + output + `[stderr]`, status `error` on
  non-zero; a timeout is `Exit Code: 124` with what the command had printed.
* **Defaults for a coding agent**: 900 s shell timeout (`SHELL_DEFAULT_TIMEOUT`; the SDK's 120 s kills a test suite), a
  relative editor path resolved against the project (the SDK's editor answers with an error and a hint).
* **A stop stops the command.** Strands' cancellation is cooperative: `agent.cancel()` stops the loop around a tool,
  never inside it. `HarnessSandbox` is the SDK's own host environment (`sh -c`, own process group, killed as a tree on
  timeout) plus one thing — a running command watches the agent's cancel signal and, when set, closes its stream (which
  kills the group) and returns what it printed with exit code 130. Esc, `!stop` and *Deny & stop* end a runaway build;
  the Debugger specialist's `shell` too.
* **`http_request`** is a thin shim over an `httpx` client the operator supplies; `FreshClient` supplies one *per call*:
  redirects followed, a 30 s ceiling, and the process environment read at call time — the Agent Vault's proxy and CA
  variables can appear after the session began. The SDK raises on a 4xx/5xx and drops the body, where an API says what
  was wrong; the hook puts it back, fenced as untrusted. Success reads `HTTP 200 OK`, the few headers that matter, the body.
* **`web_fetch`** fetches a page and has an *analyst* model answer a prompt about it, so the page never enters the main
  agent's context (`context.webFetch: agentic`, default; `markdown` returns the page). The analyst runs on the fast model
  tier when `MODEL_TIER_FAST` is set. The analyst is an agent of its own, so its tokens are in nobody's metrics — a
  `UsageTally` behind a thin `CountingModel` wrapper writes them down, and the per-turn receipt and the turn budget
  (`context.maxTurnUsage`) include them. Swarm nodes do not get it. Both web tools are egress tools (an unreachable
  vault refuses them), fenced as untrusted, and cold.
* **The clock.** `current_time` is gone; the date and time ride along with each message the person sends
  (`core_tools.clock`, a `ContextInjector` — ephemeral, ~20 tokens, never stored, so a resumed session is told the time it is
  *then*). `calculator` and `think` are gone too (the SDK deprecated them for `shell`/`python_repl` and native reasoning).
  Arithmetic is the model's own or `python_repl`'s (which asks, like any code run).

Behaviour that differs from the old tools, on purpose: each shell call is a fresh `sh` (`cd` and variables do not persist —
chain with `&&`; the tool's description says so), there is no PTY, `undo_edit` / `pattern_replace` / `find_line` are gone
(checkpoints are the undo), and `http_request` no longer offers the old tool's auth modes (credentials are never the
model's to hold). The shell is still the host, not a sandbox: `HarnessSandbox` adds no isolation.

## Finding code without reading it (code_search.py, code_nav.py)

The way an agent used to answer "where is `build_session` used?" was to `file_read` the files it guessed at — a
2,800-line module is ~30k tokens to learn that the answer is three lines. Three pieces replace that, all read-only
(they never ask, and run in plan mode). The two tools are *cold* (2.3 KB of schema): loaded when the message reads like
code work (find, where, references, a file name, a function, a fix…), on a specialist or team route, or — for
`code_outline` — the moment a read comes back as an outline; "hello" does not carry them.

* **`code_search`** — a regex (or literal) search over the project that returns only `path:line: text`, sorted, capped
  at 60 matches (400 at most) and honest about the cap. ripgrep when it is on `PATH` (it respects `.gitignore` exactly as
  the person's own `rg`); otherwise a Python engine over `git ls-files --cached --others --exclude-standard` with the same
  output. Never outside the project, never a secrets file (`.env`, keys). `brew install ripgrep` for the fast path.
* **`code_outline`** — a file's classes, functions and methods with line ranges, or (`symbol="Class.method"`) just that
  symbol's source, numbered. Python by `ast` (exact, decorators included); Rust, Go, JS/TS, Java/Kotlin/C#/Swift, C/C++,
  Ruby, shell and Markdown by a structural scan (brace depth that ignores braces in strings and comments); the optional
  `tree_sitter_language_pack` is used when installed.
* **`LargeFileHooks`** — a whole-file read of a source file over `context.outlineLines` (500) lines returns its outline
  and how to read what is needed instead of the file. Ordered with `core_tools.SHAPE_ORDER`, ahead of the offloader, so
  the file is never set aside either. A ranged read, a short file and data files are untouched.

The LSP tools (`lsp_definition`, `lsp_references`, `lsp_hover`, `lsp_symbols`) give the semantic answer once a position
is known; the system prompt and the workspace context steer the agent search → outline → LSP, not read. Fixed on the way:
the built-in plugins (the LSP tools, the memory store) used to wait for `/trust` like a repository's MCP server, so an
untrusted folder had no `lsp_*` tools at all while its language servers were running. Covered by
`_code_nav_smoke_test.py` (both engines, every language, the hook through the real ACP server, and "find references"
through the real server: `code_search` → `code_outline` → `lsp_references`, no whole-file read).

## Preflight: edits are checked before anyone is asked (preflight.py)

An agent's broken edit used to be the person's problem: the approval card showed a diff that did not parse, or — in
`auto` — it landed and the next command failed. Now every `apply_patch`, `file_write` and `editor` change by every agent
sharing the session's gate (orchestrator, Debugger, Architect) is checked **in front of** `_ApprovalHandler`, on a worker
thread (the intervention registry awaits an async handler, so the event loop keeps streaming and honouring cancel):

1. **syntax**, in process (Python `compile`, JSON, TOML, YAML);
2. **ruff** on the proposed text through stdin with `--stdin-filename`, so the project's configuration applies (ruff
   reads code, never runs it; only a `ruff` on `PATH`, or a trusted folder's own `.venv/bin/ruff`);
3. **the language server** on the *unsaved* text (`LspClient.check_texts`): every file of one edit changes together in the
   server's memory, so a rename and its callers are judged as one change; no `didSave`, so no `cargo check` and nothing
   executes; only a server that is already running; the server's view is put back afterwards;
4. optionally **tests** (`preflight.tests`, trust-gated): a command in a throwaway copy with the edit applied; a failure
   the copy without the edit shows too is not blamed.

Each check compares against the file *as it is*, so only what the edit newly breaks counts. Errors about **other** files
(an import that does not resolve yet — the agent may be about to add it) are notes, never a refusal. A failing edit is
not applied and nobody is asked: the agent gets the problems as the call's result and tries again, up to
`preflight.maxRetries` (3) per file per turn. Then the turn is told to stop editing and report, later edits that turn are
refused, and with `preflight.rollback` the files the turn changed are restored from its first checkpoint
(`checkpoints.undo_turn` — the shadow repository, never the person's own git; the present is checkpointed first, so the
rollback can itself be undone). The self-heal loop holds the Debugger's sandbox-verified patches to the same static checks.
Results are cached per (call, file contents), so a call re-entered after an approval pause is not checked twice.
Covered by `_preflight_smoke_test.py` (including "a deliberate syntax error is fixed by the agent and *then* asked about")
and `evals` (all 42 oracle tasks pass with preflight on).

## Conversations on disk (session_store.py)

A session is a `SnapshotSessionManager` snapshot — `backend/sessions/session/<id>/scopes/agent/default/snapshots/snapshot_latest.json`,
one atomic JSON file written at the end of every invocation (so every approval and every checkpoint pause), 0600 under a
0700 directory — instead of a `session_<id>/` directory of per-message files. It restores messages, agent state, the
conversation manager's state (a compaction summary) and any interrupt in flight, and supports immutable time-travel
checkpoints (`snapshot_trigger`; not used yet). Two deliberate differences from the SDK's manager: the **system prompt is
not persisted** (this harness builds a fresh one every session — project instructions, specialist roster, settings — and
plugins add and remove blocks in it, so restoring last week's would undo every change since and double any block that was
in it), and a message the harness appends by hand (the settled results of a cancelled call, a dispatched specialist's
exchange) is persisted by capturing the agent (`session_store.record`), because the snapshot manager's `append_message` is a no-op.

**Going back.** One immutable snapshot (the SDK's time-travel feature) is written when each turn *starts*, labelled with the
prompt about to run; the turn count lives in the agent's own state, so restoring a checkpoint restores the count with it. Three
surfaces: the TUI's `/back [n | list | to <number>]`, Discord's `!back` (the same arguments), and over ACP
`_sierralab/conversation/{list,rewind}` (`turns`, `index` or `to`). Going back restores messages, agent state and the
conversation manager's state; the agent forgets the turns and **the files are not touched** (`/undo` is for files — they are two
undos, deliberately). The state it leaves is saved as a checkpoint first, so `/back to <number>` undoes a `/back`. It is refused
while a turn is running, audited (`conversation_rewind`), announced to clients (`_sierralab/conversation`, so a transcript can
rewind too — the TUI drops the forgotten turns from the screen), and a chat that is asleep in Discord is woken first. A session
keeps its newest 50 checkpoints, and cannot go back past where checkpoints began (a session migrated with history starts
at that history). "Forget" means *the agent* forgets: the turns are still on disk in the checkpoints.

**Migration** is exact and additive. `session_store.migrate` opens a legacy directory through the SDK's own
`FileSessionManager`, so the format is whatever the SDK says it is, and writes the snapshot; the legacy directory is left
byte-for-byte as it was, so a rollback to the old manager loses nothing. It happens on first use of a session, or in bulk —
`python session_store.py migrate --all` (and `status`). A legacy session that cannot be migrated (a corrupt file) keeps the
legacy manager and still loads. Measured on the 420 sessions in this checkout: 420 migrated in 1.2 s, every one restoring
to messages, state and conversation-manager state identical to its original.

## Skills: semantic search with Laya deciding (skill_search.py)

The 49 skills that ship (plus a project's learned ones) were all listed in the system prompt, in full, on every model
call — 19 KB, ~5k tokens, and a model choosing among forty-nine descriptions chooses badly. The Strands docs call the
intended pattern *progressive disclosure*; `skill_search.py` makes "a little" mean "the few that fit this request",
using two engines for what each is good at:

- **Semantic search finds candidates.** Every skill (name, description, the opening of its instructions) is embedded
  once with the harness's own local `multi-qa-MiniLM-L6-cos-v1` (cached by content hash in `~/.sierralab/skill_index.json`,
  0600); a request is embedded and the nearest skills — cosine plus a 0.3-weighted keyword overlap — form a shortlist.
  On 27 natural requests against the shipped skills the right skill was in the nearest four 25/27 times by embeddings alone
  and 27/27 with the keyword boost. With no sentence-transformer it falls back to keywords.
- **Laya decides.** Laya is asked one `choice` question over the shortlist plus an explicit `none`
  ("which skill, if any, should guide this request?") — the router's own forward pass, abstention rule and `Calibrator`
  (`skill/<name>`). Measured live: ~25 ms, and when confident it named the right skill every time (`design-an-interface`
  0.85, `tdd` 0.93, `resolving-merge-conflicts` 0.96, `caveman` 0.96); it abstains on vague requests rather than guess.
  Confident → that skill is listed first and the model is told, for that call only, that it fits; unsure or `none` → no claim, the
  shortlist is listed and the model decides. Whether the model then activated the skill Laya named is recorded back into
  the calibrator, so the decision engine learns the way the router does.

Built from documented Strands surfaces only (looked up in the Strands docs): `AgentSkills.set_available_skills` re-lists the
selection each turn ("the XML block is refreshed before each invocation"), the `skills` tool still activates by name,
`ContextInjector` carries the one-line hint into the model input without touching stored history, and the `Plugin` base
class with `@hook` / `@tool` packages it. `search_skills` is the way out of a wrong shortlist: semantic search over the whole
library, whose results become activatable. Every failure lists everything (never a skill out of reach). Laya not yet loaded
(it warms in the background) means semantic search alone — a turn never waits on it. Switches: `context.skillSearch`,
`context.skillsListed`.

Considered and not used: the experimental `context_manager="auto"` (it replaces the conversation manager this harness
tunes; its tool-result threshold, 1,500 tokens, is adopted as the default cap), `ContextOffloader` (lossless, but its
retrieval tool is 2.7 KB of schema on every call — more than it saves unless it is kept cold), and a `MemoryManager` store
for skills (generic memory injection, no decision step). Tests: `backend/_skill_search_smoke_test.py`.

## Rejection memory: a "no" becomes a rule (rejection_memory.py)

`reflect.py` learns from turns that went badly; a single rejection taught only one bit (the turn's reward fell). Now,
when a person — TUI, GUI, Discord — rejects an `apply_patch` / `editor` / `file_write` / `shell` / `python_repl` /
`claude_code_engineer` call, the change is kept (a masked unified diff, the paths, the request, and the person's note: the
GUI's *Reject & Isolate* field travels from the daemon in ACP's `_meta`), and on a background thread the
**ReflectionAgent** — one tool-less model call that reads all of it as data — states the preference as one rule
as a prohibition ("Do not … / Never … / Avoid …", with what to do instead when the data says), scoped to the whole
project unless the note limits it, or `NONE`. The note is the strongest evidence (with no usable model reply, the note itself is the
rule); a model-written rule must pass the same `textfilter` a learned skill does. Rules live in
`~/.sierralab/preferences/<project>/rules.jsonl` (0600) and are mirrored into the knowledge graph as `USER_PREFERENCE` nodes
joined to the project and the files they are about (when `kg.enabled`; LanceDB or Cognee). The same rule twice is
counted, not duplicated; at most 40 live per project.

Before each message, the rules that bear on it — all of them while there are few, otherwise those about a file the
message names and then the nearest by embedding — ride along as **hard negative constraints** (a `ContextInjector`
on *every* model call — an edit is usually written after a read or two, and a constraint folded only into the turn's
first call is gone by then; ephemeral, never stored). A fail-closed "no client" rejection is not a person's decision and teaches nothing. A rule only
ever constrains: nothing here touches an approval, a Guard score or a deny rule. `learning.rejections` (trust-gated) is the
switch; `python backend/rejection_memory.py list | forget <id>`. Covered by `_rejection_memory_smoke_test.py`, including
the gate: reject a `print()` patch with a note, ask for a similar change in a new session, and the proposal (and the one
approval) is the logging one.

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

### The loop closes: memory, stalls, more than shell, resuming

Four additions turn the loop from "fixes this failure" into "gets better at failures it has seen":

- **Remembered fixes (`fix_memory.py`, `selfHeal.memory`).** A patch that passes sandbox verification is
  written to the ledger against the failure's *signature* — the command family (`pytest`, `cargo test`,
  `npm run build`) plus the salient error lines with paths, numbers and addresses normalised away — and the
  project. The next time a similar failure happens *in the same project*, the remembered patch is tried first,
  in the sandbox, before the Debugger is asked anything. It is never trusted, only tried: a patch that no longer
  applies, or applies and still fails, is recorded against the fix and the Debugger runs as it would have (told
  what memory tried). A fix that failed its last two uses, or whose reliability — (successes + 1) / (uses + 2) —
  drops under a third, retires. A patch containing a credential-shaped string is not stored. Recalled text is
  capped, stripped of control characters and labelled unverified; another project never sees it.
- **Stall detection (`stall_guard.py`).** The same call with the same result three times gets a `[loop]` note in
  its result; the fifth identical call is refused (`was not run` — a nudge, the count restarts); the third
  refusal in a turn tells the model to stop and report. Two calls alternating, and a file edited back to an
  earlier version (A → B → A), are noticed too. The turn's outcome record carries the stall and its reward falls
  (`stalled`, −0.15). Per agent and per turn: a specialist's loop is not the orchestrator's, and a new turn starts clean.
- **More than a failing shell command.** A failed `apply_patch` hunk now comes back with the file's *current*
  lines around the spot it expected, so the next patch is written against what is there. A check the Claude
  Code CLI left failing inside its own session is reproduced in a throwaway copy; if it really fails it is
  healed like a shell failure, and if it passes in a clean copy the model is told so instead of reporting done.
- **Auto-resume (`selfHeal.autoResume`).** When a session loads with a turn a crash interrupted, the backend says so
  (`_sierralab/resume`) and the TUI continues it on its own once idle — at most twice from the same checkpoint
  (a turn that got further has a new checkpoint and a fresh count), never for a checkpoint over thirty minutes
  old, and never past an approval: a resumed turn asks exactly as the original did. `/resume-turn` always works.

Covered by `_fix_memory_smoke_test.py`, `_stall_guard_smoke_test.py`, `_self_heal_smoke_test.py` and `_auto_resume_smoke_test.py`.

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

`strands-agents[a2a]` is pinned in `backend/requirements.txt` (a2a-sdk 0.3.26 with 1.57.2).

## Around the LLM

- **Language servers (`lsp_bridge.py`)** — per-project language servers started off the request
  path (`lsp.enabled`, default on): diagnostics, symbols, definitions, references as read-only
  tools and post-edit diagnostic hooks, so the model checks its edits against the real compiler
  instead of guessing. Status rides the event bus.
- **Plugins (`plugin_host.py`)** — per-project MCP plugin servers (`backend/mcp-servers/`,
  `mcp_servers.json`), started in the backend's process, their tools reaching the orchestrator;
  a confident `plugin_<name>` route scopes the turn's toolset to that plugin. **Discovery** also reads the
  other editors' declarations — `.cursor/mcp.json`, `.vscode/mcp.json` (its `servers` / `type` shape and
  `${workspaceFolder}`; a server needing a VS Code `${input:…}` prompt is skipped with the reason) and `mcp.json` —
  as *project* servers: started once the folder is trusted, the harness's own `.mcp.json` winning a name clash.
  What is running *now* is told to the model with each message (a `ContextInjector`, so a server that started in the
  background is known from the next message; an untrusted folder's servers are named as waiting for `/trust`), and
  **`mcp_resources`** exposes the MCP spec's other half — each server's resources and RFC 6570 resource templates,
  through the SDK's own `list_resources` / `list_resource_templates` / `read_resource` (listing never asks; a read asks
  like the server's tools, is refused while the vault blocks egress, and is fenced as untrusted).
- **Environment profile (`env_profile.py`) and adaptive effort (`adaptive_effort.py`)** — one line of the workspace
  context says what the project is (size class, files, lines, languages, dependency manifests and how many each
  declares, a multi-package workspace) and how much unified memory the machine has; `environment_profile` adds the
  toolchain versions (`--version` of what is on `PATH`; the project's own interpreter is never run). On Ollama Cloud the
  orchestrator's `reasoning_effort` is then chosen per model call from the size class and Laya's complexity reading
  (strong work in a medium/large project → `high`, standard work in a big one → `medium`, trivial work stays put),
  never below the role's level (the Lab's dial and `OLLAMA_ROLE_ORCHESTRATOR` are the floor), never `max` unless
  chosen. `reasoning.adaptive: false` keeps the fixed level. Covered by `_environment_awareness_smoke_test.py`.
- **Patches (`patch_tool.py`, `edit_preview.py`)** — unified-diff `apply_patch` (atomic,
  root-confined, context-repairing) through the normal edit-approval gate, plus ACP diff
  previews of edits *before* they happen.
- **Secrets (`secret_guard.py`, `prompt_security.py`)** — env/Keychain values are masked in tool
  output, redacted before anything reaches Laya or the logs, and shell denies for Keychain
  reads are built in. Untrusted tool results get the injection-hardening treatment.
- **Egress (`vault_proxy.py`)** — with `vault.enabled`, tool subprocesses carry placeholder
  credentials and reach the network through the Agent Vault proxy, which substitutes the real
  values on the way out; `install()` runs at session start (after `settings.py` has taken this
  harness's own keys out of the environment, so a placeholder can never shadow them —
  `RESERVED`). A vault enabled and unreachable is a hard denial of `shell`, `python_repl` and
  `http_request` in every mode, not a prompt. `vault_proxy.launch_under_vault` is the wrapping
  API for a caller that owns its argv; there is none in this repo yet, since `shell` comes from
  `strands_tools` and offers no argv to wrap — hence the environment. Scope and the sandbox
  caveat: `THREAT_MODEL.md`, "Agent Vault is egress-only".
- **Knowledge graph (`kg/`)** — off by default (`kg.enabled`). The turn that just finished is
  reduced to entities and edges deterministically — project, session, tools, the files the
  path-bearing tools named, error signatures — and handed to a `KnowledgeGraphSink`. The default
  `LanceGraphSink` keeps them in `backend/memory.kg/` (`graph.lance` upserted by node id, so a
  repeat sighting refreshes `last_ts` rather than duplicating) and writes `nodes.jsonl` /
  `edges.jsonl` mirrors even without `lancedb`; `CogneeGraphSink` sits behind the same three
  methods but is unverified against an installed Cognee and is not a dependency. No prompts and no
  command lines go in: that is `state.db`'s and `memory.traces`' job, and the graph is the
  structure over them.
- **Event bus (`harness_events.py`)** — the producers (plugins, LSP, self-heal, sub-agent tiers)
  emit thread-safe events; the ACP server forwards them as `_sierralab/<kind>` extension
  notifications. Unknown extensions are ignorable per the ACP spec, so Zed is unaffected; the
  TUI renders them as progress bars.
- **Observer (`daemon/observer.py`)** — daemon-only, off by default (`observer.enabled`). While
  nobody is attached it reads each known project with a fixed `git` argv list plus the tails of
  `SIERRALAB_OBSERVER_LOGS`, hands that to a read-only agent (`observer_runner.py`, in a child
  process because `agent_common` chdirs at import), and writes `nightly_briefing.md`. The agent
  holds `file_read` and nothing else — the daemon runs the commands, the model never
  chooses one — and the briefing is written by the parent from the child's stdout, so there is no
  path from the agent to the filesystem. A client attaching kills a cycle in flight. Proposals are
  staged `proposed`, never `staged`, so they never read as a pending approval. Scope:
  `THREAT_MODEL.md`, "The background observer writes nothing"; keys: `settings.md`.

## The Experimental Lab

`lab.py` observes every agent through Strands hooks and gives the person at the terminal
controls over them (E-Stop, a per-agent Laya dial, guidance, the HITL station); `ollama_cloud.py`
is the provider it was built around. Design, protocol and limits: [`experimental-lab.md`](experimental-lab.md).

## Where things are applied and tested

Settings keys for all of this (`laya.*`, `selfHeal.*`, `lsp.enabled`, model/temperature) are in
[`settings.md`](settings.md), applied at session start by `_apply_startup_preferences` — except
`observer.*`, which the daemon re-reads every cycle so a change lands without a restart, and
`learning.*`, which the turn path re-reads every turn. Each
piece has a hermetic smoke test — see `_self_heal_smoke_test.py`, `_team_pipeline_smoke_test.py`,
`_a2a_client_smoke_test.py`, `_laya_router_smoke_test.py`, `_harness_loop_smoke_test.py`,
`_plugins_lsp_smoke_test.py` and friends; the daemon's transport and its new `/observe` route in
`_daemon_smoke_test.py`, the observer's read-only toolset and preemption in
`_observer_smoke_test.py`, the vault gate in `_vault_proxy_smoke_test.py`, the graph adapter in
`_kg_smoke_test.py`, the learning loop's clamp in `_bandit_smoke_test.py`, its reflection/promotion half in
`_learning_smoke_test.py`, prompt overlays and optimization in `_optimize_smoke_test.py`, the trajectory export
and the checkpoint registry in `_trajectories_smoke_test.py`, and the denylist and
graph-structure assumptions in `_subagent_denylist_smoke_test.py` and
`_local_graph_structure_smoke_test.py`. Web-UI and TUI behavior is the same backend — the
AG-UI front door (`main.py`) gets the same tools; only the specialist/laya dispatch is ACP-side.
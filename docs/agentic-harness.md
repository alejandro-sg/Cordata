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
going to answer. The Discord door (`daemon/discord_bot.py`) posts an embed built from the same
masked `PendingPermission.meta` the GUI modal uses — pod, tool, Laya score, the fenced command or
diff, redaction count — with Approve/Reject buttons whose callback checks the configured operator
id *before* it touches any state. It is outbound-only (no inbound port, no webhook) and off unless
the token and both ids are set. What it is and isn't: `THREAT_MODEL.md`, "Discord remote control".

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
  holds `file_read` and `think` and nothing else — the daemon runs the commands, the model never
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
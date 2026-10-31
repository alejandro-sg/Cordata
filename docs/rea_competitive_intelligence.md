# Competitive intelligence: Claude Code and Antigravity (2026-10-30)

Static inspection of the locally installed arm64 Mach-O executables, to learn which *interaction patterns* the two
established agent CLIs converged on for background shells, hooks and permissions — and port the patterns (not code)
into Cordata. Codex was out of scope.

| Target | Path | Size |
|---|---|---|
| Claude Code 2.1.296 | `~/.local/share/claude/versions/2.1.296` | 240.7 MB |
| Antigravity CLI | `~/.local/bin/agy` | 180.7 MB |
| Antigravity language server | `/Applications/Antigravity.app/Contents/Resources/bin/language_server` | 166.4 MB |

**Method.** The `rea` MCP server's `open_binary` succeeded on Claude Code, but `search_strings` hit its 330 s provider
timeout on a 240 MB binary (full Hopper analysis), so the string inventory was taken with `strings -n 8`
(386k / 524k / 531k strings) and searched for the features below. Nothing was executed, decompiled or copied; only
identifiers, enum names and user-facing text are cited.

## Claude Code

* **Background shell.** The Bash tool takes `run_in_background`; output is read with `BashOutput` and a task is ended
  with `KillShell` (older name `KillBash`), both keyed by a `shell_id`. User-facing text: a background command "keeps
  running after your reply ends, until it finishes or reaches its timeout". Subagents that are synchronous refuse it
  ("run_in_background … not available in this context").
* **Timeouts and read-back.** `BASH_DEFAULT_TIMEOUT_MS` and `BASH_MAX_TIMEOUT_MS` environment overrides;
  `BASH_MAX_OUTPUT_LENGTH` "only sizes the read-back window" — the log is not the window.
* **Process groups.** Children are spawned `detached: true` (their own group), so a kill reaches the tree.
* **Hooks.** Events `PreToolUse` (111 references), `PostToolUse`, `Stop`, `SubagentStop`, `UserPromptSubmit`,
  `SessionStart`, `PreCompact`, `Notification`. Exit code `2` is the blocking error: "the action the event guards (a
  tool call, a permission request, a prompt) is blocked", ignored for async hooks and on Stop. Hooks may be async.
* **Compaction.** Environment knobs ending `…AUTO_COMPACT` and `…AUTO_COMPACT_WINDOW` (full names not recovered from
  the fragments); an explicit window "is set and takes precedence" over the setting.

## Antigravity

* **Run lifecycle.** `CASCADE_RUN_STATUS_{IDLE,BUSY,RUNNING,CANCELING}`; commands `COMMAND_STATUS_{RUNNING,COMPLETED}`
  with `COMMAND_OUTPUT_PRIORITY_BOTTOM` (the tail matters most) and `BACKGROUND_FOR_SECONDS` /
  `BACKGROUND_TO_FOREGROUND` (a foreground command that runs long is moved to the background, and can come back).
  `BACKGROUND_WORKTREE`, `BACKGROUND_CITC_WORKSPACE_CLONE`: background agents work in a cloned workspace.
* **Permission decisions are attributed.** `AUTO_RUN_DECISION_{USER,MODEL,SYSTEM}_{ALLOW,DENY}` plus
  `DEFAULT_{ALLOW,DENY}`; scopes `PERMISSION_SCOPE_{GLOBAL,WORKSPACE}`; refusals carry a reason
  (`BLOCK_REASON_GITIGNORED`, `BLOCK_REASON_OUTSIDE_WORKSPACE`).
* **Hooks** (`exa.hooks_pb`): `CommandHook`, `HookToolCall`, `HookCheckpoint`, `HookInjectedStep`, `HookSystemMessage`,
  `HookEphemeralMessage`, `HookErrorMessage` — a hook can inject a step or a message, not just veto.
* **Orchestration** (`exa.cortex_pb`): typed trajectory steps (`CORTEX_STEP_TYPE_RUN_COMMAND`, `…COMMAND_STATUS`,
  `…TASK_BOUNDARY`, `…LINT_DIFF`), `CortexStepCreditReason` (per-step credit), `CHECKPOINT_STRATEGY_SAME_MODEL`,
  `AGENT_MODE_VERIFICATION`, `MCP_TOOL_BACKGROUND_MODE`.

## Ported into Cordata

| Pattern | Where |
|---|---|
| Background shell with a task id, bounded head+tail read-back, per-task timeout, process-group kill, exit notice, 0600 summary | `backend/background_tasks.py` (`background_shell`, `task_output`, `task_kill`), `_cordata/tasks/{list,status,kill}` |
| The background tool guarded exactly like the shell (approval, Laya Guard, `shell(...)` rules, checkpoints) | `acp_approval.py`, `laya_guard.py`, `settings_store.TOOL_ALIASES`, `checkpoints.writes_files` |
| PreToolUse / PostToolUse / Stop command hooks, exit 2 blocks, JSON on stdin, matcher, trust-gated | `backend/lifecycle_hooks.py`, `hooks.*` settings (both schemas) |
| Background work in a cloned workspace | `backend/orchestrator/synthesizer.py` (APFS clonefile sandboxes for parallel waves) |
| Per-step credit | `backend/orchestrator/rlaif.py` |

**Not ported yet:** moving a foreground `shell` call to the background when it runs long (`BACKGROUND_TO_FOREGROUND`)
needs a shell tool of Cordata's own rather than the SDK's vended one; hooks that inject steps or messages; attributed
permission decisions (who allowed it) in the approval log.

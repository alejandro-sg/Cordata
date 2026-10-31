# Competitive intelligence: Claude Code and Antigravity (2026-10-30)

Static inspection of the locally installed arm64 Mach-O executables, to learn which *interaction patterns* the two
established agent CLIs converged on for background shells, hooks and permissions — and port the patterns (not code)
into Cordata. Codex was out of scope.

| Target | Path | Size | SHA-256 (prefix) | Verified Toolchain (`rea` `inspect_macho`) |
|---|---|---|---|---|
| Claude Code 2.1.296 | `~/.local/share/claude/versions/2.1.296` | 240.7 MB | `c9b5341637be…` | Bun + JavaScriptCore (`__BUN` 171.8 MB + `__TEXT` 66.6 MB) |
| Antigravity CLI | `~/.local/bin/agy` | 180.7 MB | `96dd362d5583…` | Go + CGO/Objective-C (`__TEXT` 126.7 MB, `__go_buildinfo`, deterministic zeroed `LC_UUID`) |
| Antigravity language server | `/Applications/Antigravity.app/Contents/Resources/bin/language_server` | 166.4 MB | `8b660ea42c9e…` | Go + CGO (`__TEXT` 113.4 MB, `__go_buildinfo`, deterministic zeroed `LC_UUID`) |

**Method.** Structural Mach-O, load-command, segment, dylib, and code-signing inspection was executed directly via the `rea` MCP server (`open_binary`, `inspect_macho`, `inspect_signature`, `close_binary`) without launching full Hopper disassembly:
- **Claude Code 2.1.296** (`ev_5cd7527fe11a374d3ccbb9bab1926d7b9b9a0cf9708cf47368c47d2d06a27a57`, `ev_455f531ec4cb5420dbf02a07cd37928c1e576893b5f2a758eddfbf8a8c94419b`): signed `com.anthropic.claude-code` (`Q6L2SF6YDW`, Anthropic PBC, `min_os: 13.0`, `sdk: 26.5`). Embeds Bun/JSC (`__jsc_int`, `__bun_builtins`, `__wtf_config`, `__jsc_opcodes`) and a `171,769,856`-byte `__BUN` payload (71.4% of binary). Hardened runtime requires 5 entitlements (`allow-jit`, `allow-unsigned-executable-memory`, `disable-library-validation`, `automation.apple-events`, `device.audio-input`).
- **Antigravity CLI `agy`** (`ev_de0f83af6f17dc542f5cb46b8343b5067a5ae5de8a1c2755ac45bccf66f177dc`, `ev_788e9d8393985b758755650a3ceaac4d694455a0530fd77fdfd27963614a65c2`): signed `cli` (`EQHXZ8M8AV`, Google LLC, `min_os: 12.0`, `sdk: 26.4`, Blaze/Bazel reproducible zeroed `LC_UUID` + `lld 9999.0`). Compiled Go (`__lrodata_gopcln`, `__go_type`, `__go_func`, `__go_buildinfo`, `__go_fipsinfo`, `__go_module`) linked against `AVFoundation`, `AudioToolbox`, `CoreAudio` (voice), `Security`, and `IOKit`, running under hardened runtime with **zero entitlements** (`entitlements: null`).
- **Antigravity `language_server`** (`ev_9e067c06a5cc77a39100f64fba7ac9e925bf769a70eb835542cd80561b4901af`, `ev_4e4707ed53fc4b801fed1f5ed0a0ea46b2e7f3a0493c540a2fdaee2465ca8df5`): signed `language_server` (`EQHXZ8M8AV`, Google LLC), headless Go daemon (omits `AVFoundation`/`CoreAudio`) with a single `com.apple.security.cs.allow-jit` entitlement.
- Because `rea` `search_strings` triggers full Hopper analysis (330 s timeout on >160 MB binaries), string symbol inventories were extracted via `strings -n 8` (386k / 524k / 531k strings). Nothing was executed, decompiled or copied; only structural metadata, identifiers, enum names and user-facing text are cited.

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

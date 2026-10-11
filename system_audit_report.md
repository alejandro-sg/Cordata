# System audit report — Phase 11 & 12

Date: 2026-10-05 · Branch: `feat/acp-tui-laya-secrets` (uncommitted working tree) · SDK: `strands-agents 1.57.2`,
`strands-agents-tools 0.8.9`.

What this pass did: audited the harness for security, memory and backpressure faults and fixed what it found; brought
the remaining hand-rolled pieces onto Strands/MCP constructs; added semantic code navigation, a preflight
self-correction loop in front of the approval gate, environment awareness (MCP discovery and resources, a profiler,
adaptive reasoning effort) and rejection learning. Everything below names the code, the test that holds it, and — for
the verification gate — what a real model did.

## 1. Security patches

| # | Finding | Severity | Fix | Held by |
|---|---|---|---|---|
| S1 | An untrusted repository's `.sierralab/settings.json` could set `laya.callGuard: off`, raise `laya.riskDestroy` past every score, or set `laya.mode: off` — removing the per-call asks that bound `auto` mode. A prompt-injected agent could write that file for the next session (see S2). | High | `laya.mode`, `laya.callGuard`, `laya.riskSafe`, `laya.riskDestroy` (and the new `preflight.*`, `learning.rejections`, `context.outlineLines`) added to `TRUST_GATED_KEYS`; mirrored in the TUI (`settings.rs`). The parity test now also compares the trust-gated sets. | `_phase11_hardening_smoke_test.py`, `_settings_schema_parity_smoke_test.py`, `_evals_smoke_test.py` (a repo setting `callGuard: off` no longer lets the injected-delete attack through), TUI `config_ui` test |
| S2 | A shell write to a policy file (`printf … > .sierralab/settings.json`, `tee`/`cp`/`mv`/`sed -i` into it, or into `.mcp.json`) scored 0.4 and ran in `auto`; `file_write` to the same path scored 0.9. | High | `laya_guard._write_destinations`: redirect targets, `tee`/`touch`/`truncate` args, `cp`/`mv`/`ln`/`install`/`rsync` destinations, in-place `sed`/`perl` files — scored against `_POLICY_FILE` (0.9). `_POLICY_FILE` extended to every MCP declaration the plugin host reads (`mcp.json`, `mcp.local.json`, `.cursor/` and `.vscode/mcp.json`, `backend/mcp_servers.json`). | `_phase11_hardening_smoke_test.py` |
| S3 | Agent Vault fail-closed (`egressPolicy: deny`, vault down) covered a fixed name list only. Not covered: generated skills (run in-process, can open sockets), external MCP plugin tools, `a2a_client`, `skill_forge` (its researcher sub-agent holds `http_request`), and every sub-agent in a swarm/graph/team (no gate for the refusal to reach). | High | `_ApprovalHandler._can_egress` (generated skills, plugin tools, `mcp_resources`); `EGRESS_TOOLS` += `a2a_client`, `skill_forge`; `vault_proxy.egress_blocked()` + `sub_agent_common.resolve_tools` drops `http_request`/`web_fetch` from sub-agent kits while egress is blocked. | `_phase11_hardening_smoke_test.py`, `_vault_proxy_smoke_test.py` |
| S4 | The generated-skill AST gate banned only `subprocess`, `os.system`, `eval`, `exec`: `socket`, `ctypes`, `os.posix_spawn`, `os.exec*`, `asyncio.create_subprocess_*`, `from os import system`, `getattr(os, "popen")`, `__import__`, `importlib.import_module` all passed. | Medium | `skill_validation_hooks._BANNED_*` widened (modules, os spawns by name, `ImportFrom`, literal `getattr`). | `_phase11_hardening_smoke_test.py`, `_proposer_guardrail_smoke_test.py` |
| S5 | rust-analyzer started for every opened project — including an untrusted clone — runs its `build.rs` files and proc-macros on load and `cargo check` on save: code execution on session open, before anyone is asked. | High | In an untrusted folder `LspClient` sends `initializationOptions` (and answers `workspace/configuration`) with build scripts, proc-macros and check-on-save off, experimental native diagnostics on. `/trust` restores the full set. Verified live: syntax errors still reported (1.8 s); type errors need `cargo check`, so they appear only in a trusted folder. | `_plugins_lsp_smoke_test.py`, `_preflight_smoke_test.py` |
| S6 | Discord `ALLOWED_USER_ID` lock and prompt injection. | — | **Verified, no change needed**: the id is read from the environment/Keychain at start, compared as an integer *before* any state change on every click and message, and unauthorized users get no reply. The injection path to it — an agent editing `backend/.env` — scores 0.85–0.95 by Laya Guard (shell and `file_write`) and asks in every mode. Laya thresholds: see S1. | `_discord_remote_smoke_test.py`, guard probes in this audit |

Also fixed while auditing (functional, found by the new tests): the built-in plugins (`lsp_*` tools, the memory store)
never autostarted in an untrusted folder, so the LSP tools silently disappeared there while the servers ran
(`plugin_host.start_autostart`).

## 2. Unified-memory leaks (oMLX / Prism)

The 27B Prism weights live in the oMLX server process; the harness's job is to make sure every model it loads is
unloaded and that "unloaded" is true. Four leak paths were found:

| Leak | Fix |
|---|---|
| An unload that failed (timeout, server error) cleared `loaded_by_us` and was never retried: ~8 GB resident until someone noticed. | `LeaseTable` keeps the model **stranded** and retries on the next acquire/release (never straight back at the server that just failed); `lease_unload_failed` on the memory channel. |
| The SDK skips `AfterInvocationEvent` when a *later* `BeforeInvocation` hook raises (`strands/agent/agent.py`), so `PrismLeaseHooks` never released that invocation's lease. | `PrismLeaseHooks._before` releases any lease still on its stack (an agent runs one invocation at a time, so a leftover is an orphan). |
| No release at process exit: a crashed or stopped harness left its models loaded. | `LeaseTable.release_all`, registered with `atexit` for the default table. |
| `unload` trusted the `loaded` flag; a server that flips the flag but keeps its Metal buffers (an uncleared MLX cache) looked clean. | `OmlxManager.unload` also waits up to 3 s for the pool (`current_model_memory`) to fall by half the model's size; otherwise records `unreclaimed`, logs it, emits `lease_unreclaimed`, and `omlx_resource_manager` tells the person to restart oMLX. |

Limit: the check reads oMLX's own accounting; the harness cannot see Metal allocations inside another process. Held by
`_phase11_hardening_smoke_test.py` (a fake oMLX that leaks on purpose), `_omlx_prism_smoke_test.py`,
`_memory_budget_smoke_test.py`.

## 3. Backpressure (gRPC, GPUI)

The Lab's design already isolates the UI thread: telemetry is conflated per session (latest wins), discrete events are a
bounded queue that never drops an approval, commands use `try_send`, and nothing on the network side awaits the UI.
Two gaps:

* **Rust `Inbox`** — a telemetry merge that "spills" a second one-off memory notice pushed it into the event queue
  *without* the 1,024 bound; a stalled UI under a lease storm grew the queue without limit. `Inbox::trim` now applies
  the bound on both paths. Test: `inbox::tests::spilled_memory_notices_obey_the_bound` (3,072 frames → ≤ 1,025 entries).
* **Daemon** — `InterventionStream` replayed pending approvals and answered stale decisions with bare
  `queue.put_nowait`, which raises `QueueFull` into the stream handler and drops the GUI's connection. All puts go
  through `grpc_bridge.offer` (drop-oldest, never raises). Test: `_phase11_hardening_smoke_test.py`.

Not done: no live GPUI freeze reproduction was attempted (the native Lab needs the screen; see the
`gpui_verification_without_screenshots` note).

## 4. Strands SDK & MCP conformance

Re-read every custom construct against the SDK (`docs/strands-sdk-audit.md`, "Phase 11"):

* Already native, unchanged: `claude_code_engineer` (`@tool(context=True)`, cooperative cancel), the resource managers
  (`ToolResult` dicts), specialists (`as_tool()`), team pipeline (`GraphBuilder`), swarm, the approval gate
  (`InterventionHandler`), MCP loading (`MCPClient.load_servers`).
* Replaced: `skill_forge`'s deprecated `strands_tools.http_request` → the SDK's vended `http_request`.
* Wired: `ollama_cloud.ReasoningThrottle`'s purpose (a `BeforeModelCallEvent` hook on the model's `ReasoningControl`),
  which existed and was never used — now `adaptive_effort.AdaptiveEffort`.
* MCP: resources and RFC 6570 resource templates were never exposed (tools only). `mcp_resources` reads them through the
  SDK's `list_resources_sync` / `list_resource_templates_sync` / `read_resource_sync`; listing never asks, a read asks as
  the server's tools do, is fenced untrusted, and is refused while the vault blocks egress. MCP *prompts* stay unexposed
  on purpose (server-authored instructions).
* New pieces use documented surfaces only: `ContextInjector` (per-message MCP capabilities, rejection constraints),
  `HookOrder` (large-file outline ahead of the offloader), async `InterventionHandler.before_tool_call` (preflight off the
  event loop).

## 5. What was built (Parts 2–5)

| Directive | Implementation | Docs / tests |
|---|---|---|
| Ripgrep tool | `code_search` (`code_search.py`): ripgrep when on `PATH`, identical Python engine otherwise; capped, sorted `path:line: text`; never outside the project or into secrets files. | `docs/agentic-harness.md` "Finding code"; `_code_nav_smoke_test.py` |
| LSP: definition / references / types | Already present (`lsp_definition`, `lsp_references`, `lsp_hover`, `lsp_symbols`); now reachable in untrusted folders (fix above), steered by the prompt, and given a multi-file **unsaved-text check** (`LspClient.check_texts`). | same |
| Smart windowing (> 500 lines) | `code_outline` (Python `ast`; structural scan for Rust/Go/TS/Java/C/Ruby/shell/Markdown; tree-sitter if installed) and `LargeFileHooks` (whole-file read → outline, `context.outlineLines`). | same |
| Pre-flight testing + auto-correction | `preflight.py`, in front of `_ApprovalHandler` for every agent's edits: syntax, ruff, LSP on unsaved text, optional tests in a copy; only *new* problems count; cross-file import errors are notes; refusals go back to the agent (`preflight.maxRetries`); the self-heal Debugger's patches get the same static checks. | "Preflight"; `_preflight_smoke_test.py`, `_self_heal_smoke_test.py`, `_evals_smoke_test.py` |
| Atomic checkpoints + rollback | On give-up, the turn is rolled back from its first **shadow** checkpoint (`checkpoints.undo_turn`), present checkpointed first so it can be undone. See §6 for why not `git commit`. | `_preflight_smoke_test.py` asserts the person's own `.git` is untouched |
| MCP registry scanner + dynamic prompt | `.cursor/mcp.json`, `.vscode/mcp.json` (VS Code shape), `mcp.json` discovered as project servers; running/blocked servers injected per message; `mcp_resources`. | "Plugins"; `_environment_awareness_smoke_test.py` |
| Environment profiler | `env_profile.py`: size class, languages, manifests (dependencies counted, nothing executed), unified memory; one prompt line; `environment_profile` tool adds toolchain versions. | same |
| Adaptive reasoning budget | `adaptive_effort.py`: per model call from size class × Laya complexity; never below the role's level (Lab dial / env override), never `max` unless chosen; `reasoning.adaptive`. | same |
| Rejection analysis → KG → negative constraints | `rejection_memory.py`: capture on a person's rejection (GUI note relayed through `_meta`), tool-less ReflectionAgent → one prohibition, `textfilter`, store + `USER_PREFERENCE` KG node; injected as hard constraints on every model call. | "Rejection memory"; `_rejection_memory_smoke_test.py` |

## 6. Deviations from the directive, and why

* **No `git add . && git commit -m "strands_checkpoint"`.** This working tree already held about 60 uncommitted paths of in-progress work; committing
  the person's repository on every task would mix agent snapshots into their history, run their hooks, and fail in a
  folder that is not a repository. The harness already had a shadow-git checkpoint store (`~/.sierralab/checkpoints/`)
  that never touches the person's `.git`; rollback uses it.
* **Discovered MCP servers connect only in a trusted folder.** A server is a command a cloned repository names; starting
  it unasked is code execution. Untrusted folders get the servers *announced* ("waiting for /trust"), not started.
* **No `pip freeze`.** Running a repository's own interpreter runs its code; dependencies are counted from manifests.
* **Tests in preflight are opt-in (`preflight.tests`, trust-gated).** Running a suite on every edit executes the project's
  code; static checks (syntax, ruff, LSP) run on every edit and execute nothing.
* **Cognee.** Rules are mirrored into the configured KG sink (LanceDB by default, Cognee when `kg.sink: "cognee"`), but
  retrieval reads the preference store directly — Cognee is not installed here and its sink is documented as unverified.
* **tree-sitter, ripgrep, ruff** are not installed on this machine and were not installed (downloads need your OK). Each
  path has a fallback and says so; `brew install ripgrep ruff` enables the fast/lint paths.

## 7. Cleanup

Scanned for stale code: no tracked build artefacts, caches or logs; no orphan modules (every backend module is imported
or is an entry point); every pinned dependency is imported. Removed/replaced: the deprecated `strands_tools.http_request`
in `skill_forge_tool.py`; dead wiring brought to use (`ReasoningThrottle`'s role). Left in place deliberately:
`backend/evals/spikes/` (an untracked, documented spike — deleting untracked work is not reversible), the
`laya.riskAutoApprove` key (reserved, documented as inert, kept so existing files are not rejected), and
`_self_heal_smoke_test.py`'s use of the legacy tools (it tests the loop, not the tools). `ARCHITECTURE.md` added;
`docs/agentic-harness.md`, `docs/settings.md`, `docs/strands-sdk-audit.md`, `THREAT_MODEL.md`, `README.md` updated.

## 8. Verification gate

### Hermetic (scripted model, the real ACP server object)

1. **LSP & ripgrep** — "find every reference to build_widget": `code_search` → `code_outline` → `lsp_references`, the
   search answer is the lines, no whole-file read (`_code_nav_smoke_test.py`).
2. **Self-correction** — a patch with a syntax error is refused before anyone is asked; the agent gets
   `calc.py:2: [syntax]`; the fixed patch is the *one* approval the person sees; then the give-up path rolls the turn's
   earlier edit back (`_preflight_smoke_test.py`).
3. **Rejection learning** — reject a `print()` patch with a note; in a new session, after a read, the call that writes the
   patch still carries the rule and proposes logging (`_rejection_memory_smoke_test.py`). This last check was added
   because of what the live run found (below), and was shown to fail on the earlier code.

### Live (`_phase12_gate_live_test.py`, `gpt-oss:20b` via Ollama Cloud, in-process, Discord off)

| Gate | Result |
|---|---|
| 1 | The model searched first (`code_search`) and read two 6-line ranges — all 4 references, no whole-file read, 24 s. It did not choose an LSP tool. **The first live run found a bug**: the model wrote `glob: "**/*.py"` and the Python engine skipped root-level files (`app.py`), unlike ripgrep. Fixed (`code_search._glob_filter`) and covered. |
| 2 | Told to write `def mul(a, b) return a * b` through `apply_patch`: preflight refused it twice (`expected ':'`), the model fixed it, and both approvals the person saw were for patches that parse; `calc.py` ends correct. 24 s. |
| 3 | **Failed twice, then passed.** Run 1: the rule was learned from the note but scoped to "app.py's handle()", so the model used `print()` in `report.py` → ReflectionAgent told to follow the note's scope (project-wide unless limited). Run 2: rule "Prefer logging…" and the model still printed → root cause: the constraint was folded only into the turn's *first* model call (`trigger="userTurn"`), and the model wrote the patch after a read → now `everyTurn`, and rules must be prohibitions. Run 3: rule "Do not use print() for debugging; instead use logger.debug()…"; in a new session the proposal (and the file) used `logging`, no `print`. 63 s for both turns. |

### Test results

New suites: `_phase11_hardening_smoke_test.py`, `_code_nav_smoke_test.py`, `_preflight_smoke_test.py`,
`_rejection_memory_smoke_test.py`, `_environment_awareness_smoke_test.py` (all in CI). Updated:
`_evals_smoke_test.py`, `_laya_call_guard_smoke_test.py`, `_plugins_lsp_smoke_test.py`,
`_settings_schema_parity_smoke_test.py`, TUI `config_ui` / `ui` tests, Lab `inbox` test.

Final run, after the last change (2026-10-05):

* **Python: 61 of 61 hermetic suites pass** (every `backend/_*_smoke_test.py` except the live ones: `_acp_smoke_test.py`,
  `_acp_stdout_smoke_test.py`, `_e2e_lab_live_test.py`, `_lab_gui_live_smoke_test.py`), 981 s in all — including the eval
  suite's 42 oracle tasks through the real ACP server with preflight on.
* **Rust, `tui/`: 214 tests pass** (acp-tui 165, acp-core and the rest 49). `cargo clippy --all-targets`: clean.
* **Rust, `lab/`: 116 pass, 1 ignored** (unchanged), including the new inbox bound test. `clippy -p harness-client`: clean.
* `cargo fmt --check` still reports diffs in files this pass did not touch (`acp-tui/src/app.rs`, `commands.rs`; the Lab
  crates' long-line style) — left as they were. The edited Rust is formatted like its surroundings.

## 9. Residual risks and follow-ups

* tsserver can load a plugin a repository's `tsconfig.json` names from its `node_modules` (not restricted here).
* `preflight.tests` runs in a copy with the session user's privileges (THREAT_MODEL gap 6): it contains mistakes, not malice.
* The oMLX "unreclaimed" check trusts the server's accounting.
* Rejection rules are only as good as the reflecting model; a person can list and remove them
  (`python backend/rejection_memory.py list | forget <id>`). The constraint block costs its length on every model call
  while rules exist (≈ 60–120 tokens per rule, at most 5 shown).
* Token cost of the new tools: `code_search` + `code_outline` (2.3 KB of schema) are cold — loaded on code-ish messages,
  specialist routes, or after an outlined read — so a plain turn carries none of it; `environment_profile` and
  `mcp_resources` are cold too. The MCP capabilities line appears only when the project has MCP servers.
* Not attempted: a live GPUI freeze reproduction; a live Cognee sink.

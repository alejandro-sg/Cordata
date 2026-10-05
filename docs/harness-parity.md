# Harness parity: acp-tui + backend vs the tools it is modelled on

What a coding-agent harness is expected to do, who does it, and where this repo stands — with
the gaps left in. Reference points: **Claude Code** (settings, permissions, `CLAUDE.md`, native
scrollback), **Kilo CLI** (`kilo.jsonc`, permission wildcards, "always run", `run`/`--continue`) and
**DeepSeek-TUI** (Plan / Agent / YOLO modes, snapshots with `/undo`, sub-agents). Status:
✅ done · 🟡 partly · ⬜ not done.

| capability | reference | here | notes |
|---|---|---|---|
| Output in the terminal's own scrollback (wheel / trackpad / selection / copy work) | Claude Code | ✅ | inline renderer (`term.rs`): finished messages are printed once; only the prompt box and the unfinished tail are redrawn. A long reply streams into scrollback as it arrives. No alternate screen, no mouse capture |
| Bracketed paste (multi-line paste doesn't submit) | Claude Code | ✅ | |
| Layered settings: user / project / local | Claude Code, Kilo | ✅ | `docs/settings.md`; JSON only, no comments (Kilo's `.jsonc` not supported) |
| Interactive settings panel | Claude Code `/config` | 🟡 | 11 settings. No theme, keybindings, status line, env, hooks |
| Permission rules `allow` / `ask` / `deny` with patterns | Claude Code, Kilo | ✅ | `deny` holds in every mode; shell commands are split before matching. Not a sandbox |
| "Always allow" from the approval prompt, saved as a rule | Claude Code, Kilo | ✅ | exact-command rules only; no prefix/wildcard suggestions |
| Project trust before a repo's settings can loosen permissions | Claude Code (trust dialog) | 🟡 | `/trust` and "Always allow"; no first-run dialog — the banner and `/permissions` show what's ignored |
| Instruction files (`CLAUDE.md` / `AGENTS.md`) | Claude Code, Kilo | ✅ | `SIERRALAB.md`, falling back to `AGENTS.md` / `CLAUDE.md`; user + project + local. No `@import`, no `#` quick-add |
| `/init` drafts the instruction file | Claude Code | ✅ | asks the agent; it writes through the normal approval gate |
| Plan / build / auto modes, cycled with Tab | Claude Code, DeepSeek-TUI | ✅ | Tab / Shift+Tab, `/mode`, `permissions.defaultMode` |
| Model + temperature switching | Claude Code `/model`, Kilo | ✅ | picker (Ctrl+O / Ctrl+T), `/model`, `/temp`, settings |
| Provider switching | Claude Code, Kilo | ⬜ | provider/URL/credentials are environment (`backend/.env`, Keychain) on purpose — see `docs/settings.md` |
| Reasoning and sub-agent activity visible | DeepSeek-TUI | ✅ | live tail, per-agent rows, hand-offs |
| Fast triage in front of the LLM (Laya) | — | ✅ | `docs/laya-triage.md`. Ours alone; near chance before fine-tuning |
| Secrets kept from the agent | — | ✅ | `THREAT_MODEL.md`; env/Keychain only, masked, built-in Keychain deny rule |
| Resume a session by id | Claude Code, Kilo `--continue` | 🟡 | `--resume <id>`; text history only, comes back in the default mode. No picker (ACP `list_sessions` isn't implemented in the backend), no `--continue` |
| `/status` | Claude Code | 🟡 | session, mode, model, Laya, rule counts. No usage/cost — ACP has no usage push |
| `/clear` | Claude Code | 🟡 | clears the screen; the *conversation* continues (a new conversation needs a new session) |
| New conversation without restarting (`/new`) | Claude Code | ⬜ | needs a second `new_session` on the live connection |
| `/compact` (summarise to free context) | Claude Code | ⬜ | Strands has a conversation manager; no ACP surface for it yet |
| Snapshots and `/undo` / `/restore` of file changes | DeepSeek-TUI | ✅ | `checkpoints.py` snapshots into a shadow git repo before any call that may write; `/undo`, `/checkpoints`; see `docs/agentic-harness.md` |
| `/export` / `/copy` | Claude Code | ⬜ | selecting text in the terminal works; no command |
| Non-interactive single prompt (`-p`, `kilo run`) | Claude Code, Kilo | 🟡 | `acp-stress` runs scripted prompts and prints JSONL; there is no `acp-tui -p "…"` |
| `@file` mentions | Claude Code, Kilo | ⬜ | the backend reads ACP resource blocks; the TUI doesn't send them |
| User-defined hooks | Claude Code | ⬜ | the backend uses Strands hooks internally (secret masking, untrusted-output guard) but exposes none |
| MCP server management in the terminal | Claude Code `/mcp`, Kilo | ⬜ | the web UI has it (`provider_settings`, `mcp_manager`); the TUI doesn't |
| Custom slash commands / skills from files | Claude Code, Kilo | ⬜ | |
| Themes, vim mode, status-line command | Claude Code | ⬜ | not planned |

## What this harness does that the references do not

Rows above are parity; these are ours alone — the agentic harness layer (see
[`docs/agentic-harness.md`](agentic-harness.md)):

| capability | here | notes |
|---|---|---|
| Fast typed routing in front of the LLM (Laya) | ✅ | `docs/laya-triage.md`: route / risk / complexity in one ~18 ms pass |
| Direct specialist dispatch | ✅ | confident `architect` / `debugger` routes skip the orchestrator's decide-to-delegate model call; `laya.dispatch` |
| Autonomous self-healing loop | ✅ | failing shell command → debugger diagnosis → sandbox-verified patch → re-run; `docs/self-healing-architecture.md`, `selfHeal.*` |
| Team pipeline (decompose-to-graph) | ✅ | confident `team` route: planner call + Strands GraphBuilder pipeline, same node denylist as `local_graph`; `laya.teamDispatch` |
| Remote agents over A2A | ✅ | `a2a_client` orchestrator tool wraps Strands' `A2AAgent`; orchestrator-only (egress needs a gate), no credential injection |
| Language-server diagnostics | ✅ | `lsp_bridge` starts the project's language servers (gated by `lsp.enabled`), feeds symbols/diagnostics to specialists instead of grep-only |
| Plugins with per-turn tool scoping | ✅ | `plugin_host`: confident plugin route scopes the turn's toolset to that plugin |
| Harness event bus | ✅ | `harness_events`: background-task progress, plugin/LSP status and heal phases as ACP extension notifications, rendered as progress bars in acp-tui |
| Unified-diff patch tool | ✅ | `apply_patch` applies unified diffs through the same approval gate as any edit |

## Known limits of the inline renderer

- Resizing the terminal re-wraps what is already on screen; the live prompt box is redrawn, but a
  frame that was already printed may leave a duplicated top border in scrollback (terminals can't
  be told to forget it). Same trade-off as other inline TUIs.
- Reasoning / tool-output expansion and `/laya off` change what is printed *from now on*; earlier
  output stays as it was printed.
- Terminals without synchronized-update support (`CSI ? 2026`) may flicker slightly while streaming.

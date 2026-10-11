# Ported from Claude Code

These 43 skills are the user's personal Claude Code skill library
(`~/.claude/skills/`), ported here so the same methodology is available to
this Strands agent on both front doors (AG-UI web and ACP/Zed) — `AgentSkills`
loads every `SKILL.md` under `backend/skills/` for both, so no separate ACP
wiring was needed beyond copying the files.

**Deliberately excluded** (referenced tools/mechanisms this agent doesn't
have, would just be broken if copied as-is):
- `claude-handoff` — spawns `claude --bg`, a Claude Code CLI binary.
- `git-guardrails-claude-code` — configures `.claude/settings.json` hooks;
  this repo already has the equivalent native Strands mechanism
  (`skill_validation_hooks.py`, `interrupt_on_call` tool gating).
- `obsidian-vault` — hardcoded to a Windows/WSL path (`/mnt/d/...`) from a
  different machine.

Also excluded (per the user's own scoping decision, not a compatibility
judgment call): ~35 skills tied to ecosystems this agent doesn't have
installed — `aws-*`, `cloudflare-*`, `google-agents-cli-*` and `ripwire-*`
(symlinks to external tools), `amazon-bedrock`, `sandbox-sdk`, `wrangler`,
`durable-objects`, `workers-best-practices`, `turnstile-spin`, `web-perf`,
`launch-with-aws`, `signing-in-to-aws`, `agents-sdk`.

## Tool-name translation

Every skill below was written for Claude Code's tool names. This agent's
tools are named differently — same idea, different registry:

| Claude Code | This agent | Notes |
|---|---|---|
| `Read` | `file_read` | |
| `Write` / `Edit` | `file_write` | one tool covers both create and modify |
| `Bash` | `shell` | a real host shell either way — `gh`, `git`, etc. all still work |
| `Grep` / `Glob` | `code_search` / `code_outline` | fast ripgrep-backed search; fallback to `shell` (`grep`/`find`) |
| `Task` (parallel sub-agents) | `orchestrate` | hands a multi-step task to the orchestrator (plan → execute → audit); worker pools across Claude Code, Antigravity, Ollama |
| `WebSearch` / `WebFetch` | `web_fetch` / `http_request` | `web_fetch` parses HTML to markdown; `http_request` handles raw HTTP calls |
| `AskUserQuestion` | — | ask in plain chat text or via interactive approval prompts |
| `TaskCreate` / `TaskUpdate` (todo tracking) | — | track progress in chat, checkpoints, or orchestrator plans |
| `Artifact` | `plan_artifact` | orchestrator auto-generates `plan.md` and `walkthrough.md` in `~/.cordata/orchestrator/<run_id>/` (or `~/.sierralab/`, Antigravity artifact model) |

A skill that assumes one of the unavailable tools still describes a valid
methodology — the missing step just becomes "ask the user directly" or
"skip it" rather than a distinct tool call.

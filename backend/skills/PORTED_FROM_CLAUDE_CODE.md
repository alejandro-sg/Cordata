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
| `Grep` / `Glob` | `shell` | no dedicated search tool here; use `grep`/`find` via `shell` |
| `Task` (parallel sub-agents) | `local_swarm` / `local_graph` | see the `sub-agent-team` skill for the schema — different from Claude Code's Task tool syntax |
| `WebSearch` / `WebFetch` | `http_request` | raw HTTP only — no search index, no HTML→markdown conversion |
| `AskUserQuestion` | — | no equivalent; ask in plain chat text instead |
| `TaskCreate` / `TaskUpdate` (todo tracking) | — | no equivalent; track progress in chat or a scratch file instead |
| `Artifact` | — | no equivalent; write files to `backend/workspace/` instead |

A skill that assumes one of the unavailable tools still describes a valid
methodology — the missing step just becomes "ask the user directly" or
"skip it" rather than a distinct tool call.

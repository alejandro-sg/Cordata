/**
 * Claude Code-style input triggers for the web chat, applied once at submit
 * time in useQueueingAgentInput.ts — before text ever reaches the agent.
 *
 * `/skillname rest` — deterministically activates a backend skill
 * (backend/skills/*, loaded via Strands' AgentSkills plugin) instead of
 * hoping the model decides to call its `skills` tool on its own. Confirmed
 * against the installed strands.vended_plugins.skills source: activation is
 * exactly one tool call, `skills(skill_name)` — so the rewritten text is a
 * blunt, imperative instruction to call it, the same idiom
 * SettingsModal.tsx's runInstruction already relies on for this local
 * model's documented tool-calling reliability (a longer or softer prompt
 * measurably hurts it — see agent_common.SYSTEM_PROMPT's own comment).
 *
 * `$ command` — shell passthrough. Deliberately NOT a dedicated backend
 * route: an earlier draft was exactly that (POST /shell/run calling
 * strands_tools.shell directly, gated only by a client-side approval card),
 * and Claude Code's own auto-mode classifier refused to create that file
 * ("Create RCE Surface") — correctly: a plain REST endpoint that executes
 * shell commands with no server-side gate is a real unauthenticated
 * remote-code-execution surface no matter what the frontend promises to do
 * first. Rewriting into a normal chat message instead routes it through the
 * *real* agent turn and the existing, server-side-enforced
 * ToolBehavior(interrupt_on_call=True) gate (main.py) plus the existing
 * useInterrupt "shell" ApprovalCard (AgentTools.tsx) — the same path every
 * other shell call already goes through, not a parallel weaker one.
 *
 * `@path` mentions — inlines a real workspace file's content, matching
 * acp_server.py's own `_extract_prompt_text` handling of embedded resources
 * for the Zed/ACP front door, so both front doors treat a file mention the
 * same way. Deliberately not approval-gated the way readWorkspaceFile is
 * for generateSandboxedUi: this is the signed-in human directly asking, in
 * their own typed message, to read one file — gating every @mention behind
 * a click would be pure friction for a read-only action with no plausible
 * confused-deputy angle (the caller and the person approving would be the
 * same click). A `@token` that doesn't resolve to a real file is left
 * exactly as typed, same as a chat client that doesn't recognize a mention.
 */

const SKILL_TRIGGER = /^\/(\S+)(?:\s+([\s\S]*))?$/;
const SHELL_TRIGGER = /^\$\s+([\s\S]+)$/;
const MENTION_TOKEN = /@(\S+)/g;

export async function rewriteInputTrigger(text: string, knownSkillNames: ReadonlySet<string>): Promise<string> {
  const slashMatch = text.match(SKILL_TRIGGER);
  if (slashMatch) {
    const [, name, rest] = slashMatch;
    if (knownSkillNames.has(name)) {
      return rest
        ? `Call the skills tool with skill_name="${name}". Then: ${rest}`
        : `Call the skills tool with skill_name="${name}".`;
    }
  }

  const dollarMatch = text.match(SHELL_TRIGGER);
  if (dollarMatch) {
    return `Call the shell tool with command=${JSON.stringify(dollarMatch[1])}.`;
  }

  return inlineWorkspaceMentions(text);
}

async function inlineWorkspaceMentions(text: string): Promise<string> {
  const tokens = [...new Set([...text.matchAll(MENTION_TOKEN)].map((match) => match[1]))];
  if (tokens.length === 0) return text;

  let result = text;
  for (const path of tokens) {
    try {
      const res = await fetch(`/api/sandbox/workspace-file?path=${encodeURIComponent(path)}`);
      if (!res.ok) continue;
      const body = (await res.json()) as { content?: string };
      if (typeof body.content !== "string") continue;
      result = result.replaceAll(`@${path}`, `@${path}\n[Attached: ${path}]\n${body.content}`);
    } catch {
      // Backend unreachable or path genuinely doesn't resolve — leave the
      // literal @token in place rather than failing the whole send.
    }
  }
  return result;
}

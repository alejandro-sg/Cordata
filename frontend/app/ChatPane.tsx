"use client";

import { useEffect, useMemo, useState } from "react";
import { CopilotChatView } from "@copilotkit/react-core/v2";
import MentionAutocomplete from "./MentionAutocomplete";
import { useQueueingAgentInput } from "./useQueueingAgentInput";

type SkillInfo = { name: string; description: string };

/**
 * Renders CopilotChatView directly instead of the higher-level <CopilotChat>
 * — required for the queueing input (see useQueueingAgentInput):
 * <CopilotChat> always overrides inputValue/onInputChange/onSubmitMessage/
 * onStop with its own internal handlers (confirmed by reading the bundled
 * source — its mergedProps spreads whatever's passed in *before*
 * unconditionally setting these four itself), so passing custom versions to
 * it is silently a no-op. <CopilotChatView> is the fully-controlled
 * lower-level component the docs describe for exactly this "own your input
 * handling" case, and it still renders the same message list, tool-call
 * cards, and reasoning display internally — only the input wiring is now
 * ours instead of auto-derived.
 */
export default function ChatPane({ threadId, hasExplicitThreadId }: { threadId: string; hasExplicitThreadId: boolean }) {
  // Fetched once, here, rather than inside useQueueingAgentInput: this
  // component needs the same list twice (the /slash toolsMenu below, and
  // the known-name set the hook needs to tell a real "/build" apart from a
  // message that merely starts with a slash) — one fetch, one source of
  // truth, instead of two independent ones drifting if skills change mid
  // session.
  const [skills, setSkills] = useState<SkillInfo[]>([]);
  useEffect(() => {
    fetch("/api/skills")
      .then((res) => res.json())
      .then((data: unknown) => setSkills(Array.isArray(data) ? (data as SkillInfo[]) : []))
      .catch(() => setSkills([]));
  }, []);
  const skillNames = useMemo(() => new Set(skills.map((skill) => skill.name)), [skills]);

  const { messages, isRunning, inputValue, onInputChange, onSubmitMessage, onStop, queuedMessage, clearQueuedMessage } =
    useQueueingAgentInput("sierralab_agent", threadId, hasExplicitThreadId, skillNames);

  // ToolsMenuItem's `action` takes no arguments and returns void: it just
  // prefills the box (like Claude Code inserting a picked slash command for
  // you to finish typing), it doesn't send anything itself. onInputChange
  // is stable (useState's setter), so this array only needs to change when
  // the skills list itself does.
  const toolsMenu = useMemo(
    () => skills.map((skill) => ({ label: `/${skill.name}`, action: () => onInputChange(`/${skill.name} `) })),
    [skills, onInputChange],
  );

  return (
    <div className="chat-pane-inner">
      <div className="chat-pane-chat">
        <CopilotChatView
          messages={messages}
          isRunning={isRunning}
          inputValue={inputValue}
          onInputChange={onInputChange}
          onSubmitMessage={onSubmitMessage}
          onStop={onStop}
          hasExplicitThreadId={hasExplicitThreadId}
          input={{ toolsMenu }}
        />
        <MentionAutocomplete inputValue={inputValue} onInputChange={onInputChange} />
      </div>
      {queuedMessage && (
        <div className="queued-message">
          <span className="queued-message-label">Queued — sends when the current reply finishes</span>
          <span className="queued-message-text">{queuedMessage}</span>
          <button type="button" className="queued-message-cancel" onClick={clearQueuedMessage} aria-label="Cancel queued message">
            ×
          </button>
        </div>
      )}
    </div>
  );
}

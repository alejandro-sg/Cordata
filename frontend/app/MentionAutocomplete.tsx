"use client";

import { useEffect, useState } from "react";

const TRAILING_MENTION = /(?:^|\s)@(\S*)$/;

/**
 * `@` mention suggestions for the chat input, resolving against
 * agent_common.WORKSPACE_DIR (backend/main.py's /workspace-files).
 *
 * Scope limitation, disclosed rather than silently shipped: this only
 * matches a trailing, still-being-typed `@partial` at the *end* of the
 * current input value, not one under the cursor mid-string. ChatPane.tsx
 * renders CopilotChatView with inputValue/onInputChange as plain controlled
 * props (see useQueueingAgentInput.ts's own module docstring for why —
 * CopilotChatInput's send() owns the real <textarea>), so this component
 * never gets the textarea's DOM node or selection range, only the string
 * value. A cursor-following dropdown would need owning that element
 * directly, which useQueueingAgentInput's docstring already explains this
 * app deliberately avoided doing for unrelated reasons (the queued-input
 * UX). Typing an @mention at the end of a message — the common case — works
 * fully; editing one back into the middle of already-typed text does not
 * get suggestions (the mention itself, once sent, still resolves either way
 * — see chatInputTriggers.ts's inlineWorkspaceMentions, which scans the
 * whole string, not just the trailing token).
 */
export default function MentionAutocomplete({
  inputValue,
  onInputChange,
}: {
  inputValue: string;
  onInputChange: (value: string) => void;
}) {
  const [files, setFiles] = useState<string[]>([]);

  useEffect(() => {
    fetch("/api/workspace-files")
      .then((res) => res.json())
      .then((paths: unknown) => setFiles(Array.isArray(paths) ? paths.filter((p): p is string => typeof p === "string") : []))
      .catch(() => setFiles([]));
  }, []);

  const match = inputValue.match(TRAILING_MENTION);
  if (!match) return null;

  const partial = match[1];
  const matches = files.filter((path) => path.toLowerCase().includes(partial.toLowerCase())).slice(0, 8);
  if (matches.length === 0) return null;

  const matchIndex = match.index ?? inputValue.length;
  const select = (path: string) => {
    const before = inputValue.slice(0, matchIndex);
    const leadingSpace = match[0].startsWith(" ") ? " " : "";
    onInputChange(`${before}${leadingSpace}@${path} `);
  };

  return (
    <div className="mention-autocomplete" role="listbox" aria-label="Workspace files">
      {matches.map((path) => (
        <button key={path} type="button" className="mention-autocomplete-item" onClick={() => select(path)}>
          {path}
        </button>
      ))}
    </div>
  );
}

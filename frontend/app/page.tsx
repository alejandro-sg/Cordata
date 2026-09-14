"use client";

import { CopilotChatConfigurationProvider, CopilotKitProvider } from "@copilotkit/react-core/v2";
import "@copilotkit/react-core/v2/styles.css";
import { useState } from "react";
import AgentTools from "./AgentTools";
import ChatPane from "./ChatPane";
import SandboxedUiFullscreen from "./SandboxedUiFullscreen";
import SwarmProgress from "./SwarmProgress";
import ThreadSidebar from "./ThreadSidebar";
import { useLocalStorageToggle } from "./useLocalStorageToggle";
import { useSandboxFunctionBridge } from "./useSandboxFunctionBridge";

export default function Page() {
  const [threadId, setThreadId] = useState(() => crypto.randomUUID());
  // true = "resume this specific existing thread" (sidebar selection) vs
  // false = "brand new, no history to fetch" (New chat). Real fix for New
  // chat not clearing lived in useQueueingAgentInput, not this flag — see
  // its own comment for why: switching threads needs an explicit
  // setMessages([])/connectAgent() call this app has to make itself
  // (<CopilotChat> does it internally; <CopilotChatView>, which this app
  // uses instead for the queueing input, is presentational only).
  const [isExplicit, setIsExplicit] = useState(false);
  const [dark, toggleDark] = useLocalStorageToggle("sierralab-agents:dark", false);
  const [enableInspector, toggleInspector] = useLocalStorageToggle("sierralab-agents:inspector", true);
  const [subAgentsOn, toggleSubAgents] = useLocalStorageToggle("sierralab-agents:subagents", true);
  const { sandboxFunctions, approvalUi } = useSandboxFunctionBridge();

  return (
    <CopilotKitProvider
      runtimeUrl="/api/copilotkit"
      agentId="sierralab_agent"
      enableInspector={enableInspector}
      openGenerativeUI={{ sandboxFunctions }}
    >
      {approvalUi}
      <CopilotChatConfigurationProvider
        threadId={threadId}
        hasExplicitThreadId={isExplicit}
        labels={{
          // `title`/`initial` (this app's original keys) were never valid —
          // CopilotChatLabels has no such fields, so this custom message
          // never actually applied; every screenshot taken this session
          // showed the library's generic "How can I help you today?"
          // default instead. welcomeMessageText is the real field for it.
          welcomeMessageText:
            "Local Strands agent — shell, files, code execution, long-term memory, and sub-agent teams, all running on your machine. Ask me anything.",
        }}
      >
        <AgentTools onToggleDark={toggleDark} />
        <SandboxedUiFullscreen />
        <div className={"app-layout" + (dark ? " dark" : "")}>
          <ThreadSidebar
            activeThreadId={threadId}
            onSelect={(id) => {
              setThreadId(id);
              setIsExplicit(true);
            }}
            onNew={() => {
              setThreadId(crypto.randomUUID());
              setIsExplicit(false);
            }}
            dark={dark}
            onToggleDark={toggleDark}
            inspectorOn={enableInspector}
            onToggleInspector={toggleInspector}
            subAgentsOn={subAgentsOn}
            onToggleSubAgents={toggleSubAgents}
          />
          <div className="chat-pane">
            <ChatPane threadId={threadId} hasExplicitThreadId={isExplicit} />
          </div>
          {subAgentsOn && <SwarmProgress />}
        </div>
      </CopilotChatConfigurationProvider>
    </CopilotKitProvider>
  );
}

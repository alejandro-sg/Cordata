"use client";

import { CopilotChatConfigurationProvider, CopilotKitProvider } from "@copilotkit/react-core/v2";
import "@copilotkit/react-core/v2/styles.css";
import { useState } from "react";
import AgentTools from "./AgentTools";
import ChatPane from "./ChatPane";
import { randomId } from "./randomId";
import SandboxedUiFullscreen from "./SandboxedUiFullscreen";
import ThreadSidebar from "./ThreadSidebar";
import { useLocalStorageToggle } from "./useLocalStorageToggle";
import { useSandboxFunctionBridge } from "./useSandboxFunctionBridge";

export default function Page() {
  const [threadId, setThreadId] = useState(() => randomId());
  // true = "resume this specific existing thread" (sidebar selection) vs
  // false = "brand new, no history to fetch" (New chat). Real fix for New
  // chat not clearing lived in useQueueingAgentInput, not this flag — see
  // its own comment for why: switching threads needs an explicit
  // setMessages([])/connectAgent() call this app has to make itself
  // (<CopilotChat> does it internally; <CopilotChatView>, which this app
  // uses instead for the queueing input, is presentational only).
  const [isExplicit, setIsExplicit] = useState(false);
  const [dark, toggleDark] = useLocalStorageToggle("cordata-agents:dark", false);
  const [enableInspector, toggleInspector] = useLocalStorageToggle("cordata-agents:inspector", true);
  const { sandboxFunctions, approvalUi } = useSandboxFunctionBridge();

  return (
    <CopilotKitProvider
      runtimeUrl="/api/copilotkit"
      agentId="cordata_agent"
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
            "Local Strands agent — shell, files, code execution and long-term memory, all running on your machine. Ask me anything.",
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
              setThreadId(randomId());
              setIsExplicit(false);
            }}
            dark={dark}
            onToggleDark={toggleDark}
            inspectorOn={enableInspector}
            onToggleInspector={toggleInspector}
          />
          <div className="chat-pane">
            <ChatPane threadId={threadId} hasExplicitThreadId={isExplicit} />
          </div>
        </div>
      </CopilotChatConfigurationProvider>
    </CopilotKitProvider>
  );
}

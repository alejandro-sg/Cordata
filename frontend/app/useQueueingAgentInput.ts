"use client";

import { useEffect, useRef, useState } from "react";
import { useAgent, useCopilotKit } from "@copilotkit/react-core/v2";
import { rewriteInputTrigger } from "./chatInputTriggers";
import { randomId } from "./randomId";

/**
 * Claude Code-style chat input: keep typing enabled while the agent is
 * running, and auto-send a queued follow-up the instant the run finishes —
 * instead of the default "box locks until the reply is done" chat UX.
 *
 * Checked both layers before building this: CopilotKit has a real stop/
 * interrupt primitive (copilotkit.stopAgent / agent.abortRun — wired below
 * via onStop), but no built-in message queue. Strands' agent loop has none
 * either — it's request-to-completion per turn, no mid-run injection API.
 * So the queue itself is purely client-side.
 *
 * "Busy" is tracked with our own ref, set true the instant runAgent() is
 * called and false only once that promise actually resolves — not derived
 * from agent.isRunning. Confirmed empirically that agent.isRunning does not
 * reliably stay true for the *entire* duration of a run that includes a
 * tool call (e.g. local_swarm): a message queued during a live, visibly
 * still-running swarm was sent immediately instead of queued when gated on
 * agent.isRunning. Chaining directly off the runAgent() promise sidesteps
 * that entirely: whatever gets submitted while busyRef is true is queued,
 * and the finally block below sends it the moment the *actual* awaited
 * call resolves, recursing if another one arrives in the meantime.
 *
 * The queued text is tracked separately from the box's own `inputValue`,
 * not stuffed back into it: CopilotChatInput's internal send() always calls
 * onChange("") right after onSubmitMessage(), unconditionally, so anything
 * written back into inputValue from inside onSubmitMessage is immediately
 * clobbered by that same synchronous call — confirmed by reading the
 * bundled source (CopilotChatInput's send()). Showing the queued text as a
 * separate chip (rendered by the caller from `queuedMessage`) sidesteps
 * that and is arguably closer to Claude Code's own UX anyway — the queued
 * line is visually distinct from the live input, not stale/editable text
 * sitting in the box you just pressed Enter on.
 *
 * <CopilotChat> already forwards inputValue/onInputChange/onSubmitMessage/
 * onStop straight to its internal input while keeping everything else
 * (message list, tool cards, reasoning display) intact — no need to
 * replace its `input` slot, just control these four props.
 *
 * A second, separate thing <CopilotChat> does internally that this app lost
 * by switching to <CopilotChatView> (a presentational-only component, no
 * agent-connection logic of its own): actually reacting to a thread switch.
 * useAgent({agentId}) resolves ONE shared agent object keyed only by
 * agentId — reassigning agent.threadId (which useAgent itself does
 * reactively when fed an explicit thread via chat configuration) does not
 * by itself clear or reload agent.messages, so without this effect "New
 * chat" silently kept showing/appending to whatever thread was already
 * active. Confirmed by reading <CopilotChat>'s own bundled source
 * (copilotkit-D5BTo0YG.mjs): on a resolvedThreadId change it either calls
 * agent.setMessages([]) (no hasExplicitThreadId — an ephemeral new thread
 * with nothing to fetch) or awaits copilotkit.connectAgent({agent}) (an
 * explicit thread id — fetches that thread's real history). Replicated
 * here directly against the same public copilotkit API, not reimplemented.
 *
 * A third bug this hook has to guard against: main.py pauses the agent loop
 * itself (a real Strands interrupt, see AgentTools.tsx) for shell/
 * python_repl/file_write/skills_generated approval — that pause is NOT
 * "running" (copilotkit.runAgent()'s promise resolves as soon as the
 * interrupt fires, not once it's answered), so gating sends on busyness
 * alone let a plain new message straight through while an approval card was
 * still sitting unresolved. Strands then rejects the resume with "Thread
 * has N pending interrupt(s) not addressed by resume" — a real crash,
 * reproduced live. Busy AND pending-interrupt are now both derived from the
 * agent's own run-lifecycle events (onRunStartedEvent/onRunFinishedEvent/
 * onRunFinalized/onRunFailed — the same events useInterrupt itself uses to
 * track pending interrupts), not a locally-scoped ref: that also closes a
 * second gap the old ref-based busy tracking had — it only covered runs
 * *this hook* started, so a run kicked off elsewhere (approving/denying an
 * interrupt calls copilotkit.runAgent() itself, from AgentTools.tsx, not
 * from here) was invisible to it. Subscribing to the shared agent's own
 * events covers every run regardless of who started it.
 */
export function useQueueingAgentInput(
  agentId: string,
  threadId: string,
  hasExplicitThreadId: boolean,
  knownSkillNames: ReadonlySet<string> = new Set(),
) {
  const { agent } = useAgent({ agentId });
  const { copilotkit } = useCopilotKit();
  const [inputValue, setInputValue] = useState("");
  const [queuedMessage, setQueuedMessage] = useState<string | null>(null);
  const busyRef = useRef(false);
  const pendingInterruptRef = useRef(false);
  const queuedRef = useRef<string | null>(null);
  const previousThreadRef = useRef<string | null>(null);

  useEffect(() => {
    const threadChanged = previousThreadRef.current !== threadId;
    previousThreadRef.current = threadId;
    agent.threadId = threadId;

    if (!hasExplicitThreadId) {
      if (threadChanged && agent.messages.length > 0) agent.setMessages([]);
      return;
    }

    let cancelled = false;
    void (async () => {
      try {
        await copilotkit.connectAgent({ agent });
      } catch (error) {
        if (!cancelled) console.error("Failed to load thread:", error);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [threadId, hasExplicitThreadId, agent, copilotkit]);

  const runNow = async (text: string) => {
    agent.addMessage({ id: randomId(), role: "user", content: text });
    try {
      await copilotkit.runAgent({ agent });
    } catch (error) {
      console.error("Agent run failed:", error);
    }
  };

  useEffect(() => {
    const trySendQueued = () => {
      if (busyRef.current || pendingInterruptRef.current) return;
      const next = queuedRef.current;
      if (!next) return;
      queuedRef.current = null;
      setQueuedMessage(null);
      void runNow(next);
    };

    const subscription = agent.subscribe({
      onRunStartedEvent: () => {
        busyRef.current = true;
        pendingInterruptRef.current = false;
      },
      onRunFinishedEvent: (params) => {
        if (params.outcome === "interrupt") pendingInterruptRef.current = true;
      },
      onRunFinalized: () => {
        busyRef.current = false;
        trySendQueued();
      },
      onRunFailed: () => {
        busyRef.current = false;
        pendingInterruptRef.current = false;
        trySendQueued();
      },
    });
    return () => subscription.unsubscribe();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [agent]);

  const onSubmitMessage = async (text: string) => {
    const trimmed = text.trim();
    if (!trimmed) return;

    // /skill, $ shell, and @mention rewriting happens once, here, before the
    // busy/queue check below — a queued message should already be in its
    // final, agent-ready form when it's parked, not re-rewritten (or
    // re-fetching @mention content) at send time later. See
    // chatInputTriggers.ts for what each trigger actually does.
    const rewritten = await rewriteInputTrigger(trimmed, knownSkillNames);

    if (busyRef.current || pendingInterruptRef.current) {
      // Busy, or paused on an unresolved approval card: park it as a queued
      // chip instead of a real send — the subscription above sends it once
      // the in-flight run (or pending interrupt) actually clears.
      queuedRef.current = rewritten;
      setQueuedMessage(rewritten);
      return;
    }
    void runNow(rewritten);
  };

  return {
    messages: agent.messages,
    isRunning: agent.isRunning,
    inputValue,
    onInputChange: setInputValue,
    onSubmitMessage,
    onStop: () => copilotkit.stopAgent({ agent }),
    queuedMessage,
    clearQueuedMessage: () => {
      queuedRef.current = null;
      setQueuedMessage(null);
    },
  };
}

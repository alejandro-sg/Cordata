"use client";

import { useEffect } from "react";

/**
 * generateSandboxedUi's own renderer (CopilotKit's OpenGenerativeUIRenderer)
 * mounts a plain, class-less <div style="position: relative"> and injects an
 * <iframe> into it imperatively via @jetbrains/websandbox — there's no React
 * prop or CopilotKit config to opt into a pop-out/fullscreen affordance, and
 * no stable className to target from our own CSS. This mounts a
 * MutationObserver instead: whenever a new <iframe> shows up anywhere in the
 * page, drop a small button into its already-`position: relative` parent
 * and wire it to the browser's native Fullscreen API directly on that
 * iframe — no CopilotKit internals touched, works with iframes it injects
 * at any point during generation.
 *
 * GameSandbox.tsx's own iframe is explicitly excluded below (`.game-sandbox
 * -frame`) — that panel already has its own dedicated fullscreen button in
 * its header, so a second floating one here would be redundant, and worse:
 * that iframe is deliberately remounted on every open_game call (a fresh
 * `key={game_id}:{version}`, so a human can reopen or fix-and-retry the
 * *same* game_id and actually see the new content) — real, observed churn
 * this MutationObserver was never written to expect.
 *
 * The real bug behind three separate live "Element is not connected"
 * reports, each seemingly surviving its own fix: this effect appends
 * buttons to the DOM *imperatively* (outside React's own tree), but its
 * cleanup only ever did `observer.disconnect()` — it never removed the
 * buttons themselves. Every dev-mode edit to this file makes Next's Fast
 * Refresh re-run the effect; each re-run's fresh `seen` WeakSet has no idea
 * a button already exists from the *previous* run, so it adds a second one
 * on top, and the first one — with whatever onclick closure that older
 * version of the code built — is never removed and stays clickable
 * forever. Every "fix" to the click handler's body only changed what *new*
 * buttons do; it did nothing for the stale one(s) already sitting in the
 * DOM from before the edit, which is almost certainly what kept getting
 * clicked. `buttons` below is what actually closes this: the cleanup
 * function now removes every button this effect run added, so a fresh
 * effect run (from HMR, or a real remount) never leaves an orphaned one
 * behind with a closure from a stale module version. This is also a
 * correctness fix independent of dev-mode HMR: React 18 Strict Mode
 * double-invokes effects in dev for exactly this class of bug, and any
 * future real remount of this component would have hit the identical
 * accumulation without a cleanup that undoes its own DOM mutations.
 *
 * requestFullscreen() on a disconnected element doesn't throw synchronously
 * in every browser — confirmed live (Chromium here returns a *rejected
 * Promise* with message "Element is not connected"; Safari reportedly
 * throws synchronously for the same case) — so the click handler still
 * guards both forms: an isConnected check first, try/catch around the call,
 * and .catch() on what it returns.
 */
export default function SandboxedUiFullscreen() {
  useEffect(() => {
    const seen = new WeakSet<HTMLIFrameElement>();
    const buttons: HTMLButtonElement[] = [];

    const addButton = (iframe: HTMLIFrameElement) => {
      if (seen.has(iframe)) return;
      if (iframe.classList.contains("game-sandbox-frame")) return;
      const parent = iframe.parentElement;
      if (!parent) return;
      seen.add(iframe);

      if (!iframe.isConnected || !parent.isConnected) return;

      try {
        const button = document.createElement("button");
        button.type = "button";
        button.textContent = "⛶ Fullscreen";
        button.setAttribute("aria-label", "View this sandboxed UI fullscreen");
        Object.assign(button.style, {
          position: "absolute",
          top: "8px",
          right: "8px",
          zIndex: "20",
          padding: "4px 10px",
          fontSize: "12px",
          lineHeight: "1.4",
          borderRadius: "6px",
          border: "1px solid #d0d0d0",
          background: "rgba(255,255,255,0.92)",
          color: "#333",
          cursor: "pointer",
        } satisfies Partial<CSSStyleDeclaration>);
        button.onclick = () => {
          if (!iframe.isConnected) return;
          try {
            iframe.requestFullscreen?.()?.catch(() => {});
          } catch {
            // Synchronous throw (Safari) for the same disconnected-element
            // case the .catch() above handles for browsers that reject
            // instead — see this file's own module comment.
          }
        };
        parent.appendChild(button);
        buttons.push(button);
      } catch {
        // Whatever's left of the TOCTOU window above (isConnected was true a
        // moment ago, node detached since) — a missing fullscreen button is
        // never worth crashing the whole app over.
      }
    };

    const scan = () => {
      document.querySelectorAll("iframe").forEach((el) => {
        try {
          addButton(el as HTMLIFrameElement);
        } catch {
          // One stale iframe node must not abort the rest of this scan.
        }
      });
    };

    scan();
    const observer = new MutationObserver(scan);
    observer.observe(document.body, { childList: true, subtree: true });
    return () => {
      observer.disconnect();
      // Undo this run's own DOM mutations, not just the observer — see this
      // file's own module comment on why leaving these behind is the actual
      // bug every earlier fix here missed.
      for (const button of buttons) button.remove();
    };
  }, []);

  return null;
}

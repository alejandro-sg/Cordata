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
 * Two separate "Element is not connected" crashes were reported live, at
 * two different times relative to this button's own lifecycle:
 * 1. During addButton itself — scan() reads querySelectorAll("iframe")
 *    synchronously but the observer callback runs async, so a fast remount
 *    could detach the node before addButton got to it. isConnected checks
 *    right before mutating, plus try/catch around the mutation and the scan
 *    loop, close that window.
 * 2. Later, from a genuine user click — the button's onclick closure holds
 *    the *iframe* reference, not just the parent, and that iframe can go
 *    stale between button creation and the actual click (the sandboxed-UI
 *    iframe it was made for got replaced by a newer generateSandboxedUi
 *    call, or removed some other way) — calling `.requestFullscreen()` on a
 *    disconnected element throws synchronously in some browsers (Safari,
 *    matching the reported stack) and returns a rejected Promise in others,
 *    so the click handler below guards both: an isConnected check first, a
 *    try/catch around the call itself, and a .catch() on what it returns.
 */
export default function SandboxedUiFullscreen() {
  useEffect(() => {
    const seen = new WeakSet<HTMLIFrameElement>();

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
    return () => observer.disconnect();
  }, []);

  return null;
}

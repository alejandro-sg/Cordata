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
 * this MutationObserver was never written to expect. Live-reported crash:
 * "Element is not connected" thrown from addButton, because scan() reads
 * document.querySelectorAll("iframe") synchronously but the observer
 * callback itself runs async (a microtask after the mutation record), so a
 * fast remount can have already detached the exact node addButton is about
 * to touch by the time it runs. isConnected checks right before mutating
 * (not just at addButton's own entry, which raced the same way) close that
 * window; try/catch below is defense in depth for whatever's left, since
 * a floating cosmetic button crashing the whole app is strictly worse than
 * it silently not appearing once.
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
          void iframe.requestFullscreen?.();
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

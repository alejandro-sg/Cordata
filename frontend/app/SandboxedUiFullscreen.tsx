"use client";

import { useEffect } from "react";

/**
 * generateSandboxedUi's own renderer (CopilotKit's OpenGenerativeUIRenderer)
 * mounts a plain, class-less <div style="position: relative"> and injects an
 * <iframe> into it imperatively via @jetbrains/websandbox — there's no React
 * prop or CopilotKit config to opt into a pop-out/fullscreen affordance, and
 * no stable className to target from our own CSS. This mounts a
 * MutationObserver instead: whenever a new <iframe> shows up anywhere in the
 * page (the only iframes this app ever renders are sandboxed-UI ones), drop
 * a small button into its already-`position: relative` parent and wire it to
 * the browser's native Fullscreen API directly on that iframe — no
 * CopilotKit internals touched, works with iframes it injects at any point
 * during generation.
 */
export default function SandboxedUiFullscreen() {
  useEffect(() => {
    const seen = new WeakSet<HTMLIFrameElement>();

    const addButton = (iframe: HTMLIFrameElement) => {
      if (seen.has(iframe)) return;
      const parent = iframe.parentElement;
      if (!parent) return;
      seen.add(iframe);

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
    };

    const scan = () => {
      document.querySelectorAll("iframe").forEach((el) => addButton(el as HTMLIFrameElement));
    };

    scan();
    const observer = new MutationObserver(scan);
    observer.observe(document.body, { childList: true, subtree: true });
    return () => observer.disconnect();
  }, []);

  return null;
}

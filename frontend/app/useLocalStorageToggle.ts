"use client";

import { useEffect, useState } from "react";

/**
 * A boolean preference persisted to localStorage — the pattern dark theme,
 * the CopilotKit inspector toggle, and the sub-agents panel toggle all need
 * identically (read once on mount since localStorage doesn't exist during
 * SSR, then write through on every change). Factored out once these hit
 * three near-identical copies in page.tsx.
 */
export function useLocalStorageToggle(key: string, defaultValue: boolean): [boolean, () => void] {
  const [value, setValue] = useState(defaultValue);

  useEffect(() => {
    try {
      const stored = localStorage.getItem(key);
      if (stored !== null) setValue(stored === "true");
    } catch {
      // localStorage unavailable (private mode, etc.) — fall back to the default.
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const toggle = () => {
    setValue((prev) => {
      const next = !prev;
      try {
        localStorage.setItem(key, String(next));
      } catch {
        // ignore
      }
      return next;
    });
  };

  return [value, toggle];
}

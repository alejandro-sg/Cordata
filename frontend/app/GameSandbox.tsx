"use client";

import { useEffect, useRef, useState } from "react";
import { useAgent } from "@copilotkit/react-core/v2";
import { registerGameFrame } from "./gameSandboxRegistry";

type ActiveGame = { game_id: string; title: string; version?: string };

/**
 * Renders the browser game open_game announced (backend/main.py's
 * `_game_state` pushes agent.state.active_game = {game_id, title}) in a
 * `sandbox="allow-scripts"` <iframe> — same security posture as the
 * existing generative-UI iframe (scripts only, no forms/same-origin; see
 * gameSandboxRegistry.ts for why that means auth-by-event.source instead of
 * event.origin). Renders nothing when no game is open. `onHide` is the same
 * state ThreadSidebar's "Game panel" checkbox drives (passed down from
 * page.tsx, mirroring SwarmProgress's onHide) — a second, more discoverable
 * place to turn the whole thing off besides a checkbox buried in the
 * settings footer, persisted (useLocalStorageToggle) so it stays off across
 * reloads until turned back on.
 *
 * Unlike SwarmProgress.tsx's fullscreen (which renders a second, independent
 * copy of its stateless node list in an overlay — harmless duplication for
 * plain JSX), this stays a SINGLE <aside> whose class alone changes on
 * fullscreen toggle, so the <iframe> is never unmounted/remounted by the
 * switch: the exact same DOM node just gets repositioned via CSS. A second
 * live iframe would mean a genuinely duplicate running game instance with
 * its own script state, and gameSandboxRegistry's one-entry-per-game_id map
 * would only ever forward postMessage traffic to whichever copy registered
 * last — silently talking to the wrong instance the other one shows.
 */
export default function GameSandbox({ onHide }: { onHide: () => void }) {
  const { agent } = useAgent({ agentId: "sierralab_agent" });
  const [fullscreen, setFullscreen] = useState(false);
  const iframeRef = useRef<HTMLIFrameElement | null>(null);

  const state = agent.state as { active_game?: ActiveGame } | undefined;
  const activeGame = state?.active_game;

  useEffect(() => {
    if (!fullscreen) return;
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") setFullscreen(false);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [fullscreen]);

  useEffect(() => {
    const iframe = iframeRef.current;
    if (!activeGame || !iframe) return;
    return registerGameFrame(activeGame.game_id, iframe);
    // The <iframe> below is keyed by game_id+version, so *every* open_game
    // call remounts it fresh (even re-announcing the same game_id — see
    // main.py's own comment on why) and this effect re-runs to register the
    // new node — never left pointing at a previous game's stale frame.
  }, [activeGame?.game_id, activeGame?.version]);

  if (!activeGame) return null;

  return (
    <aside
      className={`game-sandbox${fullscreen ? " game-sandbox-fullscreen" : ""}`}
      role={fullscreen ? "dialog" : undefined}
      aria-modal={fullscreen ? true : undefined}
      aria-label={fullscreen ? `${activeGame.title}, fullscreen` : undefined}
    >
      <div className="game-sandbox-header">
        <div className="game-sandbox-title">{activeGame.title}</div>
        <div className="game-sandbox-actions">
          <button
            type="button"
            className="game-sandbox-icon-btn"
            onClick={() => setFullscreen((f) => !f)}
            aria-label={fullscreen ? "Exit fullscreen" : "Open game in fullscreen"}
            title={fullscreen ? "Exit fullscreen" : "Fullscreen"}
          >
            {fullscreen ? "Exit fullscreen  ✕" : "⛶"}
          </button>
          {!fullscreen && (
            <button
              type="button"
              className="game-sandbox-icon-btn"
              onClick={onHide}
              aria-label="Hide game panel"
              title="Hide panel"
            >
              ✕
            </button>
          )}
        </div>
      </div>
      <iframe
        key={`${activeGame.game_id}:${activeGame.version ?? ""}`}
        ref={iframeRef}
        src={`/api/games/${encodeURIComponent(activeGame.game_id)}/index.html`}
        sandbox="allow-scripts"
        className="game-sandbox-frame"
        title={activeGame.title}
      />
    </aside>
  );
}

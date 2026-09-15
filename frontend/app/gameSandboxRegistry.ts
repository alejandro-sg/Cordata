"use client";

/**
 * Module-level (not React state): GameTools.tsx's useFrontendTool handlers
 * run outside any component's render tree and need synchronous access to
 * whichever <iframe> GameSandbox.tsx currently has mounted for a given
 * game_id — there's no prop/context path from a frontend-tool handler down
 * into a specific DOM node, so this is the shared handle both sides use.
 */
const frames = new Map<string, HTMLIFrameElement>();

/** Called by GameSandbox.tsx on mount; the returned cleanup must run on
 * unmount (or game_id switch) so a stale/unmounted iframe never lingers as
 * the target of a later postToGame call. */
export function registerGameFrame(gameId: string, iframe: HTMLIFrameElement): () => void {
  frames.set(gameId, iframe);
  return () => {
    if (frames.get(gameId) === iframe) frames.delete(gameId);
  };
}

type Pending = { resolve: (value: unknown) => void; iframe: HTMLIFrameElement };

const pending = new Map<string, Pending>();

// One shared listener for every postToGame() call, not one per call — two
// concurrent calls (e.g. the human clicking while the agent also calls
// get_game_state, or two sub-agents' moves interleaving through the
// orchestrator) must not race over the same 'message' event.
if (typeof window !== "undefined") {
  window.addEventListener("message", (event: MessageEvent) => {
    const data = event.data as { type?: string; requestId?: string; state?: unknown } | undefined;
    if (!data || data.type !== "sierralab:state" || !data.requestId) return;
    const waiter = pending.get(data.requestId);
    if (!waiter) return;
    // event.source identity, not event.origin: sandbox="allow-scripts" (no
    // allow-same-origin) gives the game iframe an opaque origin, which always
    // reads as the literal string "null" — comparing event.source against
    // the exact iframe this requestId was sent to is what's left to confirm
    // the reply actually came from that game, not a lookalike message from
    // some other frame.
    if (event.source !== waiter.iframe.contentWindow) return;
    pending.delete(data.requestId);
    waiter.resolve(data.state ?? data);
  });
}

/**
 * Sends one request to the named game's iframe and waits for its reply,
 * matched by a fresh requestId — see GameTools.tsx for the two callers
 * (get_game_state/apply_game_move) and backend/skills/build-a-game/SKILL.md
 * for the postMessage contract every agent-built game must implement to
 * answer this. Rejects (never hangs) if the game isn't open or doesn't
 * reply within timeoutMs.
 */
export async function postToGame(gameId: string, message: Record<string, unknown>, timeoutMs = 4000): Promise<unknown> {
  const iframe = frames.get(gameId);
  if (!iframe || !iframe.contentWindow) {
    throw new Error(`Error: game '${gameId}' isn't open right now.`);
  }

  const requestId = crypto.randomUUID();
  const result = new Promise<unknown>((resolve, reject) => {
    pending.set(requestId, { resolve, iframe });
    setTimeout(() => {
      if (pending.delete(requestId)) {
        reject(new Error(`Error: game '${gameId}' didn't respond within ${timeoutMs}ms.`));
      }
    }, timeoutMs);
  });

  // targetOrigin "*": the frame's own origin is the opaque "null" described
  // above, so there is no real origin string to target — every game iframe
  // this registry talks to is one this app itself just wrote and loaded via
  // open_game, not a third-party frame, so a broadened target here isn't
  // handing anything to an untrusted party.
  iframe.contentWindow.postMessage({ ...message, requestId }, "*");
  return result;
}

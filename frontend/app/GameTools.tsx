"use client";

import { useFrontendTool } from "@copilotkit/react-core/v2";
import { z } from "zod";
import { postToGame } from "./gameSandboxRegistry";

/**
 * The two agent-facing halves of the postMessage contract every game built
 * via backend/skills/build-a-game/SKILL.md must implement. Frontend tools,
 * not backend ones — see backend/game_tool.py's own module docstring — the
 * actual game state lives inside a specific <iframe> only the browser has a
 * handle on, so get_game_state/apply_game_move have to run here.
 * gameSandboxRegistry.ts owns the request/reply/timeout plumbing both
 * share; open_game (backend/game_tool.py) is the third, backend-side tool
 * that announces a game exists in the first place.
 *
 * Errors are returned as plain "Error: ..." strings, not thrown — a thrown
 * exception from a frontend tool handler surfaces to the model as an opaque
 * tool-call failure; a returned string is something the model can actually
 * read and react to (e.g. try open_game again, or tell the human what went
 * wrong).
 *
 * Must render somewhere under <CopilotKitProvider>, same as AgentTools.tsx —
 * no visual output of its own.
 */
export default function GameTools() {
  useFrontendTool({
    name: "get_game_state",
    description:
      "Read the current state of an open browser game (board, whose turn, outcome, etc — shape is game-specific). Call open_game first if the game hasn't been announced yet.",
    parameters: z.object({
      game_id: z.string().describe("The game's slug, exactly as passed to open_game."),
    }),
    handler: async ({ game_id }) => {
      try {
        return await postToGame(game_id, { type: "sierralab:getState" });
      } catch (err) {
        return err instanceof Error ? err.message : String(err);
      }
    },
  });

  useFrontendTool({
    name: "apply_game_move",
    description:
      "Submit a move to an open browser game. `move` is game-specific (e.g. a cell index) — call get_game_state first if unsure what shape this game expects.",
    parameters: z.object({
      game_id: z.string().describe("The game's slug, exactly as passed to open_game."),
      move: z.string().describe("The move to make, in whatever format this game's own SKILL.md example expects."),
    }),
    handler: async ({ game_id, move }) => {
      try {
        return await postToGame(game_id, { type: "sierralab:applyMove", move });
      } catch (err) {
        return err instanceof Error ? err.message : String(err);
      }
    },
  });

  return null;
}

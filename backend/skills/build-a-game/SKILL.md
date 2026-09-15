---
name: build-a-game
description: Write a real interactive browser game the human can play directly, and you (or sub-agents) can play too via get_game_state/apply_game_move.
---

One self-contained HTML file per game, written with `file_write` (your existing,
already-approval-gated tool — no new write path exists for this), announced with
`open_game` so it opens in the human's game panel. The file must implement a small
postMessage contract so `get_game_state`/`apply_game_move` can actually read and drive
it — copy the boilerplate below verbatim and only fill in the two functions it leaves
for you.

## 1. File convention

Write to `games/<game_id>/index.html` — one file, inline `<style>` and `<script>`,
canvas or SVG/DOM only. No `<img src="...">` referencing a separate asset: `file_write`
is text-only, so there's nowhere for a second file to come from. `game_id` is a slug:
1-64 characters, lowercase letters/digits/hyphens, e.g. `tic-tac-toe`.

## 2. The postMessage boilerplate (copy this verbatim)

Paste this into your `<script>` unchanged. It handles the message contract; you only
write `getState()` (return the current board/state as a plain JSON-serializable object)
and `applyMove(move)` (validate and apply a move, return `{ok: true, ...}` or
`{ok: false, reason: "..."}`), then call `pushState()` yourself from your own click
handlers whenever the human's own interaction changes the state.

```html
<script>
  // --- sierralab game bridge: do not edit below this line ---
  function reply(requestId, state) {
    parent.postMessage({ type: "sierralab:state", requestId, state }, "*");
  }
  function pushState() {
    parent.postMessage({ type: "sierralab:state", state: getState() }, "*");
  }
  window.addEventListener("message", (event) => {
    const msg = event.data;
    if (!msg || typeof msg !== "object") return;
    if (msg.type === "sierralab:getState") {
      reply(msg.requestId, getState());
    } else if (msg.type === "sierralab:applyMove") {
      const result = applyMove(msg.move);
      reply(msg.requestId, { ...getState(), ...result });
    }
  });
  // --- sierralab game bridge: do not edit above this line ---

  // Your own click handlers (direct DOM/canvas interaction, human plays for free)
  // must call pushState() after they change anything, e.g.:
  //   cell.addEventListener("click", () => { ...update board...; pushState(); });
</script>
```

Every `state` object your `getState()` returns should include `ok: true` (or `ok:
false, reason: "..."` for a rejected move) — the two frontend tools return whatever
you put in `state` verbatim to the model, so make it self-describing (board, whose
turn, `winner` once decided) rather than something that needs the model to guess at
the shape.

## 3. Announcing it

Call `open_game(game_id, title)` only *after* `file_write` on `games/<game_id>/index.html`
has actually succeeded — `open_game` writes nothing itself, it only checks that file
exists, and errors clearly if you call it first. This call needs no approval; the
`file_write` before it is the one that does.

## 4. Playing it yourself

Once `open_game` has run, the game panel is open in the human's browser and any click
they make there works with zero tool calls (pure DOM/JS, `pushState()` reporting it
back to you live). You can also play: call `get_game_state(game_id)` to read the board,
`apply_game_move(game_id, move)` to make a move — `move` is whatever shape your own
`applyMove()` expects (document it in your reply if it's not obvious, e.g. "cell index
0-8").

## 5. Two sub-agents playing each other

`local_swarm`/`local_graph` sub-agents are plain, memory-less `strands.Agent`s with no
AG-UI connection — they cannot call `get_game_state`/`apply_game_move` themselves (see
`sub-agent-team`'s own tool-exclusion rules; those two are frontend-proxied tools,
excluded from every sub-agent the same way `shell`/`file_write` are). **You are the
required referee** — relay every move by hand, always re-stating the full board since
a fresh sub-agent call has no memory of the previous one. Concretely, once a game is
open:

1. `get_game_state(game_id)` — read the current board.
2. `local_graph(task="It's your move. Board:\n<board as plain text>\nYou are 'X'. Reply with only the cell index (0-8) you choose.", agents=[{"name": "player_x", "system_prompt": "You are a tic-tac-toe strategist playing as X. Think briefly, then reply with only a single digit 0-8."}])`
   — one fresh sub-agent call per move, not a persistent one: it has no memory between
   calls, so the board text in `task` is its only knowledge of the game so far.
3. `apply_game_move(game_id, move=<the digit the sub-agent replied with>)`.
4. Repeat step 2-3 for `player_o` with the updated board, alternating, until
   `get_game_state`'s `winner` (or equivalent) field is set.
5. Report the winner to the human.

This is a hard architectural constraint, not a simplification you could optimize away —
sub-agents genuinely have no path to call a frontend tool (see `game_tool.py`'s and
`sub_agent_common.py`'s own comments on why `open_game`/`get_game_state`/
`apply_game_move` are excluded from every sub-agent's tool set), so the orchestrator
relaying moves one at a time is the only way two sub-agents can play each other at all.

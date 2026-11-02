# Cordata mark

A **C** and a terminal cursor cell. The ring is the cord and the sandbox boundary; the amber cell sitting in its mouth is
the agent's next action, waiting at the approval gate. In a terminal it is literally `C█`.

## Brief

What the product is, from `ARCHITECTURE.md` and `docs/orchestrator.md`: several coding agents on one job; every edit and
command asks first; commands run inside a Seatbelt boundary; work moves between pools when a quota runs out; checkpoints
make a turn undoable; it runs in a terminal and in a GPUI window, on the 2049 theme (amber = "needs you", teal = agent).

The mark has to:

1. be drawn from one of those facts, not from a pun found afterwards;
2. survive one colour, 16 px, and a blur (it must stay one object);
3. exist as terminal text, since the TUI is the primary front end;
4. carry state with a **shape** cue, not hue alone (`hud.rs`: "colour is never the only signal");
5. not read as a moon, a heart, an infinity sign, a fork or a crop box.

## Candidates, scored

1–5 per criterion (a judgement made from the renders, not a measurement), weighted: truth to the product 20, one object at small sizes 25, one colour 10, distinct and
unambiguous 15, exists as terminal text 15, state by shape 15.

| candidate | idea | score | why it lost or won |
|---|---|---|---|
| **C + cursor cell** | the gate is the cursor | **79** | wins: true to the medium, `C█` is the text form, states come from real cursor conventions |
| chord ring (first version) | circle cut by a chord | 62 | strongest silhouette, but the chord is a pun; the amber half-disc has no meaning, reads as a moon / `C)`, no text form, states were hue only |
| frame + core cell | corner brackets round a cell | – | keeps the old brackets but reads as a screenshot / focus icon |
| bracket-C + cell | square `[` with a cell | – | reads as an `E` |
| circle ∩ square | two model families, agreement in amber | – | the idea is good (cross-family audit) but it is mush at 16 px and needs three colours |
| fan-in | three lanes merge | – | reads as a fork / trident; every devtool has one |
| interlocking C's | handoff between pools | – | reads as a heart or an infinity sign |
| gate + dot | dot waiting at a bar | – | too generic to own |

The honest cost of the winner: at small sizes the cell and the ring read as two objects where the chord ring read as
one. Stroke 9 and a cell 10 wide are the weights that keep them balanced; thinner strokes make the cell a speck.

## Geometry

64 grid. Ring: outer radius 28, stroke 9, cut vertically at x = 45. Cell: 10 × 24 at x = 49, y = 20, which is the cell
aspect of a terminal (about 1 : 2.4), 4 units clear of the ring, 1.9 units inside the ring's mouth. The bounding box
(4 to 59) is centred on the ring, so the mark crops cleanly into a circle or a squircle.

## State

The cell is the only thing that moves, and each state differs in shape and place as well as colour:

| state | cell | colour | terminal text |
|---|---|---|---|
| idle (unfocused) | hollow, in the mouth | ink at 55% | `C▯` |
| working | solid, **inside** the ring (in the sandbox) | teal | `(█` |
| needs you (the brand mark) | solid, in the mouth (at the gate) | amber | `C█` |
| done | none; the ring closes | ink | `O` / `◯` |
| error | solid, split by a slash | red | `C▚` |

Hollow for idle is the terminal convention for an unfocused cursor.

## Files

| file | use |
|---|---|
| `cordata-mark.svg` | dark grounds (2049, graphite, dim) |
| `cordata-mark-light.svg` | the `light` theme's paper |
| `cordata-mark-mono.svg` | one colour, `currentColor`: favicons, print, embossing |
| `cordata-mark-states.svg` | the five states above |
| `cordata-icon.svg`, `cordata-icon-1024.png`, `Cordata.icns` | macOS app icon; `package-app.sh` ships the `.icns` |

Colours are theme tokens: ink `#ECE7DE`, amber `#E8A254`, teal `#5FB3B3`, danger `#E56050`; on paper ink `#16181D`,
amber `#7F470B`. Keep a quarter of the mark's width clear on every side; do not go below 16 px.

Rebuild the icon after editing `cordata-icon.svg`: render it to a transparent 1024 px PNG with headless Chrome
(`--default-background-color=00000000 --window-size=1024,1024 --screenshot`), `sips -z` each size into an `.iconset`,
then `iconutil -c icns`.

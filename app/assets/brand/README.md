# Cordata brand

In Italian a *cordata* is a roped party: climbers tied to one rope, one moving while another holds. Cordata is that
for you and your agents. The agent does the climbing; you hold the rope and decide at every gate. Everything below
is drawn from that, and every element has one job. Anything that only decorates is left out.

Three rules run through all of it:

1. **The loop has a gap, and the gap is yours.** The agent's work is a loop; it is never drawn closed while something
   is still yours to decide.
2. **Nothing moves unless something happened.** No spinner, shimmer or breathing: motion is caused by a real event, so
   a quiet picture means a quiet agent.
3. **Say it in plain words.** A surface is named for what you do there, not for how it was built.

## The mark

An open loop and a node. The loop is the cord, and the sandbox the agent works inside. The gap is the gate, where you
stand. The node is the live thing: the agent's next action. In the logo the node sits in the gap, on the loop's own
path: the loop is only complete with you in it.

The mark has to be drawn from a fact about the product, survive one colour at 16 px, and carry state by **shape and
place** as well as colour. It must not read as a moon, a power button, a target or a face.

Geometry, on a 64 grid: loop radius 21 (centre line), stroke 7, round ends, open 42° either side of east; node radius 7
at (53, 32), on the loop's path, 4 units clear of each end. The loop is centred in the box, so the mark crops into a
circle or a squircle. Keep a quarter of its width clear on every side; do not go below 14 px.

### State

Only the node changes, and where it is says what is happening.

| state | drawn | colour | as text |
|---|---|---|---|
| idle | the open loop alone | ink at 55% | `C` |
| working | node **inside** the loop (in the sandbox) | signal | `(•` |
| needs you | node **in the gap** (at the gate) | attention | `C●` |
| done | no node; the loop closes | ink | `◯` |
| error | a slash across the gap | danger | `C/` |

The app draws these as vectors (`ui::mark`); the text forms are for the menu bar and the terminal.

## Colour

| role | Ink (dark) | Paper (light) | means |
|---|---|---|---|
| ground | `#111110` | `#F6F4EF` | warm graphite and bone; columns are told apart by hairlines |
| ink | `#E8EAED` | `#111418` | text and the loop |
| signal (viridian) | `#3DC9A4` | `#0A6553` | the live thing: the node, the cord, your `›`, focus |
| attention (ochre) | `#E2A640` | `#8A5100` | something needs you — and nothing else |
| second voice (lavender) | `#A096E6` | `#5B4BA8` | delegated or untrusted work |
| danger | `#E5584E` | `#A8231B` | failure, destruction |

The tokens live in `app/crates/cordata-theme/src/tokens.rs`, with contrast tests (AA everywhere, AAA for text under
macOS Increase Contrast). No state is told by colour alone.

Everything you are meant to read or find is held to a number, and a test fails if it slips: text (down to the
faintest label) is 4.5:1 on every ground in every theme; every state of the mark, at rest and dimmed, is 3:1; no
text is set under 11 px. Tighter radii: badge 2, control 4, card 4, popover 6, modal 8.

## Type

Three voices, using native macOS system fonts only (no bundled or fetched fonts):

| face | for |
|---|---|
| reading serif (Charter → Iowan Old Style → Georgia, shipped with macOS) | the agent's prose, brief, gate question (22 px), empty-state hero (40 px) |
| SF Pro (`.SystemUIFont`) | your words, rope bar, and chrome |
| SF Mono → Menlo | machine facts: paths, commands, marginalia, counts |

Scale (1.2 ratio): 11 · 12 · 13 · 14 · 16 · 17 (prose) · 19 · 22 · 28 · 40.

## The layout: "The Rope"

A *cordata* is a roped party. The rope is the layout: one continuous line the work hangs from.

- **Rope bar**: a hairline across the top with each open thread as a mark node on it. The active thread's name is set in serif. A thread that needs you sits *in a gap* in the rope with an ochre attention node. ⌘B opens the full **Logbook** (Threads / Earlier / Other checkouts) as a drawer.
- **The page**: a manuscript column (680 px) with a **120 px left margin**. The **cord** runs down that margin as a vertical line. Every event is a node on it, with mono marginalia (turn no., time, diff counts).
- **Anchors and pitches**: your prompt is an *anchor* (sans, medium weight, a small square knot on the cord). The agent's prose is a *pitch* in Charter at 17 px. Tool calls are quiet lines off the cord.
- **The next line (composer)**: no box. A caret line at the end of the cord, with `›` hanging in the margin, a hairline above, and model / mode / send key in small mono right-aligned beneath.
- **Tool sheet (⌘\)**: Files, Editor, Review and Terminal slide in over the page from the right (drawer with tight modal shadow).
- **Empty state**: optical centre headline "What should change?" at 40 px serif, a short stub of cord dropping into the caret, and numbered starters (`01` Map how this project fits together).

## The standout: the gate

The gate is when the agent needs you, which is the core of the product. It is the only expressive moment in the app; everything else stays quiet.

1. **Arrive** (240 ms): the cord above stops, opens a **gap**, and the node travels into it, turning ochre.
2. The gate is set across the page as an editorial pull-quote:
   - the question in **22 px Charter**
   - the exact command in a ruled mono block (top and bottom rules only)
   - **If allowed / If denied** as a two-column ledger
   - the choices as text buttons with keycaps (`Allow ↵` · `Always (cargo)` · `Reply W` · `Deny esc`)
3. **Close** (280 ms): when you decide, the two ends of the cord meet over the gap and the line carries on. Deny leaves a small danger-coloured slash in the margin as a record.

## Icons

Cordata's own set (`app/assets/icons`, loaded by `app/crates/cordata-app/src/icons.rs`), drawn from the mark's two
parts: an open line and, where something is set, a solid node. 16 grid, **1.25 stroke**, round caps, `currentColor`;
a test holds every file to that. 17 icons in total, including `logbook`, `sheet`, `terminal`, `files`, `review`, and `gate`.

An icon is used in exactly two cases:

1. **It is the whole control** — no room for a word — and it carries a tooltip: search, the two panel toggles, new
   thread, close, settings, attach, dictate, refresh.
2. **It shows a state you would otherwise have to read**: the chevron of a folder or a select, open or closed.

An icon next to a word that already says the same thing is decoration, and is not used. Three of them carry the
mark: search is a loop open toward its handle, refresh is the loop with a direction, settings is two cords with a
node on each.

## Sound

Two cues, synthesised in `app/crates/cordata-app/src/sound.rs` (no audio files), each for something you could not see
happen. Off unless you turn on Settings → General → Sounds.

| cue | shape | when |
|---|---|---|
| needs you | two notes rising a fourth (A5 → D6): a question | an approval arrives, or the terminal's agent is waiting, out of sight |
| done | two notes falling a major third (E6 → C6): a resolution | a thread finishes while you are elsewhere |

Never while you are looking at the thing, never for a turn you cancelled, at most one every three seconds. A sine with
a faint second harmonic and a soft attack, under half a second, peak at 22% of full scale.

## Motion

Nothing loops. Each movement is started by an event and runs once (`app/crates/cordata-app/src/motion.rs`), so
what you see is what the agent is doing, not a clock. A thread that has stopped talking goes dim and still: you can
tell stuck from busy without reading anything.

| movement | started by | says |
|---|---|---|
| **draw**: cord extends down to the new node (180 ms) | a new block | the cord extends down, replacing settle |
| **beat**: the node lights and falls back to 70% (900 ms) | an event from that thread | this is alive, and how recently |
| **tug**: one pulse runs the header's cord, left to right (640 ms) | events on the thread you are looking at; a burst is one pass | work is passing along the cord |
| **arrive / close**: gate opens with ochre gap (240 ms) / decisions close it (280 ms) | gate opening or being answered | the standout moment of agent supervision |
| **slide**: drawer moves in (240 ms ease-out-quint) | ⌘B (Logbook), ⌘\ (Tool sheet) | drawers slide in over the page |
| **tick**: numeric value rolls vertically (220 ms) | count changes (+/−, turns, cost) | digits roll into place (NumberFlow-style) |
| **stack**: notice arrives bottom-right and stacks | notification or toast | notifications arrive without jumping |

One live thing per place: the rail's mark beats, the header tugs, the newest tool call beats. Everything else that is
live is said still, in the signal colour. Events faster than a beat keep the node lit, so nothing blinks. All of it is
off under macOS Reduce Motion; the still picture carries the same state.

A thread's own events drive this. An orchestrator run is polled, not heard, so its mark is still.

## Words

A surface is named for what you do there: "See every thread at once", "Review what changed". Not "Supervisory Control
Room", not "Proof-First Change Sheet".

## Files

| file | use |
|---|---|
| `cordata-mark.svg` | dark grounds (Ink, graphite, dim) |
| `cordata-mark-light.svg` | Paper |
| `cordata-mark-mono.svg` | one colour, `currentColor`: favicons, print, embossing |
| `cordata-mark-states.svg` | the five states above |
| `cordata-icon.svg`, `cordata-icon-light.svg`, `cordata-icon-1024.png`, `cordata-icon-light-1024.png`, `Cordata.icns` | the macOS app icon, dark and light: flat ground, the mark, nothing else (macOS adds its own material); `package-app.sh` ships the `.icns` |
| `../icons/*.svg` | the icon set |

Rebuild the icon after editing `cordata-icon.svg`: render it to a transparent 1024 px PNG with headless Chrome
(`--headless=new --default-background-color=00000000 --window-size=1024,1024 --screenshot`), `sips -z` each size into an
`.iconset`, then `iconutil -c icns`.

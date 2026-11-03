# Cordata brand

Cordata is a quiet instrument: it shows what the agent is doing and gets out of the way. Every element below has one
job; anything that only decorates is left out.

## The mark

A **C** and a terminal cursor cell. The ring is the cord and the sandbox boundary; the cell in its mouth is the agent's
next action, waiting at the approval gate. In a terminal it is literally `C█`. The cell is the brand's one moving part,
the same signal colour as the window's live line and your `›`.

The mark has to be drawn from a fact about the product, survive one colour at 16 px, exist as terminal text, and carry
state by **shape** as well as colour. It must not read as a moon, a heart, an infinity sign, a fork or a crop box.

Geometry, on a 64 grid: ring outer radius 28, stroke 9, cut vertically at x = 45; cell 10 × 24 at x = 49, y = 20 (a
terminal cell's aspect), 4 units clear of the ring. The bounding box (4 to 59) is centred on the ring, so the mark crops
into a circle or a squircle. Keep a quarter of its width clear on every side; do not go below 16 px.

### State

The cell is the only thing that changes; each state differs in shape and place as well as colour.

| state | cell | colour | terminal text |
|---|---|---|---|
| idle (unfocused) | hollow, in the mouth | ink at 55% | `C▯` |
| working | solid, **inside** the ring (in the sandbox) | signal | `(█` |
| needs you | solid, in the mouth (at the gate) | attention | `C█` |
| done | none; the ring closes | ink | `O` |
| error | solid, split by a slash | danger | `C▚` |

## Colour

| role | Ink (dark) | Paper (light) | means |
|---|---|---|---|
| ground | `#0E1013` | `#FAFAF9` | one per theme; columns are told apart by hairlines |
| ink | `#E8EAED` | `#111418` | text and the ring |
| signal (viridian) | `#3DC9A4` | `#0A6553` | the live thing: the cell, the live line, your `›`, focus |
| attention (ochre) | `#E2A640` | `#8A5100` | something needs you — and nothing else |
| second voice (lavender) | `#A096E6` | `#5B4BA8` | delegated or untrusted work |
| danger | `#E5584E` | `#A8231B` | failure, destruction |

The tokens live in `app/crates/cordata-theme/src/tokens.rs`, with contrast tests (AA everywhere, AAA for text under
macOS Increase Contrast).

## Icons

An icon is used in exactly two cases, and each one is in the app's embedded set (`icon_assets!` in
`app/crates/cordata-app/src/main.rs`, Lucide):

1. **It is the whole control** — no room for a word — and it carries a tooltip: search, the two panel toggles, new
   thread, close, settings, attach, dictate, refresh.
2. **It shows a state you would otherwise have to read**: the chevron of a folder or a select, open or closed.

An icon next to a word that already says the same thing is decoration, and is not used.

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

Motion says that something changed or is alive; it never decorates. All of it stops under macOS Reduce Motion.

| motion | says |
|---|---|
| the live line (a signal segment sweeping the header's edge) | the agent is working |
| a working thread's edge mark breathing | that thread is alive, not stuck |
| the approval's attention rule drawing top to bottom (240 ms) | a decision just arrived |
| the newest transcript block rising 6 px into place (200 ms) | this is what just came in |

## Files

| file | use |
|---|---|
| `cordata-mark.svg` | dark grounds (Ink, graphite, dim) |
| `cordata-mark-light.svg` | Paper |
| `cordata-mark-mono.svg` | one colour, `currentColor`: favicons, print, embossing |
| `cordata-mark-states.svg` | the five states above |
| `cordata-icon.svg`, `cordata-icon-light.svg`, `cordata-icon-1024.png`, `Cordata.icns` | the macOS app icon in dark and light Apple liquid glass variants; `package-app.sh` ships the `.icns` |

Rebuild the icon after editing `cordata-icon.svg`: render it to a transparent 1024 px PNG with headless Chrome
(`--headless=new --default-background-color=00000000 --window-size=1024,1024 --screenshot`), `sips -z` each size into an
`.iconset`, then `iconutil -c icns`.

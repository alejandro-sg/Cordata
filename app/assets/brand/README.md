# Cordata mark

A circle cut by a chord. The ring is a **C**; the amber segment is the piece of the same disk on the other side of the
chord — the cursor, the part that is lit. *Cordata* is a chord: one straight line through a circle.

The mark sits on a 64 grid: disk radius 28 centred at (32, 32); ring stroke 8; the ring is cut at x = 41, the segment
starts at x = 46 (a 5-unit gap). Whole mark inscribes in the circle, so it crops cleanly to a circle or squircle.

| file | use |
|---|---|
| `cordata-mark.svg` | dark grounds (2049, graphite, dim) |
| `cordata-mark-light.svg` | the `light` theme's paper |
| `cordata-mark-mono.svg` | one colour, `currentColor`: favicons, print, terminals, embossing |
| `cordata-mark-states.svg` | the segment as a status light: idle (ink at 35%), working (teal), needs you (amber), error (red) |
| `cordata-icon.svg`, `cordata-icon-1024.png`, `Cordata.icns` | macOS app icon; `package-app.sh` ships the `.icns` |

Colours are the theme tokens: ink `#ECE7DE`, amber `#E8A254`, teal `#5FB3B3`, danger `#E56050`; on paper ink `#16181D`,
amber `#7F470B`. Keep a quarter of the mark's width clear on every side, and never go below 16 px.

Rebuild the icon after editing `cordata-icon.svg`:

```bash
chrome --headless --default-background-color=00000000 --window-size=1024,1024 --screenshot=cordata-icon-1024.png file://$PWD/cordata-icon.svg
# then sips -z each size into an .iconset and: iconutil -c icns Cordata.iconset -o Cordata.icns
```

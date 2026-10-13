"""Draw Cordata.app's icon (docs/app.md §4b) and write it as an .iconset folder for `iconutil`.

Near-black plate, sodium-amber HUD brackets at the corners, an amber arc (the C) with a teal scan line through it.
Drawn, not stored: the colours come from the palette's hex values, and the PNGs are regenerated at packaging time.

    backend/.venv/bin/python app/assets/make_icon.py OUT.iconset
"""

import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

GROUND = (7, 9, 12, 255)
SURFACE = (13, 17, 23, 255)
AMBER = (255, 158, 61, 255)
TEAL = (72, 214, 224, 255)

SIZES = [16, 32, 64, 128, 256, 512, 1024]


def draw(size: int = 1024) -> Image.Image:
    s = size / 1024
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    # The plate: macOS draws no mask on app icons, so the rounded square and its margin are ours.
    margin, radius = 100 * s, 185 * s
    d.rounded_rectangle([margin, margin, size - margin, size - margin], radius=radius, fill=GROUND, outline=SURFACE, width=max(1, int(6 * s)))
    # Glow: a soft amber haze rising from the bottom-left.
    glow = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    ImageDraw.Draw(glow).ellipse([150 * s, 520 * s, 700 * s, 1000 * s], fill=(255, 158, 61, 55))
    glow = glow.filter(ImageFilter.GaussianBlur(120 * s))
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle([margin, margin, size - margin, size - margin], radius=radius, fill=255)
    img.paste(glow, (0, 0), Image.composite(glow, Image.new("RGBA", (size, size)), mask))
    d = ImageDraw.Draw(img)
    # HUD brackets.
    w, arm, inset = max(2, int(22 * s)), 120 * s, 210 * s
    for x, y, dx, dy in ((inset, inset, 1, 1), (size - inset, inset, -1, 1), (inset, size - inset, 1, -1), (size - inset, size - inset, -1, -1)):
        d.line([(x, y), (x + dx * arm, y)], fill=AMBER, width=w)
        d.line([(x, y), (x, y + dy * arm)], fill=AMBER, width=w)
    # The C: an amber arc, open to the right.
    box = [330 * s, 330 * s, 694 * s, 694 * s]
    d.arc(box, start=40, end=320, fill=AMBER, width=max(2, int(54 * s)))
    # The scan line, in teal, through the opening.
    d.line([(300 * s, 512 * s), (724 * s, 512 * s)], fill=TEAL, width=max(1, int(14 * s)))
    return img


def main() -> None:
    out = Path(sys.argv[1] if len(sys.argv) > 1 else "Cordata.iconset")
    out.mkdir(parents=True, exist_ok=True)
    master = draw(1024)
    for size in SIZES:
        if size <= 512:
            master.resize((size, size), Image.LANCZOS).save(out / f"icon_{size}x{size}.png")
        if size >= 32:
            master.resize((size, size), Image.LANCZOS).save(out / f"icon_{size // 2}x{size // 2}@2x.png")
    print(out)


if __name__ == "__main__":
    main()

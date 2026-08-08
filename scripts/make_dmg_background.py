#!/usr/bin/env python3
"""Draw the DMG install background at 1x and 2x.

Kept as a generator rather than a committed PNG so the geometry can never drift
away from scripts/make_dmg.sh: the constants below are the same window size and
icon centres that script sets on the Finder window, and the arrow is placed from
them rather than by eye.

Run after changing either file:

    python3 scripts/make_dmg_background.py

The .DS_Store does not need rebaking for a pure artwork change — it references
the background by filename, not by content.

Needs Pillow. If it is missing the DMG still builds — make_dmg.sh treats the
background as optional and falls back to Finder's default arrangement.
"""

from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

# Must match scripts/make_dmg.sh: WIN_W/WIN_H, APP_X/APP_Y, LINK_X/LINK_Y, ICON_SIZE.
WIN_W, WIN_H = 600, 400
APP_X, APP_Y = 175, 200
LINK_X, LINK_Y = 425, 200
ICON_SIZE = 128

# Light, and not up for debate: Finder renders the icon labels ("datagrep",
# "Applications") in BLACK over a DMG background picture, regardless of whether
# the viewer is in light or dark mode. A dark background was tried first and the
# labels were practically invisible against it. Anything here must keep black
# text readable, so the palette stays near-white with dark ink drawn on top.
TOP = (252, 252, 253)
BOTTOM = (237, 239, 242)
# The site's light-mode accent — 5.1:1 on white, so the arrow stays visible.
ACCENT = (46, 125, 79)
INK = (28, 33, 38)
MUTED = (91, 100, 110)
FAINT = (150, 158, 167)

HELVETICA = "/System/Library/Fonts/Helvetica.ttc"


def font(size: int, index: int = 0) -> ImageFont.FreeTypeFont:
    try:
        return ImageFont.truetype(HELVETICA, size=size, index=index)
    except OSError:
        return ImageFont.load_default()


def draw(scale: int) -> Image.Image:
    w, h = WIN_W * scale, WIN_H * scale
    img = Image.new("RGB", (w, h), BOTTOM)
    d = ImageDraw.Draw(img)

    # Vertical gradient, one row at a time — cheap at this size and avoids
    # pulling in numpy just for a ramp.
    for y in range(h):
        t = y / max(h - 1, 1)
        d.line(
            [(0, y), (w, y)],
            fill=tuple(round(TOP[i] + (BOTTOM[i] - TOP[i]) * t) for i in range(3)),
        )

    def s(v: float) -> float:
        return v * scale

    # No logo mark here on purpose: the app's own 128px icon sits in this window
    # a few pixels below, so a second copy of it read as clutter and collided
    # with the wordmark.
    d.text((s(40), s(40)), "datagrep", font=font(round(s(20)), index=1), fill=INK, anchor="lm")

    # The one instruction. Drawn into the image rather than left to Finder, so
    # it is always legible and always says the same thing.
    d.text(
        (w / 2, s(96)),
        "Drag datagrep into your Applications folder",
        font=font(round(s(15))),
        fill=MUTED,
        anchor="mm",
    )

    # --- arrow, centred in the gap between the two icons --------------------
    # Centred on the midpoint of the two icon centres, not on the window, and
    # sized to leave clear air either side of both 128px icons.
    gap_l = APP_X + ICON_SIZE / 2
    gap_r = LINK_X - ICON_SIZE / 2
    mid_x, mid_y = s((APP_X + LINK_X) / 2), s((APP_Y + LINK_Y) / 2)
    length = min(s(78), s(gap_r - gap_l) - s(24))
    head_w, head_h = s(22), s(13)
    x0 = mid_x - length / 2
    tip = mid_x + length / 2
    d.rounded_rectangle([x0, mid_y - s(3), tip - head_w, mid_y + s(3)], radius=s(3), fill=ACCENT)
    d.polygon(
        [(tip - head_w, mid_y - head_h), (tip, mid_y), (tip - head_w, mid_y + head_h)],
        fill=ACCENT,
    )

    # Sits below the icon labels, which Finder draws at roughly y=270-290.
    d.text(
        (w / 2, s(352)),
        "Every database in one native app",
        font=font(round(s(11.5))),
        fill=FAINT,
        anchor="mm",
    )
    return img


def main() -> None:
    out = Path(__file__).resolve().parent.parent / "assets"
    out.mkdir(exist_ok=True)
    draw(1).save(out / "dmg_background.png")
    draw(2).save(out / "dmg_background@2x.png")
    print(f"wrote {out/'dmg_background.png'} (600x400) and @2x (1200x800)")


if __name__ == "__main__":
    main()

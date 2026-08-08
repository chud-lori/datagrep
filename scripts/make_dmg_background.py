#!/usr/bin/env python3
"""Draw the DMG install background at 1x and 2x.

Kept as a generator rather than a committed PNG so the geometry can never drift
away from scripts/make_dmg.sh: the constants below are the same window size and
icon centres that script sets on the Finder window, and the arrow is placed from
them rather than by eye.

Run after changing either file:

    python3 scripts/make_dmg_background.py

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

# Mid-dark slate, not light and not near-black, on purpose. Finder draws icon
# labels in the *viewer's* appearance — black in light mode, white in dark, each
# with a contrasting shadow. A light background loses the white labels and a
# near-black one loses the black ones; this tone keeps both legible and matches
# the app icon's tile so the DMG and the icon read as one product.
TOP = (59, 65, 73)
BOTTOM = (42, 47, 54)
ACCENT = (111, 181, 133)
TEXT = (242, 243, 245)
MUTED = (174, 180, 188)
FAINT = (124, 131, 140)
ROWS = (230, 232, 235)

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

    # --- brand mark: the grep lens over rows, same idea as the app icon ------
    # Positioned from the mark's own bounding box in the icon's 1024-unit space
    # (x 148..884, y 300..784) rather than by eye, so the wordmark beside it
    # cannot end up overlapping when the scale changes.
    BB_X0, BB_Y0, BB_X1, BB_Y1 = 148, 300, 884, 784
    mark_h = s(27)
    k = mark_h / (BB_Y1 - BB_Y0)
    mark_w = (BB_X1 - BB_X0) * k
    mark_x, mark_y = s(40), s(27)  # top-left of the bounding box

    def ix(v: float) -> float:
        return mark_x + (v - BB_X0) * k

    def iy(v: float) -> float:
        return mark_y + (v - BB_Y0) * k

    def bg_at(y: float) -> tuple:
        """The gradient's own colour at a given y, for the lens halo.

        A flat fill here reads as a visible disc, because the backdrop behind it
        is a gradient rather than the icon's solid tile.
        """
        t = min(max(y / max(h - 1, 1), 0.0), 1.0)
        return tuple(round(TOP[i] + (BOTTOM[i] - TOP[i]) * t) for i in range(3))

    for ry, rw in ((300, 520), (548, 440), (672, 260)):
        d.rounded_rectangle(
            [ix(148), iy(ry), ix(148 + rw), iy(ry + 88)], radius=(iy(88) - iy(0)) / 2, fill=ROWS
        )
    d.rounded_rectangle(
        [ix(148), iy(424), ix(148 + 320), iy(424 + 88)],
        radius=(iy(88) - iy(0)) / 2,
        fill=ACCENT,
    )
    d.ellipse(
        [ix(720 - 182), iy(620 - 182), ix(720 + 182), iy(620 + 182)], fill=bg_at(iy(620))
    )
    lw = max(1, round(52 * k))
    d.ellipse(
        [ix(720 - 130), iy(620 - 130), ix(720 + 130), iy(620 + 130)], outline=ROWS, width=lw
    )
    d.line([ix(814), iy(714), ix(884), iy(784)], fill=ROWS, width=lw)

    d.text(
        (mark_x + mark_w + s(11), mark_y + mark_h / 2),
        "datagrep",
        font=font(round(s(19)), index=1),
        fill=TEXT,
        anchor="lm",
    )

    # --- the one instruction ------------------------------------------------
    # Drawn into the image so it reads regardless of how Finder's own labels
    # land against the viewer's appearance.
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
    d.rounded_rectangle(
        [x0, mid_y - s(3), tip - head_w, mid_y + s(3)], radius=s(3), fill=ACCENT
    )
    d.polygon(
        [(tip - head_w, mid_y - head_h), (tip, mid_y), (tip - head_w, mid_y + head_h)],
        fill=ACCENT,
    )

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

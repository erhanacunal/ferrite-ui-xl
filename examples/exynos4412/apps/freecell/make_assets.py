#!/usr/bin/env python3
"""make_assets.py — generates the FreeCell app's assets into assets/.

    python make_assets.py

- Cards: the 52 faces (CARD_W x CARD_H, rounded corners with alpha), drawn
  here with PIL: rank + suit in the top-left corner (the part a card shows
  when another lies on it), the same turned round in the bottom-right, a
  large suit in the middle; J/Q/K get a framed panel with the letter.
  card_SR.png: S = suit index (0 clubs, 1 diamonds, 2 hearts, 3 spades),
  R = rank 1..13; image_id = S * 13 + R (main.fl `card_img`).
- Fonts: Ubuntu Regular (ferrite-ui fonts/Ubuntu-R.ttf), 2 bpp, through
  ferrite-ui tools/ferrite_font_converter.py (FERRITE_UI or ../ferrite-ui).

Everything is deterministic, so re-running gives the same files.
"""

import os
import subprocess
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

HERE = Path(__file__).resolve().parent
OUT = HERE / "assets"
FERRITE_UI = Path(os.environ.get("FERRITE_UI", HERE.parents[4] / "ferrite-ui"))
TTF = FERRITE_UI / "fonts" / "Ubuntu-R.ttf"

# Keep in sync with main.fl (CARD_W, CARD_H, CARD_R).
CARD_W, CARD_H, CARD_R = 128, 176, 10
SS = 4  # supersampling

RED = (196, 32, 44)
BLACK = (28, 30, 36)
FACE_BG = (252, 252, 248)
EDGE = (150, 154, 160)
RANKS = ["", "A", "2", "3", "4", "5", "6", "7", "8", "9", "10", "J", "Q", "K"]
PANEL = {0: (226, 232, 244), 1: (250, 230, 226), 2: (250, 230, 226), 3: (226, 232, 244)}

# Fonts: (output name, font id, size, ranges); ids match main.fl F_*.
TEXT_RANGES = "32-126,0xB7,0xD7,0x2022,0x2026"
FONTS = [
    ("f22", 1, 22, TEXT_RANGES),
    ("f40", 2, 40, TEXT_RANGES),
]


def suit_color(s):
    return RED if s in (1, 2) else BLACK


def draw_suit(d, s, cx, cy, size, fill):
    """Suit `s` centred at (cx, cy), `size` px tall (supersampled coords)."""
    h = size
    w = size * 0.92
    if s == 1:  # diamond
        d.polygon([(cx, cy - h / 2), (cx + w * 0.40, cy), (cx, cy + h / 2), (cx - w * 0.40, cy)], fill=fill)
        return
    if s == 2:  # heart: two lobes + a point
        r = w * 0.27
        top = cy - h * 0.20
        d.ellipse([cx - 2 * r, top - r, cx, top + r], fill=fill)
        d.ellipse([cx, top - r, cx + 2 * r, top + r], fill=fill)
        d.polygon([(cx - 2 * r + r * 0.06, top + r * 0.35), (cx + 2 * r - r * 0.06, top + r * 0.35),
                   (cx, cy + h / 2)], fill=fill)
        return
    if s == 3:  # spade: upside-down heart + stem
        r = w * 0.27
        bot = cy + h * 0.08
        d.ellipse([cx - 2 * r, bot - r, cx, bot + r], fill=fill)
        d.ellipse([cx, bot - r, cx + 2 * r, bot + r], fill=fill)
        d.polygon([(cx - 2 * r + r * 0.06, bot - r * 0.35), (cx + 2 * r - r * 0.06, bot - r * 0.35),
                   (cx, cy - h / 2)], fill=fill)
        d.polygon([(cx, bot), (cx + w * 0.16, cy + h / 2), (cx - w * 0.16, cy + h / 2)], fill=fill)
        return
    # club: three balls + stem
    r = w * 0.22
    d.ellipse([cx - r, cy - h / 2, cx + r, cy - h / 2 + 2 * r], fill=fill)
    d.ellipse([cx - 2.05 * r, cy - h * 0.08 - r, cx - 0.05 * r, cy - h * 0.08 + r], fill=fill)
    d.ellipse([cx + 0.05 * r, cy - h * 0.08 - r, cx + 2.05 * r, cy - h * 0.08 + r], fill=fill)
    d.polygon([(cx, cy - h * 0.2), (cx + w * 0.16, cy + h / 2), (cx - w * 0.16, cy + h / 2)], fill=fill)


def corner(s, rank, font, small):
    """The top-left index (rank + small suit side by side), as an RGBA tile."""
    tile = Image.new("RGBA", (CARD_W * SS, 40 * SS), (0, 0, 0, 0))
    d = ImageDraw.Draw(tile)
    col = suit_color(s)
    text = RANKS[rank]
    x = 7 * SS
    d.text((x, 1 * SS), text, font=font, fill=col, stroke_width=SS // 2, stroke_fill=col)
    tw = d.textlength(text, font=font) + SS
    draw_suit(d, s, x + tw + 12 * SS, 18 * SS, small, col)
    return tile


def make_card(s, rank, font, big_font):
    img = Image.new("RGBA", (CARD_W * SS, CARD_H * SS), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle([0, 0, CARD_W * SS - 1, CARD_H * SS - 1], radius=CARD_R * SS,
                        fill=FACE_BG, outline=EDGE, width=SS + SS // 2)
    col = suit_color(s)
    tile = corner(s, rank, font, 21 * SS)
    img.alpha_composite(tile, (0, 0))
    img.alpha_composite(tile.rotate(180), (0, (CARD_H - 40) * SS))
    cx, cy = CARD_W * SS / 2, CARD_H * SS / 2 + 2 * SS
    if rank >= 11:
        px0, py0, px1, py1 = 22 * SS, 44 * SS, (CARD_W - 22) * SS, (CARD_H - 44) * SS
        d.rounded_rectangle([px0, py0, px1, py1], radius=6 * SS, fill=PANEL[s], outline=col, width=SS * 2)
        letter = RANKS[rank]
        lw = d.textlength(letter, font=big_font)
        d.text((cx - lw / 2, py0 - 1 * SS), letter, font=big_font, fill=col, stroke_width=SS, stroke_fill=col)
        draw_suit(d, s, cx, py1 - 15 * SS, 20 * SS, col)
    else:
        draw_suit(d, s, cx, cy, (64 if rank == 1 else 52) * SS, col)
    return img.resize((CARD_W, CARD_H), Image.LANCZOS)


def make_cards():
    font = ImageFont.truetype(str(TTF), 30 * SS)
    big_font = ImageFont.truetype(str(TTF), 46 * SS)
    for s in range(4):
        for rank in range(1, 14):
            make_card(s, rank, font, big_font).save(OUT / f"card_{s}{rank:02d}.png")
    print("cards: 52")


def make_fonts():
    conv = FERRITE_UI / "tools" / "ferrite_font_converter.py"
    for name, fid, size, ranges in FONTS:
        subprocess.run([sys.executable, str(conv), "resource", str(TTF), str(size),
                        "--bpp", "2", "--font-id", str(fid), "-r", ranges,
                        "-o", str(OUT / f"{name}.font")], check=True,
                       stdout=subprocess.DEVNULL)
    print(f"fonts: {len(FONTS)}")


def main():
    if not TTF.exists():
        sys.exit(f"{TTF} not found (set FERRITE_UI)")
    OUT.mkdir(exist_ok=True)
    make_cards()
    make_fonts()


if __name__ == "__main__":
    main()

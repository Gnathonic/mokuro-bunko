"""Generate a synthetic Japanese manga volume (.cbz) for OCR testing.

Copyright-free: all pages are drawn locally with rendered Japanese text in
speech-bubble-like shapes so mokuro's text detector + manga-ocr have real
Japanese to read. Not real manga art -- purpose is to exercise the pipeline.
"""
from __future__ import annotations

import sys
import zipfile
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

OUT_DIR = Path(sys.argv[1])
CBZ_PATH = Path(sys.argv[2])

W, H = 1200, 1700
FONT_PATH = r"C:\Windows\Fonts\msgothic.ttc"

# (page bubbles): each bubble = (center_x, top_y, text, font_size)
PAGES: list[list[tuple[int, int, str, int]]] = [
    [
        (860, 180, "おはよう\nございます", 54),
        (360, 760, "きょうは\nいい天気\nですね", 48),
    ],
    [
        (820, 200, "本を\n読むのが\n好きです", 50),
        (360, 900, "この\nマンガは\n面白い", 52),
    ],
    [
        (840, 240, "日本語を\n勉強\nしています", 46),
        (380, 980, "がんばって\nください", 52),
    ],
    [
        (860, 300, "また\nあした\n会いましょう", 46),
        (360, 900, "さようなら", 60),
    ],
]


def draw_vertical_bubble(draw: ImageDraw.ImageDraw, cx: int, top: int,
                         text: str, font: ImageFont.FreeTypeFont) -> None:
    """Draw a rounded speech bubble with vertical Japanese text.

    text uses '\n' to separate columns; columns are laid out right-to-left
    (authentic Japanese vertical writing / tategaki).
    """
    columns = text.split("\n")
    ascent, descent = font.getmetrics()
    ch = ascent + descent
    col_gap = int(ch * 0.35)
    col_w = ch + col_gap
    max_chars = max(len(c) for c in columns)

    text_w = col_w * len(columns)
    text_h = ch * max_chars
    pad = int(ch * 0.9)

    left = cx - text_w // 2 - pad
    right = cx + text_w // 2 + pad
    bottom = top + text_h + 2 * pad
    draw.rounded_rectangle([left, top, right, bottom], radius=pad,
                           fill="white", outline="black", width=5)

    # Columns right-to-left.
    start_x = cx + text_w // 2 - col_w // 2
    for ci, col in enumerate(columns):
        x = start_x - ci * col_w
        y = top + pad
        for chpr in col:
            bbox = font.getbbox(chpr)
            gw = bbox[2] - bbox[0]
            draw.text((x - gw // 2 - bbox[0], y), chpr, font=font, fill="black")
            y += ch


def make_page(idx: int, bubbles: list[tuple[int, int, str, int]]) -> Image.Image:
    img = Image.new("RGB", (W, H), "white")
    d = ImageDraw.Draw(img)
    # Outer page border + a simple panel divider to look manga-ish.
    d.rectangle([20, 20, W - 20, H - 20], outline="black", width=4)
    d.line([20, H // 2, W - 20, H // 2], fill="black", width=3)
    # Page number (bottom-left, small).
    small = ImageFont.truetype(FONT_PATH, 28, index=0)
    d.text((40, H - 60), f"- {idx} -", font=small, fill="black")
    for cx, top, text, size in bubbles:
        font = ImageFont.truetype(FONT_PATH, size, index=0)
        draw_vertical_bubble(d, cx, top, text, font)
    return img


def main() -> None:
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    page_files = []
    for i, bubbles in enumerate(PAGES, 1):
        img = make_page(i, bubbles)
        p = OUT_DIR / f"page_{i:03d}.png"
        img.save(p)
        page_files.append(p)
        print(f"wrote {p} ({img.size[0]}x{img.size[1]})")

    CBZ_PATH.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(CBZ_PATH, "w", zipfile.ZIP_DEFLATED) as zf:
        for p in page_files:
            zf.write(p, arcname=p.name)
    print(f"packed {len(page_files)} pages -> {CBZ_PATH}")


if __name__ == "__main__":
    main()

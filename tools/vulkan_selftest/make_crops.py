"""Render the Vulkan self-test crops: synthetic Japanese text lines (no copyrighted pages).

    python make_crops.py <out dir> [--font-dir /usr/share/fonts/noto-cjk]

Writes `NN.png` (RGB) per crop and `crops.json` (file, what was drawn). The golden texts
are not what was drawn but what libtorch fp32 reads from each crop (`vulkan_check golden`):
the self-test proves a runtime reads exactly like the reference, not that the reference
is right. Fonts: Noto Sans/Serif CJK (SIL OFL 1.1); a rendered image is not the font.
"""

import argparse
import json
import random
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont

LINES = [
    "おはようございます",
    "これは何だ!?",
    "科学の力で",
    "ちょっと待って…",
    "ありがとう",
    "そんなバカな",
    "三千七百年後の世界",
    "俺は絶対に諦めない!!",
    "ふざけるなよ",
    "ここはどこ?",
    "千空",
    "ヤバい",
    "えっ",
    "今日もいい天気ですね",
    "本当に大丈夫なの?",
    "行くぞ!",
]


def font(dir_: Path, name: str, size: int) -> ImageFont.FreeTypeFont:
    # index 0 of the .ttc is the JP face
    return ImageFont.truetype(str(dir_ / name), size, index=0)


def horizontal(text, f, pad, fg, bg):
    l, t, r, b = f.getbbox(text)
    w, h = r - l + 2 * pad, b - t + 2 * pad
    im = Image.new("RGB", (w, h), bg)
    ImageDraw.Draw(im).text((pad - l, pad - t), text, font=f, fill=fg)
    return im


def vertical(text, f, pad, fg, bg):
    size = f.size
    step = int(size * 1.05)
    w, h = size + 2 * pad, step * len(text) + 2 * pad
    im = Image.new("RGB", (w, h), bg)
    d = ImageDraw.Draw(im)
    for i, ch in enumerate(text):
        l, t, r, b = f.getbbox(ch)
        x = pad + (size - (r - l)) // 2 - l
        y = pad + i * step + (step - (b - t)) // 2 - t
        d.text((x, y), ch, font=f, fill=fg)
    return im


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out", type=Path)
    ap.add_argument("--font-dir", type=Path, default=Path("/usr/share/fonts/noto-cjk"))
    a = ap.parse_args()
    a.out.mkdir(parents=True, exist_ok=True)
    rng = random.Random(803)
    faces = ["NotoSansCJK-Regular.ttc", "NotoSansCJK-Bold.ttc", "NotoSerifCJK-Regular.ttc",
             "NotoSerifCJK-Bold.ttc"]
    meta = []
    for i, text in enumerate(LINES):
        face = faces[i % len(faces)]
        size = rng.choice([22, 28, 34, 40])
        vert = i % 3 != 2
        fg, bg = (0, 0, 0), (255, 255, 255)
        if i % 5 == 4:
            fg, bg = (40, 40, 40), (226, 226, 226)  # screen-tone grey
        f = font(a.font_dir, face, size)
        im = (vertical if vert else horizontal)(text, f, rng.randint(3, 8), fg, bg)
        if i % 4 == 3:
            im = im.filter(ImageFilter.GaussianBlur(0.6))  # a soft scan
        name = f"{i:02d}.png"
        im.save(a.out / name)
        meta.append({"file": name, "drawn": text, "vertical": vert, "face": face, "size": size})
    (a.out / "crops.json").write_text(json.dumps(meta, ensure_ascii=False, indent=1))
    print(f"{len(meta)} crops in {a.out}")


if __name__ == "__main__":
    main()

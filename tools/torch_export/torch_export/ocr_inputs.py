"""Inputs for an OCR smoke run where no real volume may be used (CI runners, public logs).

    python -m torch_export.ocr_inputs <dir> [--pages N]

Writes ``<dir>/models/``, a model store for ``ocr_volume --models`` -- hayai-nova's host
files (``hayai-nova_{pos_table,token_embeddings}.npy``, ``hayai-nova_tokenizer.json``;
written as tools/onnx_export writes them: byte-identical to the models-v1 release files,
so the store's sha256 check accepts them) and the PP-OCR files from their pinned Hugging
Face revision -- and ``<dir>/synthetic.cbz``: pages of generated Japanese text in
vertical columns inside white "bubbles" (a system Japanese font; Latin text if none).
The pages are random but seeded: every run writes the same volume.
"""

from __future__ import annotations

import argparse
import io
import random
import shutil
import sys
import zipfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
FONTS = [
    "C:/Windows/Fonts/YuGothM.ttc", "C:/Windows/Fonts/msgothic.ttc", "C:/Windows/Fonts/meiryo.ttc",
    "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc", "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
]
KANA = ("あいうえおかきくけこさしすせそたちつてとなにぬねのはひふへほまみむめもやゆよらりるれろわをん"
        "アイウエオカキクケコサシスセソタチツテトナニヌネノハヒフヘホマミムメモヤユヨラリルレロワヲン")
WORDS = ["科学", "王国", "研究", "実験", "未来", "仲間", "文明", "時間", "世界", "名前", "電気", "魔法"]


def models(dst: Path) -> None:
    sys.path.insert(0, str(HERE.parents[1] / "onnx_export"))
    import numpy as np
    from onnx_export.common import snapshot_file
    from onnx_export.hayai.export import POS_TABLE, TOKEN_EMBEDDINGS, TOKENIZER, load_model
    from onnx_export.pins import HAYAI, PPOCR, PPOCR_FILES

    dst.mkdir(parents=True, exist_ok=True)
    model, _tok = load_model()
    vm = model.vision_encoder.vision_model if hasattr(model.vision_encoder, "vision_model") else model.vision_encoder
    np.save(dst / POS_TABLE, np.ascontiguousarray(vm.embeddings.position_embedding.weight.detach().float().numpy(), dtype="<f4"))
    np.save(dst / TOKEN_EMBEDDINGS, np.ascontiguousarray(model.decoder.token_embeddings.weight.detach().float().numpy(), dtype="<f4"))
    shutil.copyfile(snapshot_file(HAYAI, "tokenizer.json"), dst / TOKENIZER)
    (dst / "ppocr-manga").mkdir(exist_ok=True)
    for repo_path in PPOCR_FILES.values():
        shutil.copyfile(snapshot_file(PPOCR, repo_path), dst / "ppocr-manga" / Path(repo_path).name)


def volume(path: Path, pages: int) -> str:
    from PIL import Image, ImageDraw, ImageFont

    font_path = next((f for f in FONTS if Path(f).is_file()), None)
    rng = random.Random(7)
    with zipfile.ZipFile(path, "w", zipfile.ZIP_STORED) as z:
        for n in range(pages):
            img = Image.new("L", (1100, 1600), 225)
            d = ImageDraw.Draw(img)
            for _ in range(rng.randint(5, 8)):
                cols, size = rng.randint(1, 4), rng.randint(28, 40)
                font = ImageFont.truetype(font_path, size) if font_path else ImageFont.load_default(size)
                lines = []
                for _ in range(cols):
                    s = rng.choice(WORDS) + "".join(rng.choice(KANA) for _ in range(rng.randint(2, 8)))
                    lines.append(s if font_path else "".join(rng.choice("ABCDEFGHJKLMNPRSTUVWXYZ") for _ in s))
                w, h = cols * (size + 14) + 40, max(len(s) for s in lines) * (size + 4) + 50
                x, y = rng.randint(20, 1080 - w), rng.randint(20, 1580 - h)
                d.ellipse((x - 15, y - 15, x + w + 15, y + h + 15), fill=255, outline=0, width=3)
                for c, s in enumerate(lines):  # right to left, top to bottom
                    cx = x + w - 30 - c * (size + 14) - size
                    for i, ch in enumerate(s):
                        d.text((cx, y + 25 + i * (size + 4)), ch, fill=0, font=font)
            buf = io.BytesIO()
            img.save(buf, "PNG")
            z.writestr(f"{n + 1:03d}.png", buf.getvalue())
    return font_path or "(none: Latin text)"


def main(argv=None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("dir", type=Path)
    ap.add_argument("--pages", type=int, default=12)
    a = ap.parse_args(argv)
    a.dir.mkdir(parents=True, exist_ok=True)
    models(a.dir / "models")
    font = volume(a.dir / "synthetic.cbz", a.pages)
    print(f"{a.dir}: models/, synthetic.cbz ({a.pages} pages, font {font})")
    return 0


if __name__ == "__main__":
    sys.exit(main())

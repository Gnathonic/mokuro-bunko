"""Regenerate the bunko-vlm golden fixtures from the Python 0.5.2 code.

Run with an environment that has opencv-python-headless 5.0, Pillow 12.3, numpy,
tokenizers and transformers 5 (the dev box's ~/.cache/mokuro-bunko-demo/engines-env):

    ~/.cache/mokuro-bunko-demo/engines-env/bin/python crates/bunko-vlm/tests/golden/make_golden.py

Writes next to this file:
  page.png                 synthetic BGR page (stored as RGB PNG)
  hayai_<k>_<c>.png        engine_runner.make_line_crop_fn() crops of quad k, chunk c
  paddle_<k>_<em>.png      engine_runner.make_quad_crop_fn(em) crops (em = 25 / 50)
  rs_<k>_in.png, rs_<k>_<filter>.png   Pillow Image.resize inputs/outputs
  golden.json              quads, sizes, NaFlex / smart_resize sizes, detokenizer cases
"""

import json
import math
import random
import sys
from pathlib import Path

import numpy as np
from PIL import Image

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[3]
sys.path.insert(0, str(ROOT / "src" / "mokuro_bunko" / "ocr"))
import engine_runner as er  # noqa: E402

HF = Path.home() / ".cache/huggingface/hub"
HAYAI_TOK = HF / "models--JustANormalTinkerer--hayai-ocr-v2.5-nova/snapshots/e46d79138499600564f810d44ab6bdea7230dee1/tokenizer.json"
PADDLE_TOK = HF / "models--PaddlePaddle--PaddleOCR-VL-1.6/snapshots/c5630abae1d940eafe0697512a0325494b02ab42/tokenizer.json"


def page():
    rng = np.random.default_rng(11)
    h, w = 260, 360
    yy, xx = np.mgrid[0:h, 0:w]
    img = np.stack([200 + 40 * np.sin(xx / 17.0), 220 + 30 * np.cos(yy / 13.0), 235 - 0.1 * xx], -1)
    img += rng.normal(0, 6, img.shape)
    img = np.clip(img, 0, 255).astype(np.uint8)
    # "glyphs": dark blobs along a few lines, some tilted
    for _ in range(140):
        cx, cy = rng.integers(0, w), rng.integers(0, h)
        r = int(rng.integers(2, 6))
        img[max(0, cy - r) : cy + r, max(0, cx - r) : cx + r] = rng.integers(0, 60, 3)
    return img  # BGR order, by convention


def quads():
    def rot(q, deg, c):
        a = math.radians(deg)
        ca, sa = math.cos(a), math.sin(a)
        return [[c[0] + (x - c[0]) * ca - (y - c[1]) * sa, c[1] + (x - c[0]) * sa + (y - c[1]) * ca] for x, y in q]

    out = []
    out.append(([[20.0, 15.0], [44.5, 15.0], [44.5, 230.25], [20.0, 230.25]], True))  # upright column
    out.append((rot([[70, 20], [92, 20], [92, 200], [70, 200]], 4.0, (81, 110)), True))  # tilted column
    out.append(([[110.0, 30.0], [340.0, 30.0], [340.0, 52.0], [110.0, 52.0]], False))  # long line: chunked
    out.append((rot([[120, 80], [330, 80], [330, 98], [120, 98]], -3.0, (225, 89)), False))  # tilted, chunked
    out.append(([[300.0, 120.0], [372.0, 120.0], [372.0, 140.0], [300.0, 140.0]], False))  # off the page edge
    out.append(([[150.0, 150.0], [158.0, 150.0], [158.0, 162.0], [150.0, 162.0]], True))  # tiny
    out.append(([[200.0, 110.0], [222.0, 110.0], [222.0, 258.0], [200.0, 258.0]], True))  # column to the bottom
    out.append(([[10.0, 240.0], [355.0, 236.0], [355.5, 255.0], [10.5, 259.0]], False))  # very long: 3 chunks
    return out


def detok_cases(path, rng, n_ids):
    from tokenizers import Tokenizer

    tok = Tokenizer.from_file(str(path))
    t = json.loads(Path(path).read_text(encoding="utf-8"))
    special = [a["id"] for a in t["added_tokens"] if a["special"]]
    added = [a["id"] for a in t["added_tokens"] if not a["special"]]
    cases = []
    for k in range(40):
        ids = [rng.randrange(n_ids) for _ in range(rng.randrange(1, 12))]
        if k % 3 == 0:
            ids.insert(rng.randrange(len(ids) + 1), rng.choice(special))
        if k % 4 == 0 and added:
            ids.insert(rng.randrange(len(ids) + 1), rng.choice(added))
        cases.append({"ids": ids, "text": tok.decode(ids, skip_special_tokens=True)})
    return cases


def main():
    for f in HERE.glob("*.png"):
        f.unlink()
    img = page()
    Image.fromarray(img[:, :, ::-1]).save(HERE / "page.png")  # RGB PNG of the BGR page
    line_fn = er.make_line_crop_fn()
    quad_fn = {25: er.make_quad_crop_fn(0.25), 50: er.make_quad_crop_fn(0.5)}
    qs = []
    for k, (q, vertical) in enumerate(quads()):
        blk = {"vertical": vertical, "lines": [q]}
        chunks = line_fn(img, blk, 0)
        for c, ch in enumerate(chunks):
            ch.save(HERE / f"hayai_{k}_{c}.png")
        for em, fn in quad_fn.items():
            fn(img, blk, 0)[0].save(HERE / f"paddle_{k}_{em}.png")
        qs.append({"quad": q, "vertical": vertical, "chunks": len(chunks)})

    rng = np.random.default_rng(5)
    resizes = []
    for k, (iw, ih, ow, oh) in enumerate([(23, 17, 40, 31), (40, 120, 30, 90), (120, 40, 200, 70), (31, 9, 7, 3), (50, 50, 50, 28), (17, 40, 17, 77)]):
        src = Image.fromarray(rng.integers(0, 256, (ih, iw, 3), dtype=np.uint8))
        src.save(HERE / f"rs_{k}_in.png")
        for name, flt in (("bilinear", Image.BILINEAR), ("bicubic", Image.BICUBIC)):
            src.resize((ow, oh), flt).save(HERE / f"rs_{k}_{name}.png")
        resizes.append({"k": k, "out": [ow, oh]})

    # NaFlex sizes from transformers' own Siglip2 helper; smart_resize from PaddleOCR-VL's processor
    from transformers.models.siglip2.image_processing_siglip2 import get_image_size_for_max_num_patches
    from transformers.models.paddleocr_vl.image_processing_paddleocr_vl import smart_resize

    r = random.Random(3)
    sizes = [(64, 716), (716, 64), (64, 64), (3, 900), (64, 151), (37, 1200)] + [(r.randrange(8, 2000), r.randrange(8, 2000)) for _ in range(30)]
    naflex = []
    for h, w in sizes:
        for budget in (256, 384, 512):
            th, tw = get_image_size_for_max_num_patches(h, w, 16, budget)
            naflex.append({"hw": [h, w], "budget": budget, "out": [th, tw]})
    smart = []
    for h, w in sizes + [(10, 500), (500, 10), (28, 28 * 150)]:
        try:
            out = list(smart_resize(h, w, factor=28, min_pixels=112896, max_pixels=1003520))
        except ValueError:
            out = None
        smart.append({"hw": [h, w], "out": out})

    rr = random.Random(9)
    golden = {
        "quads": qs,
        "resize": resizes,
        "naflex": naflex,
        "smart_resize": smart,
        "detok_hayai": detok_cases(HAYAI_TOK, rr, 16004),
        "detok_paddle": detok_cases(PADDLE_TOK, rr, 101316),
    }
    (HERE / "golden.json").write_text(json.dumps(golden, ensure_ascii=False, indent=1), encoding="utf-8")
    print("wrote", len(qs), "quads,", sum(q["chunks"] for q in qs), "hayai crops")


main()

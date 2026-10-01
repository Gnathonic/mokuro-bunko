"""pillow_golden.json: Pillow 12.3 LANCZOS resizes (thumbnail resampler parity) and
ImageOps.contain sizes.

    ~/.cache/mokuro-bunko-demo/ref052-ocr/bin/python crates/bunko-ocr/tests/golden/gen_pillow_golden.py
"""

import base64
import json
from pathlib import Path

import numpy as np
from PIL import Image, ImageOps

HERE = Path(__file__).resolve().parent
rng = np.random.default_rng(3)
cases = []
for (w, h, W, H) in [(97, 131, 41, 57), (20, 15, 125, 94), (60, 84, 50, 70), (64, 64, 17, 90), (151, 9, 150, 9)]:
    a = rng.integers(0, 256, (h, w, 3), dtype=np.uint8)
    out = np.asarray(Image.fromarray(a, "RGB").resize((W, H), Image.Resampling.LANCZOS))
    cases.append({"w": w, "h": h, "W": W, "H": H,
                  "src": base64.b64encode(a.tobytes()).decode(), "dst": base64.b64encode(out.tobytes()).decode()})
contain = []
for (w, h) in [(1777, 2800), (3850, 2800), (100, 140), (50, 100), (1000, 1400), (251, 351), (3, 1000)]:
    im = ImageOps.contain(Image.new("RGB", (w, h)), (250, 350), Image.Resampling.LANCZOS)
    contain.append([w, h, im.width, im.height])
(HERE / "pillow_golden.json").write_text(json.dumps({"lanczos": cases, "contain": contain}))

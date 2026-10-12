"""Generate cv_golden.json: OpenCV 5.0 reference outputs for the primitives bunko-ocr
reimplements (resize INTER_LINEAR, warpPerspective INTER_CUBIC + BORDER_REPLICATE,
findContours RETR_LIST/SIMPLE, fillPoly, minAreaRect, ppocr.db_postprocess).

    ~/.cache/mokuro-bunko-demo/ref052-ocr/bin/python crates/bunko-ocr/tests/golden/gen_cv_golden.py
"""

import base64
import json
import sys
from pathlib import Path

import cv2
import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[3] / "src" / "mokuro_bunko" / "ocr"))
import ppocr  # noqa: E402

rng = np.random.default_rng(20261001)


def b64(a):
    """uint8 arrays as base64 of their bytes; float32 arrays as base64 of little-endian f32."""
    return base64.b64encode(np.ascontiguousarray(a).tobytes()).decode()


def smooth_image(h, w):
    base = rng.integers(0, 256, (h // 4 + 2, w // 4 + 2, 3), dtype=np.uint8)
    img = cv2.resize(base, (w, h), interpolation=cv2.INTER_CUBIC)
    noise = rng.integers(-20, 21, (h, w, 3))
    return np.clip(img.astype(int) + noise, 0, 255).astype(np.uint8)


out = {}

# resize
cases = []
for (h, w, H, W) in [(17, 23, 48, 64), (40, 61, 48, 40), (64, 96, 32, 48), (30, 200, 48, 320),
                     (120, 90, 47, 33), (31, 7, 48, 16), (100, 150, 64, 96), (48, 50, 48, 50)]:
    src = smooth_image(h, w)
    dst = cv2.resize(src, (W, H), interpolation=cv2.INTER_LINEAR)
    cases.append({"w": w, "h": h, "W": W, "H": H, "src": b64(src), "dst": b64(dst)})
out["resize"] = cases

# warp (crop_line)
src = smooth_image(72, 96)
warps = []
quads = [
    [[10.3, 5.7], [60.2, 8.1], [59.4, 30.6], [9.1, 28.2]],
    [[40.0, 2.0], [55.5, 2.5], [54.0, 70.25], [38.5, 69.75]],
    [[-5.5, -3.25], [30.0, -2.0], [31.0, 12.0], [-4.0, 10.0]],
    [[70.1, 50.2], [100.7, 49.9], [101.3, 80.4], [70.6, 81.0]],
    [[20.0, 20.0], [44.0, 30.0], [38.0, 45.0], [14.0, 35.0]],
]
import math
for _ in range(25):
    cx, cy = rng.uniform(-5, 100), rng.uniform(-5, 76)
    L, T, a = rng.uniform(3, 40), rng.uniform(2, 15), rng.uniform(-0.6, 0.6)
    c, s_ = math.cos(a), math.sin(a)
    q = [[cx + x * c - y * s_, cy + x * s_ + y * c] for x, y in [(-L, -T), (L, -T), (L, T), (-L, T)]]
    quads.append((np.array(q) + rng.normal(0, 0.8, (4, 2))).tolist())
for q in quads:
    q = np.array(q, np.float32)
    w, h = ppocr.quad_size(q)
    W, H = max(2, int(round(w))), max(2, int(round(h)))
    target = np.array([[0, 0], [W, 0], [W, H], [0, H]], np.float32)
    m = cv2.getPerspectiveTransform(q, target)
    crop = cv2.warpPerspective(src, m, (W, H), flags=cv2.INTER_CUBIC, borderMode=cv2.BORDER_REPLICATE)
    warps.append({"quad": q.tolist(), "W": W, "H": H, "m": m.ravel().tolist(), "dst": b64(crop),
                  "crop_line": b64(ppocr.crop_line(src, q)), "crop_shape": list(ppocr.crop_line(src, q).shape)})
out["warp"] = {"w": 96, "h": 72, "src": b64(src), "cases": warps}

# contours / fillPoly / minAreaRect on a blobby bitmap with holes
h, w = 120, 160
field = cv2.GaussianBlur(rng.random((h, w)).astype(np.float32), (0, 0), 3.0)
field = (field - field.min()) / (field.max() - field.min())
bitmap = ((field > 0.55) | ((field > 0.3) & (field < 0.36))).astype(np.uint8) * 255
bitmap[0:6, 0:9] = 255
bitmap[50:53, 155:160] = 255
contours, _ = cv2.findContours(bitmap, cv2.RETR_LIST, cv2.CHAIN_APPROX_SIMPLE)
cs = []
for c in contours:
    pts = c.reshape(-1, 2)
    x0, y0, bw, bh = cv2.boundingRect(c)
    mask = np.zeros((bh, bw), np.uint8)
    cv2.fillPoly(mask, [(pts - [x0, y0]).astype(np.int32)], 1)
    (cx, cy), (rw, rh), ang = cv2.minAreaRect(c)
    cs.append({"pts": pts.tolist(), "bbox": [x0, y0, bw, bh], "mask": b64(mask),
               "rect": [cx, cy, rw, rh, ang]})
out["contours"] = {"w": w, "h": h, "bitmap": b64((bitmap > 0).astype(np.uint8)), "contours": cs}

# db_postprocess on a smooth probability map
prob = cv2.GaussianBlur(rng.random((h, w)).astype(np.float32), (0, 0), 2.5)
prob = ((prob - prob.min()) / (prob.max() - prob.min())) ** 3
prob = prob.astype(np.float32)
res = ppocr.db_postprocess(prob)
out["db"] = {"w": w, "h": h, "prob": b64(prob.astype("<f4")),
             "boxes": [{"quad": q.tolist(), "score": s} for q, s in res]}

(HERE / "cv_golden.json").write_text(json.dumps(out))
print("contours", len(cs), "db boxes", len(res))

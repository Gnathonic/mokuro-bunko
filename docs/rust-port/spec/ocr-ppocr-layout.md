# Spec: PP-OCR manga detection/recognition, line layout, reconcile, sidecar output

Source of truth: mokuro-bunko **0.5.2** (`199cff5`), tree
`src/mokuro_bunko/ocr/`. Every claim cites `file:line` in that tree; paths below
are relative to `src/mokuro_bunko/`. Target: Rust + ONNX Runtime (`ort` crate),
no OpenCV.

In scope:

| Python | What | Port |
|---|---|---|
| `ocr/ppocr.py` | DBNet detector + CTC recognizer, page policies, joins, probes, votes | **port** |
| `ocr/line_layout.py` | lines → mokuro blocks (ruby, bodies, merge, paragraphs, order) | **port** |
| `ocr/line_reconcile.py` | merge a VLM read with the CTC read | port if the VLM engines are kept (see Q1) |
| `ocr/engine_runner.py` (parts) | page reading, crops for the VLMs, page/volume assembly | **port** |
| `ocr/detectors/_common.py`, `ocr/detectors/ppocr_manga.py` | adapter scaffold + ppocr adapter | reference only (the runner does not use the adapter for ppocr, see §7.0) |
| `ocr/detectors/{animetext,ctd,rtdetr}.py` | other detectors | **DROP** (shared helpers noted in §7.5) |
| `ocr/processor.py` (parts) | server-side sidecar normalization, cover thumbnail | port if the server side moves too |

Section map, matching the five requested topics: (1) detection = §3; (2) recognition and
model files = §1, §4; (3) detections to blocks per engine = §5–§7; (4) sidecar and
thumbnail = §8, §9; (5) archive/page reading = §2. Appendices: A = Python
semantics that bite a port, B = OpenCV primitives, C = `difflib.SequenceMatcher`.
Open questions are at the end.

---

## 0. Conventions used everywhere

* **Image**: `uint8` H×W×3 in **BGR** order (OpenCV order). Every model sees BGR.
  The ImageNet mean/std are applied in BGR order without an RGB swap
  (`ppocr.py:518-528`).
* **Quad**: 4 points `[x, y]` in page pixels, ordered **TL, TR, BR, BL of the
  line's own upright reading frame**. A tilted line has a tilted quad. In
  `ppocr.py` quads are `float32` numpy arrays (`ppocr.py:30-38`). In `line_layout`
  they are Python floats (f64) read back from JSON rounded to 0.01 px (§5.0).
* **angle**: degrees in (-45, 45], positive = clockwise on screen (y down)
  (`ppocr.py:36-37`).
* **vertical**: the reading axis is the quad's TL→BL edge, decided as
  `height > width` (`ppocr.py:466-473`).
* **em / thickness**: the short side of a line quad, i.e. the glyph size. Every layout
  threshold is in ems (`line_layout.py:27-30`).
* Python `round()` is **round-half-even, correctly rounded on the exact binary
  value**. It is used in many places that produce integers (`int(round(x))`) and in JSON
  rounding (`round(x, 2)`). Rust: `f64::round_ties_even()` for integers. For
  `round(x, n)`, see Appendix A.1.

---

## 1. Model files (PP-OCRv6 manga)

### 1.1 Repository and pin

* HF repo `Kellenok/PP-OCRv6_manga` (Apache-2.0), pinned revision
  **`ba1d479e8a61a20e8318c9758c73fbbbd290b98d`** ("v0.2", 2026-09-28)
  (`ppocr.py:79-83`).
* Files per precision (`ppocr.py:89-99`):

| precision | detector | recognizer |
|---|---|---|
| `fp32` (default) | `det/manga_det_v0.2.onnx` | `rec/manga_rec_v0.2.onnx` |
| `fp16` | `det/manga_det_v0.2_fp16.onnx` | `rec/manga_rec_v0.2_fp16.onnx` |

  Dictionary is `ppocrv6_dict.txt` at the repo root for both. The FP16 files take float32 input. They
  are slower on CPU and read 19 of 322 lines differently, so they buy only a smaller
  download (`ppocr.py:95-99`).
* Download URL: `https://huggingface.co/Kellenok/PP-OCRv6_manga/resolve/<rev>/<path>`
  (via `hf_hub_download(repo_id, filename, revision[, local_dir])`, `ppocr.py:380-387`).

**Present locally** (`~/.cache/huggingface/hub/models--Kellenok--PP-OCRv6_manga/`),
checked 2026-10-01:

| file (snapshot `ba1d479e…`) | bytes | sha256 |
|---|---|---|
| `det/manga_det_v0.2.onnx` | 1,816,954 | `d132078c46e292b226fb5a2ca52a7612ad319262dfdf493e7d8e3be435295978` |
| `rec/manga_rec_v0.2.onnx` | 21,167,540 | `de12c84c63e62c80339e882e675983d886670dcb6f0147e1ed041afd6fa81888` |
| `ppocrv6_dict.txt` | 74,947 | `b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d` (18,708 lines, LF, final newline) |

Also cached but **not used**: the v0.1 snapshot `3f5274450b5074e54e8c0997480bb7cdc5e875d7`
(`manga_det_v0.1.onnx`, `manga_rec_v0.1.onnx`). Note that `refs/main` in the local cache
points at that OLD commit. The port must resolve by the pinned sha, never by
`refs/main`. The fp16 files are not cached locally.

### 1.2 Resolution order (`resolve_models`, `ppocr.py:335-377`)

* `precision` comes from the argument, else `$MOKURO_PPOCR_PRECISION`, else `fp32`,
  lower-cased. An unknown value is an error.
* `download` comes from the argument, else `$MOKURO_PPOCR_DOWNLOAD`. It is false iff the value,
  stripped and lower-cased, is one of `0,false,no,off`. Unset or blank means true
  (`ppocr.py:320-324`).
* `base` comes from the argument, else `$MOKURO_PPOCR_MODELS` (with `~` expanded), else none.
* For each of (detector, recognizer, dict): if `base` is set, try `base/<repo path>`
  and then `base/<basename>` (`_find_local`, `ppocr.py:327-332`). If found, the file is
  used and **`pinned = False`**. Otherwise, with download disabled, fail with a message naming the
  file. With download enabled, download into `base` (as `local_dir`) or into the shared HF cache.
* `pinned` is true only when all three files came from the pinned download. It
  controls the sidecar's `weights` claim (§8.2).

### 1.3 Model I/O (verified with onnxruntime 1.30 on the cached files)

| model | input | output |
|---|---|---|
| detector | `x`: f32 `[N, 3, H, W]` (dynamic N/H/W) | `fetch_name_0`: f32 `[N, 1, H, W]`, **probabilities already** (sigmoid applied, range [0,1]) |
| recognizer | `x`: f32 `[N, 3, 48, W]` (dynamic N/W) | `fetch_name_0`: f32 `[N, W/8, 18710]`, **softmax probabilities** (each row sums to 1) |

The Python code takes the input name from `get_inputs()[0]` and uses output index 0
(`ppocr.py:1491-1501`). The detector result is read as `out[0][0, 0]` (H×W map).

Session options (`ppocr.py:1477-1483`): CPU execution provider only,
`intra_op_num_threads = threads`, and `log_severity_level = 3`. Everything else is default:
graph optimization ALL, sequential execution. `threads` is the argument, else
`$MOKURO_PPOCR_THREADS` if it is all digits, else 4. A value of 0 counts as unset
(`ppocr.py:1465-1466`). The engine is created lazily per model (`ppocr.py:1491-1501`).

### 1.4 Dictionary → vocab (`load_vocab`, `ppocr.py:390-400`)

Read as UTF-8 and split on `\n`. Strip **only a trailing `\r`** from each line, because
symbols can be whitespace. If the last element is `""` (final newline), drop it.
Vocab = `["", *symbols, " "]`: index 0 is the CTC blank and the last index is a space. With
the v0.2 dict this gives 1 + 18,708 + 1 = **18,710 classes**, which equals the model output dim.

---

## 2. Page and archive reading

### 2.1 Directory input (`list_pages`, `engine_runner.py:803-818`)

* Take every regular file under `input_dir`, recursively (`input_dir.glob("**/*")`), whose
  lower-cased suffix is in `IMAGE_EXTENSIONS = (".jpg", ".jpeg", ".png", ".webp",
  ".avif")` (`engine_runner.py:76`). Hidden files and `__MACOSX/` entries are
  **not** excluded.
* Paths are relative to `input_dir`, sorted by `natsort.natsorted` (natsort is
  installed in the engines env, `installer.py:96-104`). See §2.3. The fallback key is
  used only if natsort is missing (`_natural_key`, `engine_runner.py:765-768`).

### 2.2 Archive input (`.cbz` = zip) (`engine_runner.py:7298-7384`)

* Open with Python `zipfile`. Take `infolist()` entries that are not directories
  (`is_dir()`, meaning the name ends with `/`). Filenames are decoded as UTF-8 when flag bit 11 is
  set, else as **cp437** (Python zipfile default). The Info-ZIP Unicode-path extra field
  is ignored (see Q6).
* `extracted_name(member)` (`:7298-7312`): replace `/` (and the OS altsep) with the OS
  separator, drop a drive prefix, split, and **drop every empty, `.` and `..` component**.
  Rejoin. On Linux this means split on `/` and filter.
* `member_map(names, stem)` (`:7315-7340`): skip a landed name that is empty or whose
  lower-cased suffix is not in `IMAGE_EXTENSIONS`. Skip a landed name **exactly equal**
  (case-sensitive) to `f"{stem}.webp"`, where `stem` is the library archive's stem. This is the
  top-level embedded thumbnail some uploaders ship. When two members land on the same path,
  **the later one wins**, because extraction would overwrite. Nothing else is excluded.
* Pages = `natsorted(keys)` (`reading_order`, `:7288-7295`). A page's identity and its
  sidecar `img_path` is the landed relative path in POSIX form (`:7779`).
* Bytes are read with `ZipFile.read(member)`. A read error yields a page with empty bytes. That
  page then fails decode and takes the blank-page path (§8.4) (`:7518-7533`).
* The server's extract-to-directory road (`processor._extract_and_clean`,
  `processor.py:673-693`) applies the same thumbnail rule. The two roads are required to name
  the same pages and decode identical arrays (`engine_runner.py:4514-4523`).

### 2.3 Natural sort exactly as natsort 8.4 does it on `Path` objects (verified)

natsort 8.4.0 on `PosixPath` uses the **whole `str(path)`**. It does **not**
apply path splitting. The default algorithm is case-sensitive, unsigned integers.
The verified key is a tuple that alternates str and int:

```
key("a 2/x10.jpg") == ("a ", 2, "/x", 10, ".jpg")
key("10.jpg")      == ("", 10, ".jpg")          # leading "" when it starts with a digit
```

* Split the string on maximal runs of **Unicode decimal digits**. Full-width `１２`
  became the int 12 in the test. Each digit run becomes `int(run)`, so leading zeros are lost and
  `001`, `1`, `01` compare equal.
* Compare strings by code point, with **no case folding** (`B.jpg` < `_a.jpg` < `a.jpg`).
* The sort is stable, so equal keys keep their input order. For directories that order is the
  glob order; for archives it is the dict insertion order (first occurrence).
* Observed order: `001, 1, 01, 3, 9, 10, １２, B, _a, "a 2/1", "a.b/1", a.jpg, a/1,
  a/2, b, e, é, page-1.5, page-1.10, x9y99, x10y2, x10y10, z`.
* `-` and `.` are text, so `page-1.5 < page-1.10`.

### 2.4 Decoding (`_imdecode`, `engine_runner.py:1193-1199`; `ppocr.imread_bgr`, `ppocr.py:1795-1802`)

`PIL.Image.open(src).convert("RGB")` is converted to a numpy array and then to BGR.
The runner applies `cv2.cvtColor(RGB2BGR)`, which is a pure channel swap. Consequences a port
must reproduce:

* Only the **first frame** of an animated or multi-frame image is decoded.
* **No EXIF orientation is applied.** Pixels stay as stored.
* `convert("RGB")` from RGBA/LA/PA **drops alpha without compositing**. Palette images
  are expanded. L is replicated. CMYK uses Pillow's naive inverse. 16-bit modes use
  Pillow's conversion rules.
* Decoders: libjpeg-turbo (Pillow default, ISLOW IDCT, fancy upsampling), libpng,
  libwebp, and AVIF via Pillow 12.3.0's plugin (pinned, `installer.py:103`).
  Bit-exactness of JPEG decoding is an open question (Q4).

---

## 3. PP-OCR detection (DBNet)

### 3.1 Constants (`ppocr.py:108-155`)

```
DEFAULT_SIDE=1120  SIDE_MULTIPLE=32  MAX_UPSCALE=1.5
IMAGENET_MEAN=(0.485,0.456,0.406)  IMAGENET_STD=(0.229,0.224,0.225)   # applied in BGR order
DB_THRESH=0.15  DB_BOX_THRESH=0.25  DB_UNCLIP_RATIO=1.4  DB_MIN_SIDE=3.0  DB_MAX_CANDIDATES=3000
FINE_THICKNESS_PX=24.0  DENSE_LENGTH_RATIO=10.0  FINE_MIN_LINES=6
FUSED_THICKNESS_RATIO=1.7  FUSED_MIN_COUNT=2  FINE_TARGET_THICKNESS_PX=32.0
MAX_DETECTOR_PIXELS=2816*2048  TILE_SIZE=1536  TILE_OVERLAP=256  TILE_EDGE_MARGIN=8.0
STRIP_ASPECT=2.5
```

The runner always builds `PPOcr()` with side 1120 and tile `auto`
(`engine_runner.py:4653`). `MOKURO_PPOCR_SIDE` and `MOKURO_PPOCR_TILE` only affect the
standalone adapter (`detectors/ppocr_manga.py:545-547`).

### 3.2 Input size and tensor

```
detector_input_size(w, h, side=1120):                       # ppocr.py:504-515
    scale = min(side / max(w, h), 1.5)
    new_w = max(32, round_half_even(w*scale/32) * 32)
    new_h = max(32, round_half_even(h*scale/32) * 32)
_scaled_size(w, h, scale): same two lines with the given scale   # ppocr.py:1788-1792
```

`detector_tensor(bgr, (W,H))` (`ppocr.py:518-528`): `cv2.resize(bgr, (W,H),
INTER_LINEAR)` (uint8, see B.1), then `x = u8/255.0` in float32,
`x = (x - mean)/std` per channel in **BGR order with the RGB-named constants**
(channel 0 = B uses 0.485/0.229), HWC→CHW, batch 1.

### 3.3 DB post-processing (`db_postprocess`, `ppocr.py:531-571`)

Input: `prob` = H×W f32 map in detector pixels.

```
bitmap = (prob > 0.15) ? 255 : 0                              # strict >
contours = findContours(bitmap, RETR_LIST, CHAIN_APPROX_SIMPLE) # B.2: ALL borders incl. holes
for contour in contours[:3000]:
    (cx,cy),(w,h),angle = minAreaRect(contour)                 # B.3
    if min(w,h) < 3.0: continue
    score = contour_score(prob, contour)                       # below
    if score < 0.25: continue
    d = w*h*1.4 / (2*(w+h))   (0 if perimeter == 0)            # unclip_distance, ppocr.py:493-496
    w += 2d; h += 2d
    if min(w,h) < 3.0 + 2: continue
    emit (order_quad(rect_to_quad((cx,cy),(w,h),angle)), score)
```

* The unclip is the **closed form** of PaddleOCR's pyclipper offset. For a rectangle, offsetting
  by `d` and taking the min-area rect gives the same rectangle grown by `d` on every side.
  It differs from pyclipper only by pyclipper's integer rounding (`ppocr.py:546-553`). Do not
  port pyclipper.
* `contour_score` (`ppocr.py:574-579`) works on the contour's integer bounding rect `x0,y0,bw,bh`
  (`boundingRect`: `x0 = min x`, `bw = max x - min x + 1`). Make a zero `bh×bw` u8 mask
  and `fillPoly` the contour (shifted by `-x0,-y0`, int32) with value 1 (B.4). The score is
  the mean of `prob[y0:y0+bh, x0:x0+bw]` over mask≠0 pixels (`cv2.mean`, f64 sum / count,
  0 if empty).
* `rect_to_quad` (`ppocr.py:481-490`) uses half sizes `hw,hh`, `c = cos(angle°)`, `s = sin(angle°)`,
  and corners `(-hw,-hh),(hw,-hh),(hw,hh),(-hw,hh)` mapped to
  `(cx + x c - y s, cy + x s + y c)`, then cast to float32. Because `order_quad` canonicalizes
  afterwards, the result does not depend on which equivalent `(w,h,angle)` representation the
  min-area-rect routine returns. cv2 5.0 returns `angle = -90` for axis-aligned boxes, which is
  irrelevant here.
* Order of `contours` matters only for the 3000 cap and for union-find order in tiling
  (§3.6). Final lines are re-sorted (§3.8).

### 3.4 Quad geometry helpers (`ppocr.py:408-496`), all float32

* `order_quad(pts)` (`:408-448`): `centre = mean(pts)`. Sort the 4 points by
  `atan2(y - cy, x - cx)` ascending, which is clockwise on screen. For each `start` in 0..3, take the
  edge `pts[start+1] - pts[start]`, `dx = edge.x / |edge|` (−∞ if the length is 0). Keep the
  first `start` with `dx > best_dx + 1e-6`. Roll the points so that start comes first. The
  KNOWN LIMIT at >45° tilt is documented at `:423-433`.
* `quad_size(q) = (w, h)` with `w = (|q1-q0| + |q2-q3|)/2` and `h = (|q3-q0| + |q2-q1|)/2`.
* `quad_angle(q) = degrees(atan2(top.y, top.x))` with `top = (q1-q0)+(q2-q3)`.
* `quad_is_vertical = h > w`. `quad_thickness = min(w,h)`.

### 3.5 Pass policy (`PPOcr.detect`, `ppocr.py:1543-1596`)

```
h, w = image size
strip = max(w,h) / max(1, min(w,h)) >= 2.5
if tile == "force" or (tile == "auto" and strip):
    scale = 1.0 if tile == "force" else min(1.0, side / (min(w,h) * 1.5))
    lines = detect_tiled(bgr, scale)                       # 3.6
    return sort_lines(lines)
size = detector_input_size(w, h, side)
first = detect_region(bgr, size)       # [(page quad, score, thickness in DETECTOR px)]
first_scale = size.W / w
result = first
if tile == "auto":
    thick  = [t for each first]                            # detector px
    longs  = [max(quad_size(page quad)) * first_scale]    # page quad -> detector px
    why = needs_fine_pass(thick, longs)
    if why:
        scale = fine_scale(dense_median(thick, longs) or 0.0, first_scale)
        if scale > first_scale * 1.15:
            fs = _scaled_size(w, h, scale)
            if fs.W * fs.H <= 2816*2048: result = detect_region(bgr, fs)
            else:                       result = detect_tiled(bgr, scale)
return sort_lines(result)
```

* `detect_region(bgr, size, origin=(0,0))` (`:1505-1519`) runs the tensor, the network and
  `db_postprocess`. For each quad it records `thickness = quad_thickness(quad)` in **detector px**.
  The page quad is `quad * [w/size.W, h/size.H] + origin` in float32. x and y are scaled
  separately.
* `dense_median(thick, lengths)` (`:587-601`): `body = sorted(t for t, L if L >= 10*t)`.
  Return None if `len(body) < 6`, else `body[len//2]` (the **upper** median, not the mean of the middle two).
* `needs_fine_pass` (`:604-627`): median None → None. If `median < 24`, return a reason.
  Otherwise count `fused` = boxes with `L >= 10*median and t >= 1.7*median`, and return a reason if
  `fused >= 2`. Otherwise None.
* `fine_scale(m, first)` = `1.0 if m <= 0 else min(1.0, first*32/m)` (`:630-638`).
* The pass log `info = {"side","tile","passes":[{"scale": round(s,4), "tiles",
  "lines", "why"}]}` goes to the raw dump only (`:1555-1595`).

### 3.6 Tiling (`ppocr.py:641-866`, `_detect_tiled` `:1521-1541`)

```
tile_px    = max(256, int(1536 / scale))          # int() truncates
overlap_px = int(256 / scale)
grid = tile_grid(w, h, tile_px, overlap_px)
for idx, (x0,y0,x1,y1) in grid:                   # row-major: y outer, x inner
    region = bgr[y0:y1, x0:x1]
    for quad, score in detect_region(region, _scaled_size(x1-x0, y1-y0, scale), (x0,y0)):
        if clipped_by_tile(quad, tile, (w,h), overlap_px, 8.0/scale): drop
        else keep (quad, score, owner=idx)
return merge_tile_lines(...)
```

* `tile_grid` starts (`:650-655`): an extent `<= tile` gives `[0]`. Otherwise
  `count = ceil((extent-overlap)/(tile-overlap))`, `step = (extent-tile)/(count-1)`, and
  `starts = [round_half_even(i*step)]`. Tiles are `(x, y, min(x+tile,w), min(y+tile,h))`.
* `clipped_by_tile` (`:664-698`): with `lo/hi` = the axis-aligned bounds of the quad and
  `reach = hi - lo` per axis, drop the box if any of these holds:
  `x0>0 && lo_x <= x0+m && reach_x < ov/2`; `x1<w && hi_x >= x1-m && reach_x < ov/2`;
  the same for y.
* `merge_tile_lines(quads, scores, tiles)` (`:788-834`): union-find with path halving
  (`parent[find(j)] = find(i)`). Pairs `i<j` from **different** tiles whose
  axis-aligned bounds are not strictly separated (touching counts) are merged when
  `same_line`. Groups are collected in index order. A single box stays as it is. Otherwise
  `score = weighted mean(scores, weights = max(quad_size))` and `quad = union_quad(members)`.
  Then `_drop_fragments`.
* `same_line(a,b)` (`:736-764`): returns true if `quad_iou >= 0.5` (`contourArea` +
  `intersectConvexConvex`, B.5). Otherwise compute frames with `fa` the longer one. Require
  `0.7 <= thick_a/thick_b <= 1/0.7`, `fb.half_len >= 1.5*fb.half_thick`,
  `|dot(axis_a, axis_b)| >= cos(6°)`, `|dot(off, normal_a)| <= 0.35*min(thick)`, and
  `overlap = half_len_a + half_len_b - |dot(off, axis_a)| > 0.5*min(thick)`.
* `_frame(q)` (`:712-720`): `across = (q1-q0)+(q2-q3)` and `down = (q3-q0)+(q2-q1)`. The axis is
  `down` if `h >= w` else `across`; the normal is the other one. Both are normalized (a norm of 0
  is treated as 1). `half_len = max(w,h)/2` and `half_thick = min/2`.
* `union_quad(quads)` (`:767-785`): `ref` = the frame with the largest half_len (the first one on
  ties). Project all corners onto `ref.axis` to get lo and hi. `mid_n` = the half_len-weighted mean
  of each frame's centre offset along `ref.normal`. `half_thick` = the half_len-weighted mean of
  half_thick. The result is `order_quad([c-a-n, c+a-n, c+a+n, c-a+n])` with
  `c = ref.centre + axis*(lo+hi)/2 + normal*mid_n`, `a = axis*(hi-lo)/2`, and `n = normal*half_thick`.
* `_drop_fragments(lines, inside=0.6)` (`:837-866`): `areas = contourArea`. Line `i` is
  dropped if there exists `j != i` with `areas[j] > areas[i] > 0`,
  `seen_by[j]` **not a subset-or-equal** of `seen_by[i]`, and
  `intersectConvexConvex(i,j)/areas[i] >= 0.6`. `seen_by` is the frozenset of tile ids.
  Python `other <= seen` on frozensets means subset, so the check is
  `continue if other_seen_by ⊆ seen_by`.

### 3.7 Line object (`ppocr.py:1356-1372`)

`Line{quad f32[4,2], score, text="", conf=0.0, char_confs=[]}`. `vertical` and `angle`
are derived from the quad.

### 3.8 `sort_lines` (`ppocr.py:1375-1386`)

A stable sort by `(-centre.x, centre.y)`, where the centre is the mean of the 4 corners. This is not reading order.

---

## 4. PP-OCR recognition (CTC)

### 4.1 Constants (`ppocr.py:158-281`)

```
REC_HEIGHT=48 REC_STRIDE=8 REC_MIN_WIDTH=16 REC_MAX_BATCH=16 REC_MAX_PAD_WASTE=0.0
REC_WINDOW=2000 REC_WINDOW_OVERLAP=192 REC_WINDOW_GUARD=3
GAP_MIN_GLYPHS=6 GAP_MIN_PITCH=5.0 GAP_MIN_RATIO=1.5 GAP_MAX_FILL=3
GAP_BLANK_MAX_INK=0.02 GAP_DASH_MAX_INK=0.12 GAP_GLYPH_MIN_INK=0.2 GAP_INK_CONTRAST=0.5
IDEOGRAPHIC_SPACE="　" MISSING_GLYPH="〓" DASHES="―—ー" FOREIGN_MAX_CONF=0.5
VOTE_MAX_CONF=0.8 VOTE_MIN_GLYPHS=4 VOTE_WIDEN=(0.06,0.12)
PROBE_PAD_EM=1.0 PROBE_INSIDE_EM=2.5 PROBE_MIN_GLYPHS=2
PROBE_OPENERS="「『（〈《【〔" PROBE_CLOSERS="、。」』）〉》】〕！？" PROBE_THIN_OPENERS="一―"
PROBE_GROW_PITCH=0.5 PROBE_GROW_PITCH_THIN=1.0 JOIN_MIN_CONF=0.6
```

### 4.2 Line crop (`crop_line`, `ppocr.py:874-893`)

```
(width, height) = quad_size(q)
w = max(2, round_half_even(width)); h = max(2, round_half_even(height))
M = getPerspectiveTransform(q, [[0,0],[w,0],[w,h],[0,h]])          # B.6; note w,h not w-1,h-1
crop = warpPerspective(bgr, M, (w,h), INTER_CUBIC, BORDER_REPLICATE) # B.7
if h > w: crop = rotate90_counterclockwise(crop)                      # B.8
```

A vertical column ends up with its top on the left and glyphs lying on their side, which is how the
model was trained.

### 4.3 Recognizer tensor (`ppocr.py:1080-1099`)

```
recognizer_width(cw, ch) = max(16, ceil((48*cw/max(1,ch)) / 8) * 8)    # rounded UP to stride
resized = cv2.resize(crop, (recognizer_width(w,h), 48), INTER_LINEAR)   # uint8 BGR
x = (resized/255.0 - 0.5)/0.5   (float32), CHW  -> [3, 48, W]
```

### 4.4 Windows and batching (`recognize_crops`, `ppocr.py:1600-1643`)

1. Build a tensor per crop and compute `T = W/8`. Pieces come from
   `window_spans(T, window=250, overlap=24)` (`:1252-1268`). If `T <= 274`, the only
   piece is `[(0,T)]`. Otherwise `step = 226`, `count = ceil((T-24)/226)`,
   `step_f = (T-250)/(count-1)`, and the spans are `(r, r+250)` with `r = round_half_even(i*step_f)`.
2. Piece widths are `(e-s)*8`. Batches come from `plan_batches(widths, 16, 0.0)` (`:1102-1130`).
   Order the piece indices by `-width` with a **stable** sort. Add pieces to the current batch;
   close it when it is full (16) or when `1 - (used+w_i)/(batch_w*(n+1)) > 0.0`. With
   waste 0.0, **only equal widths share a batch**.
3. Each batch is a zero-filled `[n,3,48,batch_w]` holding the piece slices
   `tensor[:, :, s*8:e*8]`. Run the model. `probs[k] = out[row, :width/8]`.
4. Per crop, a single piece is used as is. Several pieces go through `stitch_windows` (4.5).
   Then:
   * `decoded = ctc_greedy(full, vocab)` (4.6)
   * `conf = mean(decoded confs)`, or 0.0 if nothing was decoded. This is computed **before**
     gap filling.
   * `chars = fill_gaps(doubt_foreign_glyphs(decoded), tensor)`
   * `text = join(chars)` and `char_confs = [c.conf]`. The result is `(text, conf, char_confs)`.

### 4.5 Window stitching (`ppocr.py:1271-1348`)

* `_class_runs(classes, offset)` collects `(cls, first, last)` for each maximal run of the same
  non-blank class at consecutive timesteps, in full-line time.
* `choose_cut(left, right, lo, hi)`: compute `runs_l`/`runs_r` from the argmax of each and
  `centre = (lo+hi)/2`. For each `cut` in `lo..=hi`, skip it if any run of either window has
  `first < cut <= last`. Otherwise
  `key = (int(before_l==before_r) + int(after_l==after_r), -|cut-centre|)`, where `before` is
  the list of classes of runs with `last < cut` and `after` the list for runs with `first >= cut`.
  Keep the strictly greater key, so the earliest cut wins ties. If there is no candidate, use
  `lo + argmax(min(left[:,0], right[:,0]))`.
* `stitch_windows(windows, spans, guard=3)`: `out = zeros(total, C)` and `start_at = 0`. For
  each window idx with span (s,e), trim the window to `e-s`. If there is a next span `next_s`, set
  `lo = next_s+3` and `hi = e-3`; if `hi <= lo`, use `lo, hi = next_s, e`. Then
  `cut = choose_cut(win[lo-s:hi-s], next[lo-next_s:hi-next_s], lo, hi)`. Otherwise
  `cut = e`. Copy `out[start_at:cut] = win[start_at-s:cut-s]` and set `start_at = cut`.

### 4.6 CTC greedy (`ctc_greedy`, `ppocr.py:1142-1168`)

```
classes = argmax over classes (first max wins), confs = prob at argmax
prev = 0
for t: if cls != 0:
          if cls == prev and out non-empty: if conf > out[-1].conf: out[-1] = (char, conf, t)
          else: out.append((vocab[cls] if cls < len(vocab) else "�", conf, t))
       prev = cls
```

A run's character takes the confidence and timestep of the run's strongest step. The same class
after a blank starts a new character.

### 4.7 Post-decode fixes

* `doubt_foreign_glyphs` (`:1185-1201`): for each interior index `i` (1..n-2), if
  `conf < 0.5`, the char is Latin, and both neighbours are Japanese, replace the char with `〓`
  and keep conf and t. Latin means ASCII alpha, or U+FF21..FF5A that `isalpha()`, i.e.
  full-width A–Z and a–z. Japanese means U+3041–30FF, U+3400–9FFF or U+F900–FAFF
  (`:1171-1182`). This is a single forward pass that reads the already-updated neighbours, but
  `〓` is not Latin, so the result is the same either way.
* `fill_gaps(chars, tensor)` (`:1204-1249`):
  ```
  if n < 2: return
  steps = diff(t); dashes_only = n < 6
  pitch = 48/8 (=6.0) if dashes_only else median(steps)       # numpy median: mean of middle two
  if pitch < 5.0 or no step > 1.5*pitch: return
  grey = mean over channels of tensor (float32) ; ink = |grey - median(grey)| > 0.5
  for consecutive (prev, nxt) with step > 1.5*pitch:
      lo = int(prev.t + pitch/2); hi = int(nxt.t - pitch/2)        # int() truncates
      hole = ink[:, lo*8 : (hi+1)*8]; share = mean(hole) if size else 1.0
      count = min(3, max(1, round_half_even(step/pitch) - 1))
      dash = prev if prev.char in DASHES else nxt if nxt.char in DASHES else None
      fill = (dash.char, dash.conf) if 0.02 <= share < 0.12 and dash
           else None if dashes_only
           else ("　", 1.0) if share < 0.02
           else ("〓", 0.0) if share >= 0.2
           else None
      if fill: for k in 0..count-1: insert (fill, t = prev.t + round_half_even((k+1)*step/(count+1)))
  ```
  Slices clamp as in numpy: a negative `lo` counts from the end. This can happen only if
  `prev.t + 3 < 0`, which is impossible.

### 4.8 Page-level read (`read_page`, `ppocr.py:1649-1657`)

`detect`, then a single `recognize_crops` over `crop_line` of every line (one call, so
batching spans the whole page), then set `text`, `conf` and `char_confs`.

---

## 5. Page-level passes in the `ppocr-manga` reader (`PPOcrPageReader.read_lines`, `engine_runner.py:4687-4730`)

### 5.0 The raw page JSON is the layout's input. Mind the rounding.

`ppocr.page_to_json(lines, w, h, detector=info)` (`ppocr.py:1389-1420`) produces
`{"format":"ppocr-lines/1","width","height","detector"?,"lines":[{"quad":[[round(x,2),
round(y,2)]×4],"score":round(s,4),"text","conf":round(c,4),"vertical","angle":round(a,2),
"char_confs":[round(c,4)…]}]}`. **The layout always runs on this JSON**, i.e. on quads
rounded to 0.01 px and score/conf rounded to 4 decimals (`engine_runner.py:4704,4713,4745`).
The port must round the same way before layout. `conf < 0.5` comparisons see the rounded value.

### 5.1 Sequence

```
lines = engine.read_page(img)                                    # §3 + §4
raw   = page_to_json(lines)
pieces = line_layout.column_pieces(raw)                          # §6.6
if pieces: lines = engine.join_lines(img, lines, pieces); info.joined = Δ; raw = page_to_json(lines)
first = line_layout.layout_page(raw)                             # only to know ruby + bodies
ruby  = {r.line for r in first.ruby}; in_body = ∪ body.members
text_lines = [(i, line) for i, line in enumerate(lines) if i not in ruby]
info.recovered_ends  = engine.recover_clipped_ends(img, [l for _,l in text_lines], thin=[i in in_body …])
info.second_opinions = engine.second_opinions(img, [l for _,l in text_lines])
```

Ruby is excluded from probes and votes because its probes and wider crops would read the
neighbouring column (`ppocr.py:1669-1673`).

### 5.2 `join_lines(img, lines, groups)` (`ppocr.py:1745-1785`)

For each group (list of indices): `quad = union_quad(member quads)` on the **unrounded**
f32 quads. All group quads are read in one `recognize_crops(crop_line(img, quad))` call. For
each group:

* `wanted = Σ max(1, len(text_i.strip())) - shared_seam_glyphs(pieces, quad)`
* skip if `conf < 0.6` or `len(text.strip()) < wanted`
* replace with `Line(quad, mean(member scores), text, conf, confs)` at `min(group)`. The other
  members are removed. The order of the rest is preserved.

`shared_seam_glyphs(pieces, union)` (`:1056-1077`): project each piece's quad onto the union's
`_frame` axis to get `(min, max, text.strip())`. Sort by `(min, max)`. For consecutive pairs,
count the pair if `start_next < end_prev`, both texts are non-empty, and
`before[-1] == after[0]`.

### 5.3 `recover_clipped_ends(img, lines, thin)` (`ppocr.py:1659-1715`)

* `owners` = the lines with `len(text.strip()) >= 2`, each with its thin flag.
* `probe_quads(q)` (`:985-998`) uses `length = max(quad_size)`, `em = thickness`,
  `pad = 1.0*em`, and `inside = 2.5*em`. If `length <= 2*inside`, use one probe,
  `slice_quad(q, -pad, length+pad)`. Otherwise use two: `slice_quad(q, -pad, inside)` and
  `slice_quad(q, length-inside, length+pad)`.
* `slice_quad(q, start, end)` (`:896-917`): for a vertical quad (`h > w`), set
  `axis = unit(((q3-q0)+(q2-q1))/2)` and return
  `[q0+a·s, q1+a·s, q1+a·e, q0+a·e]`. For a horizontal quad, set
  `axis = unit(((q1-q0)+(q2-q3))/2)` and return `[q0+a·s, q0+a·e, q3+a·e, q3+a·s]`.
* All probes of all owners are read in one `recognize_crops` call, in order.
* Single probe: `opener, closer = clipped_marks(text, head.strip())`. If both are empty
  and the line is thin, use `opener = clipped_opener(text, head.strip(), thin=True)`. The tail
  conf is the head conf.
* Two probes: `opener = clipped_opener(text, head.strip(), thin=flag)` and
  `closer = clipped_closer(text, tail.strip())`.
* Rules (`:1001-1053`):
  * `_repeats_start(text, rest) = text.startswith(rest) or (len(rest)>=2 and
    text.startswith(rest[:-1]))`. `_repeats_end` mirrors it with `rest[1:]`.
  * `clipped_opener`: let `marks = OPENERS (+ THIN if thin)`. Return "" if `len(probe)<2` or
    `probe[0]` is not in marks. Return "" if `probe[0]` is in OPENERS and `text[:1]` is in
    OPENERS. Return "" if `probe[0]` is in THIN and `text[:2] == probe[0]*2`. Otherwise return
    `probe[0]` if `_repeats_start(text, probe[1:])`.
  * `clipped_closer`: return "" if `len<2`, if `probe[-1]` is not in CLOSERS, or if `text[-1:]` is
    in CLOSERS. Otherwise return `probe[-1]` if `_repeats_end(text, probe[:-1])`.
  * `clipped_marks`: set `opener = probe[:1]` if it is in OPENERS and `text[:1]` is not;
    `closer` likewise with CLOSERS. Try `(o,c)`, `(o,"")`, `("",c)` in that order and return the
    first non-empty pair with `head+text+tail == probe`. Otherwise return `("","")`.
* On a recovery: `pitch = length/max(1, len(text))`. The head grows by `1.0*pitch` if the
  opener is a THIN mark, else `0.5*pitch`. The tail grows by `0.5*pitch`.
  `quad = order_quad(slice_quad(quad, -head_grow if opener else 0, length + (0.5*pitch if
  closer else 0)))`. Set `text = opener+text+closer` and
  `char_confs = [head_conf]? + confs + [tail_conf]?`.

### 5.4 `second_opinions(img, lines)` (`ppocr.py:1717-1743`)

* `doubted` = lines with `len(text) >= 4`, `len(char_confs) == len(text)`, and
  `min(char_confs) < 0.8`.
* Read `crop_line(img, widen_quad(q, s))` for `s` in (0.06, 0.12), interleaved per line, in
  one call.
* `widen_quad(q, share)` (`:920-930`): for a vertical quad, take
  `side = unit((q1-q0)+(q2-q3)) * width * share` and return `[q0-side, q1+side, q2+side, q3-side]`.
  For a horizontal quad, take `side = unit((q3-q0)+(q2-q1)) * height * share` and return
  `[q0-side, q1-side, q2+side, q3+side]`. The corners are **not** re-ordered.
* `vote_characters(text, confs, others)` (`:950-982`): `_aligned(text, other)` maps the indices
  of `text` to indices of `other` over opcode ranges tagged `equal` and over `replace` ranges with
  equal lengths (difflib on characters, Appendix C). For each `i`, skip if `conf >= 0.8` or the
  char is `〓` or U+3000. The votes are `(other[m[i]], oc[m[i]])` for the others where `i` is in
  the map and `m[i] < len(oc)`. Require `len(votes) == 2` and exactly one distinct name `winner`.
  Compute `support = mean(vote confs)`. Skip if `winner == char`, if `support < conf`, or unless
  both chars are kanji (U+3400–9FFF or U+F900–FAFF). Otherwise replace the char and set its
  conf to `support`.

---

## 6. `line_layout.py`: raw lines → mokuro blocks

Pure Python. Floats are f64. Input is `{"width","height","lines":[{quad,text,score,conf}]}`.
Pipeline (`layout_page`, `:1825-1870`): measure → orientations → furigana → bodies → roles →
merge → paragraphs/order → reading order → build blocks → grow boxes over ruby.

### 6.1 Thresholds (`:84-303`)

```
AMBIGUOUS_ASPECT=1.3 VOTE_ASPECT=1.1 NEIGHBOUR_REACH_EM=3.0
ANGLE_RELIABLE_ASPECT=3.0 SETTLE_MAX_TILT_DEG=12.0 ANGLE_TOLERANCE_DEG=6.0
FURIGANA_MAX_THICKNESS_RATIO=0.75 FURIGANA_MAX_GAP_EM=0.30 FURIGANA_MIN_CENTRE_OFFSET_EM=0.35
FURIGANA_MIN_MAIN_OVERLAP=0.5 FURIGANA_GENEROUS_MAX_THICKNESS_RATIO=1.35
FURIGANA_MAX_GLYPH_PITCH_RATIO=0.85 FURIGANA_LATTICE_MAX_PITCH_FRACTION=0.70
LOW_CONFIDENCE=0.5 FURIGANA_UNREADABLE_MAX_GLYPHS=2 FURIGANA_UNREADABLE_MAX_RATIO=0.6
FURIGANA_TINY_MAX_RATIO=0.45 FURIGANA_LATTICE_MAX_BODY_RATIO=0.65
LONG_LINE_EM=12.0 BODY_MIN_LONG_COLUMNS=3 BODY_MIN_COLUMNS=5 BODY_SIZE_RATIO=1.35
BODY_EXTENT_SLACK_EM=1.0 BAND_GUTTER_COVERAGE=0.15 BODY_EDGE_CLUSTER_EM=0.35
BODY_HANGING_REACH_EM=1.5 BODY_GAP_FACTOR=1.6
MARGIN_BAND_FRACTION=0.15 MARGIN_CLEARANCE_EM=0.25 MARGIN_MAX_LENGTH_FRACTION=0.6
MARGIN_MIN_BODY_COVERAGE=0.5 LONE_GLYPHS_MAX=2 LONE_GLYPHS_MIN_CONF=0.9
MERGE_MAX_SIZE_RATIO=2.5 MERGE_GAP_EM=0.75 MERGE_GAP_MIXED_SIZE_EM=0.50 MERGE_MIXED_SIZE_RATIO=1.8
MERGE_ALIGNED_GAP_EM=1.25 MERGE_ALIGNED_SIZE_RATIO=1.35 MERGE_ALIGNED_START_EM=0.3
MERGE_LOOSE_GAP_EM=1.0 MERGE_LOOSE_START_EM=0.5 MERGE_MIN_MAIN_OVERLAP=0.5 MERGE_STAGGER_EM=2.0
STITCH_MAX_GAP_EM=0.6 STITCH_MIN_CROSS_OVERLAP=0.7 STITCH_MAX_SIZE_RATIO=1.6 BODY_STITCH_MAX_GAP_EM=3.0
BODY_QUAD_MARGIN_EM=0.05
PARAGRAPH_INDENT_MIN_EM=0.6 BRACKET_INK_INSET_EM=0.45 PARAGRAPH_SHORT_END_EM=1.25
PARAGRAPH_SOFT_END_EM=0.3 PARAGRAPH_DEEP_INSET_EM=1.6
ROW_MIN_OVERLAP=0.2 ROW_CHAIN_OVERLAP=0.5 COLUMN_CLUSTER_EM=0.5
OPENING_BRACKETS="「『（(〈《【〔［[｢"
SENTENCE_ENDINGS="。．.！？!?」』）)〉》】〕］]｣…‥―—"
KANA_LOOKALIKES={"夕":"タ","力":"カ","卜":"ト"}
BOUTEN="・･、，,.﹅﹆丶ヽ゛゜'`"
HIRAGANA_LOOKALIKES={"へ":"ヘ","べ":"ベ","ぺ":"ペ","り":"リ","き":"キ"}
SMALL_KANA={"ャ":"ヤ","ュ":"ユ","ョ":"ヨ","ゃ":"や","ゅ":"ゆ","ょ":"よ"}
SMALL_KANA_HOSTS="キシチニヒミリギジヂビピテデフヴきしちにひみりぎじぢびぴてでふ"
LONG_VOWEL="ー" DASH="―"
```

### 6.2 Script predicates (`:311-501`)

* `is_kanji`: U+4E00–9FFF, 3400–4DBF, F900–FAFF, 20000–2FFFF, or one of `々〆〇`.
  These ranges differ from `ppocr._is_kanji` and from `line_reconcile._is_kanji`; keep three
  separate predicates.
* `is_kana`: U+3041–309F, 30A0–30FF, FF66–FF9F. `is_katakana`: 30A1–30FF.
  `is_hiragana`: 3041–3096.
* `glyph_count(t)` = the number of chars that are not `isspace()` (A.2).
* `is_ruby_script(t)`: strip leading `。`/`．` (Python `lstrip("。．")`), then drop whitespace
  chars. Return false if nothing is left, else true iff every char is kana or in BOUTEN.
* `normalize_text(t)` (`:457-483`), one-for-one character rules (length preserved):
  1. `fix_kana_lookalikes` (`:343-361`): for `i < len-1`, if `t[i]` is in KANA_LOOKALIKES and
     `t[i+1]` is katakana and (`i==0` or `t[i-1]` is not kanji), map it. The checks read the
     **original** text.
  2. For each `i`, reading the text after step 1:
     * char in HIRAGANA_LOOKALIKES → `_katakana_for(t, i)` or keep (`:384-411`):
       `prev = t[i-1]` or "", `after = t[i+1:i+3]`, `next_kata = after non-empty and
       is_katakana(after[0])`.
       (a) If prev is katakana and next_kata, map it.
       (b) If ch == き: map to キ iff the two chars before exist, are both katakana, and
       **not** next_kata. Otherwise None.
       (c) If ch is in へべぺ and next_kata: map if not (prev and (prev is hiragana or kanji)).
       Else if `ch != へ`, prev is not kanji, `len(after)==2`, and `after[1]` is katakana, map.
     * char in SMALL_KANA, `i>0`, and `t[i-1]` kana or kanji: if `t[i-1]` is not in
       SMALL_KANA_HOSTS, map to the full-size kana.
     * `—` (U+2014) and `len>1`: if the neighbours `t[i-1:i] + t[i+1:i+2]` are **not all ASCII**,
       map to `―`.
     * ASCII space with `0<i<len-1` and both neighbours non-ASCII: map to U+3000.
  3. `_fix_dashes` (`:414-454`): for each maximal run of `ー` at `[i,j)`, with
     `prev=t[i-1]` or "" and `nxt=t[j]` or "": the run is a dash if prev is empty, prev is not
     kana, or prev is `―`. If prev is hiragana, it is a dash iff `j-i >= 2` and nxt is non-empty
     and (nxt is kana, kanji, whitespace, or in `」』。`). Otherwise (prev katakana) it is not.
     Replace a dash run with `―` × length.

### 6.3 Measuring (`:632-779`)

* `canonical_quad(q)`: return None if there are fewer than 4 points. Compute
  `area2 = Σ x_i y_{i+1} - x_{i+1} y_i` and return None if `|area2| < 1e-6`. If
  `area2 < 0`, set `pts = [p0,p3,p2,p1]`. Then apply the same start-corner rule as
  `order_quad` (max `dx/|e|`, `+1e-6` tolerance, first wins). `order_quad` output passes through
  unchanged.
* `quad_frame(q)`: `mids[i]` = the midpoint of edge i→i+1. `across = mids1 - mids3` and
  `down = mids2 - mids0`. Return `(|across|, |down|, degrees(atan2(across.y, across.x)))`.
* `_measure` (`:701-719`): `text = str(text or "").strip()` (Unicode strip, A.2),
  `score/conf = float(x or 0.0)`, `vertical = height > width`. Return None if the quad is
  degenerate or `w<=0` or `h<=0`.
* `measure_lines`: a line that is None **or has empty text** goes to `dropped` (its index).
  `Line.index` = the position in the raw list.
* Properties: `thickness = width if vertical else height`, `length` is the other,
  `aspect = max/min` (inf if min is 0), `angle_reliable = aspect >= 3.0`,
  `centre = mean of corners`.
* **`spans(theta)`** (`:551-566`): `key = round(theta, 3)` (A.1). Use
  `c = cos(rad(key))`, `s = sin(rad(key))`, `x' = x c + y s`, `y' = -x s + y c`, and return
  `(min x', max x', min y', max y')`. The rotation uses the **rounded** angle.
  `main_cross(theta, vertical) = (y0,y1,x0,x1) if vertical else (x0,x1,y0,y1)`.
* `is_ambiguous(l) = glyph_count(text) <= 1 or aspect < 1.3`.
* `dominant_vertical(lines)`: `area = w*h`. Add the area to V if `h > 1.1w`, to H if `w > 1.1h`.
  Return `V >= H` (ties go to vertical).
* `_box_distance(a,b)` on the axis-aligned `spans()`: `hypot(max(0, max(a0,b0)-min(a1,b1)),
  same for y)`.
* `decide_orientations`: `clear` = the non-ambiguous lines; `page_vertical =
  dominant_vertical(clear)`. For each ambiguous line, `reach = 3.0 * max(w,h)`. Among
  `clear` lines with `dist <= reach`, the smallest `(dist, index)` sets `vertical`. If there is
  none, use `page_vertical`.
* `pair_theta(a,b)`: if both angles are reliable, return None when `|Δangle| > 6`. Otherwise use
  the angle of the line with the larger `max(w,h)` (a on ties). If only one is reliable, use its
  angle. Otherwise 0.0.
* `_overlap(a0,a1,b0,b1) = min(a1,b1) - max(a0,b0)`. A negative value is a gap.

### 6.4 Furigana (`:813-986`)

* `column_lattice(lines)`: `long = [l : length >= 12*thickness]`. Return None if empty.
  `vertical = dominant_vertical(long)`. Keep the long lines with that orientation; return None if
  there are fewer than 3. `theta = median(angle)` and `em = median(thickness)` (statistics.median,
  A.3). `centres` = the sorted values of `(c0+c1)/2` of `main_cross(theta, vertical)`, over all
  lines of that orientation that are not `is_ruby_script(text)` and have
  `max(c1-c0, em)/min(c1-c0, em) <= 1.35`. `steps` = the consecutive differences with
  `0.5em < d < 3em`. Return None if there are fewer than 3. Otherwise return
  `(median(steps), em)`.
* `_ruby_candidacy(line, lattice_em)`:
  1. If `is_ruby_script(text)`, return 0.75.
  2. If there is a lattice and `thickness <= 0.65*lattice_em`, return 0.75.
  3. If `glyph_count > 2`, return None.
  4. If `conf < 0.5`, return 0.75.
  5. Otherwise return 0.6 with a lattice, else 0.45.
* `ruby_of(cand, base, pitch, max_ratio)`: work in the base's frame,
  `theta = base.angle if base.angle_reliable else 0.0`, with `vertical = base.vertical`.
  `bm0,bm1,bc0,bc1 = base.main_cross` and `cm0,cm1,cc0,cc1 = cand.main_cross` (both with
  base's theta and vertical). `base_t = bc1-bc0` and `cand_t = cc1-cc0`; return None if either
  is `<= 0`. `ratio = cand_t/base_t`. `side = +1` if vertical else `-1`.
  `offset = side*(mid(cand_c) - mid(base_c))`. Reject when:
  * `offset < 0.35*base_t`
  * `gap = (cc0-bc1 if vertical else bc0-cc1) >= 0.30*base_t`
  * `_overlap(bm,cm) < 0.5*(cm1-cm0)`

  Then:
  * `thin = ratio <= max_ratio`
  * `readable = is_ruby_script(cand.text)`
  * `generous = readable and ratio <= 1.35`
  * `small_glyphs = generous and glyphs>=2 and base_glyphs>=1 and (cm1-cm0)/glyphs <= 0.85*(bm1-bm0)/base_glyphs`
  * `on_lattice_gap = generous and pitch is not None and offset <= 0.70*pitch`

  Return None unless one of thin, small_glyphs, on_lattice_gap holds. Otherwise return the span
  `(clamp01((cm0-bm0)/len), clamp01((cm1-bm0)/len))` with `len = bm1-bm0`.
* `filter_furigana(lines)`: `lattice = column_lattice(lines)`. `limits[index] =
  _ruby_candidacy`. Run two passes (`final_pass` = False, then True):
  * `bases` = lines that have a kanji (`is_kanji`), are not already ruby, and have
    `limits is None` or `final_pass`.
  * For each candidate line in input order with a non-None limit that is not already ruby:
    take the best base (`!= cand`) by min `(_box_distance, base.index)` among those whose
    `ruby_of(cand, base, pitch, limit)` is not None.
  * Record `Ruby(line=cand.index, base=base.index, text, quad, span, chars)` with
    `n = len(base.text)` and `chars = (min(n, floor(span0*n + 1e-6)), min(n, ceil(span1*n - 1e-6)))`.

  `kept` = the non-ruby lines in order. The ruby list is sorted by line index.

### 6.5 Bodies, roles (`:994-1203`)

* `_text_start(l, theta, v) = main_cross[0] - (0.45*thickness if text[:1] in OPENING_BRACKETS)`.
* `_supported_edge(values, window, lowest)`: sort the values ascending if `lowest`, else
  descending. `need = 2 if n>=2 else 1`. For the first value with at least `need` values within
  `window` (itself included), return the median of that cluster. Otherwise return `ordered[0]`.
* `_full_column_edge(ends, window, reach)`: `lowest = max(ends)` and
  `near = [v : lowest-v <= reach]`. `best` = max over `near` by key
  `(count of u in near with |u-v| <= window, v)`. Return the median of `{u in near : |u-best| <= window}`.
* `_coverage_bands(extents)`: `events = sorted([(m0,+1)…] + [(m1,-1)…])`. Tuples sort by value
  and then by step, so −1 comes before +1 at an equal position. One sweep gives `peak`.
  `floor = max(1.0, 0.15*peak) if peak > 2 else 0.0`. A second sweep opens a band when
  `level > floor` and closes it when `level <= floor`, giving `(start, position)`.
* `find_bodies(kept)`: for `vertical` in (True, False):
  * Take `pool` (that orientation) and `long` (`length >= 12*thickness`); skip if fewer than 3
    long lines. `theta = median(long.angle)`.
  * For each band `(b0,b1)` of `_coverage_bands([main_cross(theta,v)[:2] of long])`:
    `band` = the long lines with `b0 <= (m0+m1)/2 <= b1`; skip if fewer than 3.
    `em = median(band thickness)` and `slack = 1.0*em`.
  * `members` = the pool lines with `max(t,em)/min(t,em) <= 1.35` and
    `b0-slack <= m0 <= b1`. Skip if fewer than 5.
  * `spans` = `main_cross` of the members, `window = 0.35*em`.
    `gaps` = for spans sorted by `c0`, the values `b.c0 - a.c1` of consecutive pairs with
    `0 < gap < 2.5em`.
  * Build `Body(vertical, theta, em,
    top = min(_supported_edge([_text_start(m) for members], window, lowest=True),
              min(_text_start(l) for band)),
    bottom = _full_column_edge([m1 of spans], window, 1.5*em),
    cross0 = min c0, cross1 = max c1, gap = median(gaps) or 0.0,
    members = frozenset(indices))`.

  Sort the bodies by `(top if vertical else cross0, cross0)`.
* `classify_roles(kept, bodies, H, W)`:
  * Start every role as `text`. A line with `conf < 0.5` becomes `noise`.
  * If exactly one line is still `text` and it is a lone doubt (`glyph_count <= 2`, no kanji,
    `conf < 0.9`), it becomes `noise`.
  * Stop here if there are no bodies or `H <= 0`.
  * Use the body extents `(top,bottom)` if vertical, else `(cross0,cross1)`. Take
    `y_top = min` and `y_bottom = max`. Stop if `y_bottom - y_top < 0.5*H`.
  * `em = median(body ems)` and `theta = bodies[0].theta`.
  * For each `text` line not in any body with `length <= 0.6*W`: take `_,_,y0,y1 = spans(theta)`
    and `centre = (y0+y1)/2`.
    * It becomes `header` if `y1 <= y_top - 0.25em` and `centre <= 0.15*H`.
    * Else it becomes `footer` if `y0 >= y_bottom + 0.25em` and `centre >= 0.85*H`.

### 6.6 Column pieces (pre-join, `:1211-1292`)

* `_is_stitch(ta, tb, cross_overlap, main_gap, body_gap)`: `em = max(ta,tb)` and
  `limit = 0.6 if body_gap is None else 3.0`. True iff `em/min <= 1.6` and
  `cross_overlap >= 0.7*min` and `main_gap <= limit*em`.
* `is_column_piece(a, b, body_gap=None)`: `theta = pair_theta`. Return false if the orientations
  differ or theta is None. Use the `main_cross` of both **in a's orientation**, and return false
  if either cross extent is `<= 0`. Then return
  `_is_stitch(ta, tb, overlap(cross), -overlap(main), body_gap)`.
* `column_pieces(raw)`:
  * `lines, dropped = measure_lines`; `decide_orientations`; `kept, _ = filter_furigana`.
  * `blanks` = the `_measure` of dropped indices that are not None (lines with empty text).
  * `body_of` = for each `find_bodies(kept)` member, the first body that lists it.
  * Run union-find over `kept ∪ blanks` with `parent[max(root)] = min(root)`. For `i<j` in
    `kept`, use `body_gap = a.body.gap` when both lines share the same body object (identity),
    else None. Then **for every kept a, for every blank**: set `blank.vertical = a.vertical`
    (a mutation) and test `is_column_piece(a, blank)` without a body gap.
  * Return the groups (sorted by root) with more than one member, each listed in ascending index
    order.

### 6.7 Merging (`should_merge`, `merge_lines`, `:1295-1411`)

`should_merge(a, b, body_gap, body_top)`:

```
if a.vertical != b.vertical: no;  theta = pair_theta; None -> no
am0,am1,ac0,ac1 / bm.. = main_cross(theta, a.vertical);  ta=ac1-ac0, tb=bc1-bc0; <=0 -> no
em=max(ta,tb); ratio=em/min; ratio > 2.5 -> no
gap = -overlap(ac,bc); main_overlap = overlap(am,bm)
if _is_stitch(ta, tb, -gap, -main_overlap, body_gap): yes
if body_gap is not None and body_top is not None:
    main_overlap = overlap(min(am0,body_top), am1, min(bm0,body_top), bm1)
if main_overlap < 0.5*min(am1-am0, bm1-bm0): no
if body_gap is not None: return gap < max(0.75*em, 1.6*body_gap)
start_diff=|am0-bm0|; end_diff=|am1-bm1|
if start_diff > 2em and end_diff > 2em: no
mean_t=(ta+tb)/2; tier1 = 0.50 if ratio > 1.8 else 0.75
if gap < tier1*mean_t: yes
if ratio < 1.35 and gap < 1.25*mean_t and start_diff < 0.3*em: yes
return gap < 1.0*mean_t and start_diff < 0.5*em
```

`merge_lines(kept, bodies, roles)`: union-find over the positions in `kept` with
`parent[max] = min`. For each pair `i<j`, `body_gap` and `body_top` come from a's body when b
is in the same body object, else None. If either role is `noise`, join iff the roles are
`{noise, text}` and `is_column_piece(a, b, body_gap)`. Otherwise join iff the roles are equal
and `should_merge`. Groups are returned sorted by root, with members in kept order.

### 6.8 Paragraphs and line order (`:1419-1498`)

* `block_theta(group)` = the median of the reliable angles, else 0.0.
* `cluster_columns(group, theta=None)`: `flow = -1` if vertical else `+1`. Key each line as
  `(flow*(c0+c1)/2, m0, index)` and sort ascending. Walk the sorted list: append an item to
  the current column if `|key0 - key0 of that column's FIRST item| <= 0.5*item.thickness`,
  otherwise start a new column. Inside each column, sort by `(m0, index)`. The result is right
  to left (vertical) or top to bottom (horizontal), and within a column top to bottom or left to
  right.
* `order_lines` flattens that.
* `split_paragraphs(group, body)`: take the columns `cluster_columns(group, body.theta)`. For
  each column:
  * `inset = (_text_start(col[0]) - body.top)/em`
  * `short = (body.bottom - max m1)/em`
  * `text = concat`

  A column starts a paragraph if:
  * it is the first, or `prev_short >= 1.25`;
  * else, if `inset >= 1.6`, when `|inset - prev_inset| > 0.6`;
  * else, if `inset >= 0.6`, always;
  * else when `text[:1]` is in OPENING_BRACKETS and `prev_short >= 0.3` and `prev_text[-1:]` is
    in SENTENCE_ENDINGS.

### 6.9 Reading order (`order_blocks`, `:1516-1594`)

`boxes[i] = _group_box` gives `(x0,x1,y0,y1)` over the members' axis-aligned `spans()`.

The order is built in four parts:

1. Headers, ordered by `_order_rows`.
2. The flow (kinds `text`/`body`), split into tiers by cuts between stacked **vertical** bodies.
   For consecutive vertical bodies (upper, lower) in body order, if `lower.top > upper.bottom`
   and the shared cross overlap is at least `0.5*narrower`, add `cut = (upper.bottom+lower.top)/2`.
   A block goes to tier `#cuts with centre_y > cut`. Each tier is ordered by `_order_rows`.
3. Footers, ordered by `_order_rows`.
4. Noise, ordered by `_order_rows`.

`_order_rows(indices)`:

* `remaining` = the indices sorted by `(y0, -x1, i)`.
* Loop: the seed is `remaining[0]`. In remaining order, an item joins the row if it is the seed,
  or `_row_overlap(seed_y, item_y) >= 0.2`, or any member already in the row has
  `_row_overlap(member_y, item_y) >= 0.5`. Here
  `_row_overlap(a0,a1,b0,b1) = overlap / max(min(len_a, len_b), 1e-6)`.
* The row is vertical if `Σ glyph_count(line.text)*(+1 if vertical else -1)` over all its
  lines is `>= 0`. Sort a vertical row by `(-x1, y0, i)`, otherwise by `(x0, y0, i)`.
* Append the row to the output and remove its members from `remaining`.

### 6.10 Building a block (`build_block`, `:1639-1675`)

```
theta = block_theta(group)
margin_i = min(0.05*thickness_i, margin_cap)        # cap = body.gap/2 for body paragraphs, else 0
quad_i = _widened(_settled_quad(l_i, theta), l_i.vertical, margin_i)
block = {
  "box": _page_box((min xs, max xs, min ys, max ys) over all quad_i, W, H),
  "vertical": group[0].vertical,
  "font_size": int(round_half_even(median(thickness_i + 2*margin_i))),
  "lines": [normalize_text(l.text) for l in group],
  "lines_coords": [[[int(round_half_even(x)), int(round_half_even(y))] for 4 pts] for quad_i],
}
```

* `_settled_quad(l, theta)` (`:1794-1817`): return the line's own quad if
  `l.angle_reliable` or (aspect `>= 1.3` and `|angle - theta| > 12`). Otherwise rebuild the
  rectangle about its centre with its own `width`, `height` at angle `theta`:
  corners `(-hw,-hh),(hw,-hh),(hw,hh),(-hw,hh)` → `(cx + x c - y s, cy + x s + y c)`.
* `_widened(quad, vertical, m)` (`:1602-1620`): if `m <= 0` or the norm is 0, return the quad
  unchanged. `far = b if vertical else d`, and `u = unit(far - a)*m`.
  Vertical gives `(a-u, b+u, c+u, d-u)`; horizontal gives `(a-u, b-u, c+u, d+u)`.
* `_page_box((x0,x1,y0,y1), W, H)`: `[floor x0, floor y0, ceil x1, ceil y1]`, each clamped to
  `[0, limit]` when `limit = int(W or H) > 0`, else only to `>= 0`.

### 6.11 Box growth over ruby (`grow_boxes_over_ruby`, `:1700-1791`)

Run this on the ordered blocks, before noise is filtered. `owner` maps a line index to its
block. Each ruby run goes to the block owning `run.base`. For each such block `k`:

* `obstacles` = every `lines_coords` quad of every other block whose kind is not `noise`.
* Take the runs sorted by `(_quad_spans(run.quad), run.text)`, where spans are
  `(x0,x1,y0,y1)` of the ruby's canonical quad. For each run:
  * `goal = _page_box(spans(run.quad))`.
  * For `side` in `(1, 3, 0, 2)`, the indices of `box = [x0,y0,x1,y1]`:
    `target = min(box[side], goal[side])` for sides 0 and 1, `max` for 2 and 3, and then
    `box[side] = _clear_reach(box, side, target, obstacles)`. The box is updated in place
    between sides.
* `_clear_reach`: `strip(value)` = side 0 → `(value,y0,x0,y1)`, 1 → `(x0,value,x1,y0)`,
  2 → `(x1,y0,value,y1)`, 3 → `(x0,y1,x1,value)`. Here `clear(v)` = no obstacle meets the
  strip. Return `far` if `far == near` or `clear(far)`. Otherwise bisect while
  `|far-near| > 1` with `mid = (near+far) // 2` (**floor** division), keeping `near` on the clear
  side, and return `near`.
* `_rect_meets_quad(rect, quad)`: false if `x1 <= x0` or `y1 <= y0`. Otherwise run a
  separating-axis test on the axes `(1,0)`, `(0,1)` and the normals `(-ey, ex)` of quad edges
  0→1 and 1→2. The shapes are separated if `max(rect) <= min(quad)` or `max(quad) <= min(rect)`.
  Touching does not count as meeting.

### 6.12 `layout_page` assembly (`:1825-1870`)

For each merged group:

* `role` = the first non-noise role among its members, else `noise`.
* `body = max(bodies, key=|indices ∩ members|)`. The first body wins ties; None if there are no
  bodies.
* If `role == text`, a body exists, and `2*|∩| >= |group|`: each `split_paragraphs` paragraph
  becomes its own block, with kind `body` and cap `body.gap/2`.
* Otherwise: one block of `order_lines(group)`, with kind = role and cap 0.

Then: `order_blocks`, `build_block` in that order, `ordered_groups` (line indices) and kinds,
then `grow_boxes_over_ruby`. The result is `PageLayout{blocks, groups, kinds, ruby, dropped, bodies}`.

`layout_page_dict` (`engine_runner.py:1110-1137`) writes
`{"version": "0.2.5", "img_width": int(width or 0), "img_height": int(height or 0), "blocks":
[blocks whose kind != "noise"]}`. **Noise blocks are dropped from the sidecar.**

---

## 7. Engines: how detections become blocks

### 7.0 Roads

* `LINE_ENGINES = {"ppocr-manga": "ppocr-manga"}` (`engine_runner.py:626`). The recorded
  detector is `ppocr-manga` and `--detector` is ignored (`:6500-6508`).
* `LAYOUT_DETECTORS = {"ppocr-manga"}` (`:671`). Any other engine on this detector runs the
  **reconciled road** in-process via `ReconciledPageReader`. The `detectors/ppocr_manga.py`
  adapter is **not** started by the runner (`:662-671`; `ppocr_manga.py:423-427`).
* `CROP_MODES = {"hayai-nova": "line", "paddle-manga": "upright"}` (`:87-90`).
  `select_crop` (`:680-696`) maps paddle-manga to `quad` on a layout detector.
* With ctd/animetext/rtdetr dropped, **every remaining engine/detector pair runs through
  `_open_ppocr_road`** (`:6932-6985`):

| engine | reader | crop fn | second read |
|---|---|---|---|
| `ppocr-manga` | `PPOcrPageReader` | – | – |
| `hayai-nova` | `ReconciledPageReader` | `make_line_crop_fn()` | none |
| `paddle-manga` | `ReconciledPageReader` | `make_quad_crop_fn(0.25)` | `make_quad_crop_fn(0.5)` |

### 7.1 ppocr-manga (`PPOcrPageReader`, `engine_runner.py:4641-4789`)

`read_lines` (§5.1) is followed by `finish` (`:4732-4754`), which builds
`raw = page_to_json(lines, info)`, then `page, result = layout_page_dict(raw)`, and adds
`raw["ruby"] = [{"line","base","text","chars"}]`. The page goes to the sidecar and `raw` is dumped
to `detect_dir/<rel>.json` (`:6964-6971`).

### 7.2 Reconciled road (`ReconciledPageReader.engine_read`, `:4831-4918`)

```
img, lines, info, first = detected            # after read_lines (§5.1): joined, probed, voted
skip = {ruby line indices of first}; in_body = ∪ first.bodies.members
targets = [i not in skip]
blocks  = [{"lines":[quad_i as float lists], "vertical": line.vertical}]
pitch   = body_pitch(lines)                   # 7.2.1
neighbours = parallel_neighbours(lines)       # 7.2.1
cells[k] = line_cells(*quad_extents(quad, vertical), pitch)      # line_reconcile 7.3.3
texts = _read(img, blocks, cells, crop_fn, all k)                 # one engine batch call
settled[k] = reconcile_line(texts[k], layout.normalize_text(lines[i].text.strip()), cells[k],
              thin = i in in_body, ctc_conf = lines[i].conf, ctc_char_confs = lines[i].char_confs)
if second_crop_fn:                                                # paddle-manga only
    doubted = [k : needs_second_read(settled[k])]
    second = _read(img, blocks, cells, second_crop_fn, doubted)
    settled[k] = settle_disputes(settled[k], second[k], cells[k])
for k, i:
    main, em = quad_extents(quad, vertical)
    if settled.engine_only:
        keep, why = engine_only_verdict(r, cells, det_score=lines[i].score, main, thickness=em,
                                        pitch, neighbours[i]); notes += [why]
        if not keep: r.text = normalize_text(lines[i].text.strip()); notes += ["dropped"]; continue
        lines[i].text = r.text; lines[i].conf = max(lines[i].conf, 0.75); continue
    lines[i].text = r.text
_trim_repeats(lines, targets, settled, img)                       # 7.2.2
```

Then `finish_read` (`:4920-4942`) builds the page and raw dump exactly as in §7.1. It then
merges `Reconciled.to_json()` into `raw["lines"][i]` and sets `raw["reconcile"] =
page_summary(...) + {"body_pitch": round(pitch,1)}`. `doubtful_lines(raw)` (`:5032-5051`)
feeds `review.json` (§8.3). Ruby lines keep their CTC text.

`_read` (`:4948-4979`): build the crops for each `k` in `which`, in order. A line may yield
several crops, and `owners` records the line for each. `caps = token_cap(cells[owner])` is passed
only if the recognizer has `token_caps` (paddle-manga). The texts of a line's crops are
**concatenated**.

#### 7.2.1 Page measures (`engine_runner.py:882-954`)

* `quad_extents(q, vertical)`: midpoints `mid[i]` of edge i→i+1.
  `len_v = |mid2-mid0|` and `len_h = |mid1-mid3|`. Return `(len_v, len_h)` if vertical, else
  `(len_h, len_v)`, i.e. (main, cross).
* `body_pitch(lines)`: the cross extents of lines with `text.strip()` non-empty and
  `conf >= 0.5`, keeping only positive values. If there are fewer than 3, return 0.0; otherwise
  return the `statistics.median`.
* `parallel_neighbours(lines)`: for each line `i`, count the lines `j != i` whose text is
  non-empty, whose `vertical` is the same, with `|angle_j - angle_i| <= 15`, and with
  `hypot(Δcentre) <= 3.0*max(thick_i, thick_j, 1.0)`.

#### 7.2.2 `_trim_repeats` (`:4987-5026`)

`raw = page_to_json(lines, detector={})`. For each `column_pieces(raw)` group, take the axis
`y` if the first member is vertical, else `x`. `spans` = sorted
`(min coord, max coord, i)` over the members. For consecutive `(_, end, before), (start, _,
after)`, both of which must be targets:

* `quad = lines[after].quad` and `em = quad_extents(quad, vertical)[1]`.
* `shared = (end-start) + 2*line_margin_px(quad)`. Skip if `shared <= 0` or `em <= 0`.
* `room = int(shared/em + 0.5) + 1` (truncation).
* `n = overlap_repeat(before.text, after.text, room)`. If `n > 0`, set
  `lines[after].text = after.text.strip()[n:]` and add the note `seam`.

#### 7.2.3 Crops

* **hayai-nova, `make_line_crop_fn`** (`:1202-1270`): `warp_line(img, quad, vertical, 64)`.
  * Take the `mid` points as above. `ratio = |mid2-mid0| / max(1e-6, |mid1-mid3|)`.
  * Vertical: `w = 64` and `h = max(1, round_half_even(64*ratio))`. Horizontal: `h = 64` and
    `w = max(1, round_half_even(64/ratio))`.
  * `dst = [[0,0],[w-1,0],[w-1,h-1],[0,h-1]]`. Note the **−1**, unlike `crop_line`.
  * `warpPerspective` with the default INTER_LINEAR and BORDER_CONSTANT 0.
  * If vertical, rotate 90° CCW to get a strip.
  * `split_long_line(strip, 16 if vertical else 8)`: `ratio = w/max(1,h)`. If
    `ratio <= max`, return one chunk. Otherwise:
    * `n = ceil(ratio/max)`
    * `ink = 255 - BGR2GRAY(strip)` (float32), and `density = Σ ink over rows`
    * `kernel = getGaussianKernel(128, 8.0)` (B.9), and `density = np.convolve(density, kernel, "same")`
    * `cuts = chunk_cut_points(density, w, n, 128)` (`:1140-1161`): for `k` in `1..n-1`,
      `anchor = round_half_even(w*k/n)`, `lo = max(0, anchor-64)`, `hi = min(w, anchor+64)`,
      and the cut is the first argmin of density over `[lo,hi)`, or `anchor` if `hi <= lo`
    * `np.split` at the cuts and drop chunks of width 0.

    Vertical chunks are rotated 90° CW back, so the recognizer sees **upright columns**. Each
    chunk becomes an RGB PIL image.
* **paddle-manga, `make_quad_crop_fn(margin_em)`** (`:1284-1339`):
  * `pad = line_margin_px(quad, em) = min(0.12*max(main,cross), em*min(main,cross))` with
    `quad_extents(quad, True)`.
  * `padded_quad`: `u = unit(p0→p1)` and `v = unit(p0→p3)` (a norm of 0 is treated as 1). Corner
    `k` moves by `pad*(su*u + sv*v)` with signs `(-,-),(+,-),(+,+),(-,+)`.
  * `width = |s1-s0|` and `height = |s3-s0|`. `scale = max(1, 16/max(1, min(width,height)))`.
    `w = max(2, round_half_even(width*scale))`, and likewise for `h`.
  * `dst = [[0,0],[w,0],[w,h],[0,h]]`. `warpPerspective` with INTER_CUBIC and BORDER_REPLICATE.
  * **No rotation**, so a column stays a column. The result is converted to RGB.
* The recognizers are torch VLMs. They are out of the ONNX scope (Q1). Their output contract is:
  * hayai-nova: SigLIP2 NaFlex processor `max_num_patches=512` and a greedy loop of up to 96 new
    tokens. It runs in batches of 16 in crop order. On this road `fold=False`, so the text is
    returned only `.strip()`-ed (`:1591-1618`).
  * paddle-manga: chat prompt `[image, "OCR:"]` with left padding. Batches come from
    `plan_generation_batches`: sort by `(cap, area, i)`, then chunks of 12. Generation is greedy
    with `max_new_tokens = max cap in the batch`. Each row is truncated to its own cap, decoded
    with special tokens skipped, and `.strip()`-ed (`:1881-1920`).

### 7.3 `line_reconcile.py`

#### 7.3.1 Constants and predicates (`:63-219`)

```
OPENERS="「『（〈《【〔"  CLOSERS="」』）〉》】〕。、"  ANCHOR_GLYPHS=2  PATCH_MIN_AGREEMENT=0.8
RUNAWAY_RATIO=1.35 RUNAWAY_SLACK=2 REGION_MIN_PITCHES=2.0 ROOM_MIN_CELLS=4
REPEAT_MAX_UNIT=6 REPEAT_MIN_COUNT=4 LOOP_RATIO=2.0 LOOP_MIN_SHARE=0.8
TOKENS_PER_CELL=1.5 TOKENS_FLOOR=12 TOKENS_EXTRA=8 TOKENS_CEILING=160
SHORT_LINE_GLYPHS=2 SHORT_LINE_MIN_CONF=0.9 CTC_DOUBT_CONF=0.5
DETECTOR_SURE=0.80 DETECTOR_REGION=0.60 DETECTOR_BODY=0.65 BODY_MAX_PITCH_SHARE=0.8
BODY_MIN_NEIGHBOURS=1 CONFIRMED_CONF=0.75 CONFIRMED_MIN_AGREEMENT=0.85
SKIPPED_MIN_CONF=0.94 KANA_STANDS_CONF=0.995
ENGINE_DASHES="-‐–—―"  THIN_GLYPHS="一―"  MISSING_GLYPH="〓"
_DOTS=".…‥"  _BLANKS="　 "  ELLIPSIS="…"
```

* `fold(t)` = NFKC(t) with every whitespace char removed.
* `_is_japanese`: U+3041–30FF, 3400–9FFF, or `々〆〇`. `_is_kanji`: 3400–9FFF or `々〆〇`.
  `_is_kana`: 3041–30FF. `_is_plain_kana` excludes `ー・゠`.
* `_OPENERS_FOLDED = fold(OPENERS)` and `_CLOSERS_FOLDED = fold(CLOSERS)`. NFKC maps `（` to `(`
  etc.
* `_keeps_width(ch)`: true iff `NFKC(ch) != ch` and ch is not in U+FF61–FF9F.

#### 7.3.2 Tokens (`_tokens`, `:241-265`)

For each `i, ch`:

* If `blanks` is set and ch is in `_BLANKS`, emit a token `(ch, i, i+1)`.
* Otherwise `folded = NFKC(ch)` minus whitespace. If folded is non-empty and made only of
  `.…‥`, it is a dot token: if the previous token ends at `i` and its value is `.` or `…`, it
  becomes `(…, prev.start, i+1)`. Otherwise emit `(… if len(folded)>1 else ., i, i+1)`.
* Otherwise emit one token per char of `folded`, each spanning `(i, i+1)`.

Helpers:

* `_values(tokens)` = the token values, excluding those in `_BLANKS`.
* `_core(values)` strips leading `_OPENERS_FOLDED` and trailing `_CLOSERS_FOLDED`.
* `_ratio(a,b)` = 1.0 if both are empty, else SequenceMatcher(a, b).ratio() (Appendix C).
* `_matches(a,b)` = the sum of the sizes of the matching blocks.

#### 7.3.3 Room and runaway (`:303-478`)

* `widen_punctuation(text, keep)`: return the text unchanged unless it has one of `!?.` **and**
  some Japanese char. Then scan runs of `!?.` that are not kept, where a dot run and a `!?` run
  are separate runs. For each run:
  * Keep the run as is if the previous char is ASCII alnum, or (it is a dot run and the next
    char is ASCII alnum), or it is a single `.`.
  * Else, for dots: `‥` if the length is 2, else `…` × `max(1, round_half_even(len/3))`.
  * Else, for a single `!`/`?`: `！`/`？`.
  * Else keep the run.
* `region_cells(main, t, p) = max(1, round(main/p)) * max(1, round(t/p))`, or 0 if any input is
  `<= 0`. `is_region = p>0 and main>0 and t>0 and t >= 2.0*p`.
* `line_cells(main, t, p) = max(1, round(main/t))`, raised to `max(that, region_cells)` when
  `is_region`. It is 0 if `main <= 0` or `t <= 0`.
* `axis_cells(main, t, p) = max(1, round(main/p))` if region, else `max(1, round(main/t))`.
* `token_cap(cells) = min(max(ceil(max(cells,0)*1.5) + 8, 12), 160)`.
* `repeated_tail(text)`: for `unit` in 1..6 (break when `len < unit*4`), take the
  `tail = last unit chars` and count consecutive repeats backwards. If `count >= 4`,
  `best = max(best, unit*count)`. Return `best`.
* `_cells_of(t)` = Σ (0.5 for an ASCII alnum, else 1.0).
* `engine_looped(vlm, cells, axis=0)`: let `t = fold(vlm)` and `r = repeated_tail(t)`. Return
  false if `cells <= 0`, `t` is empty, or `r <= 0`. Return true if `_cells_of(t) > 2*cells + 2`.
  Return false if `r < 0.8*len(t)`. Otherwise use `room = axis if axis>0 else cells` and return
  `_cells_of(t[-r:]) > 2*room + 2`.
* `is_runaway(text, ctc, cells)`: `room = max(_cells_of(fold(ctc)), cells)`. Return false if
  `room <= 0`. Let `L = _cells_of(fold(text))`. Return true if `L > 1.35*room + 2`, else return
  `L > room + 2 and repeated_tail(text) > 0`. Note that `repeated_tail` here is applied to the
  **unfolded** text.

#### 7.3.4 `reconcile_line(vlm, ctc, cells, thin, ctc_conf, ctc_char_confs)` (`:548-723`)

```
vlm_text = vlm without any whitespace chars; ctc_text = ctc.strip()
confs = char_confs if len(confs)==len(ctc_text)==len(ctc) else []
if vlm_text == "": return R(ctc_text, source="ctc", notes=["empty"] if ctc_text else [], agreement=None)
vlm_text, dash_runs = _adopt_dash_runs(vlm_text, ctc_text)
mine = _tokens(vlm_text); theirs = _tokens(ctc_text, blanks=True)
if _values(theirs) == []:
    text = widen_punctuation(vlm_text)
    if engine_looped(vlm_text, cells):
        text = text[:len(text)-repeated_tail(text)] or text[:cells]
        return R(text, agreement=None, notes=["runaway"], engine_only=True)
    return R(text, agreement=None, engine_only=True)
mine_values = values of mine
same_line = max(ratio(dashes_folded(mine_values), core(values(theirs))),
                ratio(dashes_folded(mine_values), values(theirs))) >= 0.8
notes = ["dash"] if dash_runs else []
replace = {}; insert_before = {}; vouched = [False]*len(vlm_text)
opcodes = SequenceMatcher(mine_values, [t.value for t in theirs]).get_opcodes()   # blanks INCLUDED on b
```

For each opcode `k = (tag, i1, i2, j1, j2)` (`:624-693`):

* **equal**: for each aligned token pair `(a, b)`, `printed = _printed_form(...)` (below).
  If it is not None, set `replace[a.start] = (a.end, printed)` and add the note `width`. Mark
  `vouched[a.start:a.end]`. Continue.
* Otherwise, skip if `not same_line` or `j2 == j1`.
* `added` = the values of `theirs[j1:j2]`; `source = ctc_text[theirs[j1].start : theirs[j2-1].end]`;
  `at = mine[i1].start if i1 < len(mine) else len(vlm_text)`.
* **insert at line start** (`i1 == 0`) where every added value is in
  `_OPENERS_FOLDED (+ THIN if thin)`: if `anchored(k+1, forward)`, set
  `insert_before[at] = source` and add the note `opener` (or `thin`). Continue in either case.
* **insert at line end** (`i1 == len(mine)`) where every value is in `_CLOSERS_FOLDED (+ THIN)`:
  if `anchored(k-1, backward)`, set `insert_before[at] = source` and add the note `closer`
  (or `thin`). Continue.
* `stretch = confs[theirs[j1].start : theirs[j2-1].end]`.
* **insert** where `_sure_text(source, stretch, 0.94)`: if the left side is anchored (or
  `i1 == 0`) and the right side is anchored (or `i1 == len(mine)`), append `source` to
  `insert_before[at]` and add the note `skipped`. Continue.
* **replace** where `mine[i1:i2]` are all plain kana, `source` is all plain kana, `stretch` is
  non-empty and `min(stretch) >= 0.995`: set `replace[mine[i1].start] = (mine[i2-1].end, source)`
  and add the note `kana`. Continue.
* If `source.strip(" ")` is empty (blanks only): if it is an **insert** with `0 < i1`, the two
  values around it (`mine_values[i1-1:i1+1]`) are both ASCII alnum, and one side is anchored,
  append `" "` and add the note `space`. Continue.
* Skip if any char of `source` is not in `"　―"`. Skip unless every `i` in `[i1,i2)` is
  `dashlike`, meaning its value is in ENGINE_DASHES, or it is `ー` with `i == 0` or a non-kana
  predecessor.
* `beside_dash` = mine `i1-1` or `i2` exists and is dashlike. `after_mark = i1 > 0` and
  `mine_values[i1-1]` is in `!?`.
* If `i2 > i1`, or (`―` in source and beside_dash), or (`―` not in source and after_mark):
  replace the stretch (or insert if `i2 == i1`) with `source` and add the note `dash` or `space`.

`anchored(k, forward)`: opcode `k` exists and is `equal`, and its length is
`>= min(2, rest)`, where `rest = len(mine) - i1` when forward, else `i2`.

After the opcode loop (`:695-723`):

```
merged = _assemble(vlm_text, replace, insert_before, vouched); notes = dedupe(notes) (+ "disagree" if not same_line)
if is_runaway(merged, ctc_text, cells): return R(ctc_text, agreement=ratio(mine_values, values(theirs)), source="ctc", notes+["runaway"])
their_core = core(values(theirs)); merged_values = values(tokens(merged))
if ctc_conf >= 0.9 and len(their_core) <= 2 and core(merged_values) != their_core and (
       any kanji in their_core
       or (any japanese in their_core and no japanese char in merged)
       or _lacks_end_bracket(merged_values, values(theirs))):
    return R(ctc_text, agreement=ratio(mine_values, values(theirs)), source="ctc", notes+["short"])
agreement = ratio(values(tokens(merged)), values(theirs))
return R(merged, agreement, "merged", notes, engine_only = ctc_conf < 0.5 and not same_line)
```

Helpers:

* `_assemble` (`:813-834`): walk `i` in `0..=len`. First emit `insert_before[i]` chars with
  keep=True. At `len`, stop. Otherwise `(end, new) = replace.get(i, (i+1, vlm[i]))`; emit each
  char of `new` with `keep = vouched[i] or i in replace`, then set `i = end`. The result is
  `widen_punctuation(chars, keep)`.
* `_printed_form` (`:785-810`): `engine = vlm[a]` and `printed = ctc[b]`. Return None if they are
  equal.
  * If `mine.value` is the ellipsis: return `printed` if it is made only of `…‥`. Otherwise
    `cells = count(…‥ in printed) + round_half_even(count(".")/3)`, and return `…`×cells if
    `cells > len(engine.replace("...","…"))`, else None.
  * Otherwise both tokens must be the only token starting at their start position (one char to
    one token on both sides). Return `printed` if `_keeps_width(printed)`, else None.
* `_sure_text(source, confs, floor)` (`:726-741`): the lengths must be equal and the source
  non-empty. `brackets = OPENERS + CLOSERS` without `。、`. Each char must be a bracket or blank,
  or (Japanese or in `。、！？…‥`) with `conf >= floor`. At least one char must be a non-blank
  non-bracket.
* `_lacks_end_bracket(merged, theirs)` (`:744-751`): `closers = _CLOSERS_FOLDED` minus `。、`.
  True if `theirs[0]` is an opener and `merged[:1] != [theirs[0]]`, or if `theirs[-1]` is in
  closers and `merged[-1:] != [theirs[-1]]`.
* `_adopt_dash_runs(vlm, ctc)` (`:754-782`): run the SequenceMatcher over the opcodes **in
  reverse**. Consider only a `replace` with `i2-i1 == 1` whose mine value is `ー` or in
  ENGINE_DASHES. Trim `j2` back while it equals `len(theirs)` and `theirs[j2-1]` is a folded
  closer. Advance `j1` while it equals 0 and `theirs[j1]` is a folded opener. If the remaining
  run has length >= 2 and is all `―`, splice `―`×len into the text at `mine[i1].start:end` and
  count it.
* `_dashes_folded(values)`: map a value in ENGINE_DASHES, or `ー` at position 0 or after a
  non-kana, to `―`.

#### 7.3.5 Second read and verdict (`:837-1163`)

* `needs_second_read(r)`:
  * If `source == ctc`: return `bool(notes) and "short" not in notes`.
  * Else if `engine_only`: return `bool(text)`.
  * Else return `agreement is not None and agreement < 1.0`.
* `settle_disputes(r, second, cells)`:
  * `second_text` = second without whitespace.
  * If `r.source == ctc`: `retry = reconcile_line(second, r.ctc, cells)` with **no** thin/conf
    arguments. If `second_text` is non-empty, `retry.source == merged` and `disagree` is not in
    `retry.notes`, return `retry` with `vlm = r.vlm`, `second = second`, and
    `notes = [r.notes ∩ {empty, runaway}] + retry.notes + ["retry"]`. Otherwise set
    `r.second = second` and return `r`.
  * Otherwise set `r.second = second`. If `second_text` is empty, return `r`. If `engine_only`,
    set `confirmed` if `corroborates(r.vlm, second_text, cells)` and add the note `confirmed`;
    return `r`.
  * Otherwise vote. `witness = dashes_folded(values(tokens(second_text)))` and
    `theirs = tokens(r.ctc.strip(), blanks)`. Using `mine = tokens(r.text)` and the opcodes of
    mine vs theirs, walk `_single_disputes(opcodes)` **in reverse**:
    * `source = ctc_stripped[theirs[j1].start : theirs[j2-1].end]` if `j2 > j1`, else "".
    * Skip if `source` is non-empty and all blank, or if it contains `〓`.
    * `swapped = text[:start] + source + text[end:]`.
    * Accept the swap if `_matches(df(tokens(swapped)), witness) > _matches(df(tokens(text)), witness)`.

    If anything changed, return a new `R(text, agreement = ratio(values(tokens(text)), values(theirs)),
    "merged", notes + ["vote"], second)`.
* `_single_disputes(opcodes)` (`:1103-1126`): for each non-equal `(i1,i2,j1,j2)`, let
  `pairs = min(i2-i1, j2-j1)`.
  * If `i1 == 0` and `pairs > 0`: first the remainder `(i1, i2-pairs, j1, j2-pairs)`, then the
    single pairs `(i2-n, i2-n+1, j2-n, j2-n+1)` for `n = pairs..1`.
  * Otherwise: the pairs `(i1+n, …)` for `n = 0..pairs-1`, then the remainder
    `(i1+pairs, i2, j1+pairs, j2)`.

  Drop empty ranges.
* `corroborates(vlm, second, cells)`: `mine = values(tokens(fold(vlm)))` and likewise `theirs`.
  Return false if either is empty. `shared = _matches`. Return true if `cells >= 4`,
  `shared >= 4` and `shared >= 0.85*min(len)`. Otherwise return `ratio >= 0.85`.
* `engine_only_verdict(r, cells, det_score, main, thickness, pitch, neighbours)` (`:1082-1100`),
  first match wins:
  * `engine_looped(r.vlm, cells, axis_cells(main, thickness, pitch))` gives `(False, "looped")`.
  * `det_score >= 0.80` gives `(True, "backed")`.
  * `det_score >= 0.60` and `is_region` and `len(fold(r.text)) >= 4` gives `(True, "region")`.
  * `det_score >= 0.65`, `pitch > 0`, `neighbours >= 1`, `0 < thickness <= 0.8*pitch` and
    `fold(r.text)` non-empty gives `(True, "body")`.
  * Otherwise `(False, "unbacked")`.
* `overlap_repeat(before, after, max_glyphs)`: with `a = before.strip()` and `b = after.strip()`,
  for `n` from `min(len a, len b, max(0, max_glyphs))` down to 1, return the first `n` with
  `a[-n:] == b[:n]`. Otherwise return 0.
* `Reconciled.to_json` (`:514-524`) and `page_summary` (`:1166-1180`) feed the raw dump only.

### 7.4 What ends up in the sidecar for each engine

On every engine the text is the layout's `normalize_text(line text)`. The difference between
engines is the line text going in:

* ppocr-manga uses the CTC text after join, probe recovery and votes.
* hayai-nova and paddle-manga use the reconciled text for non-ruby lines. A line dropped by
  `engine_only_verdict` keeps its CTC text, which is usually empty and then dropped by
  `measure_lines`. Ruby lines are removed either way.
* The geometry is identical across engines. Quads come from ppocr; reconcile never moves a
  quad. The only conf change is the engine-only floor of 0.75, which can change a line's
  `noise` role.

### 7.5 Dropped detectors: shared helpers worth knowing

These live in `detectors/_common.py` and are used only by animetext/ctd/rtdetr:
`box_iou`, `dedupe_boxes` (NMS with IoU 0.6), `drop_containers`, `block_quad`, and
`estimate_font_size` (`_common.py:302-408`). None of them is on the ppocr road. The adapter
serve protocol (`_common.py:15-40, 218-248`) and `_weights.json` (`:57, 273-290`) are needed
only if the adapter road survives. The ppocr adapter's per-line block shape
(`ppocr_manga.py:504-535`: `box` from floor/ceil clipped to the page, `font_size =
max(8, round(thickness))`, plus `lines`, `angle`, `score`) is not used by the runner.

---

## 8. Output: the `.mokuro` sidecar and the runner's other artefacts

### 8.1 Page dict (`layout_page_dict`, `engine_runner.py:1126-1137`)

The key order matters for a byte-identical port:

```json
{"version": "0.2.5", "img_width": 1925, "img_height": 2800,
 "blocks": [{"box": [x0, y0, x1, y1], "vertical": true, "font_size": 61,
             "lines": ["…"], "lines_coords": [[[x, y], [x, y], [x, y], [x, y]]]}]}
```

* `version` = `MOKURO_FORMAT_VERSION = "0.2.5"` (`:80`).
* `img_width`/`img_height` are the decoded image size.
* `box` holds ints: the axis-aligned bounds of the widened and settled quads, clamped, then grown
  over ruby (§6.10–6.11).
* `vertical` is a bool. `font_size` is an int: the median of (thickness + 2·margin), half-even.
* `lines` are strings with `normalize_text` applied, never with leading or trailing whitespace.
* `lines_coords` holds int points: one quad per line in TL,TR,BR,BL reading-frame order,
  half-even rounded.
* Noise blocks are excluded. A block may have lines of different lengths; the reader lays the
  characters on a uniform grid along each quad.

### 8.2 Volume dict (`build_volume` `:1077-1107`, `Session._assemble` `:7897-7951`)

```json
{"version": "0.2.5", "title": T, "title_uuid": U1, "volume": V, "volume_uuid": U2,
 "ocr_engine": {"id": E, "recognizer": REPO, "detector": "ppocr-manga",
                "generator": "mokuro-bunko 0.5.2", "patch_budget": 512,
                "weights": {"repo": "sha", …}, "precision": "fp16"},
 "pages": [{…page dict…, "img_path": "rel/posix/path.webp"}]}
```

* `title`/`volume` = the request values, else `name`. `name` is `request.stem`, else the
  archive stem, else the input dir name. `title_uuid`/`volume_uuid` = the request values
  (`--volume-uuid` from the server) else `uuid4()`.
* `ocr_engine` (composed engines always write it):
  * `id` = the engine, and `recognizer = RECOGNIZER_REPOS[engine]` (`:92-96`):
    ppocr-manga → `Kellenok/PP-OCRv6_manga`, hayai-nova →
    `JustANormalTinkerer/hayai-ocr-v2.5-nova`, paddle-manga →
    `sorryhyun/paddleocr-vl-1.6-manga-lora`.
  * `detector` = `ppocr-manga` for all three.
  * `generator` = `--generator` (the server passes `"mokuro-bunko <version>"`,
    `processor.py:1619-1620`), else `"mokuro-bunko"`.
  * `patch_budget` appears only for hayai-nova (`PATCH_BUDGET_ENGINES`, `:279`).
  * `weights` appears only if non-empty (`OpenPipeline.weights`, `:7228-7251`). It starts from the
    reader's `repos` and merges the recognizer's: `{"Kellenok/PP-OCRv6_manga": "ba1d479e…"}`
    (omitted when the models were not pinned, §1.2), then hayai-nova adds
    `JustANormalTinkerer/hayai-ocr-v2.5-nova: e46d79138499600564f810d44ab6bdea7230dee1` and
    `google/siglip2-base-patch16-naflex: b53b807d3a2d5e2b3911292f2d69e5341cdc064c`, while
    paddle-manga adds `PaddlePaddle/PaddleOCR-VL-1.6: c5630abae1d940eafe0697512a0325494b02ab42`
    and `sorryhyun/paddleocr-vl-1.6-manga-lora: 26292839d1469c14212a12a1e01b5b1fe01bff15`
    (`REPO_REVISIONS`, `:124-129`). The dict keeps insertion order.
  * `precision` appears only when a recognizer loaded (`OpenPipeline.precision`,
    `:7179-7189`). It is **absent for ppocr-manga**.
* Each `pages[i]` is the page dict plus `img_path` = the relative path in POSIX form (`:7779`,
  `:1105`). Pages are in input order (natsort).
* Serialization: `json.dump(obj, f, ensure_ascii=False, default=json_default)` with the
  default separators `", "` / `": "` and no indent (`dump_json`, `:832-835`). The runner writes
  `<output>.tmp` and then `rename` (`:7952-7954`). The sidecar holds no floats (all ints, bools
  and strings), so float formatting matters only for the raw dumps.

### 8.3 Runner artefacts beside the sidecar (not part of the sidecar)

* `cache_dir/<rel with suffix .json>` holds each page dict when it leaves the pipeline
  (`:7778`). **The server counts these files for progress** (`processor.py:986-991`). Note that
  `with_suffix` makes `001.jpg` and `001.png` collide.
* `detect_dir/<rel>.json` holds the raw dump (§7.1/7.2).
* `detect_dir/review.json` = `{"format":"ocr-review/1","engine","detector","pages":[{"page":
  rel,"lines":[doubtful…]}]}` on the reconciled road only (`:7955-7966`).
* `pipeline.json` (stats), and log lines the server parses: `[runner] wrote … pages=N
  failed_pages=F …` and `Processed successfully: 1/1` (`:7967-7972`;
  `provenance.py:43-44, 93-112`).

### 8.4 Failed pages (`Session._page`, `:7729-7792`)

An exception in any stage gives a **blank page** `{"version","img_width","img_height",
"blocks":[]}`. Its size comes from the image header via Pillow (`_blank_from`, `:771-784`). If
even the header cannot be read, the page is **omitted entirely** from `pages`. A volume with no
pages at all fails and no sidecar is written (`:7903-7906`).

### 8.5 Server-side normalization of every installed sidecar (`processor.py:899-984`)

After the runner, the server rewrites the file:

* `title` = the series name: the parent folder name stripped, or the cbz stem when the parent is
  the library root or the inbox (`processor.py:786-792`).
* `volume` = the cbz stem.
* `title_uuid = uuid5(NAMESPACE_DNS, series_name)`.
* `volume_uuid = volume_uuid_for(cbz, generation)` (`:857-897`). The order is: the primary
  sidecar's id (for non-primary rows), the remembered DB id, a layer's id (the oldest mtime), and
  finally `deterministic_uuid("<Series>/<Volume>")`. That last one is a djb2-xor pair over UTF-16
  units of the lower-cased, stripped string, in 8-4-4-4-8 shape (client quirk)
  (`metadata/reader_compat.py:91-115`).
* Non-primary rows only: `ocr_engine` is `setdefault`-ed with `id` and
  `generator = "mokuro-bunko <ver>"`, and `generation = <row name>` is set (appended last)
  (`processor.py:962-984`).
* The file is rewritten **compact** with `json.dump(ensure_ascii=False, separators=(",", ":"))`.
  Keys that already exist keep their positions.
* Sidecar names are `<Volume>.mokuro` for the primary row and `<Volume>.<row>.mokuro` otherwise
  (`generations.py:283-287`). `.gz` is only ever written by the mokuro CLI.

---

## 9. Cover thumbnail `<Volume>.webp` (server side, `processor.py:591-656`)

* It is needed iff the target is a `.cbz` file and neither `<Volume>.webp` nor `<Volume>.nocover`
  exists.
* The cover is the **first name in plain `sorted()` of `zip.namelist()`**, which is code-point
  order on the full member path and **not** natural sort. Candidates are names whose lower-cased
  suffix is in `{.jpg,.jpeg,.png,.gif,.bmp,.webp,.tiff,.tif}`. There is no `.avif`, no
  `__MACOSX` exclusion, and **no exclusion of the embedded `<stem>.webp`**, so this rule differs
  from the page rule in §2.2.
* If there is no image, or a BadZipFile/OSError/KeyError occurs, touch `<Volume>.nocover` and
  return false.
* `Image.open(bytes).convert("RGB")` (first frame, alpha dropped, no EXIF transpose), then
  `ImageOps.contain(img, (250, 350), LANCZOS)`:
  * Compute `im_ratio = w/h` and `dest = 250/350`.
  * If they are not equal and `im_ratio > dest`: `new_h = round_half_even(h/w*250)` and
    `size = (250, new_h)`.
  * Otherwise, if not equal: `new_w = round_half_even(w/h*350)` and `size = (new_w, 350)`.
  * Then `resize(size, LANCZOS)`. **This upscales** images smaller than the box.
* Save as WebP with `quality=85, method=6`, lossy, without ICC, EXIF or XMP. Pillow passes
  metadata only from save kwargs (verified, `PIL/WebPImagePlugin._save`).
* A failure is logged and returns false. No marker is written in that case.

---

## Appendix A: Python semantics a port must reproduce

1. **`round(x, n)`** (JSON rounding at `ppocr.py:1406-1414`, and `Line.spans` at
   `line_layout.py:558`) is correctly rounded to `n` decimals on the exact binary value, with
   ties to even. For example, `round(2.675, 2) == 2.67` because the binary value is below
   2.675. Implement it by exact decimal formatting with ties-to-even, then parse back to f64.
   Do **not** use `(x*100).round()/100`. `int(round(x))` → `round_ties_even`.
2. **`str.isspace()` / `str.strip()`**: whitespace means Unicode White_Space **plus**
   U+001C..U+001F. Rust's `char::is_whitespace` lacks 1C–1F. U+3000 counts as whitespace, so
   `strip()` removes leading and trailing ideographic spaces. `strip(" ")` strips ASCII space only.
3. **`statistics.median`** = the mean of the two middle values for even n. `numpy.median`
   behaves the same. `ppocr.dense_median` uses the **upper** median `sorted[n//2]`.
4. **`max(..., key=)` / `min(...)`** return the **first** extreme on ties. `sorted` is stable.
5. **Floor division** `//` on ints rounds toward −∞. `int(x)` on floats truncates toward 0.
6. **`numpy.argmax`** returns the first max.
7. **`unicodedata.normalize("NFKC")`**: the engines env runs Python 3.12.12, i.e. Unicode
   **15.0.0** (checked locally). The Rust `unicode-normalization` crate tracks a newer Unicode,
   which may matter for new compatibility characters (Q7).
8. **float32 vs f64**: all `ppocr.py` geometry is float32 numpy, including `quad*scale + origin`,
   `order_quad`, `union_quad`, and probe and widen math. `line_layout` works in f64 on values
   already rounded to 0.01. For bit-equal raw quads, do the ppocr geometry in f32 as numpy does
   (Q3).
9. **JSON**: `ensure_ascii=False`. Python escapes `"`, `\`, `\n`, `\r`, `\t`, `\b`, `\f`, and
   other C0 chars as `\u00XX` (lowercase hex). It escapes neither `/` nor U+2028/2029. Float repr
   is the shortest round-trip, with exponent form when `exp < -4` or `>= 16`, e.g. `1e-05`
   (serde_json writes `1e-5`). This affects only the raw dumps.

## Appendix B: OpenCV primitives used, and their exact semantics

**B.1 `cv2.resize(src u8, (W,H), INTER_LINEAR)`** is used for the detector input and the
recognizer input. It is **not** float bilinear: a float bilinear with half-pixel centres differs
by ±1 on ~8–13 % of pixels (measured). The fixed-point algorithm below reproduces cv2 5.0
**exactly for downscaling** (0 mismatches over 5 sizes). For upscaling it differs by −1 only on
**clamped border rows** (`β = (2048, 0)`), ~0.1 % of pixels (Q2):

```
scale = 1.0 / (dst_n / src_n)                       # double, as OpenCV computes it
for d: f = float32((d+0.5)*scale - 0.5); s = floor(f); f = float32(f - s)
       if s < 0: f = 0, s = 0;  if s >= src_n-1: f = 0, s = src_n-1
       a0 = cvRound(float32(1-f)*2048); a1 = cvRound(f*2048)        # half-even
horizontal: H[y][d] = S[y][s]*a0 + S[y][min(s+1,n-1)]*a1             # int
vertical:   out = (((b0*(H[r0]>>4))>>16) + ((b1*(H[r1]>>4))>>16) + 2) >> 2, clamp 0..255
```

Also, for an exact 2× downscale on both axes, OpenCV switches INTER_LINEAR to INTER_AREA
(`is_area_fast`). This could hit the detector input. Test it.

**B.2 `findContours(bitmap, RETR_LIST, CHAIN_APPROX_SIMPLE)`** uses Suzuki–Abe border following
with 8-connected foreground. RETR_LIST returns **outer borders and hole borders** with no
hierarchy. Points are the border pixels' integer coordinates. SIMPLE keeps only the endpoints of
horizontal, vertical and diagonal runs. Foreground that touches the image edge is traced
correctly (verified: a block at the origin gives `[[0,0],[0,3],[4,3],[4,0]]`). Contour order
matters only for the 3000 cap and tile union order.

**B.3 `minAreaRect(points)`** is the convex hull plus rotating calipers on int points. It returns
float32 `((cx,cy),(w,h),angle)`. The representation is irrelevant after `order_quad`. Ties
between equal-area rectangles are resolved by the first one found. Expect ~1e-5 px differences
versus an f64 implementation. These disappear after 0.01-px rounding except at boundaries.

**B.4 `fillPoly(mask, [pts], 1)`** with the default LINE_8 and shift 0 does an even-odd scanline
fill **plus** the polygon edges drawn as 8-connected lines (OpenCV `drawing.cpp`
`CollectPolyEdges` + `FillEdgeCollection`). Verified: for **outer** contours the mask equals
*the 8-connected component with its 4-connected holes filled* (1,691/1,691 random blobs). For
**hole** contours that description fails (it matched only 239/261), so port the scanline
algorithm itself (Q5).

**B.5 `contourArea(quad)`** is the absolute shoelace area. **`intersectConvexConvex(a,b)`**
returns the area of the intersection of two convex polygons; Sutherland–Hodgman clipping plus
shoelace gives the same area to fp precision.

**B.6 `getPerspectiveTransform(src4, dst4)`** solves the standard 8×8 system (`DECOMP_LU`, f64)
for `h33 = 1`.

**B.7 `warpPerspective(src, M, (w,h), flags, border)`** inverts M, and for each dst pixel maps
`(X,Y,W) = Minv·(x,y,1)`. In the classic implementation, coordinates are quantized to **1/32 px**
(`INTER_TAB_SIZE`) and u8 interpolation uses fixed-point tables (`INTER_REMAP_COEF_BITS=15`).
Bicubic uses `A = -0.75`. The `ppocr`/paddle crops use INTER_CUBIC + BORDER_REPLICATE; hayai uses
INTER_LINEAR + BORDER_CONSTANT(0). Bit-exactness against cv2 5.0 is unverified (Q2).

**B.8 `rotate`**: CCW gives `out[r][c] = in[c][W-1-r]` (size W×H → H×W). CW gives
`out[r][c] = in[H-1-c][r]`.

**B.9 `getGaussianKernel(128, 8.0)`** (f64): `g_i = exp(-(i-63.5)²/(2·64))`, normalized to sum 1.
`np.convolve(a, k, "same")` returns `max(len)` samples centred, with the `(len(k)-1)//2` offset
convention of numpy. `cvtColor(BGR2GRAY)` is fixed-point `(4899 R + 9617 G + 1868 B + 8192) >> 14`.

## Appendix C: `difflib.SequenceMatcher(None, a, b, autojunk=False)`, exactly

Used by `ppocr._aligned`, `line_reconcile` (`_ratio`, `_matches`, opcodes). Elements are
compared with `==`.

```
b2j[x] = ascending list of indices j with b[j]==x          # no junk, no popularity pruning
find_longest_match(alo, ahi, blo, bhi):
    besti, bestj, bestsize = alo, blo, 0; j2len = {}
    for i in alo..ahi-1:
        newj2len = {}
        for j in b2j.get(a[i], []):
            if j < blo: continue
            if j >= bhi: break
            k = newj2len[j] = j2len.get(j-1, 0) + 1
            if k > bestsize: besti, bestj, bestsize = i-k+1, j-k+1, k
        j2len = newj2len
    return (besti, bestj, bestsize)      # junk-extension loops are no-ops without junk
get_matching_blocks():
    queue = [(0, la, 0, lb)]; blocks = []
    while queue: alo,ahi,blo,bhi = queue.pop()                 # LIFO
        i,j,k = find_longest_match(...)
        if k: blocks.append((i,j,k))
              if alo < i and blo < j: queue.append((alo,i,blo,j))
              if i+k < ahi and j+k < bhi: queue.append((i+k,ahi,j+k,bhi))
    blocks.sort(); merge adjacent (i1+k1==i2 and j1+k1==j2); append (la, lb, 0)
get_opcodes(): i=j=0; for (ai,bj,size) in blocks:
    tag = replace if i<ai and j<bj else delete if i<ai else insert if j<bj else none
    if tag: emit (tag,i,ai,j,bj);  i,j = ai+size, bj+size;  if size: emit (equal,ai,i,bj,j)
ratio() = 2*Σsize / (la+lb)   (1.0 if la+lb == 0)
```

---

## Open questions

1. **Scope of the VLM engines.** hayai-nova and paddle-manga are torch VLMs with
   `trust_remote_code`. Is the Rust port ppocr-manga only, with these engines staying in Python
   (keeping `line_reconcile` there), or must the VLMs become ONNX too? §7.2/7.3 is written so
   that either split works.
2. **Pixel-exact resampling.** The B.1 emulation is exact for downscaling but differs by −1 on
   clamped border rows when upscaling. `warpPerspective` with INTER_CUBIC/LINEAR (B.7) is
   unverified, and so is the 2× INTER_AREA switch. Decide whether bit-exactness against cv2 is a
   goal or whether "same lines on a bench volume" is the acceptance bar. Then port
   `resize.cpp`/`imgwarp.cpp`, or accept the drift. Recommend golden dumps of tensors per page.
3. **Float32 fidelity.** ppocr geometry is float32 numpy, and onnxruntime results depend on
   ORT version and CPU SIMD (MLAS). What tolerance does the acceptance test allow on raw quads
   and per-char confs? (Thresholds such as `conf < 0.5` and `score >= 0.80` are on rounded values.)
4. **Image decoding parity.** Pillow 12.3 (libjpeg-turbo ISLOW + fancy upsampling, libwebp,
   AVIF plugin) versus Rust decoders (`zune-jpeg`, `image`, `libwebp-sys`, `libavif`). Non-identical
   JPEG output changes everything downstream. Link libjpeg-turbo?
5. **Hole contours in DB post-processing.** RETR_LIST emits hole borders, and their `fillPoly`
   mask has no simple closed form (239/261). Port OpenCV's scanline fill verbatim, or ignore hole
   contours? Ignoring them changes output only when a hole border scores ≥ 0.25.
6. **Zip name decoding.** Python `zipfile` ignores the Info-ZIP Unicode-path extra field (0x7075)
   and uses cp437 without flag 11. The Rust `zip` crate may honor 0x7075, which would give
   different `img_path`s. Also: are encrypted, zstd or deflate64 members in scope?
7. **Unicode version.** NFKC tables differ between Python 3.12 (Unicode 15.0) and the Rust crate
   (16.x). Pin the crate or accept the drift? The production engines-env Python version should be
   confirmed; it was checked only locally.
8. **natsort fallback.** Production always has natsort, but `_natural_key`
   (`engine_runner.py:765-768`) lower-cases and would order differently. The port should
   implement natsort's rule (§2.3) only. Confirm there is no case where the fallback ran.
9. **Thumbnail cover rule vs page rule.** The cover is chosen by plain `sorted()` with the
   embedded `<stem>.webp` **included**, while the pages use natsort with it excluded. Keep this
   quirk for parity, or fix it in the port?
10. **Runner JSON separators.** The runner writes `", "`/`": "`, and the server rewrites the file
    compact. If Rust replaces only the runner, is byte-parity with the runner's intermediate file
    required, or only with the final normalized file?
11. **Determinism of the raw dumps.** `detector.passes`, `joined` and similar fields, plus the
    `vlm`/`ctc` notes, are debug output. Must the Rust port reproduce `detect_dir/<rel>.json` and
    `review.json` byte-for-byte, or only the sidecar and the `_ocr` progress files?

# Text detector adapters

Each file here is a standalone script run by the OCR engines environment's
Python in its own process. The recognizer (see `../engine_runner.py`) never
imports a detector; it launches the adapter as a subprocess and reads the JSON
it writes. That process boundary is deliberate: `ctd.py` combines with the
GPL-3.0 comic-text-detector code shipped inside the `mokuro` package, and the
boundary keeps that license from reaching the MPL-2.0 server or the runner.

## Contract

```
python <adapter>.py --input <volume dir> --pages <list file> --output-dir <dir>
python <adapter>.py --serve
```

Two ways to run, one code path behind both (`_common.run_adapter`): an adapter
loads its models once in `setup()` and hands back a per-page callable, and the
scaffold either walks the `--pages` list (**batch**) or serves pages off stdin
(**serve**). What is written cannot differ between them.

`--pages` is a text file with one page path per line, relative to `--input`,
in reading order. For every page the adapter writes `<output-dir>/<page>.json`
(same relative path, `.json` suffix):

```json
{
  "img_width": 1715,
  "img_height": 2800,
  "blocks": [
    {
      "box": [x1, y1, x2, y2],
      "vertical": true,
      "font_size": 46,
      "lines": [[[x, y], [x, y], [x, y], [x, y]], ...]
    }
  ]
}
```

- `lines` are quads in page pixels, one per text line, in reading order
  (right to left for vertical text). Block-level detectors emit a single
  line covering the block.
- A quad's four points are the corners of the line in its own upright frame:
  top-left, top-right, bottom-right, bottom-left. A detector that finds
  rotated text (`ppocr_manga`) emits the ROTATED rectangle, never the
  axis-aligned box around it; `box` is always the axis-aligned bounds.
- Optional per-block keys, ignored by readers that do not know them: `angle`
  (tilt of the line in degrees, positive clockwise on screen) and `score`
  (detector confidence).
- `font_size` is the approximate glyph size in pixels (line width for
  vertical text, line height for horizontal).
- Adapters print progress lines to stdout and must exit non-zero on failure.

### Serve mode

`--serve` is what the runner uses, so that detection is a streaming stage of
the page pipeline rather than a pass over the whole volume that finishes
before the recognizer has been built. The adapter loads its models, then:

```
stdout  @@detect ready <device>\t<weights as JSON>
stdin   <image path>\t<JSON path to write>        (both absolute)
stdout  @@detect ok <note>\t<image path>          (the JSON is written first)
        @@detect fail <message>\t<image path>     (that page failed; the process lives)
```

one request at a time until **EOF on stdin**, which ends the loop and the
process — and is also how a parent that died takes its detector with it,
without a signal or a pid file.

**A served process knows nothing about a volume.** It is given the image to
read and the file to write, one request at a time, and reports what it loaded
on the ready line rather than into a run's directory — so the same process can
serve the pages of one volume and then the next, for as long as there is work
for its model. `--input`, `--pages` and `--output-dir` belong to batch mode,
where a volume is exactly what the adapter is given; in serve mode they are
not passed at all and `_weights.json` is written by the runner instead.

The path comes **last, after a tab**, because page names contain spaces; a
reply the runner cannot split back into note and page is a desync it kills the
process over. Anything else the process prints is not a reply and is forwarded
to the volume's log, and a reply is found by its prefix anywhere in the line,
so a library writing a partial line to stderr cannot swallow one.

Adapters implement `setup(args, out) -> detect` (`_common.run_adapter` drives
both modes): `setup` loads the models once and calls `out.weights({...})`;
`detect(image_path)` returns `(page JSON, progress note)` and is given the
image, nothing else.

The adapter also reports the model versions it loaded, once, after they are
resolved and before any page is read — in batch mode as
`<output-dir>/_weights.json`, in serve mode on the ready line:

```json
{ "deepghs/AnimeText_yolo": "a180c191bfdb9f0e31b57e7de567e7b6bac50f84" }
```

Each key is a model source — a Hugging Face repo id, or a release asset URL
for weights that are not on the Hub — mapped to the revision or `sha256:…`
digest it was **pinned** to. The runner merges this into the sidecar's
`ocr_engine.weights`, so an OCR file names what boxed its text as well as what
read it. Report it with `out.weights({...})`, which puts it wherever this run
can receive it; an adapter that reports nothing claims nothing, and the
sidecar stays silent rather than guessing.

Every adapter pins the weights it loads, because a detector resolved at a
moving `main` can change under a self-hoster between runs. The pins live in
the adapter (the runner's `REPO_REVISIONS` is out of reach from another
process): `REVISION` in `animetext.py`, `WEIGHTS_SHA256` in
`ctd.py` (a GitHub release asset, and a `.pt` is a pickle, so the digest is
verified before torch loads it), `ppocr.REPO_REVISION` for `ppocr_manga.py`.

## Adapters

| id | script | license | geometry |
|---|---|---|---|
| `ctd` | `ctd.py` | GPL-3.0 | blocks + lines (identical to mokuro) |
| `animetext` | `animetext.py` | GPL-3.0 | blocks (YOLO12-x, onnxruntime); **disabled for now** (`engines.DISABLED_DETECTORS`): offered nowhere, refused in a row |
| `ppocr-manga` | `ppocr_manga.py` | Apache-2.0 | rotated lines, ruby as its own lines, one block per line; needs `../ppocr.py` staged next to the runner |

## `ppocr-manga`: adapter and engine

The same model pair is reachable two ways:

- **`--engine ppocr-manga`** does not use this adapter. Detector and
  recognizer run inside the runner process (`engine_runner.LINE_ENGINES`):
  both are Apache-2.0, so there is no licence to contain, the page is decoded
  once instead of twice, and the layout step gets the recognizer's
  confidences, which this contract does not carry.
- **`--detector ppocr-manga`** with another recognizer (`hayai-nova`,
  `paddle-manga`) does not use it either (`engine_runner.LAYOUT_DETECTORS`).
  The runner reads the page the engine's way first -- detection, the CTC
  read, column pieces joined, clipped brackets recovered, ruby told from text
  -- then hands the configured recognizer a deskewed crop of every line that
  is not ruby and merges the two reads per line (`../line_reconcile.py`). Run
  through the adapter the recognizer would get bare quads: nothing to
  reconcile with, a third of a novel page's crops spent on ruby, and a column
  the detector boxed as two overlapping quads read twice (measured: a
  duplicated five-glyph run on a bench page).

The adapter itself stays a faithful implementation of the contract -- one
block per line, ruby included -- for anything else that wants this detector's
geometry as JSON.

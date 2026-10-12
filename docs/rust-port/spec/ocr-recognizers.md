# Spec: OCR runner protocol, page pipeline, and the hayai-nova / paddle-manga recognizers

**Baseline:** Python 0.5.2 (`release/0.5.2`, 199cff5), file
`src/mokuro_bunko/ocr/engine_runner.py` (9,684 lines; cited below as `ER:<line>`).
**ONNX spike:** `~/.cache/mokuro-bunko-demo/onnx-spike/{hayai,paddle,runtime}` (2026-09-29, throwaway;
cited as `spike/hayai/<file>:<line>` and `spike/paddle/<file>:<line>`).
**Target:** Rust + ONNX Runtime (`ort` crate), see `../ARCHITECTURE.md` §3, §6, §7.

Legend: **KEEP** = port as specified; **DROP** = not ported (licence or Python-only machinery);
**CHANGE** = port the meaning, not the mechanism.

Facts marked **[verified 2026-10-01]** were re-measured while writing this spec (scripts in the
writer's scratchpad, reproduced in §4/§3.4 so they can be re-run). Everything else is cited.

---

## 0. Scope and what is dropped

0.5.2 has four page "roads" (`page_road`, `ER:2880-2892`):

| road | engine / detector | 0.7 |
|---|---|---|
| `line` | `ppocr-manga` alone (PP-OCR det + CTC rec + `line_layout`) | **KEEP** (det/rec/layout are separate specs) |
| `reconciled` | `hayai-nova` or `paddle-manga` reading the lines of the `ppocr-manga` detector, merged with the CTC read by `line_reconcile` | **KEEP** — the only road the two recognizers in this spec run on in 0.7 |
| `adapter` | any recognizer behind a detector-adapter subprocess (`ctd`, `animetext`) | **DROP** (both detectors are GPL/removed; `DetectorProcess`, `DetectorPool`, `@@detect` reply protocol `ER:5446-6000`, `load_detection` `ER:957`, `ocr_page*` `ER:983-1074`, `make_upright_crop_fn` `ER:1273`, `BLOCK_LEVEL_DETECTORS` `ER:677`, `_weights.json` contract `ER:653`) |
| `served` | `mokuro` (manga-ocr) in its own serve process | **DROP** (`ServedEngine` `ER:6007-6400`, `ServedPrecisionTarget` `ER:6403`, `mokuro_placement` `ER:2686-2727`, `--mokuro-python`, `SERVED_FP16_FLAG`) |

Also **DROP**:
- `EngineProcess` / `_engine_process_main` / multiprocessing `spawn` copies (`ER:5312-5443`) and the
  process half of `RecognizerPool` (`ER:5182-5302`). **CHANGE**: "N engine copies" becomes N engine
  worker threads sharing one ORT session per (engine, device, precision) (ARCHITECTURE §3).
- torch thread caps: `GPU_ENGINE_TORCH_THREADS`, `cap_torch_threads`, `hold_torch_threads`,
  `served_thread_env` (`ER:1404-1490`); `_MODEL_IMPORT_LOCK`/`import_model_stack` (`ER:1374-1386`);
  `load_sibling` (`ER:1389`); `apply_rocm_override` (`ER:9655`); `linear_patch_embedding` (`ER:1732`,
  a MIOpen workaround — already baked into both ONNX exports as a MatMul); `fp32_master`/`cast_from_master`
  (`ER:558-577`).
- rtdetr: not present in the 0.5.2 tree at all (grep finds nothing); nothing to drop.

The PP-OCR detector/CTC recognizer (`ppocr.py`), `line_layout.py` and `line_reconcile.py` are only
described here at their interfaces with the recognizers.

---

## 1. The runner process and its JSON-over-stdio protocol

**CHANGE in 0.7:** the runner is no longer a subprocess; the processor runs the pipeline in-process
and talks to the scheduler over a channel (ARCHITECTURE §6). The *events and their invariants* below
are what the server-side logic (`ocr/session.py`, `ocr/watcher.py`) consumes, so they are the
contract to preserve, whatever the transport.

### 1.1 Modes and command line (`build_parser`, `ER:9400-9620`; `parse_args`, `ER:9623-9652`)

Three modes, chosen by flags (`main`, `ER:9673-9680`):

| mode | flags | stdout |
|---|---|---|
| single-volume CLI (`run`, `ER:8044-8118`) | `--input DIR --output FILE.mokuro --cache-dir DIR` | human log lines (server parses them, see §1.6) |
| session (`serve`, `ER:8338-8350`) | `--serve --session-log FILE` | protocol only |
| benchmark (`bench`, `ER:9392`) | `--bench --session-log FILE --input SAMPLE_DIR` | protocol only |

Flags that survive (KEEP semantics): `--engine {hayai-nova, paddle-manga, ppocr-manga}` (`ENGINE_IDS`
`ER:646`; `mokuro` DROP), `--detector` (default `ppocr-manga`; only value left), `--detect-dir`,
`--patches {256,384,512}` (default 512, hayai only, `ER:276-279`), `--cpu-workers`, `--stage-workers`,
`--queue-capacity`, `--stage-device`, `--precision`, `--precision-pick`, `--precision-why`,
`--stats-file`, `--bench-max-trials` (8), `--bench-precision-only`, `--bench-budget-seconds` (900),
`--title`, `--volume`, `--title-uuid`, `--volume-uuid`, `--generator`.
Validation: `--serve` xor `--bench`; both require `--session-log`; `--bench` requires `--input` and
refuses `--stage-workers`/`--queue-capacity`; CLI mode requires `--input/--output/--cache-dir`.

Env overrides (KEEP names): `MOKURO_OCR_CPU_WORKERS`, `MOKURO_OCR_JOBS`, `MOKURO_OCR_STAGE_WORKERS`,
`MOKURO_OCR_QUEUE_CAPACITY`, `MOKURO_OCR_STAGE_DEVICE`, `MOKURO_OCR_PIPELINE_STATS` (`ER:2114-2125`).

### 1.2 stdout discipline (`seize_stdout`, `ER:7574-7613`; logging seam `ER:141-257`)

- In `--serve`/`--bench`, fd 1 is duplicated away for the protocol and fd 1 itself is re-pointed at
  the session log, so *any* stray write (ORT, C printf, children) lands in the log, never in the
  protocol. fd 2 is redirected too only if it was the same open file as fd 1.
- Every human line goes to the session log and, if the emitting thread is bound to a volume
  (`RunnerLog.bind`, `_attributed`), also to that volume's own log file. Writes never raise.
  **CHANGE:** in Rust this is a `tracing` span carrying the volume id plus a per-volume file layer.
- Protocol lines: `json.dumps({"event": <name>, **fields})`, **ASCII-escaped** (`ensure_ascii`
  default), one object per line, flushed, under a lock (`Protocol.emit`, `ER:7554-7571`).

### 1.3 Ops: server → runner (stdin, one JSON object per line; `ServeSession.read_ops`, `ER:8237-8265`)

| op | fields | handling |
|---|---|---|
| `volume` | `id` (required), `output`, `cache_dir`, `detect_dir`?, `log`?, `workspace`?, `title`?, `volume`?, `title_uuid`?, `volume_uuid`?, and **exactly one of** `archive` (path to .cbz; plus optional `stem`) or `input` (directory) | `op_volume` (`ER:8142-8209`). As sent by the server: `SessionVolume.to_op` (`ocr/session.py:122-143`). |
| `close` | – | stop reading; end of feed after every accepted volume (`op_close`, `ER:8229-8235`) |
| `page`, `end`, or `volume` with `"pages":"stream"` | – | **retired**: emit one `fatal` ("this runner takes archives only; …") and stop reading (`_trip`, `ER:8211-8227`); process exits 1 |
| anything else | – | log `WARN unknown op`, continue |
| unparsable line | – | log `WARN unreadable op`, continue |
| EOF / broken stdin | – | same as `close` |

`volume` details (`_accept`, `ER:8171-8209`; `Session.accept`, `ER:7658-7703`):
- Missing/empty `id` → logged only, no event. Duplicate `id` already in session → logged only.
- `detect_dir` defaults to `<output dir>/_detect/<engine>`. `stem` is reduced to its basename.
- `archive`: pages = member map of the zip (§2.1); pages are decoded from memory, never written
  (spooling only happened on the dropped adapter road). `input`: pages = recursive image listing.
- Directories `cache_dir`, `detect_dir`, `output.parent` (and the log's parent) are created.
- Any exception while accepting → `volume_started{id, pages:0}` then `volume_failed{id, error}`.
- Zero pages → `volume_started{id, pages:0}` then `volume_failed{id, error:"no page images found in <stem>.cbz|<archive>|<input>"}`.
- A `volume` arriving after `close` → `WARN … arrived after close; ignored`.

### 1.4 Events: runner → server (stdout)

All carry `"event"`. Field order is irrelevant; consumers ignore unknown events/fields
(`ocr/session.py:57-62`).

| event | fields | when (source) |
|---|---|---|
| `ready` | `startup_seconds` (float, 3 dp), `weights` {repo: commit}, `stage_workers` {stage: int}, `queue_capacity` {stage: int}, `stage_device` {model-stage: "cpu"\|"gpu:<n>"}, `pipeline` (graph line string, e.g. `detect (cpu x3, queue 4) -> engine (gpu:0 x1, queue 1) -> post (cpu x1, queue 1)`) | once, after **all models loaded** (`wait_ready`, `ER:7165-7177`); `ER:8290-8298` |
| `volume_started` | `id`, `pages` (int) | once per accepted volume, before its pages (`ER:8198`) |
| `page` | `id`, `done` (pages seen so far incl. failed), `total` | per page leaving the sink, in page order (`ER:7790`) |
| `stats` | `pipeline` = `PipelineReport.as_dict()` (§1.4.1), optional `cpu_pressure` (PSI `some avg10`/100, 3 dp), optional `other_cpu` (share of host CPU used by non-runner processes over ~10 s, 0..1) | from the sink at most every `PIPELINE_STATS_INTERVAL`=2.0 s (`_tick`, `ER:7807-7820`); also rewrites `<detect_dir>/pipeline.json` (or `--stats-file`) |
| `volume_done` | `id`, `pages` (results written), `failed_pages`, `seconds` (volume's own wall time, 3 dp; volumes of one session partition time, `ER:7999-8015`), `stats` (this volume's delta of the counters, `ER:7863-7875`), optional `cpu_pressure`, optional `other_cpu` | after the sidecar is renamed into place (`ER:7975-7987`) |
| `volume_failed` | `id`, `error` (string) | every page failed (`"every page failed"`), zero pages, accept error, or session abandoned (`"the session ended before this volume did"` / the load/pipeline error) (`ER:8017-8023`, `ER:8329-8335`) |
| `fatal` | `error` | model load failure before `ready`; pipeline crash; recognizer load failure discovered mid-run; retired op (`ER:8283`, `ER:8316`, `ER:8323`, `ER:8227`) |

Invariants (KEEP):
1. Every volume with a usable id gets exactly one `volume_started` and exactly one of
   `volume_done`/`volume_failed`.
2. Pages of a volume are emitted in input order; volumes complete in arrival order (the pipeline
   reorders at its sink by sequence number), but the pipeline does **not** drain between volumes:
   volume N+1 is detecting while volume N is in the engine.
3. A recognizer that failed to **load** is never reported as blank pages: the first page that hits
   it ends the run with `fatal` (`Session._page`, `ER:7746-7753`).
4. Process exit code: 0 after a clean close; 1 after `fatal`, after a retired op, or after an
   abandoned session.

Synthetic events added by the *server's* pipe reader, not the runner (`ocr/session.py:31-38`):
`exit {returncode}` (always last) and `spawn_failed {error}`. In 0.7 the equivalent is the processor
task ending; the scheduler must still see exactly one terminal signal per session.

#### 1.4.1 `PipelineReport.as_dict()` (`ER:3479-3487`)
`{"elapsed_seconds", "items", "stages":[StageReport], "queues":[QueueReport], "bottleneck": stage key | null}`;
`StageReport` = `{key, name, device, workers, items, busy_seconds, blocked_seconds, starved_seconds, utilisation (3 dp), device_bound}` (`ER:3353-3384`);
`QueueReport` = `{name, capacity, depth, max_depth, mean_depth, depth_seconds, puts, gets, blocked_seconds, blocked_events, starved_seconds, starved_events, fill (3 dp)}` (`ER:3172-3224`).
Counters are cumulative; a volume's `stats` is `report().since(mark)` taken when its first page
entered the pipeline. The reading/verdict logic (`summarize`, `pipeline_verdict`, `ER:3515-3830`)
is shared with the server and belongs to the scheduling spec.

#### 1.4.2 Bench events (`BenchRun`, `ER:8651-9353`)
`bench_ready {startup_seconds, model_load_seconds, min_window_seconds (20.0), pages, tunable, max_trials, stage_keys, stage_device}` (`ER:9257-9267`);
`bench_progress {trial, pass_index, pages_done, pages, stage_workers, pages_per_second, window_seconds, pages_measured}` (≥1 s apart, `ER:8761-8771`);
`bench_trial {n, note, stage_workers, queue_capacity, stage_device, seconds, pages_per_second, window_seconds, pages_measured, passes, short_window, first_emission_at, last_emission_at, accepted, verdict, bottleneck, stages, queues, precision?}` (`ER:8589-8604`, `ER:8956-8960`);
`bench_done {baseline:{pages_per_second, seconds_per_page, window…}, best:{trial, stage_workers (only diffs from derived), queue_capacity:{} (always empty), stage_device (only moved stages), pages_per_second, seconds_per_page, speedup, window…}, precision?, precision_mode?, precision_trials?:[{precision, pages_per_second, chosen}], precision_why?, peak_rss_mb, peak_vram_mb}` (`ER:9284-9353`);
`fatal {error}`. Rate = `(M-1)/(t_last - t_first)` over emissions after the fill (`bench_window`, `ER:8466-8494`).
The search algorithm itself is out of scope here (tuning spec).

### 1.5 Volume assembly and files written (`Session._page`, `ER:7729-7792`; `_assemble`, `ER:7897-7988`)

Per page, in page order:
1. `outcome.unwrap()`; on error: if recognizer load failed → end run (fatal); else `failed += 1`, log
   `ERROR page <rel>: <e>` + traceback, and substitute a **blank page** `{version, img_width, img_height, blocks: []}`
   built from the image header (`_blank_from`, `ER:771-800`). If even the header is unreadable the page
   is **omitted** from the sidecar.
2. Reconciled road: if the page has a reconcile tally, append `{"page": rel, "lines": doubtful}` to the
   volume's review list (only if `doubtful` non-empty) and log the `reconcile …` line.
3. Write the page dict to `<cache_dir>/<rel with .json suffix>` (progress files the server counts).
4. Log `page <seen>/<total> <rel> blocks=<n> (<secs>s)`; emit `page`; maybe `stats`.

Per volume (last page arrived):
- If no results: `ERROR: every page failed` → `volume_failed`.
- Else build the sidecar (`build_volume`, `ER:1077-1107`) and write it to `<output>.tmp`, then rename
  to `<output>` (JSON, `ensure_ascii=False`, numpy scalars as numbers, default separators).
- Reconciled road: write `<detect_dir>/review.json` = `{"format":"ocr-review/1","engine","detector","pages":[…]}`.
- Log `wrote <output> pages=<n> failed_pages=<f> elapsed=<s>s` then exactly `Processed successfully: 1/1`.
- The raw per-page dump (what the models saw, reconcile details) is written by the post stage to
  `<detect_dir>/<rel with .json suffix>` (`ER:6980-6983`, `PageJob.dump` `ER:4504-4507`).

Sidecar shape (KEEP byte-compatible keys and order):
```json
{"version":"0.2.5","title":…,"title_uuid":…,"volume":…,"volume_uuid":…,
 "ocr_engine":{"id":"hayai-nova","recognizer":"JustANormalTinkerer/hayai-ocr-v2.5-nova",
               "detector":"ppocr-manga","generator":"mokuro-bunko",
               "patch_budget":512,            // hayai-nova only
               "weights":{repo: commit, …},   // only if non-empty
               "precision":"fp32"},           // only once known
 "pages":[{"version":"0.2.5","img_width":W,"img_height":H,"blocks":[…],"img_path":"rel/posix"}]}
```
`title`/`volume` default to the stem (or input dir name); uuids default to fresh `uuid4`.
`weights` = PP-OCR repo pin (only if its files came from the pinned download, `ER:4657-4665`) ∪
recognizer `repos` (hayai: `{hayai repo: e46d791…, google/siglip2-base-patch16-naflex: b53b807…}`;
paddle: `{PaddlePaddle/PaddleOCR-VL-1.6: c5630ab…, sorryhyun/paddleocr-vl-1.6-manga-lora: 2629283…}`,
`ER:124-129`, `ER:1566`, `ER:1836`). **CHANGE (open question Q9):** 0.7 provenance should name the ONNX
export id as well (ARCHITECTURE §7).

### 1.6 Single-volume CLI (`run`, `ER:8044-8118`)
Pages = `list_pages(input)`; none → log `ERROR: no page images found under <dir>`, exit 1. Bad config →
exit 2. The recognizer load overlaps the pipeline (no `wait_ready`). Load failure → exit 1; every page
failed → exit 1; else writes as §1.5 (summary lines printed after teardown) and exits 0. stdout carries
the log lines (the server's per-volume log parser reads `Processed successfully: 1/1`).

---

## 2. The page pipeline (reconciled road)

### 2.1 Page source (`ER:7274-7385`, `ER:803-818`)
- Archive: page list = every non-directory member whose *extracted* name (`extracted_name`: `/` and
  altsep → path sep, drive dropped, empty/`.`/`..` components removed) has suffix in
  `(.jpg,.jpeg,.png,.webp,.avif)` (case-insensitive), **excluding** the top-level `<stem>.webp`
  thumbnail; duplicates collapse to the last member. Order: `natsort.natsorted` on the `Path`s
  (fallback `_natural_key`: split on `\d+`, ints compared numerically, text lower-cased). The page's
  identity/`img_path` is the extracted relative name.
- Directory: recursive glob of files with those suffixes, same ordering.
- A member that cannot be read yields an empty blob → the page fails in the first stage → blank page.
- Page order and member mapping are shared with the archive/scheduling specs; they must agree
  member-for-member or sidecars differ.

### 2.2 Page decode (`_imdecode`, `ER:1193-1199`; `PageJob.decode`, `ER:4514-4523`)
`PIL.Image.open(bytes)` → (first frame of animated formats) → `.convert("RGB")` → numpy → `cv2.cvtColor(RGB2BGR)`.
The whole pipeline works on that BGR uint8 array; crops are turned back to RGB.
Port notes **[verified 2026-10-01, Pillow 12.3.0]**:
- No EXIF orientation is applied (PIL does not auto-rotate). Do not apply it in Rust.
- `RGBA`/`LA`/`P`-with-alpha → alpha **dropped**, no compositing (RGBA(200,100,50,0) → (200,100,50)).
- `L` → (v,v,v). `I;16` → clipped (1000 → (255,255,255)).
- `CMYK` → `MULDIV255(255-c, 255-k)` per channel ((100,30,0,50) → (125,181,205)).
- Pixel values of decoded JPEG/WebP/AVIF depend on the decoder (Pillow 12.3 = libjpeg-turbo,
  libwebp, libavif). See open question Q3.

### 2.3 Stage graph (`STAGE_GRAPHS[ROAD_RECONCILED]`, `ER:2559-2566`; callables `ER:6932-6985`)

```
feed (archive reader, one thread) ─► detect (pool, CPU) ─► engine (1 per model, GPU or CPU) ─► post (pool, CPU) ─► sink (reorder by seq)
```
- **detect** (`detect + CTC read`, 0.225 s/page): lease a PP-OCR session pair from `PPOcrPool`
  (`ER:4583-4638`), decode the page, `PPOcrPageReader.read_lines` (`ER:4687-4730`): detect + CTC-read
  lines; join column pieces (`layout.column_pieces` → `engine.join_lines`); first layout (to find ruby
  and text bodies); recover clipped brackets on non-ruby lines; second opinions on doubted characters.
  Output `DetectedPage(image, lines, info, first_layout)` (`ER:4535`).
- **engine** (`engine read + reconcile`, device-bound width 1 per model): `ReconciledPageReader.engine_read`
  (`ER:4831-4918`), detailed in §2.4.
- **post** (`layout + dump`): `finish_read` (`ER:4920-4942`): `line_layout.layout_page` over the merged
  lines, drop blocks of kind `noise` (`layout_page_dict`, `ER:1110-1137`), raw dump with reconcile
  fields, tally, `doubtful_lines` (`ER:5032-5051`); write dump.
- Line road (`ppocr-manga` alone) is `detect → layout` (`ER:2552-2555`, `ER:6964-6973`).

Scheduling (KEEP semantics, details in the scheduling spec): bounded queues between every pair of
stages; capacities default to one slot per worker of the filling stage, and **≥ 4** (`LOAD_WINDOW_SLOTS`)
for the queue feeding a device-bound stage so detection runs ahead while the recognizer loads
(`stage_capacities`, `ER:3047-3079`); widths derived from per-page costs (`plan_stage_workers`,
`ER:2895-2967`; `ENGINE_STAGE_SECONDS` paddle 0.915 s, hayai 0.177 s, `ER:2631-2634`); explicit
`--stage-workers key=N` wins; `--cpu-workers 0` = fully serial. A page failure in any stage travels as
an `Outcome` (`ER:3112-3127`) and surfaces at the sink in page order. The recognizer loads on its own
thread from t=0 (`DeferredRecognizer`, `ER:5098-5172`): the first page reaching the engine waits; a load
error is re-raised at that call and becomes a session `fatal`.

### 2.4 Engine stage in detail (`ReconciledPageReader`, `ER:4792-5026`)

Inputs: `lines` from detect, each with `quad` (4 points, ordered TL,TR,BR,BL in the line's upright frame —
`ppocr.order_quad`, `ppocr.py:408-448`), `vertical` (`height > width` in that frame,
`ppocr.py:466-473`), `angle`, `text` (CTC), `conf`, `score` (detector), `char_confs`.

1. `targets` = indices of lines that are **not ruby** (`first.ruby`). Each target becomes a block
   `{"lines":[quad], "vertical": bool}` (`_line_block`, `ER:4982-4985`).
2. `pitch = body_pitch(lines)` (`ER:900-918`); `neighbours = parallel_neighbours(lines)` (`ER:921-954`);
   `cells[k] = line_reconcile.line_cells(main, thickness, pitch)` with `(main, thickness) = quad_extents(quad, vertical)` (`ER:882-897`).
3. **First read**: `texts = _read(img, blocks, cells, crop_fn, all k)` (`ER:4948-4979`):
   for each target in order, all crops of `crop_fn(img, block, 0)` (hayai may give several chunks per
   line, paddle exactly one) go into **one recognizer call**; per-crop token cap
   `token_cap(cells[k])` (`line_reconcile.py:390-393`: `min(max(ceil(cells·1.5)+8, 12), 160)`) is passed
   only to recognizers with `token_caps = True` (paddle). A line's text = concatenation of its chunks'
   texts. `len(texts) != len(crops)` → RuntimeError (page fails).
4. `settled[k] = reconcile_line(texts[k], normalize_text(ctc.strip()), cells[k], thin=…, ctc_conf, ctc_char_confs)`.
5. **Second read** (only when `second_crop_fn` is set = paddle-manga): lines with
   `needs_second_read(settled[k])` are re-read in one call with the wider crop and settled with
   `settle_disputes`.
6. Engine-only verdicts, text write-back, conf bump to `CONFIRMED_CONF`, `_trim_repeats` (seam
   de-duplication between unjoined column pieces, uses `line_margin_px`).
7. Return `ReadPage(detected, targets, settled, pitch, engine_seconds)`.

Recognizer contract on this road: `recognize(crops: [RGB image], max_tokens: Option<[int]>) -> [String]`,
same length, **un-folded** text: `fold=False` (`ER:6939`), i.e. the decoded string with Python
`str.strip()` applied and **no NFKC** (`ER:1618`, `ER:1919`). (NFKC `normalize_text` is applied only on the
dropped adapter road.)

---

## 3. Crop extraction

### 3.1 Which crop (`select_crop`, `ER:680-696`, with `ER:6945-6951`)
On the `ppocr-manga` detector:
- **hayai-nova** → `"line"` crops (`make_line_crop_fn`, `ER:1253-1270`), margin 0 (tight: neighbouring
  furigana must stay out); **no second read**.
- **paddle-manga** → `"quad"` crops (`make_quad_crop_fn(LINE_MARGIN_EM=0.25)`, `ER:1313-1339`); second
  read with `make_quad_crop_fn(SECOND_MARGIN_EM=0.5)`.

"Upright requirement": neither recognizer ever sees rotated glyphs. hayai gets the line deskewed to a
64-px-thick strip *in its own orientation* (a vertical column stays a 64-px-wide column; it is only
rotated temporarily for chunking and rotated back, `ER:1261-1267`). paddle gets the deskewed quad, padded,
orientation kept ("a column stays a column", the LoRA's training format). Quads tilted > 45° are read in
the complementary orientation (known limit, `ppocr.py:422-432`).

### 3.2 hayai-nova line crop (`warp_line`, `ER:1202-1227`; `split_long_line`, `ER:1230-1250`)

Constants: `TEXT_HEIGHT=64`, `MAX_RATIO_VERTICAL=16`, `MAX_RATIO_HORIZONTAL=8`, `ANCHOR_WINDOW=2` (`ER:749-752`).

```
src = quad as f32[4][2]                       # TL, TR, BR, BL
mid[i] = (src[(i+1)%4] + src[i]) / 2           # mid0 top edge, mid1 right, mid2 bottom, mid3 left (f32)
vec_v = mid2 - mid0 ; vec_h = mid1 - mid3
ratio = f32(|vec_v|) / max(1e-6, f32(|vec_h|))  # computed in f32, then widened to f64
if vertical: w = 64; h = max(1, round_half_even(64 * ratio))
else:        h = 64; w = max(1, round_half_even(64 / max(1e-6, ratio)))
dst = [[0,0],[w-1,0],[w-1,h-1],[0,h-1]]
M = getPerspectiveTransform(src, dst)         # 3x3, f64
region = warpPerspective(img_bgr, M, (w,h), INTER_LINEAR, BORDER_CONSTANT(0,0,0))
strip = vertical ? rotate90_ccw(region) : region      # strip height is now 64
max_ratio = vertical ? 16 : 8
chunks = split_long_line(strip, max_ratio)
if vertical: chunks = [rotate90_cw(c) for c in chunks]
return [BGR→RGB(c) for c in chunks]
```

`split_long_line(strip, max_ratio)`:
```
h, w = strip.shape; ratio = w / max(1, h)
if ratio <= max_ratio: return [strip]
n = ceil(ratio / max_ratio)
gray = BGR2GRAY(strip)                         # see 3.4 for the exact integer formula
ink[y][x] = 255 - gray[y][x]                   # f32
density[x] = Σ_y ink[y][x]                     # exact in f32 (integers ≤ 16320)
K = gaussian kernel, 128 taps (table in 3.4)   # = cv2.getGaussianKernel(128, 8.0) from OpenCV 5.0
full = convolve(density, K)  (length w+127, f64)
smooth[i] = full[i + 63]   for i in 0..w       # numpy 'same' for an even 128-tap kernel [verified]
cuts = chunk_cut_points(smooth, w, n, 128)
return [strip[:, a:b] for consecutive (a,b) over [0]+cuts+[w] if b > a]
```
`chunk_cut_points(density, width, n, window)` (`ER:1140-1161`): for k in 1..n:
`anchor = int(round_half_even(width*k/n))`; `lo = max(0, anchor - window//2)`; `hi = min(width, anchor + window//2)`;
if `hi <= lo` → cut at anchor, else cut at the **first** index of the minimum of `density[lo..hi)`.
(Chunking requires w ≥ 513 px, so `w ≥ 128` always holds and the 'same' offset is fixed.)

### 3.3 paddle-manga quad crop (`ER:1284-1339`)
```
(main, cross) = quad_extents(quad, vertical=True)            # main = |mid2-mid0|, cross = |mid1-mid3| (f64)
pad = min(0.12 * max(main, cross), margin_em * min(main, cross))   # line_margin_px; 0.12 = UPRIGHT_MARGIN
u = unit(p0→p1); v = unit(p0→p3)          # unit(a,b) = (b-a)/(|b-a| or 1.0)
padded[i] = p[i] + pad*(su_i*u + sv_i*v)  with (su,sv) = (-1,-1),(1,-1),(1,1),(-1,1) for TL,TR,BR,BL
src = padded as f32
width  = |src1 - src0| ; height = |src3 - src0|               # f32 norms → f64
scale = max(1.0, 16 / max(1.0, min(width, height)))         # MIN_CROP_SIDE = 16
w = max(2, int(round_half_even(width*scale))) ; h = max(2, int(round_half_even(height*scale)))
dst = [[0,0],[w,0],[w,h],[0,h]]                             # NOTE: w,h — not w-1,h-1
M = getPerspectiveTransform(src, dst)
crop = warpPerspective(img_bgr, M, (w,h), INTER_CUBIC, BORDER_REPLICATE)
return [BGR→RGB(crop)]
```
`margin_em` = 0.25 for the first read, 0.5 for the second read.

### 3.4 OpenCV numeric fidelity (OpenCV pinned at `opencv-python-headless==5.0.0.93`, `installer.py:96-104`)

**[verified 2026-10-01 against cv2 5.0.0]**
- `cvtColor(BGR2GRAY)` is exactly `Y = (B*3735 + G*19235 + R*9798 + 16384) >> 15` (15-bit; R=round(0.299·2¹⁵),
  G=round(0.587·2¹⁵), B=remainder) — bit-exact over an 86³ colour grid. (The OpenCV 4 14-bit formula is *not* it.)
- `getGaussianKernel(128, 8.0)` is **not** a plain normalised `exp(-x²/128)`: tails are the normalised
  Gaussian ×0.99980543 and the two centre taps are raised so the sum is 1. Use this exact f64 table
  (symmetric: `K[127-i] = K[i]`):
  ```
  K[0..64] =
    1.0389895019074014e-15, 2.7804800927626877e-15, 7.325589342867247e-15, 1.900113298220203e-14,
    4.852109287460303e-14, 1.2198201404822987e-13, 3.019083842773331e-13, 7.356456934342762e-13,
    1.7647222790927795e-12, 4.167716688152101e-12, 9.690231624253081e-12, 2.2181161089654484e-11,
    4.998601813631643e-11, 1.1089882831357237e-10, 2.4222531072100706e-10, 5.208662698467078e-10,
    1.1026739019184627e-09, 2.298169761000774e-09, 4.715538172432299e-09, 9.525648951343214e-09,
    1.8944014991623525e-08, 3.709058078072457e-08, 7.149396568086143e-08, 1.3567170722359226e-07,
    2.534681185584036e-07, 4.661992183999863e-07, 8.441777275272705e-07, 1.504909511410206e-06,
    2.64119844630433e-06, 4.563581678754164e-06, 7.762913937226354e-06, 1.300043438435446e-05,
    2.143409269782652e-05, 3.479096654644236e-05, 5.5595806240599604e-05, 8.746448026196381e-05,
    0.00013546763708401668, 0.00020656347932306637, 0.0003100885094505717, 0.00045828110906597245,
    0.0006667950791726975, 0.0009551398514655293, 0.0013469630855317647, 0.0018700730459375219,
    0.0025560867919538536, 0.0034395907356138287, 0.004556717382806207, 0.0059430798490069815,
    0.007631065579529709, 0.009646570775100003, 0.012005351300600717, 0.01470926389308055,
    0.017742759054549573, 0.021070047708563668, 0.024633381437753288, 0.028352848238740617,
    0.03212798612176742, 0.03584135850494638, 0.03936403145442814, 0.04256266634277,
    0.045307722478648345, 0.047482085479490184, 0.04898932866777286, 0.04985808237175914,
  ```
- `warpPerspective` in OpenCV 5.0 is **not** the old 1/32-subpixel fixed-point remap (that model
  mismatches 61% of channel values). It is very close to straightforward per-pixel float sampling:
  `(sx, sy) = (M⁻¹·(x,y,1))` projected, bilinear (`INTER_LINEAR`) or Keys cubic with **a = −0.75**
  (`INTER_CUBIC`, 4×4 taps at `floor−1..floor+2`), round-to-nearest, saturate. Best reproduction found
  (f32 coordinates, `1/w` reciprocal, lerp form) differs by **±1 on 0.055%** of channel values
  (bilinear) and ±1 on ~0.08% (cubic); never more than 1. Bit-exactness would require porting
  OpenCV 5's `imgproc/src/warp_kernels.simd.hpp` or linking OpenCV (Q2).
- `getPerspectiveTransform(src, dst)`: solve the standard 8-unknown linear system in f64 (OpenCV uses
  LU/SVD `DECOMP_LU`); `warpPerspective` without `WARP_INVERSE_MAP` inverts `M` (f64).
- Border: `BORDER_CONSTANT` with 0 for line crops (pixels outside the page are black);
  `BORDER_REPLICATE` (clamp coordinates) for quad crops.
- `cv2.rotate` 90° CW/CCW is lossless index arithmetic.

---

## 4. Shared numeric primitives

### 4.1 PIL `Image.resize` (used by both recognizers' preprocessing) — **[verified bit-exact, Pillow 12.3.0]**
A pure re-implementation of Pillow's `libImaging/Resample.c` matched `Image.resize` with `BILINEAR` and
`BICUBIC` bit-for-bit on 24 random RGB up/down-scales (max abs diff 0). Algorithm (8-bit RGB, box = whole image):
```
PRECISION_BITS = 22
filters: bilinear(x) = max(0, 1-|x|), support 1.0
         bicubic(x, a=-0.5) = |x|<1: ((a+2)|x| - (a+3))|x|² + 1 ; |x|<2: (((|x|-5)|x|+8)|x|-4)·a ; else 0   (support 2.0)
coeffs(in, out, filter, support0):           # all f64
  scale = in/out ; fs = max(scale, 1.0) ; support = support0*fs ; ksize = ceil(support)*2 + 1
  for xx in 0..out:
    center = (xx + 0.5)*scale ; ss = 1/fs
    xmin = max(0, trunc(center - support + 0.5)) ; xmax = min(in, trunc(center + support + 0.5)) - xmin
    k[x] = filter((x + xmin - center + 0.5)*ss) for x in 0..xmax ; ww = Σk ; if ww != 0: k /= ww
    ki[x] = k[x] < 0 ? trunc(-0.5 + k[x]*2^22) : trunc(0.5 + k[x]*2^22)       # i32
pass(value):  acc = 2^21 + Σ src·ki  (i64/i32) ; out = clamp(acc >> 22, 0, 255)
order: if out_w != in_w: horizontal pass over source rows [ybounds[0].xmin .. last.xmin+last.xmax) into a u8 temp;
       if out_h != in_h: vertical pass (row bounds shifted by the first used row). Each pass rounds to u8.
```
(`trunc` = C `(int)` cast, i.e. toward zero.) The same `coeffs` in f32 without the integer step is the
torch `antialias=True` bilinear used for hayai's position table (§5.4).

Note: 0.5.2 itself preprocesses through transformers' torchvision-backed processors, which differ from
PIL on 0.4% (hayai) / 0.2% (paddle) of normalised values by up to 2 LSB **[verified 2026-10-01]**; the
spike's PIL path nevertheless reproduced 0.5.2's texts exactly (§5.12, §6.12). PIL is therefore the
reference for the port.

### 4.2 Other primitives
- Python `round()` on floats is **round-half-to-even** — used in `warp_line`, quad crop sizes,
  `chunk_cut_points`, paddle `smart_resize`. `int(x)` truncates toward zero; `math.ceil/floor` as usual.
- `str.strip()` strips Python whitespace (`str.isspace`): Unicode `White_Space` **plus**
  U+001C..U+001F. Rust `str::trim` lacks U+001C..U+001F — use a custom predicate.
- NFKC (`unicodedata`, Python 3.14 = Unicode 16.0) is used by `line_reconcile`/`line_layout`, not by the
  recognizers on this road; pin `unicode-normalization` to a Unicode-16 release.
- argmax: first index of the maximum (numpy and torch CPU agree).

---

## 5. hayai-nova on ONNX Runtime

### 5.1 Model facts (`modeling_hayai.py` at `e46d791…`, HF cache snapshot)
- Vision: SigLIP2-base NaFlex (`google/siglip2-base-patch16-naflex@b53b807…`): patch 16, patch vector
  768 = 16·16·3 (py, px, c order), linear patch embedding, learned position table 256×768 (16×16 grid),
  12 layers, hidden 768, 12 heads × 64, LayerNorm eps 1e-6, post-layernorm.
- Projector `DSCProjector` (`modeling_hayai.py:93-140`): per image, features `(hp, wp, 768)`,
  replicate-pad to even `hp`, `wp`, `pixel_unshuffle(2)` → `(ceil(hp/2)·ceil(wp/2), 3072)` with channel
  index `c·4 + dy·2 + dx`, then LayerNorm(3072) → Linear(3072→512, no bias) → GELU(erf) → Linear(512→512)
  → RMSNorm(512, eps 1e-6). Batch rows zero-padded to the longest `M`.
- Decoder: 12 layers, d_model 512, GQA 8 q-heads / 2 kv-heads × 64, q/k RMSNorm per head, SwiGLU 2048,
  per-channel residual scales (`attn_res_scale`, `ffn_res_scale`), final RMSNorm, untied output head
  512→16004, SDPA scale 1/√64. RoPE is applied to **interleaved pairs** (`view_as_complex`, `modeling_hayai.py:66-73`).
- Tokenizer: ByteLevel BPE, vocab 16000 + added tokens. Special ids: `<pad>`=16000, `<bos>`=16001,
  `<eos>`=16002, `<unk>`=16003, and `[PAD]`=0, `[UNK]`=1, `[BOS]`=2, `[EOS]`=3 (all `special: true`)
  (`spike/hayai/special.json`; `tokenizer.json`). The loop uses bos=16001, eos=16002, pad=16000
  (`ER:1671-1673`: `tokenizer.bos_token_id or 1`, etc.).

### 5.2 Assets
| file | content |
|---|---|
| `nova_vision.onnx` / `_fp16` | vision tower + projector (§5.6) |
| `nova_decoder.onnx` / `_fp16` | 12 decoder layers + final norm + head, last-position logits, KV in/out (§5.9) |
| `pos_table.npy` | f32 (256, 768): `vision_encoder…embeddings.position_embedding.weight` (`spike/hayai/check_ort.py:11-13`) |
| `token_embeddings.npy` | f32 (16004, 512): `decoder.token_embeddings.weight` |
| `tokenizer.json` | from the hayai repo snapshot (for decode only) |

### 5.3 Preprocessing (`spike/hayai/nova_pre.py:8-35`; reproduces `Siglip2ImageProcessor`, `max_num_patches = --patches`)
For each RGB crop (H, W), budget `B` ∈ {256, 384, 512} (default 512):
```
size_for_budget(H, W, B):                    # f64, binary search, eps = 1e-5
  scaled(s, n) = int(max(16, ceil(n*s/16)*16))
  lo = 1e-6 ; hi = 100.0
  while hi - lo >= 1e-5:
    s = (lo+hi)/2
    if (scaled(s,H)/16)*(scaled(s,W)/16) <= B: lo = s else: hi = s
  return (th, tw) = (scaled(lo,H), scaled(lo,W))
img = PIL-resize(crop, (tw, th), BILINEAR)          # §4.1
x = (u8 * (1/255) - 0.5) / 0.5                     # f32
hp = th/16 ; wp = tw/16
patches[p = py*wp + px][(iy*16 + ix)*3 + c] = x[py*16+iy][px*16+ix][c]
pixel_values[b] : f32 (B, 768) — rows hp*wp.. are 0
pixel_mask[b]   : 1 for rows < hp*wp else 0
spatial_shapes[b] = (hp, wp)
```
Spatial shapes and masks equal the HF processor's on 64 crops **[verified 2026-10-01]**. The patch axis is
**always padded to B** (not to the batch maximum), as the HF processor does.

### 5.4 Position embeddings (`spike/hayai/nova_host.py:9-40`, `:55-62`)
Grid `G` = pos_table as (16, 16, 768). For each image, `R(hp, wp)` = antialiased bilinear resize of the
grid to (hp, wp) with `align_corners=False` (torch `F.interpolate(…, antialias=True)`, what SigLIP2's
`resize_positional_embeddings` does): separable weights `Wy = aa_weights(16, hp)`, `Wx = aa_weights(16, wp)`
computed exactly like §4.1 `coeffs` with the bilinear filter (f64, normalised, then cast to f32, no integer
step); `R[h][w][c] = Σ_y Σ_x Wy[h][y]·G[y][x][c]·Wx[w][x]`, flattened row-major to (hp·wp, 768).
`pos[b][0..hp·wp] = R`; `pos[b][hp·wp..B] = R[0]` (padding rows get the first resized row). Cache by (hp, wp).

### 5.5 Projector gather inputs (`nova_host.py:63-77`)
```
ho = ceil(hp/2) ; wo = ceil(wp/2) ; valid[b] = ho*wo ; M = max_b valid[b]
for t in 0..ho*wo:  y = t / wo ; x = t % wo
  for k, (dy,dx) in enumerate([(0,0),(0,1),(1,0),(1,1)]):
     gather_idx[b][t][k] = min(2y+dy, hp-1)*wp + min(2x+dx, wp-1)     # replicate padding
gather_idx[b][t >= valid] = 0 ; tok_valid[b][t] = (t < valid[b]) as f32
```

### 5.6 Vision graph `nova_vision[_fp16].onnx` (opset 20, IR 10; `spike/hayai/export.py:10-18`, module `nova_modules.py:12-42`)
| name | dtype (fp32 / fp16 file) | shape |
|---|---|---|
| in `pixel_values` | f32 / f16 | (b, P, 768), P = B |
| in `pixel_mask` | f32 / f16 | (b, P) — 1 valid, 0 pad (graph adds −1e9 bias where < 0.5) |
| in `pos` | f32 / f16 | (b, P, 768) |
| in `gather_idx` | i64 | (b, M, 4) |
| in `tok_valid` | f32 / f16 | (b, M) |
| out `vis_tokens` | f32 / f16 | (b, M, 512) |
Dynamic dims: b 1..64, P 2..4096, M 2..2048 (export bounds). Padded token rows are zeroed before the
projector norm, exactly like the torch zero padding.

### 5.7 RoPE tables (`nova_host.py:42-53`; `modeling_hayai.py:14-63`)
`D_AXIS = 32`; `freq[i] = 1 / 10000^(2i/32)`, i = 0..15 (f32).
- Vision token t (row y = t / wo, col x = t % wo): `angle[0..16] = y·freq`, `angle[16..32] = x·freq`.
- Text position n: `angle[0..16] = angle[16..32] = n·freq`.
- `cos = cos(angle)`, `sin = sin(angle)`, 32 values per position; pair j of the 64-dim head
  (elements 2j, 2j+1) rotates by `angle[j]`: `(x0·c − x1·s, x0·s + x1·c)`.
- Prefill table (b, M+1, 32): rows `t < min(ho·wo, M)` = vision values for that image; other vision rows
  cos=1, sin=0 (padding); row `M` (the BOS) = text position 0.
- Decode step `k` (k ≥ 1): text position `k`, shape (b, 1, 32) broadcast.

### 5.8 Masks (`nova_host.py:101-105`, `:121`; torch `nova_key_bias` `ER:1629-1641`)
NEG = −1e9 (f32; becomes −inf when cast to f16 — fine, every row keeps a finite key).
```
key_bias[b][j] = (j < M && j >= valid[b]) ? NEG : 0
prefill mask (b, 1, M+1, M+1): 0 everywhere, except
   mask[b][0][i][M] = NEG for i < M        (vision tokens never see BOS)
   mask[b][0][i][j] += key_bias[b][j]      (padding keys hidden from all queries)
decode step mask (b, 1, 1, L+1): key_bias over the first M keys, 0 for text keys
```

### 5.9 Decoder graph `nova_decoder[_fp16].onnx` (opset 20; `nova_modules.py:44-71`)
Inputs, in order: `embeds` (b, s, 512), `mask` (b, 1, s, L+s), `cos` (b, s, 32), `sin` (b, s, 32), then
`past_k0, past_v0, …, past_k11, past_v11` each (b, 2, L, 64).
Outputs: `logits` (b, 16004) **of the last position only**, then `present_k0, present_v0, …, present_v11`
each (b, 2, L+s, 64) (= past ‖ new). Dtypes: all float tensors f32 in the fp32 file, f16 in the fp16 file
(including `logits`). The prefill passes L = 0 (zero-length past arrays).

Greedy loop (`nova_host.py:95-129`, identical to `nova_generate` `ER:1644-1729`):
```
MAX_NEW = 96 (HAYAI_NOVA_MAX_NEW_TOKENS, ER:746)
vis = vision(...)                                              # (b, M, 512)
x = concat(vis, emb[16001] broadcast (b,1,512))                # (b, M+1, 512)
logits, past = decoder(x, prefill_mask, cos_prefill, sin_prefill, past=∅)
nxt = argmax(logits)                                           # (b,)
toks[b] = [16001, nxt, 16000 ×(MAX_NEW-1)]                      # length MAX_NEW+1
live = nxt ∉ {16002, 16000}
for step in 1 .. MAX_NEW-1:
    if !any(live): break
    xs = emb[nxt] as (b,1,512)                                 # dead rows feed the pad id 16000
    logits, past = decoder(xs, step_mask, cos_t[step], sin_t[step], past)
    nxt = live ? argmax(logits) : 16000
    toks[:, step+1] = nxt
    live &= nxt ∉ {16002, 16000}
text[b] = detok([t for t in toks[b][1:] if t ∉ {16002, 16000}], skip_special=True)
```
At most 96 generated tokens per crop. Rows never interact, so dead rows may be computed and ignored
(the spike does that); do not compact the batch unless parity is re-measured.

### 5.10 Detokenizer (`spike/hayai/nova_detok.py`)
`id → token string` from `model.vocab` plus `added_tokens`; drop ids whose added token has
`special: true` (0, 1, 2, 3, 16000-16003); concatenate; map every char through the GPT-2 byte decoder
(bytes `!..~`, `¡..¬`, `®..ÿ` map to themselves; the remaining 68 bytes map to U+0100+n in increasing byte
order); decode UTF-8 with replacement. This equals `tokenizers`' ByteLevel decoder; the Rust `tokenizers`
crate on the same `tokenizer.json` (`decode(ids, skip_special_tokens=true)`) is an acceptable alternative.

### 5.11 Batching and output
`HayaiNovaRecognizer.__call__` (`ER:1591-1598`): crops are processed in **consecutive chunks of 16**
(`HAYAI_NOVA_BATCH`, `ER:743`) in the order given — no sorting; `max_tokens` is ignored (`token_caps` is
not set on this class). Output per crop: `text.strip()` (fold=False on this road). Batch composition
does not change fp32 results (projector padding is masked; measured 0 of 1,137 crops changed, `ER:1658-1664`).

### 5.12 Parity and performance (spike)
- ORT CPU fp32 with the numpy/PIL host above: **220/220** texts identical to the 0.5.2 torch recognizer
  (fp32, HF processor) on 220 real line crops from two volumes (`spike/hayai/ort_cpu_fp32.json` vs
  `ref_torch_cpu_fp32.json`, re-checked 2026-10-01). Memory notes ORT fp16 on CPU/CUDA also 220/220;
  torch bf16 was the outlier (219). No crop in that set was long enough to be chunked (the chunking path is
  not covered by the parity set).
- CPU: ~2× faster than torch. 8 threads sharing one session pair on 3.14t: 436 crops/s (≈ four torch
  process copies). Single CUDA stream: 1.7× slower than torch (Python per-token loop; a Rust loop with
  IoBinding should remove most of it). `spike/hayai/bench_threads.py`, `bench_ort.py`.

---

## 6. paddle-manga (PaddleOCR-VL-1.6 + manga LoRA) on ONNX Runtime

### 6.1 Model facts
Base `PaddlePaddle/PaddleOCR-VL-1.6@c5630ab…` + LoRA `sorryhyun/paddleocr-vl-1.6-manga-lora@2629283…`
(r=16, α=32 on language-model projections), **merged** (`merge_and_unload`), then the LoRA repo's
`tower.safetensors` loaded over the vision tower (`ER:1820-1832`; `spike/paddle/common.py:27-48`).
- Vision: patch 14, learned position table 27×27=729 × 1152, 27 layers, hidden 1152, 16 heads × 72,
  MLP 4304 gelu-tanh, LN eps 1e-6; 2D RoPE (rotate-half); projector: pre-norm LayerNorm(1152) → 2×2
  merge (4608) → linear_1 → GELU → linear_2 → 1024 (`spike/paddle/wrappers.py:8-39`).
- Text (ERNIE-4.5-0.3B): 18 layers, hidden 1024, 16 q-heads / 2 kv-heads × 128, MLP 3072 SiLU,
  RMSNorm eps 1e-5, rope θ = 500000, M-RoPE sections (16, 24, 24), vocab 103424, untied lm_head
  (`config.json`).
- Image token 100295 (`<|IMAGE_PLACEHOLDER|>`), EOS 2 (`</s>`), pad 0 (`<unk>`).

### 6.2 Assets (`spike/paddle/onnx_<dtype>/`)
`vision.onnx(+.data)`, `decoder.onnx(+.data)` (external data), `embed.npy` (f16 (103424, 1024), the
input token embeddings — stored f16 even for the fp32 graphs, `export.py:10-11`), prompt ids (§6.6),
`tokenizer.json` from the base repo.

### 6.3 Preprocessing (`spike/paddle/pipeline.py:11-30`; HF `PaddleOCRVLImageProcessor`, `smart_resize` in transformers 5)
```
smart_resize(h, w, factor=28, min_pixels=112896, max_pixels=1003520):   # preprocessor_config.json
  if h < 28: w = round_half_even(w*28/h); h = 28
  if w < 28: h = round_half_even(h*28/w); w = 28
  if max(h,w)/min(h,w) > 200: error "absolute aspect ratio must be smaller than 200"   # → page fails
  hb = round_half_even(h/28)*28 ; wb = round_half_even(w/28)*28
  if hb*wb > max_pixels: beta = sqrt(h*w/max_pixels); hb = max(28, floor(h/beta/28)*28); wb = max(28, floor(w/beta/28)*28)
  elif hb*wb < min_pixels: beta = sqrt(min_pixels/(h*w)); hb = ceil(h*beta/28)*28; wb = ceil(w*beta/28)*28
  return (hb, wb)
img = PIL-resize(crop, (wb, hb), BICUBIC)                 # §4.1
x = (u8/255 - 0.5)/0.5                                    # f32
gh = hb/14 ; gw = wb/14      (both even, ≥ 2)
pixel_values[p = r*gw + c][ch][iy][ix] = x[r*14+iy][c*14+ix][ch]    # (N=gh*gw, 3, 14, 14), raster order
```
Grid sizes match the HF processor on 40 crops; values differ from HF's torchvision path by ≤ 2 LSB on
0.2% (§4.1).

### 6.4 Vision auxiliary inputs (`pipeline.py:32-51`; validated against transformers' own functions, `validate_aux.py`)
For patch p = (r, c), `SIDE = 27`:
```
src(pos, n) = n > 1 ? pos*(27-1)/(n-1) : 0          # f64, bilinear align_corners=True
i0 = clamp(floor(src), 0, 26) ; i1 = clamp(i0+1, 0, 26) ; f = src - floor(src)
pos_idx[p] = [r0*27+c0, r0*27+c1, r1*27+c0, r1*27+c1]                     # i64
pos_w[p]   = [(1-fr)(1-fc), (1-fr)fc, fr(1-fc), fr·fc]  (f64 → f32)
inv[i] = 1/10000^(2i/36), i = 0..17 (f32)
angle[p] = [r·inv (18), c·inv (18)] ; angle72 = [angle, angle]
cos[p] = cos(angle72), sin[p] = sin(angle72)                              # (N, 72) f32
hb = gh/2 ; wb = gw/2
merge_idx = for (bi, bj) in raster(hb, wb):
   [(2bi)gw+2bj, (2bi)gw+2bj+1, (2bi+1)gw+2bj, (2bi+1)gw+2bj+1]           # (N,) i64
```

### 6.5 Vision graph `vision.onnx` (opset 21; `export.py:36-48`, `wrappers.py:8-39`)
| name | fp32 | fp16 file | shape |
|---|---|---|---|
| in `pixel_values` | f32 | **f16** | (N, 3, 14, 14) |
| in `pos_idx` | i64 | i64 | (N, 4) |
| in `pos_w` | f32 | f32 | (N, 4) |
| in `cos`, `sin` | f32 | f32 | (N, 72) |
| in `merge_idx` | i64 | i64 | (N,) |
| out `image_embeds` | f32 | **f16** | (N/4, 1024) |
One image per call (full attention across that image's patches; no cross-image batching in this graph).
N dynamic 16..8192. Output rows are in `merge_idx` block order = image-token order.

### 6.6 Prompt and sequence assembly (`common.py:44-47`; `spike/paddle/prompt_ids.npy`)
Chat template with one user turn `[image, "OCR:"]` and generation prompt:
`<|begin_of_sentence|>User: <|IMAGE_START|><|IMAGE_PLACEHOLDER|>×n<|IMAGE_END|>OCR:\nAssistant:\n`.
Token ids (constant):
```
PREFIX = [100273, 2969, 93963, 93919, 101305]          # <|begin_of_sentence|> "User" ":" "▁" <|IMAGE_START|>
SUFFIX = [101306, 93972, 2497, 93963, 23, 92267, 93963, 23]   # <|IMAGE_END|> "O" "CR" ":" <0x0A> "Assistant" ":" <0x0A>
ids = PREFIX + [100295]*n + SUFFIX,   n = (gh/2)*(gw/2)
e = embed[ids] (f16 → f32) ; e[5 .. 5+n] = image_embeds
```

### 6.7 M-RoPE positions (`pipeline.py:53-63`, `:79-86`)
Per row (positions count from its first real token; padding positions are 0 and masked):
```
prefix tokens j = 0..4:          (t,h,w) = (j, j, j)
image token (bi, bj):            (5, 5+bi, 5+bj)
suffix token m = 0..7:           all three = 5 + max(hb, wb) + m
base = max position + 1 = 13 + max(hb, wb)
generated token g (0-based):     fed at (base+g, base+g, base+g)
inv[i] = 1/500000^(2i/128), i = 0..63 (f32)
angle[0..16]  = t·inv[0..16] ; angle[16..40] = h·inv[16..40] ; angle[40..64] = w·inv[40..64]
cos/sin (…, 128) = cos/sin([angle, angle])            # rotate-half: q' = q·cos + rot_half(q)·sin
```

### 6.8 Batch layout and attention bias (`pipeline.py:87-98`, `:110-116`)
- Batch of B rows, S = max row length; **left padding**: row content at `[S-n, S)`; pad slots get
  `embed[0]` and position 0.
- Prefill bias (B, 1, S, S): `allow[i][j] = (j <= i) && valid[j]`, plus `allow[i][i] = true` (keeps padded
  query rows finite); bias = 0 if allowed else `NEG` where `NEG = finfo(dtype).min`
  (−3.4028235e38 for fp32, −65504 for fp16).
- Decode step: bias (B, 1, 1, T) over all keys so far: padding keys `NEG`, everything else 0.

### 6.9 Decoder graph `decoder.onnx` (opset 21; `export.py:49-60`, `wrappers.py:41-71`)
Inputs in order: `inputs_embeds` (B, S, 1024), `cos` (B, S, 128), `sin` (B, S, 128), `bias` (B, 1, S, T),
then `past_k_0, past_v_0, …, past_k_17, past_v_17` each (B, 2, P, 128).
Outputs: `logits` (B, 103424) **f32 in every variant** (last position only), then
`present_k_0, present_v_0, …, present_v_17` each (B, 2, P+S, 128).
fp16 file: all inputs/KV f16. fp32/int8 files: f32. Prefill passes P = 0.

Greedy loop with per-row caps (`pipeline.py:100-117` + runner truncation `ER:1913-1919`):
```
max_new = max(cap_i in batch)
logits, past = decoder(x, cos, sin, prefill_bias, past = 36 × (B,2,0,128))
out[b] = [] ; done[b] = false
for step in 0 .. max_new-1:
    tok = argmax(logits)
    for b: if !done[b]: if tok[b] == 2: done[b] = true else out[b].push(tok[b])
    if all(done) or step == max_new-1: break
    tok = done ? 0 : tok
    x = embed[tok] (B,1,1024); positions = base_b + step (all three axes)
    logits, past = decoder(x, cos, sin, step_bias, past)
tokens_b = out[b][..cap_b]                          # each row keeps only its own cap (ER:1916-1917)
text_b = detok(tokens_b).strip()                    # fold=False on this road
```
Caps: `token_cap(line_cells)` from the engine stage (12..160); 64 (`DEFAULT_MAX_NEW_TOKENS`) when none
are given. The spike's `Generator` does **not** truncate per row (all caps were 64 there) — the port must.

### 6.10 Detokenizer (base repo `tokenizer.json`)
BPE vocab ids 0..100294 plus 1,041 added tokens (up to 101315). Skip only the **22 ids whose added token
is `special: true`** (0, 1, 2, 100272-…, max 101307); decode the rest through the tokenizer.json decoder
chain: `Replace("▁" → " ")` → `ByteFallback` (`<0xHH>` tokens become bytes; a run of byte tokens that is not
valid UTF-8 yields one U+FFFD per byte) → `Fuse`. Then Python `strip()`.
Notes: (a) the spike used `sentencepiece` and dropped every id ≥ 100256 or ≤ 2 (`run_ort.py:55-56`) —
that also drops the ~1,000 **non-special** added tokens ≥ 100297 and differs from HF; do not copy it.
(b) transformers 5 may rebuild the decoder as `Metaspace` (strips one leading space); after `strip()`
the results are identical. The Rust `tokenizers` crate on this `tokenizer.json` is the simplest correct
implementation.

### 6.11 Batching (`PaddleMangaRecognizer.__call__`, `ER:1881-1895`; `plan_generation_batches`, `ER:1342-1353`)
`areas[i] = w·h` of the crop (as f64); order = indices sorted by `(cap, area, index)`; consecutive
groups of `PADDLE_BATCH = 12` (`ER:738`); results scattered back to input order. The vision graph runs
per crop; the decoder runs per group.

### 6.12 Parity and performance (spike)
- ORT fp32, PIL bicubic host: **100/100** identical texts vs the 0.5.2 torch recognizer (merged, fp32,
  batch 12) on 100 upright line crops from One-Punch Man 20, CER 0/636 (`out_ort_onnx_float32_pil_cpu_b12_t16.json`
  vs `out_torch_merged_float32_cpu_b12.json`, re-checked 2026-10-01). Same with the torchvision pixels.
  fp16: 100/100 (memory notes); int8 (dynamic, per-channel MatMul/Gemm, `quant.py`): 97/100 — not a default.
- Note the parity set used 12%-margin upright crops (adapter-road style, `make_crops.py`), not the
  reconciled road's quad crops; the recognizer path is the same.
- Timing: RTX 4090 fp32 24 ms/crop (torch ~41), fp16 17 ms (torch 19); CPU ≈ torch (1.3-1.6 s/crop at
  16 threads, batch 12); session load 0.7 s (GPU box) / 2.7-5.1 s (CPU box).

---

## 7. Precision, devices, threads

### 7.1 0.5.2 precision semantics (KEEP the user-facing contract; `ER:281-608`)
- Modes: `auto-accuracy` (default; legacy `auto`), `auto-balanced`, `auto-speed`, forced `fp32`/`bf16`/`fp16`
  (`normalize_precision_mode`, `ER:392-403`).
- Policy (`PRECISION_POLICY`, `ER:351-371`):

| engine | accuracy | balanced | speed |
|---|---|---|---|
| hayai-nova | bf16, fp32 | bf16, fp32 | bf16, fp16, fp32 |
| paddle-manga | fp32 | bf16, fp32 | bf16, fp16, fp32 |

- Resolution (`resolve_mode`, `ER:442-480`): forced mode → that format if the device supports it, else
  **refuse to start** with an error containing `precision not available here` (`PRECISION_REFUSAL`,
  `PrecisionUnavailable`, `ER:385-389`; the server treats it as "give the volume back"). Auto modes → the
  candidates the device supports (fp32 always counts as supported); `accuracy` takes the first;
  `balanced`/`speed` take `--precision-pick` if usable, else the first. Logged as
  `[runner] <engine> precision: <fmt> (<why>)`.
- Device support (`supported_formats`, `ER:508-525`): CPU → {fp32}; GPU → {fp32, fp16} + bf16 if
  `torch.cuda.is_bf16_supported()`.
- hayai's precision is an **autocast** dtype over fp32 weights; paddle's is the **weight dtype**.
- Bench precision trials (`precision_phase`, `ER:9052-9110`): each usable candidate is run once; fastest wins,
  ties within 5% (`PRECISION_TIE`) go to the earlier (more accurate) candidate (`pick_precision`, `ER:483-505`).
- The resolved precision is recorded in the sidecar (`ocr_engine.precision`).

### 7.2 What changes with ONNX (CHANGE; see Q4)
Exported formats: hayai fp32, fp16; paddle fp32, fp16, int8. **No bf16 graph exists**, and ORT bf16
support is EP-specific. Consequences: 0.5.2 hayai on a GPU defaults to **bf16 autocast** (its
`auto-accuracy` first candidate); the ONNX port cannot reproduce that, and fp32/fp16 read ~47 lines per
10,000 differently from bf16 (study table, `ER:313-317`). Proposed mapping until decided: supported set =
{fp32} on CPU EP, {fp32, fp16} on GPU EPs; bf16 treated as unsupported (so auto modes fall to the next
candidate; forced bf16 refuses). Switching precision = selecting a different session (fp32 vs fp16 file);
nothing is re-cast.
Also note the hayai fp16 file is a full-graph conversion (onnxconverter-common 1.16, no Cast nodes —
RMSNorm `Pow`/`ReduceMean` run in f16), unlike torch autocast which keeps norms in f32.

### 7.3 Devices (`ER:2355-2456`, `road_specs` `ER:2808-2872`, `OpenPipeline` `ER:6475-6735`)
- Public device ids: `auto`, `cpu`, `gpu:<n>` (n ≤ 15); `gpu`/`cuda` → `gpu:0`; `cuda:<n>` accepted
  (`parse_device`). `auto` = `gpu:0` if the host has a card, else `cpu`. The PP-OCR stages are CPU-only
  whatever is asked. `--stage-device engine=gpu:1` places the recognizer; the resolved placement is
  reported in `ready.stage_device`.
- The engine stage is device-bound (one worker per model). `--stage-workers engine=N` on a GPU loads N
  copies (max 8, `MAX_ENGINE_COPIES` `ER:5179`), ignored on CPU (`_engine_copies`, `ER:6655-6667`).
  **CHANGE:** N copies → N engine threads over one shared session (ORT `run` is thread-safe).
- **CHANGE**: device → ORT execution provider. Spike-tested: CPU EP; CUDA EP (with IoBinding keeping the
  KV cache on device, logits bound to CPU — `spike/hayai/ort_be.py:26-46`, `spike/paddle/run_ort.py:26-36`);
  TensorRT EP tried for hayai. AMD: ROCm EP is gone from ORT ≥ 1.23; WebGPU EP ran on an RX 9070 XT
  (Mesa Vulkan) in the runtime spike (memory note). Mapping is open (Q5).
- Session options used by the spike: `graph_optimization_level = ORT_ENABLE_ALL`, `intra_op_num_threads`
  set explicitly when benchmarking, `inter_op_num_threads = 1`. The CUDA-EP "~600 ms per new input
  shape" cost seen in the runtime spike was the PP-OCR conv recognizer (cuDNN algo search); neither
  recognizer graph here has convolutions (patch embeddings are MatMuls).
- torch-specific thread pinning (GPU recognizer limited to 4 CPU threads) is dropped; the equivalent knob
  is the ORT intra-op thread count of a GPU-placed session (small) vs a CPU-placed one (all cores ÷ jobs).

---

## 8. ONNX files on disk now, and what still has to be exported

Measured 2026-10-01 (`ls -la`, `onnx.load(load_external_data=False)`):

| path (under `~/.cache/mokuro-bunko-demo/onnx-spike/`) | bytes | dtype | producer |
|---|---|---|---|
| `hayai/onnx/nova_vision.onnx` | 350,488,395 | fp32, single file | torch 2.14 dynamo, opset 20 |
| `hayai/onnx/nova_vision_fp16.onnx` | 175,514,092 | fp16 (I/O fp16 except `gather_idx` i64) | onnxconverter-common 1.16 conversion |
| `hayai/onnx/nova_decoder.onnx` | 216,530,709 | fp32 | torch 2.14 dynamo, opset 20 |
| `hayai/onnx/nova_decoder_fp16.onnx` | 108,865,607 | fp16 (incl. logits) | onnxconverter-common |
| `hayai/onnx/pos_table.npy` | 786,560 | f32 (256, 768) | |
| `hayai/onnx/token_embeddings.npy` | 32,776,320 | f32 (16004, 512) | |
| `paddle/onnx_float32/vision.onnx` + `.data` | 1,329,930 + 1,757,216,768 | fp32 | torch 2.14 dynamo, opset 21 |
| `paddle/onnx_float32/decoder.onnx` + `.data` | 1,267,049 + 1,443,037,184 | fp32 | " |
| `paddle/onnx_float16/vision.onnx` + `.data` | 1,405,746 + 883,949,568 | fp16 pixels/out; f32 aux inputs | exported from an fp16 model |
| `paddle/onnx_float16/decoder.onnx` + `.data` | 1,349,090 + 721,551,360 | fp16; logits f32 | " |
| `paddle/onnx_int8/vision.onnx` + `.data` | 2,579,729 + 444,721,600 | dynamic int8 MatMul/Gemm, f32 I/O | ORT `quantize_dynamic` |
| `paddle/onnx_int8/decoder.onnx` + `.data` | 2,686,684 + 360,861,696 | " | " |
| `paddle/onnx_*/embed.npy` | 211,812,480 each | f16 (103424, 1024) | |
| `paddle/prompt_ids.npy` | 240 | `[5, prefix…, suffix…]` | |

All IR version 10. No single file exceeds 2 GB (the fp32 paddle vision data is 1.76 GB).

Still to do (for `tools/onnx_export/`, ARCHITECTURE §7):
1. **Reproducible re-export** from the pinned revisions with recorded export ids/sha256. The spike scripts
   import the runner from the `pr-0.5.0` worktree and depend on hand-written functional copies of the
   models (`nova_modules.py`, `wrappers.py`) — re-verify on any pin bump. `hayai/export.py` currently has
   the decoder export disabled (`0 and …`, line 28); the decoder on disk is from an earlier run of the same
   code. The hayai fp16 conversion step is not in any script.
2. Decide/produce the **bf16 story** (Q4) or accept the policy change.
3. Optionally fold host tables into graphs: hayai position-table resize and token embedding; paddle
   `embed.npy` (f16; consider f32 for bit-faithful fp32, +212 MB) — see Q6.
4. Optional: a **batched** paddle vision graph (block-diagonal attention over several images) for GPU
   throughput; today one call per crop.
5. Tokenizer assets: hayai `tokenizer.json`, paddle `tokenizer.json`; store `.npy` tables as safetensors or
   raw little-endian for Rust.
6. Parity suites to re-run on the exported files from the **Rust** host: hayai 220 line crops (with some
   chunked lines added), paddle 100 crops plus a quad-crop set from the reconciled road, fp32 and fp16, CPU and
   one GPU EP.
7. PP-OCR det/rec are already ONNX upstream (`Kellenok/PP-OCRv6_manga@ba1d479…`, `ppocr.py:79-83`) — no export.

---

## 9. Open questions

- **Q1 Parity bar.** Is "identical texts on the parity sets" the acceptance criterion (what the spike
  achieved), or must Rust crops/pixels be bit-identical to 0.5.2? Bit-identity is not reachable without
  OpenCV 5.0's warp kernels (§3.4) and Pillow's decoders (§2.2).
- **Q2 OpenCV.** Port OpenCV 5.0 `warp_kernels.simd.hpp` exactly, link OpenCV (Apache-2.0, heavy for
  Android/Windows packaging), or accept the float reimplementation (±1 LSB on <0.1% of values)?
- **Q3 Image decoding.** Which decoders (libjpeg-turbo with Pillow's settings vs `zune-jpeg`/`image`)? JPEG
  IDCT/upsampling differences change every crop slightly; a Pillow-vs-Rust page-level text diff is needed.
- **Q4 bf16.** hayai-nova on GPU ran bf16 autocast by default in 0.5.2 and that was judged its most
  accurate format. Export a bf16 graph (EP support?), keep fp32 as the new default, or change the policy
  table? Existing sidecars say `precision: bf16`; does a generation upgrade treat fp32 as a different
  precision?
- **Q5 GPU EPs.** Which EP per platform (CUDA, DirectML, CoreML, WebGPU for AMD on Linux)? What does
  "fp16 supported" mean per EP for the precision probe? How is `gpu:<n>` mapped for EPs without device ids?
- **Q6 Embedding precision.** paddle `embed.npy` is f16 even for fp32 (texts still matched 100/100). Ship
  f16 (212 MB) or f32 (424 MB)? Fold lookups into the graphs?
- **Q7 Batching.** Keep hayai's fixed 16-in-order chunks and paddle's (cap, area) groups of 12 exactly,
  or retune for Rust (dynamic batching across pages)? Changing them is safe for fp32 hayai (masked) but
  not proven for paddle (left padding + fp16).
- **Q8 int8 paddle.** 97/100 parity — offer as an explicit speed mode on CPU, or never?
- **Q9 Provenance.** What goes in `ocr_engine.weights`/`recognizer` for ONNX models (source revisions +
  export id + sha256)? Should `precision` record the session file's dtype?
- **Q10 Transport.** With the subprocess gone, does any consumer still need the exact `stats`/`bench_*`
  JSON shapes, or only the scheduler's typed equivalents? (`ocr/pipeline_stats.py` re-exports the reading.)
- **Q11 Page order.** `natsort.natsorted` on `Path` objects vs the `_natural_key` fallback (lower-cases)
  can order differently; which exact rule must the Rust archive reader implement? (shared with the
  archive spec)
- **Q12 hayai fp16 file.** Full-f16 RMSNorm (`Pow` in f16) can overflow on large activations; re-export
  with norms kept in f32 (op block list) before shipping fp16?

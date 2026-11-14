# Models: the `models-v1` artifact set

How the `models-v1` ONNX files and host tables are produced, checked, described and
published. In 0.7 the hayai-nova and paddle-manga recognizers run compiled libtorch
packages from a separate release, `torch-models-v1` (TORCH-BACKEND.md). Of `models-v1`
the shipped build uses only the PP-OCR files and the recognizers' host files (tokenizers,
hayai-nova's pos table and token embeddings, paddle-manga's embeddings for packages
without them); the hayai/paddle ONNX graphs serve only the unshipped `onnx-vlm`
recognizers. ARCHITECTURE.md §7 states the policy; `spec/ocr-recognizers.md` §5/§6/§8
defines the graphs' inputs and outputs and the host-side arithmetic around them.
The tool is `tools/onnx_export/` (Python, dev-only, never on users' machines).

## 1. What is in the release

One GitHub release, tag `models-v1`, flat file names, every file < 1.9 GB (GitHub
caps an asset at 2 GiB; larger external-data files would be split into
`<name>.onnx.data`, `.data.1`, … automatically — none needs it today).

| file | what | dtype / shape |
|---|---|---|
| `hayai-nova_vision_fp32.onnx` / `_fp16.onnx` | SigLIP2 NaFlex tower + DSCProjector (spec §5.6), single file | fp16 file: all float I/O f16, `gather_idx` i64 |
| `hayai-nova_decoder_fp32.onnx` / `_fp16.onnx` | 12-layer decoder, last-position logits, explicit KV in/out (§5.9) | fp16 file: all float I/O f16 incl. logits |
| `hayai-nova_pos_table.npy` | position table | f32 (256, 768) |
| `hayai-nova_token_embeddings.npy` | input token embeddings | f32 (16004, 512) |
| `hayai-nova_tokenizer.json` | the model repo's tokenizer, byte for byte | |
| `hayai-nova_config.json` | special ids (bos 16001, eos 16002, pad 16000), `skip_ids`, loop constants | |
| `paddle-manga_vision_{fp32,fp16}.onnx` + `.onnx.data` | vision tower + projector, one image per call (§6.5) | fp16 file: `pixel_values`/`image_embeds` f16, aux inputs f32/i64 |
| `paddle-manga_decoder_{fp32,fp16}.onnx` + `.onnx.data` | ERNIE-4.5 decoder, KV in/out (§6.9) | logits f32 in both; fp16 file: other float I/O f16 |
| `paddle-manga_embed_fp32.npy` / `_fp16.npy` | input token embeddings | f32 / f16 (103424, 1024) |
| `paddle-manga_tokenizer.json` | the base repo's tokenizer, byte for byte | |
| `paddle-manga_config.json` | prompt prefix/suffix ids, image token 100295, eos 2, pad 0, `skip_ids`, constants | |
| `ppocr-manga_det_v0.2.onnx`, `ppocr-manga_rec_v0.2.onnx`, `ppocr-manga_dict.txt` | PP-OCRv6 manga detector / CTC recognizer / dictionary, copied as-is | |

`models.json` (the manifest, §4) is written beside the exports but is not a release
asset; the program checks downloads against a table generated from it
(`crates/bunko-ocr/src/models_release.rs`).

`.npy` files are NumPy v1.0 format, little-endian, C order (a 128-byte header then the raw
array) — trivial to read from Rust (`ndarray-npy`, or skip the header).

Every exported graph carries `metadata_props` an ONNX session can read at load:
`mokuro.export_id` (e.g. `models-v1/hayai-nova/decoder/fp16`), `mokuro.export_tool_version`,
`mokuro.engine`, `mokuro.role`, `mokuro.precision`, `mokuro.sources` (JSON
`{repo: revision}`) and, for fp16, how it was made. Q9 (sidecar provenance) can use these
directly.

## 2. Sources, pins, licences, attribution

The pins of the exported models equal 0.5.2's (`REPO_REVISIONS` in
`src/mokuro_bunko/ocr/engine_runner.py`); every export step re-parses that file and
refuses to run if `tools/onnx_export/onnx_export/pins.py` disagrees. The PP-OCR files are
no longer the ones of this release: since 0.8 the program reads with PP-OCRv6 manga v0.3,
fetched from its repository (§2.1).

| repo | revision | licence (model card at that revision) | used for |
|---|---|---|---|
| `JustANormalTinkerer/hayai-ocr-v2.5-nova` | `e46d79138499600564f810d44ab6bdea7230dee1` | Apache-2.0 | hayai-nova weights, tokenizer |
| `google/siglip2-base-patch16-naflex` | `b53b807d3a2d5e2b3911292f2d69e5341cdc064c` | Apache-2.0 | hayai-nova preprocessing definition (no weights shipped from it) |
| `PaddlePaddle/PaddleOCR-VL-1.6` | `c5630abae1d940eafe0697512a0325494b02ab42` | Apache-2.0 (card + `LICENSE` file) | paddle-manga base weights, tokenizer |
| `sorryhyun/paddleocr-vl-1.6-manga-lora` | `26292839d1469c14212a12a1e01b5b1fe01bff15` | Apache-2.0 | LoRA (merged) + fine-tuned vision tower |
| `Kellenok/PP-OCRv6_manga` | `ba1d479e8a61a20e8318c9758c73fbbbd290b98d` (v0.2) | Apache-2.0 | PP-OCR det/rec/dict as copied into `models-v1`; read by 0.7 only |

Licences were checked 2026-10-01 with `HfApi().model_info(repo, revision=<pin>).card_data`
(all `license: apache-2.0`) and by reading the cards. Apache-2.0 permits redistribution of
the weights and of derived (converted, merged) weights; obligations when publishing:

- Ship the Apache-2.0 licence text and a NOTICE-style attribution with the release (put
  both in the release description, or add `LICENSE-models.txt` as an asset). State that
  the files are **modified**: converted to ONNX, fp16-converted, and for paddle-manga the
  LoRA merged into the base and the LoRA's vision tower substituted.
- Attribution lines to include:
  - hayai-ocr-v2.5-nova by JustANormalTinkerer (Apache-2.0); vision tower from Google's
    SigLIP2 (Apache-2.0).
  - PaddleOCR-VL-1.6 by PaddlePaddle / Baidu (Apache-2.0), with the manga LoRA + tower by
    sorryhyun (Apache-2.0).
  - PP-OCRv6 manga by Kellenok (Apache-2.0), fine-tuned from PaddlePaddle's
    PP-OCRv6_tiny_det / PP-OCRv6_small_rec.
- Training-data notes from the cards (no obligation on us beyond attribution): the manga
  LoRA, hayai and PP-OCRv6_manga list **Manga109-s** (and COO) among their training data.
  The LoRA card states: "Use of the training data is governed by the Manga109-s terms (no
  redistribution of images; results and pretrained models may be published with
  attribution)". We redistribute no images. Keep the Manga109 attribution/citation
  pointer in the release notes (the cards carry the BibTeX).

Nothing in the `models-v1` and `torch-models-v1` releases is GPL; all of it is fine for
app-store builds.

### 2.1 Models fetched from their authors' repositories

These are not exported: the program downloads each file as its authors published it
and checks it against the sha256 compiled into `crates/bunko-ocr/src/models.rs`. All
but one come straight from the pinned source; AnimeText's repository is gated, so that
file is mirrored (§2.2). The licence class decides which builds know a model at all
(`bunko_core::engines::LicenseClass`; Cargo features `gpl-models`, `nc-models`).

| model | source and pin | files | weights' licence | class |
|---|---|---|---|---|
| PP-OCRv6 manga v0.3 (every engine's line detector and CTC reader) | `Kellenok/PP-OCRv6_manga` @ `d65838996a00ef6b1c73ca3a700d7a8c5fbbef8a` | `det/manga_det.onnx` 1,816,958 B, `rec/manga_rec.onnx` 21,334,702 B, `ppocrv6_dict.txt` 76,327 B | Apache-2.0 | permissive |
| `ctd-lines` detector | the `comictextdetector.pt.onnx` asset of `zyddnys/manga-image-translator` release `beta-0.2.1` (the detector's authors' ONNX export); the same bytes at `mayocream/comic-text-detector-onnx` @ `a5d67ec772adef819ef5b0e7aa701fcf4c8bf74a` | 94,669,756 B | GPL-3.0 | `gpl-models` |
| `animetext-blocks` detector | `deepghs/AnimeText_yolo` @ `a180c191bfdb9f0e31b57e7de567e7b6bac50f84`, `yolo12x_animetext/model.onnx`; fetched from our `upstream-models-v1` release (§2.2) | 236,662,175 B | GPL-3.0 | `gpl-models` |
| `charspot-small` engine | `muscgab/manga-ocr-charspot-small` @ `72ca29202673880ca9c67b1c02c107535be4472b` | `model.onnx` 43,992,111 B, `vocab.json` | CC BY-NC-SA 4.0 (code Apache-2.0) | `nc-models` |
| `nar-small` engine (the `r16` model) | `muscgab/manga-ocr-nar-small` @ `8cc14c152ccb21da3aa88fa51a92766537c12da2` (branch `r16`); the graph from our `textbox-graphs-v1` release (§2.3) | `model.safetensors` 217,035,140 B (sha256 `bc7418e2…20a3`), `nar_vocabulary_6817.json`; our `nar-small_r16_graph.onnx` 714,861 B | CC BY-NC-SA 4.0 (code Apache-2.0; our graph Apache-2.0) | `nc-models` |

What each licence means for us:

- **comic-text-detector (GPL-3.0).** We distribute neither its weights nor its code. The
  program fetches the authors' own file when an admin picks the detector. Only the
  model's line map is used, through the DBNet post-processing PP-OCR already has here:
  `crates/bunko-ocr/src/ppocr/ctd.rs` is written against the model's tensors and holds
  nothing of comic-text-detector's source (its block grouping and ordering are not
  reproduced; Bunko's own layout does that).
- **AnimeText YOLO12-x (GPL-3.0).** The weights' licence allows passing them on, and
  §2.2 does that, unmodified, with the licence text and the source. The code that runs
  the model (`crates/bunko-ocr/src/animetext.rs`: scaling, the candidate decoding,
  overlap suppression) is written against the model's tensors. Its card names the
  AnimeText dataset (CC BY-NC-SA 4.0) as training data.
- **muscgab's recognizers (weights CC BY-NC-SA 4.0).** Non-commercial use, attribution,
  share alike. We do not rehost the weights. The inference code they are published with
  is Apache-2.0 and `crates/bunko-engines/src/textbox.rs` follows its pre- and
  post-processing. Their cards name AnimeText-OCR (CC BY-NC-SA 4.0) as real training data.
  nar-small's `r16` model is published as PyTorch weights only; the ONNX graph Bunko runs
  it with is ours, made from the author's Apache-2.0 model code, and holds no weights
  (§2.3). So do the compiled libtorch graphs of its GPU route (§2.4).
- A build made with `xtask dist --permissive-models` has none of the rows of the last
  four lines and none of the code behind them.

### 2.2 The `upstream-models-v1` release

`deepghs/AnimeText_yolo` can only be downloaded by an account that accepted its terms,
which a server cannot do for its admin. Its ONNX file is therefore mirrored, byte for
byte, as an asset of the GitHub release `upstream-models-v1`
(`bunko_ocr::models::MIRROR_RELEASE`), with `NOTICE.md` (source, revision, size,
sha256) and the GPL-3.0 text beside it. A later mirrored file, or another revision of
this one, is a new asset name or a new release, never a changed asset.

How it was made, for the next file:

```sh
cd <the folder with animetext_yolo12x.onnx, NOTICE.md, LICENSE-GPL-3.0.txt, RELEASE-NOTES.md>
gh release create upstream-models-v1 --repo Gnathonic/mokuro-bunko \
   --title "Upstream models (mirror) v1" --notes-file RELEASE-NOTES.md \
   --prerelease --latest=false animetext_yolo12x.onnx NOTICE.md LICENSE-GPL-3.0.txt
```

**Published** 2026-11-10 as a pre-release (never "latest"), with those three assets. An
anonymous download gives back the sha256 above, and a build with empty storage and no
mirror fetched the file from it and read a volume. The tag starts no release build
(`release.yml` runs on `v*`). An air-gapped host serves the file itself with
`MOKURO_MODELS_MIRROR=<folder>`, under its flat name `animetext_yolo12x.onnx`.

Checked 2026-11-09 against the model cards and licence files at those revisions. The
Rust host of the two recognizers was compared with their own Python packages on 200
text boxes: identical text for both, and character boxes within 0.001 px (nar-small's
first release; for `r16` see §2.3).

### 2.3 The `textbox-graphs-v1` release: graphs without weights

nar-small's author publishes the `r16` model (161 output slots, at most 160 characters;
the first release had 65) as `model.safetensors` with PyTorch code, and no ONNX file.
`tools/textbox_export/export_nar.py` makes one that contains **no trained weights**:

- it runs the author's own `model.py` at the pinned commit (Apache-2.0), with the RMSNorm
  eps values of the commit's `config.json` (`reparam.norm_eps`), and traces it with the
  author's `prepare()` step inside the graph, so every value derived from the weights is
  computed at run time;
- it exports without constant folding and without merging initializers of equal value
  (the exporter would fold the three untrained, all-ones `cond.{1,2,3}.s3` into
  `cond.0.s3`), then turns each of the 181 initializers into an ONNX external-data entry:
  location `model.safetensors`, offset 8 + header length + the tensor's start. The file's
  other 4 tensors (`inject`, `loop_emb`, `occ.*`) are not read by the code;
- it refuses to write the graph unless every initializer maps onto one safetensors
  tensor of the same type and shape, the inputs and output are `P [324, 588]`, `V [324]`
  float32, `R`, `C [324]`, `H`, `W [1]` int32 → `ids [161]` int64, and an export of the
  same network with every parameter replaced by random numbers gives the same bytes:
  nothing in the graph depends on the trained values. What it holds besides the
  network is 358 KB of rotary position tables computed from the code.

The output is byte-reproducible: `nar-small_r16_graph.onnx`, 714,861 B, sha256
`d630455db967a243ca70e887db5a96866bbd6e052e56f27297495aa136159948` (two runs, export
environment of `tools/onnx_export/pyproject.toml`: torch 2.14.0, onnx 1.23.1).
`check_parity.py` then runs it on ONNX Runtime (CPU) with the weights handed over from
memory, against the author's PyTorch code in fp32 on the first 200 JMangaBench_Read
crops: identical ids on 200/200. Bunko's own preprocessing and runtime give the same
ids on the same 200 (the ignored test `textbox::tests::nar_parity_with_the_authors_code`).

Bunko fetches the graph from our pre-release `textbox-graphs-v1`
(`bunko_ocr::models::TEXTBOX_RELEASE`) and the weights and vocabulary from the author's
repository at the pin, each checked against its sha256. ONNX Runtime gets the weights
from memory under the name the graph gives them
(`bunko_ocr::runtime::Model::load_with`), so the file may sit under any name or folder
(the store keeps it as `nar-small/r16_model.safetensors`; an override folder or
`MOKURO_MODELS_MIRROR` may hold it as `nar-small_r16_model.safetensors`). The first
release's 213 MB ONNX file is no file of the manifest any more, so a store prune
removes it, and `nar-small` output is at engine revision 2.

The release's assets: the graph, `NOTICE.md` (source, commit, the weights' sha256 and
size, "contains no trained weights", both licences) and `nar-small_r16_LICENSE` (the
author's Apache-2.0 text), all written by `export_nar.py`. Publishing, as for §2.2:

```sh
cd <the folder export_nar.py wrote>
gh release create textbox-graphs-v1 --repo Gnathonic/mokuro-bunko \
   --title "Text-box recognizer graphs v1" --notes-file NOTICE.md \
   --prerelease --latest=false nar-small_r16_graph.onnx NOTICE.md nar-small_r16_LICENSE
```

A changed graph is a new asset name or a new release, never a replaced asset.

### 2.4 The `textbox-torch-v1` release: nar-small on a GPU

On an NVIDIA or AMD card with the libtorch backend pack, nar-small reads through a
compiled AOTInductor package of the same `r16` network instead of the ONNX graph:
`torch/nar-small/fp32/<target>/model.pt2`, one per GPU target, made by
`tools/torch_export` (`python -m torch_export build -e nar-small -p fp32 -t <target>`). Like the
ONNX graph it holds **no trained weights**: the exporter refuses to embed them, the
package's 181 constants are bound at load from the author's own `model.safetensors`
(the same file, at the same pin, the CPU route reads), and `verify` and
`publish_textbox.sh` refuse any package whose constants live inside it. The cdylib reads
a page's blocks in one batched call (`bt_read_chars`), with the same preprocessing code
as the CPU route (`bunko_vlm::textbox`), at fp32 with TF32 off, and gives the same ids
as the author's fp32 PyTorch code: a target ships only once all 7,929 JMangaBench_Read
crops read the same text on a card of that target (or, for a target with no card to
test, on its verified neighbours), otherwise that GPU keeps the CPU route.

Beside it, `torch/nar-small/fp16/<target>/model.pt2` is the author's own fp16 mode
(`net.to(torch.float16)`, as their `PyTorchBackend(fp16=True)` runs it), compiled the same
way and bound to the **same** fp32 file: the package says `bunko.weights_dtype = "fp16"`
and the cdylib casts the file's floating tensors to fp16 on the card as it loads them, so
no fp16 copy of the weights exists anywhere. Every auto precision mode of a `nar-small`
row reads with it on a card that has one; a row set to `fp32` reads with the fp32
package; the CPU always reads at fp32. An fp16 package ships for a target only once its
texts differ from the author's fp16 and from fp32 on at most 0.1% of the 7,929 crops and
it shows no paired JMangaBench_Read loss against fp32. The details
are in [TORCH-BACKEND.md](TORCH-BACKEND.md#single-graph-engines) and
[TEXTBOX-GPU.md](TEXTBOX-GPU.md).

The packages are published once as the pre-release `textbox-torch-v1` with a `NOTICE.md`
and the author's Apache-2.0 `LICENSE`; the compiled-in manifest
(`crates/bunko-ocr/src/textbox_torch_models.json`) lists them, and a sidecar read on
the GPU names `textbox-torch-v1` as its export set where one read on the CPU names
`textbox-graphs-v1`. Neither changes the engine revision: at fp32 the ids are the same,
and fp16 is recorded as `ocr_engine.precision = "fp16"`, which re-reads nothing.

## 3. The pipeline (`tools/onnx_export/`)

```
tools/onnx_export/
  pyproject.toml              exact pins of the export stack ([export] extra), CPU torch index
  onnx_export/
    pins.py                   source repos/revisions/licences + check against the 0.5.2 runner
    common.py                 flat-name ONNX writer (external data, split at 1.9 GB, metadata), sha256, ORT helpers
    fp16.py                   fp32->fp16 conversion keeping RMSNorm chains + Softmax fp32; activation audit
    hayai/  modules.py export.py host.py check_parity.py
    paddle/ modules.py export.py host.py check_parity.py
    ppocr.py                  copies the PP-OCR files from the pin
    build_manifest.py         writes models.json
    __main__.py               python -m onnx_export {hayai,paddle,ppocr,parity-hayai,parity-paddle,manifest,all}
```

### Environment

```sh
export TMPDIR=~/.cache/mokuro-bunko-demo/tmp            # /tmp is a small RAM disk here
export UV_PROJECT_ENVIRONMENT=~/.cache/mokuro-bunko-demo/export-env
cd tools/onnx_export && uv sync --python 3.14 --extra export
```

(models-v1 was produced with CPython 3.14.3 free-threaded (`3.14t`, what `uv` picked),
torch 2.14.0+cpu, transformers 5.17.0, peft 0.21.1, onnx 1.23.1, onnxscript 0.7.2,
onnxruntime 1.30.0, onnxconverter-common 1.16.0, numpy 2.5.3, Pillow 12.3.0 — the pins in
`pyproject.toml`. No GPU is needed. Disk: 6.4 GB of artifacts + 5.3 GB of `_stage/`
(deletable). Wall clock on a Ryzen 9 7950X: hayai export ~1 min, paddle export ~10 min,
hayai parity ~5 min (+4 min with `--audit`), paddle parity ~45 min.)
Weights come from the Hugging Face cache (`HF_HUB_OFFLINE=1` works once cached).

### Steps

```sh
P=~/.cache/mokuro-bunko-demo/export-env/bin/python
$P -m onnx_export all            # = the six steps below, stopping at the first failure
$P -m onnx_export hayai          # export hayai-nova fp32, convert fp16, write tables
$P -m onnx_export paddle         # export paddle-manga fp32 and fp16, write tables
$P -m onnx_export ppocr          # copy PP-OCRv6 manga files
$P -m onnx_export parity-hayai   # gate: 220/220 at fp32 (exit 1 otherwise); fp16 reported
$P -m onnx_export parity-paddle  # gate: 100/100 at fp32, two cap regimes; fp16 reported
$P -m onnx_export manifest       # models.json; refuses without passing parity reports
```

Output goes to `~/.cache/mokuro-bunko-demo/models-v1/` (`--out` to change; never into the
repo). `_stage/` holds raw exporter output and `_parity/` the reference/candidate texts and
the reports; neither is published (the manifest step refuses stray files in the top level).

How each engine is exported:

- **hayai-nova**: the model is loaded with `trust_remote_code` at the pin; `modules.py`
  re-expresses the vision tower + projector and the decoder as functional graphs whose
  data-dependent parts (position-table resize, projector gather, RoPE, masks) are host
  inputs (spec §5.4-§5.8). torch dynamo exporter, opset 20, dynamic batch/patch/token/KV
  dims. The spike had the decoder export disabled; it is exported again here.
- **hayai-nova fp16**: `fp16.py` converts the fp32 graphs with onnxconverter-common,
  keeping every RMSNorm chain (`Pow -> ReduceMean -> Add -> Sqrt -> Reciprocal -> Mul`,
  matched structurally: 49 in the decoder, 1 in the projector) and every `Softmax` in fp32,
  as torch autocast does. See §5 for why.
- **paddle-manga**: loaded exactly like 0.5.2's `PaddleMangaRecognizer` (base in the target
  dtype, LoRA `merge_and_unload`, `tower.safetensors` over the tower, patch conv as matmul
  — the runner's own `linear_patch_embedding`). fp32 and fp16 are separate loads; the fp16
  graphs are exported from the fp16 model (that is 0.5.2's GPU fp16 arithmetic, and
  transformers' RMSNorm already upcasts to fp32 inside). Opset 21, external data.
  The prompt ids are re-derived from the processor's chat template on every export and
  checked against the expected constants.
- **Batched paddle vision graph: not built.** Cross-image batching needs a block-diagonal
  attention mask over all patches of the batch: attention memory grows with (Σ patches)²
  (12 crops × ~600 patches × 16 heads ≈ 3 GB of scores in fp32) instead of Σ(patches²),
  and adding a mask changes the SDPA numerics, so parity would have to be re-earned. The
  vision call is a minority of paddle's time; revisit with GPU measurements.

### Parity (`check_parity.py`)

Reference = the **0.5.2 runner's own recognizer classes**, imported from
`src/mokuro_bunko/ocr/engine_runner.py` of a 0.5.2 checkout (commit `199cff5`) named by
`MOKURO_REF_052` (default: the sibling worktree from
`git worktree add --detach ../ref-0.5.2 199cff5`), torch CPU fp32, `fold=False` (the
un-folded texts the reconciled road consumes). Candidate = the torch-free numpy/PIL host
in `host.py` (the reference implementation for the Rust host) over the exported files on
ORT's CPU EP.
Texts must be identical; CER is reported for information.

- hayai-nova: 220 line crops (`~/.cache/mokuro-bunko-demo/onnx-spike/hayai/crops`, made with
  0.5's `make_line_crop_fn` from One-Punch Man 20 and Dr. Stone 01).
- paddle-manga: 100 upright crops (`…/onnx-spike/paddle/crops`, One-Punch Man 20), read twice:
  default cap 64, and tight per-crop caps (2..15 tokens) under which 32 of the 100 reference
  texts stop at their cap — so the per-row truncation of a batch that runs to its longest
  cap, the (cap, area)-sorted batch grouping, and byte-fallback decoding of a cut-off
  multi-byte character (two tight-cap reference texts end in U+FFFD) are all exercised.

The crop images are derived from copyrighted manga and are not in the repo; `--crops`
points the scripts at another set with the same layout.

## 4. `models.json`

```json
{
 "format": "mokuro-bunko-models/1",
 "release": "models-v1",
 "export_tool": "tools/onnx_export (mokuro-bunko-onnx-export)",
 "export_tool_version": "1.0.0",
 "parity": { "hayai-nova": {"pass": true, "fp32": "220/220 exact, ...", "fp16": "..."}, "paddle-manga": {...} },
 "files": [
  {"engine": "paddle-manga", "file": "paddle-manga_vision_fp32.onnx.data",
   "url": "https://github.com/Gnathonic/mokuro-bunko/releases/download/models-v1/paddle-manga_vision_fp32.onnx.data",
   "size": 1755789760, "sha256": "…", "licence": "Apache-2.0",
   "sources": [{"repo": "PaddlePaddle/PaddleOCR-VL-1.6", "revision": "c5630ab…", "licence": "Apache-2.0"}, …],
   "export_tool_version": "1.0.0",
   "kind": "onnx-external-data", "role": "vision", "precision": "fp32",
   "part_of": "paddle-manga_vision_fp32.onnx"},
  …
 ]
}
```

`kind` ∈ `onnx`, `onnx-external-data` (with `part_of`), `npy`, `tokenizer`, `config`,
`dictionary`. The PP-OCR entries add `"exported": false` and `source_path`
(`repo@revision:path`). The Rust client reads the table compiled from this file (§1), not
the file itself. It fetches a fixed id list per (engine, precision) into
`<storage>/models/`, checks sha256 and only then renames each file into place. `onnx-vlm`
builds fetch the vision and decoder `.onnx` files with their `.data` files, the `npy`
tables and the tokenizer; paddle-manga uses the fp16 embed table at both precisions.
Config files are never fetched. Default (libtorch) builds fetch only the host tables and
tokenizers they need.

## 5. fp16 and the RMSNorm overflow question (spec Q12)

Measured with `parity-hayai --audit` (every float tensor of the fp32 graphs exposed as an
output, max |x| over the 220 crops, batch 4): see the results table in §6. The decoder's
RMSNorm `Pow` (x²) reaches ~36,800 — 56% of fp16's max (65,504) — on ordinary line crops,
i.e. a residual-stream value of ~192 where fp16 x² overflows at ~256. That is too little
headroom to ship a full-fp16 RMSNorm, so the published fp16 hayai graphs keep the RMSNorm
chains and Softmax in fp32 (cost: casts around 50 small reductions; file size +0.1%). The
full-fp16 conversion is still built into `_stage/` with `hayai --fp16-full-variant` for
comparison and is not published.

paddle-manga's fp16 graphs come from the fp16 torch model, whose RMSNorm computes in fp32
(transformers upcasts), so the same concern does not apply.

## 6. Results (models-v1, 2026-10-01)

### Parity (ORT CPU EP vs the 0.5.2 torch recognizer, CPU fp32, `fold=False`)

| engine | set | fp32 (gate) | fp16 |
|---|---|---|---|
| hayai-nova | 220 line crops | **220/220**, CER 0/1172 | 220/220, CER 0/1172 |
| paddle-manga | 100 crops, cap 64 | **100/100**, CER 0/608 | 100/100, CER 0/608 |
| paddle-manga | 100 crops, tight caps | **100/100**, CER 0/520 | 100/100, CER 0/520 |

The unpublished full-fp16 hayai conversion also read 220/220 on this set — the islands are
there for headroom, not because this set overflowed. fp16 on the CPU EP is slower than
fp32 (it exists for GPU EPs); CPU timings from the parity runs (32 threads, Python host,
not a benchmark): hayai fp32 303 ms/crop, fp16 328; paddle fp32 2.5-3.2 s/crop, fp16
3.1-3.4 s/crop. The 0.5.2 torch reference took ≤191 ms/crop (hayai, incl. model load) and 2.0-2.9 s/crop
(paddle) on the same machine, so the CPU-EP Python host is *not* faster than torch here;
performance belongs to the Rust host and its own benchmarks.

### Activation audit (hayai fp32 graphs, max |x| over 220 crops)

| graph | RMSNorm `Pow` (x²) | `ReduceMean` (mean x²) | residual `Add` | LayerNorm out | tensors > 65504 |
|---|---|---|---|---|---|
| decoder | **36,790** | 81.9 | 191.8 | – | 0 |
| vision + projector | 335 | 8.2 | 311.9 | 222 | 0 |

Single residual-stream channels reach |x| ≈ 192 (x² = 36,790 = 0.56 × fp16 max); fp16 x²
overflows at |x| ≥ 256. Hence the fp32 islands (§5).

### Byte reproducibility

A second export of both engines from scratch into a separate directory produced
byte-identical files (all 20 exported files' sha256 equal), so the sha256 values in
`models.json` are reproducible with the pinned stack.

### Files

| file | bytes | sha256 |
|---|---:|---|
| hayai-nova_vision_fp32.onnx | 350,519,569 | `5e3823d76c1826911793c15288850a57ced3d183f82ea9e0493299e917aaa7a4` |
| hayai-nova_decoder_fp32.onnx | 216,611,483 | `d6e4677de36941164bbadb8d10a4ab72d62ae1d199eb635cb7c1777ae339cf22` |
| hayai-nova_vision_fp16.onnx | 175,548,522 | `4ea6f18cc4c5ff49d36f29d4503708a508d573c3efd4f8d1d3b3b673b8bd811f` |
| hayai-nova_decoder_fp16.onnx | 109,020,272 | `3f12a09559086ac478c52b15dee327d1d7c5f9f695255a119798a88fad69773a` |
| hayai-nova_pos_table.npy | 786,560 | `d91476488d133da38b9908b049975173e024d73e16db8b3aba1b213cee298f03` |
| hayai-nova_token_embeddings.npy | 32,776,320 | `9a86508db3783874c4e1e603d4cfd95f98cdee90d95a14dbcfed9ad0bf057efd` |
| hayai-nova_tokenizer.json | 1,247,253 | `f8a0a909c628a684fe463094614e236a8b1d3609e7770f77e7beafaf1056bf13` |
| hayai-nova_config.json | 549 | `9f7457f50fe0a54de26dfa3eb8114bcaa470ff55a0f614ec0599b8caa63d96d0` |
| paddle-manga_vision_fp32.onnx | 1,445,621 | `dc7c05af5cd6ae1cf8f25cd42b09da9a0d3a7424222728ac26cd0c363b8d9e1e` |
| paddle-manga_vision_fp32.onnx.data | 1,755,789,760 | `08d8cd0a1fb1c8baca58678d8689062534e64aca915d51c27f34a219b1c7006c` |
| paddle-manga_decoder_fp32.onnx | 1,370,555 | `573a50b1a066b1708506e6d8bb240a9d7b61f2f9caca94f9e49b710b4546f38f` |
| paddle-manga_decoder_fp32.onnx.data | 1,442,992,128 | `2d1c66ad23a9606fbfada9ede9aa68b59cd3116178564d7cb788cb5e1a08a4cd` |
| paddle-manga_embed_fp32.npy | 423,624,832 | `d7f46e3ea1f07125b088dd37ceeccefc081bc19b22fee747b628e01df5bf43a9` |
| paddle-manga_vision_fp16.onnx | 1,530,186 | `a2d562e218223b629d8963e7a7f72c7dc37e4c8ef3303b1383ba71d196088bef` |
| paddle-manga_vision_fp16.onnx.data | 877,895,744 | `7c6776b7cb98747501caffb9900ecd101fcaa86cd8b0609ec67de26d2fca378a` |
| paddle-manga_decoder_fp16.onnx | 1,457,698 | `5c47509d95dec391d543ae9929a60cb0f5e23a9e1efd28bc4c4e59da92aa545a` |
| paddle-manga_decoder_fp16.onnx.data | 721,496,064 | `7843113600b6552e0ac5c2453f38b71c9311802e8f8378f92444a0c7db58ba18` |
| paddle-manga_embed_fp16.npy | 211,812,480 | `2ebd015d3e28397a956386ca8cc1699b1914aa2aac5ec40436f74bf3e84b7e4e` |
| paddle-manga_tokenizer.json | 11,189,060 | `c8a215a59183d0d0781adc33bacd3ce6162716f7fd568fb30234a74d69803a7d` |
| paddle-manga_config.json | 978 | `322649b1722457bb49cadc5cbb2ccd49a7fdbc1f9f88a243980ffc2b62385ffd` |
| ppocr-manga_det_v0.2.onnx | 1,816,954 | `d132078c46e292b226fb5a2ca52a7612ad319262dfdf493e7d8e3be435295978` |
| ppocr-manga_rec_v0.2.onnx | 21,167,540 | `de12c84c63e62c80339e882e675983d886670dcb6f0147e1ed041afd6fa81888` |
| ppocr-manga_dict.txt | 74,947 | `b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d` |

Total 6,360,175,075 bytes in 23 files (`models.json` is not uploaded); largest 1,755,789,760.

### Not covered yet

- Parity was measured on ORT's **CPU EP only** (no usable GPU on the export machine). The
  fp16 graphs are meant for GPU EPs; re-run on CUDA/DirectML/WebGPU before relying on them.
- The hayai crop set has no line long enough to be chunked; paddle's set uses 12%-margin
  upright crops, not the reconciled road's quad crops (spec §8 item 6). Both belong to the
  Rust-host parity suite.
- No bf16 graphs (spec Q4). No int8 paddle graph (the spike's 97/100; Q8) — add it to
  `paddle/export.py` with ORT `quantize_dynamic` if Q8 is decided "yes".

## 7. Publishing

**Published:** `models-v1` is a GitHub pre-release since 2026-10-09, with the 23 files of
§1. `models.json` itself and a signature were not uploaded: the file table (every size and
sha256) is compiled into the program instead (`crates/bunko-ocr/src/models_release.rs`,
from this release's `models.json`), and the program comes from a signed release
(PACKAGING.md §3), so every download is checked against the binary's own table. The
compiled libtorch packages the recognizers run in 0.7 are a second release,
`torch-models-v1` (`tools/torch_export`; its `torch-models.json` is compiled in the same
way, TORCH-BACKEND.md).

The steps, for the next export:

1. Re-run `python -m onnx_export all` (or check the existing outputs: `manifest` recomputes
   every sha256) and read the parity block in `models.json`.
2. Put the manifest's table into bunko-ocr (`models_release.rs`) in the program release
   that first uses the files; no separate signature is needed (the release manifest
   signs the program that carries it).
3. Create the release and upload every top-level file (not `_stage/`, not `_parity/`):
   ```sh
   cd ~/.cache/mokuro-bunko-demo/models-v1
   gh release create models-v1 --repo Gnathonic/mokuro-bunko --title "OCR models v1" \
      --notes-file /path/to/release-notes.md --prerelease   # notes kept outside this dir
   gh release upload models-v1 --repo Gnathonic/mokuro-bunko \
      $(ls | grep -v '^_' | grep -vx models.json)   # the 23 files
   ```
   The release notes must carry the licence/attribution text from §2. The published
   `models-v1` notes do not yet, and there is no `LICENSE-models.txt` asset; add one or
   the other (`gh release edit models-v1 --notes-file …`).
4. Verify: download a few assets back and compare sha256 with `models.json`
   (`gh release download models-v1 -p 'hayai-nova_*' -D /tmp/check && sha256sum …`).
5. A later re-export with a pin or tool change is a **new** release (`models-v2`), never a
   re-upload under the same tag: sidecars record the export id.

## 8. Accuracy on JMangaBench (2026-11-11)

What the engine choices of 0.8 rest on. **JMangaBench_Read is the primary measure;
JMangaBench_Hard only breaks ties.** Neither benchmark's data nor our harness is in this
repository: the scripts read the benchmarks from their own clones.

**Method.**

- **Read**: `muscgab/JMangaBench_Read` v1.3.3, the whole main set (7,789 boxes; crops from
  the Hugging Face dataset at `ff10b200`, labels and `eval/score.py` from the GitHub repo
  at `e8f2b89`), scored with the benchmark's own scorer. **Composite** is 1 − NED,
  averaged over the printed and the non-printed boxes; **EM** is the share of boxes read
  exactly. The scorer reproduces the published scores from the benchmark's own
  prediction files exactly.
- **Hard**: `muscgab/JMangaBench_Hard` (3,015 boxes, its own scorer), secondary.
- **Tight crop**: the recognizer reads the benchmark's box as it is, the way the
  leaderboard is made. **Pipeline**: each box becomes a page with a 32 px border of its
  own colour and goes through Bunko's whole page pipeline (`ocr_volume`, the detector
  finds the text again). The border alone costs 0.8 to 1.8 points even when the box is
  read as it is, so compare numbers within a column, not across.
- Differences come with a paired bootstrap 95% interval over boxes. Speeds were taken
  on a shared, loaded machine and are not reported.

**Read (primary) and Hard (secondary), Composite / EM %:**

| engine | licence | Read, tight crop | Read, pipeline, `ppocr-manga` detector | Read, pipeline, `animetext-blocks` | Hard, tight / pipeline |
|---|---|---|---|---|---|
| hayai-nova v2.5 (default, patch budget 512) | Apache-2.0 | 96.60 / 88.16 | 94.58 / 86.15 | — | 88.46 / 78.06 |
| paddle-manga v3 | Apache-2.0 | 93.47 / 84.77 (64-token cap) | 94.37 / 86.39 | — | not run |
| ppocr-manga v0.3, its own read alone | Apache-2.0 | — | 94.51 / 87.69 | — | — / 75.88 |
| charspot-small | CC BY-NC-SA 4.0 | 97.17 / 92.37 | 93.72 / 85.95 | 95.08 / 87.98 | 89.80 / 75.89 |
| nar-small, first release | CC BY-NC-SA 4.0 | 97.28 / 91.36 | 94.84 / 88.27 | 96.02 / 90.08 | 89.94 / 75.29 |
| **nar-small `r16`** (0.8) | CC BY-NC-SA 4.0 | 98.12 / 93.86 | **95.84 / 90.58** | **96.73 / 92.05** | 93.91 / 81.03 |

- `r16` against the first release, pipeline: +0.99 on the `ppocr-manga` detector (95%
  interval +0.77 to +1.25), +0.71 on `animetext-blocks` (+0.49 to +0.91). It is the same
  architecture with more output slots, on the same CPU path at the same cost, and it is
  ahead of the default pipeline too. So `nar-small` reads with `r16` (§2.3).
- hayai-nova's patch budget: 256 gives +0.18 on Read (+0.02 to +0.38) and no exact-match
  gain; 384 scored 94.72 against 94.58 (+0.14, no interval computed); Hard does not
  move and sound effects get slightly worse.
  The default stays 512.
- No permissively licensed model we tried beats hayai-nova on Read: PaddleOCR-VL manga
  v6 scores 96.14 on tight crops against hayai-nova's 96.60; hayai v2.6 was rejected
  before.
- On Read, PP-OCR's read alone ties the default pipeline on Composite (94.51 against
  94.58) and beats it on EM (87.69 against 86.15); taking, box by box, the better of the
  two would score 96.10 / 91.36. Choosing between them per box is open work; hayai-nova
  stays the default (it is ahead on Hard and on sound effects, the tie-breakers).
- The sound-effect set beside Read (380 boxes, 1 − NED, tight crops): nar-small `r16`
  94.92, nar-small 92.56, charspot-small 91.93, hayai-nova 91.12.

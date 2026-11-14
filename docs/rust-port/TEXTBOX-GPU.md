# The text-box recognizers and AnimeText on the libtorch GPU route

Status: design, 2026-11-13. Nothing here is built yet. The owner's decisions are in §2.

**Basis.** Paths are relative to the repository root, on branch `0.8/textbox-torch` at
ae9d4e84 (draft PR #102 "nar-small r16 via a weights-free ONNX graph" merged with
`0.8/remote-storage` 2b5590f0, which brings the CPU pool for the text-box engines from
#106). **[v]** marks a claim re-read in that tree. **[i]** marks one taken from the
investigations and not re-read. **guess** marks a guess. The spike scripts and their logs
are kept outside the repository; what they measured is in §5, and `tools/torch_export`
carries the production versions (§7).

Hardware named here: "a 24-core host with an RTX 4090" (sm_89) and "a 16-core desktop with
an RX 9070 XT" (gfx1201). Every spike ran on the desktop. Nothing has run on NVIDIA yet.

---

## 1. Intent

Give `nar-small` and `charspot-small` (muscgab; weights CC BY-NC-SA 4.0, built behind
`nc-models`) and the AnimeText block detector (`animetext-blocks`, YOLO12-x, GPL-3.0 weights,
built behind `gpl-models`) a GPU route through Bunko's existing libtorch backend pack, the
way `hayai-nova` and `paddle-manga` already run.

ONNX Runtime on the CPU stays the route everywhere else, and the fallback. A host with a
GPU and an older pack fetches the new pack by itself (§16); until it is in, the CPU route
serves.

Why AnimeText too: with the engine on a big GPU the pace moves to the block detector on the
CPU. The shipped pooled CPU detector reached 8.3-9.1 pages/s at 2.4-2.9 CPU-s a page on the
16-core desktop [i]; nar-small on the CPU pool runs at about 6.0
pages/s on the 24-core host (`crates/bunko-engines/src/plan.rs:156` doc comment [v]).

## 2. Owner decisions (2026-11-13, final)

1. **Both text-box engines, before the 0.8.0 tag, in one pack update.** nar-small (r16) goes
   first end to end, then charspot-small.
2. **AnimeText also gets the libtorch GPU route, in the same pack update.**
3. **Precision: fp32 packages for both engines, plus an fp16 option for nar-small** (§11).
   charspot-small and AnimeText are fp32 only.
4. **Packs update themselves.** A host with an older backend pack gets the new pack
   automatically when it has a GPU and an enabled row uses one of these engines or the
   detector. The CPU route is the silent fallback until the new pack is in (§16).

Standing rules that apply throughout:

- **Weights-free.** We host only compiled graphs (`.pt2`). The text-box packages bind the
  authors' unmodified `model.safetensors`, fetched from the authors' repos at pinned
  revisions. The muscgab weights are never hosted by us. The AnimeText packages bind the
  ONNX file we already mirror (GPL-3.0, `upstream-models-v1`), not a copy (§7.5).
- **`BT_ABI_VERSION` stays 1.** The loader demands exact equality
  (`crates/bunko-engines/src/torch/loader.rs:258` [v]): a bump would orphan every installed
  pack. New entry points are optional symbols (§6).
- **No `ENGINE_REVISIONS` bump** for nar-small (2), charspot-small (1) or any detector. The
  GPU route is gated on parity with the CPU route (§20). A bump would re-OCR every sealed
  sidecar (`crates/bunko-layout/src/sidecar.rs:30-42` [v]).
- **`OLDEST_PACK_RELEASE` stays "0.7.4"** (`crates/bunko-torch/src/abi.rs:253` [v]):
  hayai-only hosts keep their pack.
- Name no deployment, host or machine anywhere.

The original design's other recommendations stand as decided unless the owner says
otherwise (§22): the target set (CUDA sm_75/sm_80 on Linux and Windows, six ROCm gfx
targets), the new pre-releases for the `.pt2` files, and landing before 0.8.0.

Lead calls after the builder check (2026-11-13; the owner reviews them):

- **L1 A failed fp32 gate.** That target is left out of the manifest, so its GPU uses the CPU
  route, and the failure goes to the owner. Never a revision bump; never a looser tolerance
  without the owner's sign-off (§20).
- **L2 Targets without a test card.** gfx1201, gfx1030, gfx1032 (through the gfx1030
  override) and sm_80 on Linux and Windows are verified on real cards. gfx1100/1101/1102/1200
  and sm_75 ship built but unverified, as hayai's RDNA3 targets did, and only once their
  verified neighbours pass from the same exporter commit. The docs say which is which.
- **L3 The NVIDIA gates.** The lead runs them on the 24-core host with an RTX 4090 while its
  GPU is idle. Code work goes on meanwhile; only publishing waits for them. The lead puts the
  measured figures into `textbox_gpu_seconds` and `DETECT_GPU_SECONDS`.
- **L4 Publishing order.** Everything is staged and tested through
  `MOKURO_TORCH_MODELS_MIRROR`. The owner publishes `textbox-torch-v1` once, after plans 01-03
  pass their gates, and `detector-torch-v1` once, after plan 04, if the owner approves it
  (§22 Q1). No release is added to later. `textbox-graphs-v1` goes out first (§17).
- **L5 A GPU host with no pack** (was §22 Q2): a background fetch while the CPU route serves
  (`Need::Refresh` with `have: None`).
- **L6 The refresh switch** (was §22 Q3): `MOKURO_OCR_AUTO_INSTALL`, on by default.
- **L7 hayai budgets ride the refresh** (was §22 Q6): yes on GPU variants; the cpu pack keeps
  today's "update the OCR backend pack" error.
- **L8 Targets and release names** (was §22 Q7): adopted as listed above;
  `textbox-torch-v1`, `detector-torch-v1`.
- **L9 An engine-stage CPU candidate.** The benchmark tries `stage_device.engine = cpu` beside
  the GPU for text-box rows, so a weak GPU never makes a row slower than the CPU pool (§12).

## 3. Goals, non-goals, anti-goals

**Goals**

1. On an NVIDIA (cu130 pack) or AMD (rocm7.1 pack) GPU, a nar-small or charspot-small row,
   and an AnimeText detect stage, run on the GPU with no step beyond having the pack.
2. At fp32 the GPU output equals the CPU ONNX route: nar ids and charspot texts identical,
   AnimeText block sets identical with corners within 0.01 page px and no crop edge moved.
3. A page's blocks are read in one batched call on the GPU.
4. Every host that cannot use a GPU route falls back silently and correctly to the CPU
   ONNX route (§13).
5. The planner and the benchmark size GPU text-box and GPU detect stages correctly (§12).
6. Old packs on GPU hosts are replaced automatically, for about 1 MB of download (§16).

**Non-goals**

- A libtorch **CPU** route for these models. The ONNX CPU pool from #106 stays the CPU route.
- Vulkan (ncnn, WebGPU) and macOS GPUs. Those hosts keep the CPU route.
- The old 65-slot nar-small. Only the r16 model of PR #102 gets packages.
- fp16 for charspot-small or AnimeText.

**Anti-goals**

- Rehost, embed, pre-cast or fuse the authors' weights (no `package_constants_in_so`, no
  `TORCH_EXPORT_WIN_EMBED`, no `TORCH_EXPORT_CPU_EMBED` for these builders).
- Change hayai or paddle behaviour, or the CPU text-box pool sizing from #106.
- Swap a pack inside a running process.

## 4. Facts the design rests on

| fact | where |
|---|---|
| `bt_load` accepts only `hayai-nova` and `paddle-manga`; `Engine` has two arms; `Handle::read` returns `Vec<String>`. | `crates/bunko-torch/src/runtime.rs:160-163, 205-219, 312-341` [v] |
| The loader always loads three graphs. `PackageSet::load(graphs: [&Path; 3], ..)` parses `bunko.io` as a `u32` and checks it against `IO_VERSIONS = [1, 2]`. | `crates/bunko-torch/src/package.rs:29, 230-293` [v] |
| Weightless binding works with any safetensors file: `bunko.weights` maps FQN to key; `LoadOptions.weights` takes paths; binding is zero-copy through the Pairs C-ABI and every constant must be supplied ("no tensor for constant …"). | `package.rs:251-282`, `abi.rs:176-179`, `shim/aoti_shim.cpp:88-143` [v] |
| `Weights::load(files, dev)` keeps the stored dtypes; there is no cast. The shim never compares dtypes before binding. | `package.rs:59-162`, `aoti_shim.cpp:88-143` [v] |
| `LoadOptions.tokenizer` is a required `String`; every other field is `serde(default)`. `LoadInfo.io` is a `u32`. | `abi.rs:140-198` [v] |
| `Pack::optional_symbol` exists. The loader's `Api` holds only the ABI-1 entry points. | `loader.rs:62-70, 308-313` [v] |
| The ONNX text-box readers read one crop per call; nar takes `P[324,588] V[324] R,C[324] i32 H,W[1] i32` → `ids[161]`; charspot takes `img[1,3,H,W]` f32 0..255 → `boxes, scores, ids, order`. Their `RecognizerInfo` is hard-wired to fp32, CPU, batch 1. | `crates/bunko-engines/src/textbox.rs:237-293, 319-432, 724-739` [v] |
| The pipeline hands a page's blocks to the `BlockReader` in one call; `TextboxRecognizer` loops one model call per block. | `textbox.rs:635-673` [v] |
| `open_session` skips placement for text-box engines (`let textbox = …; if road == Road::Reconciled && !textbox`), loads them through `textbox_recognizer` (ONNX, CPU), and its GPU-to-CPU fallback never reaches them. | `crates/bunko-engines/src/lib.rs:1577-1659, 716-764` [v] |
| `stage_seconds` returns the CPU figure for a text-box engine on any device (`(e, STAGE_ENGINE, _) => textbox_cpu_seconds(e)`), so a GPU text-box stage is sized as nar 1.5 s / charspot 0.4 s a page. | `plan.rs:114-122` [v] |
| The planner declares the detect stage on `"cpu"` and never device-bound; it has no detector input. `DETECT_CPU_SECONDS = 0.9` sizes the CPU split beside text-box engines. | `plan.rs:61-64, 394-425` [v] |
| Text-box engines and every detector are registered CPU-only; `generations.rs` locks `detect` to the CPU and rejects `gpu:<n>` for CPU-only engines. | `crates/bunko-core/src/engines.rs:172-256`, `crates/bunko-core/src/generations.rs:892-905` [v] |
| The torch manifest assumes three graphs (`TORCH_GRAPHS`) and one release (`torch_models.json`); a `requires` entry must name a file of the same release. | `crates/bunko-ocr/src/models.rs:1225, 1319-1366` [v] |
| nar-small's files: our weightless ONNX graph (714,861 B, `textbox-graphs-v1`), the authors' `model.safetensors` (217,035,140 B, sha `bc7418e2…`, repo `muscgab/manga-ocr-nar-small` at `8cc14c15…`), the vocabulary. charspot-small's: `model.onnx` (43,992,111 B, weights inside) and `vocab.json` at `72ca2920…`; the same snapshot holds `model.safetensors` (43,860,012 B, sha `3dec2846…`). | `models.rs:157-276` [v]; HF snapshot [v] |
| AnimeText: input `images f32[1,3,H,W]` (page fitted to 640 by area average, 114-grey canvas rounded up to 32, RGB first, /255); output `output0 f32[1,5,N]`; host post-processing (score ≥ 0.3, IoU 0.5 suppression, containers, parts). | `crates/bunko-ocr/src/animetext.rs:1-200`, `crates/bunko-ocr/src/blocks.rs` [v] |
| `block_detector()` loads AnimeText on the CPU only and caches it by detector id. | `lib.rs:1148-1187` [v] |
| The sidecar's `ocr_engine.weights` is an ordered list of `(repo, revision)` written as one JSON object, so two entries with one repo collide. The re-OCR policy compares output-affecting fields and the revision, not `weights`. | `crates/bunko-layout/src/sidecar.rs:291-345`, `crates/bunko-server/src/ocr/upgrade.rs:780-795` [v] |
| `pack.json` exists twice (`abi::PackManifest`, the loader's; `bunko_update::backend::PackManifest`, the installer's); neither lists features. A loaded pack stays for the life of the process. | `abi.rs:207-237`, `crates/bunko-update/src/backend.rs:101`, `loader.rs:83-84` [v] |
| `install_ocr::needs_backend` counts every non-PP-OCR row as needing a pack; `Need` has `Off`, `Satisfied`, `Install`, `Extra`. The Extra lane is non-blocking with a 5/15/60 min then daily back-off. | `crates/mokuro-bunko/src/cmd/install_ocr.rs:102-140`, `crates/mokuro-bunko/src/ocr_install.rs:32-41, 138-193` [v] |
| The pack guard: any change under `crates/bunko-torch`, `crates/bunko-vlm`, `crates/bunko-ocr` (src and Cargo.toml) fails `pack_inputs` until re-blessed. | `crates/bunko-torch/tests/pack_inputs.rs:22-45` [v] |
| Windows GPU packages are weightless by default and bind through the Pairs entry point (`bunko.bind = "pairs"`); `TORCH_EXPORT_WIN_EMBED=1` rebuilds the old weights-inside layout. | `tools/torch_export/torch_export/build.py:236-241, 381-384` [v] |

## 5. Spike results

All on the 16-core desktop with an RX 9070 XT (gfx1201), torch 2.13.0+rocm7.1, compiled on
the device (not through fakegpu), TF32 off, weightless packages bound at load.

**nar-small r16** (one graph, dynamic batch 1..64; the 181 constants are exactly
`"net." + safetensors key`; the RoPE tables, the fc13 concat, alpha and cos/sin are computed
in the graph):

| check | fp32 | fp16 (weights cast to F16 at load) |
|---|---|---|
| package | 3,728,153 B, 43 s compile | 3,661,721 B |
| weights on the device | 217.0 MB | 108.5 MB |
| agreement | 1,024/1,024 crops, all 161 ids equal to the authors' fp32 PyTorch on the CPU | 7,927/7,929 texts equal to the authors' own fp16 (`PyTorchBackend(fp16=True)`) on all JMangaBench_Read crops |
| fp16 vs fp32 | | 7,925/7,929 texts equal (4 differ, each a doubled or dropped kana in garbled text) |
| JMangaBench_Read (7,789 keys) | composite 96.15, CER 1.75, EM 91.49 | 96.16, 1.75, 91.50; paired Δ +0.00, 95% CI [+0.00, +0.00], EM +1/−0 |
| crops/s at b8 / b32 / b64 | 80 / 83 / 84 | 1,342 / 1,438 / 1,505 |

fp32 nar is about 17x slower than fp16 on this card. 89% of its GPU time is in untuned
tiny-tile fp32 GEMMs (`Cijk_…_S_…_MT8x8x8/8x8x16`); the authors' eager fp32 is just as slow
(50-81 crops/s), and `TORCH_BLAS_PREFER_HIPBLASLT` does not change the kernels. At 9.4 blocks
a page that is about 8.5 pages/s for fp32 and about 140 for fp16. NVIDIA is unmeasured.
Peak GPU memory was 2.2 GiB with both packages and two reference models loaded.

**charspot-small** (two graphs; `dense` 131 constants, `tail` 77, all `"dense."` /
`"tail." + key`; decode on the CPU in Python), 100 crops: texts 100/100 equal to the authors'
ONNX at b1, b8 and b32 (the authors' fp16 differs on 3/100); pipeline 161 / 248 / 250
crops/s at b1 / b8 / b32 against 35.9 for the authors' ONNX on 8 CPU threads; packages
3,466,705 B (dense) and 2,345,443 B (tail).

**AnimeText YOLO12-x** (ultralytics `DetectionModel("yolo12x.yaml", nc=1)`, fused; with A2C2f's
`gamma` kept as a `[1,C,1,1]` parameter, all 422 package constants are ONNX initializers of
the mirrored file, byte for byte):

| check | result |
|---|---|
| eager fp32 GPU vs ONNX CPU | max \|Δ\| 9.2e-5 input px over candidates with score ≥ 0.1 |
| export, dynamic B 1..16, H, W = 32·[1,20] | 4.1 s, 1,344 nodes |
| weightless compile | 88.7 s, 5,098,341 B; 236.2 MB bound at load from the ONNX file (all 460 initializers `raw_data`) |
| parity vs ONNX CPU, 30 pages (448x640) | identical block sets 30/30 at b1/b4/b8; worst corner Δ 0.0002 page px; score Δ ≤ 5.9e-5; b1 vs b8 raw Δ 7.6e-4 |
| mixed canvases, 23 pages (448x640, 640x480, 640x640) | 23/23 identical, worst corner Δ 0.0005 px |
| crop edges, 60 pages, 2,304 edges (floor x0,y0 / ceil x1,y1) | 0 edges move a pixel |
| speed, 448x640, H2D + run + D2H, GPU idle | b1 19.1-21.5, b4 22.1, b8 22.0 pages/s; 44.1 ms device time a page |
| where the time goes (b4) | 84% MIOpen GEMM convolutions (small-tile fp32 Tensile), 5.7% `naive_conv`, Triton < 2% |
| CPU on the same box | ONNX 1 session x 16 threads 5.5 pages/s; the shipped pooled detector 8.3-9.1 |
| peak VRAM | 1.90 GiB (b16) |

So the AnimeText GPU route is about 2.4x the full CPU pool on this desktop and frees about 14
cores; batching buys about 10% (the convolutions are compute-bound).

**Backend packs across releases** (the local rocm7.1 packs of 0.7.0-beta.3 to 0.8.0-beta.1,
read-only): all hold the same 5,084 files (about 7.31 GB); across every release pair the only
file that ever differs is `libbunko_torch.so` (1,528,944 B). The files outside `lib/` (the
cdylib plus `licenses/`, 136 files, 1,980,951 B) compress to 858,994 B with `zstd -19`.
Re-hashing the 4,948 `lib/` files of an installed pack against a new `pack.json` and
hard-linking them into a staging directory took 9.0 s (warm cache, NVMe) and about 2.7 MB of
new disk; verifying all 5,084 files took 3.6 s more. The per-file check caught a substituted
cdylib. cu130 and Windows packs were not available to compare.

## 6. The ABI and pack capabilities (one table for every route)

`BT_ABI_VERSION` stays 1. Three optional symbols are added; the host looks each up with
`Pack::optional_symbol` when the pack opens and keeps the pointer in its `Api`.

```c
// The pack's capabilities beyond ABI 1: a static JSON array of strings (never freed).
const char *bt_features(void);

// Text-box engines: one entry per crop, in crop order.
//   json_out: [{"text":"...","chars":[["字",x0,y0,x1,y1,score], ...] | null}, ...]
//   boxes in crop pixels; "chars" is null for nar-small.
int32_t bt_read_chars(void *handle, const BtCrop *crops, size_t n,
                      char **json_out, char **err);

// Block detector: canvases are RGB8, width and height multiples of 32, at most 640.
//   json_out: [{"n": N, "cand": [[cx,cy,w,h,score], ...]}, ...], one entry per canvas;
//   candidates with score >= min_score, in the model's output index order.
int32_t bt_detect_blocks(void *handle, const BtCrop *canvases, size_t n, float min_score,
                         char **json_out, char **err);
```

`bt_read` on a text-box handle returns the texts (the `Recognizer::read` line path). A call
on the wrong kind of handle (`bt_read` / `bt_read_chars` on a detector, `bt_detect_blocks`
on a recognizer) returns `BT_ERR_ARG`.

**Features.** `crates/bunko-torch/src/abi.rs` gets the one list:

| feature | means the pack has |
|---|---|
| `read-chars-1` | `bt_read_chars`; `bt_load` of nar-small (fp32, and fp16 through `bunko.weights_dtype`) and charspot-small; the shim's dtype check |
| `detect-blocks-1` | `bt_detect_blocks`; `bt_load` of animetext-blocks; weights bound from an ONNX file (`bunko.weights_format = "onnx"`) |
| `hayai-budgets-1` | hayai-nova at patch budgets below 512 (since f1c22975) |

- `bt_features` is the runtime truth. `Pack::features()` reads it once at `Pack::open`; a
  pack without the symbol (every 0.7.4-0.7.12 pack) has an empty set.
- `pack.json` gains `"features": [...]` (`serde(default, skip_serializing_if = "Vec::is_empty")`
  in both `PackManifest` structs). `xtask torch-pack` fills it by parsing `BT_FEATURES` from
  `abi.rs`, as it already reads `BT_ABI_VERSION`. The installer, doctor and the admin
  overview read this copy without loading a pack.
- Each route still checks its own symbol. A pack whose features claim a route whose symbol
  is missing is logged (`warn!`) and that route stays on the CPU.
- Each feature joins `BT_FEATURES` in the commit that adds its route, so `pack.json` never
  claims a route the pack lacks.

**Package I/O ids** (`bunko.io`, a string, checked by `GraphSet::load`; `IO_VERSIONS` is
untouched): `textbox-1` (nar-small, charspot-small) and `detect-1` (AnimeText). `bt_info`
reports `io: 0` for these handles.

**New package metadata** (written by the exporter):

| key | value |
|---|---|
| `bunko.io` | `textbox-1` or `detect-1` |
| `bunko.source` | `<repo>@<revision>` of the weights |
| `bunko.weights_sha256`, `bunko.weights_size` | the bound file's sha256 and byte size |
| `bunko.weights_dtype` | `fp16` for nar-small fp16 only: bind every floating tensor cast to F16. Absent: bind as stored. |
| `bunko.weights_format` | `onnx` for AnimeText only: the weights are an ONNX file's initializers. Absent: safetensors. |
| `bunko.max_batch` | the package's largest batch (nar 64, charspot dense 64, AnimeText 8) |

**`bt_load`** takes `nar-small`, `charspot-small` and `animetext-blocks`.
`LoadOptions.tokenizer` carries the vocabulary path for the text-box engines and `""` for the
detector (the field is required; the JSON shape does not change). `LoadOptions.weights`
carries the authors' safetensors path, or the ONNX path for the detector. One new field,
`max_batch: Option<u32>` (`serde(default, skip_serializing_if = "Option::is_none")`), caps
the batch below the package's on small GPUs. It defaults to `None`. The host sets it from
`DeviceEntry.vram_mb` and the measured peak memory per batch (under 4 GiB: 8) and passes it
through `TorchBackend::textbox`; TORCH-BACKEND.md records the rule. A text-box run that runs
out of memory fails the page, as hayai's does.

## 7. Export pipeline (`tools/torch_export`)

### 7.1 Roles per engine

`ROLES = ("vision", "prefill", "step")` (`graphs.py:40`) becomes a per-engine table:

| engine | graphs |
|---|---|
| hayai-nova, paddle-manga | `vision`, `prefill`, `step` (unchanged) |
| nar-small | `model` |
| charspot-small | `dense`, `tail` |
| animetext-blocks | `detector` |

`BUILDERS` (`graphs.py:379`) gains the three builders, in a new module
`tools/torch_export/torch_export/textbox.py` (nar, charspot) and `detector.py` (AnimeText).
`manifest.py` and `verify.py` take the roles from the same table.

### 7.2 Rules every new builder follows

- **Weights map.** `bunko.weights` is `{FQN: key}`, where the key is the authors' safetensors
  key (the FQN minus `net.`, `dense.` or `tail.`) or the ONNX initializer name.
- **Refusals.** The exporter refuses to write a package if any constant has no key in the
  weights file, if a key's shape or dtype differs from the file's, if any weight is left
  inside the package, or if `TORCH_EXPORT_WIN_EMBED=1` or `TORCH_EXPORT_CPU_EMBED=1` is set.
  The publish script and `verify.py` refuse any `.pt2` whose constants live inside the package
  as well; the spike's packages with weights inside are never staged, mirrored or uploaded.
- **Pins.** It reads the weights by sha256 and refuses another file: nar `bc7418e2…`
  (217,035,140 B), charspot `3dec2846…` (43,860,012 B), AnimeText ONNX `5dd2bb9f…`
  (236,662,175 B).
- **Compile options** as `build.py` (`package_constants_in_so=False`, no freezing, no
  max-autotune), TF32 off. CUDA targets go through `fakegpu.py` and the `sdpa.py` pin (nar
  uses SDPA with bool masks; a wrong SDPA pick cost hayai 25%, TORCH-BACKEND.md).
- **Targets**: `linux-cuda-sm_75`, `linux-cuda-sm_80`, `windows-cuda-sm_75`,
  `windows-cuda-sm_80` (Windows cross-built with llvm-mingw, Pairs binding),
  `linux-rocm-gfx1030|1100|1101|1102|1200|1201`. No CPU, macOS or Vulkan targets.
- **Verification.** `verify.py` runs each built package with its weights bound against the
  reference (§20.1) and must pass before the package enters a manifest.

### 7.3 nar-small

The authors' Apache-2.0 `model.py` at the r16 pin, wrapped as the spike's `NarGraph`:
`prepare()` is recomputed in the graph (RoPE tables, fc13 concat, alpha, cos/sin), the three
non-persistent buffers become graph code, nothing is constant-folded. Inputs `P[b,324,588]
f32, V[b,324] f32, R,C[b,324] i32, H,W[b] i32`; output `ids[b,161] i64`; `b = Dim(1, 64)`.
That is the ONNX graph's I/O plus a batch dim, so Bunko's `nar_input` feeds it unchanged.

- **fp32**: the model as loaded.
- **fp16**: `net.to(torch.float16)` before export (the authors' own fp16 mode, as
  `PyTorchBackend(fp16=True)` and their cuda backend run it); `dt = net.slot_pos.dtype`
  drives the RoPE and alpha casts. The 181 constants are then F16 under the authors' keys,
  and the package says `bunko.weights_dtype = "fp16"`. The authors' fp32 file is bound cast
  at load (§9.2); nothing is pre-cast or rehosted.
- Rejected for fp16: autocast at export (it would cast all 54M weights on every call, and
  its numerics are not the authors' fp16 mode, which is the parity reference), and in-graph
  casts folded by AOTI runtime constant folding (keeps both copies on the device, 325 MB,
  and a little-tested AOTI path).

### 7.4 charspot-small

- `dense(img[B,3,16h,16w] f32 0..255, hw[B,2] i64)` → `heat, box, ruby, emb`; B in 1..64, h and
  w in 1..256 cells. The authors' masked path makes a padded crop's output equal its single
  crop's.
- `tail(emb[R,144,H8,W8], boxes[R,N,4], geo[R,N,4], mask[R,N] bool)` → `ids, pair, start, end`;
  R in 1..512, N in 2..1024, bounded at run time by the authors' `tail_budget` (2^18 rows x
  chars²) and `ROWS_PER_TAIL` (512).
- A tail part is padded to `max(2, n0)` characters, `n0` its first row's count, with the
  padding's mask false (the graph needs N ≥ 2). A part always takes at least one row, so a row
  with `n0 > 512` still runs. Both rules are checked against the authors' `backends/cuda.py` at
  the pin, and a test covers `n0 > 512`.
- fp32 only (the authors' fp16 differs on 3/100 texts).

### 7.5 AnimeText

- ultralytics pinned at **8.3.105** (the ONNX file's own producer; build-time only, never
  shipped), with `onnx`, in a separate `requirements-detector.txt`. The spike ran 8.4.212, so
  the spike's constant mapping and parity check are re-run under 8.3.105 before
  `detector.py` is written. If they fail, the pin is 8.4.212, which was measured. The exporter
  refuses any other version than the pin.
- `DetectionModel("yolo12x.yaml", nc=1)`, `fuse()`, A2C2f's forward patched to
  `x + gamma * y` with `gamma` a `[1,C,1,1]` parameter. Every state-dict tensor is loaded
  strictly from the ONNX initializer of the same name (the two gammas from `onnx::Mul_2760`
  and `onnx::Mul_2777`), shape and dtype equal.
- The exporter also refuses an initializer that is not F32 `raw_data`.
- Detect head `export=True`, `dynamic=True`: output `[B,5,N]`, no NMS in the graph.
- `B = Dim(1, 8)`, `H = 32·Dim(1, 20)`, `W = 32·Dim(1, 20)`.
- Metadata adds `bunko.weights_format = "onnx"`, `bunko.weights_sha256 = 5dd2bb9f…`,
  `bunko.weights_size = 236662175`, `bunko.source = deepghs/AnimeText_yolo@a180c191…`,
  `bunko.ultralytics` = the version actually used.

## 8. Artefacts, releases and hosting

| artefact | size | host |
|---|---|---|
| `nar-small/fp32/<target>/model.pt2` x 10 | ~3.7 MB each | **ours**, pre-release `textbox-torch-v1` |
| `nar-small/fp16/<target>/model.pt2` x 10 | ~3.7 MB each | same |
| `charspot-small/fp32/<target>/{dense,tail}.pt2` x 10 | ~5.8 MB a target | same |
| `animetext-blocks/fp32/<target>/detector.pt2` x 10 | ~5.1 MB each | **ours**, pre-release `detector-torch-v1` (GPL/AGPL only) |
| nar weights `model.safetensors` | 217,035,140 B | **authors' repo** at `8cc14c15…` (`nar-small/weights-r16`, already in #102) |
| charspot weights `model.safetensors` | 43,860,012 B | **authors' repo** at `72ca2920…` (new id `charspot-small/weights`) |
| AnimeText `animetext_yolo12x.onnx` | 236,662,175 B | `upstream-models-v1` (already mirrored; the CPU route's file) |
| vocabularies | as today | authors' repos |

- **Manifests.** Two new compiled-in files with the `torch_models.json` schema and their own
  `release`: `crates/bunko-ocr/src/textbox_torch_models.json` (cfg `nc-models`) and
  `crates/bunko-ocr/src/detector_torch_models.json` (cfg `gpl-models`). Their rows carry no
  `requires` (a `requires` entry must name a file of the same release); the weights are host
  files of the base manifest, fetched by `ensure_all` and passed in `LoadOptions.weights`.
- **`bunko-ocr` changes.** `TORCH_GRAPHS` becomes `torch_graphs(engine)`, used in
  `has_graphs`, `torch_manifest_graphs`, `torch_package_complete` and
  `torch_package_present`; `torch_release_name_for(engine)` gives each engine's release for
  provenance.
- **Downloads on demand.** nar's GPU route binds the same weights file its CPU route reads:
  nothing extra. charspot's CPU route reads `model.onnx`, its GPU route `model.safetensors`:
  only the chosen route's file is fetched; the fallback fetches its file when it is needed
  (44 MB). AnimeText's GPU route needs the `.pt2` only.
- So when the GPU route is the plan, `recognizer_ids` and `download_plan` add the text-box
  engine's `torch_host_ids` (today they treat these engines as ONNX-only, and `models
  download` on a GPU host fetches charspot's `model.onnx`). `prefetch_rows` also fetches nar's
  fp16 package when the row's mode can resolve to fp16 on that device, as hayai's prefetch does.
- `build_textbox_manifest` takes the pin (authors' weights or ONNX file) and the licence, so
  one builder writes both releases' manifests; it is tested for both.
- **Licences.** The text-box `.pt2` files are compiled from the authors' Apache-2.0 code and
  hold no weights; `textbox-torch-v1` carries `NOTICE.md` and the authors' `LICENSE`, as
  `textbox-graphs-v1` does, and everything stays behind `nc-models`. The AnimeText `.pt2` is
  compiled Ultralytics model code (AGPL-3.0) and binds GPL-3.0 weights; `detector-torch-v1`
  carries `NOTICE.md` (the ultralytics version used, the export script's path and commit, the weights'
  source and sha) and the AGPL-3.0 text, stays behind `gpl-models`, and is never referenced
  by a `--permissive-models` build.

## 9. The cdylib (`crates/bunko-torch`)

### 9.1 Loading graphs

`GraphSet::load(graphs: &[(&str, &Path)], weight_files: &[PathBuf], at: Placement,
expect_io: &str)` is `PackageSet::load` without the three-role assumption: every graph's
`bunko.io` must equal `expect_io`, every graph's `bunko.weights_dtype` and
`bunko.weights_format` must agree, and the set is refused otherwise. `PackageSet` becomes a
thin three-role wrapper, so hayai and paddle code is unchanged. `Aoti`, `Placement`,
`unpack` and `par` are reused as they are. The holder declares the packages before
`Weights` (`package.rs:178-189`).

### 9.2 Weights

- `Weights::load(files, dev, cast: Option<Kind>)`. With `Some(Kind::Half)`, every F32, F64
  and BF16 tensor is converted with `to_kind(Half)` after it reaches the device (integer and
  bool tensors stay as stored); the peak extra memory is one fp32 tensor; the CPU blob-view
  branch is skipped. This is aten's round-to-nearest-even, the spike's binding.
- `Weights::load_onnx(path, dev, wanted: &HashSet<String>, size: u64)` for
  `bunko.weights_format = "onnx"` (the initializer names the graphs' `bunko.weights` map to, and
  `bunko.weights_size`). The parsing half is a dependency-free protobuf walker in
  an always-built module (`crates/bunko-torch/src/onnx_weights.rs`, testable without
  libtorch): ModelProto field 7 → GraphProto field 5 → TensorProto fields 1 (dims),
  2 (data_type), 8 (name), 9 (raw_data), 14 (data_location); every other field is skipped by
  wire type; lengths are bounded by the file size. It refuses a file whose size is not
  `bunko.weights_size`, a missing or duplicate wanted name, a dtype other than FLOAT, a byte
  count that does not match the dims, and `data_location = EXTERNAL`. The tensor half then
  reads each range as the safetensors path does. The host checked the sha256 at download, so
  the pack does not re-hash 236 MB at each load.
- **Dtype check before binding**, for every engine: `bt_aoti_load_constants` compares each
  tensor's dtype with `runner->getConstantNamesToDtypes()` and refuses with "constant X is
  F32, the package wants F16". This is inside the cdylib, not the host ABI. It is tested at
  the cdylib level: a libtorch-feature test in `package.rs` binds F32 tensors to the fp16
  package through `Weights::load(.., None)` (AOTI metadata may be compiled into the model
  library, so rewriting a package copy's metadata proves nothing).

### 9.3 Engines

New `crates/bunko-torch/src/textbox.rs` and `src/animetext.rs`; `Engine` gains
`Nar(TorchNar)`, `Charspot(TorchCharspot)`, `Detector(TorchAnimeText)`. Each runs its device
work under a `gpu_lock` like `hayai.rs`, and its host preprocessing in parallel through
`par::map_ordered`, bounded by `LoadOptions.threads`.

- **TorchNar.** `nar_input` per crop (an empty crop reads `""` without a model call), stacked
  to `[b,…]` and split at `min(bunko.max_batch, LoadOptions.max_batch)`; one `Aoti::run` per
  chunk; ids cut at the end id (the vocabulary's length, 6817).
- **TorchCharspot.** `charspot_canvas` per crop; crops grouped onto canvases by the authors'
  ladder and merge rule (`LADDER = 1, 2, 3, 4, 6, 8, 11, 14, 20, 28, 40, 56, 80, 112, 160,
  196, 256` cells, `MERGE_OVERHEAD = 1024`, at most 64 a batch); `dense`; heat, box and ruby
  copied to the host; the Rust port of the authors' `core.detect`, `nms` and `chain` (§9.4);
  `tail` with `emb` kept on the device, rows of similar length together within the tail
  budget; the class table (the vocabulary padded to 6818, then the 12 `!?` glyphs); canvas
  boxes mapped back to crop pixels with the crop's factors.
- **TorchAnimeText.** Takes the host's letterboxed RGB8 canvases; converts each with the
  shared `bunko_vlm::animetext::canvas_chw` on the host CPU (bit-identical to the ONNX
  route's tensor: 114u8/255 equals today's `PAD`); groups canvases by size, stacks, runs;
  copies `[B,5,N]` to the host; keeps candidates with score ≥ `min_score` in index order.

### 9.4 Shared host code moves to `bunko-vlm`

`bunko-vlm` is already a dependency of `bunko-torch` and `bunko-engines` and has no
workspace dependencies of its own [v].

- `bunko_vlm::textbox`: `fit_grid`, `on_canvas`, `nar_input`, `charspot_canvas` and their
  constants, moved from `bunko-engines/src/textbox.rs`; plus the charspot decode
  (`detect`, `nms`, `chain`, `canvas_batches`), ported from the authors' `core.py` and
  `backends/cuda.py` with the same dtypes: centres in f64 (numpy promotes int64 + f32),
  sizes `exp` in f32, boxes rounded to f32, NMS in f32 with its `1e-6`, the "centre in the
  central half" rule (0.25 of each side), suppression at IoU ≥ 0.2, ruby ≥ 0.5 dropped,
  heat > 0.3, geometry over the lower median of the max sides, first index on argmax ties,
  and `end[cur] > best` strictly in `chain`. The module header and the licence listing keep
  the authors' Apache-2.0 attribution (Apache §4).
- `bunko_vlm::animetext::canvas_chw(canvas: &Rgb) -> Vec<f32>` (u8 → f32 /255, CHW, red
  first). `bunko-ocr` gains a dependency on `bunko-vlm` (no cycle: `bunko-vlm` depends on no
  workspace crate).

The ONNX route and the libtorch route then build bit-identical input tensors from one
function, and no tensor crosses the ABI.

**TF32 off.** libtorch lets cuDNN convolutions use TF32 by default on Ampere and newer, and
AnimeText is 84% convolutions. `bt_init` turns TF32 off for cuDNN and for matmul
(`setAllowTF32CuDNN(false)` and the matmul flag). This also applies to hayai and paddle, so
the NVIDIA gate (§20.5) re-checks their sm_80 output on a test volume with the new pack.

**Vulkan pack.** No change: text-box engines and the detector are never offered on Vulkan
devices (the format loops list only hayai and paddle).

**Pack guard.** These changes touch `bunko-torch`, `bunko-vlm` and `bunko-ocr`, so
`crates/bunko-torch/tests/pack-inputs.txt` is re-blessed in each change
(`BUNKO_BLESS=1 cargo test -p bunko-torch --test pack_inputs`). The floor stays.

## 10. Host integration (`crates/bunko-engines`)

### 10.1 One text-box recognizer, two model back-ends

`TextboxRecognizer` keeps the `Recognizer` and `BlockReader` seam (`verbatim() = true`,
`paddle_quad_crop(LINE_MARGIN_EM)`, `share_characters`, `share_text`, `lines_of`). Its reader
becomes `Onnx(Charspot | NarSmall)` or `Torch(TorchTextbox)`; the new
`crates/bunko-engines/src/torch/textbox.rs` wraps the `Loaded` handle.

- `read_lines` and `read_blocks` make one batched call per page. The ONNX back-end loops
  inside, so CPU behaviour is unchanged; the torch back-end sends the page's crops in one
  `bt_read_chars`.
- `RecognizerInfo` gets the real device (`Gpu(n)`), the precision and `batch` from
  `bt_info`. `page.rs` then stops trimming the heap for GPU pages, as for hayai.

### 10.2 Placement in `open_session`

Text-box engines go through `runtime::place_runnable` with a text-box arm in `formats()`:

| device | formats |
|---|---|
| CPU | `["fp32"]` when the ONNX files are obtainable |
| GPU | the pack is loaded and `bt_read_chars` exists; the device's formats filtered by `store.torch_package_obtainable(engine, p, torch_targets(..))` (nar: fp32 and fp16; charspot: fp32; never bf16); and the host files (weights, vocabulary) are obtainable |

- The text-box arm loads, falls back and returns its recognizer itself, before the generic
  `resolved` / `self.recognizer()` block that serves hayai and paddle. nar's resolved precision
  (§11) therefore never reaches that block; charspot keeps its fixed fp32 path.
- A GPU result loads through `TorchBackend::textbox(..)`, with a weak cache keyed by engine,
  precision, device, package dir and threads.
- Any GPU load error: `warn!` and `textbox_recognizer()` (the ONNX CPU route, pool sizing
  from #106). Never `self.recognizer()`, which has no ONNX text-box arm.
- An explicit `gpu:<n>` that cannot run the engine falls back with the reason logged, as
  hayai does (`runtime::place_runnable`).
- A pack without `bt_read_chars` gives the reason "the OCR backend pack predates GPU text-box
  reading; it updates itself (see doctor), or `mokuro-bunko install-ocr --refresh`". A
  reason to note, not an error.
- Precision: the text-box arm resolves `precision::resolve(engine, mode, formats, pick, why)`
  after placing, as hayai does (§11).

### 10.3 The AnimeText detector

- `bunko-ocr/src/animetext.rs` splits `detect` into
  `pub fn canvas(bgr: &BgrImage) -> Option<Canvas>` with `Canvas { rgb: Rgb, scale: f64, page:
  (usize, usize) }` (`fit` with 114 padding; `None` for a zero-size page) and the model call. The ONNX path becomes `canvas` →
  `canvas_chw` → `run_named` → `blocks_from_output`, with today's tensor bit for bit.
- New `crates/bunko-engines/src/torch/detector.rs`: `TorchBlockDetector` implements
  `BlockDetector`: `canvas`, `bt_detect_blocks(1 canvas, MIN_SCORE)`, the candidates packed
  as `[5, k]`, then the unchanged `blocks_from_output`. NMS, containers and parts stay on the
  host: one implementation for both routes, the device can only perturb raw floats, and the
  stable sort's ties stay identical.
- `EnginePipeline::block_detector(detector, copies, placement)` caches by
  `(detector, device)`. On a GPU placement it needs: the pack loaded, `bt_detect_blocks`
  present, `store.torch_package_obtainable("animetext-blocks", "fp32", targets)` and the
  ONNX file obtainable; then `ensure_all` fetches the `.pt2` and loads it through
  `TorchBackend`. On any failure: `warn!("animetext-blocks: detecting on the CPU: {why}")`
  and today's ONNX CPU load.
- **Detect placement** (`detect_placement` in `open_session`): `stage_device.detect` "cpu" →
  CPU; "gpu:n" → that GPU if runnable, else the CPU with the reason; `auto` → the engine's
  GPU when the engine was placed on one, otherwise the first GPU whose detector formats are
  `["fp32"]`; `ocr.backend: cpu` → CPU; a GPU with less than 4 GiB VRAM → CPU (guess; the
  spike peaked at 1.9 GiB alone).
- **One page per call** by default; the ABI and the package stay batch-capable (B 1..8).
  Batching gained 10% on gfx1201 and would make a page's boxes depend on its batch-mates
  (b1 vs b8 raw Δ 7.6e-4). An opportunistic same-canvas combiner is added only if the NVIDIA
  measurement shows ≥ 1.3x at b4.
- A run error fails the page; the device is not switched mid-session. A load failure falls
  back to the CPU at session start.

### 10.4 Models, readiness, catalog

- `torch_host_ids` and `torch_host_files` get explicit arms: nar `[NAR_VOCABULARY,
  NAR_WEIGHTS]`, charspot `[CHARSPOT_VOCABULARY, CHARSPOT_WEIGHTS]`. Today's `_` arm is
  paddle's and would mis-handle a new engine. `TorchFiles` gains `weights: Vec<PathBuf>`.
- `recognizer_ready` is true when the ONNX route or the torch route is obtainable.
  `need_for`, `package_status_for`, `package_plan`, `prefetch_rows`, doctor and
  `cmd/models.rs` accept text-box engines; `EngineNeed.fallback` names the CPU ONNX route.
  Prefetch fetches what the plan picks: on a GPU host the `.pt2` plus the weights (and nar's
  0.7 MB ONNX graph too, since its weights are shared and the fallback is then free).
- `no_recognizer_backend` never demands a pack for text-box engines.
- `torch::runnable_formats` also loops over the text-box engines. Where a GPU's format
  exists only for some engines (nar fp16; hayai and paddle bf16), `format_engines` records
  which, so the server does not route a hayai row to a GPU that only has text-box packages,
  and `formats_for_engine` keeps a nar row off bf16.
- `engine_formats` applies the full text-box test of §10.2 (`bt_read_chars` present, packages
  obtainable, host files obtainable), not only `torch_package_obtainable`, so an old pack never
  advertises GPU formats it cannot run. `format_engines` is built for GPU devices only; the CPU
  entry would only mislead.
- New rows from `detector_torch_models.json` (cfg `gpl-models`); `detector_ids` is unchanged
  (CPU) and `detector_torch_ids(device)` covers the GPU route.

## 11. Precision: the fp16 option for nar-small

- **nar-small becomes a precision engine.** `precision: true` in the registry
  (`engines.rs:187-202`). `EngineSpec` gains `precision_formats: &[&str]`: nar
  `["fp32","fp16"]`, hayai and paddle all three, everyone else `[]`.
  `engine_modes(engine)` returns the auto modes plus those formats; the admin catalog
  (`admin/ocr.rs:229`) uses it. The admin UI is data-driven and already resets a row whose
  mode the new engine lacks [v].
- `bunko-sched` has no `bunko-core` dependency (only a dev-dependency), so
  `precision::engine_modes` keeps its own static table, which gains nar-small (the auto modes
  plus fp32 and fp16). A dev-dependency test keeps it equal to the `bunko-core` registry, the
  pattern the crate already uses for `ARCHIVE_EXTENSIONS`.
- `is_precision_engine` and `candidates` gain nar in `bunko-sched`; the `bunko-engines` copies
  become re-exports of `bunko-sched`'s, as `bf16_native` already is, so the tables cannot
  drift.

| nar-small mode | CPU (ONNX, fp32 only) | GPU with fp32 and fp16 packages |
|---|---|---|
| `auto-accuracy` (default) | fp32 | **fp32** |
| `auto-balanced` | fp32 (one candidate, no benchmark) | benchmark `[fp32, fp16]`: fp16 only when more than 5% faster |
| `auto-speed` | fp32 | benchmark `[fp16, fp32]`: fp16 unless fp32 is more than 5% faster |
| `fp32` | fp32 | fp32 |
| `fp16` | not eligible (`PRECISION_REFUSAL`; the row is held where no machine runs it) | fp16 |

- A nar row with `bf16` is a row error ("nar-small has no bf16; use fp16 or an auto mode");
  `precision_pick` must be one of the engine's formats.
- With an auto mode a GPU load failure falls back to the CPU at fp32. A forced `fp16` that
  the CPU cannot take reports both errors (`gpu_then_cpu_error`) and the row backs off on
  this machine, as a forced hayai fp16 does today.
- The processor benchmark needs no logic change: `precision_candidates` turns on through
  `is_precision_engine`.
- The sidecar records `ocr_engine.precision = "fp16"`. That is provenance, not identity:
  upgrades key on `(engine, revision)` only, so switching a row re-reads nothing.
- charspot-small stays fp32 only; it keeps `precision: false`.

## 12. Planner and benchmark

- **`stage_seconds` GPU arms** for text-box engines: nar-small 0.12 s a page (fp32 on gfx1201:
  80 crops/s at 9.4 blocks a page) and charspot-small 0.04 (248 crops/s). The planner keeps one
  figure per engine; fp16 nar is faster, but with AnimeText on the CPU the detect width is at
  its ceiling either way. The lead replaces these with NVIDIA figures after the measurement
  in §20.5.
- `plan()` already treats a GPU text-box engine stage as device-bound (`textbox = None` on a
  GPU, `plan.rs:394-398`), so the CPU pool helpers stay unused there. The default engine
  width on a fast GPU of a 16+-core host is 2 (`FAST_GPU_ENGINE_THREADS`); one page is in host
  preprocessing while the other is on the device. No cross-page batcher.
- **Detect on a GPU.** `plan()` gains `detect_device: &str`. A GPU detect stage is
  `device: "gpu:n"`, `device_bound: false` (pooled host work around one shared handle),
  default width `DETECT_GPU_WORKERS = 2`, ceiling 4; its device cost is
  `DETECT_GPU_SECONDS = 0.045` (gfx1201; NVIDIA a guess until measured). When detect and the
  engine share one GPU, the pace is the **sum** of their device seconds per page. Beside a
  CPU text-box pool, the core split uses `DETECT_GPU_HOST_SECONDS = 0.04` (decode 14.3 ms plus
  `fit` and the canvas; guess) instead of `DETECT_CPU_SECONDS = 0.9`, so the engine pool gets
  nearly every core.
- `width_ceilings` and `bench.rs::ceiling_on` give a GPU detect stage `min(budget, 4)`.
- **Benchmark candidate.** When a row has a block detector and both devices are possible,
  the benchmark adds a `stage_device.detect = cpu` candidate and pins the faster: a many-core
  host with a weak GPU can be faster on the CPU.
- **Engine-stage CPU candidate (L9).** For a text-box row whose engine can run on a GPU, the
  benchmark also adds a `stage_device.engine = cpu` candidate beside the GPU one and pins the
  faster. fp32 nar or charspot on a weak GPU (RDNA2/3) may be slower than the CPU pool. Output
  is identical either way (§20.1), so this only picks the placement.
- The module docs (`plan.rs:16-24`, "CPU only") are rewritten.

## 13. CPU fallback matrix

| host | text-box engines | AnimeText | note |
|---|---|---|---|
| cu130 / rocm7.1 pack with `read-chars-1` / `detect-blocks-1`, supported GPU, packages obtainable | GPU | GPU | |
| 0.7.4-0.7.12 pack (no symbols) | CPU | CPU | reason in the log and doctor; the pack refreshes itself (§16) |
| cpu pack, Vulkan-only pack, macOS, no pack | CPU | CPU | |
| GPU target with no package (a gfx not built) | CPU | CPU | `place_runnable` reason |
| GPU load or bind fails (VRAM, driver) | CPU | CPU | `warn!`, at session start |
| `ocr.backend: cpu` | CPU | CPU | |
| `stage_device.engine: cpu` / `stage_device.detect: cpu` | CPU | CPU | per stage |
| GPU under 4 GiB VRAM | GPU if it loads, `max_batch` 8 (§6) | CPU | detector floor (guess) |

The fallback keeps the CPU pool sizing from #106 (`textbox_recognizer`).

## 14. Sidecar provenance and revisions

- `ocr_engine.weights` keeps the authors' pins (nar, charspot) and AnimeText's
  `(deepghs/AnimeText_yolo, a180c191…)`.
- Our export sets go in **one** `Gnathonic/mokuro-bunko` entry (the sidecar writes the list
  as one JSON object, so two entries with that key would collide): the release names joined
  with `+`, detector first, then recognizer. Examples: `detector-torch-v1+textbox-torch-v1`
  (both on the GPU), `textbox-graphs-v1` (nar on the CPU: today's #102 code records no export
  set for it although the graph is ours; this closes that gap), `textbox-torch-v1` (nar on
  the GPU, AnimeText on the CPU). charspot on the CPU runs the authors' own ONNX: no entry.
- `ocr_engine.precision` from `RecognizerInfo.precision` (fp32, or fp16 for nar).
- No revision bump anywhere (§2).

## 15. Registry, admin, config, docs

- `crates/bunko-core/src/engines.rs`: nar-small and charspot-small get `cpu_only_reason: ""`
  (each in the plan that ships its route); their `cpu_note` stays empty, since that field is a
  caution shown when the CPU is picked and the catalog's `devices: "any"` already signals the
  GPU route. AnimeText's detector `cpu_only_reason` becomes `""`. Fix the "on ONNX Runtime"
  comment.
- `generations.rs:893` replaces `"detect" => true` with "the effective detector's
  `cpu_only_reason` is non-empty"; `gpu:<n>` is then accepted for `animetext-blocks` and still
  refused for `ppocr-manga` and `ctd-lines`.
- `admin/ocr.rs` follows from the registry (`devices: "any"`, `stage_lock_reason`).
- Prose: `web/admin/index.html` (engine rows with RAM and VRAM, the detector hint),
  `docs/configuration.md` (engines, precision modes, automatic updates),
  `config.example.yaml`, `docs/ocr-internals.md`, `docs/rust-port/MODELS.md`,
  `TORCH-BACKEND.md` (single-graph engines, `bunko.weights_dtype`, `bunko.weights_format`),
  `PACKAGING.md` §8, `docs/deployment.md`, README and CHANGELOG.

## 16. Pack refresh: automatic backend-pack updates (owner decision 4)

### 16.1 What is true today

- A build opens any pack from `OLDEST_PACK_RELEASE` up to its own release, and `install-ocr`,
  `--if-needed` and the release update all count such a pack as installed
  (`abi.rs:253-277`, `install_ocr.rs:709-756` [v]).
- Packs of every release share one directory name (`torch-<variant>-2.13.0`); a loaded pack
  cannot be deleted on Windows, and `pack_removal` defers that through `.remove-at-start.json`,
  run from `serve.rs:68` before anything loads (`crates/mokuro-bunko/src/pack_removal.rs` [v]).
- The Vulkan "Extra" fetch is the existing non-blocking side lane (`ocr_install.rs` [v]).
- The server's automatic update waits for a quiet moment (`Quiet::{drain, busy}`); a
  processor drains after its volume and returns `ServeExit::Updated`
  (`crates/bunko-processor/src/remote.rs:89-94` [v]). `restart_after_install` already
  restarts the server when another pack is loaded (`server_machine.rs:710-730` [v]).
- A processor learns its library's rows as `EngineWant { engine, precision }`, with no
  detector and no patch budget (`crates/bunko-proto/src/lib.rs:331-335` [v]).
- Side finding: `needs_backend` counts every non-PP-OCR row, so a CPU-only library whose rows
  are nar-small or charspot-small installs a 437 MB cpu pack its ONNX route never uses
  (`install_ocr.rs:130-140` [v]).

### 16.2 Which rows need which feature

One shared function in `crates/bunko-engines/src/models.rs`:

```rust
/// Pack features the GPU route of a row needs (empty: none, or no GPU route in this build).
pub fn torch_features_for(engine: &str, detector: Option<&str>, patch_budget: u32) -> Vec<&'static str>
```

nar-small and charspot-small → `read-chars-1` (cfg `nc-models`); detector `animetext-blocks`
→ `detect-blocks-1` (cfg `gpl-models`); hayai-nova with a budget other than 512 →
`hayai-budgets-1`. The installer, doctor and `open_session`'s reasons all use it.

hayai budgets ride the refresh (L7): on a GPU variant, today's hard "update the OCR backend
pack" error for a hayai row below budget 512 becomes a self-healing refresh. On the cpu pack
(rule 2) it stays the error.

### 16.3 When a host refreshes

`install_ocr::need()` gets a new non-blocking outcome between `Satisfied` and `Extra`:

```rust
Need::Refresh { variant: String, have: Option<String>, lacks: Vec<String>, rows: Vec<String> }
```

It is returned when all of these hold:

1. Local OCR is on.
2. The wanted variant (`wanted_choice`) is `cu130` or `rocm7.1`. `ocr.backend: cpu` never
   refreshes.
3. Either a pack of that variant that this release opens is installed and lacks a needed
   feature (`have: Some(..)`), or no pack is installed and every runnable row that wants the
   GPU pack also runs without one (text-box rows, AnimeText rows; `have: None`, a full
   archive through the same lane, fetched in the background while the CPU route serves;
   lead call L5). Rows that cannot run without a pack (hayai, paddle) keep today's blocking
   `Need::Install`.
4. The pack may be refreshed. This is two conditions, not one `replaceable` flag:
   - A development pack (`bunko_version` empty) or a pack set by `MOKURO_TORCH_PACK` never
     refreshes.
   - A release pack outside the role's own `backends_dir()` (the bundled directory, `/opt`, a
     processor's fallback directory) is not replaced in place, but it still refreshes: the new
     pack is installed into the own directory (a thin refresh linking from the found pack, or
     the full archive), which `discover` searches first. `pack_swap::at_start` then finds no
     `<name>` there and only renames `.next-<name>` into place.
5. The host has a GPU the pack serves: AMD, the hwdetect `gfx`, mapped through the same HSA
   override rule as `prepare_rocm_env` (gfx1032 → gfx1030), is in the pack's
   `requires.gpu_archs` and the torch manifests have a package for it (a gfx1032 test covers
   this); NVIDIA, always (sm_75 and sm_80 packages cover Turing on; hwdetect has no `sm_`).
6. A runnable row needs a feature the pack lacks: `torch_features_for(row) \ pack.json.features`
   is not empty. Library: `config.ocr.generations` (`.runnable()`), with `detector`,
   `patch_budget` and `pools.stage_device`; a row whose engine stage (or, for
   `detect-blocks-1`, detect stage) is pinned to `cpu` does not count. Processor:
   `library-engines.json`, which gains `detector` and `patch_budget` (§16.7).
7. This release's signed `release.json` lists a pack for the variant with those features
   (`BackendArtifact.features`), checked in the child before any download; otherwise the
   outcome is "nothing to fetch for this release", recorded and retried daily.
8. No standing failure in `<backends>/.pack-refresh.json` is due later.
9. Automatic installs are on (`MOKURO_OCR_AUTO_INSTALL`, default on; lead call L6) and
   `MOKURO_OCR_PACK_REFRESH` is not `off`. When only this rule fails the outcome is still
   `Need::Refresh` but it is offered (doctor, the admin button, the CLI), never fetched by
   itself.

It is evaluated wherever `need()` runs: the server's start, a settings save, the end of an
install (refresh first, then Extra), and each processor registration.

**Side fix.** `needs_backend` counts only rows that cannot run without a pack (hayai, paddle).
Text-box and AnimeText rows reach a pack only through rule 3, and only for a GPU variant: a
CPU-only host with such rows installs no pack.

### 16.4 Fetching: thin refresh, full archive as fallback

New child mode `install-ocr --if-needed --refresh` (background) and `install-ocr --refresh`
(by hand), holding `.install.lock`:

1. Read the signed `release.json` entry `backends[TARGET][variant]`; check
   `features ⊇ lacks` and `requires` (driver).
2. Prefer the thin archive when the entry has `thin` (and a pack is installed): check space
   for `thin.size + 64 MiB`, download it with resume, check its sha256, unpack into
   `<backends>/.next-<name>/` (`pack.json`, the cdylib, `licenses/`), then take every other
   file from the installed pack with `backend::link_from_installed`: the installed file must
   hash to the new `pack.json`'s sha256 and size, then it is hard-linked, or copied where links
   fail (after a space check for the installed size). External files go through the existing
   `reuse_external`.
3. Fall back to the full archive (today's `stage()`) when a file is in neither, or when no pack
   is installed.
4. Verify every file (`verify_files`, `make_links`). The trust chain is unchanged: the signed
   `release.json` → the thin archive's sha256 → its `pack.json` → each file's sha256, linked
   ones included.
5. Probe in the child with `MOKURO_TORCH_PACK=<.next dir>`: the pack loads, finds this GPU and
   reports `Pack::features() ⊇ lacks`. Otherwise delete `.next-<name>` and fail.
6. Fetch the new routes' model files with that pack (nar ~3.7 MB of `.pt2`; charspot ~5.8 MB
   plus 44 MB of the authors' weights; AnimeText ~5.1 MB): the child runs `prefetch_rows` for
   the rows behind `lacks` with `MOKURO_TORCH_PACK` set to the staged directory (fp32, plus fp16
   for nar when its mode benchmarks it).
7. Write `<backends>/.swap-at-start.json` (a `pack_swap::SwapRecord`)
   `{name, staged, from, to, features, at}`.
8. Never touch the installed pack. A full install, `--force` or a release switch deletes
   `.next-*` and the record.

| path | download | disk while staged |
|---|---|---|
| thin refresh | about 0.9 MB plus the routes' models | about 3 MB (`lib/` hard-linked) |
| copy fallback | about 0.9 MB | + the installed size (cu130 ~2.5 GiB [i], rocm ~6.8 GiB) |
| full archive | 364,667,883 B (cu130 linux), 336,140,073 B (cu130 windows), ~3.2 GB (rocm) | archive + pack |

### 16.5 Swapping without disrupting OCR

The swap always happens at a process start, before anything loads a pack; until then the
running OCR keeps the old pack and the CPU route for these rows.

- **No pack loaded in this process** (`torch::backend_if_open().is_none()`): swap now,
  `forget_failure()`, bump `finished`.
- **A pack is loaded and `MOKURO_OCR_PACK_REFRESH=auto`** (default): restart at a quiet moment.
  Server: a new `MachineHooks.restart_when_quiet: Arc<dyn Fn(String) + Send + Sync>`,
  factored out of `UpdateService::auto_install`'s quiet loop (hold new claims everywhere, wait
  for in-flight claims and uploads). A release update that starts meanwhile wins and drops the
  staged refresh. Processor: a "pack staged" `watch` in `ServeOptions`; the serve loop drains
  as for an update and returns a new `ServeExit::Restart(String)`, and `cmd/processor.rs`
  re-execs. If re-exec fails: a problem "restart this processor to use the GPU route".
- **`MOKURO_OCR_PACK_REFRESH=next-start`**: no restart; swapped at the next start.

`pack_swap::at_start(root)` (new, beside `pack_removal`; called after `pack_removal::at_start`
in `serve.rs` and at the top of `processor serve`):

1. Read `.swap-at-start.json`; drop it if `to` is not this binary's version or `.next-<name>`
   is incomplete (`pack_complete`).
2. Rename `<name>` → `.prev-<name>`, then `.next-<name>` → `<name>`. If a rename fails (Windows:
   another process holds the DLLs), undo, keep the record and log "the OCR backend pack update
   waits: <dir> is in use by another process; it is swapped in at the next start".
3. Probe in a child (`install-ocr --probe`). On failure: `<name>` → `.failed-<name>`,
   `.prev-<name>` → `<name>`, delete `.failed`, write a lasting `.pack-refresh.json`
   `{variant, bunko_version, target, gpu, failures, lasting: true, message}` and raise a
   warning problem; the machine runs on as before.
4. On success delete `.prev-<name>` and the record; log "OCR backend pack updated to the
   <variant> pack of <to> (adds …)".

`.pack-refresh.json` also carries the back-off of refreshes that fail before staging, with
`ExtraAttempt`'s shape and waits: 5, 15 and 60 min, then daily; "nothing to fetch for this
release" waits a day at once; a lasting failure waits until the release, target or GPU
changes; a manual start clears it.

The refresh publishes **no** `OcrInstall` state, like the Extra fetch: the server's OCR never
stops and a processor never pauses as "installing".

### 16.6 Release updates and Docker

- `update.auto` / `processor.auto_update`: `prefetch` stops keeping an installed pack that
  lacks a feature the rows need (rules 2-6); it stages a pack instead (thin when possible),
  and the release switch brings binary and pack together with one restart and the existing
  probe and rollback.
- Docker's `serve` runs the same background installer, so a container updated to 0.8.0 with an
  old pack in `/data/backends` refreshes about 1 MB and restarts at a quiet moment.
  `BAKE_PACK=1` images install the refreshed pack into `/data/backends` (rule 4).

### 16.7 Remote processors

- `EngineWant` gains `detector: Option<String>` and `patch_budget: Option<u32>` (both
  `serde(default, skip_serializing_if = …)`), filled by the library's registry. Old processors
  ignore them; from an old library a processor assumes `ppocr-manga` and 512.
- `wants.rs` stores them; `on_engines` → `start_if_needed` evaluates `Need::Refresh` as the
  server does. A processor does not know the library's per-machine `stage_device` pins, so it
  ignores them (a false refresh costs about 1 MB and one drain).
- Reports (optional, ignored by old libraries): `HostInfo.backend_pack: Option<String>`,
  `HostInfo.pack_features: Vec<String>`, `UpdateReport.kind: Option<String>` = `"pack"`.
- Scheduling needs nothing new: a processor lists GPU formats for nar-small only once the new
  pack is loaded, because `engine_formats` applies the full text-box test, `bt_read_chars`
  included, and builds `format_engines` for GPU devices only (§10.4). Until then the library
  places those rows on its CPU or elsewhere.
- The refresh runs even with `processor.auto_update` off: it brings this release's own pack,
  not a new release.

### 16.8 What people see

- Log: "OCR backend: the installed cu130 pack (0.7.12) cannot run nar-small on the GPU (needs
  read-chars-1); fetching this release's pack (0.9 MB) in the background; nar-small runs on the
  CPU meanwhile", then "…staged and checked; restarting at a quiet moment to load it", then at
  start "OCR backend pack updated: cu130 for 0.8.0 (adds read-chars-1)".
- Admin → This server → OCR card: `overview().backend.need.refresh = {variant, lacks, rows,
  text, state: available|fetching|staged|waiting-quiet|failed, next_at, message}`; an "Update
  pack" button (a manual start that clears the back-off); a staged `.next-` shown as "update
  staged, swapped in at the next start".
- doctor: a check "OCR backend features": pass; warn (self-healing) "the cu130 pack of 0.7.12
  cannot run nar-small on the GPU; it runs on the CPU until the pack updates (automatic:
  <state>; or `mokuro-bunko install-ocr --refresh`)"; warn + needs_you for a lasting failure
  or `MOKURO_OCR_AUTO_INSTALL=false`.
- CLI: `install-ocr --refresh`; `install-ocr --list` prints each pack's features and what is
  missing; `--force` keeps its meaning.

### 16.9 Release and packaging changes

- `xtask torch-pack` writes `features` into `pack.json` and a second archive,
  `mokuro-bunko-backend-<ver>-<platform>-<variant>.thin.tar.zst`: `pack.json` first, then every
  file outside `lib_dir`.
- That name would parse as variant `cu130.thin` today. `parse_pack_name` skips any
  `.thin.tar.zst`, and `collect_backends` reads the thin archives separately to fill
  `BackendArtifact.thin`.
- `xtask manifest`: `BackendArtifact` gains `features: Vec<String>` and
  `thin: Option<ThinArtifact { url, sha256, size }>`, both `serde(default)`; 0.7.x installers
  ignore them. `xtask verify` checks the thin archive. `release.yml` uploads the 3-6 extra
  assets of about 1 MB each: its `dist/*.tar.zst*` glob already takes them, so no workflow
  changes. `publish.yml` only un-drafts the release.
- PACKAGING.md §8: "A feature an older pack lacks does not move the floor: the pack lists its
  `features`, and a host whose enabled rows need one it lacks fetches its release's pack (thin
  refresh)."

### 16.10 Opt-out

`MOKURO_OCR_AUTO_INSTALL=false` turns every automatic fetch off (the refresh is then offered
only). `MOKURO_OCR_PACK_REFRESH=auto|next-start|off` (env only, default `auto`). Without any
setting, `ocr.backend: cpu`, a row pinned to `cpu`, `MOKURO_TORCH_PACK` and a development pack
also keep a host from refreshing.

## 17. What is published, and by whom

| item | prepared by | published by |
|---|---|---|
| `textbox-graphs-v1` (nar's weightless ONNX graph, #102) | done | **owner** |
| `textbox-torch-v1`: nar fp32 + fp16 and charspot fp32 `.pt2` for 10 targets, `NOTICE.md`, the authors' `LICENSE`; sha256 and sizes pinned in `textbox_torch_models.json` | an agent builds (Linux CUDA via fakegpu, ROCm, Windows CUDA cross-built), stages the assets and a publish script, verifies the sha256 after upload | **owner** (`gh release create --prerelease`); a re-export is a new tag |
| `detector-torch-v1`: AnimeText fp32 `.pt2` for 10 targets, `NOTICE.md`, the AGPL-3.0 text | same | **owner** |
| backend packs with the new cdylib, `features` and the thin archives | `release.yml` at the 0.8.0 tag | **owner**: tag, then `publish.yml` |
| branches and PRs | agents, locally | **owner** |

No authors' weights are uploaded anywhere, and no package with weights inside is staged.

**Order (L4).**

1. `textbox-graphs-v1` (#102) is published before any of this ships; it is nar's CPU
   fallback.
2. Everything is staged and tested locally through `MOKURO_TORCH_MODELS_MIRROR`.
3. The NVIDIA gates run (L3): the lead runs them on the 24-core host with an RTX 4090, only
   while its GPU is idle. Code work does not wait for them; publishing does.
4. The owner publishes `textbox-torch-v1` once, after plans 01-03 pass their gates (nar fp32
   and fp16, charspot). No partial release that is later added to.
5. The owner publishes `detector-torch-v1` once, after plan 04, if the owner approves it
   (§22 Q1).
6. The compiled-in manifests are filled after publication and before the 0.8.0 tag.

## 18. Effect on users

- **Model downloads for the GPU routes**: nar ~3.7 MB of `.pt2` (weights shared with the CPU
  route); charspot ~5.8 MB plus the authors' 44 MB safetensors; AnimeText ~5.1 MB (the ONNX
  file is already there).
- **Backend pack**: fresh installs get the 0.8.0 pack. Upgraders on a GPU with these rows fetch
  about 1 MB (thin refresh) and restart once at a quiet moment. hayai-only hosts keep their
  pack (the floor stays 0.7.4), unless a hayai row asks for a patch budget below 512.
- **Releases sizes** (from v0.7.12 [v]): linux cu130 364,667,883 B; windows cu130 336,140,073 B;
  linux rocm7.1 1,992,294,400 + 1,235,713,627 B.

## 19. Risks

1. **fakegpu-built CUDA packages are untested for these graphs.** The spikes compiled on a real
   AMD device; the SDPA pin matters for nar. The first NVIDIA run gates publishing (§17,
   §20.5).
2. **fp32 on ROCm is BLAS-bound.** nar fp32 runs about 8.5 pages/s on an RX 9070 XT; AnimeText
   fp32 is MIOpen-bound. RDNA2/3 cards may be slower than the CPU pool on big hosts; the
   benchmark's `detect: cpu` and `engine: cpu` candidates (§12) and the fp16 option for nar
   cover it.
3. **charspot decode parity.** The authors' ONNX does NMS and the chain in-graph; the GPU route
   uses the Rust port. Gate on texts and boxes over ≥ 1,000 crops.
4. **Batch numerics.** nar fp32 ids were identical on 1,024 crops at b32 only. Gate at b1, b8
   and b32.
5. **Two models on one GPU.** Detect and engine device time add up (AnimeText 44 ms + charspot
   ~38 ms a page on the 9070 XT, about 12 pages/s). The planner sums them (§12).
6. **Host preprocessing becomes the cost.** About 190+ crops/s of bicubic resizing per GPU must
   run in Rust inside the pack; bounded by `threads`, overlapped with `par::map_ordered`.
7. **VRAM on small GPUs.** Peaks were 1.8-2.2 GiB per model. `max_batch` caps the text-box
   batch; the detector stays on the CPU under 4 GiB.
8. **The protobuf walker is new parsing code** in the pack: size check before parsing, bounded
   lengths, fixture tests.
9. **Pack refresh on Windows**: renaming a pack another process holds; hard-linking a loaded
   DLL (guess: `CreateHardLinkW` works on a loaded image). Both have a fallback (keep the
   record; copy).
10. **AGPL artefact** (`detector-torch-v1`): see §22.

## 20. Tests and verification gates

1. **Package parity (`verify.py`, every built package, on a device that runs it).**
   The crop set is JMangaBench_Read's 7,929 block crops (7,789 scored keys), compared with
   the project's `paired.py`. A "0.1%" limit is `floor(0.001 × n)` of that set's own count.
   - nar fp32: all ids up to and including the first end id (6817) identical to the authors'
     fp32 PyTorch on all 7,929 JMangaBench_Read block crops.
   - nar fp16: texts against the authors' fp16 on all 7,929 crops, at most 7 differing
     (`floor(0.001 × 7,929)`; measured 2). Ids past the end id are not compared.
   - charspot: texts identical to the authors' ONNX, boxes within 0.5 px, order identical, on
     ≥ 1,000 crops.
   - AnimeText: identical block sets against ONNX Runtime CPU on ≥ 300 real pages (DB test
     volumes plus JMangaBench_Read pages, spreads and odd canvases), every matched corner
     within 0.01 page px, no crop edge moved (`floor`/`ceil` as `page.rs`).
   - **A failed fp32 gate (L1).** That target is left out of the manifest, so that GPU uses
     the CPU route, and the failure goes to the owner. It never bumps a revision, and it never
     ships under a looser tolerance without the owner's sign-off. fp16 has its own gate (4).
   - **Which targets are verified (L2).** On real cards: gfx1201, gfx1030, gfx1032 (through
     the gfx1030 override), and sm_80 on Linux and on Windows. Every file copied to a test
     machine is hash-checked there. gfx1100, gfx1101, gfx1102, gfx1200 and sm_75 (Linux and
     Windows) have no test card: they ship built but unverified, as hayai's RDNA3 targets did
     (TORCH-BACKEND.md), and only once their verified neighbours pass from the same exporter
     commit. The docs list which targets were verified.
2. **Rust parity** (ignored tests with env vars, as `nar_parity` today): `nar_torch_parity`,
   `nar_torch_parity_fp16`, `charspot_torch_parity`, `charspot_decode_parity` (the Rust decode
   against `core.py` on dumped dense and tail outputs), `animetext_torch_parity`.
3. **Route parity.** One test volume through the CPU and the GPU routes: byte-identical
   sidecar text at fp32 (nar, charspot, AnimeText); the count of differing fp16 nar lines goes
   in the CHANGELOG.
4. **fp16 accuracy.** On full JMangaBench_Read (Hard only as a tie-breaker), fp16's composite
   is not below fp32's under `paired.py` (the 95% CI reaches 0; EM losses ≤ wins + 7, which is
   `floor(0.001 × 7,789)` keys). Batch stability at b1, b8, b32 on all 7,929 crops.
5. **Speed and the NVIDIA gate.** On the 24-core host with an RTX 4090 (the lead runs it, while
   its GPU is idle; L3) and on the RX 9070 XT: crops/s per batch for each package, pages/s end
   to end, which stage bounds, GPU utilisation; AnimeText b1/b4/b8 (decides the combiner); nar
   fp32 vs fp16; through the benchmark's width search with AnimeText and with the PP-OCR
   detector; against the CPU pool. With TF32 now off in `bt_init` (§9), the same run re-checks
   hayai and paddle sm_80 output on a test volume with the new pack. The lead puts the
   measured figures into `textbox_gpu_seconds` and `DETECT_GPU_SECONDS`. sm_75 has no test
   card (L2).
6. **Fallback matrix** (§13) with a dev pack, a 0.7.12 pack, the cpu pack, `ocr.backend: cpu`
   and a forced load failure (the weights file truncated in a scratch model store): each OCRs
   correctly with the reason logged.
7. **Unit tests** in each plan; every `cargo test` runs with `XDG_CONFIG_HOME` and
   `XDG_DATA_HOME` redirected.
8. **Live check** on a local trial server only: install the 0.7.12 rocm7.1 pack; serve the dev
   release (`MOKURO_BACKEND_MANIFEST`, `MOKURO_TORCH_MODELS_MIRROR`); add a nar-small row on
   AnimeText; expect about 1 MB fetched, a restart at a quiet moment, the swap, `.prev` gone,
   the row on `gpu:0`, detect on `gpu:0`, and the admin and doctor states at each step. Then a
   processor on its own storage, and `MOKURO_OCR_PACK_REFRESH=next-start`. Checks on any real
   deployment belong to the separate ops project, never this repository's sessions.

## 21. Build order and plans

| plan | covers | depends on | size (guess) |
|---|---|---|---|
| 01 foundations and nar-small fp32 | §7.1-7.3 (fp32), §9.1, §9.3 nar, §9.4 nar, §10.1-10.2, §10.4, §12 text-box, §14, §15 nar; the first NVIDIA run | #102 | 6-8 days + builds |
| 02 nar-small fp16 | §7.3 fp16, §9.2 cast + dtype check, §11 | 01 | 2 days |
| 03 charspot-small | §7.4, §9.3-9.4 charspot, §15 charspot | 01 | 3 days |
| 04 AnimeText on the GPU | §7.5, §9.2 ONNX weights, §9.3 detector, §10.3, §12 detect, §15 detector | 01 | 5-6 days |
| 05 pack refresh | §6 features, §16 | 01-04 merged for the final feature list | 4-5 days |

02, 03 and 04 each run on their own branch and worktree off 01, and they merge one at a time;
all three edit `package.rs`, `runtime.rs`, `abi.rs`, `ffi.rs`, `open_session` and
`bunko-vlm/src/textbox.rs`. `pack-inputs.txt` is re-blessed after each merge. 05 can start
after 01; each feature joins `BT_FEATURES` in the commit that adds its route (§6), so 05 lists
only routes that have landed. The 0.8.0 tag needs all five; one pack update carries all of it.

## 22. Open questions for the owner

The spec assumes the recommended answer to each; a different answer changes only the named
part. The questions keep their original numbers. Q2, Q3, Q6 and Q7 are settled as lead calls
L5-L8 (§2).

- **Q1. AGPL `.pt2`.** The weightless AnimeText `.pt2` is compiled from Ultralytics' AGPL-3.0
  model code (the mirrored ONNX already declares AGPL-3.0 in its metadata). Publish it in a
  separate pre-release `detector-torch-v1` with the AGPL-3.0 text and a NOTICE, used only by
  `gpl-models` builds? Recommended: yes. The alternative is no GPU route for AnimeText.
- **Q4. Restart when the refreshed pack is staged.** Restart by itself at a quiet moment (new OCR
  claims held on every machine until the running volumes and uploads finish, as `update.auto`
  does), or wait for the next start? Recommended: restart by itself; `MOKURO_OCR_PACK_REFRESH=
  next-start` keeps the waiting per host.
- **Q5. nar-small's default on GPUs.** Keep `auto-accuracy` at fp32 (recommended, as decided), and
  say in docs and admin that AMD users should pick `auto-balanced` or `fp16`? On an RX 9070 XT
  fp32 nar is about 8.5 pages/s, close to the CPU pool, and fp16 about 140, with the same
  JMangaBench_Read score. NVIDIA fp32 is not measured yet.

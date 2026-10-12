# The Vulkan route: ncnn and WebGPU side by side

Status: shipped in 0.7.1.

GPUs that no compiled libtorch package covers (AMD Polaris/Vega/RDNA1 on Linux; on request
any GPU with a Vulkan driver) read with one of two runtimes for the same engines:

* **ncnn** Vulkan (`crates/bunko-ncnn`): custom GEMM/attention kernels, decode loop and KV
  cache on the GPU.
* **webgpu**: ONNX Runtime's WebGPU EP over Dawn (`crates/bunko-vlm`): per engine a vision
  graph (paddle-manga's run layer by layer) and one shared decoder graph for prefill and
  every decode step, KV cache on the GPU (`tools/webgpu_export`).

Both are fp32 and text-identical to 0.5.2's torch fp32 on every card measured. Both ship:
when one has a situational problem on a machine the other fills the gap; when one is
complete and bugless it becomes the only one (see the scorecard, §7).

## 1. The pack

The runtimes are **not** linked into `mokuro-bunko`: ORT's WebGPU build makes
`libwebgpu_dawn` a NEEDED library of whatever links it (the binary would not start without
it), and ncnn + glslang add ~20 MB. Both live in one download-on-demand backend pack:

```
<storage>/backends/vulkan-<release>/
  pack.json            variant "vulkan", library libbunko_vulkan.so / bunko_vulkan.dll
  libbunko_vulkan.so   crates/bunko-vulkan (cdylib): ncnn (static) + ORT WebGPU (static)
  lib/libwebgpu_dawn.so  Dawn (found through the library's $ORIGIN/lib run path)
  THIRD_PARTY.md       ncnn BSD-3, glslang (BSD-3/MIT/Apache-2.0), ORT MIT, Dawn BSD-3 + Abseil/Tint/SPIRV-Tools Apache-2.0
```

It exports the libtorch pack's C ABI (`bunko_torch::abi`, ABI 1) unchanged, so the host opens
it with the same loader. Additive JSON fields (old packs and hosts ignore them):

* `DeviceEntry.driver`, `.uuid`, `.backends` (the runtimes that serve the device);
* `LoadOptions.backend` (`ncnn` / `webgpu`; a libtorch pack ignores it).

`bt_devices` lists only GPUs (kind `vulkan`, id `gpu:<catalog index>`, arch = PCI ids
`1002:67df`, formats `fp32`); no CPU entry (the host's CPU recognizers come from its libtorch
pack). The catalog comes from the system Vulkan loader (`bunko_vulkan::probe`, `ash`
opened at run time): vendor/device ids, driver name+info, Vulkan version, the largest
device-local heap, `deviceUUID`, PCI address. Software rasterizers are never offered.

## 2. The runtime contract (`crates/bunko-vulkan/src/backend.rs`)

```rust
pub trait VkBackend: Send + Sync {
    fn id(&self) -> &'static str;                 // "ncnn" | "webgpu"
    fn version(&self) -> String;                  // part of the verdict cache key
    fn engines(&self) -> &'static [&'static str]; // engines it has a loader for
    fn serves(&self, catalog: &[VkDevice]) -> Vec<usize>;   // catalog indices
    fn load(&self, req: &LoadRequest<'_>) -> Result<Box<dyn VkRecognizer>, String>;
}
pub trait VkRecognizer: Send + Sync {
    fn read_crops(&self, crops: &[&Rgb], caps: Option<&[u32]>) -> Result<Vec<String>, String>;
    fn batch(&self) -> usize;
}
```

`LoadRequest` = engine, the runtime's graph directory for it, the catalog device, the host
files (`LoadOptions`: tokenizer, position table, embeddings, patch budget) and the
`RecognizerInfo`. Adapters: `bunko-vulkan/src/ncnn.rs`, `bunko-vulkan/src/webgpu.rs`.

What the adapters need from the runtime crates (keep these stable or update the adapter in
the same commit):

| runtime | crate API used |
|---|---|
| ncnn | `bunko_ncnn::devices() -> Vec<VkDevice{index,name,kind,..}>` (loader order, software rasterizers left out); `NcnnHayai::load(&HayaiFiles{graphs,pos_table,tokenizer}, index, flags, info)`, `.read_crops(&[&Rgb])`; `NcnnPaddle::load(&PaddleFiles{graphs,tokenizer}, index, flags, info)`, `.read_crops(&[&Rgb], caps)`; `self_check(index, flags)`; `NcnnError::Vram` when the model does not fit the free heap. Flags: 0, or `FLAG_FP16_STORAGE` (opt-in, not parity). Graph files: whatever `bn_hayai_load` / `bn_paddle_load` read from `graphs`. |
| webgpu | `bunko_vlm::{HayaiNova (+ set_gpu_vision_rows), HayaiAssets{.., gpu: Some(HayaiGpuGraphs)}, PaddleManga, PaddleAssets{.., gpu_dir: Some(dir)}, paddle::webgpu_complete, OrtSessionFactory, SessionOptions{device: Gpu(0), provider: WebGpu}}`; graph files below, each engine's in one directory; the process-wide `WebGpuGuard` inside bunko-vlm. |

Device mapping: ncnn numbers devices in loader order — matched to the catalog by name (n-th
of a name ↔ n-th catalog entry of that name). The WebGPU EP opens Dawn's high-performance
adapter: the first discrete GPU in loader order, else the first integrated one; it serves
that device only.

## 3. Model files

Per engine and runtime, under the model store (manifest ids `vulkan/<engine>/<runtime>/<file>`):

| runtime | hayai-nova files | size |
|---|---|---|
| ncnn | `vision.param/.bin`, `prefill.param` + `step.param` sharing `decoder.bin`, `token_embeddings.f32`, `ncnn.json` (graphs format 2) | 0.60 GB |
| webgpu | `hayai-nova_webgpu_{vision,decoder}_fp32.onnx` | 0.57 GB |

WebGPU graphs are built and verified by `tools/webgpu_export` (reproducible bytes; `verify`
reads a crop set on the CPU or the GPU and must match torch fp32 exactly). paddle-manga
also needs the existing embeddings (`LoadOptions.embeddings`) and tokenizer.

| runtime | paddle-manga files | size |
|---|---|---|
| ncnn | `vision.param/.bin`, `prefill.param` + `step.param` sharing `decoder.bin`, `embeddings.f32`, `pos_table.f32`, `ncnn.json` (prompt, vocab) | 3.62 GB; needs ~4.6 GiB free VRAM (refused below: `NcnnError::Vram`) |
| webgpu | `paddle-manga_webgpu_{weights,embed,layer,qkv,attn,post,tail,decoder}_fp32.onnx`, `paddle-manga_webgpu_weights_fp32.onnx.data`, `paddle-manga_webgpu_vision.json` | 3.20 GB; 6.3 GB VRAM peak, refused below 7 GB |

Host files shared with libtorch (tokenizer, position table, token embeddings) come from the
existing manifest ids. Self-test goldens: `vulkan/selftest/<engine>.json` (§5).

## 4. Candidates, benchmark, fallback (host: `crates/bunko-engines/src/vulkan.rs`)

The rule: the user can override everything; with the defaults every backend is measured by
the machine's benchmark and the winner is used; a short list of hardcoded exceptions keeps
runtimes that measurably offer nothing out of the way; a runtime that does not work on a GPU
is skipped there.

* **Candidates.** With `ocr.backend: auto` every backend that can drive a GPU is a
  candidate: libtorch (the CUDA/ROCm pack, where its architecture is covered), ncnn and
  WebGPU (the Vulkan pack), and the CPU as the floor. The catalog lists each GPU once:
  - a GPU libtorch drives keeps its libtorch formats (`fp32`, `bf16`, `fp16`) and gains
    `fp32@ncnn`, `fp32@webgpu` (bunko-sched `BACKEND_SEP`: a precision on another backend);
  - a GPU only the Vulkan pack drives (Polaris, Vega, RDNA1, ...) is `gpu:<n>` after the
    libtorch GPUs, provider `vulkan`, formats `fp32` (its first runtime) and
    `fp32@<other runtime>`.
* **Benchmark.** The machine's existing benchmark measures them (bunko-sched
  `needs_bench`): balanced/speed modes over every precision × backend, and now also the
  accuracy mode and forced precisions over the backends of their one precision. Same sample,
  same trial rules, same 5% tie (the earlier = libtorch / the first runtime wins a tie). The
  pick (`fp32@ncnn`) travels as `precision_pick`; the engines resolve it to (precision,
  backend). Until a machine has benchmarked, the first candidate serves (libtorch, or the
  first Vulkan runtime that passes its self-test), so OCR starts at once.
* **Skipped, not failed.** A runtime that does not load or fails its self-test on a device
  is left out of that device's formats with its reason (verdict cache) — no error to the
  user, it simply is no candidate. A libtorch GPU whose Vulkan pick cannot start falls back
  to libtorch's own fp32.
* **Exceptions** (`vulkan::EXCEPTIONS`): runtime × engine × OS × vendor × gfx/sm prefix ×
  beside-libtorch → `exclude`, `force` (the only fp32 candidate) or `fallback-only` (not
  benchmarked, not fetched ahead; fetched and self-tested only when everything ranked above
  it fails on that device — libtorch, then the candidate runtimes), each with its evidence.
  Shipped: ncnn and WebGPU fallback-only on NVIDIA GPUs the CUDA pack drives (0.40–0.53x of
  CUDA), WebGPU fallback-only for paddle-manga on AMD (ncnn faster on every card, 7 GB vs
  4.6 GB of VRAM). Doctor and admin show "fallback-only: <reason>". An NVIDIA + CUDA machine
  fetches no Vulkan graphs up front (the ~18 MB pack only).
* **Overrides.** `ocr.backend: vulkan` pins the Vulkan runtimes in `ocr.vulkan_backends`
  order (first that passes, no benchmark); `cuda`/`rocm` pin libtorch; `cpu` the CPU.
  `ocr.vulkan_backends` (default `[ncnn, webgpu]`) orders and limits the runtimes; the
  processor reads `MOKURO_VULKAN_BACKENDS`.
* **Verdicts** are cached in `<backends>/vulkan-state.json` keyed by (runtime, pack release
  + `pack.json` stamp, driver, device UUID, engine, model files' sizes+mtimes): re-tested
  when any of them changes.
* **Crash safety:** a marker is written before a runtime loads, self-tests and reads its
  first batch; one left by a dead process demotes that runtime on that device at the next
  start (`reap_crashes`).
* **Runtime failure** mid-volume: the runtime is demoted on that device and the same read
  is retried on the next runtime; with none left the session's GPU→CPU fallback applies.
* **Downloads stay on demand.** `install-ocr` fetches the Vulkan pack (~18 MB) first and
  the libtorch pack second on a machine with a GPU (`hwdetect::vulkan_wanted`); a runtime's
  graphs (~0.8–1 GB each) are fetched when a benchmark trial or a session first needs them
  (`models download` on a Vulkan-only GPU fetches them smallest first).
* **Windows.** Windows lists its GPUs through DXGI: AMD and Intel adapters (no libtorch GPU
  pack exists for them there) get the Vulkan pack and the CPU pack as the floor, NVIDIA goes
  through its driver check as before, the software rasterizer is ignored. A machine with an
  NVIDIA card and an Intel iGPU gets CUDA for the one and the Vulkan pack for the other.

## 5. Self-test

A runtime proves itself on a device before it reads a page: it reads the engine's golden
crops (`vulkan/selftest/<engine>.json`: 16 synthetic Japanese lines rendered from Noto
Sans/Serif CJK by `tools/vulkan_selftest/make_crops.py`, vertical and horizontal, grey and
blurred ones, no copyrighted pages) and must return the golden texts exactly — what libtorch
fp32 on the CPU reads from them (`examples/vulkan_check golden`). Any difference skips the
runtime on that device (cached until runtime/pack/driver/model change). libtorch itself is
the reference and is not self-tested.

Checking a runtime by hand: `MOKURO_VULKAN_PACK=… MOKURO_VULKAN_MODELS_DIR=…
cargo run -p bunko-engines --example vulkan_check -- check hayai-nova <golden.json>
--runtime ncnn|webgpu [--device gpu:N]`.

## 6. Surfaces and tests

* `doctor` ("Vulkan runtimes"): the pack, the mode (benchmark candidates / pinned), per GPU
  its driver (RADV or not), whether libtorch drives it too, each runtime × engine as
  candidate or skipped with the reason, and runtimes a crash demoted.
* Admin → This server: "GPU runtimes" row (the same report); "Run it on" offers
  Vulkan (ncnn / WebGPU). The benchmark's trials and winner (`fp32@ncnn` …) are in the
  generations' precision view, as every precision trial.
* `install-ocr`: `--variant vulkan`; with a GPU, `--if-needed` installs the Vulkan pack
  first (never fatal) then the libtorch pack.
* Tests: `bunko-engines` `vulkan::tests` (selection, cache, mismatch, load failure,
  crash marker, mid-volume failover with fake runtimes; exceptions; KFD map),
  `precision::tests::backend_qualified_formats_resolve_to_a_backend`,
  `bunko-sched` `backend_variants_are_benchmarked_in_every_mode`. End to end (desktop
  9070 XT, RX 6600): server local OCR, the machine benchmark choosing among
  `fp32`/`fp32@ncnn`/`fp32@webgpu`, `ocr.backend: vulkan`, a broken runtime and an induced
  crash (`BUNKO_VULKAN_ABORT=<runtime>`, a test hook in the pack) handing over to the other.

## 7. Scorecard

"Victor" = complete (both engines, every GPU class it is offered on) and bugless (no open
item). Measured 2026-10-11/12 through the real paths (server local OCR with the machine
benchmark, or `examples/ocr_volume`) on a 196-page manga volume, or its first 20 pages
where noted.

| | ncnn | webgpu |
|---|---|---|
| hayai-nova parity | 196/196: RX 6600, 6900 XT, 9070 XT, RTX 4060 (Linux), RTX 3070 (Windows); 20/20 through the server (9070 XT, RX 6600) | 196/196 with the shipping graphs (shared decoder): RX 6600, 6900 XT, 9070 XT, RTX 4060 (Linux); 20/20: RTX 4090, RTX 3070 (Windows, D3D12) |
| hayai-nova p/s, 196 p, RX 6600 / 6900 XT / 9070 XT | 1.119 / 1.893 / 2.392 (RTX 4060 1.006 = 0.40x CUDA pack; RTX 3070 Win 0.925) | 1.071 / 2.092 / 2.282 (3-graph build; the shipping shared-decoder graph ~3% slower) |
| paddle-manga parity | exact (196 p, 3201 lines) on RX 6600, 6900 XT, 9070 XT, RTX 4060, RTX 3070 Win; 20/20 through the server (RX 6600) | 196/196 (3201/3201 lines) = 0.5.2 eager fp32: RX 6600, 6900 XT, 9070 XT, RTX 4060; 20/20: RTX 4090, RTX 3070 (Windows) |
| paddle-manga p/s, 196 p, RX 6600 / 6900 XT / 9070 XT | 0.144 / 0.287 / 0.398 (RTX 4060 0.141 = 0.49x CUDA pack; RTX 3070 Win 0.146); needs ~4.6 GiB free VRAM | 0.122 / 0.211 / 0.215 (RTX 4060 0.154); 6.3 GB VRAM peak, refused below 7 GB |
| store install + server e2e (`install-ocr --from` a model mirror, autobench, 20 p) | 9070 XT beside ROCm: self-test pass both engines; picked for hayai fp32 (3.45 vs libtorch 1.85, WebGPU 2.63 p/s) and paddle (0.47 vs libtorch 0.16, WebGPU 0.09); RX 6600 (no ROCm pack): picked for both (hayai 1.49 vs WebGPU 1.40; paddle 0.175 vs 0.029); 20/20 = libtorch fp32 for both engines on both cards | self-test pass both engines on both cards; lost every benchmark above |
| exceptions | fallback-only beside the CUDA pack on NVIDIA (0.40–0.53x of CUDA) | fallback-only beside the CUDA pack on NVIDIA; fallback-only for paddle-manga on AMD (behind ncnn) |
| Windows 11, RTX 3070, real pack | `install-ocr --variant vulkan --from` a model mirror; `ocr.backend: vulkan`: self-test pass both engines; 20/20 = libtorch fp32 for both engines; 20 p: hayai 17.9 s, paddle 131 s | same, D3D12: self-test pass both engines; 20/20 both; hayai 13.7 s, paddle 114 s |
| Windows default settings (CUDA pack + Vulkan pack) | fallback-only (doctor), no Vulkan graphs fetched, sessions on CUDA; the pack is shut down before the process exits (Windows unloads the Vulkan driver first: without it every process ended 0xC0000005) | same |
| devices served | every catalog GPU (matched by vendor/device id) | Dawn's high-performance adapter only (first discrete, else integrated) |
| Windows | shim + pack built on Windows (RTX 3070 runs); AMD/Intel auto-install covered by unit tests only (no such Windows hardware tested) | `bunko_vulkan.dll` cross-links (cargo-xwin, MSVC 14.51); RTX 3070: both engines 20/20 exact |
| gfx803 / Polaris | untested (no card; kernels wave-agnostic and self-checked) | untested |
| forced failures (server, 9070 XT) | induced crash in self-test → demoted at the next start, WebGPU read 20/20; unloadable graphs → WebGPU read it | — |
| fixed before release | host loaded paddle `embed-fp32` (424 MB) ncnn does not use → per-runtime host files; paddle-manga was offered `fp32@webgpu` → engine-scoped formats | process-exit crash on NVIDIA (contexts released before the Vulkan driver unloads) |
| open items | paddle on <4.6 GiB cards reads on the CPU; failed attention self-check = no paddle; fp16 storage opt-in only (176/196 identical); multi-GPU untested; a crash after the first batch is not attributed | multi-GPU adapter choice; process-wide WebGPU lock serialises engines; ~75-80% (hayai) / ~55% (paddle) of 0.5.2 eager on RDNA2 (the EP's fp32 MatMul at ~42% of peak) |

Not victor yet: both runtimes now read both engines, but neither is proven on Polaris, and
WebGPU reaches one GPU per machine. Both stay; the machine benchmark picks per GPU and engine.

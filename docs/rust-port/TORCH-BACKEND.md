# libtorch recognizer backend (0.7 parity plan)

**Status:** design, 2026-10-02. **Decision (owner):** 0.7 aims at *parity with 0.5.2, in Rust*.
The recognizers (hayai-nova, paddle-manga) run on **libtorch** — the same torch 0.5.2 ran — on
NVIDIA CUDA, AMD ROCm (Linux) and the CPU. ONNX Runtime stays only where 0.5.2 used it: the
PP-OCR detector + CTC reader of ppocr-manga, on the CPU (0.5.2 `ppocr.py`, CPUExecutionProvider).
The ORT recognizer path (bunko-vlm on ORT) and every ORT GPU execution provider are **deferred**:
kept in the tree behind a non-default feature, not built for release, not tested.

Evidence: the backend shootout (2026-10-01/02, `~/.cache/mokuro-bunko-demo/tmp/shootout/`,
`results.jsonl`): libtorch AOTInductor beat tuned 0.5.2 by +31% (RTX 4090) / +69% (RX 9070 XT) on
hayai-nova, 1.2–1.5x on paddle-manga, exact crop parity with 0.5.2 at every precision.

## Scope (parity with 0.5.2)

| platform | 0.5.2 had | 0.7 recognizer backend |
|---|---|---|
| Linux x86_64 | CUDA, ROCm, CPU | libtorch cu130 / rocm7.1 / cpu |
| Windows x86_64 | CUDA, CPU | libtorch cu130 / cpu |
| macOS arm64 | CPU (its MPS option never reached the engines) | libtorch cpu |
| Docker | `Dockerfile` (CPU), `Dockerfile.unraid` (CUDA) | one full image; the cpu / cu130 / rocm7.1 pack is downloaded on first start for the GPU the container is given (`-cuda` is a tag of it) |

Not in scope: Android/iOS, Windows AMD, pre-Turing NVIDIA, Intel GPUs, Intel Macs, Metal (MPS).
The lite build is unchanged (no OCR, no ONNX Runtime, no libtorch).

## Architecture

The `mokuro-bunko` binary never links libtorch, so it starts (and serves) with no OCR backend
installed — as 0.5.2's server ran before `install-ocr`. libtorch arrives as a **backend pack**:

    <storage>/backends/torch-<variant>-<torch version>/
        lib/            libtorch runtime libraries (+ CUDA/ROCm libs it bundles)
        libbunko_torch.so|.dll|.dylib   our cdylib, linked against exactly this libtorch
        pack.json       variant, torch version, ABI version, the mokuro-bunko release
                        it belongs to (bunko_version), file list with sha256

`variant` ∈ `cpu`, `cu130`, `rocm7.1` (Linux), `cu130`/`cpu` (Windows), `cpu` (macOS arm64).

### `crates/bunko-torch` (cdylib + rlib)

Built **per pack variant** (not part of the normal workspace build of `mokuro-bunko`). Uses
`tch` 0.26 (= libtorch 2.13.0) and the AOTInductor C++ shim (`shim/aoti_shim.cpp`, from the
shootout harness, ported to `LoadLibrary` on Windows). Holds the hayai-nova and paddle-manga
generation loops (the shootout's `torch.rs`). Exposes a small, versioned **C ABI** at the
recognizer level — no tensors cross it:

    bt_abi_version() -> u32
    bt_devices(json_out) -> status                // CUDA/ROCm devices, arch (sm_89 / gfx1030), VRAM; cpu ISA
    bt_load(engine, model_dir, precision, device, opts_json, err_out) -> handle
    bt_read(handle, crops: [{rgb ptr, w, h}], n, caps, out_json, err_out) -> status
    bt_free(handle); bt_free_str(ptr)

Images are passed as RGB8 crops already cut by bunko-vlm's crop code (so cropping stays
bit-identical across backends); preprocessing that 0.5.2 did in torch happens inside the cdylib.

### Main-binary side (`bunko-engines`, feature `torch`, on in `full`)

Release lock (0.7): the loader opens only a pack whose `pack.json`
`bunko_version` equals its own version (`torch::loader::RELEASE`; empty = a development
pack, accepted), then checks the ABI. A release and its pack are installed, updated and
rolled back together (PACKAGING.md §4, docs/configuration.md "Automatic updates").

`libloading` opens the pack's `libbunko_torch`, checks `bt_abi_version`, and wraps the handle
in a `Recognizer` (the existing trait). Device catalog, placement and precision selection use
`bt_devices`. If no pack is installed or loading fails, the processor reports no GPU engines
for that device (CPU ppocr-manga still works) and says why — as 0.5.2 did without its venv.

### Compiled model packages (stream B, `tools/torch_export/`, 2026-10-03)

AOTInductor `.pt2` per engine × precision × **target**, weights shared:

    <storage>/models/torch/<engine>/<precision>/
        weights-vision.safetensors      every target's vision graph binds these (GPU and CPU)
        weights-decoder.safetensors     prefill + step (+ paddle's input embeddings)
        <target>/{vision,prefill,step}.pt2   code only (1-10 MB each)

`target` = `<os>-<backend>-<arch>`: `linux-cuda-sm_75` (Turing: fp32/fp16), `linux-cuda-sm_80`
(Ampere and newer), `linux-rocm-gfx1030` (RDNA2, incl. gfx1031/1032 via
`HSA_OVERRIDE_GFX_VERSION=10.3.0`), `linux-rocm-gfx1201` (RDNA4; gfx110x/gfx1200 build the same
way, untested: no card), `linux-cpu-x86_64-v3` (fp32), `linux-cpu-x86_64-v4bf16` (bf16,
AVX512_BF16 hosts), `windows-cuda-sm_75|sm_80`, `windows-cpu-x86_64-v3`, `macos-cpu-arm64`.
Also built: `sm_86`, `sm_89`, `sm_120` (identical speed to `sm_80` on sm_86/89, see A1; not
needed). CPU targets are hayai-nova only: paddle-manga is a GPU-only engine (2026-10-08, owner
decision; a CPU read took ~1 s a crop, ~27 s a page, from a 4.65 GB package per platform).

**Tool.** `python -m torch_export build -e hayai-nova -p bf16 -t linux-cuda-sm_80`; `matrix`
(every combination, skips finished ones); `manifest` (`torch-models.json`); `check-isa`;
`fetch-windows-cross`; `python -m torch_export.verify` (load on this machine's device, bind the
shared weights as the runtime does, compare with eager). Linux targets compile in a
glibc-2.28 container (`docker/Dockerfile`, manylinux_2_28 + gcc-toolset-14, pinned by digest;
`--host` to skip, for experiments). No GPU is needed for any target (`fakegpu.py`), so CI
needs no GPU runners. Pinned stack: torch 2.13.0 (+cu130 / +rocm7.1 / cpu), the CUDA 13.0
pip wheels as toolkit (`requirements-*.txt`), llvm-mingw 20260922 for Windows.

**What a build does.**
1. Loads the model on the CPU exactly as onnx_export does (pins checked against 0.5.2).
   hayai-nova: Linear/Conv weights *pre-cast* to the autocast dtype (bit-identical to
   autocast's per-call cast; removes the casts and halves the weights). Both engines:
   q|k|v and gate|up as one GEMM each (`FusedNovaDecoder`, `FusedPaddleDecoder`: the same
   fusion inductor's `freezing` does, done once in the weights; `TORCH_EXPORT_FUSE=0` off).
2. Writes `weights-<group>.safetensors` (byte-reproducible writer; every target of an
   engine × precision must produce identical bytes or the build fails).
3. Exports on fake tensors of the target device, pins every `scaled_dot_product_attention` to
   the kernel the target's own `select_sdp_backend` picks (`sdpa.py`; CUDA rules from
   `sdp_utils.cpp`, ROCm rules probed on gfx1201/gfx1030 with `sdpa_probe.py`), then compiles
   with inductor told it runs on the target (`fakegpu.py`): Triton kernels compiled for the
   target's arch, the heuristics' first config, never launched; no pad_mm benchmarking.
4. Post-build: clears `PT_GNU_STACK` X (glibc ≥ 2.41 refuses RWE), checks every native
   object needs no newer `GLIBC_`/`GLIBCXX_`/`CXXABI_` than libtorch itself (2.28 / 3.4.22 /
   1.3.11; container builds need 2.14 / 3.4.21 / 1.3.11), no AVX-512 in x86-64-v3 code, then
   normalizes the zip (drops inductor's compile-command trailer from the bundled .cpp,
   derives `serialization_id` from content): two builds of a target are byte-identical.
5. `build.json` per target: options, sha256s, sizes, ABI needs, timings.

**Graph I/O (`bunko.io` metadata).** v1 = the shootout's: prefill/step return
`(logits, *present)`. v2 (default): prefill returns `(next_ids i64[b], live bool[b], *present)`;
step takes `(embeds, mask|cos.., live, *past)` — `live` right after the v1 inputs — and returns
the same triple; `next = where(live, argmax(logits), fill)`, `live' = live & next≠eos
[& next≠pad]` (hayai fill/pad 16000, eos 16002; paddle fill 0, eos 2; paddle caps stay on the
host: AND them into `live` before the step). Other metadata: `bunko.weights` (constant FQN →
weights key; `{}` = weights inside), `bunko.engine/role/precision/target/torch/tool`,
`bunko.special` (hayai), `bunko.prompt` (paddle), `bunko.fused`. The decoder blob's
safetensors metadata `bunko.alias.host.embed_tokens` names paddle's input-embedding table.

**Windows.** A Linux package does not load on Windows (ELF vs PE). `windows-cuda-*` targets are
cross-compiled on Linux (torch's `cross_target_platform="windows"`, llvm-mingw, the Windows
torch/CUDA wheels' import libraries; no Windows machine, no GPU) and are weightless like
Linux: they bind the same shared weights files. Binding: libtorch's MSVC-built
`AOTIModelPackageLoader::load_constants` passes the wrapper a `std::unordered_map*`
(`AOTInductorModelContainerUpdate[UserManaged]ConstantBuffer`, the "V1 API"), which a MinGW/libc++
wrapper reads with the wrong STL layout (crash in `std::__1::__hash_table::find`). torch 2.13 also
exports the C-ABI-safe `AOTInductorModelContainerUpdateUserManagedConstantBufferPairs(container,
const AOTInductorConstantMapEntry{const char* name; AtenTensorHandle handle;}* pairs, n,
use_inactive=false, validate_full_update=true)` from every wrapper, but the 2.13 loader never calls
it. The runtime must call it itself on Windows (metadata `bunko.bind = "pairs"`): get the wrapper's
container handle, enumerate constants with `AOTInductorModelContainerGetNumConstants` /
`GetConstantName` / `GetConstantOriginalFQN` (the pairs are keyed by the internal constant
*name*, not the FQN; `bunko.weights` maps FQN → weights key), build the pairs from
`AtenTensorHandle`s of the shared tensors, call Pairs. Proven on pimax with a ctypes driver over
the wrapper's C ABI (`wincheck.py`): paddle fp32 sm_80 bound 443/110/110 constants, crops 100/100
vs 0.5.2 fp32 (identical to the embedded-weights packages' texts); device memory peak 5.15 GB
vs 5.97 GB with per-package weights (−0.82 GB here, −1.24 GB once the runtime's own embedding
table is counted, which the embedded layout uploaded separately). Packages: 4–5 MB per graph
instead of 1.4–1.8 GB. (`TORCH_EXPORT_WIN_EMBED=1` rebuilds the old weights-inside layout.)
The wrapper is renamed `*.wrapper.pyd` (the Windows loader's extension). Windows CPU packages
are built natively with MSVC (`winpatch.py` applies the shootout's inductor fixes in memory;
`cpp.simdlen=256`, i.e. `/arch:AVX2`, whatever the build host has: 0 zmm), since 2026-10-08 on a
GitHub `windows-latest` runner (`.github/workflows/torch-export-win.yml`, manual dispatch only:
build, AVX-512 check, weights = the release's, `verify.py` against eager, optionally the
embedded layout built and timed beside it and a generated volume read with both through the
Rust runtime; the package is a workflow artifact, nothing is published; the runner's MSVC
(VS 18) cannot compile torch-sys 0.26's libtch against libtorch 2.13 in either C++17 or C++20,
so the workflow builds the backend pack's C++ with clang-cl, as the cross-built packs are).
They are weightless
like every other package and bind through the same C-ABI Pairs entry point. Cross-building them
on Linux was not adopted: unlike a GPU wrapper (C shim calls only), the CPU kernels would be
compiled by clang/MinGW with a different OpenMP runtime (libomp instead of MSVC's vcomp140,
whose thread count the runtime sets) and checked on no Windows machine. macOS arm64 packages
are built natively on the Mac (hayai fp32); the build rewrites the wrapper's install
name (`@rpath/<file>`) and its libomp import (`/opt/llvm-openmp/lib/libomp.dylib` →
`@rpath/libomp.dylib`, satisfied by libtorch's libomp), drops absolute LC_RPATHs and re-signs ad
hoc. Every Linux/macOS package is checked for absolute build-host paths in ELF
RUNPATH/RPATH/NEEDED/SONAME, Mach-O install name/imports/LC_RPATH and the package metadata
(OpenMP source-location strings with the build path remain inside CPU kernels; harmless).

**Run-to-run differences (paddle bf16 on pimax, 6/196 pages between two full runs).** Not the
package: on the RTX 3070 (and the 4090) every graph is bitwise repeatable for the same inputs
(vision/prefill/step 30/30 identical, `det2.py`; eager SDPA flash/efficient and the GEMMs too).
But a row's result depends on the *shape* of the batch it is read in: same row, other rows
changed → bitwise equal; batch of 12 vs 5 vs 1, or 3/20 extra left-padding positions → KV
differs (bf16, max |Δ| ≈ 0.6–1.0) and the argmax can flip — identically on Linux and Windows
(GEMM/attention kernels are chosen per shape). So two runs that group a page's lines into
different batches (the pipeline's batching depends on timing) read some lines differently.
`CUBLAS_WORKSPACE_CONFIG`/deterministic-algorithm settings cannot help (a given shape is
already reproducible); the fix is in the runtime: form batches from page content only
(e.g. per page, in reading order), never from arrival timing. (0.5.2's eager torch has the
same shape sensitivity.)

**Download/install.** Release assets stay one file per package (`.pt2`, stored zip, sha256 in
the manifest). Each archive has exactly one top folder `<role>/` (checked by `manifest`); the
installer extracts that folder's *contents* into `unpack_to` (`<target>/<role>/`, i.e. strips the
top component — extracting the archive as is gives the doubled `<role>/<role>/data/...`) and the
runtime loads `unpack_to` in place (no per-load extraction to TMPDIR). Manifest ids = store
paths (`torch/<engine>/<precision>/<target>/<role>.pt2`, `torch/<engine>/<precision>/weights-<g>.safetensors`).

#### A1 — compile without the target GPU: yes, for every target

All packages above were compiled on the desktop (RX 9070 XT, **no NVIDIA GPU**) inside the
glibc-2.28 container, without touching any GPU, and run unchanged on the fleet (crop gates:
220 hayai / 100 paddle crops vs 0.5.2 torch on that card at that precision):

| package (built on desktop) | ran on | result | crops/s (0.5.2 / shootout libtorch) |
|---|---|---|---|
| hayai bf16 `linux-cuda-sm_89` | beast RTX 4090 | 220/220 | **377** (0.5.2 336 / shootout 334; same graphs compiled *on* the 4090 with autotuning: 360) |
| hayai fp32/bf16/fp16 `linux-cuda-sm_80` | beast 4090 · lily RTX 4060 | 220/220 ×6 | 190/380/383 · 49/133/132 (lily shootout bf16 123) |
| hayai fp32/fp16 `linux-cuda-sm_75` (PTX JIT on sm_89) | beast · lily | 220/220 ×4 | 187/376 · 49/131 (+0.3–0.5 s first-load JIT) |
| hayai bf16 `windows-cuda-sm_86` / `sm_80` (cross-built on Linux) | pimax RTX 3070, Windows | 220/220 ×2 | 126.5 / 130.7 (shootout, compiled on pimax with MSVC: 122.2) |
| paddle bf16 `windows-cuda-sm_80` (cross-built) | pimax | 100/100 | 29.3 (shootout on pimax: 27.7) |
| hayai fp32/bf16/fp16 `linux-rocm-gfx1030` | server 6900 XT · steven 6800 · patrick 6600 (gfx1032, HSA override) | 220/220 ×9 | 88/44/115 · 64/34/85 · 37/21/50 (0.5.2 on server: 65/34/72) |
| hayai bf16 `linux-rocm-gfx1201` | desktop 9070 XT | 220/220 | 174–183 (shootout frozen: 175–184) |
| paddle fp32/bf16/fp16 `linux-cuda-sm_80` | beast · lily | 100/100 ×5 | 32.4/97.5/99.4 · –/26.3/26.5 (shootout 4090: 31.9/94.9/96.5, lily bf16 26.1) |
| paddle fp32/bf16 `linux-rocm-gfx1030` | server | 100/100 ×2 | 12.6/7.7 (0.5.2: 12.2/6.5) |
| hayai fp32 `linux-cpu-x86_64-v3` | lily Zen 3 (AVX2 only) | 220/220 | 5.5 (8 threads) |
| hayai bf16 `linux-cpu-x86_64-v4bf16` | desktop Zen 4 | 220/220 (= shootout CPU bf16) | 18.4 (16 threads, host load 16) |
| paddle fp32 `linux-cpu-x86_64-v3` | desktop Zen 4 | 100/100 | 1.05 (16 threads) |
| hayai, paddle fp32 `macos-cpu-arm64` (built on the Mac) | M2 Pro | ids equal to eager, max |Δ| 2e-5 / 7e-5 | (verify.py; no crop harness on the Mac) |
| hayai, paddle fp32 `windows-cpu-x86_64-v3` (native MSVC on pimax) | pimax 3800X | ids equal to eager, max |Δ| 9e-6 / 2e-5 | (verify.py) |
| **weightless CPU (2026-10-08)**, vs the embedded packages, alternating runs, Dr Stone 01 20 pages | | | |
| hayai fp32 `linux-cpu-x86_64-v3` (container) | Ryzen 7 5800X, idle | ids = eager (= embedded's max |Δ|); `.mokuro` byte-identical to embedded (8/8 runs) | 0.256 vs 0.257 p/s; peak RSS 1643 vs 1864 MiB (one buffer per weights file; 1714 with one allocation per tensor) |
| hayai bf16 `linux-cpu-x86_64-v4bf16` (container) | Ryzen 9 7950X, host load 18–30 | ids = eager; `.mokuro` byte-identical (14/14) | 0.709 vs 0.723 p/s (noise: same-process graph times equal); peak RSS 1220 vs 1337 MiB |
| hayai fp32 `macos-cpu-arm64` (on the Mac) | M2 Pro | ids = eager; `.mokuro` byte-identical (36/36, = the Linux runs') | 37.9 vs 38.8 s per 20 pages (n=9 rounds, within noise); max RSS 2030 vs 2238 MiB |
| hayai fp32 `windows-cpu-x86_64-v3` (GitHub runner, MSVC) | `windows-latest`, 4 vCPU | ids = eager, max |Δ| 3e-6 / 9e-6 / 7e-6 (= embedded's); a generated 12-page volume (`torch_export.ocr_inputs`; no real manga on a public runner) through the Rust runtime: `.mokuro` identical to the embedded build's (6/6 runs, every exit 0) | 186.1 vs 185.4 s per 12 pages; peak working set 1536 vs 1739 MiB; graph times equal |

How: `fakegpu.py` answers inductor's device queries with the target's properties, gives
Triton a driver that only reports the target (`GPUTarget("cuda", 80, 32)` /
`("hip", "gfx1030", 32)`), traces the pattern tables on the CPU, folds uniform constants on the
CPU, and replaces the compile-time autotune block with "compile every Triton kernel for the
target, keep the heuristics' first config, never launch". The one thing that silently
changed the graphs without a device was SDPA: with no GPU the composite falls back to the
math path (bmm+softmax): 285 crops/s on the 4090 instead of 377, different numerics — that
was the 281.6 vs 344.5 stream A measured. `sdpa.py` pins it to the target's choice (rules
from torch's `sdp_utils.cpp`; probed on the 4090, 9070 XT and 6900 XT: identical choices).
First-config kernels were not slower: the same graphs autotuned on the 4090 ran 360 vs 377.

**One multi-arch package?** CUDA: effectively yes. `emit_multi_arch_kernel` puts SASS + PTX
of one arch in each kernel; SASS runs on any card of the same major (sm_80 SASS on sm_86/
sm_89: same speed as native sm_86/sm_89 builds, measured on 4090/4060) and the driver JITs the
PTX for newer majors (sm_90, sm_120; forced-PTX test on the 4090: 220/220, same speed, +0.45 s
on the first load, 1.5 MB JIT cache). So `linux-cuda-sm_80` covers Ampere → Blackwell and
`linux-cuda-sm_75` Turing (fp32/fp16 only; Turing has no bf16). (libtorch's own cuBLAS/SDPA
kernels are NVIDIA's multi-arch builds.) ROCm: no — code objects are per ISA and the graphs
differ anyway (RDNA2 has no AOTriton attention: math SDPA; RDNA3/4 use AOTriton). One package
per family: `gfx1030` (RDNA2, gfx1031/1032 with `HSA_OVERRIDE_GFX_VERSION=10.3.0`) and one per
RDNA3/RDNA4 ISA. torch 2.13's ROCm `emit_multi_arch_kernel` (LLVM IR → bundle) failed in the
fake-device flow (`Failed to compile multi-arch bundle`); not pursued, see above.

#### A2 — portability

* GPU family: one package per arch family runs on every card of it (sm_80 on 4090/4060/3070;
  gfx1030 on 6900 XT/6800/6600).
* Host CPU: the AOTI wrapper (and CPU kernels) are compiled `-march=x86-64-v3`,
  `cpp.simdlen=256`; the build fails on any AVX-512 (zmm) instruction in x86-64-v3 code.
  Desktop-built (Zen 4) packages ran on Zen 3 (lily 5800X, server 5800X, steven 5500),
  Zen 2 (pimax 3800X) and Zen 5 (beast). The shootout's lily SIGILL (beast-native packages)
  cannot recur.
* glibc/libstdc++: packages built in the manylinux_2_28 container need GLIBC_2.14,
  GLIBCXX_3.4.21, CXXABI_1.3.11 (libtorch 2.13 itself: 2.28 / 3.4.22 / 1.3.11); host-built
  ones on this Arch box needed GLIBC_2.38 + GLIBCXX_3.4.30 (stream C's Debian failure).
  `PT_GNU_STACK` is checked RW (cleared if a toolchain marks it RWE).
* CPU packages: `cpp.dynamic_threads=True` (OpenMP width set at run time).
* Reproducible: two container builds of the same target are byte-identical after
  normalization; weights files are identical across all targets of an engine × precision.
  (Stream A's 4/196-page difference was between the shootout's beast-native build and a
  different build — different SDPA backend/fusion/precast: different numerics, each within
  0.5.2's own fp32-vs-bf16 range. A release ships one pinned build.)

#### A3 — weight split

GPU packages carry no weights; vision binds `weights-vision`, prefill + step bind the same
`weights-decoder` tensors (`load_constants(user_managed)`, no copy). Outputs identical: every
crop gate above ran on split weights (220/220, 100/100, unchanged texts vs the shootout's
self-contained packages). Sizes per engine × precision (download / per extra target):

| | before (3 self-contained .pt2 + host table) | after: shared weights | + per target |
|---|---|---|---|
| hayai fp32 | 788 MB per target | 565 MB | 8 MB |
| hayai bf16 / fp16 | 789 MB (398 frozen) per target | 282 MB | 9 MB |
| paddle fp32 | 4651 + 423 MB per target | 3622 MB (vision 1756 + decoder 1867) | 10 MB |
| paddle bf16 / fp16 | 2330 + 423 MB per target | 1811 MB | 10 MB |

GPU memory/RSS: hayai bf16 RSS 993 MB vs 1722 (4090); paddle bf16 1.3 GB vs 3.1 GB.
Windows GPU packages share the weights too, bound through the C-ABI Pairs entry point (above).

**CPU packages (2026-10-08): weightless too.** They used to be built with `freezing=True` and
the weights inside (hayai fp32 787 MB per CPU target, paddle fp32 4.65 GB): freezing was
expected to fold oneDNN/MKL-prepacked weights into the graph -- derived constants no shared
file could provide -- so the CPU path of `build.py` never mapped the constants to the weights
files. On these exported graphs it folds nothing: the old and new packages' generated C++ is
identical on every CPU target (the same 201/122/122 constants under their original FQNs, no
prepacked or folded constant, no oneDNN call; the Linear weights are pre-cast and q|k|v /
gate|up pre-fused at export). Freezing only cost the shared weights. CPU targets now build like
the GPU ones (`freezing=False`, `package_constants_in_so=False`, `bunko.weights` mapped; the
build checks the weights files are the GPU targets' bytes: they are, fp32 and bf16), and
`TORCH_EXPORT_CPU_EMBED=1` rebuilds the old layout. CPU tensors bind through the Pairs entry
point like GPU ones. The runtime reads each weights file into one buffer on the CPU (the
weights are views of it), the layout AOTInductor gives an embedded package's constants.

#### Argmax in the step graph (I/O v2)

Same tokens everywhere (all gates above are v2). Desktop RX 9070 XT, hayai bf16, 220 crops,
7 timed passes, alternating runs: v1 166–175 crops/s, v2 174–183 (+4–10 %), equal to the
shootout's frozen package (175–184) while weightless. RTX 4090: no measurable change (v1
365–379, v2 365–378). The per-token host sync remains (`live.any()`); checking it only every
4 or 8 steps was slower on the 4090 (370, 362) — batches run past their last EOS.

#### Recommended package matrix (MB; weights shared by all targets of a row)

| engine × precision | shared weights | `linux-cuda-sm_80` | `linux-cuda-sm_75` | `linux-rocm-gfx1030` | `linux-rocm-gfx1201` | `windows-cuda-sm_80` / `sm_75` | CPU |
|---|---:|---:|---:|---:|---:|---:|---|
| hayai fp32 | 565 | 9 | 9 | 9 | 8 | ~11 each | `linux-cpu-x86_64-v3` 8, `windows-cpu-x86_64-v3` 4, `macos-cpu-arm64` 3 (were 789 / 784 / 783 with the weights inside) |
| hayai bf16 | 283 | 9 | — | 9 | 9 | ~11 / — | `linux-cpu-x86_64-v4bf16` 9 (was 399) |
| hayai fp16 | 283 | 9 | 9 | 9 | 9 | ~11 each | — |
| paddle fp32 | 3622 | 10 | 10 | 10 | 10 | ~13 each | — (GPU-only since 2026-10-08; were 4645–4651 per CPU target) |
| paddle bf16 | 1811 | 10 | — | 10 | 10 | ~13 / — | — |
| paddle fp16 | 1811 | 10 | 10 | 10 | 10 | ~13 each | — |

Adding a GPU family costs ~10 MB per engine × precision. RDNA3 (`gfx1100/1101/1102`) and
`gfx1200` are one `matrix` line each (compiled the same way as gfx1201; no card to test).
`sm_86`/`sm_89`/`sm_120` packages are unnecessary (sm_80 SASS/PTX covers them at equal speed).
Largest single file: paddle fp32 `weights-decoder` 1,866,630,056 B (< 1.9 GB cap).

#### The release set (2026-10-08)

The 2026-10-03 set below minus paddle-manga's CPU packages (GPU-only engine) and with
weightless hayai-nova CPU packages: `linux-cpu-x86_64-v3` and `linux-cpu-x86_64-v4bf16` from
the container, `windows-cpu-x86_64-v3` from the `torch-export-win` workflow (native MSVC on a
GitHub runner; rebuilds differ only in the wrapper DLL's PE timestamp, 4 bytes), `macos-cpu-arm64` on the Mac; GPU
packages and every weights file unchanged. 180 packages + 12 weights files = 192 assets,
8.94 GB (was 25.6 GB), largest asset 1,866,630,056 B (paddle fp32 `weights-decoder`, < 1.9
GB). What a machine downloads for its default row (packages + the weights they bind; plus
35 MB of hayai-nova host files and 23 MB of PP-OCR files): CPU hayai-nova fp32 568–573 MB
(was 783–789), bf16 on AVX512_BF16 hosts 291 MB (was 399); GPU hayai-nova bf16/fp16 292–294
MB, fp32 574 MB (unchanged); GPU paddle-manga bf16/fp16 1822 MB, fp32 3633 MB (unchanged);
paddle-manga on the CPU: nothing (was 4651 MB). Weights entries in `torch-models.json` carry
the variant-free torch version (`2.13.0`): every target shares them.

#### The release set (2026-10-03)

Built with the final tool (container for Linux, Linux cross-build for `windows-cuda-*`, native
MSVC on pimax for `windows-cpu-x86_64-v3`, native on the Mac for `macos-cpu-arm64`):
hayai-nova fp32/bf16/fp16 and paddle-manga fp32/bf16/fp16 × `linux-cuda-sm_80`, `sm_75`
(no bf16), `linux-rocm-gfx1030/1100/1101/1102/1200/1201`, `windows-cuda-sm_80/sm_75`; CPU:
hayai fp32 `linux-cpu-x86_64-v3`, `windows-cpu-x86_64-v3`, `macos-cpu-arm64`, hayai bf16
`linux-cpu-x86_64-v4bf16`, paddle fp32 on the same three fp32 CPU targets. 195 packages + 12
weights files, 44.6 GB, largest asset 1,866,630,056 B; every file < 1.9 GB.
`python -m torch_export manifest --out <dir> --flat <mirror dir>` writes `torch-models.json`
(copied to `crates/bunko-ocr/src/torch_models.json`) and a flat directory of hard links under
the release asset names (usable as `MOKURO_TORCH_MODELS_MIRROR`). `requires` lists, per graph,
only the weights group it binds (vision → `weights-vision`, prefill/step → `weights-decoder`).
Linux packages rebuild byte-identically; Windows (host-built) ones differ between rebuilds
only in the cubins' debug line tables (2 bytes per kernel; same code) — set
`TRITON_DISABLE_LINE_INFO=1` if byte-identity is wanted there. `verify.py` compares graphs on
random inputs: in bf16 the step's argmax can tie-flip between eager and compiled, so its
"ids_equal: false" for bf16 step is not a failure (the crop gates are the parity test).

#### Runtime libraries the packages need (for the packs, stream C)

* Linux GPU packages `NEED`: `libtorch.so`, `libtorch_cpu.so`, `libtorch_cuda.so` |
  `libtorch_hip.so`, `libgomp.so.1`, `libstdc++.so.6`, `libgcc_s.so.1`, `libm/libc` (glibc ≥
  2.14), and CUDA: `libcuda.so.1` (the driver's) / ROCm: `libamdhip64` via libtorch_hip.
  All but the driver and glibc/libstdc++ are in the pack's `lib/`.
* The rocm7.1 wheel's `torch/lib` names its libraries without SONAME versions
  (`libamd_comgr.so` has SONAME `libamd_comgr.so.3`, `libhiprtc`, `librocroller` and
  `librocprofiler-sdk` need it): on a host without /opt/rocm (steven, patrick) libtorch fails
  with `libamd_comgr.so.3: cannot open`. The ROCm pack must add the SONAME symlinks (26 of
  them: `libamdhip64.so.7`, `libhsa-runtime64.so.1`, `librocblas.so.5`, … — list with
  `readelf -d $f | grep SONAME` over `torch/lib`). On server/desktop the system ROCm 7.2's
  comgr was silently used instead.
* Windows packages import `torch_cpu.dll`, `torch_cuda.dll`, `nvcuda.dll` (driver),
  `cudart64_13.dll` (in the wheel's `torch/lib`), the UCRT (`api-ms-win-crt-*`, Windows 10+).
  The Windows wheel has no flash attention (packages pin efficient attention for it).
* CPU packages: as Linux GPU minus the GPU half. macOS: `libtorch_cpu.dylib`, `libomp.dylib`
  from the wheel.

#### Graph I/O contract for the runtime (stream A) — changes vs the shootout

* v2 (above) is the default; v1 still builds (`--io 1`).
* Weights: every package (GPU and CPU, Linux, Windows, macOS) binds `bunko.weights` from
  `../weights-*.safetensors`; only `TORCH_EXPORT_CPU_EMBED=1` / `TORCH_EXPORT_WIN_EMBED=1`
  builds have `bunko.weights = {}` (weights inside). On Windows (`bunko.bind = "pairs"`)
  bind through the wrapper's `AOTInductorModelContainerUpdateUserManagedConstantBufferPairs`,
  never `load_constants` (see "Windows" above).
* Windows wrappers are `*.wrapper.pyd`; strip the archive's top `<role>/` folder when extracting
  into `unpack_to`, which then contains `data/aotinductor/model/`.

## Work and owners (parallel agents, shared worktree)

| stream | owns | deliverable |
|---|---|---|
| **A. runtime** | `crates/bunko-torch` (new), `crates/bunko-engines`, `crates/bunko-vlm` (gate the ORT recognizer behind `onnx-vlm`), `crates/bunko-ocr/src/models.rs` (torch package locations) | cdylib + loader; hayai/paddle via libtorch; parity on desktop ROCm, beast CUDA, CPU |
| **B. export** | `tools/torch_export/` (new; the shootout's `export_aoti.py` grown up) | per-target `.pt2` builds, manifest entries, answers to A1–A3 |
| **C. packaging + Docker** | `xtask`, `packaging/`, `deploy/`, `crates/mokuro-bunko` (features, `install-ocr`), `docs/` (README, MIGRATING, PACKAGING) | pack builder, install flow, flavors, Docker images + instructions; local builds only — **no CI runs, pushes or uploads** until the owner approves |

Open questions B answers first, because they decide the pipeline:

- **A1** Can `.pt2` packages be compiled without the target GPU (CUDA: `TORCH_CUDA_ARCH_LIST`,
  `aot_inductor.emit_multi_arch_kernel`; ROCm: `PYTORCH_ROCM_ARCH`)? Triton autotuning off.
- **A2** Portability: does one package run on every card of its arch family, and on any x86-64-v3
  host CPU? (Fleet: 4090/4060 sm_89, 3070 sm_86; 6900 XT/6800/6600 gfx1030; 9070 XT gfx1201.)
- **A3** Weight split: one weights blob shared by vision/prefill/step (today each `.pt2` embeds
  its own copy: hayai 0.8 GB, paddle 2.3 GB per precision).
- **A4** Minimal libtorch file set per variant (CUDA ~2.8 GB, ROCm ~6.6 GB unpacked) and the
  redistribution terms of the NVIDIA libraries libtorch bundles.

## Verification gates

1. Crop parity: 220 hayai / 100 paddle crops identical to 0.5.2 torch at each precision.
2. Full volume (Dr Stone 01, 196 pages): fp32 sidecars text-identical to 0.5.2; half precision
   within 0.5.2's own fp32-vs-half differences.
3. Speed ≥ tuned 0.5.2 on every fleet machine (desktop 9070 XT, beast 4090, lily 4060,
   server/steven/patrick RDNA2, pimax 3070 Windows, Mac M2 Pro CPU).
4. Docker: the full image (pack downloaded on first start) runs a full volume end to end on the
   CPU and on a GPU.
5. Dry run against a copy of a real library + database.

## A. Runtime as built (stream A, 2026-10-02)

Changes to the interfaces above are marked **(changed)**.

### C ABI v1 (`crates/bunko-torch/src/abi.rs`, the source of truth)

    uint32_t bt_abi_version(void);
    int32_t  bt_init(const char *config_json, char **err);          // (changed) added; once, first
    int32_t  bt_devices(char **json_out, char **err);
    void    *bt_load(const char *engine, const char *model_dir, const char *precision,
                     const char *device, const char *opts_json, char **err);   // NULL on error
    int32_t  bt_info(void *h, char **json_out, char **err);         // (changed) added
    int32_t  bt_read(void *h, const BtCrop *crops, size_t n, const uint32_t *caps,
                     char **json_out, char **err);                  // ["text", ...] per CROP
    void     bt_free(void *h);
    void     bt_free_str(char *s);
    typedef struct { const uint8_t *data; uint32_t width, height; } BtCrop;   // packed RGB8

Status: 0 ok, 1 bad argument, 2 load, 3 run, 4 panic (caught; never unwinds across).
`bt_read` reads crops, not lines: the main side cuts crops with bunko-vlm's crop code and
joins per-line texts (`bunko_vlm::read_flat`), so cropping is identical for every backend.
A handle is shared by any number of threads (device work is serialised inside).

* `bt_init` `{"lib_dir": "<pack>/lib", "cpu_only": false}`: loads, by absolute path and
  `RTLD_GLOBAL`, the pack's CPU half (`libgomp.so.1`, `libc10`, `libtorch_cpu`), the GPU half
  (`libtorch_cuda` | `libtorch_hip`) unless `cpu_only`, then `libtorch` (Windows:
  `c10.dll`, `torch_cpu.dll`, `torch_cuda.dll`, `torch.dll`). **Why**: the AOTInductor model
  libraries `NEED` `libtorch.so`/`libtorch_cpu.so`/`libgomp.so.1` by soname with no run path,
  so whatever is loaded under that name wins; without this a system libtorch
  (`/usr/lib/libtorch.so` on Arch) was picked up (stream C's report). The loader in
  bunko-engines also preloads the CPU half before `dlopen`ing the cdylib (so
  `LD_LIBRARY_PATH` cannot inject one either) and refuses a recognizer if any
  `libtorch*`/`libc10*` outside the pack is mapped (`/proc/self/maps`).
* `bt_devices` → `DevicesReport {abi, torch: "2.13.0+rocm7.1", gpu_backend: cuda|rocm,
  devices: [{id: cpu|gpu:<torch index>, kind: cpu|cuda|rocm, name, arch: sm_89|gfx1201|
  x86_64, isa: [avx2, fma, avx512f, avx512bw, avx512vl, avx512dq, avx512_bf16, amx_bf16, ...],
  vram_mb, pci, formats}], warnings}`. GPU facts come from the CUDA driver API / HIP runtime
  (+ KFD sysfs for the gfx target; `HSA_OVERRIDE_GFX_VERSION` honoured). Formats: GPUs
  fp32/fp16/bf16 (what 0.5.2's torch probe said); the CPU fp32, plus bf16 on x86-64-v4 +
  AVX512_BF16 hosts (the bf16 CPU packages need it). 0.5.2 ran the CPU in fp32 only: on a
  Zen 4/5 host hayai-nova's auto-accuracy now picks bf16 on the CPU (0.5.2's policy puts bf16
  first for hayai). **Owner decision** if that should stay fp32.
* `bt_load(engine, model_dir, precision, device, opts)`: `opts` = `LoadOptions {tokenizer,
  pos_table?, embeddings?, patch_budget?, threads, vision?, prefill?, step?, cache_dir?,
  weights?}`. Graphs default to `<model_dir>/{vision,prefill,step}.pt2`; a `.pt2` may be the
  zip AOTInductor writes or that zip **unpacked into a directory of that name** (preferred:
  loaded in place). A zip is never handed to libtorch (which extracts to `TMPDIR` on every
  load and leaks it on failure): the runtime unpacks it once into `cache_dir` (main side:
  `<storage>/models/torch/.unpacked/<name>-<hash>`; atomic rename, zip-slip checked) and loads
  that directory; a failed load removes the unpacks it used. `threads`: OpenMP/ATen width set
  on each reading thread (only on change). Load errors that mean "built for a newer system"
  (`GLIBC_x not found`, `GLIBCXX_`, `cannot enable executable stack`) are reported as such,
  for the pack's libraries and for packages (`abi::explain_load_error`).
* `bt_info` → `LoadInfo {engine, precision, device, batch, load_seconds, torch, io, weights}`.

### Packages the runtime drives (with stream B's `tools/torch_export`)

* Graph I/O: package metadata `bunko.io` — **v1** (no metadata; the shootout's packages:
  prefill/step return `(logits, *present)`) and **v2** (B: `(next_ids, live, *present)`, step
  takes `live`). Both verified.
* Weights: every package (GPU and CPU, since 2026-10-08) is weightless; each package's `bunko.weights`
  (FQN → key) is bound (user-managed, no copy) to tensors loaded once from
  `<engine>/<precision>/weights-*.safetensors` (the parent of the target directory),
  streamed one tensor at a time. paddle-manga's input embeddings come from the decoder blob
  (`bunko.alias.host.embed_tokens`); packages without a blob (the shootout's, hand-made) read
  `paddle-manga/embed-fp32` (bf16/fp32, cast on load; bit-identical to the model's table) or
  `embed-fp16`. hayai-nova reads `hayai-nova/{pos-table,token-embeddings,tokenizer}` from the
  existing manifest. `bunko.special` / `bunko.prompt` are checked against the code's
  constants. hayai-nova packages: patch budget 512 only (static axis).
* Layout **(changed: B's target names)**:
  `<storage>/models/torch/<engine>/<precision>/{weights-*.safetensors, <target>/{vision,prefill,step}.pt2}`,
  targets `<os>-cuda-sm_NN` (a device tries its own arch, then every older one: PTX),
  `<os>-rocm-gfxNNNN`, `<os>-cpu-x86_64-v3` (fp32), `<os>-cpu-x86_64-v4bf16` (bf16),
  `<os>-cpu-arm64` (`bunko_ocr::models::torch_targets`). Manifest ids = store paths:
  `torch/<engine>/<precision>/<target>/<file>` and `torch/<engine>/<precision>/weights-<g>.safetensors`.
  **(2026-10-03)** The release's `torch-models.json` is compiled into bunko-ocr
  (`crates/bunko-ocr/src/torch_models.json`, refreshed with each `torch-models-*` release; it
  must also list the windows-* / macos-* targets). `ModelStore::ensure_torch_package` fetches
  a package's graphs and only the weights files its graphs `require` (every release package),
  verifies each `.pt2`, unpacks it to its `unpack_to` directory (`<target>/<role>/`, stamped
  `.unpacked` = its sha256) and deletes the zip: one copy on disk, loaded in place.
  `MOKURO_TORCH_MODELS_MIRROR` (a base URL, or a directory of the flat asset names) is tried
  before GitHub (air-gapped hosts, local release copies, tests).
* A compiled package's supported precisions = device formats ∩ packages obtainable here; a
  precision without a package is "not supported" (auto modes skip it, forced ones refuse
  with 0.5.2's marker). **Auto modes take bf16 only where it is native**
  (`precision::bf16_native`: NVIDIA sm_80+, AMD gfx11xx/gfx12xx; never the CPU): on RDNA2
  bf16 is emulated (6900 XT hayai-nova: fp32 2.72, bf16 2.19 p/s, and less accurate), so
  auto-accuracy runs fp32 there and auto-speed fp16; a forced `bf16` still runs. A GPU with no package in any format (e.g. its arch not compiled)
  places the session on the CPU with a logged reason (`runtime::place_runnable`); a GPU pack
  on a host without a GPU still offers hayai-nova on its CPU packages (0.5.2's CUDA image
  fell back to CPU torch). **(2026-10-08)** paddle-manga is GPU-only
  (`bunko_sched::precision::gpu_only`, `runtime::place_engine`): never placed on or
  offered for the CPU, no CPU fallback when its GPU has no package; without a GPU a session,
  `models download --engine paddle-manga` and the library's claim fail with "paddle-manga
  needs a GPU (NVIDIA CUDA or AMD ROCm); use hayai-nova on the CPU" (`doctor`: WARN, since a
  processor with a GPU can still run that generation). Unit-tested with fake `bt_devices`
  reports.

### Main side (bunko-engines)

Features: `torch` (default; needs no libtorch: libloading + `bunko-torch`'s ABI types),
`onnx-vlm` (the deferred ORT recognizers; used only when no pack loads). bunko-vlm:
`onnx-vlm` gates `ort` (the crate builds without ONNX Runtime by default); its ORT EP
features imply it. One pack per process, opened on first use: `MOKURO_TORCH_PACK=<dir>`, else
`<storage>/backends/` (`MOKURO_BACKENDS_DIR` overrides; = `models_dir/../backends`), GPU
variant whose driver is present (`/dev/kfd`, NVIDIA driver) first, then `cpu`. The device
catalog (ids `gpu:<torch index>`, provider `cuda`/`rocm`) and `host.runner_build`
(`..., libtorch 2.13.0+cu130`) come from the pack. `Backend::Rocm` added (`rocm` was WebGPU).
`EnginePipeline::{need_for, prefetch_rows, package_status_for}`: what a row (engine +
precision mode) runs on here (device, precision, targets), fetching it (`models download`,
`install-ocr`: the enabled generations; non-zero exit naming the engine when nothing here can
run it) and checking it without a download (`doctor`: FAIL when a row's package is not on
disk or not runnable; reports `HSA_OVERRIDE_GFX_VERSION`). `EnginePipeline::recognizer_for`
(benches). Sidecar provenance names the torch release (`Gnathonic/mokuro-bunko:
torch-models-v1`).

**Default pools (2026-10-03).** The detect budget counts the shared PP-OCR session's real
threads: `(logical CPUs / jobs − 2 reserved − 4) / 2 + 1` detect workers (16 threads → 6,
12 → 4, 8 → 2, 4 → 1), capped at 8 (the ppocr-manga road keeps 0.5.2's 4). On a fast GPU
(native bf16 class) of a host with 16+ physical cores hayai-nova's engine stage gets 2
threads and detect is derived against that pace (x6). Measured, default pools, Dr Stone 01:
6900 XT fp32 2.72 → 3.83 p/s (0.5.2 tuned 2.66), RX 6600 fp32 1.72 → 1.77 (1.32), 4090 bf16
9.06 → 12.6 (0.5.2 tuned ~10.5), fp32 parity 196/196 on all three.

**ROCm environment (2026-10-03)**, set before the ROCm runtime starts (loader and
`bt_init`, `abi::prepare_rocm_env`): `AMDGPU_ASIC_ID_TABLE_PATHS` = `<pack>/share/libdrm` then
`/usr/share/libdrm` (else libdrm walks the executable's directory tree, prints "(null): No
such file or directory" and names every card "AMD Radeon Graphics"), and 0.5.2's
`rocm_gfx.override_for`: a card whose target is not built but whose family's `…0` is gets
`HSA_OVERRIDE_GFX_VERSION` (RX 6600 gfx1032 → 10.3.0, verified on patrick). Neither
overrides a user value.

**Runtime details (2026-10-03).** `MOKURO_TORCH_THREADS` is applied on every read to every
loaded OpenMP runtime (libgomp, libomp, libiomp5, and MSVC's vcomp140 that Windows CPU
packages import). An `atexit` hook (re-registered after each package load, so it runs
before their destructors) stops reads and waits for running ones: no more "aoti_torch_cpu_…
API call failed" when the server stops mid-volume. Per-crop host preprocessing runs in
parallel (hayai-nova: all crops of a batch; paddle-manga on the CPU, before it became
GPU-only: ahead of the vision runs; outputs identical): 4090 hayai bf16 crops 277 → 337/s. Load errors name the missing
library / newer system (`abi::explain_load_error`, with libloading's source chain).

**Catalog and benchmarks (2026-10-03).** `describe()` lists per device only the formats a
recognizer can run here (compute formats ∩ packages on disk or downloadable,
`torch::runnable_formats`: a Turing card lists fp32/fp16, not bf16). The host's CPU string
carries 0.5.2's core count (`"… (8 cores)"`, `runtime::cpu_label`). `EngineRunner` answers
`VolumeRunner::width_ceilings` from the planner (`plan::width_ceilings`: pooled stages up
to the budget and the road's cap, a GPU engine up to 8 threads within the budget, a CPU
engine 1), so a benchmark never searches widths the planner would refuse.
Env: `MOKURO_TORCH_PACK`, `MOKURO_BACKENDS_DIR`, `MOKURO_TORCH_MODELS_DIR` (dev override of
`<storage>/models/torch`), `MOKURO_TORCH_THREADS` (CPU recognizer threads; default the
session's share of the physical cores -- of the performance cores on a hybrid CPU that
reports them, Apple silicon, as libtorch's own default: `plan::cpu_engine_threads`; GPU
recognizers 1).

**Binding and unpack layout (2026-10-04).** The shim binds weights through the model
library's C-ABI `AOTInductorModelContainerUpdateUserManagedConstantBufferPairs` on every
platform (container handle and library read from the runner's protected members via a
pointer to member; pairs keyed by the internal constant names from
`getConstantNamesToOriginalFQNs`; `validate_full_update`), never `load_constants`, whose
`std::unordered_map*` a MinGW-built Windows wrapper misreads. Unpacks strip the archive's
top `<role>/` folder, so `unpack_to` holds `data/aotinductor/` directly (stamp line
`layout 2`; an older `<role>/<role>/` unpack is moved up in place on first use; the
runtime's own zip cache marks `.complete-2` and redoes older ones). Desktop ROCm: hayai
fp32 crops 220/220, paddle fp32 100/100, hayai fp32 volume 196/196 (fresh store).

**Release blockers fixed (2026-10-03).** (1) `torch_models.json` `requires` entries are
store ids (as `torch_export manifest` writes them) or flat release file names; an entry
naming neither fails the release load instead of being dropped (it was: every GPU
package lost its `weights-*` and `models download` / first use never fetched them). A
package counts as on disk only with the weights its graphs bind beside it
(`ModelStore::locate_torch_package`); `doctor` FAILs on a missing weights file
(`EnginePipeline::package_status_for` checks the recognizer's host files too; the CPU
check for paddle-manga's `embed-*` table went with paddle's CPU packages, 2026-10-08). (2) SIGTERM mid-volume / mid-benchmark: the
server's OCR stop now awaits the local processor (`LocalChannels::finished`), whose
sessions drop their runner -- joining the stage threads and freeing the recognizers --
before they report their end; `bt_free` after the exit hook has run leaks the handle
instead of freeing into a torn-down CUDA/HIP allocator. Before: beast CUDA 3/3 crashed
(SIGABRT, SIGSEGV x2), desktop ROCm 1/3 SIGSEGV + 2/3 "API call failed"; after: 10/10
clean (rc 0, 5 mid-volume + 5 mid-benchmark) on each.

### Notes for export (B)

* CPU packages: AOTInductor bakes `#pragma omp parallel num_threads(<compile host cores>)`
  (16 in the shootout's) unless `cpp.dynamic_threads=True`; compile CPU targets with it so
  `threads` applies.
* Two sm_89 compiles of the same graphs (shootout vs today) differ on 4 of 196 pages in bf16
  (both within 0.5.2's own fp32-vs-bf16 range); a release must ship one build and record it.
* B's GPU-less (fake-GPU) sm_89 hayai bf16 v2 package runs and reads 220/220 on the real
  4090; with the parallel host preprocessing it reaches 337 crops/s (on-card compile with
  the old runtime: 344).

### Verified (2026-10-02)

| gate | desktop RX 9070 XT (ROCm 7.1) | beast RTX 4090 (cu130) | desktop CPU (7950X) |
|---|---|---|---|
| crops hayai fp32/bf16/fp16 vs 0.5.2 | 220/220 ×3 (23.9/158.5/161.4 crops/s) | 220/220 ×3 (181/336/347) | bf16 220/220 (cpu pack) |
| crops paddle fp32/bf16/fp16 | 100/100 ×3 (2.4*/50.0/50.3) | 100/100 ×3 (32.1/92.4/94.3) | — (no CPU paddle package) |
| B's v2 weightless packages (bf16) | hayai 220/220 148.6 crops/s (RSS −750 MB); paddle 100/100 50.6 crops/s (RSS 3.5→1.3 GB) | hayai sm_89 220/220, 281.6 crops/s | |
| full volume hayai fp32 vs 0.5.2 | 196/196 pages, 3203/3203 lines, 1.14 p/s | 196/196, 7.61 p/s | 20 p: 20/20 vs 0.5.2 CPU fp32 (322/322 lines) |
| full volume hayai bf16 | = shootout libtorch (196/196); 4.5–5.2 p/s on a loaded host** | 8.42 p/s; vs 0.5.2 bf16 187/196 (0.5.2 fp32-vs-bf16: 185/196) | |
| full volume paddle bf16 | | 3.26 p/s, = shootout libtorch output | |
| CPU hayai bf16 20 p | | | 20/20 = shootout libtorch CPU bf16 |

\* fp32 paddle on the 9070 XT varies run to run (30–46 s per pass); the shootout's 3.4 was the
fast end. \** the desktop was shared with unrelated jobs (load 19–80) all evening, so its
CPU-bound pipeline could not reach the shootout's 6.05 p/s; alternating A/B runs against the
shootout's own `ocr_volume` (same packages, same load): 4.96 vs 4.94, then 1.88 vs 5.48 and
3.11 vs 2.81 p/s under shifting load — no systematic difference. Desktop CPU timings (0.33 p/s
fp32, 0.25 bf16 on 20 pages) were taken under the same load and are not comparable to the
shootout's 0.64 / 0.95; re-measure on a quiet host.

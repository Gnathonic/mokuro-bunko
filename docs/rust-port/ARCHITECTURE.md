# mokuro-bunko 0.7 — Rust architecture

**Status:** living document · **Baseline:** Python 0.5.2 (`release/0.5.2`, 199cff5)
**Specs of the 0.5.2 behaviour being ported:** `docs/rust-port/spec/*.md`

## Goals (owner, 2026-10-01)

1. Rewrite bunko in Rust, improving the architecture and performance.
2. Licence compliance: no GPL component anywhere in the shipped artifacts.
   mokuro/manga-ocr, comic-text-detector (ctd), AnimeText and rtdetr are removed.
   Remaining OCR: **hayai-nova**, **paddle-manga** (PaddleOCR-VL + manga LoRA) and
   **ppocr-manga** (PP-OCR det + rec), all Apache-2.0, all on ONNX Runtime.
3. Per-platform release packages: Linux, Windows, macOS, Docker, Android (best effort).
4. Auto-update: the server notifies, and the admin applies an update with one click.
5. **Lean server:** runs on a 1 GB RAM host, with OCR done by processors on other machines.
6. Drop-in over 0.5.2 storage: the same `config.yaml`, SQLite DB, library tree, sidecars,
   and the same reader-facing WebDAV and JSON APIs. The web UIs are reused.

## Non-goals

- Wire compatibility with 0.5 Python processors. Both sides are Rust, and the processor
  protocol is redesigned (see §6).
- Python OCR environments, venvs, uv, torch. All of that is gone.

## 1. One binary, two builds

`mokuro-bunko` is a single executable with subcommands (the same CLI surface as 0.5.2,
minus `install-ocr`'s venv work). Cargo features choose what is linked in:

| Build | Features | Contains | Target host |
|---|---|---|---|
| **lite** | `default-features = false` | server, WebDAV, catalog, admin, scheduler for remote processors, updater | 1 GB VPS, NAS, Raspberry Pi |
| **full** | `ocr` (+ EP features per platform) | lite + ONNX Runtime + engines + `processor` subcommand + local OCR | desktop, GPU box |

In the lite build `ocr.local_processing` is forced off and the admin UI says so. A full
build on another machine runs `mokuro-bunko processor serve` against the lite server.

## 2. Crates

```
crates/
  bunko-core       config (YAML + env overrides), storage layout, roles, generation recipes,
                   sidecar naming, shared ids, errors. No I/O beyond config/fs helpers.
  bunko-db         SQLite (rusqlite, bundled), schema identical to 0.5.2 + forward migrations,
                   one writer connection + small reader pool, busy/lock retry.
  bunko-library    archive access (zip/cbz, dirs), natural sort, library scan + index,
                   metadata compiler (series.json / catalog.json), fs watcher, thumbnails.
  bunko-dav        WebDAV method handling (PROPFIND/GET/HEAD/PUT/DELETE/MOVE/COPY/MKCOL/
                   OPTIONS/LOCK) over a resource trait; per-user progress-file mapping lives
                   in the server's resource impl, not here.
  bunko-proto      processor wire types (serde) shared by server and processor.
  bunko-ocr        [feature ocr] ONNX engines: PP-OCR det/rec, hayai-nova, paddle-manga,
                   layout/reconcile, sidecar writer, model store (download + verify), devices.
  bunko-processor  [feature ocr] processor runtime: job loop over a `JobSource` (HTTP to a
                   remote server, or in-process channel for local OCR), bench.
  bunko-engines    [feature ocr] the `PagePipeline` over bunko-ocr + bunko-vlm + bunko-layout:
                   staged pages (detect → engine → post), devices, precision, model fetch.
  bunko-thumb      cover thumbnails (Pillow-exact contain + Lanczos, lossy WebP); no ONNX
                   Runtime, used by the server (lite too) and re-exported by bunko-ocr.
  bunko-server     axum app: auth, sessions, all JSON APIs, static UIs (embedded), WebDAV
                   mount, OCR scheduler + processor API, catalog enrichment, TLS, tunnel,
                   dyndns, update checks.
  bunko-update     release manifest fetch, signature verification, self-replace, restart.
  mokuro-bunko     the binary: clap CLI, logging, allocator, feature wiring.
web/               the reused frontends (moved from src/mokuro_bunko/*/web), embedded.
```

Dependency direction: `core ← db ← library ← dav ← server ← mokuro-bunko`;
`core ← proto ← {server, processor}`; `{ocr, vlm, layout, processor} ← engines ←
mokuro-bunko[ocr]`; `thumb ← {ocr, server}`.
The server never depends on `bunko-ocr`; local OCR is wired in by the binary through the
`LocalProcessor` hook (§6), which is what keeps the lite build free of ONNX Runtime.

## 3. Runtime model

- tokio multi-thread runtime; worker threads = `min(available_parallelism, server.threads
  or 4)`. Blocking work (SQLite, archive reads, image decode) goes through
  `spawn_blocking` with a bounded semaphore.
- OCR inference runs on dedicated OS threads (std::thread), never on tokio workers.
  `ort` sessions are `Send + Sync`; N recognizer "copies" in 0.5 become N threads
  sharing one session per (engine, device, precision). No process sharding, no pickling.
- Shutdown: a `CancellationToken` tree; ordered shutdown mirrors 0.5.2's
  (`server.py shutdown_app`).

## 4. Memory budget (lite build, 1 GB host)

Target steady-state RSS ≤ 64 MB with a 10k-volume library; peak ≤ 256 MB.

- Responses stream from disk (`tokio::fs` + `ReaderStream`), never buffered; uploads stream
  to a temp file in the destination directory, then rename (atomic, as 0.5.2 batch 5).
- Caches are byte-bounded LRUs with config knobs (`server.cache_mb`, default 32):
  PROPFIND cache, library index, compiled metadata.
- SQLite `cache_size` capped (−8000 = 8 MB), `mmap_size` 0 on lite.
- mimalloc as global allocator (returns memory to the OS better than glibc malloc
  under fragmentation; musl's allocator is too slow for the static Linux build).
- No image decoding on the lite server except thumbnails, which are generated by
  processors and uploaded with the sidecar (0.5.2 already makes `.webp` thumbnails in OCR).

## 5. HTTP surface

Identical paths, methods, JSON field names and status codes to 0.5.2 (see
`spec/http-webdav.md`, `spec/web-frontend-contract.md`, `spec/db-auth-admin.md`,
`spec/metadata-catalog.md`). The web UIs are served unchanged except for removed engine
options. nginx X-Accel offload (`MOKURO_NGINX_ACCEL=1`) is kept.

## 6. OCR: one scheduler, many processors

0.5.2 had two separate paths: local OCR drove Python runner subprocesses over
JSON-over-stdio, and remote processors used an HTTP API. 0.7 has one path:

```
              ┌──────────── bunko-server ────────────┐
 library ───► │ watcher → JobQueue → Scheduler/ETA    │◄── HTTP (bunko-proto) ── remote processor(s)
              │                   ▲                   │
              │                   └── LocalProcessor ─┼── in-process channel (full build only)
              └───────────────────────────────────────┘
```

- A processor (local or remote) reports a profile (engines, devices, measured
  throughput); the scheduler assigns each volume to whichever processor finishes it
  first (0.5.2 semantics, `spec/ocr-scheduling.md`).
- Remote processors claim jobs, stream pages from the server's archive (or from a local
  cache), and upload the sidecar and thumbnail, with leases and heartbeats.
- A local processor is the same `bunko-processor` code reading archives directly from
  disk.

## 7. Models

- ONNX exports are made by `tools/onnx_export/` (Python, dev-only), never on users'
  machines.
- They are published as GitHub release assets (`models-v1` release). Files over 2 GB are
  split. A signed `models.json` lists url, size, sha256 and source-model revisions.
- The processor downloads on first use into `<storage>/models/` and verifies the sha256.
  Provenance in sidecars records the export id and the source revisions.

## 8. Packaging & updates

See `docs/rust-port/PACKAGING.md` (to be written after the server builds): GitHub Actions
matrix → tar.gz/zip per target, an MSI or zip for Windows, a macOS universal tarball,
Docker images (`lite` distroless-static, `full`, `full-cuda`), and an Android APK. A
signed `release.json` manifest drives the updater.

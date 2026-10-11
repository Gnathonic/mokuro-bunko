# mokuro-bunko 0.7 — Rust architecture

**Status:** living document; describes the code of the 0.7 branch · **Baseline:** Python
0.5.2 (`release/0.5.2`, 199cff5), held to 0.5.3 for library paths (CONVENTIONS.md)
**Specs of the 0.5.2 behaviour being ported:** `docs/rust-port/spec/*.md`

## Goals (owner, 2026-10-01)

1. Rewrite bunko in Rust, improving the architecture and performance.
2. Licence compliance: no GPL component anywhere in the shipped artifacts.
   mokuro/manga-ocr, comic-text-detector (ctd), AnimeText and rtdetr are removed.
   Remaining OCR: **hayai-nova**, **paddle-manga** (PaddleOCR-VL + manga LoRA) and
   **ppocr-manga** (PP-OCR det + rec), all Apache-2.0. The recognizers (hayai-nova,
   paddle-manga) run on **libtorch**, as 0.5.2's torch did (owner decision 2026-10-02,
   TORCH-BACKEND.md); PP-OCR runs on ONNX Runtime on the CPU.
3. Per-platform release packages: Linux, Windows, macOS, Docker. Android is out of 0.7's
   scope (owner, 2026-10-03; MOBILE.md).
4. Auto-update: the server notifies, and the admin applies an update with one click
   (fully automatic updates are opt-in).
5. **Lean server:** runs on a 1 GB RAM host, with OCR done by processors on other machines.
6. Drop-in over 0.5.2 storage: the same `config.yaml`, SQLite DB, library tree, sidecars,
   and the same reader-facing WebDAV and JSON APIs. The web UIs are reused; 0.7 adds the
   first-run wizard's new steps, the admin panel's **This server** tab and the desktop
   app's local pages (§9).

## Non-goals

- Wire compatibility with 0.5 Python processors. Both sides are Rust, and the processor
  protocol is redesigned (§6, PROTOCOL.md).
- Python OCR environments, venvs, uv, pip-installed torch. All of that is gone; libtorch
  arrives as a backend pack (§8).

## 1. One program, two builds

`mokuro-bunko` is a single executable with subcommands (the same CLI surface as 0.5.2,
plus `models`, `healthcheck`, `gui` and `tray`). Cargo features choose what is linked in:

| Build | Features | Contains | Released for |
|---|---|---|---|
| **lite** | `--no-default-features` | server, WebDAV, catalog, admin panel, first-run setup, the scheduler for remote processors, the updater, the local control API and app pages | Linux arm64 (static, musl), the lite Docker image (amd64 + arm64) |
| **full** | `ocr`, `tray` (the defaults) | lite + ONNX Runtime on the CPU (PP-OCR), the libtorch pack loader, the engines, local OCR, `processor`, the desktop tray | Linux x86_64 (glibc 2.28+), Windows x86_64, macOS on Apple silicon, the full Docker image (amd64; built with `--no-tray`, so `ocr` only) |

GPU support is not a build: the full binary runs whatever backend pack is installed (§8).
In the lite build the server reads no OCR itself (`Config::processes_locally`); OCR comes
from `mokuro-bunko processor serve` on a full build elsewhere.

`mokuro-bunko` with **no arguments** starts the app (`crates/mokuro-bunko/src/no_args.rs`):
in a desktop session the tray, headless what is configured (`serve`, `processor serve`, or
both), or `setup` in a terminal. On Windows the program ships twice from one build:
`mokuro-bunko.exe` (the app, GUI subsystem: no console window) and
`mokuro-bunko-cli.exe` (console, for terminals); the PE header's subsystem field is the
only difference (PACKAGING.md §1).

## 2. Crates

```
crates/
  bunko-core       config (YAML + MOKURO_* env overrides), the storage layout, roles, the
                   OCR engine and generation registry. No network, no database.
  bunko-db         mokuro.db (rusqlite, bundled): 0.5.2's schema + forward migrations, one
                   writer connection + a small read pool, busy/lock retry. Sync.
  bunko-library    the library tree: archives (zip/cbz, dirs), sidecar naming, natural sort,
                   the library index, the metadata compiler (series.json / catalog.json),
                   the reader manifest, the Python/reader semantics they rest on. Sync.
  bunko-dav        WebDAV: the virtual tree (library + per-user progress files), every DAV
                   method, conditional requests and ranges, staged atomic uploads, per-path
                   write locks, the PROPFIND cache; side effects go out through hooks. Async.
  bunko-thumb      cover thumbnails (Pillow-exact contain + Lanczos, lossy WebP); no ONNX
                   Runtime, so the lite server makes covers too.
  bunko-layout     the OCR output stage: line layout, reconcile, the reconciled road's pure
                   half, sidecar dicts and the atomic write, Python json/float semantics.
  bunko-sched      the scheduling math (job order, ETA, congestion, throughput, precision
                   rules) and its three JSON state files. Pure.
  bunko-proto      processor protocol v3 types (serde), shared by server and processor, and
                   the background install's progress (`OcrInstall`).
  bunko-control    the local control API: status / pause / events types, the pause state
                   machine, activity and stats, the 127.0.0.1 listener (token, cookie, SSE).
  bunko-update     release manifest fetch + ed25519 check, self-replace and the layout of an
                   installed copy, backend pack installs, automatic updates.
  bunko-server     axum app: auth, sessions, every JSON API, the embedded web UIs, WebDAV
                   mount, first-run setup (and its setup code), the admin panel with This
                   server, the OCR scheduler + processor API, catalog enrichment, TLS,
                   tunnel, dyndns, update checks, thumbnails. No ONNX Runtime, no libtorch.
  bunko-ocr        [ocr] ONNX Runtime on the CPU: PP-OCR det/rec, page decoding, the model
                   store (manifests compiled in; download + verify).
  bunko-vlm        hayai-nova / paddle-manga host side: line crops, the model facts and host
                   helpers, the `Recognizer` seam (the ONNX recognizers behind the
                   non-default `onnx-vlm` feature).
  bunko-torch      hayai-nova / paddle-manga on libtorch: built per backend pack as a cdylib
                   (`--features libtorch`) with a small C ABI; the workspace build has only
                   the ABI types (`abi`), so the binary never links libtorch.
  bunko-processor  [ocr] the processor runtime: the remote link (WebSocket + HTTP fetch and
                   PUT) and the local one (in-process channels), bench, processor.yaml and
                   the `processor` commands.
  bunko-engines    [ocr] `EnginePipeline`: the staged page pipeline over bunko-ocr, bunko-vlm,
                   bunko-layout and the pack loader (`torch`), devices, precision, model fetch.
  bunko-tray       [tray] the desktop tray: discovery, the control API client, the menu
                   model, supervision of tray.json roles, autostart; a StatusNotifierItem on
                   Linux, tray-icon + muda on Windows and macOS.
  bunko-android    the lite server as a JNI library for the Android app (not released in 0.7).
  mokuro-bunko     the binary: clap CLI, logging, allocator, feature wiring; the background
                   OCR install (`ocr_install.rs`), hardware detection (`hwdetect.rs`), the
                   server's `Machine` (`server_machine.rs`), the app pages (`gui/`), the
                   no-argument start.
  xtask            release tooling: dist, torch-pack, manifest, sign, verify, release notes,
                   licences, docker context (PACKAGING.md).
web/               the frontends (moved from src/mokuro_bunko/*/web, plus the desktop app's
                   `app/`), embedded at build time.
```

Dependency direction (normal dependencies):

- `core ← {db, library, dav, update, server, android, mokuro-bunko}`;
  `{db, library, dav, layout, sched, thumb, control, proto, update} ← server ← android`;
- `proto ← {control, update, server, processor, engines}`;
  `control ← {server, processor, tray, mokuro-bunko}`; `{sched, update} ← processor`;
- `thumb ← ocr`; `vlm ← torch`; `{ocr, vlm, torch (ABI only), layout, processor, sched}
  ← engines`;
- `{core, db, server, update, control} ← mokuro-bunko`, plus `{engines, ocr, processor,
  proto}` with the `ocr` feature and `tray` with the `tray` feature.

The server depends on none of `bunko-ocr`, `bunko-engines` or `bunko-processor`
(`bunko-processor` only for its tests): local OCR, the machine settings and the background
install are wired in by the binary through hooks (§6), which is what keeps the lite build
free of ONNX Runtime.

## 3. Runtime model

- tokio multi-thread runtime; worker threads = `server.threads`, or `min(cores, 4)` when 0;
  at most 64 blocking threads; at most 1024 connections served at once (past that,
  accepting waits). Blocking work (SQLite, archive reads, fs walks, image decode) goes
  through `spawn_blocking`.
- OCR runs on dedicated OS threads, never on tokio workers: a volume's pages flow through
  stages (detect → engine → post) that are pools of threads joined by small bounded
  queues (`bunko-engines` `stages.rs`). Models are shared: one PP-OCR detector/recognizer
  pair, one recognizer per (engine, package, device, precision) across sessions. No
  process sharding, no pickling.
- Shutdown: a `CancellationToken` stops the services in 0.5.2's order (`server.py
  shutdown_app`); SIGTERM (`docker stop`, systemd) and Ctrl+C both stop the server
  gracefully, also while OCR runs.

## 4. Memory budget (lite build, 1 GB host)

Target steady-state RSS ≤ 64 MB with a 10k-volume library; peak ≤ 256 MB (measured: about
16 MiB idle in the lite Docker image).

- Responses stream from disk, never buffered; uploads stream to a temp file beside the
  destination, are checked, then renamed (atomic, as 0.5.2 batch 5).
- Caches are byte-bounded by `server.cache_mb` (default 32): the `Depth: infinity`
  PROPFIND cache gets half, the library index a quarter.
- SQLite's page cache is 8 MiB split across all connections; `mmap` stays off.
- mimalloc is the global allocator (returns memory to the OS better than glibc malloc
  under fragmentation; musl's allocator is too slow for the static Linux build).
- The only image decoding on the server is cover thumbnails: one at a time, the decode
  capped, so a hostile "cover" cannot take a small host's memory.

## 5. HTTP surface

Identical paths, methods, JSON field names and status codes to 0.5.2 (see
`spec/http-webdav.md`, `spec/web-frontend-contract.md`, `spec/db-auth-admin.md`,
`spec/metadata-catalog.md`), minus the removed engine options. New in 0.7:

- `/setup` is the whole first-run wizard (admin account, who may join, remote access,
  OCR on this machine). From another computer it asks for the one-time setup code the
  server logs at each start while no admin exists (`bunko-server` `accounts/setup.rs`).
- `/_admin/api/machine/*`: the admin panel's **This server** tab (OCR backend, engines
  and models, doctor, the log); admin only, CSRF-checked.
- `/_processor/...`: protocol v3 (a WebSocket per processor, result `PUT`s).
- The local control listener (§9) serves `/control/*` and `/app/*` on 127.0.0.1 only;
  it is not part of the public server.

nginx X-Accel offload (`MOKURO_NGINX_ACCEL=1`) is kept (off by default in the images).

## 6. OCR: one scheduler, many processors

0.5.2 had two separate paths: local OCR drove Python runner subprocesses over
JSON-over-stdio, and remote processors used an HTTP API. 0.7 has one path:

```
              ┌──────────── bunko-server ────────────┐
 library ───► │ watcher → JobQueue → Scheduler/ETA    │◄── WebSocket + HTTP (bunko-proto) ── remote processor(s)
              │                   ▲                   │
              │                   └── LocalProcessor ─┼── in-process channels (full build only)
              └───────────────────────────────────────┘
```

- A processor (local or remote) reports a profile (engines, devices, measured
  throughput); the scheduler assigns each volume to whichever processor finishes it
  first (0.5.2 semantics, `spec/ocr-scheduling.md`).
- A remote processor holds one WebSocket for every op and event, fetches archives with
  ordinary `GET`s (`Range` / `If-Range` resume) and `PUT`s each finished sidecar before
  its `volume_done` (PROTOCOL.md).
- The local processor is the same `bunko-processor` code (`local::LocalProcessor`) over
  channels, reading archives from disk.
- The binary plugs the full build into the server through hooks of `bunko-server`:
  `ocr::LocalProcessorFactory` (local OCR over `bunko_engines::EnginePipeline`),
  `ocr::BackgroundInstall` (the OCR backend install, `ocr_install.rs`) and
  `machine::Machine` (This server and the setup's OCR step, `server_machine.rs`). The
  lite build passes none.

## 7. Models

- Two GitHub releases hold the files: `models-v1` (the PP-OCR ONNX files, hayai-nova's
  and paddle-manga's host files, and the ONNX graphs of the deferred path; made by
  `tools/onnx_export/`, MODELS.md) and `torch-models-v1` (the compiled AOTInductor
  packages per target and their shared weights; made by `tools/torch_export/`,
  TORCH-BACKEND.md). Both tools are Python, dev-only, never run on users' machines.
- Their tables (size and sha256 of every file) are compiled into the program (`bunko-ocr`
  `models_release.rs`, `torch_models.json`), so the release's signed manifest covers them;
  new model files arrive only with a new release.
- Files are downloaded on demand into `<storage>/models/` for the enabled generations and
  this machine's devices, resumable and verified; `MOKURO_MODELS_DIR`,
  `MOKURO_MODELS_MIRROR` and `MOKURO_TORCH_MODELS_MIRROR` serve air-gapped hosts.
  Provenance in sidecars records the export id and the source revisions.

## 8. OCR backend: libtorch packs

The binary never links libtorch, so it starts (and serves) with no backend installed. A
**backend pack** (`<storage>/backends/torch-<variant>-2.13.0/`, variant `cpu`, `cu130` or
`rocm7.1`) holds a trimmed libtorch and `libbunko_torch` (the `bunko-torch` cdylib built
against it); `bunko-engines` opens it at run time through its C ABI. A pack belongs to
exactly one release. `install-ocr` installs the pack for this hardware and `ocr.backend`,
and `serve` / `processor serve` run the same install in the background when local OCR
needs a missing backend (a new server only after its first-run setup). An update installs
the program and its pack together. Details: TORCH-BACKEND.md, PACKAGING.md §8.

## 9. One web interface, the local control API and the tray

- **A library server is set up and run from its own web pages**, the same locally, from
  another computer and in Docker: `/setup`, then the admin panel (§5).
- **The local control API** (`bunko-control`): every `serve`, `processor serve` and `gui`
  binds `127.0.0.1:<ephemeral>` and writes `<storage>/.control.json` (port, token).
  `/control/{status,pause,resume,events,stop,ocr-install,login-code}` let the tray and
  the pages see and steer the instance; `/app/*` are the desktop app's local pages (the
  first-launch chooser, the processor's pairing, settings and status), signed in with a
  one-time code from `/control/login-code`.
- **The tray** (`bunko-tray`, `mokuro-bunko tray`) is in the program: it finds instances
  through their `.control.json`, shows and pauses them, opens the admin panel, the
  processor and the library, and starts and supervises what `tray.json` lists.

GUI.md has the contract and the behaviour; GUI-COVERAGE.md maps every CLI flag to a page,
a tray item or "CLI-only".

## 10. Packaging & updates

See PACKAGING.md: four downloads per release (Windows zip, macOS disk image, Linux x64
tarball, static Linux arm64 server), built by GitHub Actions, the backend packs, and the
Docker images (`lite`, distroless-static; `full`, which downloads its OCR backend pack in
the background once it serves; `-cuda` is a tag of `full`). A signed `release.json`
manifest drives `install.sh`, `install.ps1` and the updater, which installs the program,
its backend pack and its models as one unit.

# Changelog

## [0.7.0] - Unreleased

A rewrite in Rust, as a drop-in over 0.5.2 and 0.5.3 storage (same `config.yaml`,
`mokuro.db` and library tree, same reader-facing APIs) and at parity with 0.5.3's
features. See [docs/MIGRATING-0.7.md](docs/MIGRATING-0.7.md).

### Added
- One native `mokuro-bunko` binary: no Python, uv or venvs.
- Lite and full builds. Lite is the server only and runs in 1 GB of RAM (about
  16 MiB idle in Docker); full adds local OCR and the `processor` command.
- OCR recognizers on libtorch, the same torch 0.5.2 used, from compiled model
  packages: `install-ocr` installs a *backend pack* for the hardware it finds —
  NVIDIA CUDA (Turing or newer, Linux and Windows), AMD ROCm (RX 6000/7000/9000,
  Linux) or the CPU (all platforms) — and the packages for the GPU, verified
  against the signed release manifest.
- Release packages: Linux x86_64 full (glibc 2.28+) and static lite (x86_64,
  arm64), Windows x86_64 zip with a portable mode, macOS on Apple silicon, and
  Docker images `:latest` (local OCR; the backend for the container's GPU is
  downloaded on first start) and `:latest-lite` (amd64 + arm64).
- `scripts/install.sh` and `scripts/install.ps1` install a release, checking its
  signed manifest and sha256.
- One-click updates: the admin panel's Updates card checks for new releases
  and installs them (ed25519-signed `release.json`, sha256-checked archive). Also
  `mokuro-bunko update check|apply`, and `update.*` settings.
- Processor protocol v3: one WebSocket per processor plus streamed result
  uploads, replacing 0.5's long-lived chunked streams.
- `mokuro-bunko models list|download|verify` and `mokuro-bunko healthcheck`.
- A processor needs no checkout or Python: `processor setup` and `processor serve` come
  with the binary, and `processor service` also installs a launchd agent on macOS.
- `server.threads` and `server.cache_mb` settings.
- Generation upgrade (`ocr.upgrade`, off by default) replaces old primary OCR
  with the current primary generation and keeps the old file as a layer.
- Desktop app in the browser (`mokuro-bunko gui`; double-clicking the program on
  Windows or macOS opens it): a setup wizard for a library server, a processor,
  the OCR install (with progress) and starting with the machine, settings pages
  covering every command-line option, and a live dashboard. Every running server
  and processor serves it on 127.0.0.1 only, behind a per-run token.
- Pause OCR on a processor or on the server's own OCR: after the running volume,
  now, or until a time; the pause survives restarts. Pausing now hands the claimed
  volumes straight back to the queue without counting a failure, and the admin
  panel's processor list shows who is paused until when.
- `install-ocr`, `doctor` and `models` take `--processor` to act on the
  processor's storage (picked automatically on a processor-only machine), and a
  processor also uses a backend pack installed for the library on the same machine.
- `mokuro-bunko-tray`, a tray icon for Windows, macOS and Linux: what the server
  or processor is doing (volume, pages, rate), statistics, pause after this
  volume / now / for an hour / until tomorrow, resume, and links to the
  dashboard, library, settings and logs. It can start the server or processor at
  login and restart it if it crashes. It ships next to the CLI (Windows zip, a
  `mokuro-bunko.app` on macOS, Linux full and lite archives with a `.desktop`
  entry); Linux needs GTK 3 and AppIndicator, which `doctor` checks. Docker
  images have no tray.
- Opt-in automatic updates. `update.auto: true` (config, admin panel Updates card,
  app Settings) installs a newer release on the channel by itself: new OCR work is
  held, the running volumes and uploads finish, the release is fetched and checked
  as one unit — the program, its OCR backend pack for the installed variant and its
  models — before anything is switched, then the server restarts (exec under the
  tray, a service or a terminal; exit 75 under the Windows tray or `run.bat`). If the
  new release's pack does not load after the switch, program and pack roll back
  together. A processor with `processor.auto_update: true` (`processor setup
  --auto-update`) follows its library: when the library reports a newer version it
  finishes its running volume, installs exactly that release, restarts and
  reconnects; it never downgrades. Self-managed installs only; Docker and package
  installs are told what to do. `/control/status` has an `update` block, the tray
  shows "Updating to X…" / "Updated to X", and anything that needs the owner (a
  driver, disk space, a bad signature, a rollback, a library older than its
  processor, a changed GPU) raises a `fail` problem the tray flags and announces
  once with a desktop notification.
- Processor protocol v3 additions: `version_mismatch` in the registration reply
  and the `update_status` event; the admin panel's processor list shows both.
- `update.public_key` (config file only, logged loudly) for fork and test releases.

### Changed
- Library paths are case-insensitive, as on Windows and as in 0.5.3: `kingdom/` reaches
  `Kingdom/`, an upload spelled in another case lands in the existing folder, renaming
  a folder or file to fix its case works (0.5.2 refused it as locked), and the catalog
  keeps one row per series folder, so case-variant folders that already exist no longer
  hide each other.
- A backend pack belongs to exactly one release: the program refuses a pack from
  another release ("the backend pack is from mokuro-bunko A, this is B: run
  install-ocr"), and `doctor`, `/control/status` and the tray name the pack as
  "<variant> for <release>". The admin panel's Update button and `update apply`
  install the release's pack and models together with the program.
- OCR engines are `hayai-nova` (the new default primary), `paddle-manga` and
  `ppocr-manga`, all Apache-2.0. A fresh config has one `hayai-nova` primary row.
  At fp32 their text is identical to 0.5.2's; default speed is above a tuned 0.5.2
  on every GPU tested (e.g. RTX 4090 13.9 vs 10.5 pages/s, RX 6900 XT 4.35 vs 2.75).
- paddle-manga now needs a GPU (NVIDIA CUDA or AMD ROCm); 0.5.2 also ran it on the CPU.
  CPU-only machines use hayai-nova: they no longer offer paddle-manga, fetch nothing for
  it, and say "paddle-manga needs a GPU (NVIDIA CUDA or AMD ROCm); use hayai-nova on the
  CPU" where its generation is asked for (`doctor`, `models download`, a session).
- OCR backends and models are downloaded on demand, after hardware detection and
  by the owner's preference, never baked into a release:
  - The full Docker image carries no backend pack and no model. On every start it
    runs `install-ocr --if-needed`: it detects the GPU the container was given
    (NVIDIA through the container toolkit, AMD through `/dev/kfd` + `/dev/dri`, a
    host GPU not passed in does not count), applies `ocr.backend`, downloads the
    matching pack into `/data/backends` and the enabled engines' models into
    `/data/models`, and does nothing on later starts. A new GPU gets its pack (the
    replaced one is removed). `MOKURO_OCR_AUTO_INSTALL=false` turns it off;
    0.5.2's `OCR_AUTO_INSTALL=false` is ignored, as it never stopped 0.5.2 from
    installing. `processor serve` in Docker does the same.
  - AMD GPUs work in Docker (`--device /dev/kfd --device /dev/dri`); the server keeps
    the device nodes' groups when it drops to `PUID:PGID`.
  - `:latest-cuda` / `:<ver>-cuda` is a second tag of the full image (the 0.7 CUDA
    image with its pack built in is gone), so templates written for the 0.5.2 CUDA
    image keep working (they set `PUID`/`PGID` and `MOKURO_CONFIG` themselves).
    `BAKE_PACK=1` builds a pack into a self-built image for hosts without internet.
  - `install-ocr` without `--variant` installs the pack `ocr.backend` asks for on
    this hardware: `cpu` stays on the CPU with a GPU present, `cuda`/`rocm` fall back
    to `cpu` with a hint when that GPU is not usable; `--variant auto` is the hardware
    alone. `doctor` judges the installed pack the same way. Model downloads follow
    the enabled generations: a library that only enables hayai-nova never fetches
    paddle-manga.
  - The macOS disk image no longer bundles the OCR backend and models: the setup
    wizard's OCR step detects the Mac's hardware and downloads them, as on the other
    platforms (`make-dmg.sh --bundle-ocr <dir>` still builds an offline image).
- `ocr.backend` is `auto`, `cuda`, `rocm`, `cpu` or `skip`. Automatic precision
  picks bf16 only where the hardware runs it natively (NVIDIA Ampere and newer,
  AMD RDNA3/4); RDNA2 and the CPU run fp32 like 0.5.2.
- RX 6600/6700-class cards (gfx1031/1032) get `HSA_OVERRIDE_GFX_VERSION=10.3.0`
  automatically, as in 0.5.2.
- Old configs are migrated at load: a `mokuro` row is retired (kept, disabled),
  rows on removed detectors move to `ppocr-manga`, and a `hayai-nova` primary is
  added when none is left. Existing `.mokuro` files are kept and served.
- The licence is MPL-2.0 everywhere, including the image labels (0.5 images said MIT).
- nginx download offload is off by default in the images; `MOKURO_NGINX_ACCEL=1`
  still turns it on. The in-image nginx passes WebSockets for `/_processor/`.
- Request threads are gone: the server is async, `MOKURO_THREADS` is no longer read.
- Memory: the server streams downloads and uploads from and to disk, and caches
  are byte-bounded (`server.cache_mb`); OCR uses less RAM than 0.5.2 on every
  platform measured.

### Removed
- The `mokuro` engine (manga-ocr and its GPL detector).
- The `ctd`, `animetext` and `rtdetr` detectors.
- Python packaging: the PyPI package, zipapp, `uv` portable zip, `setup-windows.ps1`
  source install (it forwards to `install.ps1`), `docs/make_volume.py`.
- Python OCR environments and `MOKURO_BUNKO_OCR_ENV`, `MOKURO_BUNKO_OCR_ENGINES_ENV`,
  `MOKURO_BUNKO_MOKURO_SPEC`, `MOKURO_PPOCR_THREADS`, `MOKURO_EFT_TRACE`,
  `MOKURO_DEBUG` (use `MOKURO_LOG` or `-v`).
- `processor install` and the install step of `processor setup`.
- Protocol v2: 0.5.2 processors cannot connect to a 0.7 library, nor the reverse.
- OCR on arm64 Linux (that platform gets the lite build; OCR from a remote x86_64
  processor) and on Intel Macs.

### Fixed
- WebDAV `MOVE`/`COPY` between a reader's own files and library files, of a file
  onto a folder, and of the library root used to delete data and answer 201;
  they are now refused (403, or 409 for a file/folder mismatch). `DELETE` of the
  library root is refused too.
- `If-Modified-Since` with an equal date answers 304 instead of 200.
- `config set`, `cors-add/-remove`, `ssl enable/disable` and the `dyndns` commands no
  longer write `MOKURO_*` overrides into `config.yaml` (Docker's `MOKURO_STORAGE=/data`
  or the nginx backend port used to be saved).
- The server shuts down gracefully on SIGTERM (`docker stop`, systemd), not only on
  Ctrl+C, also while OCR is running; 0.5.2 was simply killed.
- Removing a folder over WebDAV no longer forgets upload ownership of other
  folders whose names matched by `_` or case.
- Invites with an unreadable expiry are treated as expired instead of causing a 500,
  and an invite and its audit row are written together.
- The failed-login limiter's table is bounded in size.
- `ocr.backend: rocm` drives the AMD GPU again: pre-release 0.7 builds read it as
  WebGPU (an ONNX Runtime provider releases do not have), which left the GPU unused.

### Known issues
- On a Mac (Apple silicon, CPU OCR) hayai-nova runs about 3–4% slower than 0.5.2
  (0.497 vs 0.516 pages/s on an M2 Pro, consistent across runs); its text matches
  the reference exactly, where 0.5.2's differed on arm64.
- Windows packs are not yet tested on a machine without the VC++ runtime installed
  (they ship it app-local).

## [0.5.3] - 2026-10-06

### Changed
- Library paths are case-insensitive, as on Windows: `kingdom/` reaches `Kingdom/`.

### Fixed
- Uploads spelled in another case land in the existing folder, not a new one.
- The catalog lists every series folder; case-variant folders no longer hide each other.
- Renaming a folder or file to fix its case works (it was refused as locked).
- A case-fixing rename on a Windows or macOS server no longer deletes the folder.

## [0.5.2] - 2026-10-01

### Added
- `series.json` volumes carry `mokuro_sha256`, so readers re-download changed OCR.

### Changed
- Uploaders may replace only files of their own volumes, not others' or untracked ones.
- Generation name `updated-ocr` is reserved for readers.

### Fixed
- A re-OCR'd volume keeps its `volume_uuid`, so read progress stays attached.
- Uploaders can edit and delete their own series with non-ASCII titles.
- Uploaders can delete the OCR layer files of their own volumes.
- Volume files are revalidated, so readers never install stale cached OCR.
- An expired token on the volume manifest gets a Bearer challenge, not Basic.

## [0.5.1] - 2026-09-30

### Added
- Bearer tokens: sign in once with `POST /login/api/token`, then send the token.
- The web pages and processors use tokens; passwords are no longer stored.

### Changed
- PP-OCRv6 manga models updated to v0.2, run at the authors' best settings.
- Queue status and queue file list the next 100 waiting volumes; totals stay whole.

### Fixed
- Queue page, queue file and claims share one library walk; large queues load fast.
- OCR settings page no longer stalls on large libraries; volume counts fill in.

## [0.5.0] - 2026-09-29

### Added
- OCR on other machines: `mokuro-bunko processor` logs in and runs the queue.
- `processor` account role for OCR machines, granted only by an admin.
- `processor setup` checks the account, writes processor.yaml, installs and starts it.
- `processor service`: a systemd user unit, or a Windows Startup entry.
- `admin restore-user` brings back a deleted account.
- AMD GPUs need no system ROCm; unbuilt RDNA cards run as their family's target.
- Each volume goes to the machine predicted to finish it first.
- `MOKURO_EFT_TRACE=1` logs which machine gets which volume, and why.
- `ocr.local_processing: false` leaves all OCR to remote processors.
- OCR generations: an ordered list of named OCR recipes, one layer each.
- `hayai-nova` and `paddle-manga` engines, stronger on display text and sound effects.
- `ppocr-manga` engine reads text lines, including scanned novel pages, on CPU.
- Text detectors for the new engines: `ppocr-manga`, opt-in GPL `ctd`.
- Per-generation pools: stage widths and devices.
- Per-generation precision mode (accuracy, balanced, speed, or one format) for every machine.
- Benchmark and tune a generation on your own pages, from the admin panel.
- New generations are benchmarked automatically on each machine (`ocr.autobench`).
- `ocr.concurrency` OCRs several volumes at once.
- `ocr.sessions` keeps OCR models loaded between volumes.
- Queue page: one card per machine, and `queue.display` detail levels.
- Queue page shows a finishing time for every pending volume.
- Admin panel: generations editor, Processors card and per-row congestion history.
- Extra OCR layers record their engine, detector and pinned model commits.
- Every OCR sidecar records which machine wrote it; results are audited.
- Admin audit log: search, filters, paging; reading-progress sync hidden by default.
- OCR settings saved in the admin panel apply without a restart.
- `auto` backend rebuilds a CPU-only OCR environment when a GPU is available.
- Catalog read links carry a volume manifest listing its OCR, layers and cover.
- Archives uploaded over WebDAV join the OCR queue at once, not at the next poll.
- Manifests and `.cbz` PUT replies say when pending OCR should be done.
- Every WebDAV upload answers a verdict: verified and stored, or why not.
- Uploads may carry `Content-Digest`, telling damage in transit from a damaged file.
- `/mokuro-reader/.mokuro-queue.json`: the whole OCR queue, per volume, with ETAs.

### Changed
- mokuro runs from the optimized fork and stays loaded; output unchanged.
- `ocr.backend: skip` now means no OCR on this machine, not none anywhere.
- OCR queue goes round-robin across series in reading order, not upload date.
- Volumes missing pages get only their primary OCR layer.
- Queue page shows raw errors, log paths and machine names to admins only.
- `GET /queue/api/status` is grouped by machine; `current` and `pending_ocr` are gone.
- A failed GPU install is retried once without pip's cache; a CPU fallback warns loudly.
- Python 3.11 or newer is required; tested on 3.11 through 3.14.
- Unraid image: Ubuntu 24.04 and Python 3.12; its OCR environment is rebuilt.

### Fixed
- A cut-short upload of a sidecar or JSON file is refused, not stored short.
- OCR of a volume deleted or replaced mid-run no longer leaves a stray sidecar.
- Benchmarks taken at a precision a machine no longer runs are re-measured.
- Benchmarks sample interior pages of a few volumes, not every volume's cover.
- A volume's OCR time no longer includes the previous volume's tail.
- A processor no longer re-registers when a session the library ended reports late.
- A cancel or benchmark pre-empt that lands before an OCR process starts is no longer lost.
- OCR retry backoff no longer freezes the queue after many failures.
- Sidecars for names with brackets like [Author] now import.
- Same-named volumes in different series no longer overwrite each other's OCR log.
- A stray `goals.json` in the shared folder no longer breaks the library listing.
- Windows consoles on legacy code pages no longer crash commands.
- Cancelling a running benchmark no longer reports it as failed.
- Audit log pages stay fast on SQLite 3.53 and newer.
- A mokuro-only server runs served mokuro without an engines environment.
- Large libraries no longer stall OCR sessions between pages.
- The queue page rescans large libraries less often.

### Security
- Catalog folder names can no longer run script in the page.
- Proxy headers are trusted only from this machine or `trusted_proxies`.
- Uploaded archives that inflate far beyond their size are refused.
- A machine name belongs to the processor account that first used it.
- Processors ignore library ids that are not plain ids.

## [0.3.6] - 2026-09-02

### Fixed
- **goals.json is now a per-user file.** It joins volume-data.json and profiles.json, so mokuro-reader's reading goals sync per account instead of into the shared library.

## [0.3.5] - 2026-08-29

### Added
- **Incomplete volumes are counted and surfaced.** Each `series.json` volume entry now carries `matched_page_count`: how many of the pages its `.mokuro` references actually exist inside the `.cbz`. Matching is a port of the reader's own `matchImagesToPages` — exact path, then stem (an extension changed since OCR), then the whole-volume positional fallback for archives whose images were renamed — so the server and the reader agree on which pages are missing. The field is omitted, never zeroed, when the match could not be determined (an unreadable archive, or a sidecar that names no images at all); an unreadable archive is also left out of the entry cache so it is re-checked next pass instead of being remembered as broken.
- **The catalog shows missing pages.** Volume cards badge how many pages are absent, series cards badge how many volumes are incomplete, and a "Missing pages" filter above the grid narrows the library to just those series. The filter appears only when the library actually has damage.

### Changed
- **Archive contents are read the way the reader reads them.** An archive's images are now listed with the reader's own filters — OS junk (`__MACOSX`, `._` forks, `Thumbs.db`, backup files) and the volume's embedded cover sidecar are excluded, and `.avif`/`.jxl` join the recognised image types. Page counts for image-only volumes shift accordingly, in the direction of what a reader would actually display.

## [0.3.4] - 2026-08-29

### Fixed
- **Renames over HTTPS always failed with 502.** The internal nginx overwrote the front proxy's `X-Forwarded-Proto: https` with its own plain-http scheme, so wsgidav's MOVE/COPY destination check ("source and destination must have the same scheme") rejected every rename arriving through a TLS-terminating edge — deterministically, since the deployment's first day. The header now passes through, falling back to the local scheme only for direct connections.

## [0.3.3] - 2026-08-29

### Fixed
- **Long metadata passes no longer starve the whole server.** A full compile pass held the metadata lock for minutes on a cold library; every incoming `series.json` update parked on it, the worker thread pool jammed, and every request — renames, covers, even OPTIONS — failed at the proxy with 502 until the pass ended (observed live as failing renames and silently dropped link/title edits). The pass now takes the lock per series with an explicit fair handoff, full passes stay mutually exclusive, and an update that still cannot get the lock within 10s is answered `503 Retry-After` instead of pinning a thread. Shutdown aborts a running pass between series instead of waiting out the crawl.

## [0.3.2] - 2026-08-29

### Changed
- **JSON metadata is gzip-compressed for transport.** `catalog.json` and `series.json` downloads shrink ~10x; the reader refreshes the catalog on every listing by design, so this applies to every refresh. Archives and images are untouched.

## [0.3.1] - 2026-08-29

### Added
- **Instant enrichment on link.** A metadata update that introduces or changes a series' AniList/MAL id fetches its rating, tags and genres within seconds, instead of waiting for the hourly sweep (which remains the refresh and catch-all).

## [0.3.0] - 2026-08-28

### Added
- **Catalog page overhaul.** Series cards can render display titles as a language progression — Native (native → romaji → english → folder), English (english → romaji → native → folder), or plain folder names (default: Native) — with the user's series tag appended to alt-title renders ("よつばと！ (HD Scan)"). Sort by A–Z, Newest (latest archive mtime), Wordiest (characters per page), or Rating; filter by genre chips; search matches every title variant, tag and genre. Grid controls hide inside a series view, and backing out restores search, filter and scroll position.
- **Community enrichment from AniList/MAL.** A background fetcher fills ratings, tags and genres for every series whose client-submitted facts carry an external id: AniList primary (batched GraphQL, no key needed), Jikan for MAL-only links, scores normalized to one 0–100 scale, refreshed weekly. `catalog.enrich_community` (default `true`) switches it off.
- **Materialized catalog database.** Each series' render-ready row (folder, cover, volume count, latest archive mtime, page/character totals) is kept in a `catalog_series` table, upserted by the same passes that compile metadata and re-synced by a 6-hour periodic full pass. The catalog listing is now a single database read — no filesystem walk on the request path.
- **Volume uploads trigger recompilation.** Client uploads of volume files (`.cbz`, `.mokuro`, covers) schedule a per-series recompile through the filesystem watcher, so `series.json` and `catalog.json` follow uploads within seconds instead of waiting for a quiet window. Both debounces are capped so a sustained upload stream can no longer starve compilation. Metadata passes log `[METADATA]` fire/done lines.
- **Crawler opt-out.** Every response carries `X-Robots-Tag: noindex, nofollow`, and `/robots.txt` (served outside auth) disallows all compliant crawlers.
- **Server-side compilation of the reader's metadata files.** mokuro-bunko now compiles `<Series>/series.json` (v2: series facts plus an index of the series' volumes — uuid, title, page and character counts, mokuro version, spine width, archive size, freshness stamps, shelf offsets) and a root `catalog.json` (name/mapping/search data for every series folder) from the library's own `.mokuro` and `.cbz` files. Both are regenerated when the library changes and served with accurate size/mtime, so clients can cache them; a rebuild that changes nothing rewrites nothing. Character counts are computed with the reader's own counting rules, and volumes with no OCR sidecar are indexed as image-only with the uuid the reader derives for them.
- **Metadata updates from accounts that cannot write to the library.** A `series.json` PUT is treated as an update REQUEST: the facts fields are validated, merged newest-stamp-wins against the server's store (a factless payload never clears a link unless it is strictly newer — an explicit unlink), and both files are regenerated. Shelf offsets ride along as index data and never move the facts stamp. Repeating an identical update is a no-op, so a client can retry safely.
- **Cover sidecars are generated even when OCR is disabled** (`backend: skip`), since the reader now installs them onto volumes it has not downloaded.
- **Freshness stamps on each volume entry.** `series.json` volume entries optionally carry `mokuro_size`/`mokuro_modified` and `cover_size`/`cover_modified` — integer byte sizes and integer epoch seconds from a plain `stat()` of the `.mokuro` sidecar and the cover `.webp`, omitted (not `null`) when either doesn't exist. Clients use these to detect a stale local copy without downloading anything: a size mismatch, or a strictly newer `_modified` than what they have stored, means re-fetch.
- **`/login/api/me` reports metadata write scope.** `permissions.metadata` is `all`, `owned` (with an `ownedSeries` list), or `none`, so the reader can show only the series.json edits an account may actually submit.

### Fixed
- **Metadata updates for non-ASCII series titles were always rejected.** PEP 3333 delivers `PATH_INFO` latin-1-encoded and wsgidav re-encodes it only inside the DAV app, beneath the interception middleware — which parsed the mojibake spelling, failed folder resolution, and answered 400 for every Japanese-titled `series.json` PUT (observed: 102 rejections in one upload session, with clients retrying up to 23 times per series). The middleware now applies the same re-encode to its own copy of the path.
- **Sidecar uploads no longer create volume ownership.** A cover/mokuro PUT onto an untracked volume (legacy content, or anything predating ownership tracking) used to insert an ownership row — a blind sidecar backfill could hand an uploader edit and delete rights over series they never made. Ownership now comes only from uploading the archive.
- **Catalog listing payload cut ~40×.** The root listing no longer nests every volume of every series (~3 MB of JSON at 1,000-series scale that the grid never rendered) and JSON API responses gzip when the client accepts it.

### Changed
- Compiled metadata files are owned by the server: `catalog.json` cannot be written by any account, and neither compiled file can be deleted, moved or copied. Rejections are ordinary 403s — a client that treats metadata writes as best-effort keeps full read/write access to everything else.
- **Cache-Control on cover and page image responses.** Image GETs now send `Cache-Control: private, max-age=86400`, letting browsers cache them instead of re-fetching on every page turn; `series.json`/`catalog.json` keep `no-store`.

## [0.2.0] - 2026-07-11

### Fixed
- **Fresh OCR installs were broken (transformers 5.x).** `install-ocr` installed `transformers` unpinned; 5.x cannot instantiate manga-ocr's tokenizer, so a brand-new install silently produced thumbnails but never `.mokuro` files. `install_mokuro()` now installs `mokuro "transformers>=4.25,<5" sentencepiece`, and every install ends with an in-env smoke test (`verify_installation()`: imports torch/transformers/sentencepiece/manga-ocr/mokuro, checks the version pin, reports CUDA availability) so a broken environment fails loudly at install time. Existing broken envs: `mokuro-bunko install-ocr --force`.
- **Silent per-volume OCR failures.** mokuro exits 0 even when a volume fails, and its output was piped to DEVNULL — failures were invisible and retried every poll interval forever. Now: mokuro's combined output is captured to `<storage>/logs/ocr/<volume>.log`; exit-0 failures are detected from mokuro's own `Processed successfully: N/M` summary; a short error reason is extracted from the log (final traceback line / loguru ERROR / "No module named"); failures are persisted to `<storage>/.ocr-failures.json` with attempt counts and retried with exponential backoff (poll×4^attempts, capped at 1h). Replacing a failed `.cbz` (newer mtime) resets its record.
- Repo now ships `.python-version` (3.12) so `uv sync` always provisions a CUDA-compatible interpreter (CUDA wheels are unavailable on Python ≥3.13; the installer refuses CUDA there).
- shiv binary dep lists: added missing `click` to `scripts/build-binary.sh` and `Pillow` to both build-binary.sh and the release workflow.

### Added
- **`mokuro-bunko doctor`** — diagnostics command printing a PASS/WARN/FAIL table with fix hints: Python version, config/storage writability, NVIDIA driver, OCR env + full stack smoke test, disk space, port availability, failed-volume count. Exit 1 on FAIL (scriptable).
- **Windows one-command installer** (`scripts/setup-windows.ps1`): `irm .../setup-windows.ps1 | iex` downloads the source (no git needed), installs uv, syncs, installs OCR (GPU auto-detected), runs doctor, creates a start script + desktop shortcut, starts the server and opens the browser to the first-run wizard. PowerShell 5.1 compatible, no admin, idempotent, transcript in `%TEMP%\mokuro-bunko-setup.log`.
- **Portable Windows edition** (`scripts/build-portable.ps1` → `dist/mokuro-bunko-portable-windows-x64.zip`): extract anywhere and double-click `run.bat`; bundled uv.exe bootstraps Python + OCR into the folder on first run. All state (runtime, models, config, library, logs) stays inside the folder — move by copying, uninstall by deleting. Includes `doctor.bat` and a plain-English README.txt.
- **Persistent logging.** Rotating server log at `<storage>/logs/server.log` (the `logging` module is now actually configured; previously watcher.py's log calls were dropped and nothing was persisted). OCR worker/installer output goes through loggers instead of bare prints.
- **Queue page shows failures.** New "Failed" section listing each failing volume with its error, attempt count, and log path; failed volumes no longer masquerade as "pending". `/queue/api/status` gains a `failed` array.
- `/api/health` gains an `ocr` section: backend, worker liveness (heartbeat file), pending and failed counts.
- `docs/troubleshooting.md`; README quick-start rewritten around the new install paths.

## [0.1.8] - 2026-07-08

### Fixed
- **Sporadic `502 Bad Gateway` on WebDAV renames (and other requests) under the nginx X-Accel download offload.** With `MOKURO_NGINX_ACCEL=1`, `MokuroFileResource` served every library download (`.mokuro`/`.webp`/`.cbz`) with an empty body and *dropped* the `Content-Length` header. WsgiDAV force-closes any keep-alive response that has a body-bearing status but no `Content-Length` (`wsgidav_app.py` `_start_response_wrapper`), so every single library GET tore down its upstream connection — logged as `Missing required Content-Length header in 200-response: closing connection` (thousands per hour). That churn poisoned nginx's `keepalive` upstream pool, and a reused-then-closed socket surfaced to the reader as a sporadic `502` on unrelated requests such as volume renames. The offload response now sends `Content-Length: 0` (the Python body genuinely is empty; nginx overrides it with the real file size when it serves the file), keeping the upstream connection reusable. Verified against a real nginx + cheroot harness: the full file is served whether upstream sends `Content-Length: 0`, the real size, or none.

## [0.1.6] - 2026-06-29

### Fixed
- **CORS broken on nginx-offloaded library downloads (regression from 0.1.5).** Two independent faults in the in-container nginx both surfaced as `Access-Control-Allow-Origin`-missing errors in the reader:
  - **Missing CORS on every successful download.** nginx does not carry the upstream (Python) response headers onto the file it serves via the `X-Accel-Redirect` internal redirect, so the `Access-Control-Allow-Origin` that `CorsMiddleware` set was dropped and *every* `200`/`206` library download failed the browser's cross-origin check. CORS is now re-attached on the internal library location (reflecting the request `Origin`; only shared, anonymously-served library files reach that location, so reflection exposes nothing and avoids duplicating Python's allowlist in nginx).
  - **CORS-less `503` under load.** The internal nginx also throttled requests per-IP (`limit_req`/`limit_conn`); the reader's normal burst of concurrent thumbnail GETs tripped the limit, and nginx returned the `503` *itself* — before the request reached Python — with no CORS header. Removed the per-IP throttle: abuse control belongs to the front proxy, and X-Accel-Redirect already keeps downloads off Python's thread pool (the thread-exhaustion the limit was meant to prevent).
  - As a safety net, errors nginx generates itself (e.g. backend down/timeout) now route through a CORS-bearing error handler, so a real failure surfaces to the reader as a readable `503` instead of an opaque cross-origin block. Responses on the `location /` proxy path are untouched and keep Python's own CORS header (no duplication).

## [0.1.5] - 2026-06-29

### Added
- **nginx X-Accel-Redirect download offload.** When `MOKURO_NGINX_ACCEL=1`, library file downloads are served by nginx via `sendfile()` instead of holding a cheroot worker thread for the whole transfer — the main driver of 503 / thread-pool exhaustion under download load (cf. 0.1.1 watchdog). `MokuroFileResource` emits `X-Accel-Redirect` for GETs of library files (the redirect path is confined to the library root and URL-encoded), returns an empty body, drops `Content-Length`, and delegates `Range` handling to nginx; the internal nginx location is aliased to the library root only and marked `internal`. Enabled by default in the generic Docker image (`deploy/Dockerfile`). On the Unraid image it is **opt-in**: set `MOKURO_NGINX_ACCEL=1` and nginx fronts the public port while Python moves to `MOKURO_BACKEND_PORT` (default 8081). Cheroot thread count is now configurable via `MOKURO_THREADS` (default 50).

### Changed
- **Simplified mokuro OCR invocation.** Dropped the redundant `--output-dir` flag (mokuro writes sidecars next to the input regardless) and removed the now-dead compatibility-fallback retry path. Raised the finalizing-phase timeout from 180s to 900s so large volumes are no longer killed during mokuro's finalize step.

## [0.1.4] - 2026-06-11

### Fixed
- **Silent anonymous downgrade on non-UTF-8 Basic auth.** Legacy clients (browser `btoa`, the npm `base-64` package) encode `username:password` as Latin-1 bytes; such headers failed UTF-8 decoding and were silently served as anonymous read-only — users with non-ASCII passwords could browse but never sync or upload, with no error. Any present-but-undecodable `Authorization` header (Latin-1 bytes, invalid base64, missing colon) now returns 401 with a `charset="UTF-8"` challenge. Credentials must be UTF-8 encoded (mokuro-reader ≥1.6.2 complies); requests without any `Authorization` header remain anonymous as before.
- `WWW-Authenticate` challenges now advertise `charset="UTF-8"` (RFC 7617) so compliant clients encode credentials as UTF-8.
- Login, account, and setup pages now build Basic-auth strings with a UTF-8-safe encoder instead of bare `btoa`.

### Added
- `/login/api/me` identity endpoint extended: reports `authenticated` (boolean, present in every response) and a `permissions` object (`canWriteProgress`, `canAddFiles`, `canModifyDelete`); returns `200` with `authenticated: false` for credential-less requests instead of 401, and is rate-limited against credential stuffing. Existing `username`/`role`/`created_at` keys are preserved.

## [0.1.2] - 2026-03-09

### Added
- WebDAV MOVE/COPY support for files and folders in the library
- Rename files and folders directly from WebDAV clients (e.g. mokuro-reader)
- OCR sidecar files (.mokuro, .mokuro.gz, .webp, .nocover) are moved alongside renamed CBZ volumes
- Folder moves are atomic with recursive volume upload tracking updates
- Audit logging for move and copy operations

## [0.1.1] - 2026-02-28

### Fixed
- **503 Service Unavailable on Windows.** Cheroot worker threads can die from unhandled Windows socket errors ([cheroot#375](https://github.com/cherrypy/cheroot/issues/375), [cheroot#710](https://github.com/cherrypy/cheroot/issues/710)), eventually leaving zero threads to process requests. Added a thread pool watchdog that detects dead threads and replaces them, plus a resilient serve loop that recovers from the interrupt flag a dying thread sets.
- **Windows compatibility.** `os.rename()` replaced with `os.replace()` for atomic file moves (Windows fails if the destination exists). `os.umask()`/`os.chmod()` guarded on Windows where they have no effect on NTFS.
- **SQLite concurrency.** Enabled WAL journal mode, `busy_timeout`, and longer connection timeout to prevent database locking under concurrent access.

### Added
- Audit logging for account self-deletion
- Soft-delete users (sets status to `deleted` instead of removing the row, preserving audit trail and upload ownership records)
- 30-day audit log retention with automatic pruning
- Debug request logging (set `MOKURO_DEBUG=1` to log every request with thread name, method, path, status code, and timing)

## [0.1.0] - 2026-02-25

- Initial release

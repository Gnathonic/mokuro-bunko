# Changelog

## [Unreleased]

### Fixed
- **This server → Engines and models tells the truth about the models folder.** Each
  engine's row counts what it needs on this machine: the models-v1 files and the
  compiled package (graphs and shared weights) for the device and precision it runs at
  ("bf16 · linux-cuda-sm_80 on gpu:0"), present out of needed, and bytes on disk. An
  engine not in use says what enabling it would download (from the manifest). The
  total line is the folder's size (as `du --apparent-size`), every file counted once:
  what the engines in use need, files of engines not in use, files nothing needs, and
  the rest (checksum stamps). The OCR backend's own size (and the part NVIDIA's CUDA
  wheels added) is on the OCR backend card. `models list` prints the same plan, and
  `models download` / `doctor` share its planning code, so they cannot disagree.

## [0.7.0-beta.4] - 2026-10-09

### Added
- **The OCR backend installs in the background; getting up and serving never waits
  for it.** `serve` and `processor serve` start at once and, when local OCR is on and an
  enabled generation needs the libtorch backend that is missing (any OS, not only
  Docker), run the same detection and install as `install-ocr --if-needed` in the
  background. The progress (stage, percent, bytes, pack) is in the log, the admin panel
  (Settings → OCR → Processors), the dashboard, the tray and `/control/status`
  (`install`). The server's local OCR starts by itself when it is done; a processor
  registers at once as not available (reason `installing`; the library's list shows
  "installing OCR backend: NN%") and takes work once its backend is ready. A failed
  install is a problem ("needs you", tray "!") with a Retry button, retried after 5, 15
  and 60 minutes. `MOKURO_OCR_AUTO_INSTALL=false` still means never (the missing backend
  is then a problem with an Install button). One install at a time per backends
  directory; an interrupted one resumes its downloads at the next start. `install-ocr`
  stays a foreground command for scripts.
- **One web interface for a library server, the same locally, remotely and in Docker.**
  The server's own first-run page (`/setup`) is the setup wizard: the admin account,
  who may join, remote access (Cloudflare tunnel, Dynamic DNS, HTTPS, other web apps)
  and **OCR on this machine** (on by default when a usable GPU is found; Auto, CPU or
  the GPU found). Finishing signs the admin in and opens the admin panel, where the
  OCR backend's background install shows its progress. Answers the environment fixes
  (`MOKURO_REGISTRATION_MODE`, `MOKURO_SSL_ENABLED`, `MOKURO_OCR_BACKEND`) are shown, not
  asked. Before setup, a new server does not start installing OCR by itself. Every
  setup route, its reads included, answers only while no admin exists and only to
  localhost or a holder of the setup code; HTTPS certificate files can be named only
  from the server itself.
- The admin panel's new **This server** tab: OCR on this machine, the hardware, the
  backend in use ("rocm7.1 for 0.7.0-beta.4"), the backend preference (`ocr.backend`:
  Auto, CPU or the GPU found; a switch installs what is missing, and restarts the
  server when another backend is already loaded), Install / Reinstall / Remove with
  live progress, the engines' models (download, verify, details), diagnostics
  (`doctor`) and the server log. Admin only, behind the panel's CSRF checks; installs
  come only from the signed release or the `ocr-offline` folder next to the program (no
  folder from a page); one install at a time and a short cooldown between actions.
- The running version is shown at the foot of the admin panel, on the local processor
  pages and as the tray menu's first (greyed) line.
- First-run setup from another computer (Docker, a NAS): while no admin exists the
  server prints a one-time **setup code** in its log at every start (`First run:
  create the admin account at http://…/setup (setup code: XXXXX-XXXXX)`). `/setup`
  opened from another address asks for it (an HTML page; five tries per address,
  one more every 12 s; after 30 wrong codes within a minute from everyone the code is
  replaced and the new one logged, so a flood never locks the owner out), then shows
  the wizard; scripts may send it as `X-Setup-Code`.
  Localhost still needs no code. The code lives in memory only and dies once an admin
  exists. 0.5 allowed setup from localhost only.

### Changed
- The desktop app is only what is local. With nothing set up, `mokuro-bunko` opens a
  chooser: **Library server** (pick the folder; the tray starts the server on a free
  port and the browser moves to its `/setup`) or **Processor** (the local pairing page:
  library address, login, connection test, name, sessions, automatic updates; then
  its own status and settings pages). The tray menu: status, pause, Statistics, Open
  admin panel, Open processor, Open library, Start at login, Quit. Removed: the
  desktop server setup and server settings pages, the "Start with the machine" step,
  the Start-up settings page and installing services from a page (the tray's "Start at
  login" checkbox replaces them; `processor service --install` stays a command). The
  login page now returns to the page that sent you there.
- The full Docker image's entrypoint no longer runs `install-ocr --if-needed` before
  the server: the server answers within seconds of the first start (health check start
  period 30 s instead of 20 min).
- Docker images: Publish also moves `beta` / `beta-lite` (every release, beta or
  stable). The Unraid templates and compose files of the 0.7 branch default to them (no
  `latest*` exists before 0.7.0) and the templates' `<TemplateURL>` points at the `0.7`
  branch (`main` still has the 0.5 templates); both go back to `latest` / `main` with
  0.7.0.
- **Generation upgrade (`ocr.upgrade`) replaces the old primary file; it no longer
  keeps it as a layer.** beta.3 kept the replaced `<Volume>.mokuro` beside the new one
  as `<Volume>.<old-name>.mokuro` (usually `<Volume>.mokuro.mokuro`); now the new file
  replaces it outright, atomically, and the old bytes are gone. Every gate stays: a
  volume a person edited is skipped (an admin can force one), a short archive is never
  regenerated. A direct replace removes the layer it copied when that layer is
  byte-identical and no enabled generation writes it. The audit event
  `ocr_sidecar_upgraded` says `replaced: true` (no `kept_as`). The admin panel's
  per-volume Revert and `POST /_admin/api/ocr/upgrade/<volume>/revert` are gone (410):
  with no kept layer there is nothing to revert to. Extra OCR beside the primary is
  what non-primary generations are for.
- The layers beta.3's upgrade kept (stamped `ocr_engine.upgraded_from_primary: true`)
  are removed, with their saved originals under `<storage>/.upgrade-originals/`, at
  startup (the ones its audit names) and at the next census, once the volume's bare
  file is the upgraded output. One log line per file. A layer without that stamp (a
  person's, another generation's, what a forced upgrade kept of an edit) is never
  touched.
- The setup token of the earlier 0.7 betas (`<storage>/.setup-token`,
  `/setup?token=`, `MOKURO_SETUP_TOKEN`) is gone: its file is deleted at startup and
  the setup code replaces it. A remote setup API call without a code is answered with
  how to get one ("…see the server log for the setup code").

### Fixed
- A library server set up without its OCR backend (beta.3's wizard had OCR install as
  a separate card) ran with ppocr-manga only and said so in one INFO line: a missing
  backend for an enabled generation is now installed in the background, or shown as a
  problem everywhere.
- The tray's Quit did not stop a server the setup wizard had just started (it was the
  wizard's child, and the tray treated it as someone else's): what the wizard starts is
  handed to a running tray (tray.json, picked up live; an instance started for the tray
  is adopted), so Quit stops it; an instance the tray does not manage (a system service)
  still keeps running, and Quit says so.
- A failed libtorch backend load was remembered for the life of the process; a backend
  installed later is now picked up without a restart.
- Generation upgrade marked every volume uploaded together with its existing `.mokuro`
  "skipped (edited)": adding a volume's OCR file counted as a hand edit. Only changing
  an existing one does now (a WebDAV overwrite with different bytes, a revert, or a file
  newer than its provenance row). A PUT that re-sends a `.mokuro` byte for byte (a
  backup, a re-upload) leaves the file, its mtime and its provenance alone and is
  audited as an `edit` with `"unchanged": true`, which is not an edit either. Volumes
  flagged before are judged again at the next start.
- The admin panel's pools table said a stage left on Auto runs on the CPU ("Auto →
  CPU") whatever the machine had, so after a GPU backend was installed it kept saying
  the engine would use the CPU while volumes were read on the card. Auto now reads as
  the device it resolves to (this server's first card while its OCR runs, a processor's
  own first card in its table), as soon as the backend is loaded; the copies box of an
  engine on a card says "auto" rather than 1 (a fast card on a large host runs two).
- A benchmark run by hand on this server was saved for the admin panel only, so the
  scheduler benchmarked the same row again before its first volume. It is now this
  server's benchmark of the row (its rate; the pools stay as set), and one benchmark
  per engine and machine is enough.
- After a failed OCR backend install the server started its OCR anyway and logged
  "Starting this server's OCR: its OCR backend is installed". With no backend in place
  it now waits (the failure shows with a Retry, and the automatic retries go on) and
  starts once an install succeeds; a failed install never restarts OCR that runs.

## [0.7.0-beta.3] - 2026-10-09

A pre-release on the way to 0.7.0 (everything below under 0.7.0 applies).

### Changed
- Four downloads per release, named for people: `mokuro-bunko-<ver>-windows.zip`,
  `-macos.dmg` (Apple silicon), `-linux-x64.tar.gz`, `-linux-arm64-server.tar.gz`
  (the server; OCR from a processor), plus the Docker images. The notes start with a
  table linking each one. No lite Windows/macOS download, no Intel macOS build, no
  static x64 Linux tarball (the lite Docker image stays). OCR backend packs are
  `mokuro-bunko-backend-<ver>-<platform>-<variant>.tar.zst`; the per-file `.sha256`
  files are gone (`SHA256SUMS` and the signed `release.json` cover everything).
- One program: the tray is `mokuro-bunko tray`, and `mokuro-bunko` with no arguments
  starts the app (on a desktop the tray, the first time with the setup wizard;
  headless what is set up, or `setup`). On Linux the tray needs no GTK or AppIndicator
  library (`doctor` checks for a tray host; GNOME needs the AppIndicator extension).
  On Windows `mokuro-bunko.exe` is the app (no console window) and
  `mokuro-bunko-cli.exe` the same program for terminals. On macOS opening the app runs it.
- The macOS updater installs from the disk image, and the app is sealed (ad hoc) when
  built and again after an update: `codesign --verify` passes before and after.

### Fixed
- An automatic update's download progress reaches the log as it happens.
- A server with automatic updates off logs "X is available" once per version.

## [0.7.0-beta.2] - 2026-10-09

A pre-release on the way to 0.7.0 (everything below under 0.7.0 applies): an
install of 0.7.0-beta.1 finds it on the pre-release channel and can update to
it. Fixes `models verify` after `install-ocr` (see 0.7.0, Fixed).

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
- Release packages: Windows x86_64 zip with a portable mode, macOS on Apple silicon as a
  drag-to-Applications disk image (`.dmg`), Linux x86_64 (glibc 2.28+), a static Linux
  arm64 server, and Docker images `:latest` (local OCR; the backend for the
  container's GPU is downloaded on first start) and `:latest-lite` (amd64 +
  arm64).
- `scripts/install.sh` and `scripts/install.ps1` install a release, checking its
  signed manifest and sha256.
- One-click updates: the admin panel's Updates card checks for new releases
  and installs them (ed25519-signed `release.json`, sha256-checked archive). Also
  `mokuro-bunko update check|apply`, and `update.*` settings. `update.channel`
  defaults to `auto`: a pre-release install (a beta or release candidate) follows
  later pre-releases, a stable install only stable releases; `stable` or
  `prerelease` pins it.
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
- A tray icon for Windows, macOS and Linux (`mokuro-bunko tray`): what the server
  or processor is doing (volume, pages, rate), statistics, pause after this
  volume / now / for an hour / until tomorrow, resume, and links to the
  dashboard, library, settings and logs. It can start the server or processor at
  login and restart it if it crashes. It is part of the full build (`Mokuro Bunko.exe`
  on Windows, the app on macOS, a `.desktop` entry on Linux); the lite build and the
  Docker images have none.
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
- The CPU's compiled model packages no longer carry their own copy of the weights:
  like the GPU packages they use the engine's shared weights files. hayai-nova on the
  CPU downloads 0.57 GB in fp32 (was 0.79 GB) and 0.29 GB in bf16 (was 0.40 GB), uses
  about 200 MB less memory, and reads the same text at the same speed.
  paddle-manga's CPU packages use its fp32 GPU packages' 3.6 GB of weights
  instead of 4.65 GB each of their own (and about 1.3 GB less memory, same text,
  same speed). The compiled-models release shrinks
  from 25.6 GB to 8.96 GB.
- Where the CPU is picked for paddle-manga, the setup wizard, the settings page
  and the admin panel's pools table say it is slow there (about 40 s a page on
  16 threads) and that hayai-nova is the CPU engine of choice.
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
- The admin panel's users list is newest first with a fixed order for accounts
  created in the same second (the newer one first); 0.5 left those in whatever order
  SQLite returned them.

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
- A processor's result for a volume whose name holds `? : * " < > |` or is a Windows
  device name (`CON`) is accepted again: 0.7.0-beta.1 refused it, so such volumes
  never got OCR from a processor. A processor holds the finished file under a fixed
  name, so a Windows processor can read them too, and the library logs every result
  it refuses.
- `/api/health`'s `ocr.pending` counts the volumes without a primary sidecar again,
  as in 0.5 (0.7.0-beta.1 counted the waiting jobs); the job count is the new
  `ocr.queued_jobs`.
- The admin panel's generations list shows a retired `mokuro` primary's sidecar as
  `<Volume>.mokuro` (it read `<Volume>.mokuro.mokuro`) and counts its files, and the
  volume totals count series volumes as 0.5.3 did, not loose archives at the library
  root.
- Archives with an upper-case extension (`Vol 1.CBZ`) get a cover and OCR; 0.5 and
  0.7.0-beta.1 listed them in the catalog but their library walks skipped them.
- `doctor` no longer asks for `install-ocr` and a model download when
  `ocr.local_processing` is off (one INFO line instead), and checks only the models
  of the engines the enabled generations use.
- `models verify` accepts an unpacked libtorch model package (its archive was checked
  before the unpack and its stamp names the manifest's sha256) instead of failing
  with "Is a directory", as 0.7.0-beta.1 did after every `install-ocr`.
- At start the server no longer logs "OCR is waiting for hardware" while its own OCR
  is still coming up, and prints each config migration warning once, not twice.

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

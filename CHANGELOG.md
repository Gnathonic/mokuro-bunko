# Changelog

## [Unreleased]

### Added
- **Set a library up in Google Drive, OneDrive, MEGA or a WebDAV server.** The setup
  wizard asks where the library lives, then connects it with one button: a browser on
  the server signs in directly. Anywhere else (Docker, a NAS, a phone), Google Drive
  gives a code to enter on any device, shown in the wizard, the admin panel, the tray
  and the log; OneDrive gives its sign-in page to open on any device, then you paste
  back the address the browser ends on (`http://localhost:…/bunko?code=…`), since personal
  Microsoft accounts cannot sign in with a code (a code stays available for work or
  school accounts, and a code refused for a personal account says to use the browser
  instead). The
  Check step counts the volumes Bunko can see; on Google Drive it asks which Mokuro
  Reader you use and offers the other one when few volumes are visible. Readers can
  keep reading straight from the cloud (the default for a cloud library), or connect to
  Bunko as before. The admin panel's new **Storage** tab shows the library's health,
  first sync, uploads, conflicts, lease and quota, and connects, signs in again,
  changes the Drive pairing or disconnects (waiting for, exporting or discarding what
  is not uploaded yet; nothing in the cloud is deleted). An export from the admin panel
  goes to a new folder under Bunko's storage folder (`exports/`), named in the reply;
  `library disconnect --export DIR` copies it into a new or empty folder of your own.
  The same from a terminal or
  `docker exec`: `mokuro-bunko library connect gdrive|onedrive|mega|webdav` and
  `library disconnect`. `library connect` opens the sign-in page in the browser in a
  desktop session, and otherwise prints a code (Google Drive) or the sign-in address
  and asks for the address the browser ends on (OneDrive; `docker exec -it`);
  `--browser`, `--device` and `--paste` choose instead; `doctor` checks the pairing, the spool and the credentials
  file. The tray shows one library line ("Google Drive · synced 1 min ago") and
  "Sign in again…" when needed.
- **Folders and volumes whose names differ only in case are joined in a cloud
  library.** Google Drive, MEGA and many WebDAV servers can hold `Kingdom/` beside
  `kingdom/`; Bunko now moves the younger spelling's volumes into the oldest one by
  itself, once neither has changed for a while and no OCR runs on them, and puts the
  emptied folder in the provider's trash (never a permanent delete). A copy that is
  byte-for-byte the same goes to the trash; covers, layers and OCR files only the
  younger folder has move in under the older spelling; its `series.json` is merged into
  the series first. When both spellings hold different OCR for the same archive, the
  one matching the primary OCR setting becomes the main file and the other is always
  kept beside it as `<Volume>.pre-upgrade.mokuro` first, so no OCR or edit is lost.
  Two different archives (or OCR files Bunko cannot choose between)
  stay where they are, listed under the Storage tab's conflicts with **Keep this copy**
  / **Keep that copy**; the tab also lists the **Recent merges**. Upload owners, OCR
  records and volume ids follow every moved volume. A folder Mokuro Reader keeps
  recreating is left alone after the fourth merge in a day.

### Changed
- **A WebDAV library on this computer needs no "Allow http".** A plain `http://` URL
  whose host is `localhost`, `127.x.x.x` or `[::1]` (a server on the same machine, such
  as an rclone or Nextcloud container bound to loopback) is accepted without
  `allow_http` / `--allow-http`, in the wizard, the admin panel, `library connect` and
  the config file; any other `http://` URL still needs it.
- **Internal: the library goes through a storage seam.** Every library file access in
  the server, the WebDAV layer and the metadata compiler now passes through one
  interface (`bunko-store`), so the library can live somewhere other than the local disk
  in 0.8. Local libraries behave exactly as before.
- **Upgrades never skip a main OCR file for being edited; a file Bunko didn't write is
  kept as a "pre-upgrade" layer first.** With the generation upgrade on, an outdated
  `<Volume>.mokuro` in a family listed under `ocr.upgrade.replace` is upgraded even if a
  person or Mokuro Reader changed it. One Bunko can prove it wrote, byte for byte (it
  now records the sha256 of every OCR file it installs), is replaced as before. Any
  other (stock mokuro output, a file the reader saved corrections into, or one that only
  carries Bunko's `ocr_engine` stamp, which anyone who can write the file can add) is
  first copied, byte for byte, to
  `<Volume>.pre-upgrade.mokuro` (`pre-upgrade-2` and so on if that name holds other
  bytes), and the new file goes in only once that copy is safely written; if the copy
  cannot be written, the volume is left alone until the next pass. A file whose recipe
  cannot be read is still never upgraded. A layer is copied in as the new main file only
  when Bunko can prove it wrote that layer the same way and its own file carries the
  matching stamp, and layers without a stamp (a reader's
  edits, `pre-upgrade` copies) are never used or deleted. The admin page's "skipped
  (edited)" count, its list of edited volumes and the "Upgrade anyway" button are gone,
  as is the `force` option of `POST /api/ocr/upgrade/<volume>`.

### Fixed
- **Uploading through Bunko into a cloud library works.** Every WebDAV PUT into a
  Google Drive, OneDrive, MEGA or WebDAV library failed with a server error (500, "this
  store has no local file to open"), so neither Mokuro Reader nor an uploader could add
  a volume or an OCR file through Bunko. The body is now staged on Bunko's disk,
  checked (size, `Content-Digest`, the archive's CRCs) and uploaded before the answer,
  so a 201/204 means the cloud has the file; an uploaded volume is then OCR'd like any
  other. The client's `If-Match` / `If-None-Match: *` now travel with the upload, so a
  file changed or created in the cloud meanwhile is a 412 instead of being
  overwritten. A full cloud account answers 507, and an upload the library cannot take
  right now (see below) a 503 with `Retry-After`, never a 500.
- **A failed upload into a cloud library no longer leaves a staging file behind.** Its
  empty staging file and record survived restarts; a start now also removes any a crash
  left.
- **While Bunko is still claiming a cloud library, it says so.** For about 10 minutes
  after the first connect (on WebDAV; a minute or more elsewhere) Bunko waits to make
  sure no other Bunko manages the library before it writes. Uploads and other changes
  through Bunko in that time are refused with 503 and a `Retry-After` of the time left,
  and the message says this Bunko is still claiming the library and roughly how long is
  left, instead of "another Bunko manages this library". The admin Storage tab and
  status show the same.
- **Failed WebDAV requests are logged.** A request that ends in a server error (5xx),
  uploads included, is logged with its method, path, user and cause (503s as warnings,
  other failures as errors); before, a failed upload left no trace in the log.
- `library connect --create` no longer asks "Create it?" before creating the folder; it
  prints "Creating mokuro-reader/…".

## [0.7.3] - 2026-10-12

Background work can no longer make the web server stop answering.

### Fixed
- **A burst of background work hung every request.** Turning on the generation
  upgrade on a large library swapped in a matching layer for every volume that had
  one, from inside the census walk; each swap queued a recompile of its series, each
  recompile walked the whole library, and they all waited in the same blocking-thread
  queue every request uses to check its login, in arrival order. On a large library
  the backlog could grow faster than it drained, and the server answered nothing, a
  static 404 included, until it was restarted.
  Requests and background work are now kept apart:
  - **Requests check their login without a blocking thread when they can**:
    anonymous requests and recently verified Basic logins are decided at once; only
    a password or token check (bcrypt, the database) waits for one. The public OCR
    status (`/catalog/api/ocr-status`) reads what the scheduler publishes about once
    a second instead of asking it.
  - **One metadata thread compiles series**, batching every series that changed since
    its last pass into one library walk and one `catalog.json` write, at most one
    batch every 2 s (a fifth of the debounce). A burst of changes costs a few walks
    instead of one per series.
  - **Background work has a runtime of its own** (two threads and a bounded blocking
    pool): metadata timers, PROPFIND cache refreshes, thumbnails, the community
    fetcher, OCR control and update checks. PROPFIND refreshes also have their own
    generation slots, so they never hold the ones a reader's listing needs.
  - **The generation upgrade's walk only reads.** Volumes with a matching layer are
    recorded during the walk and swapped in afterwards by one job, one volume at a
    time (about 20 a second); the admin panel's upgrade census shows its progress
    (`direct`).
  - **The library index is walked by one reader at a time.** While a walk runs, other
    readers (`/api/health`, `/api/stats`) get the previous result at once instead of
    each walking the library or waiting for a walk a busy background job began. A
    change made during a walk no longer gets lost (the walk's result was marked
    current).

  Measured on 3,000 series × 3 volumes with every volume upgradable, under 30
  polling readers: 0.7.1 left static 404s, `/api/health` and PROPFIND unanswered for
  15 s at a time over the first two and a half minutes (71 threads); 0.7.3 answered
  all of them throughout, the slowest in 0.2 s (18 threads), while it upgraded the
  library.

## [0.7.2] - 2026-10-12

A hotfix for 0.7.1.

### Fixed
- **The tray now restarts into an updated program.** An automatic update restarted the
  server and processor into the new version but left the tray running the old one
  (its menu kept showing the old version). The tray now notices when its program file
  is replaced by a newer version, asks the instances it runs to pause after their
  current volume, stops them once idle, restarts itself and starts them again, which
  takes a few seconds. It never interrupts a volume being read or a download, and
  lifts only the pause it set. Trays from before 0.7.2 still need one restart by hand.
- **Vulkan crash detection on Windows and macOS.** A Vulkan runtime that crashed its
  process was demoted at the next start only on Linux; on Windows and macOS only after
  15 minutes, so a quick restart could crash on it again. The process check now works
  on every platform.

## [0.7.1] - 2026-10-12

GPU OCR for graphics cards no compiled libtorch package covers, through Vulkan.

### Added
- **The Vulkan route.** A second, download-on-demand backend pack (`vulkan`, about
  18 MB) carries two runtimes for hayai-nova and paddle-manga: **ncnn** (Vulkan, with
  its own matrix and attention kernels) and **WebGPU** (ONNX Runtime over Dawn: Vulkan
  on Linux, Direct3D 12 on Windows). Both keep the decode loop and KV cache on the GPU
  and read fp32 text identical to the libtorch fp32 reference. This brings GPU OCR to
  AMD cards without a ROCm package (Polaris RX 400/500, Vega, RX 5000 on Linux), AMD and
  Intel GPUs on Windows, and any GPU when asked for.
- **Every backend is a benchmark candidate.** With the defaults, each GPU's candidates
  are libtorch (where its pack covers the card), ncnn and WebGPU, with the CPU as the
  floor; the machine's benchmark measures them and the fastest is used per GPU and
  engine. `ocr.backend: vulkan` pins the Vulkan runtimes and `ocr.vulkan_backends`
  orders them.
- **Self-test before use.** A runtime reads 16 synthetic golden crops on each GPU and
  must return the reference text exactly; one that fails, does not load or does not
  fit the GPU's memory is skipped there with its reason, and the next one serves.
  Verdicts are cached and re-checked when the runtime, pack, driver, GPU or models
  change. A runtime that crashes is demoted on that GPU at the next start; one that
  fails mid-volume hands the same read to the next.
- **Exceptions where a runtime measurably offers nothing:** on NVIDIA GPUs the CUDA
  pack drives, ncnn and WebGPU are fallback-only (0.40–0.53x of CUDA): not benchmarked,
  and no Vulkan models are fetched unless CUDA fails. WebGPU is fallback-only for
  paddle-manga on AMD, behind ncnn.
- `install-ocr` installs the Vulkan pack beside the libtorch pack where a GPU can use
  it, including Windows machines with AMD or Intel graphics; `--variant vulkan`
  installs it alone. Its models are a separate release (`vulkan-models-v1`), fetched
  per runtime and engine as they are first needed.
- `doctor` checks "Vulkan runtimes" (loader, driver, each runtime × engine per GPU:
  candidate, fallback-only or skipped with the reason); admin → This server shows the
  same as "GPU runtimes".

### Known issues
- No Polaris (gfx803) card has run 0.7.1: the kernels assume no wavefront size and check
  themselves at load, and a runtime that fails its self-test is skipped, but speed and
  correctness there are unmeasured.
- paddle-manga needs about 4.6 GiB of free GPU memory on ncnn and 7 GB on WebGPU; on
  smaller cards it reads on the CPU (hayai-nova fits 4 GB cards).
- WebGPU drives one GPU per machine (the system's high-performance adapter); machines
  with several GPUs are untested on both runtimes.
- AMD and Intel GPUs on Windows have not been tested on real hardware.

## [0.7.0] - 2026-10-11

A rewrite in Rust, as a drop-in over 0.5.2 and 0.5.3 storage (same `config.yaml`,
`mokuro.db` and library tree, same reader-facing APIs) and at parity with 0.5.3's
features. See [docs/MIGRATING-0.7.md](docs/MIGRATING-0.7.md).

This section compares 0.7.0 with 0.5.3. The 0.7.0-beta entries below it list what
changed from one pre-release to the next; all of it is in 0.7.0.

### Added
- One native `mokuro-bunko` program: no Python, uv or venvs. With no arguments it
  starts the app: in a desktop session the tray, headless what is set up (`serve`,
  `processor serve` or both), or `setup` in a terminal when nothing is. On Windows
  `mokuro-bunko.exe` is the app (no console window) and `mokuro-bunko-cli.exe` the
  same program for terminals.
- Lite and full builds. Lite is the server only and runs in 1 GB of RAM (about
  16 MiB idle in Docker); full adds local OCR, the `processor` command and the
  desktop tray.
- OCR recognizers on libtorch, the same torch 0.5.2 used, from compiled model
  packages: `install-ocr` installs a *backend pack* for the hardware it finds —
  NVIDIA CUDA (Turing or newer, Linux and Windows), AMD ROCm (RX 6000/7000/9000,
  Linux) or the CPU (all platforms). The pack is checked against the signed
  `release.json`, and its `pack.json` pins every file, NVIDIA's CUDA libraries
  included; the model packages are checked against sha256 values built into the
  program.
- **The OCR backend installs in the background; serving never waits for it.** `serve`
  and `processor serve` start at once and, when local OCR is on and an enabled
  generation needs the libtorch backend that is missing, run the same detection and
  install as `install-ocr --if-needed` meanwhile, on any system. The progress is in the
  log, the admin panel, the dashboard, the tray and `/control/status`; the server's
  local OCR starts by itself when it is done, and a processor takes work once its
  backend is ready. A failed install is a problem with a Retry button, retried after
  5, 15 and 60 minutes. A new server installs nothing before its first-run setup;
  `MOKURO_OCR_AUTO_INSTALL=false` means never by itself (the missing backend is then a
  problem with an Install button). `install-ocr` stays a foreground command.
- Four downloads per release: `mokuro-bunko-<ver>-windows.zip` (x86_64, with a portable
  mode), `-macos.dmg` (Apple silicon, a drag-to-Applications disk image),
  `-linux-x64.tar.gz` (glibc 2.28+) and `-linux-arm64-server.tar.gz` (a static server;
  OCR from a processor), plus the Docker images `:latest` (local OCR; the backend for
  the container's GPU is downloaded once it serves) and `:latest-lite` (amd64 + arm64).
  The release notes start with a table linking each one. `:beta` / `:beta-lite` follow
  every release, beta or stable.
- `scripts/install.sh` and `scripts/install.ps1` install a release, checking the
  archive's sha256 against `release.json`. install.sh also checks its ed25519
  signature when OpenSSL 3 is available (without it, it warns, or fails with
  `--require-signature`); install.ps1 relies on HTTPS to GitHub. mokuro-bunko itself
  checks the signature when it updates.
- One-click updates: the admin panel's Updates card checks for new releases and,
  on self-managed installs (a release archive, the disk image, `install.ps1`, or
  `install.sh` run as a user), installs them (ed25519-signed `release.json`,
  sha256-checked archive). Elsewhere it reports the new release and says how to
  update (re-run install.sh, pull the image, or use the package manager). Also
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
  with the current primary generation's output. The old file is not kept (extra OCR
  beside the primary is what non-primary generations are for); a volume a person
  edited is skipped unless an admin forces it, and a short archive is never
  regenerated.
- **One web interface for a library server**, the same locally, from another
  computer and in Docker. Its first-run page (`/setup`) is the setup wizard: the
  admin account, who may join, remote access (Cloudflare tunnel, Dynamic DNS, HTTPS,
  other web apps) and OCR on this machine (on by default when a usable GPU is found;
  Auto, CPU or the GPU found). Answers the environment fixes are shown, not asked.
  Finishing signs the admin in and opens the admin panel, whose new **This server**
  tab covers the machine: OCR on or off, the hardware, the backend in use and the
  preference (`ocr.backend`), Install / Reinstall / Remove with live progress, the
  engines' models, diagnostics (`doctor`) and the server log. Installs come only from
  the signed release or the `ocr-offline` folder next to the program.
- First-run setup from another computer (Docker, a NAS): while no admin exists the
  server prints a one-time **setup code** in its log at every start (`First run:
  create the admin account at http://…/setup (setup code: XXXXX-XXXXX)`). `/setup`
  opened from another address asks for it (five tries per address, one more every
  12 s; a flood of wrong codes replaces it with a new one in the log rather than
  locking the owner out); scripts may send it as `X-Setup-Code`. Localhost needs no
  code. The code lives in memory only and dies once an admin exists. Every setup
  route answers only while no admin exists, and HTTPS certificate files can be named
  only from the server itself. 0.5 allowed setup from localhost only.
- The desktop app keeps what is local (`mokuro-bunko` with no arguments on a desktop,
  or `mokuro-bunko gui`): a chooser (**Library server**: pick the folder, the tray
  starts the server on a free port and the browser moves to its `/setup`;
  **Processor**: the pairing page: library address, login, connection test, name,
  OCR choices), then the processor's own status and settings pages. Every running
  server and processor serves these pages on 127.0.0.1 only, behind a per-run token.
  `docs/rust-port/GUI-COVERAGE.md` gives every command-line option its place (the
  admin panel, these pages or the tray) or says why it is CLI-only; a test checks it.
- The running version is shown at the foot of the admin panel, on the local
  processor pages and as the tray menu's first line.
- Pause OCR on a processor or on the server's own OCR: after the running volume,
  now, or until a time; the pause survives restarts. Pausing now hands the claimed
  volumes straight back to the queue without counting a failure, and the admin
  panel's processor list shows who is paused until when.
- A processor downloads ahead of work only the models of the engines its library
  runs: the library names them when the processor registers, and the processor fetches
  the models of an engine the library enables later at its next registration. Any
  engine's models are still fetched when a session first needs them.
- `install-ocr`, `doctor` and `models` take `--processor` to act on the
  processor's storage (picked automatically on a processor-only machine), and a
  processor also uses a backend pack installed for the library on the same machine.
- A tray icon for Windows, macOS and Linux (`mokuro-bunko tray`, and what
  `mokuro-bunko` with no arguments runs on a desktop): what the server or processor
  is doing (volume, pages, rate), statistics, pause after this volume / now / for an
  hour / until tomorrow, resume, Open admin panel, Open processor, Open library, and
  a Start at login checkbox. It starts the server or processor `tray.json` lists,
  restarts it if it crashes, and its Quit stops what it started (an instance it does
  not manage, a system service, keeps running). It is part of the full build (the
  app `mokuro-bunko.exe` on Windows, the `Mokuro Bunko` app on macOS, a `.desktop`
  entry on Linux, where it needs no GTK or AppIndicator library); the lite build and
  the Docker images have none.
- Opt-in automatic updates. `update.auto: true` (config, the admin panel's Updates
  card) installs a newer release on the channel by itself: new OCR work is
  held, the running volumes and uploads finish, the release is fetched and checked
  as one unit — the program, its OCR backend pack for the installed variant and its
  models — before anything is switched, then the server restarts (exec under the
  tray, a service or a terminal; exit 75 under the Windows tray or `run.bat`). If the
  new release's pack does not load after the switch, program and pack roll back
  together. A processor with `processor.auto_update: true` (`processor setup
  --auto-update`, the processor's pairing and settings pages) follows its library:
  when the library reports a newer version it finishes its running volume, installs
  exactly that release, restarts and reconnects; it never downgrades. Self-managed
  installs only: Docker, package-manager and root `install.sh` installs are told how
  to update instead (the admin panel shows that line in place of the checkbox, and a
  saved `update.auto: true` is ignored there); a processor there finishes its
  volumes, then its install is refused with a `fail` problem and retried later.
  `/control/status` has an `update` block,
  the tray shows "Updating to X…" / "Updated to X", and anything that needs the
  owner (a driver, disk space, a bad signature, a rollback, a library older than its
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
- Where the CPU is picked for paddle-manga, the processor's OCR pages and the admin
  panel's pools table say it is slow there (about 40 s a page on 16 threads) and that
  hayai-nova is the CPU engine of choice.
- OCR backends and models are downloaded on demand, after hardware detection and
  by the owner's preference, never baked into a release:
  - The full Docker image carries no backend pack and no model. Once it serves, the
    server (or `processor serve`) installs them in the background as on any system
    (above): it detects the GPU the container was given (NVIDIA through the container
    toolkit, AMD through `/dev/kfd` + `/dev/dri`, a host GPU not passed in does not
    count), applies `ocr.backend`, downloads the matching pack into `/data/backends`
    and the enabled engines' models into `/data/models`, and does nothing on later
    starts. A new GPU gets its pack (the replaced one is removed).
    `MOKURO_OCR_AUTO_INSTALL=false` turns it off; 0.5.2's `OCR_AUTO_INSTALL=false` is
    ignored, as it never stopped 0.5.2 from installing. The entrypoint installs
    nothing before the server, so it answers within seconds of the first start (the
    health check's start period is 30 s).
  - AMD GPUs work in Docker (`--device /dev/kfd --device /dev/dri`); the server keeps
    the device nodes' groups when it drops to `PUID:PGID`.
  - `:latest-cuda` / `:<ver>-cuda` is a second tag of the full image (the 0.7 CUDA
    image with its pack built in is gone). Templates written for the 0.5.2 CUDA
    image keep their volumes and variables but name the locally built
    `mokuro-bunko:unraid-cuda`: change their Repository to
    `ghcr.io/gnathonic/mokuro-bunko:latest` (or `:latest-cuda`).
    `BAKE_PACK=1` (`PACK_VARIANT=cpu|cu130|rocm7.1`, default `cpu`) builds a backend
    pack into a self-built image. The models are not included: a host without
    internet needs a populated `/data/models`.
  - `install-ocr` without `--variant` installs the pack `ocr.backend` asks for on
    this hardware: `cpu` stays on the CPU with a GPU present, `cuda`/`rocm` fall back
    to `cpu` with a hint when that GPU is not usable; `--variant auto` is the hardware
    alone. `doctor` judges the installed pack the same way. Model downloads follow
    the enabled generations: a library that only enables hayai-nova never fetches
    paddle-manga.
  - The macOS disk image does not bundle the OCR backend and models: the server
    (after its setup's OCR step) or the processor downloads them in the background,
    as on the other platforms (`make-dmg.sh --bundle-ocr <dir>` still builds an
    offline image).
- `ocr.backend` is `auto`, `cuda`, `rocm`, `cpu` or `skip`. Automatic precision
  picks bf16 only where the hardware runs it natively (NVIDIA Ampere and newer,
  AMD RDNA3/4); RDNA2 and the CPU run fp32. On RDNA2 (RX 6000) that is a change:
  0.5.2 ran hayai-nova there in emulated bf16 by default, which was slower.
- RX 6600/6700-class cards (gfx1031/1032) get `HSA_OVERRIDE_GFX_VERSION=10.3.0`
  automatically, as in 0.5.2.
- Old configs are migrated at load: a `mokuro` row is retired (kept, disabled),
  rows on removed detectors move to `ppocr-manga`, and a `hayai-nova` primary is
  added when none is left. Existing `.mokuro` files are kept and served.
- The licence is MPL-2.0 everywhere, including the image labels (0.5 images said MIT).
- nginx download offload is off by default. `MOKURO_NGINX_ACCEL=1` turns it on in
  the full image when the container starts as root (the default); the lite images
  have no nginx and ignore it. The in-image nginx passes WebSockets for `/_processor/`.
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
  `MOKURO_BUNKO_MOKURO_SPEC`, `MOKURO_PPOCR_THREADS` and `MOKURO_EFT_TRACE`.
- `processor install` and the install step of `processor setup`.
- Protocol v2: 0.5.2 processors cannot connect to a 0.7 library, nor the reverse.
- OCR on arm64 Linux (that platform gets the lite build; OCR from a remote x86_64
  processor).
- Intel Mac builds: there is no macOS x86_64 download, and install.sh fails there.
  The full Docker image (linux/amd64, with OCR) runs on them.

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

## [0.7.0-beta.6] - 2026-10-11

A pre-release of 0.7.0.

### Changed
- **A processor downloads only the models its library uses.** The library names the
  engines of its enabled generations when a processor registers (`RegisterReply.engines`);
  the processor keeps the list (`library-engines.json` in its storage) and its install,
  `models download` and automatic updates fetch those engines' models, not every
  engine's (paddle-manga alone is 3.4 GB). When the library enables another engine, the
  processor fetches its models in the background at its next registration. Before the
  first registration, or with an older library, it fetches PP-OCR's only; any engine's
  models are still fetched when a session first needs them.

### Fixed
- **Big model files download on slow links.** A download had 120 seconds for the
  whole file, so the 1.8 GB paddle-manga weights failed below about 10 MB/s and the
  install (or automatic update) failed with them. Each request now gets 120 seconds and
  the next one asks for the rest (`Range`), so the file finishes at any speed; a
  download fails only when four requests in a row add nothing.
- **An OCR install whose models failed finishes on its retry.** The pack was in place,
  so the automatic retry found nothing to do and the missing models stayed missing.
  The install now leaves `backends/.models-incomplete` when its models do not all
  download, and `install-ocr --if-needed` (the background installer's retry) fetches
  them while it is there.
- **A processor tells the library what it can run after its install changes.** It
  registered again only when an install it had registered beside finished. An install
  started later (from the dashboard, or a retry) or a failure that cleared now
  registers it again too, so engines whose models arrived are offered work.

## [0.7.0-beta.5] - 2026-10-10

A pre-release of 0.7.0.

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
- **"Install updates automatically" is honest in Docker and package-managed
  installs.** They cannot update themselves, so the admin panel shows one line instead
  of the checkbox ("Runs in Docker: update by pulling the new image", with the image
  when known; a managed install names its manager), `GET /_admin/api/update` says
  `auto_supported: false` (and `manual_update`), a saved `update.auto: true` is ignored
  without an error, and a newer release is reported as available, not as a failure
  (no problem, no tray warning).
- **Big WebDAV listings arrive whole.** At some listing sizes a `PROPFIND` of a large
  folder or of the whole library lost up to 4 KiB at its end: the plain answer came out
  shorter than its Content-Length and the gzip one did not decode. Rare (a few listings
  in a thousand), but the same listing compresses the same way every time, so a client
  kept getting the broken one until the folder changed.
- **A processor installing its OCR backend says so, like a server.** Its dashboard
  showed "Idle — waiting for volumes" and the problem "Run 'mokuro-bunko install-ocr
  --processor'" for up to two minutes while the background install was already
  running (its problems check ran before the install started). Now no such problem
  shows while the install runs, and a server or processor with nothing to read until
  its backend is in place reads **Installing OCR backend** (`/control/status` `state:
  "installing"`; the dashboard's state, the tray's status line with the percent).
- **AMD GPUs: no crash when the temporary folder is unusable.** When `TMPDIR` (or
  `TMP`/`TEMP`, or `/tmp` with none set) names a folder that is missing or read-only,
  the ROCm runtime cannot build its copy kernels and crashed the process (SIGSEGV) at
  its first GPU copy -- the first-run benchmark took the server down at every start.
  The backend now checks that folder before the ROCm runtime starts and, when it is
  unusable, points `TMPDIR` at `/tmp`, `/var/tmp` or a `.tmp` folder beside the backend
  pack, with a warning in the log and `doctor`. With no usable folder at all the GPU
  stays off (OCR on the CPU) and the log says why.

## [0.7.0-beta.4] - 2026-10-09

A pre-release of 0.7.0.

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

A pre-release of 0.7.0.

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

A pre-release of 0.7.0: an
install of 0.7.0-beta.1 finds it on the pre-release channel and can update to
it. Fixes `models verify` after `install-ocr` (see 0.7.0, Fixed).

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

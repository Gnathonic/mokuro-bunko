# Mokuro Bunko

A self-hosted manga library server with WebDAV, built-in OCR processing, and multi-user support. Designed as a backend for [Mokuro Reader](https://reader.mokuro.app).

> [!NOTE]
> **0.7 is a rewrite in Rust.** It replaces the Python 0.5 line as a drop-in: the same `config.yaml`, database and library tree, the same reader-facing APIs. One native binary, no Python, OCR on the same libtorch 0.5.2 used (installed as a backend pack by `mokuro-bunko install-ocr`, for NVIDIA CUDA, AMD ROCm or the CPU) with Apache-2.0 models only, release packages for Linux, Windows, macOS and Docker, and one-click updates. Coming from 0.5.2? Read [docs/MIGRATING-0.7.md](docs/MIGRATING-0.7.md).

## What it does

- Serves a shared manga library over WebDAV so Mokuro Reader can connect directly
- Tracks per-user reading progress (each user gets their own progress files transparently)
- Compiles the reader's metadata files (`series.json`, `catalog.json`) server-side from the library itself, merging in client-submitted series facts (titles, links, tags), and recompiles within seconds of an upload
- Runs OCR automatically on uploaded manga with [hayai-nova](https://huggingface.co/JustANormalTinkerer/hayai-ocr-v2.5-nova) by default, plus optional extra OCR layers from PaddleOCR-VL (`paddle-manga`) and PP-OCRv6 (`ppocr-manga`, which also reads scanned novels)
- Can hand OCR to a stronger computer: a remote processor logs in to the library and works the queue, and each volume goes to whichever machine will finish it first
- Manages users with role-based permissions (anonymous browse, registered, uploader, editor, inviter, admin)
- Provides a web catalog for browsing the library (display-title language options, sorting, genre filters, ratings and tags fetched from AniList/MAL for linked series, no API key required) plus an admin panel for user, config and OCR management
- Checks for new releases and installs them from the admin panel in one click (signed releases)
- Asks search engines and crawlers to stay out (`X-Robots-Tag: noindex` on every response, deny-all `robots.txt`)

See [CHANGELOG.md](CHANGELOG.md) for release history.

## Download

**[Latest release](https://github.com/Gnathonic/mokuro-bunko/releases/latest)**: the table at the top of its notes links the file for your system. Betas are pre-releases, on the [Releases page](https://github.com/Gnathonic/mokuro-bunko/releases) only (`releases/latest` skips them).

Or in one line:

```bash
# Linux (x64 or arm64; checks the release signature)
curl -fsSL https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.sh | sh
```
```powershell
# Windows
powershell -c "irm https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.ps1 | iex"
```
```bash
# Docker (`:latest-lite` for the server only)
docker pull ghcr.io/gnathonic/mokuro-bunko:latest
```

These take the latest stable release; for a beta, use the script of its tag with `--version` (`-Version`), e.g. `curl -fsSL https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/v0.7.0-beta.3/scripts/install.sh | sh -s -- --version 0.7.0-beta.3`, or the image tag `:0.7.0-beta.3`.

## Editions

Every release has two builds of the same `mokuro-bunko` executable:

| Edition | Contains | Use it for |
|---|---|---|
| **full** | The server, WebDAV, catalog, admin panel, local OCR, the `processor` command and the desktop tray. | Windows, macOS (Apple silicon), Linux x64, the Docker `:latest` image. |
| **lite** (server) | The server without OCR engines: OCR comes from **remote processors**. A static binary; idles around 16 MiB of RAM in Docker. | Linux arm64 (Raspberry Pi, NAS), the Docker `:latest-lite` image, a 1 GB VPS. |

The full build runs on any GPU: the OCR recognizers run on **libtorch**, which `mokuro-bunko install-ocr` (or the setup wizard) downloads as a *backend pack* for the hardware it finds — CUDA for NVIDIA GPUs (Linux, Windows; driver 580 or newer), ROCm for AMD Radeon RX 6000/7000/9000 (Linux), or the CPU (all platforms).

## Quick start

Run `mokuro-bunko`. On a desktop it starts the app: a tray icon, and the first time the setup wizard in your browser. Headless (a server, SSH) it runs what is set up, or the terminal setup when nothing is. `mokuro-bunko --help` lists the commands. On Windows, use `mokuro-bunko-cli` in a terminal.

The first browser visit to a new server (`http://localhost:8080`) walks you through creating the admin account; `mokuro-bunko setup` does the same in the console. From a browser on another machine (Docker, a NAS) the setup page asks for the one-time **setup code** the server prints in its log at startup (`First run: create the admin account at http://…/setup (setup code: XXXXX-XXXXX)`). Or let Docker create the admin on first start with `MOKURO_ADMIN_USERNAME` and `MOKURO_ADMIN_PASSWORD` (or `MOKURO_ADMIN_PASSWORD_FILE`).

### Linux

`install.sh` downloads the release for your CPU, checks the signed manifest and the archive's sha256, installs into `~/.local/lib/mokuro-bunko` (`/usr/local/lib/mokuro-bunko` as root), links `mokuro-bunko` into `~/.local/bin` and adds the tray to the applications menu. Options: `--version X.Y.Z`, `--systemd` (a systemd unit; a user unit, or a system unit when run as root), `--processor` (the OCR processor unit), `--autostart` (the tray at login), `--prefix DIR`, `--dry-run`. The x64 build needs glibc 2.28 or newer (Debian 11+, Ubuntu 20.04+, RHEL/Alma 8+). Or unpack the archive yourself and run `./mokuro-bunko serve`, or `./mokuro-bunko tray` for the tray icon.

### Windows

Unzip the release anywhere and double-click **`mokuro-bunko.exe`**: a tray icon appears by the clock and the setup wizard opens in your browser the first time. In a terminal use `mokuro-bunko-cli`; `run.bat` runs the server in a console window instead, `doctor.bat` diagnoses problems. Config, library, logs and OCR models stay in a `data\` folder next to it (portable mode). `install.ps1` installs into `%LOCALAPPDATA%\mokuro-bunko\app` instead, with Start-menu shortcuts (`-Startup` for the tray at logon; parameters at the top of [`scripts/install.ps1`](scripts/install.ps1)), and keeps your data in `%LOCALAPPDATA%\mokuro-bunko`. For an NVIDIA GPU see [docs/setup-windows-nvidia-ocr.md](docs/setup-windows-nvidia-ocr.md).

### macOS

Open the disk image and drag **Mokuro Bunko** to Applications (Apple silicon). Opening it puts an icon in the menu bar and starts the setup wizard the first time. The app is not notarized: if macOS says it cannot be opened, right-click it and choose Open once. The command line is `/Applications/Mokuro Bunko.app/Contents/MacOS/mokuro-bunko`. Intel Macs have no build: use the Docker image.

### Docker

Images are published at `ghcr.io/gnathonic/mokuro-bunko`:

| Tag | What | Replaces (0.5.2) |
|---|---|---|
| `latest` | Server + local OCR + optional nginx for fast downloads. linux/amd64. No OCR backend inside: on first start it detects the GPU the container was given (none, NVIDIA or AMD), follows `ocr.backend` and downloads the matching backend pack and the models of the enabled OCR engines into `/data`. | `deploy/Dockerfile` and `deploy/Dockerfile.unraid` (`unraid-cuda`) |
| `latest-cuda` | The same image as `latest` under the name the 0.5.2 CUDA image used, so existing templates keep working. | `unraid-cuda` |
| `latest-lite` | Server only, distroless, 29 MB, amd64 + arm64 (the only image for arm64 hosts). OCR by remote processors. | new |

Each tag also has a versioned form (`0.7.0`, `0.7.0-cuda`, `0.7.0-lite`); pin one for production.

**Run it** (everything in one volume; OCR on the CPU):

```bash
docker run -d --name mokuro-bunko -p 8080:8080 -v mokuro-data:/data \
  -e PUID=1000 -e PGID=1000 ghcr.io/gnathonic/mokuro-bunko:latest
docker logs mokuro-bunko      # the OCR backend download, then: First run: … /setup (setup code: XXXXX-XXXXX)
```

The first start downloads the OCR backend before the server listens: ~100 MB for the CPU, ~2 GB for NVIDIA (~350 MB from the release, the rest NVIDIA's CUDA libraries from PyPI), ~3 GB for AMD; then the models of the enabled engines. Later starts reuse them and download nothing. Starting the container with a different GPU (or setting `MOKURO_OCR_BACKEND` to a GPU it now sees) installs that GPU's backend on the next start.

**NVIDIA GPU**: the host needs an NVIDIA driver **580 or newer** (CUDA 13), the [NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html) and a Turing (GTX 16xx / RTX 20xx) or newer GPU. Check with `docker run --rm --gpus all ghcr.io/gnathonic/mokuro-bunko:latest install-ocr --list` (it prints the driver and GPU it sees, and the pack it would install).

```bash
docker run -d --name mokuro-bunko --gpus all -p 8080:8080 \
  -v /srv/mokuro/data:/data ghcr.io/gnathonic/mokuro-bunko:latest
```

**AMD GPU** (Linux; Radeon RX 6000, 7000, 9000): pass the ROCm devices in. No ROCm on the host besides the kernel driver, and no special image.

```bash
docker run -d --name mokuro-bunko --device /dev/kfd --device /dev/dri -p 8080:8080 \
  -v /srv/mokuro/data:/data ghcr.io/gnathonic/mokuro-bunko:latest
```

The server keeps the groups that own those devices when it switches to `PUID:PGID`; with `docker run --user`, add them yourself (`--group-add`).

**Compose**: [`deploy/docker-compose.yml`](deploy/docker-compose.yml) (CPU, with commented NVIDIA and AMD lines), [`deploy/docker-compose.unraid-cuda.yml`](deploy/docker-compose.unraid-cuda.yml) (NVIDIA, `gpus: all`), [`deploy/docker-compose.lite.yml`](deploy/docker-compose.lite.yml), [`deploy/docker-compose.processor.yml`](deploy/docker-compose.processor.yml) (a GPU processor for a library elsewhere): `docker compose -f deploy/docker-compose.yml up -d`.

**Unraid**: add the template [`deploy/unraid/mokuro-bunko.xml`](deploy/unraid/mokuro-bunko.xml) (`--runtime=nvidia` for an NVIDIA GPU with the Nvidia-Driver plugin, driver ≥ 580; for AMD replace it with `--device=/dev/kfd --device=/dev/dri`; PUID 99 / PGID 100, `/data` and `/config` under `/mnt/user/appdata/mokuro-bunko/`) or [`mokuro-bunko-lite.xml`](deploy/unraid/mokuro-bunko-lite.xml).

**Volumes and variables**:

| Path / variable | Purpose |
|---|---|
| `/data` | Library, database, logs, the OCR backend (`/data/backends`) and the OCR models (`/data/models`: 0.3–0.6 GB for hayai-nova depending on the device and precision, a few GB more with paddle-manga). Keep it on a persistent volume. |
| `/config` | Optional: with `MOKURO_CONFIG=/config/config.yaml` (as the Unraid template sets, like the 0.5.2 CUDA image) the config lives there; by default it is `/data/config.yaml`. |
| `PUID`, `PGID`, `UMASK`, `TAKE_OWNERSHIP` | The user the server runs as (1000:1000 by default; the Unraid template passes 99:100), file mode mask, recursive chown on start. |
| `MOKURO_NGINX_ACCEL=1` | Put the bundled nginx in front for library downloads (as in 0.5). |
| `MOKURO_OCR_BACKEND` | `auto` (default: the GPU the container sees), `cuda`, `rocm`, `cpu` (stay on the CPU even with a GPU), or `skip` (no OCR here; remote processors only, nothing downloaded). Decides which backend the first start downloads. |
| `MOKURO_OCR_AUTO_INSTALL` | `true` (default): download the OCR backend on start when it is missing. `false`: no OCR backend until you run `docker exec mokuro-bunko mokuro-bunko install-ocr` (and restart). |
| `MOKURO_*` | Any config key (`MOKURO_<SECTION>_<KEY>`), see [docs/configuration.md](docs/configuration.md). |

The compiled models are unpacked once into the data volume (`/data/models`) and loaded from there. `docker exec mokuro-bunko mokuro-bunko doctor` checks the setup, including the OCR backend.

**Upgrading from the 0.5.2 images**: keep your volumes and variables and change the image to `:latest` (`deploy/Dockerfile` users and the Unraid `unraid-cuda` image alike; `:latest-cuda` is the same image). In the Unraid template, edit *Repository*. Then:

- The Python OCR environments in the data volume (`.ocr-env`, `.ocr-engines-env`, `.pip-cache`, `.tmp`) are no longer used and can be deleted; the volume now holds the OCR backend pack (`/data/backends`) and the models (`/data/models`).
- `OCR_AUTO_INSTALL` is no longer needed: the backend is installed on start by default. `true` keeps that on; `false` (the 0.5.2 template's default) is ignored, as in 0.5.2 it never stopped the install. To opt out, set `MOKURO_OCR_AUTO_INSTALL=false`. `MOKURO_BUNKO_OCR_ENV` / `_ENGINES_ENV` are ignored.
- `MOKURO_NGINX_ACCEL` now defaults to off; set it to `1` to keep nginx in front (useful when nginx serves downloads).
- The health check is `mokuro-bunko healthcheck` (no curl or Python in the image).
- 0.5.2 processors cannot talk to a 0.7 library: update them too (see the [migration guide](docs/MIGRATING-0.7.md)).

Systemd units and reverse-proxy examples are in [`deploy/`](deploy/) and described in [docs/deployment.md](docs/deployment.md).

### From source

See [Building from source](#building-from-source).

**Something not working?** Run `mokuro-bunko doctor`: it checks your build, config, the OCR backend pack and models, free disk space and port, with fix hints. See [docs/troubleshooting.md](docs/troubleshooting.md).

## Desktop app

`mokuro-bunko gui` opens the setup and settings pages in your browser. The tray icon (`mokuro-bunko` on a desktop, or `mokuro-bunko tray`) opens them too, shows what the server or processor is doing, pauses OCR and can start everything at login. On Linux it shows in any panel with a system tray (GNOME needs the AppIndicator extension); it needs no GTK or AppIndicator library. The pages are served on this machine only (`127.0.0.1`, with a one-time sign-in link), and they cover:

- **Setup**: a library server (folder, admin account, registration, remote access, HTTPS), a processor for another library (with a connection test), the OCR backend install with live progress, and starting either one with the machine.
- **Settings**: server, HTTPS, remote access, processor, OCR and models, start-up, logs, diagnostics (`doctor`), updates, and any `config.yaml` key. Users, invites and the library's own settings stay in the server's admin panel, which the app links to.
- **Dashboard**: what this machine is reading now, today's totals, problems, and pause / resume (after this volume, now, for an hour, until morning).

A running `serve` or `processor serve` serves the same pages on its loopback control port, which is what the tray icon opens. Windows 11 puts a new tray icon in the hidden-icons overflow (the **^** by the clock) at first: drag it onto the taskbar, or turn **Mokuro Bunko** on under Settings → Personalization → Taskbar → Other system tray icons. Everything the app does is also a CLI command; [docs/rust-port/GUI-COVERAGE.md](docs/rust-port/GUI-COVERAGE.md) maps each command and flag to its page.

## Configuration

`mokuro-bunko setup` (or the first visit in a browser) creates an admin account and writes a config file. After that, edit `config.yaml` directly, use `mokuro-bunko config set <section.key> <value>`, or use the admin panel at `/_admin`.

Copy [`config.example.yaml`](config.example.yaml) for a documented starting point. Key settings:

| Setting | Default | Description |
|---------|---------|-------------|
| `server.port` | `8080` | Listen port |
| `server.threads` | `0` | Async worker threads (`0` = `min(cores, 4)`) |
| `server.cache_mb` | `32` | Memory budget of the in-memory caches |
| `storage.base_path` | `~/.local/share/mokuro-bunko` | Library and database location |
| `registration.mode` | `self` | `disabled`, `self`, `invite`, or `approval` |
| `ocr.backend` | `auto` | `auto`, `cuda`, `rocm`, `cpu`, or `skip` (no OCR on this machine) |
| `ocr.generations` | one `hayai-nova` row | The OCR recipes every volume gets a layer from, in run order |
| `ocr.local_processing` | `true` | `false` leaves all OCR to remote processors |
| `update.check` | `true` | Look for new releases in the background |
| `catalog.enabled` | `false` | Web-based library browser |

Every key can also be set from the environment (`MOKURO_<SECTION>_<KEY>`, e.g. `MOKURO_OCR_BACKEND`), plus the shortcuts `MOKURO_HOST`, `MOKURO_PORT`, `MOKURO_STORAGE` and `MOKURO_CONFIG`. The full reference is [docs/configuration.md](docs/configuration.md).

## OCR

The recognizers (hayai-nova, paddle-manga) run on [libtorch](https://pytorch.org/) — the same torch 0.5.2 used, without Python — and the PP-OCR stages on [ONNX Runtime](https://onnxruntime.ai/) on the CPU. All models are Apache-2.0:

| Engine | What it is |
|---|---|
| `hayai-nova` (default) | [hayai-ocr v2.5 Nova](https://huggingface.co/JustANormalTinkerer/hayai-ocr-v2.5-nova): reads each text line, strong on display lettering and sound effects. |
| `paddle-manga` | [PaddleOCR-VL 1.6](https://huggingface.co/PaddlePaddle/PaddleOCR-VL-1.6) with a [manga LoRA](https://huggingface.co/sorryhyun/paddleocr-vl-1.6-manga-lora): the most accurate and the slowest, about 2 GB of model files (3.6 GB in fp32). It runs on the CPU too, at about 40 s a page on 16 threads: there hayai-nova is the engine of choice. |
| `ppocr-manga` | [PP-OCRv6 manga](https://huggingface.co/Kellenok/PP-OCRv6_manga) line detector and CTC recognizer on the CPU: small, reads scanned novel pages too. Its detector also feeds the other two. |

The 0.5 `mokuro` engine and the GPL `ctd`, `animetext` and `rtdetr` detectors are gone; see the [migration guide](docs/MIGRATING-0.7.md) for what happens to old configs and existing `.mokuro` files.

**Install the OCR backend once** on a full build: `mokuro-bunko install-ocr`. It looks at the hardware and installs the matching *backend pack* into `<storage>/backends/`:

| Pack | For | Download | On disk |
|---|---|---|---|
| `cu130` | NVIDIA GPUs, Turing or newer, driver ≥ 580 (Linux, Windows) | ~350 MB from the release + ~1.6 GB of NVIDIA CUDA libraries from NVIDIA's packages on PyPI | 2.5 GB |
| `rocm7.1` | AMD Radeon RX 6000 (gfx1030; RX 6600/6700 too: `HSA_OVERRIDE_GFX_VERSION=10.3.0` is set automatically), RX 7000, RX 9000 (Linux) | ~3 GB | 6.8 GB |
| `cpu` | everything else (Linux x86_64, Windows, macOS on Apple silicon) | ~75 MB | 0.4 GB |

On the CPU, OCR runs in fp32 like 0.5.2 (bf16 is an opt-in on CPUs with AVX512-BF16). Every file is checked against the signed release manifest. `install-ocr` picks the pack `ocr.backend` asks for on this hardware (`auto`: the GPU it finds; `cpu`: the CPU even with a GPU; `cuda`/`rocm`: that GPU, or the CPU with a hint when there is none). `install-ocr --list` shows what it detected, `--variant` picks a pack by hand, `--from <dir>` installs from downloaded files (air-gapped hosts). The Docker image downloads its pack on first start in the same way. ROCm needs a few system libraries (`libnuma`, including the unversioned `libnuma.so`; Debian/Ubuntu: `libnuma-dev`, Arch: `numactl`) — `install-ocr` and `doctor` name what is missing.

**Models** (the compiled hayai-nova / paddle-manga packages for your GPU, and the PP-OCR files) are downloaded on first use into `<storage>/models/` and verified by sha256, so the first OCR run needs internet access and some disk (about 0.5 GB for hayai-nova, a few GB for paddle-manga). `install-ocr` fetches them up front; `mokuro-bunko models list|download|verify` manages them; `MOKURO_MODELS_DIR` points at a directory of files for air-gapped hosts.

The server scans the library in the background and OCRs every volume that is missing a sidecar. Results (`.mokuro` overlay files and `.webp` thumbnails) are placed alongside the source volumes. What runs is a list of **OCR generations** (`ocr.generations`, editable in the admin panel): each is a named recipe (engine, text detector, reading resolution, and how it uses the hardware) that writes its own layer, `<Volume>.mokuro` for the primary one and `<Volume>.<name>.mokuro` for the others, in the order you list them. Each row can be benchmarked and tuned on your own pages from the admin panel. Existing OCR from older versions is kept and can optionally be replaced (`ocr.upgrade`, off by default).

Failures are visible: volumes that fail OCR appear on the Queue page (`/queue`) with a reason and retry count, and the server log is `<storage>/logs/server.log`. Failed volumes are retried with exponential backoff. How OCR scheduling works is described in [docs/ocr-internals.md](docs/ocr-internals.md).

### Remote processors

OCR can run on another machine, which is how a lite server on a 1 GB VPS gets its text. Create an account with the `processor` role on the library, then on the machine with the GPU install a **full** build (same version as the library) and run the setup:

```bash
mokuro-bunko admin add-user gpu-box --role processor   # on the library; asks for the password
mokuro-bunko processor setup                           # on the GPU machine: checks the account, writes processor.yaml, starts it
```

`processor setup` also offers to start the processor with the machine (a systemd user unit on Linux, a launchd agent on macOS, a Startup entry on Windows). The processor dials out to the library over HTTPS and one WebSocket, so nothing has to be opened on it and it works behind NAT. Behind a reverse proxy the `/_processor/` paths must pass WebSockets and unbuffered uploads: see the [deployment walkthrough](docs/deployment.md#remote-ocr-processors) and [remote OCR processors](docs/configuration.md#remote-ocr-processors). A Docker processor is in [`deploy/docker-compose.processor.yml`](deploy/docker-compose.processor.yml). Libraries and processors must both be 0.7 (protocol v3).

## Updates

The server checks `releases/latest` for a signed `release.json` every 12 hours (`update.check`, `update.channel`) and shows the result in the admin panel's **Updates** card. What the **Update and restart** button does depends on how it was installed:

| Install | Behaviour |
|---|---|
| Tarball, `install.sh` as a user, Windows zip or `install.ps1` | One click: downloads the archive, checks the ed25519 signature of the manifest and the sha256, swaps the executable and restarts. |
| `install.sh` as root (system unit), Docker, distro packages | Notice only: re-run `install.sh`, or pull the new image. |

From a terminal: `mokuro-bunko update check` and `mokuro-bunko update apply [--yes] [--restart]`. A processor machine updates with the same commands. Turn the background check off with `update.check: false`.

## User roles

| Role | Browse | Download | Upload | Edit/Delete | Invite | Admin |
|------|--------|----------|--------|-------------|--------|-------|
| Anonymous | configurable | configurable | -- | -- | -- | -- |
| Registered | yes | yes | -- | -- | -- | -- |
| Uploader | yes | yes | yes | own uploads | -- | -- |
| Editor | yes | yes | yes | all | -- | -- |
| Inviter | yes | yes | yes | all | yes | -- |
| Admin | yes | yes | yes | all | yes | yes |

Roles are a strict hierarchy: Admin > Inviter > Editor > Uploader > Registered > Anonymous. Each role inherits all capabilities of the roles below it.

A separate `processor` role is for [remote OCR machines](docs/configuration.md#remote-ocr-processors), not people: it can read the library and run OCR for it, nothing else. Only an admin can grant it.

## CLI reference

```
mokuro-bunko serve          # start the server (--host, --port, --ocr, --generations)
mokuro-bunko setup          # first-time setup wizard
mokuro-bunko doctor         # diagnose install/OCR problems (PASS/WARN/FAIL + hints)
mokuro-bunko admin          # users and invites: add-user, delete-user, list-users, change-role,
                            #   set-password, disable-user, approve-user, restore-user,
                            #   generate-invite, list-invites, delete-invite
mokuro-bunko config         # show, set, path, init, cors-add, cors-remove
mokuro-bunko ssl            # enable, disable, status, generate
mokuro-bunko tunnel         # status, cloudflare (a Cloudflare quick tunnel)
mokuro-bunko dyndns         # setup, status, update, enable, disable
mokuro-bunko update         # check, apply [--yes] [--restart]
mokuro-bunko models         # list, download [--engine E], verify   (full build)
mokuro-bunko processor      # serve, setup, status, service         (full build)
mokuro-bunko healthcheck    # probe /api/health, for container HEALTHCHECKs
mokuro-bunko install-ocr    # install the OCR backend pack for this GPU/CPU + the models (full build)
mokuro-bunko gui            # the desktop app: setup, settings and dashboard in the browser
```

Global options: `-c, --config <PATH>` (or `MOKURO_CONFIG`), `-v, --verbose`, `--version` (prints the version, build flavor and target). `mokuro-bunko <command> --help` has the details.

## Building from source

Requires a recent stable [Rust toolchain](https://rustup.rs/). The frontends under `web/` are embedded at build time.

```bash
git clone https://github.com/Gnathonic/mokuro-bunko.git
cd mokuro-bunko
cargo build --release -p mokuro-bunko                           # full build
cargo build --release -p mokuro-bunko --no-default-features     # lite: server only
cargo run -p xtask -- torch-pack --variant cpu --out dist       # an OCR backend pack (cpu|cu130|rocm7.1)
mokuro-bunko install-ocr --from dist                            # install it
```

Cargo features of `mokuro-bunko`: `ocr` (on by default: ONNX Runtime on the CPU, the libtorch pack loader, local OCR and the `processor` command). The ONNX Runtime binaries are fetched at build time; libtorch only by `xtask torch-pack` (the binary never links it). The ONNX Runtime GPU execution providers `cuda`, `webgpu`, `directml`, `coreml` still build but are not released. Release archives, signing and Docker images are built with `cargo run -p xtask -- <command>` (see [docs/rust-port/PACKAGING.md](docs/rust-port/PACKAGING.md)); the design is in [docs/rust-port/ARCHITECTURE.md](docs/rust-port/ARCHITECTURE.md).

```bash
cargo test --workspace      # run tests
cargo clippy --workspace    # lints
```

## License

[Mozilla Public License 2.0](LICENSE). Every release archive carries a `THIRD-PARTY-LICENSES.md` listing the licences of all bundled dependencies and native components (generated at build time; a copyleft licence with no permissive alternative fails the build). The OCR models are Apache-2.0 and are credited in the release notes, as are the datasets their model cards name.

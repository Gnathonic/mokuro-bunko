# Mokuro Bunko

A self-hosted manga library server with WebDAV, built-in OCR processing, and multi-user support. Designed as a backend for [Mokuro Reader](https://reader.mokuro.app).

> [!NOTE]
> **0.7 is a rewrite in Rust.** It replaces the Python 0.5 line as a drop-in: the same `config.yaml`, database and library tree, the same reader-facing APIs. One native binary, no Python or torch, OCR on ONNX Runtime with Apache-2.0 models only, release packages for Linux, Windows, macOS, Docker and Android, and one-click updates. Coming from 0.5.2? Read [docs/MIGRATING-0.7.md](docs/MIGRATING-0.7.md).

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

## Editions

Every release has two builds of the same `mokuro-bunko` executable:

| Edition | Contains | Use it for |
|---|---|---|
| **lite** | Server, WebDAV, catalog, admin panel, OCR scheduler for remote processors, updater. No OCR engines. A static binary on Linux; idles around 16 MiB of RAM in Docker. | A 1 GB VPS, NAS or Raspberry Pi that serves the library while OCR runs on **remote processors**. |
| **full** | Lite plus ONNX Runtime, the OCR engines, local OCR and the `processor` command. | A desktop or GPU box: run the whole library locally, or act as a processor for a lite server elsewhere. |

The full build comes in variants by GPU support: DirectML on Windows, CoreML on macOS, CPU on Linux, and a **CUDA** variant (`full-cuda`) for NVIDIA GPUs on Linux and Windows. In a lite build `ocr.local_processing` is off and the admin panel says so.

## Quick start

Releases are on the [GitHub releases page](https://github.com/Gnathonic/mokuro-bunko/releases). The first browser visit to a new server (`http://localhost:8080`) walks you through creating the admin account; `mokuro-bunko setup` does the same in the console. From a browser on another machine (or through Docker's network) the setup page needs the one-time token the server prints in its log at startup (`/setup?token=...`, also in `<storage>/.setup-token`).

### Linux (and macOS): install script

```bash
curl -fsSL https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.sh | sh
# or, with options:
curl -fsSL https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.sh | sh -s -- --flavor lite --systemd
```

It downloads the release for your OS and CPU, checks the signed manifest and the archive's sha256, installs into `~/.local/lib/mokuro-bunko` (`/usr/local/lib/mokuro-bunko` as root) and links `mokuro-bunko` into `~/.local/bin`. Options: `--flavor lite|full|full-cuda`, `--version X.Y.Z`, `--systemd` (a systemd unit; a user unit, or a system unit when run as root), `--processor` (the OCR processor unit, needs a full flavor), `--prefix DIR`, `--dry-run`. The full Linux build needs glibc 2.35 or newer (Debian 12, Ubuntu 22.04); the lite build is static and runs anywhere. The `full-cuda` build needs an NVIDIA driver 580 or newer plus CUDA 13 and cuDNN 9 libraries.

Or unpack a `mokuro-bunko-<version>-<target>-<flavor>.tar.gz` yourself and run `./mokuro-bunko serve`.

### Windows

Install with PowerShell (no admin rights, no prerequisites):

```powershell
powershell -c "irm https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.ps1 | iex"
```

It installs the **full** build (OCR on the GPU through DirectML, or the CPU), adds Start-menu shortcuts, runs `doctor` and starts the server. Parameters (`-Flavor full-cuda|lite`, `-Portable`, `-Startup`, `-Version`, ...) are documented at the top of [`scripts/install.ps1`](scripts/install.ps1). Your data stays in `%LOCALAPPDATA%\mokuro-bunko`.

**Portable zip:** download `mokuro-bunko-<version>-x86_64-pc-windows-msvc-<flavor>.zip` from the release, extract it anywhere and double-click `run.bat`. Config, library, logs and OCR models stay in a `data\` folder next to it; move it by copying the folder, uninstall by deleting it. `doctor.bat` diagnoses problems. For an NVIDIA GPU see [docs/setup-windows-nvidia-ocr.md](docs/setup-windows-nvidia-ocr.md).

### macOS

Download `mokuro-bunko-<version>-aarch64-apple-darwin-full.tar.gz` (Apple silicon, OCR through CoreML) or the `lite` build (Intel Macs have no full build; use a processor elsewhere for OCR), unpack it and run `./mokuro-bunko serve`. The `install.sh` script above works on macOS too. The binaries are not notarized: a tarball downloaded in a browser is quarantined, so run `xattr -d com.apple.quarantine mokuro-bunko` once (`curl | sh` installs are not affected).

### Docker

Images are published at `ghcr.io/gnathonic/mokuro-bunko`:

| Tag | What |
|---|---|
| `latest-lite` | Server only, distroless, 29 MB. OCR by remote processors. |
| `latest` | Server + CPU OCR + nginx for fast downloads. |
| `latest-cuda` | As `latest` with CUDA OCR for NVIDIA GPUs (driver 580 or newer, NVIDIA container runtime). |

```bash
docker run -d --name mokuro-bunko -p 8080:8080 -v mokuro-data:/data \
  -e PUID=1000 -e PGID=1000 -e MOKURO_STORAGE=/data -e MOKURO_CONFIG=/data/config.yaml \
  ghcr.io/gnathonic/mokuro-bunko:latest-lite
```

Compose files (lite, full, CUDA/Unraid, processor), Unraid templates, systemd units and reverse-proxy examples are in [`deploy/`](deploy/) and described in [docs/deployment.md](docs/deployment.md). Each tag also has a versioned form (`0.7.0`, `0.7.0-lite`, `0.7.0-cuda`).

### Android

The release also carries an **APK** (`mokuro-bunko-<version>-android.apk`, arm64-v8a and x86_64) that runs the lite server on the phone as a foreground service; OCR for it comes from remote processors. It is sideloaded and is not part of the in-app updater (it updates like any APK you install). Best effort: see the release notes for its state.

### From source

See [Building from source](#building-from-source).

**Something not working?** Run `mokuro-bunko doctor`: it checks your build, config, ONNX Runtime and models, free disk space and port, with fix hints. See [docs/troubleshooting.md](docs/troubleshooting.md).

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
| `ocr.backend` | `auto` | `auto`, `cuda`, `webgpu`, `directml`, `coreml`, `cpu`, or `skip` (no OCR on this machine) |
| `ocr.generations` | one `hayai-nova` row | The OCR recipes every volume gets a layer from, in run order |
| `ocr.local_processing` | `true` | `false` leaves all OCR to remote processors |
| `update.check` | `true` | Look for new releases in the background |
| `catalog.enabled` | `false` | Web-based library browser |

Every key can also be set from the environment (`MOKURO_<SECTION>_<KEY>`, e.g. `MOKURO_OCR_BACKEND`), plus the shortcuts `MOKURO_HOST`, `MOKURO_PORT`, `MOKURO_STORAGE` and `MOKURO_CONFIG`. The full reference is [docs/configuration.md](docs/configuration.md).

## OCR

The OCR engines run on [ONNX Runtime](https://onnxruntime.ai/) inside the full build. There is nothing to install besides the models. All of them are Apache-2.0:

| Engine | What it is |
|---|---|
| `hayai-nova` (default) | [hayai-ocr v2.5 Nova](https://huggingface.co/JustANormalTinkerer/hayai-ocr-v2.5-nova): reads each text line, strong on display lettering and sound effects. |
| `paddle-manga` | [PaddleOCR-VL 1.6](https://huggingface.co/PaddlePaddle/PaddleOCR-VL-1.6) with a [manga LoRA](https://huggingface.co/sorryhyun/paddleocr-vl-1.6-manga-lora): the most accurate and the slowest, about 2 GB of model files. |
| `ppocr-manga` | [PP-OCRv6 manga](https://huggingface.co/Kellenok/PP-OCRv6_manga) line detector and CTC recognizer on the CPU: small, reads scanned novel pages too. Its detector also feeds the other two. |

The 0.5 `mokuro` engine and the GPL `ctd`, `animetext` and `rtdetr` detectors are gone; see the [migration guide](docs/MIGRATING-0.7.md) for what happens to old configs and existing `.mokuro` files.

**Models** are ONNX exports of those weights. They are downloaded on first use into `<storage>/models/` and verified by sha256, so the first OCR run needs internet access and some disk (a few hundred MB for hayai-nova, several GB for paddle-manga). `mokuro-bunko models list|download|verify` manages them up front; `MOKURO_MODELS_DIR` points at a directory of files for air-gapped hosts. `install-ocr` still exists as a deprecated no-op for old scripts.

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
| Android | Notice only. |

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
mokuro-bunko install-ocr    # deprecated no-op (downloads models on a full build)
```

Global options: `-c, --config <PATH>` (or `MOKURO_CONFIG`), `-v, --verbose`, `--version` (prints the version, build flavor and target). `mokuro-bunko <command> --help` has the details.

## Building from source

Requires a recent stable [Rust toolchain](https://rustup.rs/). The frontends under `web/` are embedded at build time.

```bash
git clone https://github.com/Gnathonic/mokuro-bunko.git
cd mokuro-bunko
cargo build --release -p mokuro-bunko                           # full build, CPU OCR
cargo build --release -p mokuro-bunko --no-default-features     # lite: server only
cargo build --release -p mokuro-bunko --features cuda           # NVIDIA CUDA execution provider
```

Cargo features of `mokuro-bunko`: `ocr` (on by default: ONNX Runtime, the engines, local OCR and the `processor` command), plus execution providers `cuda`, `webgpu`, `directml` and `coreml` (each implies `ocr`). The ONNX Runtime binaries are fetched at build time; the CUDA provider additionally needs CUDA 13 and cuDNN 9 at run time. Release archives, signing and Docker images are built with `cargo run -p xtask -- <command>` (see [docs/rust-port/PACKAGING.md](docs/rust-port/PACKAGING.md)); the design is in [docs/rust-port/ARCHITECTURE.md](docs/rust-port/ARCHITECTURE.md).

```bash
cargo test --workspace      # run tests
cargo clippy --workspace    # lints
```

## License

[Mozilla Public License 2.0](LICENSE). Every release archive carries a `THIRD-PARTY-LICENSES.md` listing the licences of all bundled dependencies and native components (generated at build time; a copyleft licence with no permissive alternative fails the build). The OCR models are Apache-2.0 and are credited in the release notes, as are the datasets their model cards name.

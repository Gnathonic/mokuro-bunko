# mokuro-bunko 0.7 — packaging, releases and updates

**Status:** living document. Replaces the 0.5.2 packaging listed in
`spec/config-cli-ops.md` §11 (shiv zipapp, Python wheel, uv portable zip,
`setup-windows.ps1` source install, python:3.12 Docker images).

Where things live:

| Path | What |
|---|---|
| `crates/xtask/` | release tooling: `dist`, `torch-pack`, `manifest`, `sign`, `verify`, `keygen`, `licenses`, `docker-context` |
| `crates/xtask/src/torch_specs.rs` | what goes into each OCR backend pack: pinned libtorch zips, kept files, NVIDIA wheels (§8) |
| `packaging/torch/probe_pack.py` | drives an installed pack through its C ABI (load + read crops), for trimming and smoke tests |
| `packaging/dist/README.md` | README shipped in unix archives |
| `packaging/windows/` | `run.bat`, `doctor.bat`, `_env.cmd`, `README.txt`, `PORTABLE.txt` for the Windows zip |
| `packaging/android/` | the Android app (Kotlin, Gradle) and `build.sh`; the server library is `crates/bunko-android` (MOBILE.md) |
| `packaging/docker-init/` | `bunko-init`, the containers' PUID/PGID/UMASK entrypoint (static, libc only; own Cargo workspace) |
| `deploy/docker/` | `Dockerfile.lite`, `Dockerfile` (full + CPU pack), `Dockerfile.cuda` (full + cu130 pack), `entrypoint.sh` (nginx, OCR_AUTO_INSTALL), per-Dockerfile `.dockerignore` |
| `deploy/nginx-internal.conf.template` | the in-container nginx for the X-Accel offload (now with the processor WebSocket) |
| `deploy/*.service` | systemd units: server and processor, system and user variants |
| `deploy/docker-compose*.yml`, `deploy/unraid/*.xml` | compose files and Unraid templates for the published images |
| `scripts/install.sh`, `scripts/install.ps1` | installers that download a release (`setup-windows.ps1` forwards to `install.ps1`) |
| `.github/workflows/{ci,release,publish}.yml` | CI, tag → draft release, owner's publish step |

Run xtask with `cargo run -p xtask -- <command>`. A `cargo xtask` alias needs
`.cargo/config.toml` with `[alias] xtask = "run -p xtask --"` (not added yet:
outside this task's files).

## 1. Artifacts

One binary, `mokuro-bunko`, in two builds (ARCHITECTURE.md §1), plus the **OCR
backend packs** the full build downloads (§8, TORCH-BACKEND.md). The manifest names a
build by *target triple* and *flavor*:

| flavor | cargo features | contents |
|---|---|---|
| `lite` | `--no-default-features` | server only; OCR by remote processors |
| `full` | `ocr` (default) | + ONNX Runtime on the CPU (PP-OCR detector/reader), the libtorch pack loader, local OCR, `processor` |

GPU support is no longer a build flavor: the same `full` binary runs hayai-nova and
paddle-manga on whatever pack `install-ocr` installed — `cpu`, `cu130` (NVIDIA) or
`rocm7.1` (AMD, Linux). The ONNX Runtime GPU execution providers (`--ep cuda`,
`directml`, `coreml`, `webgpu` → `full-<ep>`) still compile (CI clippy checks
`cuda,webgpu`) but are not released.

Release matrix (`.github/workflows/release.yml`):

| target | lite | full | backend packs | how |
|---|---|---|---|---|
| `x86_64-unknown-linux-musl` | ✔ static | | | `cargo zigbuild` |
| `aarch64-unknown-linux-musl` | ✔ static | | | `cargo zigbuild` |
| `x86_64-unknown-linux-gnu` | | ✔ | `cpu`, `cu130`, `rocm7.1` | manylinux_2_28 container (`packaging/manylinux/build.sh`): glibc ≥ 2.28 |
| `x86_64-pc-windows-msvc` | ✔ | ✔ | `cpu`, `cu130` | native, windows-latest |
| `aarch64-apple-darwin` | ✔ | ✔ | `cpu` | native, macos-latest |
| `x86_64-apple-darwin` | ✔ | — | | cross from macos-latest |
| `aarch64-linux-android` + `x86_64-linux-android` | (APK `mokuro-bunko-<ver>-android.apk`, not in release.json; not released in 0.7) | | | job `android`: cargo-ndk + Gradle (`packaging/android/build.sh`); out of 0.7's scope, opt-in only with `vars.BUILD_ANDROID == 'true'`; see MOBILE.md |

Dropped against the ONNX-only design: `full-cuda` (Linux, Windows), Linux arm64 full
and the DirectML/CoreML defaults — there is no libtorch pack for Linux arm64 or Intel
macOS (TORCH-BACKEND.md scope), so those hosts use lite plus a processor elsewhere.

Why these choices:

- **Lite on Linux is musl + static**: one binary for every distro, NAS and Pi, and it runs
  on `distroless/static`. mimalloc is the allocator (musl's malloc is too slow).
- **Full on Linux is glibc, built in manylinux_2_28** (AlmaLinux 8: glibc 2.28,
  gcc-toolset-14), like a manylinux wheel, by `packaging/manylinux/build.sh` (the release
  workflow's `build-linux-full` / `packs-linux` jobs and local builds use the same
  script). Two things make ONNX Runtime 1.28's prebuilt static libraries work there:
  - they reference GCC 13's libstdc++ (`GLIBCXX_3.4.31`, e.g. `_M_replace_cold`):
    gcc-toolset's libstdc++ links those parts statically (`libstdc++_nonshared.a`) and
    takes only old symbol versions from the system's libstdc++.so.6;
  - they reference four glibc ≥ 2.32/2.38 symbols (`__isoc23_strtol`, `__isoc23_strtoll`,
    `__isoc23_strtoull`, `__libc_single_threaded`): `packaging/manylinux/glibc_compat.c`
    defines them (the C23 strto* forward to strto*; `__libc_single_threaded = 0`, i.e.
    "maybe several threads", always correct) and is linked into the binary.
  Result (measured 2026-10-03, `objdump -T`): the binary needs at most `GLIBC_2.28`,
  `GLIBCXX_3.4.21`, `CXXABI_1.3.11`; `libbunko_torch.so` at most `GLIBC_2.28` /
  `GLIBCXX_3.4.21`; libtorch itself is manylinux_2_28. So the full build runs on Debian
  11+, Ubuntu 20.04+, RHEL 8+ (tested: Debian 12, Ubuntu 22.04 — §7). A full build made
  on a newer distro does not run on older ones (an Arch-built binary needs
  `GLIBC_2.43`); `install.sh` catches this before replacing anything.
- **arm64 Linux has the lite build only** (owner decision 2026-10-03: no arm64 Linux
  machine to test OCR on; there are no arm64 packs). OCR for an arm64 library comes from
  a remote processor on x86_64, Windows or macOS.
- **ONNX Runtime is linked statically** from ort's prebuilt binaries
  (`download-binaries`; ort-sys rc.13 = ONNX Runtime 1.28). Only execution providers that
  ONNX Runtime loads as plugins are shared libraries: the CUDA provider
  (`libonnxruntime_providers_cuda.so` / `.dll` + `…_providers_shared`), and DirectML's
  `DirectML.dll`. A release `full` build uses none of them, so **nothing is bundled**;
  only the unreleased `--ep` builds get their provider libraries next to the executable
  (`xtask dist` picks them by name from the directory ort-sys linked from; TensorRT is
  left out). Until 2026-10-03 every library in those directories was copied: the
  Windows zip carried an unused 18 MB `DirectML.dll`, and on macOS ~25 Xcode sanitizer
  dylibs from a toolchain link path.
- **Windows: the Visual C++ runtime ships app-local.** A Rust MSVC binary imports
  `vcruntime140*.dll`; libtorch's DLLs import `msvcp140*.dll`/`vcruntime140*.dll`, and
  its CPU kernels and the compiled CPU packages `vcomp140.dll` (OpenMP). Windows does not
  guarantee them (the "VC++ 2015–2022 Redistributable"). `xtask dist` copies the ones
  the executable imports next to it, and `xtask torch-pack` puts `vcruntime140`,
  `vcruntime140_1`, `msvcp140`, `msvcp140_1`, `msvcp140_2` and `vcomp140` into each
  Windows pack's `lib/` (`crates/xtask/src/vcredist.rs`; `--no-vc-runtime` to leave them
  out). They come from the building Visual Studio's
  `VC\Redist\MSVC\<version>\x64\Microsoft.VC143.{CRT,OpenMP}` (`VC_REDIST_DIR` or
  `VCToolsRedistDir` override; the windows-latest runner has VS 2022). Terms: they are
  "Distributable Code" of Visual Studio 2022
  (<https://learn.microsoft.com/visualstudio/releases/2022/redistribution#visual-c-runtime-files>)
  and app-local deployment is a documented way to deploy them
  (<https://learn.microsoft.com/cpp/windows/deployment-in-visual-cpp>); the note is in
  `THIRD-PARTY-LICENSES.md` / the pack's `licenses/microsoft-vc-runtime.txt`. Verified
  here only as far as staging and the licence note (no Windows host); a Windows run
  without the Redistributable installed is still to do.
  Linux full builds get an `$ORIGIN` runpath so the provider libraries are found next to
  the real executable even when it is started through a symlink.
- **No macOS universal binary**: ort ships no `x86_64-apple-darwin` ONNX Runtime, so a
  universal full build is impossible, and the updater picks artifacts by target triple
  anyway. Intel Macs get lite (use a processor elsewhere for OCR).
- **GPUs come from packs, not from the binary**: the binary never links libtorch, so it
  starts (and serves) with no pack, as 0.5.2's server did before `install-ocr`. CUDA
  packs need an NVIDIA driver ≥ 580 (CUDA 13.0) and nothing else from the host.

Archive name: `mokuro-bunko-<version>-<target>-<flavor>.tar.gz` (`.zip` on Windows),
each with a top directory of the same name holding:

- unix: `mokuro-bunko`, provider libraries if any, `README.md`, `LICENSE`,
  `THIRD-PARTY-LICENSES.md`; plus the desktop tray where there is one (below): x86_64
  Linux `mokuro-bunko-tray` + `share/` (menu entry, autostart entry, hicolor icons),
  macOS `mokuro-bunko.app`;
- Windows (this *is* the portable build that replaces 0.5.2's uv zip): `mokuro-bunko.exe`,
  DLLs if any, `run.bat`, `doctor.bat`, `_env.cmd`, `PORTABLE.txt`, `README.txt`,
  `LICENSE.txt`, `THIRD-PARTY-LICENSES.md`, `mokuro-bunko-tray.exe`, `mokuro-bunko.ico`
  (batch files get CRLF line endings).
  With `PORTABLE.txt` present, `run.bat` keeps config/library/logs/models in `data\`
  next to it (0.5.2's portable guarantee: nothing in AppData or the registry).
  `install.ps1` deletes `PORTABLE.txt`, so an installed copy uses
  `%LOCALAPPDATA%\mokuro-bunko`, where 0.5.2 kept its library.

Each archive gets a `<archive>.sha256`; the release also carries `SHA256SUMS`,
`release.json` and `release.json.sig`.

### Desktop tray (`mokuro-bunko-tray`, GUI.md §5–§6)

A second executable, built from `crates/bunko-tray` (tray-icon 0.24 + muda 0.19 + tao
0.34, all MIT/Apache-2.0), shipped in the desktop archives. It stays a separate binary
because on Linux it links GTK 3: **`mokuro-bunko` itself never links a GUI toolkit**
(`cargo tree -p mokuro-bunko` has no gtk/tray-icon/muda/tao, lite or full), so the
server and processor still run on a headless box, NAS or container.

| archive | tray | how it is built |
|---|---|---|
| `x86_64-unknown-linux-gnu` full | `mokuro-bunko-tray` + `share/` | same manylinux_2_28 container (`dnf install gtk3-devel`), glibc ≥ 2.28 |
| `x86_64-unknown-linux-musl` lite | the same glibc tray (`dist --tray-bin`) | built in the manylinux job; the static CLI stays musl |
| `aarch64-unknown-linux-musl` lite | — | no arm64 Linux desktop target in 0.7 |
| `x86_64-pc-windows-msvc` lite + full | `mokuro-bunko-tray.exe`, `mokuro-bunko.ico` | native (`windows_subsystem = "windows"`: no console) |
| `*-apple-darwin` lite + full | `mokuro-bunko.app` | native |

- **macOS**: `mokuro-bunko.app/Contents/{Info.plist, PkgInfo, MacOS/mokuro-bunko-tray,
  MacOS/mokuro-bunko, Resources/mokuro-bunko.icns}`, `LSUIElement` (menu-bar only, no
  Dock icon), bundle id `io.github.gnathonic.mokuro-bunko.tray`, template
  `packaging/macos/Info.plist`. The CLI inside the bundle is a **hard link** to the
  top-level `mokuro-bunko` (the tar has one copy and a link entry, so the archive does
  not grow by a second 30–60 MB binary; the top-level entry comes first, which is the one
  the updater extracts). The tray prefers the CLI outside the bundle (`../../../`)
  because that is the file `mokuro-bunko update` replaces. Not signed/notarized in 0.7:
  first start is right-click → Open (same as the CLI).
- **macOS disk image** (`packaging/macos/make-dmg.sh <full .tar.gz> <ocr-offline dir>
  <out.dmg>`, run on a Mac with `pip install dmgbuild`): the window shows only
  `Mokuro Bunko.app` and an Applications alias on a "drag to install" background
  (`dmg-background.svg` → `.png`/`@2x.png`), plus a small `Read me.txt`. The app is the
  archive's bundle renamed, its CLI a real file (not a link), and the offline OCR files
  (pack archive + model files, what `install-ocr --from` takes) in
  `Contents/Resources/ocr-offline`. `install-ocr` without `--from` (and so the wizard's
  OCR step, which shows "Source: Bundled with this app") installs from that folder by
  itself (`bundled_offline_dir`: `<exe>/../Resources/ocr-offline` inside an app bundle,
  else `<exe>/ocr-offline`, when it holds a `*-torch-*.tar.zst`). The tray in an app not
  named `mokuro-bunko.app` always runs the CLI inside its own bundle; the updater
  replaces both binaries there (the tray is the CLI's sibling). Terminal users run
  `/Applications/Mokuro Bunko.app/Contents/MacOS/mokuro-bunko`. Measured on an M2 Pro
  (2026-10-05): 834 MB dmg; the wizard's OCR install takes the bundled files with no
  download; a 20-page volume OCRs in 36 s with hayai-nova fp32 on the CPU.
- **Linux**: the tray links `libgtk-3.so.0` and dlopens `libayatana-appindicator3.so.1`
  (or `libappindicator3.so.1`) at run time; both LGPL system libraries, not shipped.
  `mokuro-bunko doctor` checks for them when the tray is installed next to it and names
  the packages (Debian/Ubuntu `libgtk-3-0t64`/`libgtk-3-0` + `libayatana-appindicator3-1`,
  Fedora `gtk3 libayatana-appindicator-gtk3`, Arch `gtk3 libayatana-appindicator`,
  openSUSE `libgtk-3-0 libayatana-appindicator3-1`). GNOME shows tray icons only with
  the "AppIndicator and KStatusNotifierItem Support" extension (Ubuntu ships it on).
  `share/applications/mokuro-bunko-tray.desktop` and `share/autostart/…` are templates
  (`Exec=@EXEC@`, from `packaging/linux/`); `share/icons/hicolor/<n>x<n>/apps/mokuro-bunko.png`
  16–512 + scalable SVG.
- **Icons**: original artwork, generated by `packaging/icons/generate.py` (Python +
  `rsvg-convert`; outputs committed): the app icon (teal tile, open book) and tray
  icons for idle / working / paused / attention at 16–64 px (44 px for the macOS menu
  bar), `mokuro-bunko.ico` and `mokuro-bunko.icns`. The tray embeds its PNGs.
- **xtask**: `dist` builds the tray for the archive's target when `names::tray_target`
  has one (`cargo build -p bunko-tray --release --target …`) and stages it as above;
  `--no-tray` leaves it out, `--tray-bin FILE` takes a prebuilt one (the musl lite
  archive gets the manylinux job's binary). The tray runs `--version` as a smoke test
  where the host can execute it. Docker builds pass `--no-tray` and `xtask docker`
  drops `mokuro-bunko-tray`/`share/` from the context: **images are unchanged**.
- **Licences**: `xtask licenses`/`dist` add the tray's crate graph (its own target and
  features) to `THIRD-PARTY-LICENSES.md` under "Desktop tray", with the LGPL
  system-library note on Linux; the copyleft gate applies to it the same way (gtk-rs
  crates are MIT; `dlopen2_derive` and `libappindicator-sys` ship no licence file:
  warnings, both permissive).
- **Install scripts**: `install.sh` (Linux, glibc) links the tray next to the CLI in
  the bin dir, installs the menu entry with an absolute `Exec`, the hicolor icons and,
  with `--autostart`, `~/.config/autostart/mokuro-bunko-tray.desktop` (`--no-desktop`
  skips the entry and icons); on macOS the archive's `.app` is opened or dragged to
  Applications. `install.ps1` points the Start-menu shortcut "Mokuro Bunko" and
  `-Startup` at the tray (plus "Mokuro Bunko server (console)" → `run.bat`) and writes
  `tray.json` (`{"managed":[{"role":"server"}]}`) so the tray starts and supervises the
  server. "Start at login" in the tray menu writes the same autostart entry (Linux),
  a LaunchAgent `io.github.gnathonic.mokuro-bunko-tray` (macOS) or a Startup shortcut
  (Windows).
- **Updates**: `mokuro-bunko update` (bunko-update `update_companions`) also replaces an
  installed `mokuro-bunko-tray` next to the executable or inside `mokuro-bunko.app`
  from the same archive, best effort, after the CLI. A running tray keeps its old copy
  until it is next started; the instances it supervises restart into the new version
  (`MOKURO_LAUNCHER=tray`: exit code 75 restarts at once).

### Licences

`xtask dist` (and `xtask licenses`) walk the `cargo metadata` graph of `mokuro-bunko` for
the exact target and features (normal dependencies only), classify each SPDX expression
(OR = any alternative, AND = all), and write `THIRD-PARTY-LICENSES.md`: a table, the
native components, and every licence/notice file the crates ship, deduplicated by text.
**A copyleft licence (GPL/LGPL/AGPL/SSPL/EUPL/…) with no permissive alternative fails the
build**; unknown licence ids and crates without a licence file are warnings (today:
`asn1-rs-impl` and `yasna` ship no licence file; both MIT/Apache-2.0). CI runs
`xtask licenses` for lite and every full variant. Non-OSS redistributables flagged in the
file: Microsoft's Visual C++ runtime (Windows, app-local, above). CUDA/cuDNN are never in the archives;
the backend packs' licences are in §8.

## 2. Docker images (`ghcr.io/gnathonic/mokuro-bunko`)

Parity with 0.5.2: a CPU image (0.5.2 `deploy/Dockerfile`) and a CUDA image (0.5.2
`Dockerfile.unraid`), plus the new lite image. The full and CUDA images run the same
`full` binary and differ only in the **backend pack baked in** (§8) and their defaults.

| tag | Dockerfile | base | pack | platforms | size (uncompressed) |
|---|---|---|---|---|---|
| `<ver>-lite`, `latest-lite` | `deploy/docker/Dockerfile.lite` | `gcr.io/distroless/static-debian13` | — | amd64, arm64 | 29 MB |
| `<ver>`, `latest` | `deploy/docker/Dockerfile` | `debian:trixie-slim` + nginx, tini | `cpu` | amd64 | ~600 MB (177 MB compressed) |
| `<ver>-cuda`, `latest-cuda` | `deploy/docker/Dockerfile.cuda` | `debian:trixie-slim` + nginx, tini | `cu130` + NVIDIA libraries | amd64 | ~2.85 GB (1.79 GB compressed) |

- **No CUDA base image.** libtorch cu130 and the CUDA 13.0 libraries it needs (cuBLAS,
  cuDNN, ...; taken from NVIDIA's PyPI wheels, §8) are in the pack under
  `/opt/mokuro-bunko/backends/torch-cu130-2.13.0`; only the driver library comes from
  the host (the NVIDIA Container Toolkit mounts it). Host requirements: NVIDIA driver
  ≥ 580 (CUDA 13.0), the toolkit (`--gpus all`; Unraid: Nvidia-Driver plugin +
  `--runtime=nvidia`), a Turing (sm_75) or newer GPU. (Without a GPU the pack loads and
  sees the CPU and runs every engine there on the CPU packages, as 0.5.2's image fell back
  to CPU torch — verified 2026-10-03.)
- **OCR out of the box**: `MOKURO_TORCH_PACK` points the loader at the baked pack; the
  compiled model packages and ONNX models download on first use into
  `/data/models` (the data volume, so they survive image updates).
- **amd64 only** for full/cuda: there are no Linux arm64 packs (TORCH-BACKEND.md scope);
  arm64 hosts run the lite image and OCR on a processor elsewhere. No ROCm image (0.5.2
  had none; AMD users run the processor natively, `install-ocr` installs the ROCm pack).
- `Dockerfile.cuda --build-arg BAKE_PACK=0` leaves the pack out; with
  `OCR_AUTO_INSTALL=true` the entrypoint then runs `mokuro-bunko install-ocr
  --no-models` on start and the pack lands in `/data/backends` (downloaded from the
  release and PyPI, as 0.5.2's image installed CUDA torch on first start). This is the
  alternative if baked NVIDIA libraries are not acceptable (§8 licences).

Each Dockerfile has two binary sources, picked by `--build-arg BIN_FROM=`:
`source` (default; builds with `xtask dist` and `xtask torch-pack --no-archive` inside,
so `docker build` works from a checkout; it downloads libtorch, and for CUDA the NVIDIA
wheels, through a BuildKit cache mount) or `prebuilt` (the release workflow:
`xtask docker-context` unpacks the signed release archives into
`dist/docker/<amd64|arm64>/<lite|full|cuda>/` plus `bunko-init`, and installs the
released `cpu` pack into `full/backends/` and the `cu130` pack, completed with the
NVIDIA wheels, into `cuda/backends/`).

Container contract (kept from 0.5.2, `spec/config-cli-ops.md` §11.2):

- Starts as root, then `bunko-init` drops to `PUID:PGID` (defaults 1000:1000 in `lite`
  and `full` — the uid of 0.5's `mokuro` user — and 99:100 in `cuda`, as 0.5's Unraid
  image), sets `UMASK` (002), chowns the storage and config directories (recursively
  with `TAKE_OWNERSHIP=true`) and execs `mokuro-bunko` (default command `serve`).
  `docker run --user …` works too (PUID/PGID then ignored).
- `MOKURO_*` variables as before. `MOKURO_INSTALL_KIND=docker` is set, so the updater
  only reports the image to pull; the CUDA image also sets
  `MOKURO_UPDATE_FLAVOR=full-cuda`, so it is told to pull `:<ver>-cuda` (the binary
  itself is the plain `full`).
- `MOKURO_NGINX_ACCEL=1|true` (full/cuda images): `entrypoint.sh` renders
  `nginx-internal.conf.template`, starts nginx on `MOKURO_PORT` (master root, workers
  PUID:PGID) and moves the server to `127.0.0.1:MOKURO_BACKEND_PORT` (8081). Default is
  off (0.5's generic image defaulted it on; the async server no longer needs it). The
  lite image has no nginx: `bunko-init` drops the variable with a warning, so a carried
  over `MOKURO_NGINX_ACCEL=1` never produces empty X-Accel responses.
- `OCR_AUTO_INSTALL=true` (0.5.2's variable, full/cuda images) runs `install-ocr
  --no-models` as PUID:PGID before `serve`: a no-op when a pack is baked in or already
  in `/data/backends`. Retired variables `MOKURO_BUNKO_OCR_ENV`,
  `MOKURO_BUNKO_OCR_ENGINES_ENV`, `MOKURO_BUNKO_MOKURO_SPEC` are accepted and ignored.
- The compiled model packages are unpacked once into `/data/models` (the data volume)
  and loaded in place, not into `/tmp` (`bunko-torch`'s `unpack.rs`).
- `HEALTHCHECK` runs `mokuro-bunko healthcheck` (GET `/api/health`; no curl in the images).
- Labels: `org.opencontainers.image.licenses=MPL-2.0` (0.5 said MIT; spec Q9).

nginx template changes: the `/_processor/` location now passes the WebSocket upgrade
(`Upgrade` + `Connection $connection_upgrade`, mapped so ordinary requests keep upstream
keep-alive) with 3600 s timeouts, for protocol v3's processor socket, and keeps
unbuffered, uncapped `PUT` uploads.

**Unraid with `MOKURO_NGINX_ACCEL=1`**: switch the template's repository to
`ghcr.io/gnathonic/mokuro-bunko:<ver>-cuda` (or `latest-cuda`). Every existing variable
keeps working; `MOKURO_NGINX_ACCEL=1` keeps nginx. An Unraid host needs driver ≥ 580
for the CUDA 13 pack (the 0.5.2 image's torch cu130 needed the same); with OCR on another machine, `:<ver>-lite` (template
`mokuro-bunko-lite.xml`) is enough, but it has no nginx.

## 3. release.json and signing

```json
{
  "version": "0.7.0",
  "published_at": "2026-10-01T18:54:27Z",
  "notes_url": "https://github.com/Gnathonic/mokuro-bunko/releases/tag/v0.7.0",
  "artifacts": {
    "x86_64-unknown-linux-musl": {
      "lite": { "url": "https://github.com/Gnathonic/mokuro-bunko/releases/download/v0.7.0/mokuro-bunko-0.7.0-x86_64-unknown-linux-musl-lite.tar.gz",
                "sha256": "…", "size": 5814114, "binary": "mokuro-bunko" }
    }
  },
  "docker": { "full": "ghcr.io/gnathonic/mokuro-bunko:0.7.0",
              "full-cuda": "ghcr.io/gnathonic/mokuro-bunko:0.7.0-cuda",
              "lite": "ghcr.io/gnathonic/mokuro-bunko:0.7.0-lite" }
}
```

- Written by `xtask manifest` from the archives in a directory (names parsed back into
  target and flavor), serialized with bunko-update's own `Manifest` type, pretty-printed
  one key per line (what `install.sh` parses without jq).
- `release.json.sig` = base64 ed25519 signature over the exact bytes of
  `release.json` (`xtask sign`). `bunko-update` verifies it with the public key compiled
  into the binary (`RELEASE_PUBLIC_KEY`, `bvEsRQhV…HEk=`; forks override with
  `BUNKO_RELEASE_PUBLIC_KEY` at build time) before trusting any sha256 in it.
- `xtask sign` refuses a key whose public half is not the compiled-in key (such a
  release could never verify) and re-verifies its own output the way the updater does.
  `xtask verify [--dir dist --require-all]` checks signature, sha256 and size.

### Signing key handling

- The key is a base64 32-byte ed25519 seed. The owner's copy is
  `~/.config/mokuro-bunko-release/signing.key` (mode 600; `signing.pub` next to it). It
  is never in the repository and xtask never prints it.
- **The owner must add it as a GitHub Actions secret** before the first release:
  `gh secret set BUNKO_SIGNING_KEY < ~/.config/mokuro-bunko-release/signing.key`
  (or Settings → Secrets and variables → Actions → New repository secret). The release
  workflow fails early when it is missing. Consider putting it in a protected
  `release` environment with required reviewers.
- Rotation: generate a new key (`cargo run -p xtask -- keygen --out new.key`, prints the
  public key), put the public key in `crates/bunko-update/src/lib.rs`, release once
  signed with the **old** key (so installed servers accept the binary carrying the new
  key), then switch the secret. A leaked key means a release with a new key and a
  manual reinstall for anyone who might have received a forged update.

## 4. How updates work, per install kind

The server polls `https://github.com/Gnathonic/mokuro-bunko/releases/latest/download/release.json`
(+ `.sig`), verifies it, and compares versions (`update.channel`). What happens next
depends on `bunko_update::InstallKind::detect()`:

| install | detected as | admin panel | how it is applied |
|---|---|---|---|
| tarball / `install.sh` as a user (`~/.local/lib/mokuro-bunko`) | self-managed | **Update** button | download archive → sha256 from the signed manifest → swap the executable (`self-replace`) → `exec` itself (same PID: systemd user units keep tracking it) |
| `install.sh` as root + system unit | `MOKURO_INSTALL_KIND=install.sh` → managed | notice | binary is root-owned and `/usr` read-only to the service: re-run `install.sh` |
| Windows zip / portable / `install.ps1` | self-managed (`_env.cmd` sets `MOKURO_INSTALL_KIND=self`) | **Update** button | as above; `run.bat` restarts the server when it exits with code 75 (see §6) |
| Docker | `/.dockerenv` or `MOKURO_INSTALL_KIND=docker` (set in all images) | notice with the image ref from `docker[flavor]` | `docker pull` / Unraid "update" |
| distro package, Homebrew, Nix (`/usr/bin`, `Cellar`, `/nix/store`) | managed | notice | the package manager |
| Android / iOS | mobile | notice | the app store |

A self-managed install updates the release as one unit (the binary, the tray next to
it, the release's backend pack for the installed variant, the models its compiled-in
manifests name): the new binary is staged next to the running one and runs its own
hidden `update prefetch` (stages + load-checks its pack in `backends/.staging-<variant>`,
fetches its models) before anything is switched; then the binary and the pack switch
together, the old ones kept (`.mokuro-bunko-previous`, `backends/.prev-<name>`) until the
new release's pack has loaded after the restart (`install-ocr --probe` in a child), and
rolled back together when it does not. Opt-in automatic updates (`update.auto`,
`processor.auto_update`) do the same unattended at a quiet moment
(docs/configuration.md "Automatic updates"); a version's manifest is derived from
`update.manifest_url` (`…/releases/download/v<ver>/release.json`), so a mirror must keep
every version it serves under `v<ver>/`. A test or fork release signed with another key
is trusted only through `update.public_key` in the config FILE (never the environment,
never the admin API), logged loudly at every start.

`install.sh` verifies the same signature with OpenSSL 3 when available (warns or, with
`--require-signature`, fails without it) and always checks the sha256.
`install.ps1` checks the sha256 over HTTPS but cannot check ed25519 (Windows
PowerShell has no ed25519); the in-app updater does check the signature.

## 5. Cutting a release

1. Bump `version` in the root `Cargo.toml` (`[workspace.package]`), update
   `CHANGELOG.md`, commit, and push a tag: `git tag v0.7.0 && git push origin v0.7.0`.
   (`-alpha.N`/`-rc.N` versions become GitHub pre-releases.)
2. `release.yml` runs: checks the tag equals the workspace version and that
   `BUNKO_SIGNING_KEY` exists; tests; builds the matrix; writes, signs and verifies
   `release.json`; creates a **draft** release with all assets; pushes the versioned
   images `:<ver>-lite`, `:<ver>`, `:<ver>-cuda`. A draft is invisible to
   `releases/latest/download/`, so no installed server sees it yet.
3. Review the draft (notes, assets; `cargo run -p xtask -- verify release.json --dir .`
   on downloaded assets if you like).
4. Run **Publish** (Actions → Publish → tag `v0.7.0`): re-verifies the manifest,
   un-drafts the release (marks it latest), and for stable versions moves
   `latest`, `latest-lite`, `latest-cuda`. Installed servers see it on their next check.

Re-running `release.yml` for the same tag (workflow_dispatch) replaces the draft's
assets (`--clobber`) and re-pushes the versioned images.

Locally, the same pipeline (minus upload):

```sh
export CARGO_TARGET_DIR=target/release-local
cargo run -p xtask -- dist --zig --target x86_64-unknown-linux-musl --flavor lite
cargo run -p xtask -- dist --target x86_64-unknown-linux-gnu --flavor full
cargo run -p xtask -- torch-pack --variant cpu --out dist          # + cu130, rocm7.1 (Linux)
cargo run -p xtask -- manifest --version 0.7.0 --dir dist
cargo run -p xtask -- sign dist/release.json --key ~/.config/mokuro-bunko-release/signing.key
cargo run -p xtask -- verify dist/release.json --dir dist --require-all
cargo run -p xtask -- docker-context --dir dist          # then docker build --build-arg BIN_FROM=prebuilt …
```

## 6. Requirements on the binary and server (for the crates' owners)

- `mokuro-bunko healthcheck` (exists): exit 0 iff the local server answers; used by every
  image's `HEALTHCHECK`, run as root via `docker exec`, so it must not write the log file.
- `--version` must print the version (xtask's smoke test checks it) and should keep naming
  the flavor (`lite`/`full`): the smoke test fails a lite build that calls itself full.
- **The updater flavor**: released binaries are `lite` or `full` (GPU support comes from
  packs). `MOKURO_UPDATE_FLAVOR` overrides it (`crate::update_flavor()`): the CUDA image
  sets `full-cuda` so the updater names the `-cuda` image (`docker[flavor]`).
- `mokuro-bunko`'s `ocr`/`cuda`/`directml`/`coreml`/`webgpu` features must forward to
  `bunko-ocr` (and so to ort). Until they do, "full" archives contain no ONNX Runtime and
  `xtask dist` warns.
- Archives with provider libraries (`full-cuda`): `bunko-update` only replaces the
  executable. When the ONNX Runtime version changes, the bundled
  `libonnxruntime_providers_*.so`/`.dll` must be replaced too, or the CUDA EP fails to
  load. Either extract every file of the archive next to the executable, or refuse the
  in-place update for archives that hold more than the binary.
- Windows restart: `restart()` spawns a new process and exits, which detaches it from
  `run.bat`'s console handling. When `MOKURO_LAUNCHER=run.bat` (set by `_env.cmd`), exit
  with code **75** instead; `run.bat` loops on 75.
- Pre-release channel: `releases/latest/download/` never points at a GitHub pre-release,
  so `update.channel = prerelease` sees nothing newer from that URL. Needs a separate
  manifest URL (e.g. a rolling `channel-prerelease` release that the release workflow
  also uploads `release.json` to) — not built yet.
- `MOKURO_NGINX_ACCEL` must stay tolerated by the server; inside images it is only set
  when nginx really runs. Cosmetic: in nginx mode the first-run log line prints the
  backend address (`http://127.0.0.1:8081/setup?token=…`) rather than the public port.

## 7. Verification status

Verified locally (2026-10-01, x86_64 Arch host, `CARGO_TARGET_DIR=target/agent-packaging`):

- `xtask` unit tests (15): names/flavors/features, archive round trips through
  `bunko_update::extract_binary`, manifest from a directory, sign/verify/keygen with
  tamper, wrong-key and checksum-mismatch cases, SPDX classification.
- `xtask dist` for `x86_64-unknown-linux-gnu` lite and full, `x86_64-unknown-linux-musl`
  lite (zig; statically linked, smoke-tested) and `aarch64-unknown-linux-musl` lite
  (zig; static, not runnable here); a Windows zip layout from a stand-in exe.
- `manifest` → `sign` with the real key (file and `BUNKO_SIGNING_KEY`) → `verify` with
  the compiled-in key; a wrong key is refused by `sign`.
- `install.sh` against a local release: bash, dash (Debian 12) and busybox (Alpine);
  signature OK / tampered / wrong key, musl → lite, glibc-too-old refusal.
- `install.ps1` / `setup-windows.ps1` parse, and run under pwsh on Linux up to
  launching the exe (download, sha256, unzip, PORTABLE.txt removal, missing-flavor error).
- Docker: the lite image built from source (`xtask dist` inside `rust:1-alpine`) runs
  the real server as PUID:PGID with umask 002, reports `healthy` through
  `mokuro-bunko healthcheck`, idles at 15.7 MiB and stops cleanly on SIGTERM (exit 0).
  The full image (prebuilt) in nginx mode: nginx on :8080 (master root, workers PUID),
  server on 127.0.0.1:8081, a 3 MB library volume downloaded byte-identical through the
  X-Accel internal location (nginx ETag), a WebSocket upgrade to `/_processor/…/socket`
  reaches the server (401 without a token), clean stop through tini. The CUDA image
  builds (stand-in binary; apt, tini, nginx, env, PUID 99:100). `--user`,
  `TAKE_OWNERSHIP`, retired variables and the lite image's `MOKURO_NGINX_ACCEL` handling
  checked. The nginx template passes `nginx -t`.
- systemd units pass `systemd-analyze verify`; workflows pass actionlint; shell scripts
  pass shellcheck; compose files pass `docker compose config`.

Verified locally 2026-10-02 (stream C, libtorch backend; details in §8):

- `xtask torch-pack` built the Linux `cpu`, `cu130` and `rocm7.1` packs with
  libbunko_torch (closure check passed); Windows `cpu`/`cu130` and macOS `cpu` staged
  with `--no-cdylib` and closure-checked (PE / Mach-O). `xtask` tests cover pack names,
  keep rules, ROCm arch trimming, split + reassembly, pack.json round trip;
  `bunko-update` tests cover unpacking split archives, whole-archive / per-file
  checksums, unlisted and unsafe entries, links, wheel member extraction.
- `install-ocr --from` installed the cpu pack (desktop), the cu130 pack on beast with
  the NVIDIA wheels **downloaded from PyPI** (1.67 GB, 27 s) and the 2-part rocm7.1 pack
  (desktop); the packs then ran hayai-nova / paddle-manga through their C ABI on the
  CPU, an RTX 4090 and an RX 9070 XT (§8 table), the ROCm one also in a clean Debian
  container without ROCm.
- **Linux full binary on old distros** (2026-10-03): `packaging/manylinux/build.sh full
  cpu cu130 rocm7.1` in `quay.io/pypa/manylinux_2_28_x86_64` (from stream A's final
  source). In clean `debian:12` (glibc 2.36) and `ubuntu:22.04` (glibc 2.35) containers
  the archive's binary runs (`--version`), `install-ocr --variant cpu --from` installs the
  manylinux cpu pack, `doctor` passes (`OCR backend: torch-cpu-2.13.0`), the pack's C ABI
  reads 40/40 crops identical to 0.5.2, and the server OCRs the 10-page volume with
  hayai-nova (fp32, the CPU default) and a ppocr-manga layer: 10 pages each, 0 failed;
  hayai **10/10 pages, 54/54 blocks identical to 0.5.2 torch fp32** on both distros.
  (Compiled packages for the test: hayai `linux-cpu-x86_64-v3` fp32 and
  `linux-cpu-x86_64-v4bf16` bf16, built in Debian 12 with the exec-stack bit cleared —
  stream B's pipeline must do the same.)
- **CPU image** (built from source, `debian:trixie-slim`, ~600 MB / 177 MB compressed): the server
  starts healthy as PUID:PGID, loads the baked pack (`libtorch backend cpu
  (/opt/mokuro-bunko/backends/torch-cpu-2.13.0) loaded in 0.3s`), `doctor` passes
  (`OCR backend: torch-cpu-2.13.0`), `install-ocr --list` reports the baked pack, an
  admin account is created with `admin add-user`, and a 10-page cut of Dr. Stone 01
  dropped into the library is OCRed by hayai-nova bf16 on the CPU in 9.2 s — **10/10
  pages, 54/54 blocks identical to 0.5.2 torch fp32** (beast reference). The compiled
  package had to be built in Debian 12 and have its exec-stack flag cleared (§8 open
  items): packages built on Arch need GLIBC_2.43.
- **CUDA image**: built from source (rebuilt 2026-10-03 with the stubbed pack: ~2.85 GB
  uncompressed, 1.79 GB compressed; the old `nvidia/cuda:13.0.3-cudnn-runtime` base alone
  was ~5 GB); `install-ocr --list` reports the baked `torch-cu130-2.13.0` pack (2.5 GB,
  NVIDIA licence texts under `licenses/`, the three stubs ~16 KB each),
  `MOKURO_UPDATE_FLAVOR=full-cuda`. On this desktop (no NVIDIA GPU) the pack loads ("cuda
  libtorch is loaded but sees no GPU") and the server OCRs the 10-page volume on the CPU
  with hayai-nova fp32 (54/54 blocks = 0.5.2 fp32) and a ppocr-manga layer. (The first
  run offered only ppocr-manga: the CPU package was under the old target name.)
  **Not run on an NVIDIA GPU**: beast has no Docker / NVIDIA Container Toolkit (podman
  without CDI), so per the owner's instructions it stayed build-only; the identical
  cu130 pack was verified natively on beast's RTX 4090 (§8). Both containers stop
  cleanly on `docker stop` (exit 0).
- `xtask docker-context` with a stand-in full archive and the real cpu + cu130 pack
  archives: `amd64/full` and `amd64/cuda` get the binary, `full/backends/torch-cpu-…`
  (418 MB) and `cuda/backends/torch-cu130-…` completed with the NVIDIA wheels (2.7 GB).

Only verifiable in CI / on real hardware:

- macOS and Windows (MSVC) builds, Linux arm64 full (native arm runner), the CUDA
  variants and the bundling of ONNX Runtime provider libraries (needs the `ocr` feature
  wired), glibc 2.35 compatibility of ubuntu-22.04 builds.
- Multi-arch image builds and pushes, GitHub release creation, Publish's tag moves.
- CUDA EP on a GPU (driver ≥ 580), DirectML/CoreML at runtime, the Windows shortcuts,
  `run.bat` restart loop, and the Android CI job (the APK itself is verified on an emulator, MOBILE.md §6).
- Code signing is not done: Windows Authenticode (SmartScreen will warn) and macOS
  notarization (a tarball downloaded by a browser gets quarantined:
  `xattr -d com.apple.quarantine mokuro-bunko`; `curl | sh` installs are not affected).

## 8. OCR backend packs (libtorch)

TORCH-BACKEND.md decides *why*; this is *how they are built, shipped and installed*.

### Layout and pack.json

    <storage>/backends/torch-<variant>-2.13.0/     (Docker: /opt/mokuro-bunko/backends/…)
        pack.json             manifest (below)
        libbunko_torch.so     our cdylib (bunko_torch.dll / libbunko_torch.dylib), RUNPATH $ORIGIN/lib
        lib/                  the trimmed libtorch runtime (+ CUDA libraries, + ROCm libraries)
        licenses/             PyTorch LICENSE + bundled third-party licences, NVIDIA licence texts, README.md

`pack.json` (`bunko_update::backend::PackManifest`) is a superset of what the loader
reads (`bunko_torch::abi::PackManifest`: `format`, `variant`, `torch`, `abi`, `os`,
`arch`, `library`, `lib_dir`, `files`). It adds `name`, `torch_build` (PyTorch commit),
`target`, `bunko_version`, `requires` (`nvidia_driver`, `system_libs`, `gpu_archs`),
`external` (upstream wheels the installer takes files from, each pinned by url +
sha256 + size, every extracted file pinned by sha256 + size) and `links`: every ELF
library whose SONAME differs from its file name is also reachable under the SONAME
(the ROCm zip names `libamd_comgr.so` what `libtorch_cpu.so` and others NEED as
`libamd_comgr.so.3`; without it ld.so searches the host, picks `/opt/rocm` where there is
one and fails where there is not — seen on patrick). The links are symlink entries in the
archive (so a plain `tar -x` works), listed in pack.json (the installer checks each
against it and creates them, as copies on Windows) and created by `--no-archive` too. The
closure check resolves NEEDED names by **file or link name inside the pack** only, never
by a SONAME carried under another file name, and it runs in the manylinux container,
which has no `/opt/rocm`.

Which pack a process uses: `MOKURO_TORCH_PACK=<dir>` (the images set it), else the
loader scans `<storage>/backends/` (bunko-engines `torch::discover`; names starting with
`.` — `.staging-*`, `.prev-*` — are never opened).

**A pack is locked to its release.** `bunko_version` (written by `xtask torch-pack` from
the workspace version) must equal the loading binary's version: the loader refuses any
other release's pack (`the backend pack is from mokuro-bunko A, this is B: each release
runs only its own pack; run 'mokuro-bunko install-ocr'`) before it checks the ABI, and
`install-ocr` installs only the pack listed in its own release's `release.json` (an
installed pack of another release does not count as installed). A pack without
`bunko_version` (built outside a release) is a development pack and any build opens it.
`doctor`, `/control/status` (`backend.pack`) and the tray name the pack as
`<variant> for <release> (<dir name>)`. The `release.json` entry also carries the pack's
`requires` (e.g. `nvidia_driver`), so an installer can refuse before downloading.

### Archives, release.json, trust chain

`xtask torch-pack` writes `mokuro-bunko-<ver>-<target>-torch-<variant>.tar.zst`
(zstd 19, 128 MiB window + long-distance matching, multithreaded; `pack.json` is the
first tar entry so tools read it without unpacking) and splits it into `.001`, `.002`,
… parts above `--max-part` (1.9 GiB: GitHub release assets must stay under 2 GiB).
`xtask manifest` puts every pack it finds in `dist/` into `release.json`:

```json
"backends": {
  "x86_64-unknown-linux-gnu": {
    "cu130": { "name": "torch-cu130-2.13.0", "torch": "2.13.0", "abi": 1,
               "sha256": "<whole archive>", "size": 364499730,
               "parts": [ { "url": ".../mokuro-bunko-0.7.0-x86_64-unknown-linux-gnu-torch-cu130.tar.zst",
                            "sha256": "…", "size": 364499730 } ],
               "external_size": …, "installed_size": … } } }
```

(`backends` is skipped when empty and ignored by older parsers.) `xtask verify`
checks every part and the whole. Trust chain: signature of `release.json` → whole
archive sha256 (and each part's) → `pack.json` inside it → each file's sha256,
including every file taken out of an NVIDIA wheel. The installer refuses files in the
archive that `pack.json` does not list, non-regular entries and unsafe paths.

### `install-ocr`

    mokuro-bunko install-ocr [--variant auto|cpu|cu130|rocm7.1] [--from DIR] [--dir DIR]
                             [--no-models] [--force] [--list]

1. `auto` picks `cu130` for an NVIDIA driver ≥ 580 (`/proc/driver/nvidia/version`,
   `nvidia-smi`; Windows: `nvcuda.dll`), `rocm7.1` for a supported AMD GPU on Linux (KFD
   topology `gfx_target_version`: gfx1030, gfx1100–1102, gfx1200–1201; gfx1031/1032/1034
   too, as the backend sets `HSA_OVERRIDE_GFX_VERSION=10.3.0` itself when it is unset),
   else `cpu` (with a hint when the NVIDIA driver is too old). GPUs hidden from the
   process count as absent: `CUDA_VISIBLE_DEVICES` (NVIDIA), `ROCR_VISIBLE_DEVICES` and
   `HIP_VISIBLE_DEVICES` (or, unset, `CUDA_VISIBLE_DEVICES`) for AMD, set empty or to
   `-1`. 0.5.2's `--backend cuda|rocm|cpu|auto` still works.
2. Fetches **this version's** signed `release.json`
   (`releases/download/v<ver>/release.json`; `MOKURO_BACKEND_MANIFEST` overrides), checks
   the signature with the compiled-in key and that it is the same version, downloads
   the parts into `<storage>/backends/.download/` (resuming), unpacks into
   `.staging-<variant>`, verifies, fetches the `external` wheels from PyPI (resuming,
   sha256-checked) and unpacks just the listed files, creates the links, swaps the
   staging dir into place, removes older packs of the variant and the downloads.
3. Checks the pack's host libraries (`ldconfig -p` + library dirs) and prints the
   package names for what is missing (Debian/Ubuntu and Arch).
4. `models download` (skip with `--no-models`).

`--from DIR` installs from local files (air-gapped hosts, tests): the archive or its
parts, the wheels if present (else PyPI), and `release.json` + `.sig` if present (then
checked exactly like a download; without them only `pack.json`'s checksums are
checked, with a warning). Re-running is a no-op when the pack is complete (sizes);
`--force` re-verifies. `doctor` reports the pack (`OCR backend` line). `install-ocr`
and `models download` then fetch the compiled packages this machine's devices need
(`EnginePipeline::prefetch`). Packs go to `MOKURO_BACKENDS_DIR` when set (the loader
reads the same variable), `MOKURO_TORCH_PACK` pins one pack, `MOKURO_TORCH_MODELS_DIR`
(dev) overrides `<storage>/models/torch`, `MOKURO_TORCH_THREADS` sets the CPU
recognizer threads. On the CPU the default precision is fp32 (0.5.2 parity; bf16 is an
opt-in, packages `linux-cpu-x86_64-v4bf16`). Updating
mokuro-bunko does not update the pack yet: run `install-ocr --force` after an update
that changes the ABI (the loader refuses a pack with another `abi`).

### `xtask torch-pack`

    cargo run -p xtask -- torch-pack --variant cpu|cu130|rocm7.1 [--target T] [--out dist]
        [--cache DIR] [--source <zip|dir|wheel>] [--cdylib FILE | --no-cdylib]
        [--bundle-external] [--keep-all] [--gpu-archs gfx1201,...] [--no-archive]
        [--level 19] [--max-part BYTES] [--locked]

**The closure check only sees the pack's own libraries.** The compiled model packages
(AOTInductor `.so` / Windows `.pyd`) import more: `libtorch.so`, `libgomp.so.1`,
`libtorch_cuda.so`, `libcuda.so.1`; on Windows `cudart64_13.dll` (which the Windows
verifier had to add to the cu130 pack) and `vcomp140.dll`. Rule: whatever a package
imports must be in the pack, the spec's system list or the platform base set; check it
by passing a real package: `xtask torch-pack ... --sample-package <.pt2 | unpacked .pt2
dir | package dir>` (repeatable) adds its libraries' imports to the check (verified:
a sm_89 CUDA package against the cpu pack fails on `libcuda.so.1` / `libtorch_cuda.so`;
the fp32 CPU package against the cpu pack passes). Release builds should pass one
package per pack variant once the packages are published.

Steps: download the pinned libtorch zip (cache: `--cache`, `$BUNKO_PACK_CACHE`, else
`<target>/torch-pack-cache`; `curl -C -` resumes) → unpack `include/` and the kept
`lib/` files (ROCm kernel data trimmed to `--gpu-archs`) → fetch the NVIDIA wheels →
`cargo build -p bunko-torch --features libtorch --lib --release --target T` with
`LIBTORCH` = the unpack (own target dir per variant) → **closure check** (goblin
parses every ELF/PE/Mach-O: each `NEEDED`/import must be in the pack, a wheel, the
spec's `system_libs` or the platform's base set; otherwise the build fails) → SONAME
links → licences (PyTorch's from the cpu torch wheel's dist-info, pinned; the zips
carry none; the wheels' licence files) → `pack.json` → archive. `--no-archive` writes
the installed form to `<out>/<pack name>/` (what the Dockerfiles' source stage uses);
`--bundle-external` puts the NVIDIA files into the pack itself (Docker CUDA image).

What each pack keeps (`crates/xtask/src/torch_specs.rs`) and how it was found:

| variant | from libtorch `lib/` | from NVIDIA wheels | dropped (why) |
|---|---|---|---|
| `cpu` (Linux) | libc10, libtorch, libtorch_cpu, libgomp | — | test libs, libshm, global_deps, headers (LD_DEBUG: only these 4 load) |
| `cu130` (Linux) | + libc10_cuda, libtorch_cuda, libtorch_nvshmem, libcaffe2_nvrtc; **generated stubs** for libcufile.so.0, libnvshmem_host.so.3, libnvJitLink.so.13 | cudart, cublas, cublasLt, nvrtc (+builtins), cupti, cufft, curand, cusparse, **cudnn (dispatcher only)**, cusparseLt, nccl | libtorch_cuda_linalg + cuSOLVER (only torch.linalg loads it), the 7 cuDNN sub-libraries (490 MB: LD_DEBUG on an RTX 4090, hayai-nova and paddle-manga bf16, loads none), nvperf/checkpoint, alt nvrtc, cufftw, nvblas |
| `rocm7.1` (Linux) | libtorch_hip + every ROCm library it needs (HIP, HSA, comgr, rocBLAS, hipBLASLt, MIOpen, RCCL, rocSOLVER, rocSPARSE, rocRAND, rocFFT, roctracer/rocprofiler, aotriton, MAGMA, libdrm); kernel data for gfx1030/1100/1101/1102/1200/1201; `share/libdrm/amdgpu.ids` (below) | — | kernel data for gfx908/90a/942/950/1150/1151 (−3.7 GiB), libnuma/libelf/libdw (LGPL/GPL: taken from the host), libtinfo (unused) |
| `cpu` (Windows) | c10, torch, torch_cpu, libiomp5md, uv | — | torch_python, global_deps, .lib |
| `cu130` (Windows) | + c10_cuda, torch_cuda, caffe2_nvrtc | cublas(Lt), nvrtc (+builtins), cupti **13.0.48** (torch_cpu.dll imports `cupti64_2025.3.0.dll`), cufft, cusolver, cusparse, nvJitLink, cudnn (dispatcher) | the CUDA DLLs the zip bundles (taken from NVIDIA's wheels instead) |
| `cpu` (macOS arm64) | libc10, libtorch, libtorch_cpu, libomp | — | |

The Linux libtorch **cu130 zip does not bundle the CUDA libraries** (unlike Windows):
its RUNPATH points at pip's `nvidia/*/lib`, i.e. it expects torch's PyPI dependencies,
`cuda-toolkit==13.0.3` extras, `nvidia-cudnn-cu13==9.20.0.48`, `-cusparselt-cu13==0.8.1`,
`-nccl-cu13==2.29.7`, `-nvshmem-cu13==3.4.5` (nvjitlink pinned to 13.0.88). Its
libtorch_cpu itself needs `libcupti.so.13` and `libcudart.so.13`, so the CPU half of a
CUDA pack cannot load without them either.

Host libraries the packs expect: glibc ≥ 2.28, libstdc++, libgcc_s (every desktop);
cu130 + libcuda.so.1 (the driver) and zlib; rocm7.1 + libnuma (**including the
unversioned `libnuma.so`**: libtorch_rocshmem dlopen()s that name while loading and
exits the process when it is missing — Arch: numactl; Debian/Ubuntu: libnuma-dev),
libelf, libdw, libzstd, liblzma, libbz2, libatomic, zlib.

### Sizes (measured 2026-10-02)

| pack | archive (zstd 19) | unpacked archive | + from PyPI (download) | installed |
|---|---|---|---|---|
| `cpu` Linux | 75.4 MiB | 416.8 MiB | — | 416.8 MiB |
| `cu130` Linux | 347.7 MiB | 873.8 MiB | 1.65 GiB (1.57 GiB) | 2.50 GiB |
| `rocm7.1` Linux | 3.01 GiB (2 parts) | 6.81 GiB | — | 6.81 GiB |
| `cpu` Windows¹ | — | 294.1 MiB | — | 294.1 MiB |
| `cu130` Windows¹ | — | 685.3 MiB | 1.20 GiB (1.33 GiB) | 1.87 GiB |
| `cpu` macOS arm64¹ | — | 324.5 MiB | — | 324.5 MiB |

¹ staged without the cdylib (cannot be built on this Linux host); closure-checked only.

For comparison, 0.5.2 installed torch + its CUDA/ROCm wheels into a venv (~5 GB CUDA,
~13 GB ROCm). Further cuts, not done: fetch only the needed members of each wheel with
HTTP range requests (cuDNN's 366 MB wheel for a 0.1 MB file; ~-0.4 GiB download for
cu130); stubs for more of what libtorch_cuda links but OCR never calls (cuFFT, cuRAND,
NCCL, cuSPARSELt: ~0.9 GiB) — possible with the same generator, not done: those
libraries are redistributable, and fewer stubs means fewer surprises.

#### Stubs for cuFile, NVSHMEM, nvJitLink (Linux cu130)

What links them (readelf, the cu130 zip + wheels): `libtorch_cuda.so` NEEDS
`libcufile.so.0` but imports **no** symbol from it; `libtorch_nvshmem.so` (NEEDED by
libtorch_cuda) imports 19 `nvshmem*@NVSHMEM` functions from `libnvshmem_host.so.3`;
`libcusparse.so.12` imports 8 `__nvJitLink*_13_0@libnvJitLink.so.13` functions. All
three load at start-up only because of those NEEDED entries (LD_DEBUG on beast showed
them loaded before any OCR). `xtask torch-pack` therefore builds stand-ins
(`Spec::stubs`, `make_stubs`): for each SONAME it collects the functions the pack's
libraries import from it, with their GNU symbol versions (from the importers'
`.gnu.version_r`), and compiles a ~16 KB library with that SONAME and version script
in which every function prints "mokuro-bunko: <fn> (<lib>) was called, but this OCR
backend pack carries a stub for it" and aborts. **Measured (beast RTX 4090, 2026-10-03,
manylinux binary, the real server)**: with the stubs loaded (LD_DEBUG lists them),
hayai-nova bf16 and paddle-manga bf16 each OCRed the 10-page volume (0 failed pages; hayai
1.3 s, paddle 2.8 s) and the abort message never appeared; text 10/10 pages vs 0.5.2
(hayai vs fp32, paddle vs 0.5.2 bf16). Earlier crop runs with the untrimmed pack gave the
same results. The stubs save 0.1 GiB of download and, more to the point, remove the
three NVIDIA libraries the CUDA EULA does not list as redistributable.

### Runtime evidence (pack + its C ABI through `packaging/torch/probe_pack.py`)

| pack | where | engine, precision | crops | result |
|---|---|---|---|---|
| `cpu` | desktop Ryzen 9 7950X, 16 threads | hayai-nova bf16 (x86-64-v3 packages) | 40 | 40/40 = 0.5.2 torch fp32 reference, 19 crops/s |
| `cu130` (installed by `install-ocr --from`, wheels from PyPI) | beast RTX 4090, driver 595.58 | hayai-nova bf16 (sm_89) | 220 | 219/220 vs fp32 ref, 252 crops/s |
| `cu130` | beast RTX 4090 | paddle-manga bf16 | 100 | 86/100 vs fp32 ref (bf16), 80 crops/s |
| `cu130` with stubs, manylinux binary, **real server** | beast RTX 4090, B's sm_89 packages (weightless + shared weights) | hayai-nova bf16 + paddle-manga bf16 layer | 10 pages each | 0 failed; 10/10 pages vs 0.5.2; stubs loaded, never called |
| `rocm7.1` | desktop RX 9070 XT (gfx1201) | hayai-nova bf16 | 220 | 219/220, 89 crops/s; strace: only gfx1201 kernel files read |
| `rocm7.1` | same GPU, **clean debian:trixie-slim container, no ROCm installed** (`/dev/kfd`, `/dev/dri`) | hayai-nova bf16 | 220 | 219/220, 86 crops/s (after `libnuma.so` was provided) |

All runs used `LD_PRELOAD=<pack>/lib/libtorch.so` to work around a loader bug in
bunko-torch (reported to stream A): AOTInductor's model `.so` needs `libtorch.so`,
which the cdylib does not load (`--as-needed`), so ld.so searches the host — it failed
on beast ("libtorch.so: cannot open shared object file") and picked Arch's ROCm
`/usr/lib/libtorch.so` on the desktop. Fix in `bt_init`: dlopen `<lib_dir>/libtorch.so`
RTLD_GLOBAL (or link with `--no-as-needed -ltorch`).

### Licences of what the packs contain (from the files in the distributions)

- **libtorch** (all packs): BSD-3-Clause (`LICENSE` of the torch 2.13.0 wheel's
  dist-info) plus ~100 bundled third-party licence files (`licenses/third_party/…`:
  oneDNN/ideep, sleef, fmt, protobuf, cutlass, composable_kernel, flash-attention,
  aiter, kineto, …). The libtorch zips themselves contain **no** licence file; xtask
  copies them from the pinned CPU wheel into `licenses/pytorch/`.
- **NVIDIA CUDA libraries** (cu130). The `License.txt` in the cudart, cublas, nvrtc,
  cupti, cufft, cufile, curand, cusolver, cusparse, nvjitlink and nvshmem wheels is the
  same file (the CUDA Toolkit EULA, md5 298d545d…). §1.1.1(3) grants distribution of the
  portions "identified in this Agreement as distributable, as incorporated in object
  code format into a software application" meeting §1.1.2: the application has
  "material additional functionality", the distributable portions are "only accessed by
  your application", your terms are consistent with the EULA. §2.3: Linux files may be
  redistributed "provided that the object code files are not modified". **Attachment A**
  (§2.6) lists libcudart, libcufft(w), libcublas/libcublasLt, libnvblas, libcusparse,
  libcusolver, libcurand, NPP, nvJPEG, libnvrtc + libnvrtc-builtins, libnvvm,
  libdevice, libcupti, libnvToolsExt — and **does not list libnvJitLink, libcufile or
  libnvshmem_host**, although those three wheels ship this very EULA. The driver
  library (libcuda) is distributable only inside containers derived from NVIDIA's
  images on Docker Hub/NGC — we never ship it.
  - **cuDNN** (`nvidia_cudnn_cu13…/License.txt`, its own SLA): same §1.1/1.2 terms;
    its supplement §2: "the runtime files .so and .h, cudnn64_7.dll, and cudnn.lib"
    are distributable (the Windows DLL named is the cuDNN 7 one; `cudnn64_9.dll` is not
    named).
  - **cuSPARSELt** (`nvidia/cusparselt/LICENSE.txt`): same SLA terms; "the runtimes files
    ending with .so and .h as part of your application".
  - **NCCL**: BSD-3-Clause (`License.txt`). **NVTX** (not shipped): Apache-2.0.
  - **What we do with this**: release archives contain **no NVIDIA file**; `pack.json`
    lists NVIDIA's own wheels on PyPI and `install-ocr` downloads them from there on the
    user's machine (as 0.5.2's `pip install torch` did), so we redistribute nothing of
    NVIDIA's. The **CUDA Docker image** does redistribute what it bakes in: since
    2026-10-03 that is only libraries the shipped licence texts allow — Attachment A
    (cudart, cuBLAS/cuBLASLt, cuFFT, cuRAND, cuSPARSE, NVRTC + builtins, CUPTI), cuDNN's
    and cuSPARSELt's `.so` runtime files, NCCL (BSD) — each with its licence text under
    `licenses/`. nvJitLink, cuFile and NVSHMEM, which the text does not list, are
    replaced by our stubs (above), so no NVIDIA file without a stated redistribution
    right is in the image. The §1.1.2 conditions (material additional functionality,
    accessed only by our application, consistent terms) still apply; owner sign-off is
    advisable before the first push. Fallback if wanted: `BAKE_PACK=0` +
    `OCR_AUTO_INSTALL=true` (first start downloads from PyPI, nothing NVIDIA in the image). On Windows the libtorch zip itself bundles the CUDA DLLs; the Windows cu130
    pack nevertheless takes them from NVIDIA's wheels, like Linux.
- **AMD ROCm** (rocm7.1 pack): the libtorch rocm7.1 zip contains **no licence file for
  any bundled ROCm library** (no `LICENSE`/`NOTICE`/`COPYING` outside `include/`). The
  bundled ROCm is **7.1.1** (`librocm-core.so`: `7.1.1.0-38`; RCCL 2.27.7; MIOpen 3.5.1;
  comgr from llvm-project `roc-7.1.1`). The texts are now in the repository,
  `packaging/torch/licenses/rocm/`, taken from the upstream sources at tag `rocm-7.1.1`
  (ROCm/rocm-libraries, ROCm/rocm-systems, ROCm/llvm-project, ROCm/rccl), aotriton
  `0.12b`, MAGMA `v2.9.0` and libdrm `libdrm-2.4.124`, and `xtask torch-pack` copies them
  into the pack as `licenses/rocm/` (`SOURCES.md` there maps every pack library to its
  text, URL and sha256). What they are: MIT — HIP, CLR, aqlprofile, rocm-core,
  rocm-smi-lib, rocprofiler-register, rocprofiler-sdk, roctracer, rocBLAS, Tensile,
  hipBLAS, hipBLASLt, hipSPARSELt, hipSOLVER, rocSPARSE, hipSPARSE, rocRAND, hipRAND,
  rocFFT, hipFFT, MIOpen, Composable Kernel, rocRoller, aotriton, libdrm; NCSA
  (University of Illinois) — ROCR-Runtime (HSA), comgr (+ its NOTICES); BSD-style —
  rocSOLVER (BSD-2 + LAPACK notice), RCCL (BSD-3, NCCL-derived, + NOTICES), MAGMA
  (BSD-3). All permissive; no copyleft binary is shipped (libnuma, libelf, libdw stay
  host packages).
- **libdrm's GPU name table** (rocm7.1 pack): the bundled `libdrm_amdgpu.so.1` has
  `/opt/amdgpu/share/libdrm/amdgpu.ids` compiled in; without that file it walks the
  executable's directory looking for it (a stray "(null): No such file or directory",
  +3 s per load, generic GPU names — seen on the fleet). The pack ships the table as
  `share/libdrm/amdgpu.ids` (libdrm `data/amdgpu.ids`, tag `libdrm-2.4.131`; MIT, the
  licence header is in `licenses/rocm/libdrm/`; repo copy in
  `packaging/torch/share/rocm7.1/`, `Spec::share_dir`) and bunko-torch's `bt_init` sets
  `AMDGPU_ASIC_ID_TABLE_PATHS=<pack>/share/libdrm` — a ':'-separated list of
  directories; libdrm ≥ 2.4.130, which the bundled library is, appends `amdgpu.ids`.

### Open items

- bunko-torch must preload `libtorch.so` (above; stream A added this during the run,
  the CPU image's end-to-end test passed with it).
- **Compiled model packages must be portable** (stream B): AOTInductor compiles and
  links the model `.so` with the build machine's toolchain. Built on Arch it needs
  `GLIBC_2.43` (fails on Debian 13: "version `GLIBC_2.43' not found"); built on Debian
  12 (binutils 2.40) it is marked executable-stack (`GNU_STACK RWE`) and glibc ≥ 2.41
  refuses to dlopen it ("cannot enable executable stack"). Build in an old-glibc
  container (≤ 2.28 like libtorch, at most 2.35) **and** link with `-z noexecstack`
  (or clear the PT_GNU_STACK X bit afterwards); check `objdump -T` / `readelf -lW`.
- Failed package loads leave AOTInductor's extraction dirs in `/tmp` (seen: 4 dirs,
  679 MB in the CPU container); bunko-torch should clean them up or load unpacked
  packages in place.
- The ROCm pack is 3 GiB to download (licence texts: done, above).
- Windows and macOS packs: spec + closure check only; the cdylib build and a runtime
  test need those hosts (CI's `packs` job builds them; pimax/Mac to test).
- Range-fetching wheel members (smaller cu130 download); `install-ocr` after an
  in-place update when the ABI changes (today: run `install-ocr --force`).

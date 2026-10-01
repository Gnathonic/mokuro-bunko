# mokuro-bunko 0.7 — packaging, releases and updates

**Status:** living document. Replaces the 0.5.2 packaging listed in
`spec/config-cli-ops.md` §11 (shiv zipapp, Python wheel, uv portable zip,
`setup-windows.ps1` source install, python:3.12 Docker images).

Where things live:

| Path | What |
|---|---|
| `crates/xtask/` | release tooling: `dist`, `manifest`, `sign`, `verify`, `keygen`, `licenses`, `docker-context` |
| `packaging/dist/README.md` | README shipped in unix archives |
| `packaging/windows/` | `run.bat`, `doctor.bat`, `_env.cmd`, `README.txt`, `PORTABLE.txt` for the Windows zip |
| `packaging/docker-init/` | `bunko-init`, the containers' PUID/PGID/UMASK entrypoint (static, libc only; own Cargo workspace) |
| `deploy/docker/` | `Dockerfile.lite`, `Dockerfile` (full, CPU), `Dockerfile.cuda`, `entrypoint.sh` (nginx), per-Dockerfile `.dockerignore` |
| `deploy/nginx-internal.conf.template` | the in-container nginx for the X-Accel offload (now with the processor WebSocket) |
| `deploy/*.service` | systemd units: server and processor, system and user variants |
| `deploy/docker-compose*.yml`, `deploy/unraid/*.xml` | compose files and Unraid templates for the published images |
| `scripts/install.sh`, `scripts/install.ps1` | installers that download a release (`setup-windows.ps1` forwards to `install.ps1`) |
| `.github/workflows/{ci,release,publish}.yml` | CI, tag → draft release, owner's publish step |

Run xtask with `cargo run -p xtask -- <command>`. A `cargo xtask` alias needs
`.cargo/config.toml` with `[alias] xtask = "run -p xtask --"` (not added yet:
outside this task's files).

## 1. Artifacts

One binary, `mokuro-bunko`, in two builds (ARCHITECTURE.md §1). The manifest names a
build by *target triple* and *flavor*:

| flavor | cargo features | contents |
|---|---|---|
| `lite` | `--no-default-features` | server only; OCR by remote processors |
| `full` | `ocr` + the platform's default execution provider | + ONNX Runtime, local OCR, `processor` |
| `full-cuda` | `ocr,cuda` (Windows: `ocr,cuda,directml`) | full with the CUDA execution provider |

The platform default EP for `full` is DirectML on Windows (every prebuilt ONNX Runtime
that ort ships for Windows includes it), CoreML on macOS, CPU on Linux. `--ep webgpu`
would make a `full-webgpu` flavor; none is shipped.

Release matrix (`.github/workflows/release.yml`):

| target | lite | full | full-cuda | how |
|---|---|---|---|---|
| `x86_64-unknown-linux-musl` | ✔ static | | | `cargo zigbuild` |
| `aarch64-unknown-linux-musl` | ✔ static | | | `cargo zigbuild` |
| `x86_64-unknown-linux-gnu` | | ✔ CPU | ✔ CUDA 13 | native, ubuntu-22.04 (glibc ≥ 2.35) |
| `aarch64-unknown-linux-gnu` | | ✔ CPU | — (no ort build) | native, ubuntu-22.04-arm |
| `x86_64-pc-windows-msvc` | ✔ | ✔ DirectML | ✔ CUDA 13 + DirectML | native, windows-latest |
| `aarch64-apple-darwin` | ✔ | ✔ CoreML | | native, macos-latest |
| `x86_64-apple-darwin` | ✔ | — (no ort build) | | cross from macos-latest |
| `aarch64-linux-android` | TODO job (cargo-ndk, off) | | | `vars.BUILD_ANDROID == 'true'` |

Why these choices:

- **Lite on Linux is musl + static**: one binary for every distro, NAS and Pi, and it runs
  on `distroless/static`. mimalloc is the allocator (musl's malloc is too slow).
- **Full on Linux is glibc**: ort publishes no musl ONNX Runtime. Built on the oldest
  supported Ubuntu so the binary runs on glibc ≥ 2.35 (Debian 12, Ubuntu 22.04, RHEL 10).
  A full binary built on a newer distro does not run on older ones (seen locally: an
  Arch-built binary needs `GLIBC_2.38` and fails on Debian 12; `install.sh` catches this
  before replacing anything).
- **ONNX Runtime is linked statically** from ort's prebuilt binaries
  (`download-binaries`; ort-sys rc.13 = ONNX Runtime 1.28). Only execution providers that
  ONNX Runtime loads as plugins are shared libraries: the CUDA provider
  (`libonnxruntime_providers_cuda.so` / `.dll` + `…_providers_shared`), and DirectML's
  `DirectML.dll` when the dist has one. `xtask dist` takes the directory ort-sys linked
  from (its `build-script-executed` message) and bundles those libraries next to the
  executable; TensorRT providers are left out (`--exclude-lib`, unused, very large).
  Linux full builds get an `$ORIGIN` runpath so the provider libraries are found next to
  the real executable even when it is started through a symlink.
- **No macOS universal binary**: ort ships no `x86_64-apple-darwin` ONNX Runtime, so a
  universal full build is impossible, and the updater picks artifacts by target triple
  anyway. Intel Macs get lite (use a processor elsewhere for OCR).
- **CUDA**: ort only ships CUDA 13 builds (`ORT_CUDA_VERSION=13` is set by xtask). Users
  need an NVIDIA driver ≥ 580 and CUDA 13 + cuDNN 9 libraries (the Docker image has them).

Archive name: `mokuro-bunko-<version>-<target>-<flavor>.tar.gz` (`.zip` on Windows),
each with a top directory of the same name holding:

- unix: `mokuro-bunko`, provider libraries if any, `README.md`, `LICENSE`,
  `THIRD-PARTY-LICENSES.md`;
- Windows (this *is* the portable build that replaces 0.5.2's uv zip): `mokuro-bunko.exe`,
  DLLs if any, `run.bat`, `doctor.bat`, `_env.cmd`, `PORTABLE.txt`, `README.txt`,
  `LICENSE.txt`, `THIRD-PARTY-LICENSES.md` (batch files get CRLF line endings).
  With `PORTABLE.txt` present, `run.bat` keeps config/library/logs/models in `data\`
  next to it (0.5.2's portable guarantee: nothing in AppData or the registry).
  `install.ps1` deletes `PORTABLE.txt`, so an installed copy uses
  `%LOCALAPPDATA%\mokuro-bunko`, where 0.5.2 kept its library.

Each archive gets a `<archive>.sha256`; the release also carries `SHA256SUMS`,
`release.json` and `release.json.sig`.

### Licences

`xtask dist` (and `xtask licenses`) walk the `cargo metadata` graph of `mokuro-bunko` for
the exact target and features (normal dependencies only), classify each SPDX expression
(OR = any alternative, AND = all), and write `THIRD-PARTY-LICENSES.md`: a table, the
native components, and every licence/notice file the crates ship, deduplicated by text.
**A copyleft licence (GPL/LGPL/AGPL/SSPL/EUPL/…) with no permissive alternative fails the
build**; unknown licence ids and crates without a licence file are warnings (today:
`asn1-rs-impl` and `yasna` ship no licence file; both MIT/Apache-2.0). CI runs
`xtask licenses` for lite and every full variant. Non-OSS redistributables flagged in the
file: Microsoft's `DirectML.dll` (when bundled). CUDA/cuDNN are never in the archives;
the CUDA Docker image inherits them from NVIDIA's base image under NVIDIA's licence.

## 2. Docker images (`ghcr.io/gnathonic/mokuro-bunko`)

| tag | Dockerfile | base | platforms | size |
|---|---|---|---|---|
| `<ver>-lite`, `latest-lite` | `deploy/docker/Dockerfile.lite` | `gcr.io/distroless/static-debian13` | amd64, arm64 | 29 MB |
| `<ver>`, `latest` | `deploy/docker/Dockerfile` | `debian:trixie-slim` + nginx, tini | amd64, arm64 | 172 MB |
| `<ver>-cuda`, `latest-cuda` | `deploy/docker/Dockerfile.cuda` | `nvidia/cuda:13.0.3-cudnn-runtime-ubuntu24.04` + nginx, tini | amd64 | ~5 GB (base: 2.0 GB compressed) |

(Uncompressed sizes measured locally on 2026-10-01 with the alpha server binary, before
ONNX Runtime is linked into full builds; the CUDA figure is with a stand-in binary and
will grow by the CUDA provider libraries. A running lite container idles at ~16 MiB RSS.)

Each Dockerfile has two binary sources, picked by `--build-arg BIN_FROM=`:
`source` (default; builds with `xtask dist` inside, so `docker build` works from a
checkout) or `prebuilt` (the release workflow: `xtask docker-context` unpacks the signed
release archives into `dist/docker/<amd64|arm64>/<lite|full|cuda>/` plus `bunko-init`,
so the images contain exactly the released binaries and arm64 images need no emulated
compile). `Dockerfile --build-arg FLAVOR=lite` gives a lite server with nginx.

Container contract (kept from 0.5.2, `spec/config-cli-ops.md` §11.2):

- Starts as root, then `bunko-init` drops to `PUID:PGID` (defaults 1000:1000 in `lite`
  and `full` — the uid of 0.5's `mokuro` user — and 99:100 in `cuda`, as 0.5's Unraid
  image), sets `UMASK` (002), chowns the storage and config directories (recursively
  with `TAKE_OWNERSHIP=true`) and execs `mokuro-bunko` (default command `serve`).
  `docker run --user …` works too (PUID/PGID then ignored).
- `MOKURO_*` variables as before. `MOKURO_INSTALL_KIND=docker` is set, so the updater
  only reports the image to pull.
- `MOKURO_NGINX_ACCEL=1|true` (full/cuda images): `entrypoint.sh` renders
  `nginx-internal.conf.template`, starts nginx on `MOKURO_PORT` (master root, workers
  PUID:PGID) and moves the server to `127.0.0.1:MOKURO_BACKEND_PORT` (8081). Default is
  off (0.5's generic image defaulted it on; the async server no longer needs it). The
  lite image has no nginx: `bunko-init` drops the variable with a warning, so a carried
  over `MOKURO_NGINX_ACCEL=1` never produces empty X-Accel responses.
- Retired variables `OCR_AUTO_INSTALL`, `MOKURO_BUNKO_OCR_ENV`,
  `MOKURO_BUNKO_OCR_ENGINES_ENV`, `MOKURO_BUNKO_MOKURO_SPEC` are accepted and ignored.
- `HEALTHCHECK` runs `mokuro-bunko healthcheck` (GET `/api/health`; no curl in the images).
- Labels: `org.opencontainers.image.licenses=MPL-2.0` (0.5 said MIT; spec Q9).

nginx template changes: the `/_processor/` location now passes the WebSocket upgrade
(`Upgrade` + `Connection $connection_upgrade`, mapped so ordinary requests keep upstream
keep-alive) with 3600 s timeouts, for protocol v3's processor socket, and keeps
unbuffered, uncapped `PUT` uploads.

**Production (Unraid, `MOKURO_NGINX_ACCEL=1`)**: switch the template's repository to
`ghcr.io/gnathonic/mokuro-bunko:<ver>-cuda` (or `latest-cuda`). Every existing variable
keeps working; `MOKURO_NGINX_ACCEL=1` keeps nginx. The Unraid host needs driver ≥ 580
for the CUDA 13 image; with OCR on another machine, `:<ver>-lite` (template
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
- **The updater flavor must name the variant**: today `FLAVOR` is `full`/`lite`, so a
  CUDA build would "update" itself to the CPU build. Use `full-cuda` when the `cuda`
  feature is on (`full-webgpu` for `webgpu`; DirectML on Windows and CoreML on macOS are
  the plain `full`). The same string picks `docker[flavor]`.
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

Only verifiable in CI / on real hardware:

- macOS and Windows (MSVC) builds, Linux arm64 full (native arm runner), the CUDA
  variants and the bundling of ONNX Runtime provider libraries (needs the `ocr` feature
  wired), glibc 2.35 compatibility of ubuntu-22.04 builds.
- Multi-arch image builds and pushes, GitHub release creation, Publish's tag moves.
- CUDA EP on a GPU (driver ≥ 580), DirectML/CoreML at runtime, the Windows shortcuts,
  `run.bat` restart loop, and the Android job (disabled TODO).
- Code signing is not done: Windows Authenticode (SmartScreen will warn) and macOS
  notarization (a tarball downloaded by a browser gets quarantined:
  `xattr -d com.apple.quarantine mokuro-bunko`; `curl | sh` installs are not affected).

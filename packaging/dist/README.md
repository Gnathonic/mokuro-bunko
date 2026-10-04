# mokuro-bunko @VERSION@

Self-hosted manga library server for [Mokuro Reader](https://reader.mokuro.app):
WebDAV, a web catalog, multi-user accounts and OCR.

This archive is the **@FLAVOR@** build for `@TARGET@`.

- **lite**: the server only. OCR is done by processors on other machines
  (`mokuro-bunko processor serve` from a full build). Runs in 1 GB of RAM.
- **full**: the server plus local OCR and the `processor` subcommand. Run
  `./mokuro-bunko install-ocr` once: it installs the OCR backend for this machine
  (libtorch for NVIDIA CUDA with driver 580+, AMD ROCm on Linux, or the CPU) into
  the storage directory, and the models.

## Run

```sh
./mokuro-bunko serve          # http://127.0.0.1:8080, first visit creates the admin
./mokuro-bunko --help
./mokuro-bunko doctor         # diagnose problems
```

Config: `~/.config/mokuro-bunko/config.yaml` (`MOKURO_CONFIG` overrides).
Library, database and logs: `~/.local/share/mokuro-bunko` (`MOKURO_STORAGE`).
These are the same locations as mokuro-bunko 0.5, so an existing library is
picked up as it is.

The full Linux build is x86_64 only and needs glibc 2.28 or newer (Debian 11+,
Ubuntu 20.04+, RHEL 8+). arm64 Linux and Intel Macs have the lite build only.

## Install, service, updates

`scripts/install.sh` from the repository installs a release (checksum and
signature checked) into `~/.local/lib/mokuro-bunko` (or `/usr/local/lib`
as root) and can set up a systemd unit:

```sh
curl -fsSL https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.sh | sh -s -- --systemd
```

A self-installed binary updates itself from the admin panel: it downloads the
release, checks the ed25519 signature of `release.json` and the archive's
sha256, swaps the executable and restarts.

Project: https://github.com/Gnathonic/mokuro-bunko · Licence: MPL-2.0 (`LICENSE`) ·
Third-party licences: `THIRD-PARTY-LICENSES.md`.

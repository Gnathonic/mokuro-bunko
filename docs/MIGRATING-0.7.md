# Migrating from 0.5.2 to 0.7

0.7 is a rewrite of the server in Rust. It is a **drop-in over 0.5.2 and
0.5.3 storage** (everything said here about 0.5.2 holds for 0.5.3): point it at the same `config.yaml`, `mokuro.db` and library tree
and it carries on. Readers (Mokuro Reader) keep working against the same
WebDAV and JSON APIs. What changes is how it is installed, which OCR engines
exist, and how OCR processors talk to the library.

## Before you start

- Back up the storage directory (`mokuro.db`, `config.yaml` if it lives
  there, and ideally the library). `mokuro-bunko config path` prints where
  they are.
- Plan to update **processors at the same time**. A 0.5.2 processor cannot
  talk to a 0.7 library (see [Processors](#processors)).
- Decide on a build. **Lite** is the server only (OCR by remote processors)
  and fits a 1 GB VPS. **Full** adds local OCR and the `processor` command; run
  `mokuro-bunko install-ocr` once after installing it (as with 0.5.2). The Linux full
  build is x86_64 only and runs on glibc 2.28+ (Debian 11+, Ubuntu 20.04+, RHEL 8+).
  **arm64 Linux gets the lite build only** for now (0.5.2's pip install ran OCR there):
  OCR for an arm64 library comes from a processor on an x86_64 (or Windows/macOS)
  machine.
  See the [README](../README.md#editions).

## What carries over unchanged

| Thing | Notes |
|---|---|
| `config.yaml` | Same location and format. Every 0.5.2 key still loads. |
| `mokuro.db` | Opened and upgraded in place by 0.5.3's own idempotent steps (a 0.5.2 database gains 0.5.3's `catalog_folders` table; its `catalog_series` is left as it was). |
| Library tree | `library/`, `inbox/`, `users/`, sidecars (`.mokuro`, `.<name>.mokuro`), `.webp` covers, compiled `series.json` / `catalog.json`. |
| Storage defaults | `~/.local/share/mokuro-bunko` (Linux, macOS), `%LOCALAPPDATA%\mokuro-bunko` (Windows); config in `~/.config/mokuro-bunko/config.yaml`. |
| Accounts, invites, tokens, audit log | In the database. Passwords and bearer tokens keep working. |
| OCR state files | `.ocr-failures.json`, `.ocr-bench.json`, `.ocr-congestion.json` and `processors/*.json` keep their formats. |
| Environment variables | `MOKURO_<SECTION>_<KEY>`, `MOKURO_HOST`, `MOKURO_PORT`, `MOKURO_STORAGE`, `MOKURO_CONFIG` and `MOKURO_NGINX_ACCEL` work as before. |
| Admin panel, catalog, queue page | The same web pages. |

## What changed

### The mokuro engine is gone; the new primary is hayai-nova

0.7 ships only Apache-2.0 OCR components: `hayai-nova`, `paddle-manga` and
`ppocr-manga`. The `mokuro` engine (manga-ocr and its GPL
comic-text-detector) was removed, together with the `ctd`, `animetext` and
`rtdetr` detectors.

Your config keeps loading, and the server migrates it when it reads it
(each change is logged at start and shown in the admin panel):

- A generation row on the `mokuro` engine is kept, **retired**: it never
  runs, it loses `primary`, and its name stays reserved. When it is saved
  back to `config.yaml` it is written disabled.
- When that leaves no runnable primary, a `hayai-nova` primary row is added
  (detector `ppocr-manga`). If you already had a `hayai-nova` layer row, that
  row becomes the primary instead.
- A row on a removed detector (`ctd`, `animetext`, `rtdetr`) is switched to
  the `ppocr-manga` detector.
- A config with no `ocr.generations` at all gets the default: one
  `hayai-nova` primary row.

**Your existing `.mokuro` files are kept and served.** A bare
`<Volume>.mokuro` written by mokuro counts as complete, so nothing re-runs
OCR on a volume that already has one. Only volumes **without** a primary
sidecar are read, by the new primary, and only new uploads get new-engine
text. The two kinds of file look the same to readers; new files record the
engine that wrote them in an `ocr_engine` block.

#### Replacing old OCR (optional, off by default)

To bring the whole library to the new engine, turn on the generation
upgrade:

```yaml
ocr:
  upgrade:
    enabled: true
    replace: [mokuro-legacy, mokuro]   # which kinds of existing primary sidecar to replace
```

(set it in `config.yaml`; there is no environment variable for the nested
key). For every volume whose primary
sidecar was written by an older recipe the server either reuses an existing
layer made by the new engine, or queues an upgrade job behind all ordinary
jobs. The new file replaces the old one outright: the old OCR is not kept
(add a non-primary generation if you want another recipe's OCR beside the
primary). Volumes whose archive is missing pages, or whose sidecar a person
has edited, are skipped and listed; an admin can force one edited volume. Reading progress is untouched because the `volume_uuid` is
preserved. It is off by default because it rewrites what readers see and
costs OCR time.

### Processors

Remote processors speak **protocol v3**: one WebSocket per processor
(`/_processor/<id>/socket`) plus plain `PUT`s for results. 0.5.2 processors
(protocol v2) and 0.7 libraries refuse each other at registration. Update
the library and every processor to 0.7 together, and install the **full**
build on processor machines (then `mokuro-bunko install-ocr` into the processor's
storage, see [deployment](deployment.md#3-the-processor-machine)); they no longer
need Python, uv, git or a checkout. Your `processor.yaml` keeps working (its `ocr:` section is ignored).
Processor accounts, names and per-machine profiles are unchanged.

**Reverse proxies must pass the WebSocket upgrade for `/_processor/`.**
Caddy does this on its own. For nginx add `proxy_set_header Upgrade
$http_upgrade;`, `proxy_set_header Connection $connection_upgrade;` (with a
`map` for `$connection_upgrade`) and a long `proxy_read_timeout` on that
location; the bundled `deploy/nginx-internal.conf.template` already does.
Request buffering off and no body size limit for that location still apply.
See [Remote OCR processors behind a proxy](deployment.md#remote-ocr-processors-behind-a-proxy).

### OCR: `install-ocr` installs a backend pack instead of a Python environment

The recognizers still run on torch — libtorch 2.13, the C++ half of the torch 0.5.2
used — but without Python, pip or venvs. `mokuro-bunko install-ocr` (same command
name as 0.5.2) now downloads a **backend pack** for the hardware it finds into
`<storage>/backends/`: `cu130` for NVIDIA GPUs (driver 580 or newer, as 0.5.2's
CUDA 13 torch needed), `rocm7.1` for supported AMD GPUs on Linux, or `cpu`. As in
0.5.2, `ocr.backend` steers it: `auto` follows the hardware, `cpu` stays on the CPU
with a GPU present, `cuda`/`rocm` pick that GPU (or fall back to `cpu` with a hint when
it is not there). Every
file is checked against the signed release manifest; NVIDIA's CUDA libraries come
from NVIDIA's own packages on PyPI, as they did with pip. On the CPU, OCR runs in fp32
as in 0.5.2 (bf16 is an opt-in). `MOKURO_BACKENDS_DIR` moves the packs elsewhere. `--backend cuda|rocm|cpu|auto`
still works (it maps to `--variant cu130|rocm7.1|cpu|auto`); `--engines` and
`--detector` are ignored. Unlike 0.5.2 it fails visibly (non-zero exit) when it cannot
install. The full Docker image does the same on its first start (0.5.2's server
installed its OCR environment on start too); the image itself carries no pack.

The models are downloaded on first use into `<storage>/models/` (or up front by
`install-ocr` / `mokuro-bunko models download`) and verified by sha256. The old
`.ocr-env`, `.ocr-engines-env`, `.pip-cache`, Hugging Face cache and torch
downloads are no longer used and can be deleted.

### Environment variables

| 0.5.2 | 0.7 |
|---|---|
| `MOKURO_THREADS` (request threads, default 50) | Not read. The server is async; `server.threads` (default `min(cores, 4)`) sets the worker threads. |
| `MOKURO_BUNKO_OCR_ENV`, `MOKURO_BUNKO_OCR_ENGINES_ENV`, `MOKURO_BUNKO_MOKURO_SPEC` | Accepted by the Docker entrypoint and ignored. |
| `OCR_AUTO_INSTALL` | The OCR backend is installed by the server itself, in the background, when local OCR needs it (`MOKURO_OCR_AUTO_INSTALL`, default on). `true` keeps that on; `false` (the 0.5.2 Unraid template's default) is noted and ignored, because in 0.5.2 it never stopped the server from installing OCR on start. |
| (new) `MOKURO_OCR_AUTO_INSTALL` | Full Docker image, default `true`. `false` turns the install on start off: local OCR then has no backend until `mokuro-bunko install-ocr` is run. |
| `MOKURO_PPOCR_MODELS`, `MOKURO_PPOCR_DOWNLOAD` | Still honoured as aliases of `MOKURO_MODELS_DIR` (a directory of model files to use) and `MOKURO_MODELS_DOWNLOAD` (`0` forbids downloads). |
| `MOKURO_DEBUG`, `MOKURO_EFT_TRACE` | Not read. Use `MOKURO_LOG` (a log filter such as `debug`, or `info,bunko_server=debug`) or `-v`. |
| `MOKURO_PPOCR_THREADS` / `_SIDE` / `_TILE` / `_PRECISION`, `MOKURO_OCR_STAGE_*` | Not read. Pools and devices are per-row settings (`pools`). |

New: `server.threads`, `server.cache_mb`, `update.check`, `update.channel`,
`update.manifest_url` (each also settable as `MOKURO_<SECTION>_<KEY>`;
`update.channel` defaults to `auto`, which follows the installed build: a
pre-release install sees later pre-releases, a stable install only stable
releases) and
`ocr.upgrade.*` (`config.yaml` only). `ocr.sessions` is accepted and ignored;
`ocr.backend` keeps 0.5.2's values (`auto`, `cuda`, `rocm`, `cpu`, `skip`).
The 0.5 refusals of `ocr.engines`, `ocr.detector`, `ocr.patch_budget` and
`ocr.char_map` still apply.

### Docker images

Images are published as `ghcr.io/gnathonic/mokuro-bunko`:

| Tag | What | Replaces |
|---|---|---|
| `latest`, `<ver>` | Server + local OCR + nginx for downloads; amd64. The OCR backend for the GPU the container is given (none, NVIDIA, AMD) is downloaded on first start | the generic `deploy/Dockerfile` image and the `unraid-cuda` image (`Dockerfile.unraid`) |
| `latest-cuda`, `<ver>-cuda` | The same image under the CUDA image's name, so existing templates keep working | the `unraid-cuda` image |
| `latest-lite`, `<ver>-lite` | Server only, no OCR, no nginx (29 MB, ~16 MiB RSS idle); amd64 + arm64 | new |

`PUID`, `PGID`, `UMASK`, `TAKE_OWNERSHIP`, `MOKURO_CONFIG` and the other
`MOKURO_*` variables work as before. Defaults: PUID/PGID 1000:1000 (the Unraid
template passes 99:100, as before); `MOKURO_CONFIG` defaults to
`/data/config.yaml` (the Unraid template sets `/config/config.yaml`, as before; set it
yourself if you ran the 0.5.2 CUDA image without the template and keep `config.yaml`
in `/config`). Differences:

- The first start installs the OCR backend pack for the GPU the container sees into
  `/data/backends` (~100 MB for the CPU, ~2 GB for NVIDIA, ~3 GB for AMD) and the
  enabled engines' models into `/data/models`, instead of 0.5.2's pip-installed torch
  environments; later starts reuse them. There is no `nvidia/cuda` base any more (the
  CUDA libraries come with the pack), but an NVIDIA GPU still needs the driver (≥ 580)
  and the container toolkit on the host. **AMD GPUs now work in Docker**:
  `--device /dev/kfd --device /dev/dri`.
- `MOKURO_NGINX_ACCEL` now defaults to **off** in the images (the async
  server does not need it for throughput). Set it to `1` to keep nginx in
  front (useful when nginx serves downloads). The lite image has no nginx and
  ignores it with a warning.
- The container health check is `mokuro-bunko healthcheck`; there is no curl
  or Python in the images. The image licence label now says MPL-2.0.

Unraid: switch the template's repository to `ghcr.io/gnathonic/mokuro-bunko:beta` while
0.7 is in beta, `:latest` from 0.7.0 on (`latest-cuda` is the same image; templates in
`deploy/unraid/`). No volume or variable needs to change. The server comes up at once and
downloads the OCR backend (~2 GB for NVIDIA) in the background, where 0.5 made the web UI
wait for its OCR environment; local OCR starts by itself when it is done, and a failed
download is shown in the admin panel with a Retry button instead of stopping anything. For an AMD
GPU replace `--runtime=nvidia` in Extra Parameters with
`--device=/dev/kfd --device=/dev/dri`. The README's [Docker section](../README.md#docker)
has the full list of volumes, variables and GPU prerequisites.

### Installing and updating

There is no Python package any more. Install from a release archive,
`scripts/install.sh`, `scripts/install.ps1`, the Windows portable zip, a
Docker image, or build from source (see the [README](../README.md)).
Tarball and Windows installs update themselves from the admin panel's
Updates card (signed manifest, checksum, one click); Docker installs are told
which image to pull.

### Library paths are case-insensitive (as since 0.5.3)

As on Windows, `kingdom/` and `Kingdom/` are one folder: a request in any case
reaches the spelling already on disk, an upload spelled in another case lands in the
existing folder, and a folder or file can be renamed to fix its case. Coming straight
from 0.5.2 on a case-sensitive host (Linux, Docker), check the library for folders
whose names differ only in case (`ls library | sort -f | uniq -di`): both stay
listed in the catalog, but a request in a third spelling reaches only one of them,
and the library cannot be copied to Windows or macOS as it is. Merge them while the
server is stopped (move the volumes into one folder; the next metadata pass picks it
up). Upload ownership is keyed by path, so a merged volume needs its
`volume_uploads`, `ocr_sidecars` and `volume_identities` rows renamed too, or
re-uploaded by its uploader.

### First-run setup from another computer

This only matters for a new library (an upgraded one already has its admin). 0.5
let the setup page create the admin only from the server machine itself
(`Setup is only allowed from localhost`); in Docker or on a NAS that meant
`docker exec … mokuro-bunko admin add-user` or `mokuro-bunko setup`. 0.7 keeps
localhost working without anything more and adds one way in:

- The **setup code**: while no admin exists, the server prints a new one-time code
  at each start (`First run: create the admin account at http://…/setup (setup code:
  XXXXX-XXXXX)`). Opened from another computer, `/setup` asks for it, then shows the
  same wizard. The code works until an admin exists, a few tries a minute per address (a flood of wrong codes replaces it with a new one in the log rather than locking setup).

### Behaviour fixes you may notice

0.5.2 quirks that were fixed rather than ported (full list in the
[changelog](../CHANGELOG.md)): WebDAV `MOVE`/`COPY`/`DELETE` that used to wipe
data are refused (403/409), the config commands no longer write environment
overrides into `config.yaml`, and the server stops cleanly on SIGTERM
(`docker stop`). Archives named with an upper-case extension (`Vol 1.CBZ`), which
0.5 listed but never made a cover or OCR for, get both after the upgrade.

## Rolling back

0.5.3 and 0.5.2 can still open the database 0.7 has touched (same schema text, row
formats and JSON spellings; 0.5.2 finds its own `catalog_series` table as it left it,
or creates it, and refreshes it on its first pass), and the library files are ordinary files. To
go back:

1. Stop 0.7 (and its processors).
2. Restore your backup if you want the exact old state, or keep the current
   storage.
3. Remove keys 0.5.2 does not know from `config.yaml`: `server.threads`,
   `server.cache_mb` and `ocr.upgrade` (0.5.2 refuses unknown keys inside a
   section; 0.7 writes them only when you changed them from the defaults).
   An `update:` section is ignored by 0.5.2.
4. A migrated config has a `hayai-nova` primary row and a disabled `mokuro`
   row. 0.5.2 loads that; re-enable the `mokuro` row and make it primary
   again if you want mokuro back as the primary engine.
5. Start 0.5.2, with 0.5.2 processors. Sidecars 0.7 wrote (and any layers the
   upgrade kept) are normal `.mokuro` files that 0.5.2 serves.

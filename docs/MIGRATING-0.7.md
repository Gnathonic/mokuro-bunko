# Migrating from 0.5.2 to 0.7

0.7 is a rewrite of the server in Rust. It is a **drop-in over 0.5.2
storage**: point it at the same `config.yaml`, `mokuro.db` and library tree
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
| `mokuro.db` | Opened and upgraded in place by 0.5.2's own idempotent steps. |
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
jobs. The old file is kept beside the new one as a layer named after the old
generation, so nothing is lost and a volume can be reverted. Volumes whose
archive is missing pages, or whose sidecar a person has edited, are skipped
and listed. Reading progress is untouched because the `volume_uuid` is
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
CUDA 13 torch needed), `rocm7.1` for supported AMD GPUs on Linux, or `cpu`. Every
file is checked against the signed release manifest; NVIDIA's CUDA libraries come
from NVIDIA's own packages on PyPI, as they did with pip. On the CPU, OCR runs in fp32
as in 0.5.2 (bf16 is an opt-in). `MOKURO_BACKENDS_DIR` moves the packs elsewhere. `--backend cuda|rocm|cpu|auto`
still works (it maps to `--variant cu130|rocm7.1|cpu|auto`); `--engines` and
`--detector` are ignored. Unlike 0.5.2 it fails visibly (non-zero exit) when it cannot
install. The Docker images have their pack built in.

The models are downloaded on first use into `<storage>/models/` (or up front by
`install-ocr` / `mokuro-bunko models download`) and verified by sha256. The old
`.ocr-env`, `.ocr-engines-env`, `.pip-cache`, Hugging Face cache and torch
downloads are no longer used and can be deleted.

### Environment variables

| 0.5.2 | 0.7 |
|---|---|
| `MOKURO_THREADS` (request threads, default 50) | Not read. The server is async; `server.threads` (default `min(cores, 4)`) sets the worker threads. |
| `MOKURO_BUNKO_OCR_ENV`, `MOKURO_BUNKO_OCR_ENGINES_ENV`, `MOKURO_BUNKO_MOKURO_SPEC` | Accepted by the Docker entrypoint and ignored. |
| `OCR_AUTO_INSTALL` | Docker images: `true` runs `install-ocr --no-models` before the server starts (a no-op when the image has its pack built in). |
| `MOKURO_PPOCR_MODELS`, `MOKURO_PPOCR_DOWNLOAD` | Still honoured as aliases of `MOKURO_MODELS_DIR` (a directory of model files to use) and `MOKURO_MODELS_DOWNLOAD` (`0` forbids downloads). |
| `MOKURO_DEBUG`, `MOKURO_EFT_TRACE` | Not read. Use `MOKURO_LOG` (a log filter such as `debug`, or `info,bunko_server=debug`) or `-v`. |
| `MOKURO_PPOCR_THREADS` / `_SIDE` / `_TILE` / `_PRECISION`, `MOKURO_OCR_STAGE_*` | Not read. Pools and devices are per-row settings (`pools`). |

New: `server.threads`, `server.cache_mb`, `update.check`, `update.channel`,
`update.manifest_url` (each also settable as `MOKURO_<SECTION>_<KEY>`) and
`ocr.upgrade.*` (`config.yaml` only). `ocr.sessions` is accepted and ignored;
`ocr.backend` keeps 0.5.2's values (`auto`, `cuda`, `rocm`, `cpu`, `skip`).
The 0.5 refusals of `ocr.engines`, `ocr.detector`, `ocr.patch_budget` and
`ocr.char_map` still apply.

### Docker images

Images are published as `ghcr.io/gnathonic/mokuro-bunko`:

| Tag | What | Replaces |
|---|---|---|
| `latest`, `<ver>` | Server + OCR on the CPU (CPU backend pack built in) + nginx for downloads; amd64 | the generic `deploy/Dockerfile` image |
| `latest-cuda`, `<ver>-cuda` | As above with the CUDA backend pack built in (NVIDIA driver 580 or newer, NVIDIA container toolkit); amd64 | the `unraid-cuda` image (`Dockerfile.unraid`) |
| `latest-lite`, `<ver>-lite` | Server only, no OCR, no nginx (29 MB, ~16 MiB RSS idle); amd64 + arm64 | new |

`PUID`, `PGID`, `UMASK`, `TAKE_OWNERSHIP`, `MOKURO_CONFIG` and the other
`MOKURO_*` variables work as before. Defaults: PUID/PGID 1000:1000 (99:100 in
the CUDA image, as the Unraid image had). Differences:

- OCR works without a first-start install: nothing is pip-installed into `/data` any
  more (0.5.2 put its torch environments there); only the models are downloaded,
  into `/data/models`, on first use. The CUDA image no longer derives from
  `nvidia/cuda` — the CUDA libraries are in the pack — so it is smaller, but still
  needs the NVIDIA driver (≥ 580) and container toolkit on the host.
- `MOKURO_NGINX_ACCEL` now defaults to **off** in the images (the async
  server does not need it for throughput). Set it to `1` to keep nginx in
  front, as the Unraid production setup does. The lite image has no nginx and
  ignores it with a warning.
- The container health check is `mokuro-bunko healthcheck`; there is no curl
  or Python in the images. The image licence label now says MPL-2.0.

Unraid: switch the template's repository to
`ghcr.io/gnathonic/mokuro-bunko:latest-cuda` (templates in `deploy/unraid/`).
No volume or variable needs to change. The README's [Docker section](../README.md#docker)
has the full list of volumes, variables and GPU prerequisites.

### Installing and updating

There is no Python package any more. Install from a release archive,
`scripts/install.sh`, `scripts/install.ps1`, the Windows portable zip, a
Docker image, or build from source (see the [README](../README.md)).
Tarball and Windows installs update themselves from the admin panel's
Updates card (signed manifest, checksum, one click); Docker installs are told
which image to pull.

### Behaviour fixes you may notice

0.5.2 quirks that were fixed rather than ported (full list in the
[changelog](../CHANGELOG.md)): WebDAV `MOVE`/`COPY`/`DELETE` that used to wipe
data are refused (403/409), the config commands no longer write environment
overrides into `config.yaml`, and the server stops cleanly on SIGTERM
(`docker stop`).

## Rolling back

0.5.2 can still open the database 0.7 has touched (same schema text, row
formats and JSON spellings), and the library files are ordinary files. To
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

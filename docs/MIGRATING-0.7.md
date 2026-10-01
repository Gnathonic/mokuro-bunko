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
  and fits a 1 GB VPS. **Full** adds local OCR and the `processor` command.
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
build on processor machines; they no longer need Python, uv, git or a
checkout. Your `processor.yaml` keeps working (its `ocr:` section is ignored).
Processor accounts, names and per-machine profiles are unchanged.

**Reverse proxies must pass the WebSocket upgrade for `/_processor/`.**
Caddy does this on its own. For nginx add `proxy_set_header Upgrade
$http_upgrade;`, `proxy_set_header Connection $connection_upgrade;` (with a
`map` for `$connection_upgrade`) and a long `proxy_read_timeout` on that
location; the bundled `deploy/nginx-internal.conf.template` already does.
Request buffering off and no body size limit for that location still apply.
See [Remote OCR processors behind a proxy](deployment.md#remote-ocr-processors-behind-a-proxy).

### OCR models are downloaded, not installed

`mokuro-bunko install-ocr` is deprecated. OCR no longer uses Python
environments. The command still exists so existing scripts and container
entrypoints do not break: it always exits 0 and, on a full build, just runs
`models download`. The models are ONNX files downloaded on first use into
`<storage>/models/` and verified by sha256 (`mokuro-bunko models download` to
fetch them up front). The old `.ocr-env`, `.ocr-engines-env`, Hugging Face
cache and torch downloads are no longer used and can be deleted.

### Environment variables

| 0.5.2 | 0.7 |
|---|---|
| `MOKURO_THREADS` (request threads, default 50) | Not read. The server is async; `server.threads` (default `min(cores, 4)`) sets the worker threads. |
| `MOKURO_BUNKO_OCR_ENV`, `MOKURO_BUNKO_OCR_ENGINES_ENV`, `MOKURO_BUNKO_MOKURO_SPEC`, `OCR_AUTO_INSTALL` | Accepted by the Docker entrypoint and ignored. |
| `MOKURO_PPOCR_MODELS`, `MOKURO_PPOCR_DOWNLOAD` | Still honoured as aliases of `MOKURO_MODELS_DIR` (a directory of model files to use) and `MOKURO_MODELS_DOWNLOAD` (`0` forbids downloads). |
| `MOKURO_DEBUG`, `MOKURO_EFT_TRACE` | Not read. Use `MOKURO_LOG` (a log filter such as `debug`, or `info,bunko_server=debug`) or `-v`. |
| `MOKURO_PPOCR_THREADS` / `_SIDE` / `_TILE` / `_PRECISION`, `MOKURO_OCR_STAGE_*` | Not read. Pools and devices are per-row settings (`pools`). |

New: `server.threads`, `server.cache_mb`, `update.check`, `update.channel`,
`update.manifest_url` (each also settable as `MOKURO_<SECTION>_<KEY>`) and
`ocr.upgrade.*` (`config.yaml` only). `ocr.sessions` is accepted and ignored;
`ocr.backend` now names an ONNX Runtime execution provider (`auto`, `cuda`,
`rocm` (alias of `webgpu`), `webgpu`, `directml`, `coreml`, `cpu`, `skip`).
The 0.5 refusals of `ocr.engines`, `ocr.detector`, `ocr.patch_budget` and
`ocr.char_map` still apply.

### Docker images

Images are published as `ghcr.io/gnathonic/mokuro-bunko`:

| Tag | What | Replaces |
|---|---|---|
| `latest-lite`, `<ver>-lite` | Server only, no OCR, no nginx (29 MB, ~16 MiB RSS idle) | new |
| `latest`, `<ver>` | Server + CPU OCR + nginx for downloads | the generic `deploy/Dockerfile` image |
| `latest-cuda`, `<ver>-cuda` | As above with the CUDA execution provider (NVIDIA driver 580 or newer) | the `unraid-cuda` image |

`PUID`, `PGID`, `UMASK`, `TAKE_OWNERSHIP`, `MOKURO_CONFIG` and the other
`MOKURO_*` variables work as before. Defaults: PUID/PGID 1000:1000 (99:100 in
the CUDA image, as the Unraid image had). Two differences:

- `MOKURO_NGINX_ACCEL` now defaults to **off** in the images (the async
  server does not need it for throughput). Set it to `1` to keep nginx in
  front, as the Unraid production setup does. The lite image has no nginx and
  ignores it with a warning.
- The container health check is `mokuro-bunko healthcheck`; there is no curl
  in the images. The image licence label now says MPL-2.0.

Unraid: switch the template's repository to
`ghcr.io/gnathonic/mokuro-bunko:latest-cuda` (templates in `deploy/unraid/`).
No volume or variable needs to change.

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

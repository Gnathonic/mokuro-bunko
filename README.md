# Mokuro Bunko

A self-hosted manga library server with WebDAV, built-in OCR processing, and multi-user support. Designed as a backend for [Mokuro Reader](https://reader.mokuro.app).

> [!WARNING]
> **v0.5 -- Alpha.** Core functionality works and installation is automated (one-command Windows setup, self-contained portable build, install verification via `mokuro-bunko doctor`), but rough edges remain. No binary releases, PyPI packages or Docker images are published yet -- install from source, use the setup script, or build the portable zip or a Docker image yourself.

## What it does

- Serves a shared manga library over WebDAV so Mokuro Reader can connect directly
- Tracks per-user reading progress (each user gets their own progress files transparently)
- Compiles the reader's metadata files (`series.json`, `catalog.json`) server-side from the library itself, merging in client-submitted series facts (titles, links, tags), and recompiles within seconds of an upload
- Runs OCR automatically on uploaded manga (CUDA, ROCm, or CPU): [mokuro](https://github.com/kha-white/mokuro) by default, plus optional extra OCR layers from other engines (hayai-nova, PaddleOCR-VL, PP-OCRv6 — the last also reads scanned novels)
- Can hand OCR to a stronger computer: a remote processor logs in to the library and works the queue, and each volume goes to whichever machine will finish it first
- Manages users with role-based permissions (anonymous browse, registered, uploader, editor, inviter, admin)
- Provides a web catalog for browsing the library — display-title language options, sorting (A–Z / newest / wordiest / rating), genre filters, and ratings/tags fetched from AniList/MAL for linked series (no API key required) — plus an admin panel for user/config management
- Asks search engines and crawlers to stay out (`X-Robots-Tag: noindex` on every response, deny-all `robots.txt`)

See [CHANGELOG.md](CHANGELOG.md) for release history.

## Quick start

### Windows — one command

Paste into PowerShell (no prerequisites — installs everything, verifies it,
starts the server, and opens your browser to finish setup):

```powershell
powershell -c "irm https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/setup-windows.ps1 | iex"
```

GPU (NVIDIA CUDA) OCR is detected and set up automatically; CPU is the fallback.

### Windows — portable folder

Prefer a no-install version? Build (or download, once released) the portable
zip, extract it anywhere, and double-click `run.bat`. Everything — Python,
OCR engine, your library, config, and logs — stays inside the folder. Move it
by copying the folder; uninstall by deleting it.

```powershell
.\scripts\build-portable.ps1   # produces dist\mokuro-bunko-portable-windows-x64.zip
```

### Manual (Linux / macOS / Windows)

Requires [uv](https://docs.astral.sh/uv/) (it provisions Python 3.12 itself):

```bash
git clone https://github.com/Gnathonic/mokuro-bunko.git
cd mokuro-bunko
uv sync
uv run mokuro-bunko serve   # first browser visit walks you through setup
```

Optional: `uv run mokuro-bunko setup` for the interactive console wizard, and
`uv run mokuro-bunko install-ocr` to install OCR up front instead of on first
launch. OCR installs need `git` on the machine (the optimized mokuro fork is
fetched from GitHub).

### Docker

See [docs/deployment.md](docs/deployment.md) and [`deploy/`](deploy/) for
building and running the Docker images (a generic one and a CUDA one, with an
Unraid template), systemd units and reverse-proxy examples.

**Something not working?** Run `uv run mokuro-bunko doctor` — it checks your
Python, GPU driver, OCR stack, disk space, and port, with fix hints. See
[docs/troubleshooting.md](docs/troubleshooting.md).

## Configuration

On first run, `mokuro-bunko setup` walks you through creating an admin account and writing a config file. After that, edit `config.yaml` directly or use the admin panel at `/_admin`.

Copy [`config.example.yaml`](config.example.yaml) for a documented starting point. Key settings:

| Setting | Default | Description |
|---------|---------|-------------|
| `server.port` | `8080` | Listen port |
| `storage.base_path` | `~/.local/share/mokuro-bunko` | Library and database location |
| `registration.mode` | `self` | `disabled`, `self`, `invite`, or `approval` |
| `ocr.backend` | `auto` | `auto`, `cuda`, `rocm`, `cpu`, or `skip` (no OCR on this machine) |
| `ocr.generations` | one `mokuro` row | The OCR recipes every volume gets a layer from, in run order |
| `ocr.local_processing` | `true` | `false` leaves all OCR to remote processors |
| `catalog.enabled` | `false` | Web-based library browser |
| `catalog.enrich_community` | `true` | Fetch ratings/tags/genres from AniList/MAL for linked series |

Every key can also be set from the environment (`MOKURO_<SECTION>_<KEY>`, e.g. `MOKURO_OCR_BACKEND`), plus the shortcuts `MOKURO_HOST`, `MOKURO_PORT`, `MOKURO_STORAGE` and `MOKURO_CONFIG`. The full reference is [docs/configuration.md](docs/configuration.md).

## OCR

Mokuro Bunko manages isolated Python environments for OCR, so the heavy ML
stack stays separate from the server: one for mokuro, and a second one for
the other engines, created only when a configured OCR generation needs it.

```bash
mokuro-bunko install-ocr                 # auto-detect best backend
mokuro-bunko install-ocr --backend cuda  # force a specific backend
mokuro-bunko install-ocr --list-backends # show what's available
mokuro-bunko install-ocr --engines hayai-nova,paddle-manga,ppocr-manga
```

The server scans the library in the background and OCRs every volume that is
missing a sidecar. Results (`.mokuro` overlay files and `.webp` thumbnails)
are placed alongside the source volumes. What runs is a list of **OCR
generations** (`ocr.generations`, editable in the admin panel): each is a
named recipe — engine, text detector, reading resolution, and how it uses the
hardware — that writes its own layer, `<Volume>.mokuro` for the primary one
and `<Volume>.<name>.mokuro` for the others, in the order you list them. Each
row can be benchmarked and tuned on your own pages from the admin panel.

OCR can also run on another machine. Create an account with the `processor`
role on the library (`mokuro-bunko admin add-user gpu-box --role processor`),
then on the machine with the GPU (with `git`, its driver and uv installed):

```bash
git clone https://github.com/Gnathonic/mokuro-bunko.git && cd mokuro-bunko
uv sync
uv run mokuro-bunko processor setup   # checks the account, installs, starts it
```

It dials out to the library, so nothing has to be opened on it. See the
[deployment walkthrough](docs/deployment.md#remote-ocr-processors) (Linux
and Windows) and [remote OCR processors](docs/configuration.md#remote-ocr-processors).

The installer manages Python packages only -- CUDA/ROCm drivers must be installed on the host. Every install is smoke-tested automatically so a broken OCR environment fails loudly at install time instead of silently at OCR time.

Failures are visible: volumes that fail OCR appear on the Queue page (`/queue`) with a reason and retry count, full per-volume logs land in `<storage>/logs/ocr/`, and the server log is `<storage>/logs/server.log`. Failed volumes are retried with exponential backoff.

How the OCR pipeline, scheduling and benchmarks work is described in [docs/ocr-internals.md](docs/ocr-internals.md).

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
mokuro-bunko serve          # start the server
mokuro-bunko setup          # first-time setup wizard
mokuro-bunko doctor         # diagnose install/OCR problems (PASS/WARN/FAIL + hints)
mokuro-bunko install-ocr    # install/reinstall OCR environment
mokuro-bunko admin          # users and invites (add-user, list-users, change-role, set-password, restore-user, generate-invite, ...)
mokuro-bunko config         # view/edit config (show, set, init, path, cors-add, cors-remove)
mokuro-bunko processor      # run this machine as a remote OCR processor (setup, install, serve, service, status)
mokuro-bunko ssl            # manage SSL certificates
mokuro-bunko tunnel         # cloudflare tunnel management
mokuro-bunko dyndns         # dynamic DNS management
```

## Development

```bash
uv sync --extra dev
uv run pytest               # run tests
uv run ruff check src tests # linting
uv run mypy src             # type checking
```

## License

[Mozilla Public License 2.0](LICENSE)

# Mokuro Bunko

A self-hosted manga library server with WebDAV, built-in OCR processing, and multi-user support. Designed as a backend for [Mokuro Reader](https://reader.mokuro.app).

> [!WARNING]
> **v0.2 -- Alpha.** Core functionality works and installation is now automated (one-command Windows setup, self-contained portable build, install verification via `mokuro-bunko doctor`), but some features remain untested and rough edges remain. No binary releases or Docker images are published yet -- use the setup script or build the portable zip from source.

## What it does

- Serves a shared manga library over WebDAV so Mokuro Reader can connect directly
- Tracks per-user reading progress (each user gets their own progress files transparently)
- Runs [mokuro](https://github.com/kha-white/mokuro) OCR automatically on uploaded manga (CUDA, ROCm, or CPU)
- Manages users with role-based permissions (anonymous browse, registered, uploader, editor, admin)
- Provides a web catalog UI for browsing the library and an admin panel for user/config management

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
launch.

### Docker

See [docs/deployment.md](docs/deployment.md) and [`deploy/`](deploy/) for
Docker/Compose (including a CUDA image for Unraid).

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
| `ocr.backend` | `auto` | `auto`, `cuda`, `rocm`, `cpu`, or `skip` |
| `catalog.enabled` | `false` | Web-based library browser |

Environment variable overrides: `MOKURO_HOST`, `MOKURO_PORT`, `MOKURO_STORAGE`, `MOKURO_CONFIG`.

## OCR

Mokuro Bunko manages an isolated Python environment for OCR dependencies (PyTorch + mokuro). This keeps the heavy ML stack separate from the server itself.

```bash
mokuro-bunko install-ocr                # auto-detect best backend
mokuro-bunko install-ocr --backend cuda # force a specific backend
mokuro-bunko install-ocr --list-backends # show what's available
```

When OCR is enabled, the server watches for new uploads and processes them in the background. Results (`.mokuro` overlay files and `.webp` thumbnails) are placed alongside the source volumes.

The installer manages Python packages only -- CUDA/ROCm drivers must be installed on the host. Every install is smoke-tested automatically (imports, CUDA availability, tokenizer-stack version pins) so a broken OCR environment fails loudly at install time instead of silently at OCR time.

Failures are visible: volumes that fail OCR appear on the Queue page (`/queue`) with the error and retry count, full per-volume logs land in `<storage>/logs/ocr/`, and the server log is `<storage>/logs/server.log`. Failed volumes are retried with exponential backoff.

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

## CLI reference

```
mokuro-bunko serve          # start the server
mokuro-bunko setup          # first-time setup wizard
mokuro-bunko doctor         # diagnose install/OCR problems (PASS/WARN/FAIL + hints)
mokuro-bunko install-ocr    # install/reinstall OCR environment
mokuro-bunko admin          # user management (create, delete, list, set-role)
mokuro-bunko config         # view/edit config
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

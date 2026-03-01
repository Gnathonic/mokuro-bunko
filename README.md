# Mokuro Bunko

A self-hosted manga library server with WebDAV, built-in OCR processing, and multi-user support. Designed as a backend for [Mokuro Reader](https://reader.mokuro.app).

> **v0.1 -- Early alpha.** Core functionality works but many features are untested or incomplete. Expect rough edges. No binary releases or Docker images are published yet -- run from source for now.

## What it does

- Serves a shared manga library over WebDAV so Mokuro Reader can connect directly
- Tracks per-user reading progress (each user gets their own progress files transparently)
- Runs [mokuro](https://github.com/kha-white/mokuro) OCR automatically on uploaded manga (CUDA, ROCm, or CPU)
- Manages users with role-based permissions (anonymous browse, registered, uploader, editor, admin)
- Provides a web catalog UI for browsing the library and an admin panel for user/config management

## Quick start

```bash
git clone https://github.com/Gnathonic/mokuro-bunko.git
cd mokuro-bunko
uv sync
uv run mokuro-bunko setup   # interactive first-time config
uv run mokuro-bunko serve
```

Requires Python 3.10+ and [uv](https://docs.astral.sh/uv/).

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

The installer manages Python packages only -- CUDA/ROCm drivers must be installed on the host.

## User roles

| Role | Browse | Download | Upload | Invite | Edit/Delete | Admin |
|------|--------|----------|--------|--------|-------------|-------|
| Anonymous | configurable | configurable | -- | -- | -- | -- |
| Registered | yes | yes | -- | -- | -- | -- |
| Uploader | yes | yes | yes | -- | own uploads | -- |
| Inviter | yes | yes | yes | yes | own uploads | -- |
| Editor | yes | yes | yes | -- | all | -- |
| Admin | yes | yes | yes | yes | all | yes |

Inviter and Editor are separate capability branches, not a strict hierarchy. Inviters can create invite codes but can only edit/delete their own uploads. Editors can edit/delete any file but cannot create invites.

## CLI reference

```
mokuro-bunko serve          # start the server
mokuro-bunko setup          # first-time setup wizard
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

# Mokuro Bunko

A self-hosted manga library server with WebDAV, built-in OCR processing, and multi-user support. Designed as a backend for [Mokuro Reader](https://reader.mokuro.app).

> [!WARNING]
> **v0.1 -- Early alpha.** Core functionality works but many features are untested or incomplete. Expect rough edges. No binary releases or Docker images are published yet -- run from source for now.

## What it does

- Serves a shared manga library over WebDAV so Mokuro Reader can connect directly
- Tracks per-user reading progress (each user gets their own progress files transparently)
- Runs [mokuro](https://github.com/kha-white/mokuro) OCR automatically on uploaded manga (CUDA, ROCm, or CPU)
- Manages users with role-based permissions (anonymous browse, registered, uploader, editor, admin)
- Provides a web catalog UI for browsing the library and an admin panel for user/config management
- Exposes health and stats endpoints for monitoring

## Changelog

### 0.1.2

- **OCR history.** A persistent log of OCR events is written to `.ocr-history.jsonl`. Query it via `GET /queue/api/ocr/history` with optional filters for status, series, and time range.
- **OCR worker control.** Admins can pause and resume the OCR worker via `POST /queue/api/ocr/control`. Worker state (active/paused) is exposed in `/queue/api/ocr` and `/queue/api/status`.
- **OCR timeouts.** New config options: `ocr.hard_timeout_seconds`, `ocr.no_progress_timeout_seconds`, `ocr.finalizing_timeout_seconds`.
- **Queue UI overhaul.** The queue page now has job filtering, OCR history view, and worker control buttons.
- **Request rate limiting.** All endpoints are throttled at 120 requests per 60 seconds per IP. Excess requests receive a `429 Too Many Requests` response.
- **Admin write rate limiting.** State-changing admin endpoints (POST/PUT/DELETE) have their own rate limiter and return `Retry-After`, `X-RateLimit-Limit`, and `X-RateLimit-Window` headers.
- **CSRF protection on admin.** Admin state-changing requests validate the `Origin`/`Referer` header against the server origin.
- **Upload quota.** New config option `quota.uploads_per_day` limits how many volumes a user can upload in a 24-hour window. Uploads over quota return `429`.
- **Path traversal protection.** All WebDAV and virtual paths are normalised (resolves `..`, strips backslashes) before any access check, blocking path traversal attacks.
- **Anonymous access restrictions.** Anonymous users can no longer read or write `/mokuro-reader/volume-data.json`, `/mokuro-reader/profiles.json`, or any path under `/inbox`.
- **Configurable admin path.** The admin panel path is now set via `admin.path` in config instead of being hardcoded to `/_admin`.
- **Atomic file uploads.** Uploads are written to a temp file first, then renamed into place, preventing readers from seeing partial files.
- **WebDAV write locks.** Concurrent writes to the same path are serialised; a second writer receives a lock error rather than racing.
- **Database retry logic.** SQLite `database is locked` errors are retried with exponential backoff. New config options: `database.connect_timeout_seconds`, `database.busy_timeout_ms`, `database.connect_retries`, `database.retry_initial_delay_seconds`.
- **Startup validation.** The server checks storage writability and SSL prerequisites before accepting connections.
- **Security headers.** All JSON and static responses now include: `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `X-Frame-Options: DENY`, `X-XSS-Protection: 1; mode=block`.
- **Health and stats endpoints.** `GET /api/health` returns server status, uptime, database connection status, and user count. `GET /api/stats` returns live library and usage statistics.
- **Trusted proxy IP detection.** `X-Forwarded-For` is only trusted when the request comes from a loopback or private IP, preventing IP spoofing via the header.
- **Config validation.** `mokuro-bunko config check` now validates SSL cert/key files and checks config relationships.

### 0.1.1

- **Fix: 503 Service Unavailable on Windows.** Cheroot worker threads can die from unhandled Windows socket errors ([cheroot#375](https://github.com/cherrypy/cheroot/issues/375), [cheroot#710](https://github.com/cherrypy/cheroot/issues/710)), eventually leaving zero threads to process requests. Added a thread pool watchdog that detects dead threads and replaces them, plus a resilient serve loop that recovers from the interrupt flag a dying thread sets.
- **Fix: Windows compatibility.** `os.rename()` replaced with `os.replace()` for atomic file moves (Windows fails if the destination exists). `os.umask()`/`os.chmod()` guarded on Windows where they have no effect on NTFS.
- **Fix: SQLite concurrency.** Enabled WAL journal mode, `busy_timeout`, and longer connection timeout to prevent database locking under concurrent access.
- **Audit logging for account self-deletion.** Deleting your own account now logs a `self_delete_account` audit event before removal.
- **Soft-delete users.** Deleting a user sets status to `deleted` instead of removing the row, preserving audit trail and upload ownership records.
- **30-day audit log retention.** Audit entries older than 30 days are automatically pruned.
- **Debug request logging.** Set `MOKURO_DEBUG=1` to log every request with thread name, method, path, status code, and timing to stderr.

### 0.1.0

- Initial release.

## Quick start

```bash
git clone https://github.com/MokuroEnjoyer/mokuro-bunko.git
cd mokuro-bunko
uv sync
uv run mokuro-bunko setup   # interactive first-time config
uv run mokuro-bunko serve
```

Requires Python 3.10+ and [uv](https://docs.astral.sh/uv/).

## Configuration

On first run, `mokuro-bunko setup` walks you through creating an admin account and writing a config file. After that, edit `config.yaml` directly or use the admin panel.

Copy [`config.example.yaml`](config.example.yaml) for a documented starting point. Key settings:

| Setting | Default | Description |
|---------|---------|-------------|
| `server.port` | `8080` | Listen port |
| `storage.base_path` | `~/.local/share/mokuro-bunko` | Library and database location |
| `registration.mode` | `self` | `disabled`, `self`, `invite`, or `approval` |
| `ocr.backend` | `auto` | `auto`, `cuda`, `rocm`, `cpu`, or `skip` |
| `ocr.hard_timeout_seconds` | `3600` | Max time for a single OCR job |
| `ocr.no_progress_timeout_seconds` | `600` | Kill job if no progress for this long |
| `catalog.enabled` | `false` | Web-based library browser |
| `admin.path` | `/_admin` | URL path for the admin panel |
| `quota.uploads_per_day` | `null` | Max volumes a user can upload per day (`null` = unlimited) |
| `database.busy_timeout_ms` | `5000` | SQLite busy timeout |
| `database.connect_retries` | `3` | Retries on database lock errors |

Environment variable overrides: `MOKURO_HOST`, `MOKURO_PORT`, `MOKURO_STORAGE`, `MOKURO_CONFIG`.

Additional docs:
- [`docs/configuration.md`](docs/configuration.md)
- [`docs/deployment.md`](docs/deployment.md)
- [`docs/middleware-stack.md`](docs/middleware-stack.md)
- [`docs/release-readiness.md`](docs/release-readiness.md)

## OCR

Mokuro Bunko manages an isolated Python environment for OCR dependencies (PyTorch + mokuro). This keeps the heavy ML stack separate from the server itself.

```bash
mokuro-bunko install-ocr                # auto-detect best backend
mokuro-bunko install-ocr --backend cuda # force a specific backend
mokuro-bunko install-ocr --list-backends # show what's available
```

When OCR is enabled, the server watches for new uploads and processes them in the background. Results (`.mokuro` overlay files and `.webp` thumbnails) are placed alongside the source volumes. OCR history is persisted and queryable via the queue page or API.

The installer manages Python packages only -- CUDA/ROCm drivers must be installed on the host.

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
mokuro-bunko install-ocr    # install/reinstall OCR environment
mokuro-bunko admin          # user management (create, delete, list, set-role)
mokuro-bunko config         # view/edit config
mokuro-bunko config check   # validate current configuration
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

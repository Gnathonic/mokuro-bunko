# Changelog

## [0.1.5] - 2026-06-29

### Added
- **nginx X-Accel-Redirect download offload.** When `MOKURO_NGINX_ACCEL=1`, library file downloads are served by nginx via `sendfile()` instead of holding a cheroot worker thread for the whole transfer — the main driver of 503 / thread-pool exhaustion under download load (cf. 0.1.1 watchdog). `MokuroFileResource` emits `X-Accel-Redirect` for GETs of library files (the redirect path is confined to the library root and URL-encoded), returns an empty body, drops `Content-Length`, and delegates `Range` handling to nginx; the internal nginx location is aliased to the library root only and marked `internal`. Enabled by default in the generic Docker image (`deploy/Dockerfile`). On the Unraid image it is **opt-in**: set `MOKURO_NGINX_ACCEL=1` and nginx fronts the public port while Python moves to `MOKURO_BACKEND_PORT` (default 8081). Cheroot thread count is now configurable via `MOKURO_THREADS` (default 50).

### Changed
- **Simplified mokuro OCR invocation.** Dropped the redundant `--output-dir` flag (mokuro writes sidecars next to the input regardless) and removed the now-dead compatibility-fallback retry path. Raised the finalizing-phase timeout from 180s to 900s so large volumes are no longer killed during mokuro's finalize step.

## [0.1.4] - 2026-06-11

### Fixed
- **Silent anonymous downgrade on non-UTF-8 Basic auth.** Legacy clients (browser `btoa`, the npm `base-64` package) encode `username:password` as Latin-1 bytes; such headers failed UTF-8 decoding and were silently served as anonymous read-only — users with non-ASCII passwords could browse but never sync or upload, with no error. Any present-but-undecodable `Authorization` header (Latin-1 bytes, invalid base64, missing colon) now returns 401 with a `charset="UTF-8"` challenge. Credentials must be UTF-8 encoded (mokuro-reader ≥1.6.2 complies); requests without any `Authorization` header remain anonymous as before.
- `WWW-Authenticate` challenges now advertise `charset="UTF-8"` (RFC 7617) so compliant clients encode credentials as UTF-8.
- Login, account, and setup pages now build Basic-auth strings with a UTF-8-safe encoder instead of bare `btoa`.

### Added
- `/login/api/me` identity endpoint extended: reports `authenticated` (boolean, present in every response) and a `permissions` object (`canWriteProgress`, `canAddFiles`, `canModifyDelete`); returns `200` with `authenticated: false` for credential-less requests instead of 401, and is rate-limited against credential stuffing. Existing `username`/`role`/`created_at` keys are preserved.

## [0.1.2] - 2026-03-09

### Added
- WebDAV MOVE/COPY support for files and folders in the library
- Rename files and folders directly from WebDAV clients (e.g. mokuro-reader)
- OCR sidecar files (.mokuro, .mokuro.gz, .webp, .nocover) are moved alongside renamed CBZ volumes
- Folder moves are atomic with recursive volume upload tracking updates
- Audit logging for move and copy operations

## [0.1.1] - 2026-02-28

### Fixed
- **503 Service Unavailable on Windows.** Cheroot worker threads can die from unhandled Windows socket errors ([cheroot#375](https://github.com/cherrypy/cheroot/issues/375), [cheroot#710](https://github.com/cherrypy/cheroot/issues/710)), eventually leaving zero threads to process requests. Added a thread pool watchdog that detects dead threads and replaces them, plus a resilient serve loop that recovers from the interrupt flag a dying thread sets.
- **Windows compatibility.** `os.rename()` replaced with `os.replace()` for atomic file moves (Windows fails if the destination exists). `os.umask()`/`os.chmod()` guarded on Windows where they have no effect on NTFS.
- **SQLite concurrency.** Enabled WAL journal mode, `busy_timeout`, and longer connection timeout to prevent database locking under concurrent access.

### Added
- Audit logging for account self-deletion
- Soft-delete users (sets status to `deleted` instead of removing the row, preserving audit trail and upload ownership records)
- 30-day audit log retention with automatic pruning
- Debug request logging (set `MOKURO_DEBUG=1` to log every request with thread name, method, path, status code, and timing)

## [0.1.0] - 2026-02-25

- Initial release

# Changelog

## [0.2.0] - 2026-07-11

### Fixed
- **Fresh OCR installs were broken (transformers 5.x).** `install-ocr` installed `transformers` unpinned; 5.x cannot instantiate manga-ocr's tokenizer, so a brand-new install silently produced thumbnails but never `.mokuro` files. `install_mokuro()` now installs `mokuro "transformers>=4.25,<5" sentencepiece`, and every install ends with an in-env smoke test (`verify_installation()`: imports torch/transformers/sentencepiece/manga-ocr/mokuro, checks the version pin, reports CUDA availability) so a broken environment fails loudly at install time. Existing broken envs: `mokuro-bunko install-ocr --force`.
- **Silent per-volume OCR failures.** mokuro exits 0 even when a volume fails, and its output was piped to DEVNULL — failures were invisible and retried every poll interval forever. Now: mokuro's combined output is captured to `<storage>/logs/ocr/<volume>.log`; exit-0 failures are detected from mokuro's own `Processed successfully: N/M` summary; a short error reason is extracted from the log (final traceback line / loguru ERROR / "No module named"); failures are persisted to `<storage>/.ocr-failures.json` with attempt counts and retried with exponential backoff (poll×4^attempts, capped at 1h). Replacing a failed `.cbz` (newer mtime) resets its record.
- Repo now ships `.python-version` (3.12) so `uv sync` always provisions a CUDA-compatible interpreter (CUDA wheels are unavailable on Python ≥3.13; the installer refuses CUDA there).
- shiv binary dep lists: added missing `click` to `scripts/build-binary.sh` and `Pillow` to both build-binary.sh and the release workflow.

### Added
- **`mokuro-bunko doctor`** — diagnostics command printing a PASS/WARN/FAIL table with fix hints: Python version, config/storage writability, NVIDIA driver, OCR env + full stack smoke test, disk space, port availability, failed-volume count. Exit 1 on FAIL (scriptable).
- **Windows one-command installer** (`scripts/setup-windows.ps1`): `irm .../setup-windows.ps1 | iex` downloads the source (no git needed), installs uv, syncs, installs OCR (GPU auto-detected), runs doctor, creates a start script + desktop shortcut, starts the server and opens the browser to the first-run wizard. PowerShell 5.1 compatible, no admin, idempotent, transcript in `%TEMP%\mokuro-bunko-setup.log`.
- **Portable Windows edition** (`scripts/build-portable.ps1` → `dist/mokuro-bunko-portable-windows-x64.zip`): extract anywhere and double-click `run.bat`; bundled uv.exe bootstraps Python + OCR into the folder on first run. All state (runtime, models, config, library, logs) stays inside the folder — move by copying, uninstall by deleting. Includes `doctor.bat` and a plain-English README.txt.
- **Persistent logging.** Rotating server log at `<storage>/logs/server.log` (the `logging` module is now actually configured; previously watcher.py's log calls were dropped and nothing was persisted). OCR worker/installer output goes through loggers instead of bare prints.
- **Queue page shows failures.** New "Failed" section listing each failing volume with its error, attempt count, and log path; failed volumes no longer masquerade as "pending". `/queue/api/status` gains a `failed` array.
- `/api/health` gains an `ocr` section: backend, worker liveness (heartbeat file), pending and failed counts.
- `docs/troubleshooting.md`; README quick-start rewritten around the new install paths.
## [0.1.8] - 2026-07-08

### Fixed
- **Sporadic `502 Bad Gateway` on WebDAV renames (and other requests) under the nginx X-Accel download offload.** With `MOKURO_NGINX_ACCEL=1`, `MokuroFileResource` served every library download (`.mokuro`/`.webp`/`.cbz`) with an empty body and *dropped* the `Content-Length` header. WsgiDAV force-closes any keep-alive response that has a body-bearing status but no `Content-Length` (`wsgidav_app.py` `_start_response_wrapper`), so every single library GET tore down its upstream connection — logged as `Missing required Content-Length header in 200-response: closing connection` (thousands per hour). That churn poisoned nginx's `keepalive` upstream pool, and a reused-then-closed socket surfaced to the reader as a sporadic `502` on unrelated requests such as volume renames. The offload response now sends `Content-Length: 0` (the Python body genuinely is empty; nginx overrides it with the real file size when it serves the file), keeping the upstream connection reusable. Verified against a real nginx + cheroot harness: the full file is served whether upstream sends `Content-Length: 0`, the real size, or none.

## [0.1.6] - 2026-06-29

### Fixed
- **CORS broken on nginx-offloaded library downloads (regression from 0.1.5).** Two independent faults in the in-container nginx both surfaced as `Access-Control-Allow-Origin`-missing errors in the reader:
  - **Missing CORS on every successful download.** nginx does not carry the upstream (Python) response headers onto the file it serves via the `X-Accel-Redirect` internal redirect, so the `Access-Control-Allow-Origin` that `CorsMiddleware` set was dropped and *every* `200`/`206` library download failed the browser's cross-origin check. CORS is now re-attached on the internal library location (reflecting the request `Origin`; only shared, anonymously-served library files reach that location, so reflection exposes nothing and avoids duplicating Python's allowlist in nginx).
  - **CORS-less `503` under load.** The internal nginx also throttled requests per-IP (`limit_req`/`limit_conn`); the reader's normal burst of concurrent thumbnail GETs tripped the limit, and nginx returned the `503` *itself* — before the request reached Python — with no CORS header. Removed the per-IP throttle: abuse control belongs to the front proxy, and X-Accel-Redirect already keeps downloads off Python's thread pool (the thread-exhaustion the limit was meant to prevent).
  - As a safety net, errors nginx generates itself (e.g. backend down/timeout) now route through a CORS-bearing error handler, so a real failure surfaces to the reader as a readable `503` instead of an opaque cross-origin block. Responses on the `location /` proxy path are untouched and keep Python's own CORS header (no duplication).

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

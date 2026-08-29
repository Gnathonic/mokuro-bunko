# Changelog

## [0.3.4] - 2026-08-29

### Fixed
- **Renames over HTTPS always failed with 502.** The internal nginx overwrote the front proxy's `X-Forwarded-Proto: https` with its own plain-http scheme, so wsgidav's MOVE/COPY destination check ("source and destination must have the same scheme") rejected every rename arriving through a TLS-terminating edge — deterministically, since the deployment's first day. The header now passes through, falling back to the local scheme only for direct connections.

## [0.3.3] - 2026-08-29

### Fixed
- **Long metadata passes no longer starve the whole server.** A full compile pass held the metadata lock for minutes on a cold library; every incoming `series.json` update parked on it, the worker thread pool jammed, and every request — renames, covers, even OPTIONS — failed at the proxy with 502 until the pass ended (observed live as failing renames and silently dropped link/title edits). The pass now takes the lock per series with an explicit fair handoff, full passes stay mutually exclusive, and an update that still cannot get the lock within 10s is answered `503 Retry-After` instead of pinning a thread. Shutdown aborts a running pass between series instead of waiting out the crawl.

## [0.3.2] - 2026-08-29

### Changed
- **JSON metadata is gzip-compressed for transport.** `catalog.json` and `series.json` downloads shrink ~10x; the reader refreshes the catalog on every listing by design, so this applies to every refresh. Archives and images are untouched.

## [0.3.1] - 2026-08-29

### Added
- **Instant enrichment on link.** A metadata update that introduces or changes a series' AniList/MAL id fetches its rating, tags and genres within seconds, instead of waiting for the hourly sweep (which remains the refresh and catch-all).

## [0.3.0] - 2026-08-28

### Added
- **Catalog page overhaul.** Series cards can render display titles as a language progression — Native (native → romaji → english → folder), English (english → romaji → native → folder), or plain folder names (default: Native) — with the user's series tag appended to alt-title renders ("よつばと！ (HD Scan)"). Sort by A–Z, Newest (latest archive mtime), Wordiest (characters per page), or Rating; filter by genre chips; search matches every title variant, tag and genre. Grid controls hide inside a series view, and backing out restores search, filter and scroll position.
- **Community enrichment from AniList/MAL.** A background fetcher fills ratings, tags and genres for every series whose client-submitted facts carry an external id: AniList primary (batched GraphQL, no key needed), Jikan for MAL-only links, scores normalized to one 0–100 scale, refreshed weekly. `catalog.enrich_community` (default `true`) switches it off.
- **Materialized catalog database.** Each series' render-ready row (folder, cover, volume count, latest archive mtime, page/character totals) is kept in a `catalog_series` table, upserted by the same passes that compile metadata and re-synced by a 6-hour periodic full pass. The catalog listing is now a single database read — no filesystem walk on the request path.
- **Volume uploads trigger recompilation.** Client uploads of volume files (`.cbz`, `.mokuro`, covers) schedule a per-series recompile through the filesystem watcher, so `series.json` and `catalog.json` follow uploads within seconds instead of waiting for a quiet window. Both debounces are capped so a sustained upload stream can no longer starve compilation. Metadata passes log `[METADATA]` fire/done lines.
- **Crawler opt-out.** Every response carries `X-Robots-Tag: noindex, nofollow`, and `/robots.txt` (served outside auth) disallows all compliant crawlers.
- **Server-side compilation of the reader's metadata files.** mokuro-bunko now compiles `<Series>/series.json` (v2: series facts plus an index of the series' volumes — uuid, title, page and character counts, mokuro version, spine width, archive size, freshness stamps, shelf offsets) and a root `catalog.json` (name/mapping/search data for every series folder) from the library's own `.mokuro` and `.cbz` files. Both are regenerated when the library changes and served with accurate size/mtime, so clients can cache them; a rebuild that changes nothing rewrites nothing. Character counts are computed with the reader's own counting rules, and volumes with no OCR sidecar are indexed as image-only with the uuid the reader derives for them.
- **Metadata updates from accounts that cannot write to the library.** A `series.json` PUT is treated as an update REQUEST: the facts fields are validated, merged newest-stamp-wins against the server's store (a factless payload never clears a link unless it is strictly newer — an explicit unlink), and both files are regenerated. Shelf offsets ride along as index data and never move the facts stamp. Repeating an identical update is a no-op, so a client can retry safely.
- **Cover sidecars are generated even when OCR is disabled** (`backend: skip`), since the reader now installs them onto volumes it has not downloaded.
- **Freshness stamps on each volume entry.** `series.json` volume entries optionally carry `mokuro_size`/`mokuro_modified` and `cover_size`/`cover_modified` — integer byte sizes and integer epoch seconds from a plain `stat()` of the `.mokuro` sidecar and the cover `.webp`, omitted (not `null`) when either doesn't exist. Clients use these to detect a stale local copy without downloading anything: a size mismatch, or a strictly newer `_modified` than what they have stored, means re-fetch.
- **`/login/api/me` reports metadata write scope.** `permissions.metadata` is `all`, `owned` (with an `ownedSeries` list), or `none`, so the reader can show only the series.json edits an account may actually submit.

### Fixed
- **Metadata updates for non-ASCII series titles were always rejected.** PEP 3333 delivers `PATH_INFO` latin-1-encoded and wsgidav re-encodes it only inside the DAV app, beneath the interception middleware — which parsed the mojibake spelling, failed folder resolution, and answered 400 for every Japanese-titled `series.json` PUT (observed: 102 rejections in one upload session, with clients retrying up to 23 times per series). The middleware now applies the same re-encode to its own copy of the path.
- **Sidecar uploads no longer create volume ownership.** A cover/mokuro PUT onto an untracked volume (legacy content, or anything predating ownership tracking) used to insert an ownership row — a blind sidecar backfill could hand an uploader edit and delete rights over series they never made. Ownership now comes only from uploading the archive.
- **Catalog listing payload cut ~40×.** The root listing no longer nests every volume of every series (~3 MB of JSON at 1,000-series scale that the grid never rendered) and JSON API responses gzip when the client accepts it.

### Changed
- Compiled metadata files are owned by the server: `catalog.json` cannot be written by any account, and neither compiled file can be deleted, moved or copied. Rejections are ordinary 403s — a client that treats metadata writes as best-effort keeps full read/write access to everything else.
- **Cache-Control on cover and page image responses.** Image GETs now send `Cache-Control: private, max-age=86400`, letting browsers cache them instead of re-fetching on every page turn; `series.json`/`catalog.json` keep `no-store`.

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

# Server analytics (0.8)

Status: built, 2026-11-04. Branch `0.8/analytics` (from `0.8/remote-storage`). This
document now describes what was built: where the implementation chose differently from
the first draft, the text says what it does and why (marked **Built:**).

The admin wants to know how the server is used: what is downloaded and read, by whom,
how much bandwidth each kind of traffic costs, how healthy the server is, and whether
anyone is crawling the whole catalog. Nothing like this exists today: no counters, no
byte accounting, no per-IP request tracking. This document is the contract the
implementation follows; section 11 assigns the work.

**WebDAV is first-class.** One listener and one router serve both the web pages and
WebDAV, so the accounting layer sees both. Every metric below covers WebDAV requests
(any method: PROPFIND, GET, HEAD, PUT, DELETE, MKCOL, COPY, MOVE, LOCK, UNLOCK,
PROPPATCH, OPTIONS) as well as the web UI and JSON APIs, and every traffic table
carries `protocol` and `method` so WebDAV can be broken down by method. Generic WebDAV
clients (rclone, davfs2, the Windows mini-redirector, Finder, Cyberduck, gvfs, ...) are
recognised in the client mix and are subject to crawl detection like anyone else.

## 1. Goals

The metrics the owner asked for, and the extras they chose:

| # | Metric | Source |
|---|---|---|
| 1 | Most downloaded series (all time, last week/month, trending) | archive GETs, per (principal, volume, day) |
| 2 | Most read series (readers, reads, minutes, pages) | `positions.json` and `history/**` uploads |
| 3 | Bandwidth graphs (hourly/daily, by category / protocol / method) | response and request body bytes |
| 4 | Bandwidth by user (anonymous visitors by pseudonymous id) | same, per principal |
| 5 | Bytes spent on progress / profile sync | categories `progress_sync`, `profile_sync` |
| 6 | Bytes spent on navigation / metadata | category `metadata`, by detail |
| 7 | Process memory (RSS, peak), CPU, connections | host sampler |
| 8 | Series file types | library scan |
| 9 | IPs crawling the whole catalog: episodes, how often per IP | crawl detector |
| 10 | Reading time and pages read | decoded `history/<device>/*.events` |
| 11 | Active users (DAU/WAU/MAU), devices per account, last seen | principals + device ids |
| 12 | Completion funnel per series | `volume-data.json` |
| 13 | Library growth, uploads per day per uploader, dormant series | daily snapshot + upload events |
| 14 | Errors (status codes) and failed logins per IP | status + auth failures |
| 15 | Latency p50/p95/p99 per request kind, slowest requests | timing histograms |
| 16 | Transfer efficiency: 304 savings, gzip savings, aborted / resumed downloads, peak connections, CPU | body accounting + host sampler |
| 17 | Client mix: hosted vs self-hosted reader, Bunko web UI, WebDAV clients, browser, OS | `Origin`, `User-Agent` |
| 18 | Page image formats inside archives | zip central directory (local libraries only) |
| 19 | Rough geography (country per IP) | DB-IP Country Lite, downloaded on demand, opt-in |

Crawlers are **detected and reported only**: no blocking, no throttling, no behaviour
change.

Non-goals: outbound cloud-backend traffic (the owner did not pick it; `bunko-remote`'s
governor keeps 24 h of it already), OCR throughput history, any client-side telemetry,
any change to what the reader sends.

## 2. Constraints

- **Hot path.** The per-request cost is a classification (string work on the path), one
  short `parking_lot::Mutex` critical section when the response body ends, and no I/O.
  No SQLite on the request path. Target: < 5 µs median overhead per request; the
  interop catalog budgets (`interop/budgets.toml`) must still pass.
- **Memory.** The lite build must run in 1 GB. Every in-memory structure is bounded
  (caps in section 4.5). Worst case analytics state ≤ 32 MB.
- **No unbounded buffering.** Request bodies are never teed into memory; per-user files
  are read back from disk after they are written (section 6).
- **Storage.** A separate SQLite file, `analytics.sqlite`, beside the main database
  (same directory as `database.path` resolves to), WAL, `synchronous=NORMAL`. It never
  contends with the main writer, can be deleted to reset analytics, and does not bloat
  the main DB. Opened by `bunko_db::analytics::AnalyticsDb` (a sibling of `Database`,
  same one-writer + read-pool shape, same "sync API, call from `spawn_blocking`" rule).
- **Conventions.** `docs/rust-port/CONVENTIONS.md` applies: no `unwrap`/`expect` outside
  tests, `tracing` only, `bunko_store::fs`/`afs` instead of `std::fs`/`tokio::fs` in
  crates with a `clippy.toml`, `thiserror` in library crates, clippy clean with
  `-D warnings`, new deps in the crate's own `Cargo.toml` with an explicit version and an
  allowed licence (MIT, Apache-2.0, BSD, ISC, Zlib, MPL-2.0, Unicode, IJG; check the
  licence in the downloaded crate before adding it).
- **Library = NTFS semantics.** Series attribution uses
  `bunko_db::uploads::fold_series_title_key` on the series folder path, so every
  spelling of one series (case, Unicode form, whitespace) is one series. Store the fold
  as `series_key` and an on-disk spelling as `series_title`.
- **No deployment names.** Tests and fixtures use `192.0.2.x`, `198.51.100.x`,
  `203.0.113.x`, `example.com`, `*.test`, `localhost` only.
- **Time.** All buckets are UTC: `hour` = epoch seconds / 3600, `day` = epoch seconds /
  86400 (INTEGER columns). The API returns epoch seconds; the UI renders hourly points in
  the browser's local time and labels daily points as UTC dates.

## 3. Identity, principals and privacy

- A **principal** is who a request is attributed to:
  - `u:<username>` for an authenticated account (any role except Processor);
  - `p:<username>` for a `Role::Processor` account (OCR workers; excluded from
    popularity, reading, active users and crawl detection, kept in bandwidth totals);
  - `a:<12 hex>` for an anonymous request: the first 12 hex digits of
    `sha256(salt || network)`, where the network is the canonical IPv4 address, or the
    `/64` of an IPv6 one (one subscriber's prefix: any client there can make every
    request a new address, so per-address ids would let one client pass as thousands
    of visitors). The salt is 32 random bytes in the analytics DB `meta` table,
    replaced by the hourly upkeep once its period of `retention.ip_days` UTC days is
    over (`meta.salt_epoch` = day / `ip_days`); the old one is overwritten. An id can
    therefore be matched to an address by hashing candidates only while its salt
    lives, never longer than the raw IPs are kept, and a visitor is one id within a
    salt period only (distinct anonymous visitors over a longer range count a visitor
    once per period).
  - A UTC day keeps rows of their own for at most `ANON_PER_DAY` (10,000) anonymous
    principals: the flush folds that day's later newcomers into `a:other` (their
    traffic and downloads still count; they get no `principal_seen` or `anon_ips` row,
    the address stays in `ip_daily`). The daily tables are kept forever and anyone can
    make up addresses, so this bounds what a flood adds per day.
- **Raw client IPs** are kept only in rows with an IP retention (`retention.ip_days`,
  default 30): `ip_daily`, `anon_ips`, `auth_failures`, `slow_requests`; and in
  `crawl_episodes` (`retention.crawl_days`, default 90). After that only principals
  remain.
- The client IP is the server's own resolved client IP (`Core.proxies`,
  `RequestCtx.client_ip`), so trusted proxies / tunnels are honoured. When the identity
  was never resolved for a request (it was answered before the auth layer: `GET /`,
  `/catalog/api/*`, a CORS preflight, a `path_guard` 400), the analytics layer resolves
  the IP itself with `core.proxies` (no authentication, no limiter side effect) and the
  request is anonymous. A non-IP string from a trusted proxy header is hashed as-is for
  the principal and stored verbatim (truncated to 64 chars) where an IP is stored.

## 4. Request accounting

### 4.1 The layer

`crates/bunko-server/src/analytics/layer.rs`: `axum::middleware::from_fn_with_state`,
installed in `build_router_with` as the **second-outermost** layer (just inside
`request_log`, outside `security_headers` and `path_guard`), so it sees every request,
including ones answered early.

Before `next.run(req)` it:
1. returns `next.run(req)` untouched when analytics is disabled (`analytics.enabled`
   read live from config; cache it in an `AtomicBool` refreshed on settings change);
2. captures: `Instant` start, method, raw path + query, `Depth`, `Range`, `Origin`,
   `User-Agent` (truncate 256), request `Content-Length`, `ConnectInfo` peer;
3. inserts an `AnalyticsSlot(Arc<OnceLock<SlotIdentity>>)` into the request extensions.
   `RequestCtx::from_request_parts` (`core.rs`) fills it the first time it resolves an
   identity: `{ username: Option<String>, role, client_ip: String, auth_error:
   Option<String>, attempted_username: Option<String> }`. This is the only change to
   `core.rs` and adds no behaviour;
4. wraps the request body in a counting body (bytes in, shared `Arc<AtomicU64>`);
5. increments the in-flight gauge (section 8).

After `next.run` returns the response head it:
1. reads the `bunko_dav::DavInfo` / `bunko_dav::Served` response extensions (4.2) and
   the `X-Accel-Redirect` header;
2. records time-to-headers;
3. wraps the response body in `CountingBody` (`analytics/body.rs`), which counts
   `frame.data_ref().len()` per data frame, passes `size_hint`/`is_end_stream` through
   unchanged, and **commits one `RequestRecord` exactly once**: at end of stream, or in
   `Drop` if the body is dropped first (client abort). Drop must not block or take an
   async lock: it locks the collector's `parking_lot::Mutex` briefly and returns.

The record carries: start time, protocol, method, category, detail, status, bytes out
(counted), bytes out estimated (accel), bytes in (counted), time-to-headers, total time,
`aborted` (body dropped before end, with bytes < declared length), `range` (request had
`Range`), principal, client IP text, user agent, origin, series key/title, volume key,
`DavInfo`, `Served`.

Special cases:
- **HEAD, 204, 304, `Content-Length: 0`**: bytes out = counted body bytes (0). A 304
  with a `Served.size` adds that size to `saved_bytes`.
- **nginx offload** (`MOKURO_NGINX_ACCEL=1`): the body is empty and nginx sends the
  file. Bytes out are taken from `Served.len` (the range length, or the size) into
  `est_bytes_out`; aborts are invisible. Totals shown in the UI are
  `bytes_out + est_bytes_out`, and the UI notes when estimates are included.
- **WebSocket upgrades** (`/_processor/...`): only the 101 head is counted; frames on the
  upgraded connection are not (documented limitation).
- Body bytes are what the server handed hyper. Headers and TLS overhead are not counted
  (documented: "body bytes").

### 4.2 Annotations from the DAV layer (`bunko-dav`)

Response extensions (never on the wire), defined in `crates/bunko-dav/src/lib.rs` (or a
new `analytics.rs` there) and set in `methods/mod.rs::dispatch` and `methods/read.rs`:

```rust
/// What a WebDAV request addressed, after path-case resolution.
pub struct DavInfo {
    pub target: DavTarget,         // Root | ReaderRoot | Progress | Library | Personal | Inbox | Other
    pub rel: Option<String>,       // library- or per-user-relative path, on-disk spelling
    pub depth: Option<DavDepth>,   // PROPFIND: Zero | One | Infinity
}
/// A file body this response serves (GET/HEAD/304), for byte estimates and savings.
pub struct Served {
    pub size: u64,                 // the whole file
    pub len: u64,                  // the bytes this response carries (range length)
    pub accel: bool,               // X-Accel-Redirect: nginx sends the bytes
}
/// A PROPFIND listing served from the cache.
pub struct ListingInfo { pub raw_len: u64, pub gzip: bool }
```

`dispatch` inserts `DavInfo` on **every** DAV response (all methods, all statuses), so
classification after path-case resolution never re-parses the path and personal
`.edit*.mokuro` overlays are distinguishable from library layers. `serve_cover`
(`library/catalog.rs`) sets `Served` too.

### 4.3 Classification (`analytics/classify.rs`)

`protocol`:
- `dav`: the request reached the DAV fallback (`DavInfo` present), or the method is a
  WebDAV-only method (PROPFIND, PROPPATCH, MKCOL, COPY, MOVE, LOCK, UNLOCK);
- `api`: JSON endpoints: `/catalog/api/*`, `/login/api/*`, `/api/*`, `/_admin/api/*`,
  queue JSON;
- `processor`: `/_processor/*`, or any request whose principal is `p:`;
- `web`: everything else (pages and static assets).

The server may be mounted under a sub-path: classify on the path as the router sees it
(check `web_assets.rs` and `base_href` for how a mount prefix appears).

`category` and `detail` (detail is `""` unless listed):

| category | what | detail values |
|---|---|---|
| `volume` | GET/HEAD of a library archive (`.cbz .cbr .zip .rar`, any case) | `full`, `range` |
| `sidecar` | GET/HEAD of a library `.mokuro`, `.mokuro.gz`, layer sidecars | `primary`, `layer` |
| `cover` | GET of `<vol>.webp` / `.nocover`, `/catalog/api/cover*` | |
| `metadata` | navigation and listings | `propfind_0`, `propfind_1`, `propfind_inf`, `options`, `series_json`, `catalog_json`, `catalog_api`, `manifest`, `login_me`, `queue_poll` |
| `progress_sync` | per-user `volume-data.json`, `positions.json`, `history/**` (any method) | `volume_data`, `positions`, `history` |
| `profile_sync` | per-user `profiles.json`, `goals.json` | `profiles`, `goals` |
| `personal` | per-user correction layers (`DavTarget::Personal`) | |
| `upload` | writes to the library: PUT, MKCOL, COPY, MOVE, DELETE, LOCK, UNLOCK, PROPPATCH | `archive`, `sidecar`, `series_json`, `folder`, `other` |
| `auth` | `/login/api/token`, login / registration / setup pages and APIs | |
| `admin` | `/_admin/api/*` | |
| `web` | pages and static assets (`/`, `/catalog` page, `/_static`, `/_admin` shell, ...) | |
| `processor` | `/_processor/*`, any `p:` principal | |
| `other` | anything else | |

PROPFIND of `/` or `/mokuro-reader` carries the caller's injected per-user entries;
it stays `metadata` (the reader's whole listing is navigation).

`method`: the upper-case method; anything outside the 14 known methods (GET, HEAD,
OPTIONS, POST, PUT, DELETE, PATCH, PROPFIND, PROPPATCH, MKCOL, COPY, MOVE, LOCK,
UNLOCK) is `OTHER`.

`series_key` / `series_title`: for `volume`, `sidecar`, `cover`, library `upload`, and
`metadata` requests addressing a series folder or its `series.json`: the parent folder
of the file (or the folder itself), library-relative, on-disk spelling from
`DavInfo.rel` when present, folded with `fold_series_title_key`. Nested series folders
are `/` paths. `/catalog/api/series?name=` and `/catalog/api/manifest?series=` attribute
to that series too. `volume_key`: the archive's library-relative path (on-disk
spelling).

### 4.4 The collector (`analytics/collector.rs`)

One `Collector` behind `Arc`, holding `parking_lot::Mutex<Window>`. `record(r)` folds a
record into the window's maps (no I/O):

- `traffic`: key `(hour, protocol, method, category, detail, status_class)`
  → requests, bytes_out, est_bytes_out, bytes_in, aborted, aborted_bytes, resumed
  (`range` on a `volume`), not_modified, saved_bytes, gzip_raw_bytes, gzip_sent_bytes.
- `latency`: key `(hour, protocol, method, category)` → two histograms (time to headers,
  time to body end), fixed bucket bounds in ms:
  `[1,2,5,10,20,50,100,200,500,1000,2000,5000,10000,30000,60000,300000,+inf]`.
- `errors`: key `(hour, protocol, method, category, status)` for status ≥ 400.
- `principals`: key `(day, principal, category)` → requests, bytes_out (incl. est),
  bytes_in; plus `last_seen` per principal.
- `anon_ips`: `(day, principal, ip)` for `a:` principals (lets the admin see which IP an
  anonymous id was, within the IP retention).
- `ips`: key `(day, ip)` → requests, bytes_out, errors, auth_failures, principals seen
  (≤ 8), last user agent, country (from geo, section 9).
- `downloads`: key `(day, principal, volume_key)` → series_key, series_title, bytes,
  size (`Served.size`), for `volume` GETs (not HEAD) with status 200/206 by a `u:` or `a:`
  principal. A row counts as a **download** (popularity, downloaders, the funnel,
  dormancy) once it is `completed` (the day's bytes reached 95 % of the file, resumes
  adding up), so a `Range: bytes=0-0` probe is no download; `bytes` still counts
  every byte sent. Bytes are what was handed to hyper: an abort can count what was
  still in the socket buffers.
- The library path a volume, series or crawl touch is keyed by is canonical: empty
  and `.` segments (`S//v.cbz`, `S/./v.cbz`), which reach the same file, are dropped
  (`bunko_dav::canonical_path`).
- `auth_failures`: key `(hour, ip, attempted_username)` → count, when the slot carries an
  auth error, or a login endpoint answers 401/429.
- `slow`: the 20 slowest requests per hour (`at, method, path (≤ 256), status, ms,
  principal, ip`).
- `clients`: key `(day, app, client, browser, os, protocol)` → requests, bytes_out; plus
  `(day, app, principal)` membership for distinct counts; `origins`: `(day, origin)` →
  requests (≤ 500 origins per day, rest counted as `(other)`). Classification is
  `clients::classify` (section 9).
- `uploads`: key `(day, username)` → volumes, bytes, for successful (201/204) archive
  PUTs.

The collector also forwards, outside its lock, to the areas:
- `crawl::Detector::observe(&Observation)` for every non-processor request (cheap,
  synchronous, bounded);
- `Reading::on_file_written(FileWritten { username, target, rel, method, at_ms })` for a
  successful (2xx) PUT / DELETE / MOVE of a per-user file (`progress_sync`,
  `profile_sync`; a MOVE also reports its destination when that is in the caller's own
  tree), `try_send` onto a bounded `tokio::mpsc` channel (cap 1024, drops counted) and
  handled by the reading worker off the request path.

`clients::Clients::classify(&ClientRequest)` and `geo::Geo::country(&IpAddr)` are
methods on handles kept in `Analytics`, called once per request. The layer drops the
server's own `Origin` (the CSRF check's same-origin rule, `X-Forwarded-Host` included)
before it builds the record, so a record only carries cross-origin origins.

**Built:** authentication failures are counted when the slot carries `auth_error` (any
refused Basic or Bearer credential, with its attempted username cut to 64 characters),
or when `POST /login/api/{token,check}` answers 401 or 429 (`accounts/login.rs` attaches
a `LoginAttempt` response extension naming the attempted account). Slow requests are
ranked by time to headers, not total time, so long downloads do not fill the list; the
stored path is decoded, cut to 256 characters and has no query.

### 4.5 Bounds

- Window keys: at most 100 000 entries per map per flush window; beyond that, records
  fold into a per-map overflow counter (logged once per window, shown in the overview as
  "dropped"). **Built:** the maps keyed by text a client chooses (IPs, anonymous ids,
  principals, downloads, auth failures, client principals, origins, uploads) also share
  a 10 MiB text budget, so a flood of new addresses with long origins and user agents
  stays around 10 MiB per window (two windows can be in memory during a flush).
- `slow`: 20 per hour; `origins`: 500 per day; principals seen per IP: 8.
- Crawl trackers: 4 096 IPs, LRU (section 8). Geo cache: 16 384 IPs, LRU.

### 4.6 Flushing and retention (`analytics/flush.rs`)

- A task started beside `services.updates.start(...)` (unconditional; never inside
  `start_library_services`, which can wait 15 min for a cloud store) swaps the window
  every `flush_seconds` (default 60) and writes it in **one** `AnalyticsDb::write`
  transaction of upserts (`INSERT ... ON CONFLICT DO UPDATE SET x = x + excluded.x`;
  histograms are merged bucket-wise; `downloads.completed` is recomputed as
  `bytes + excluded.bytes >= 0.95 * size`).
- `Analytics::stop().await` does a final flush; it is called in the shutdown sequence
  after `serve` returns and before `services.library.stop()` (and before the DB handle
  is dropped).
- The crawl episodes go in the same transaction, through a two-step interface so the
  detector learns only what committed: `let mut crawl = detector.begin_flush(now)` (takes
  the detector's flush lock before the writer, refreshes a stale catalog count),
  `crawl.write(tx)` inside the transaction, `crawl.finish(committed)` after it. A
  rolled-back flush leaves every episode for the next one.
- Once per hour (and at startup) the same task:
  - rolls `traffic_hourly` and `latency_hourly` into `traffic_daily` / `latency_daily`
    for days that are complete, before pruning;
  - prunes hourly rows older than `retention.hourly_days` (90); IP-bearing rows older
    than `retention.ip_days` (30); `crawl_episodes` older than `retention.crawl_days`
    (90); `host_minute` older than `retention.host_minute_days` (7); `slow_requests`
    older than 7 days. Daily aggregates are kept forever. **Built:** the last two
    complete days keep their hourly rows whatever `hourly_days` says (the rollup still
    rebuilds them), so a short retention never erodes a day's total.
- A crash loses at most one flush window.

## 5. Tables (`analytics.sqlite`)

All DDL is created up front by the foundation, in one place per area
(`crates/bunko-db/src/analytics/{mod,traffic,reading,library,host,crawl,clients}.rs`),
idempotent (`CREATE TABLE IF NOT EXISTS`), with `analytics_schema_version` in `meta`.
Integer epoch buckets; `day`/`hour` columns are INTEGER.

```sql
-- mod.rs
meta(key TEXT PRIMARY KEY, value TEXT NOT NULL)        -- salt, schema version, first_day, geo state

-- traffic.rs
traffic_hourly(hour, protocol, method, category, detail, status_class,
  requests, bytes_out, est_bytes_out, bytes_in, aborted, aborted_bytes, resumed,
  not_modified, saved_bytes, gzip_raw_bytes, gzip_sent_bytes,
  PRIMARY KEY(hour, protocol, method, category, detail, status_class)) WITHOUT ROWID
traffic_daily(day, ...same columns..., PRIMARY KEY(day, protocol, method, category, detail, status_class)) WITHOUT ROWID
latency_hourly(hour, protocol, method, category, headers_hist TEXT, total_hist TEXT,
  PRIMARY KEY(hour, protocol, method, category)) WITHOUT ROWID       -- hist = JSON array of 17 counts
latency_daily(day, ...same...)
errors_hourly(hour, protocol, method, category, status, count, PRIMARY KEY(...)) WITHOUT ROWID
errors_daily(day, ...same...)                          -- rolled up; kept forever
auth_failures_daily(day INTEGER PRIMARY KEY, count)    -- rolled up; no IP, no username
principal_daily(day, principal, category, requests, bytes_out, bytes_in,
  PRIMARY KEY(day, principal, category)) WITHOUT ROWID
principal_seen(principal TEXT PRIMARY KEY, first_seen INTEGER, last_seen INTEGER)
anon_ips(day, principal, ip, requests, PRIMARY KEY(day, principal, ip)) WITHOUT ROWID
ip_daily(day, ip, requests, bytes_out, errors, auth_failures, principals TEXT, user_agent TEXT,
  country TEXT, PRIMARY KEY(day, ip)) WITHOUT ROWID
auth_failures(hour, ip, attempted_username, count, PRIMARY KEY(hour, ip, attempted_username)) WITHOUT ROWID
slow_requests(id INTEGER PRIMARY KEY, at INTEGER, method, path, status, ms, principal, ip)
volume_downloads(day, principal, volume_key, series_key, series_title, bytes, size, completed,
  PRIMARY KEY(day, principal, volume_key)) WITHOUT ROWID
  -- index (series_key, day), index (day)
uploads_daily(day, username, volumes, bytes, PRIMARY KEY(day, username)) WITHOUT ROWID

-- reading.rs
read_positions(username, volume_uuid, t_ms, page, chars, device, PRIMARY KEY(username, volume_uuid)) WITHOUT ROWID
reading_daily(day, username, volume_uuid, volume_key, series_key, series_title, volume_title,
  opens, pages, chars, dwell_ms, sources,           -- sources: bit 1 positions, bit 2 history
  PRIMARY KEY(day, username, volume_uuid)) WITHOUT ROWID
  -- index (series_key, day), index (username, volume_uuid); series_key '' = no known series
history_cursor(username, device, segment, last_seq, PRIMARY KEY(username, device, segment)) WITHOUT ROWID
volume_progress(username, volume_uuid, series_uuid, series_key, series_title, volume_title,
  progress, chars, completed, completed_at, time_read_min, last_progress_update, updated_at,
  PRIMARY KEY(username, volume_uuid)) WITHOUT ROWID
devices(username, device_id, label, first_seen, last_seen, source, PRIMARY KEY(username, device_id)) WITHOUT ROWID

-- library.rs
library_scan(id INTEGER PRIMARY KEY CHECK (id = 1), scanned_at INTEGER, json TEXT)   -- latest file-type scan
library_daily(day INTEGER PRIMARY KEY, series, volumes, bytes, json TEXT)            -- growth snapshot (the day's latest scan)
archive_pages(volume_key TEXT PRIMARY KEY, size, mtime, pages, formats TEXT, scanned_at)
  -- formats = JSON {format: pages}; mtime in epoch ns; pages = -1: no readable zip directory
library_series(series_key TEXT PRIMARY KEY, title, volumes, bytes) WITHOUT ROWID
  -- the latest scan's series, replaced whole by each scan (what "dormant" is measured against)

-- host.rs
host_minute(minute INTEGER PRIMARY KEY, rss_avg, rss_max, cpu_pct, connections_max, inflight_max)
host_hourly(hour INTEGER PRIMARY KEY, rss_avg, rss_max, cpu_avg, cpu_max, connections_max, inflight_max)

-- crawl.rs
crawl_episodes(id INTEGER PRIMARY KEY AUTOINCREMENT, ip, country, principals TEXT, client, user_agent, kind,
  started_at, last_at, series, volumes, catalog_series, coverage REAL, bytes_out, requests, status)
  -- index (ip), index (last_at); AUTOINCREMENT adds SQLite's sqlite_sequence: an id is never reused

-- clients.rs
clients_daily(day, app, client, browser, os, protocol, requests, bytes_out,
  PRIMARY KEY(day, app, client, browser, os, protocol)) WITHOUT ROWID
client_principals(day, app, principal, PRIMARY KEY(day, app, principal)) WITHOUT ROWID   -- ip retention
client_mix_principals(day, app, client, browser, os, protocol, principal,
  PRIMARY KEY(day, app, client, browser, os, protocol, principal)) WITHOUT ROWID   -- ip retention: distinct visitors per breakdown
origins_daily(day, origin, requests, PRIMARY KEY(day, origin)) WITHOUT ROWID
geo_daily(day, country, requests, bytes_out, PRIMARY KEY(day, country)) WITHOUT ROWID
```

## 6. Reading (`analytics/reading/`)

The reading worker receives a `FileWritten` after a per-user file is written,
**debounces per (user, file) for 10 s** (**Built:** and at most 60 s from the first
write, so a `positions.json` the reader PUTs every few seconds is still read during a
session; at most 4 096 files wait, more are dropped and counted), then reads the stored
file back from the user's directory (`users/<username>/...`, via the server's existing
per-user path resolution and `bunko_store::fs`) on a `spawn_blocking` thread, one file
at a time. **Built:** size caps per kind instead of one 32 MiB cap, so a crafted file
stays bounded in the 1 GB lite build: `positions.json` 4 MiB, a history file 16 MiB
(also its inflated JSON), `volume-data.json` 32 MiB, `device.json` 64 KiB; at most
20 000 positions, 100 000 volume-data records, 250 000 segment rows and 64 devices per
account. Skipped and refused files are counted (`reading.worker` in the API).

- **`positions.json`** `{"version":1,"positions":{"<volume_uuid>":[t_ms, page, chars,
  "<deviceId>"]}}`. For each entry, compare with `read_positions`:
  - new `t_ms` greater than stored → a reading observation on `day(t_ms)`:
    `reading_daily.opens = 1` for (day, user, uuid) (set, not add), and when no history
    covers that day, `pages += max(0, page - old_page)`, `chars += max(0, chars -
    old_chars)`; update `read_positions`; upsert `devices` (source `positions`);
  - no stored row: store the baseline; count an observation only if `t_ms` is within
    the last 48 h (a first upload carries up to 30 days of backlog);
  - entries whose `t_ms` did not change are other devices' merged copies: ignore.
  - **Built:** the file is parsed as the reader's `parsePositionsFile` parses it: a
    volume listed twice keeps only its last value (`JSON.parse`), so it is diffed once.
    A page-0 position is a reset (the reader's un-read or restart), never a read. A
    first-seen entry within 48 h counts as an open with 0 pages (there is no baseline
    to diff). Rows the file no longer lists are pruned after 35 days (the reader keeps
    30).
- **`history/<device>/<YYYY-MM>.events`, `legacy.events`**: port the reader's segment
  decoder (format 1, `src/lib/reading-history/segment-codec.ts` on the reader's
  `develop`: deflated JSON, header + volume dictionary + delta-encoded rows). Ingest
  only rows with `seq > history_cursor.last_seq` for (user, device, segment).
  **Built:** a legacy segment's cursor is per (user, `legacy:<volume>`, `legacy`), not
  per uploading device, because every converting device uploads the same converted
  events; a second device's `legacy.events` adds nothing. A segment whose header names
  another device or month than its path is refused (as the reader's importer refuses
  it), and events dated before 2015 or more than a day ahead are skipped and counted
  (`events_out_of_range`). Page rows
  (kind 0): `pages += last - first + 1`, `chars += sum(page_chars)`, `dwell_ms +=
  min(dwell, 15 min)` when dwell ≥ 0, on `day(t)`; adjust rows (kind 1): `dwell_ms +=
  time_delta_ms`, `chars += chars_delta` (clamped so totals never go negative). Other
  kinds are ignored. Mark `sources |= 2`. **Built:** a row's first history page events
  replace the pages and chars `positions.json` had estimated for that (day, user,
  volume): positions usually arrives before the 5-minute history upload and would
  otherwise count twice. Adjust rows apply only to a row that already holds a read (a
  stat edit alone is no read). `history/<device>/device.json`: upsert
  `devices` with its label/platform if present (check the reader's format).
  An unknown `format` is skipped (counted), never an error to the client.
- **`volume-data.json`**: an object keyed by volume uuid with `progress` (pages),
  `chars`, `completed`, `completedAt`, `timeReadInMinutes`, `lastProgressUpdate`,
  `series_uuid`, `series_title`, `volume_title`, `deletedOn` (see the reader's
  `src/lib/settings/volume-data.ts`). Parse with serde into a minimal struct (ignore
  every other field, especially `recentPageTurns`/`sessions`). Replace the user's
  `volume_progress` rows (delete rows that vanished or carry `deletedOn`).
- **uuid → series**: resolve `volume_uuid` to a library `volume_key` through the main
  DB's `volume_identities` (add an index on `volume_uuid` in the main schema; index
  only, no version bump needed beyond what the main schema already does), then the
  series folder of that key. Fall back to `fold_series_title_key(series_title)` from
  `volume-data.json` when the uuid is unknown. Cache the uuid map in memory, refreshed
  every 10 min. **Built:** the map also takes `volume_uuid` from the compiled
  `series_entry_cache` (a volume without a sidecar has the reader's deterministic id
  only there), is capped at 50 000 entries (about 7.5 MiB; a miss is one indexed
  `volume_identities` lookup), and is built the first time a reading file arrives. A
  volume of no known series is stored with `series_key ''` and shown as `(unknown)`.
- **Definitions** used by the API: a *read* is one (user, volume, day) row in
  `reading_daily`; *readers* are distinct users; *minutes* = `dwell_ms / 60000`.
- **Completion funnel** for a series: its volumes in library order (natural sort of the
  volume names, reuse the library's sort), and per volume: users with `progress > 0` or
  `completed` (started), users with `completed` (finished), downloads (distinct
  principals, all time). **Built:** plus `stages`: (user, volume) reads by how far they
  got (started, a quarter, half, three quarters, finished), from the compiled page
  counts (a volume without one counts as started or finished only). A series with
  nothing recorded (renamed, removed, a stale link) answers `200` with `volumes: []`
  and `in_library: false`, not 404; the `series` parameter is folded, so any spelling
  works.
- **Devices**: `devices` from positions and history paths; per user list in the Users
  panel. **Built:** a name the user gave a device in `volume-data.json`
  (`tracking.devices`) replaces its label, on a device already seen; `device.json`'s
  generated label only fills an empty one. A `volume-data.json` that is gone when the
  debounce fires (a DELETE with no PUT after it) clears that account's progress.

## 7. Library (`analytics/library/`)

- **File-type scan**: a background task, every `library_scan.interval_hours` (6), 2 min
  after startup, and on `POST .../library/rescan`. Walk the library root with
  `bunko_store::fs` (stat-only; on a cloud library this must be served from the store's
  index and must not download anything; verify). Classify each file: archive (by
  extension), primary sidecar, layer sidecar, cover, nocover marker, `series.json`,
  `catalog.json`, other (by extension). Result JSON: totals, per kind (files, bytes),
  per extension, archive mix per series (`cbz`, `zip`, `cbz+zip`, ...), and series
  without any sidecar. Stored in `library_scan`.
- **Page formats** (local libraries only; on a cloud library the API reports
  `available: false, reason: "cloud"`, and `reason: "disabled"` when
  `library_scan.page_formats` is off): for each archive not in `archive_pages` or whose
  size/mtime changed, read the zip central directory only, count image entries by
  lowercased extension, `pages` = image count. One archive at a time on
  `spawn_blocking`, at most 20 per second, resumable, rows for vanished archives
  deleted. `.cbr`/`.rar` that are not zip are counted as `unreadable`. **Built:** the
  directory is read by a hand-written streaming parser, not `zip::ZipArchive` (which
  loads every entry, so a million-entry archive would spike the heap): constant memory,
  zip64 end records used whenever present, data in front of the archive (a
  self-extractor) shifts every offset, an extension-less entry's first 16 bytes are
  sniffed (at most 512 per archive), and an archive whose parser fails, or panics, is
  stored as `pages = -1` until it changes. Formats are reported as `jpeg` (jpg, jpeg,
  jpe, jfif), `png`, `webp`, `avif`, `gif`, `jxl` and `other` (bmp, tif, tiff, heic,
  heif, and sniffed bmp/tiff).
- **Growth**: one `library_daily` row per UTC day (**Built:** upserted by every scan, so
  the day's latest snapshot wins): series, volumes, bytes, per-kind counts. The API's
  axis starts at the first snapshot when that is inside the range: before it the
  library's size is unknown, not zero.
- **Built:** volumes are archives of the four archive extensions in series folders;
  archives in the library root are `loose_archives`, not a series; the root
  `thumbnails/`, hidden and symlinked folders and the cloud-root owner's per-user files
  are skipped, as the library index skips them.
- **Uploads per day per uploader**: `uploads_daily` (from the collector) for days on or
  after `tracking_since`, plus the main DB's `volume_uploads(uploaded_at,
  uploader_username)` for days before analytics existed (those have no size). The API's
  series keys are positional (`uploader-0`.., `uploader-other`) with the account name as
  the label, so no account name is ever read as a unit. A WebDAV upload as a PUT to a
  temporary name and a MOVE to `.cbz` is not counted there (it shows in the file counts
  after the next scan).
- **Dormant series**: series in the library with no `volume_downloads` and no
  `reading_daily` row in the last 90 days, with `tracking_since` so the UI can say
  "since analytics started".

## 8. Host and crawl

- **Gauges** (foundation): `Gauges { connections, connections_peak, inflight,
  inflight_peak }` atomics. `serve.rs` increments/decrements `connections` per accepted
  connection; the layer does `inflight`. Peaks via `fetch_max`, reset by the sampler.
- **Sampler** (`analytics/host.rs`): every 15 s, process RSS (the `memory-stats` crate
  if its licence checks out, else `/proc/self/statm` on Linux/Android, `GetProcessMemoryInfo`
  on Windows via `windows-sys`, `mach task_info`/`proc_pidinfo` on macOS), peak RSS
  (`getrusage` on unix, `PeakWorkingSetSize` on Windows), process CPU time (`getrusage`
  RUSAGE_SELF on unix, `GetProcessTimes` on Windows) → CPU % of one core over the
  interval, and host memory total/available where cheap (Linux `/proc/meminfo`).
  Aggregates four samples into one `host_minute` row; rolls minutes into `host_hourly`.
  A platform without a reader reports `platform_supported.rss = false` instead of zeros.
- **Crawl detector** (`analytics/crawl.rs`): per client IP, over a window of
  `crawl.window_hours` (6), approximated by two rotating half-window buckets, track the
  distinct series touched by:
  - *content*: `volume` GETs answered 200 or 206 (WebDAV or the catalog's download link),
  - *metadata*: requests addressing ONE series answered 2xx or 304: a PROPFIND of a
    series folder with depth 1 or infinity (what rclone, davfs2, gvfs and the Windows
    redirector send to walk a tree), `/catalog/api/series?name=`, manifests.
  **Built:** `series.json` GETs are **no** signal: after a listing the reader fetches
  every one whose size or date changed, so its first sync, a parser bump or a
  server-side rewrite would fetch them all and cross the thresholds. Covers, HEADs,
  sidecars, a depth-0 stat, a depth-infinity PROPFIND of the root (the reader's normal
  listing, attributed to no series) and anything refused are no signal either; every
  request of a tracked IP still counts toward its episode's requests and bytes. Once the
  catalog's series are known (from the library index, each minute, by the host sampler)
  only a touch of one of them counts. Series are stored as `u32` hashes of the series
  key, capped at the catalog's series count (plus slack). An episode opens when distinct
  series ≥ `crawl.min_series` (20) **and** distinct series / catalog series ≥
  `crawl.min_coverage` (0.25), so an IP needs the larger of `min_series` and
  `ceil(catalog × min_coverage)`; kind `download` when the downloads alone crossed the
  thresholds, `metadata` when nothing was downloaded, `mixed` otherwise. Episodes are
  judged at each flush (`begin_flush`, section 4.6), off the request path. While open,
  every flush updates its counters (`series`, `volumes`, `coverage`, `bytes_out`,
  `requests`, `last_at`, principals, user agent, client, country). It ends after one
  window with no request from that IP; a restart closes the episodes the previous run
  left open. Processor principals are exempt; admins are not (the UI shows the
  principal, so the owner can judge). 4 096 tracked IPs (approximate LRU, an IP with an
  open episode last) and 300 000 touches in all (about 6 MiB). The catalog's size is
  `LibraryCounts::total_series()`, counted by series key like the touches: folder
  spellings of one title (case, whitespace) are one series, so a whole-library walk is
  coverage 1.0.
  Known false positive: when the reader's depth-infinity listing fails with an error it
  does not recognise as "unsupported" (a timeout or a 5xx on a large library), it falls
  back to one depth-1 listing per folder for that sync, which is a metadata episode like
  davfs2's. Detection is per IP (a crawl split over many addresses is not seen).
- **Built (sampler):** CPU is a percent of one core (two busy cores read 200 %);
  `cpu_pct` is null until the second sample; `host_hourly` is rebuilt from the minutes
  at each sample; host memory is the machine's, or the container's cgroup limit when
  lower; macOS reports no cheap "available" figure (null). Windows and macOS read their
  system calls (`windows-sys`, `libc`).

## 9. Clients and geography

- **`Clients::classify(&ClientRequest { user_agent, origin, host, protocol, path }) ->
  ClientInfo { app, client, browser, os }`**, hand-rolled (no UA database):
  - `app`: `reader-hosted` when `Origin` equals the configured reader origin
    (`catalog.reader_url`'s origin, or a default CORS origin; use the config values, never
    new literals); `reader-other` for any other cross-origin `Origin`; `bunko-web` for
    same-origin pages/APIs (`/catalog`, `/_admin`, `/account`, ...); `webdav-client` for
    DAV requests without `Origin` from a non-browser UA; `api-client` / `unknown`
    otherwise. **Built:** same-origin is decided by the CSRF check's rule
    (`http::csrf::is_own_origin`: `Host` or `X-Forwarded-Host`, and a `Host` without a
    port matches any port). A cross-origin request is, in this order: the hosted reader;
    a reader on another origin over WebDAV; Bunko's own page when a browser sends an
    origin `cors.allowed_origins` refuses (CORS would keep a reader there from reading
    the answer); a reader on another origin on `/login/api/*`; otherwise an API client.
  - `client`: `browser` or a named non-browser client: `rclone`, `davfs2`,
    `windows-webdav` (Microsoft-WebDAV-MiniRedir), `macos-finder` (WebDAVFS, WebDAVLib),
    `cyberduck`, `gvfs`, `kodi`, `nextcloud`, `winscp`, `curl`, `wget`, `python`, `go`,
    `node`, `bot` (generic crawler UAs), `other`, `none` (no UA).
  - `browser`: `firefox`, `chrome`, `edge`, `safari`, `opera`, `samsung`, `other`, `-`.
  - `os`: `windows`, `macos`, `ios`, `android`, `linux`, `chromeos`, `other`, `-`.
- **Geography** (`analytics/geo.rs`): opt-in (`analytics.geo.enabled`, default false).
  When enabled, download DB-IP "IP to Country Lite" (CC BY 4.0, mmdb), from
  `analytics.geo.url` (default
  `https://download.db-ip.com/free/dbip-country-lite-{year}-{month}.mmdb.gz`; current
  month, falling back to the previous month), ≤ 64 MiB, gunzip, verify it opens with the
  `maxminddb` crate (ISC), write atomically to `<storage base>/analytics/country.mmdb`,
  refresh monthly. Lookups are in memory with an LRU of 16 384. Loopback / private /
  link-local addresses map to `LAN`. **Built:** geography runs only while both
  `analytics.enabled` and `analytics.geo.enabled` are on (no download, no database in
  memory otherwise); before a database has loaded every lookup is `None`, local
  addresses included; an address the database does not place is `ZZ`; a failed
  refresh keeps the loaded database (status `error`) and retries after 6 hours. The UI shows the attribution "IP geolocation by
  DB-IP" with a link to db-ip.com, as the licence requires. `geo::country(&IpAddr) ->
  Option<Country>` is synchronous and cheap; the collector calls it for `ip_daily`,
  `geo_daily` and crawl episodes.

## 10. Admin API and UI

All under `/_admin/api/analytics/` (admin role, existing gate; mutating calls pass CSRF),
handled by `crates/bunko-server/src/analytics/api/` and dispatched from
`admin/mod.rs` with one arm. Query: `range=24h|7d|30d|90d|365d|all` (default `30d`);
granularity is `hour` for ranges ≤ 7d, else `day`. Every response carries:

```json
{"range": {"key": "30d", "from": 1760000000, "to": 1762600000, "granularity": "day"},
 "tracking_since": 1761000000, "enabled": true}
```

Time series use `{"t": [epoch_s, ...], "series": [{"key": "volume", "label": "Volumes",
"values": [ ... ]}]}` with one value per `t` (zeros filled). Byte values are integers.

Daily tables (`reading_daily`, `principal_daily`, `clients_daily`, `library_daily`,
`uploads_daily`, `volume_downloads`) have one point per UTC day whatever the range's
granularity: a 24h or 7d range covers every day it touches (so its counts can span up to
about 48 h), and those answers add `series_granularity: "day"`.

| Endpoint | Body (in addition to the common fields) |
|---|---|
| `GET overview` | `kpis{bytes_out, bytes_in, est_bytes_out, requests, downloads, downloaders, reads, readers, minutes_read, pages_read, dau, wau, mau, crawl_episodes, errors_5xx, auth_failures, rss_bytes, rss_peak_bytes, cpu_pct, uptime_s, dropped}` (`dau`/`wau`/`mau`: accounts and anonymous visitors, today and the last 7 and 30 days; `dropped`: all time), `bandwidth` (time series of bytes out by category, top 6 + `other`), `top_series[5]{series_key,title,downloads,readers,minutes}`, `top_principals[5]{principal,label,kind,bytes_out}` |
| `GET traffic?by=category\|protocol\|method\|detail&metric=bytes_out\|bytes_in\|requests&category=` | time series by `by` (top 12 + `(other)`); `by`, `metric`; `totals[]{key,label,requests,bytes_out,bytes_in,est_bytes_out}`; `sync{progress:{bytes_out,bytes_in,requests,by_detail[]{key,label,requests,bytes_out,bytes_in}}, profile:{...}}`; `metadata{bytes_out,bytes_in,requests,by_detail[]{key,label,requests,bytes_out}}`; `dav_methods[]{method,requests,bytes_out,bytes_in}` |
| `GET principals?limit=50` | `rows[]{principal,label,kind(user\|anon\|processor),requests,bytes_out,bytes_in,by_category{category: bytes_out},last_seen,ips[] (anon only, within IP retention)}`, `totals{users,anon,processor}` |
| `GET series?sort=downloads\|downloaders\|readers\|reads\|minutes\|pages\|bytes&limit=50&q=` | `rows[]{series_key,title,downloads,downloaders,readers,reads,pages,minutes,bytes_out,prev{downloads,readers},volumes}`, `sort` (prev = the previous period of the same length, for trending; `volumes` = distinct volumes downloaded; `downloads` = completed (day, principal, volume) rows; `bytes_out` = every byte sent, partial downloads included) |
| `GET funnel?series=<any spelling>` | `series_key,title,in_library,volumes[]{volume_key,title,started,finished,downloads,in_library,page_count}`, `readers_started`, `readers_finished`, `stages[]{key(started\|p25\|p50\|p75\|completed),label,reads}`; without `series`: `series[]{series_key,title,readers_started,readers_finished,volumes_started,downloads}` (top 50) |
| `GET reading` | time series `minutes` (0.1 min), `pages`, `readers`; `totals{reads,readers,minutes,pages,chars,opens}`; `top_readers[]{username,minutes,pages,volumes,reads,days,chars}`; `top_volumes[]{volume_key (null: not in the library),volume_uuid,title,series_key,series_title,readers,reads,pages,minutes}`; `sources{positions,history}` (each read once); `worker{queued,dropped,pending,files,positions,segments,volume_data,device_files,missing,too_large,unknown_format,malformed,failed,coalesced,pending_dropped,events_out_of_range}` (since start) |
| `GET users` | time series `dau` (accounts); `dau`, `wau`, `mau` (accounts, today and the last 7 and 30 days); `visitors{dau,wau,mau}` (accounts and anonymous, as the overview counts); `accounts` (every account of the range); `rows_limit` (200); `rows[]{username,last_seen,days_active,requests,bytes_out,bytes_in,devices[]{id,label,first_seen,last_seen,source},clients[] (app keys),minutes,pages,volumes_read,reads,volumes_started,volumes_completed}` (most bytes first) |
| `GET library` | `scanned_at, cloud, scanning, scan{phase,started_at,duration_ms,pages_duration_ms,interval_hours,waiting,error}, totals{series,volumes,bytes,files}, by_kind[]{kind,label,files,bytes}, by_ext[]{ext (no dot; '' none),files,bytes}, archive_mix[]{mix,series}, no_sidecar{count,series[]}, loose_archives, page_formats{available,reason,scanned,pending,unreadable,formats[]{ext,pages,volumes}}, growth{t,series[series,volumes,bytes]}, uploads{t,series[]{key: uploader-N, label: account},by_uploader[]{username,volumes,bytes},recent[]{volume_key,series_title,username,uploaded_at}}, dormant{days,count,tracking_since,series[]{series_key,title,volumes}}` (`dormant` null before the first scan) |
| `POST library/rescan` | `{ok:true, scanning:true}`; 409 while analytics are off |
| `GET health` | time series `status` (keys `2xx`..`5xx`); `errors[]{status,protocol,method,category,count}`; `auth_failures{t,values,top_ips[]{ip,country,count,usernames[]},top_usernames[]{username,count},ip_days}` (the counts and `errors[]` cover the whole range, from the daily rollups past the hourly and IP retention; `top_ips` and `top_usernames` only the last `ip_days`); `latency[]{protocol,method,category,requests,p50_ms,p95_ms,p99_ms,ttfb_p50_ms,ttfb_p95_ms}`; `slow[]{at,method,path,status,ms,principal}` (no protocol: the UI reads it off the method and path as the classifier does); `efficiency{not_modified,saved_bytes,gzip_raw_bytes,gzip_sent_bytes,aborted,aborted_bytes,resumed,est_bytes_out}` |
| `GET host?range=24h\|7d\|30d` (another range: 400) | `resolution` (`minute` for 24h, `hour` beyond; `range.granularity` says `hour` for both, the browser's local time), `step` (60 or 3600); time series `rss`, `rss_max`, `cpu_pct`, `cpu_max`, `connections_max`, `inflight_max`, whose values are **null** where there is no reading (the server was down), never 0; `now{rss,peak_rss,cpu_pct (null before the second sample),connections,inflight,host_total,host_available (null on macOS),reading_queue,reading_dropped,collector_dropped}`; `platform_supported{rss,cpu}` |
| `GET crawlers` | `thresholds{window_hours,min_series,min_coverage}`, `catalog_series`, `episodes[]{id,ip,country,principals[],client,user_agent,kind,started_at,last_at,series,volumes,catalog_series,coverage,bytes_out,requests,status,active}` (newest first, at most 500), `by_ip[]{ip,country,episodes,last_at,bytes_out}`, `episodes_per_day{t,granularity:"day",series[episodes]}`, `tracking{ips,max_ips,entries,max_entries,approx_bytes}` |
| `GET clients` | `apps[]`, `clients[]`, `browsers[]`, `os[]`, `protocols[]`, `dav_clients[]`, `dav_apps[]` each `{key,label,requests,bytes_out,principals}` (distinct visitors within `principals_days`, the IP retention); `origins[]{origin,requests,hosted_reader}`; `by_app{t,series}` (daily); `totals{requests,bytes_out}` |
| `GET geo` | `geo_enabled` (analytics and geography both on; `enabled` stays the common analytics flag), `status(off\|downloading\|ready\|error)`, `error`, `db{version,downloaded_at,built_at,database_type,size,attribution,attribution_url,license}`, `attribution{text,url,license}`, `checked_at`, `next_check_at`, `countries[]{country (ISO, LAN or ZZ),requests,bytes_out}` |
| `POST geo/refresh` | `{ok:true, started}`; `started: false, reason: "off"` when geography is off |

Every view answers its data. The only `available` field left is `page_formats`'s
(with a `reason`). The UI still shows "Not collected yet" for a top-level
`{"available": false}`, which development builds of 0.8 answered for an area that did
not exist yet, and an older server's 404 for a view it does not have.

Settings: the `analytics:` section is editable through the existing admin settings flow
(`admin/settings.rs`: an `analytics` editor like the others, live, saved to config).

**UI** (`web/admin/analytics*.js`, `analytics.css`): an **Analytics** tab in the admin
panel with a range picker and sub-views: Overview, Traffic, Popular, Reading, Users,
Library, Clients & geography, Health, Server, Crawlers, Settings. Charts are
dependency-free inline SVG drawn by a small shared helper (`analytics-charts.js`): line /
stacked area time series with hover readout, horizontal bar lists, sparklines, KPI
tiles, funnel bars, sortable tables. Follow the `dataviz` skill's palette and
accessibility rules; work in the admin panel's light and dark themes; responsive at
phone width. Every fetch URL stays relative (`web_assets.rs` pins this: use the
existing `API_BASE`/`apiGet` helpers). Bytes are shown with binary units; times in the
browser's local time for hourly series and as UTC dates for daily ones.

## 11. Configuration

```yaml
analytics:
  enabled: true
  flush_seconds: 60
  retention:
    ip_days: 30          # raw client IPs (ip_daily, anon_ips, auth_failures, slow requests)
    hourly_days: 90      # hourly traffic and latency rows (daily rows are kept forever)
    host_minute_days: 7
    crawl_days: 90
  crawl:
    window_hours: 6
    min_series: 20
    min_coverage: 0.25
  library_scan:
    interval_hours: 6
    page_formats: true   # local libraries only
  geo:
    enabled: false
    url: "https://download.db-ip.com/free/dbip-country-lite-{year}-{month}.mmdb.gz"
```

Validated like the other sections (`AnalyticsConfig::validate`), documented in
`docs/configuration.md` (`### Analytics`, before `### Updates`) and
`config.example.yaml`; env overrides follow the existing `MOKURO_<SECTION>_<KEY>` rule.
Disabled: the layer passes requests through untouched and nothing is written; stored
data stays viewable.

## 12. Work split

Foundation (in parallel):
- **A1 backend core**: everything in sections 2-5 and 10 except the area internals: the
  analytics DB with **all** DDL (every area's tables), prune/rollup, config section,
  `DavInfo`/`Served`/`ListingInfo` annotations, the layer, counting bodies, the
  identity slot, classification, the collector with all its maps, flushing, shutdown,
  gauges in `serve.rs`, the admin dispatch arm and API handlers for `overview`,
  `traffic`, `principals`, `series`, `health`, the settings editor, docs, CHANGELOG.
  It created stub modules with the final signatures for the areas, wired in so the
  area agents only filled in their own files: `reading` (`Reading::on_file_written`,
  worker start, `api::{reading,users,funnel}`), `library` (scanner start, `rescan`,
  `api::library`), `host` (sampler start, `now()`, `api::host`), `crawl`
  (`Detector::observe`, the flush interface of section 4.6, `api::crawlers`), `clients`
  (`Clients::classify`, `api::clients`), `geo` (`Geo::country`, `api::{geo,refresh}`).
  The stubs are gone now that every area answers.
- **A2 UI**: the whole Analytics tab against the JSON shapes in section 10 (all
  sub-views), developed against mock JSON, verified in a browser.

Areas (in parallel, after the foundation merges), each owning its own files:
- **B1 reading**: section 6 + `reading`, `users`, `funnel` endpoints.
- **B2 library**: section 7 + `library` endpoints.
- **B3 host + crawl**: section 8 + `host`, `crawlers` endpoints.
- **B4 clients + geo**: section 9 + `clients`, `geo` endpoints.

Then a multi-lens review with adversarial verification, fixes, and an end-to-end run:
a real server over a demo library, real WebDAV and reader traffic, the Analytics tab
checked in a browser.

# Configuration Reference

Mokuro Bunko Server uses a YAML configuration file. The default locations are:

- **Linux/macOS**: `~/.config/mokuro-bunko/config.yaml`
- **Windows**: `%LOCALAPPDATA%\mokuro-bunko\config.yaml`

To use another file, pass it before the command (or set `MOKURO_CONFIG`):

```bash
mokuro-bunko --config /path/to/config.yaml serve
```

A missing file means "all defaults". A 0.5.2 file loads as it is (see
[MIGRATING-0.7.md](MIGRATING-0.7.md)). `mokuro-bunko config init` writes one,
`mokuro-bunko config show` prints the configuration in effect, and
`mokuro-bunko config set <section.key> <value>` changes one value. Most
settings can also be changed in the admin panel (`/_admin`), which saves them
back to the same file. [`config.example.yaml`](../config.example.yaml) is a
commented starting point.

## Configuration Options

### Server

```yaml
server:
  host: "0.0.0.0"  # Host to bind to
  port: 8080       # Port to listen on
  trusted_proxies: []  # Reverse proxies on other hosts, e.g. ["172.16.0.0/12"]
  threads: 0       # Async worker threads (0 = min(cores, 4))
  cache_mb: 32     # Memory budget of the in-memory caches
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `host` | string | `"0.0.0.0"` | Network interface to bind to. Use `127.0.0.1` for local-only access. |
| `port` | integer | `8080` | TCP port for the server. |
| `trusted_proxies` | list | `[]` | Networks or addresses of reverse proxies whose `X-Real-IP` / `X-Forwarded-For` are believed. A proxy on this machine (the Docker image's own nginx, or nginx/Caddy on the same host) is always trusted. Add one only when the proxy runs on another host or container; otherwise every client is counted as the proxy and login rate limits are shared. Set as `MOKURO_SERVER_TRUSTED_PROXIES=172.16.0.0/12,10.0.0.5`. |
| `threads` | integer | `0` | Worker threads of the async runtime, 0-256. `0` means the smaller of the CPU count and 4. Each connected processor costs a socket, not a thread, so this rarely needs raising. Replaces 0.5's `MOKURO_THREADS`. At startup. |
| `cache_mb` | integer | `32` | Byte budget, in MiB (1-65536), of the in-memory caches: WebDAV directory listings, the library index and compiled metadata. Raise it for a very large library on a machine with spare RAM, lower it on a tiny host. At startup. |

### Storage

```yaml
storage:
  base_path: "/var/lib/mokuro-bunko"
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `base_path` | string | Platform-specific | Base directory for all data storage. |

Default paths:
- **Linux/macOS**: `~/.local/share/mokuro-bunko`
- **Windows**: `%LOCALAPPDATA%\mokuro-bunko`

#### Storage Structure

```
base_path/
├── library/                 # Shared manga library
│   ├── <Series>/
│   │   ├── Volume 01.cbz
│   │   ├── Volume 01.mokuro             # primary OCR
│   │   ├── Volume 01.paddle-manga.mokuro # an extra OCR layer (see generations)
│   │   ├── Volume 01.webp               # cover thumbnail
│   │   └── series.json                  # compiled by the server
│   └── catalog.json                     # compiled by the server
├── inbox/                   # Upload queue
├── users/                   # Per-user reading progress
│   └── alice/volume-data.json, profiles.json, goals.json
├── logs/                    # server.log, and logs/ocr/ per volume
├── processors/              # Per-machine OCR profiles (remote processors)
├── backends/                # OCR backend pack: libtorch for CUDA / ROCm / CPU (install-ocr; full build)
├── models/                  # Downloaded OCR models: compiled libtorch packages, PP-OCR ONNX (full build)
└── mokuro.db                # SQLite database
```

#### Upload verdicts

Every WebDAV `PUT` under `/mokuro-reader/` or `/inbox/` is written to a
hidden temporary file beside its destination (`.<name>.upload-*.tmp`) and
moved into place only once it checks out, so a failed upload never replaces
or deletes the file that was there, and a read during the upload sees the
old file. The body must be as long as its `Content-Length`; a `.cbz` must
also open as a zip with every member matching its CRC-32 (the check the OCR
processors run on their downloads — about 80–100 ms per 200 MB).

- Success keeps the usual 201/204 and adds `X-Mokuro-Upload: verified` (an
  archive) or `stored` (any other file) and `X-Mokuro-Size: <bytes stored>`.
- A PUT may carry `Content-Digest` (RFC 9530; `sha-256` or `sha-512`,
  hashed as the body streams in). A malformed header is ignored as absent;
  unknown algorithms are skipped. A match adds
  `X-Mokuro-Digest-Verified: <algorithm>`.
- Failure answers JSON `{"ok": false, "reason", "detail", "retry"}`:
  `truncated` (422, retry), `corrupted-in-transit` (422, retry: the body
  does not match its digest; the zip is not checked), `not-an-archive`
  (422), `disk-full` (507), `forbidden` (401/403), `server-error` (500,
  retry), and `archive-damaged` (422). An archive whose digest matched but
  whose CRCs fail is the client's own damaged copy (no retry: re-import it).
  Without a digest the first damage to a path is `retry: true` (it may have
  happened in transit); the same size and damaged members arriving again
  within an hour is `retry: false` (the copy is damaged). The server
  remembers the last 256 such signatures, in memory.

Every WebDAV `OPTIONS` answer and every `.cbz` PUT answer also carries
`X-Mokuro-Put: verified`: a client that deletes a remote file before
re-uploading it (for servers that rename instead of overwriting) may skip
that delete here, where a failed PUT leaves the old file in place.

These headers are in the CORS `Access-Control-Expose-Headers`. A
`series.json` PUT is a metadata update request and answers as before.

### Registration

```yaml
registration:
  mode: "self"
  default_role: "registered"
  allow_anonymous_browse: true
  allow_anonymous_download: true
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `mode` | string | `"self"` | Registration mode (see below). |
| `default_role` | string | `"registered"` | Role given to new accounts: `registered`, `uploader`, `inviter` or `editor`. |
| `allow_anonymous_browse` | boolean | `true` | Allow listing the library (WebDAV `PROPFIND`) without logging in. |
| `allow_anonymous_download` | boolean | `true` | Allow downloading files without logging in. |
| `require_login` | boolean | `false` | Older shorthand: `true` turns both anonymous options off. Only read when neither of them is set. |

#### Registration Modes

| Mode | Description |
|------|-------------|
| `disabled` | Admin creates all accounts via CLI or web panel. |
| `self` | Open registration - anyone can create an account. |
| `invite` | Invite codes required. Generate codes via admin panel or CLI. |
| `approval` | Users can register but admin must approve accounts. |

#### Signing in: bearer tokens

A client checks a password once and then sends a token instead of it:

```
POST /login/api/token
{"username": "alice", "password": "…", "kind": "reader", "label": "Firefox on the laptop"}
→ {"token": "…", "token_type": "Bearer", "kind": "reader", "expires_at": 1790000000.0,
   "user": {"username": "alice", "role": "uploader"}}
```

Credentials may also come as a Basic header on the same request. `kind` sets
the lifetime: `web` 7 days (the server's own pages), `reader` 90 days,
`processor` 30 days. Every request then carries `Authorization: Bearer <token>`,
which costs one database read instead of a bcrypt check. `DELETE
/login/api/token` with the token signs it out. The password is rate-limited
exactly like a login; a token is 32 random bytes and only its SHA-256 is
stored.

A token stops working when it expires, when it is signed out, when its
account's password changes (every token of the account), or when the account
is disabled or deleted. A role change applies to the next request. A request
with a token that no longer works gets `401` with `WWW-Authenticate: Bearer
error="invalid_token"`, and `GET /login/api/me` answers `401
{"authenticated": false}` so a client knows to sign in again.

The server's pages keep only the token (in `sessionStorage`), never the
password. Processors fetch a token when they register and fetch a new one if
it is refused. Basic auth still works for any WebDAV client.

#### User Roles

| Role | Read | Write Progress | Add Files | Modify/Delete | Invites | Admin |
|------|------|----------------|-----------|---------------|---------|-------|
| `anonymous` | Yes (configurable) | No | No | No | No | No |
| `registered` | Yes | Own only | No | No | No | No |
| `uploader` | Yes | Own only | Yes | Own uploads | No | No |
| `editor` | Yes | Own only | Yes | Yes | No | No |
| `inviter` | Yes | Own only | Yes | Yes | Yes | No |
| `admin` | Yes | Own only | Yes | Yes | Yes | Yes |
| `processor` | Yes | No | No | No | No | No |

`inviter` has everything `editor` has, plus the invite-management endpoints,
without full admin privileges.

`processor` is an OCR machine, not a person (see
[Remote OCR processors](#remote-ocr-processors)): it registers at
`/_processor/register`, reads the archives it is sent and posts its results
back, and nothing else. It cannot write library files (the library server
writes every sidecar), save progress, manage invites or reach the admin
panel. It is never granted by an invite code or by open registration; an
admin creates it (`mokuro-bunko admin add-user gpu-box --role processor`, or
the admin panel's Users tab).

### CORS

```yaml
cors:
  enabled: true
  allowed_origins:
    - "https://reader.mokuro.app"
    - "http://localhost:5173"
    - "http://localhost:*"
  allow_credentials: true
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enabled` | boolean | `true` | Enable CORS headers. |
| `allowed_origins` | list | See below | Origins allowed to access the server. |
| `allow_credentials` | boolean | `true` | Allow cookies and auth headers in cross-origin requests. |

Default allowed origins:
- `https://reader.mokuro.app`
- `http://localhost:5173`
- `http://localhost:*`
- `http://127.0.0.1:*`

The `*` wildcard matches any port number (e.g., `http://localhost:*` matches
`http://localhost:3000`). `mokuro-bunko config cors-add <origin>` and
`cors-remove` edit the list from the command line.

### SSL/TLS

```yaml
ssl:
  enabled: false
  auto_cert: false
  cert_file: ""
  key_file: ""
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enabled` | boolean | `false` | Enable HTTPS. |
| `auto_cert` | boolean | `false` | Auto-generate self-signed certificate. |
| `cert_file` | string | `""` | Path to SSL certificate file (PEM format). |
| `key_file` | string | `""` | Path to SSL private key file (PEM format). |

#### SSL Modes

1. **Disabled** (default): Server runs on HTTP only.

2. **Auto-generated certificate**: Server generates a self-signed certificate.
   ```yaml
   ssl:
     enabled: true
     auto_cert: true
   ```
   Certificates are stored in `~/.local/share/mokuro-bunko/certs/`
   (`%LOCALAPPDATA%\mokuro-bunko\certs\` on Windows).

3. **Custom certificate**: Use your own certificates (e.g., from Let's Encrypt).
   ```yaml
   ssl:
     enabled: true
     cert_file: "/path/to/cert.pem"
     key_file: "/path/to/key.pem"
   ```

### Admin Panel

```yaml
admin:
  enabled: true
  path: "/_admin"
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enabled` | boolean | `true` | Enable the admin web panel. |
| `path` | string | `"/_admin"` | URL path for the admin panel. |

Access the admin panel at `http://your-server:8080/_admin/` (requires admin credentials).

#### Audit log

The admin panel's **Audit** tab lists what accounts did: uploads, deletes,
moves, invites, metadata updates, and OCR results. Events are kept for 30
days. The tab has a search box and filters for actor, action, target type and
a date range. The filters are kept in the page's URL, so a refresh or a shared
link opens the same view. The "OCR results" action choice shows only OCR
sidecars written and rejected.

Reading-progress sync (each reader's progress files, target type `progress`)
is most of the log on a library with readers, so it is hidden unless you tick
**Include reading-progress sync**.

`GET /_admin/api/audit` takes the same filters:

| Parameter | Meaning |
|-----------|---------|
| `actor` | One account, exactly. |
| `action`, `target_type` | Any of these; repeat the parameter or separate with commas. |
| `since`, `until` | A date or ISO date-time (UTC unless it names a zone). `since` is inclusive, `until` exclusive. |
| `q` | Case-insensitive text in the actor, action, target path or details. |
| `include_progress` | `1` to include reading-progress sync. Naming `progress` in `target_type` also includes it. |
| `limit` | Events per page: 50 by default, at most 200. |
| `cursor` | The `next_cursor` of the previous page. |

Pages are newest first. Pass a page's `next_cursor` back for the next, older
page. Events logged in the meantime never shift a page. A first page (no
`cursor`) also returns `total`, the number of matching events, and `facets`,
the actors, actions and target types in the log.

Every OCR sidecar written is logged as `ocr_sidecar_written` (target type
`sidecar`), with the generation, engine, detector, precision, pages, failed
pages and runner build in its details. Every result the library refused is
logged as `ocr_sidecar_rejected` with the reason: a file that failed
validation, a volume deleted or replaced while it was read, or a sidecar a
processor sent under the wrong name. The actor is the processor's account, or
empty for this server's own OCR, so filtering by a processor's account shows
everything it delivered.

Who wrote each sidecar that is on disk now is kept apart from the log and is
never pruned with it; see [OCR internals](ocr-internals.md#who-wrote-each-sidecar).

### Catalog

```yaml
catalog:
  enabled: false
  reader_url: "https://reader.mokuro.app"
  use_as_homepage: false
  enrich_community: true
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enabled` | boolean | `false` | Serve the web catalog at `/catalog`. |
| `reader_url` | string | `"https://reader.mokuro.app"` | Where the catalog's "read" links open. |
| `use_as_homepage` | boolean | `false` | Redirect browser visits to `/` to the catalog. |
| `enrich_community` | boolean | `true` | Fetch ratings, tags and genres from AniList/MAL for series linked to an external id (no API key needed). |

#### Volume manifest

A catalog "read" link opens the reader at
`#/upload?cbz=<archive URL>&manifest=<manifest URL>[&cover=<path>]`. The
manifest, `GET /catalog/api/manifest?series=<folder>&volume=<volume>`, is
JSON (`Cache-Control: no-store`) naming every file of that volume with its
`url` (`/mokuro-reader/<series>/<file>`, escaped like the archive link),
`size` and `modified` (ISO UTC):

```json
{"version": 1, "series": "Dr Stone", "volume": "Dr Stone 01",
 "archive": {"url": "/mokuro-reader/Dr%20Stone/Dr%20Stone%2001.cbz", "size": 123, "modified": "2026-09-27T01:02:03Z"},
 "ocr": {"url": "/mokuro-reader/Dr%20Stone/Dr%20Stone%2001.mokuro", ...},
 "layers": [{"id": "hayai-nova-ppocr", "url": ".../Dr%20Stone%2001.hayai-nova-ppocr.mokuro", ...}],
 "cover": {"url": ".../Dr%20Stone%2001.webp", ...},
 "series_file": {"url": "/mokuro-reader/Dr%20Stone/series.json", ...},
 "pending": [{"kind": "ocr", "id": "hayai-nova", "eta": "2026-09-27T21:14:00Z"},
             {"kind": "layer", "id": "hayai-nova-ppocr", "eta": null}],
 "recheck_after": 95}
```

- `ocr` is `<volume>.mokuro`, else `<volume>.mokuro.gz`, else `null`.
  It carries `sha256` (the `series.json` entry's `mokuro_sha256`) once the
  metadata pass has hashed that exact file; never computed on the request.
- `layers` holds each `<volume>.<id>.mokuro[.gz]` whose id matches
  `[a-z0-9-]{1,32}` (plain beats `.gz`), in `ocr.generations` order, then
  alphabetically. A file belongs to the longest archive name it starts with,
  so `Vol 1.5.mokuro` is `Vol 1.5`'s OCR, not a layer `5` of `Vol 1`.
- `cover` and `series_file` are `null` when absent.
- `pending` has one entry per OCR job still to run for the volume (`kind`
  `ocr` for the primary generation, `layer` for the others; `id` is the
  generation name). `eta` is the queue page's own finishing time,
  or `null` when the queue cannot price it (a failure backoff, no machine
  connected, no measured speed). `recheck_after` is the seconds until the
  earliest `eta` plus 10, clamped to 30–3600; 300 when nothing is priced;
  `null` when nothing is pending.

#### OCR queue file

`GET /mokuro-reader/.mokuro-queue.json` (also `HEAD`) is the whole pending
OCR queue, one entry per volume, for a reader to poll instead of a timer
per volume:

```json
{"version": 1, "generated_at": "2026-09-28T16:00:00Z", "held": null, "next_check_after": 95,
 "volumes": [{"series": "Dr Stone", "volume": "Dr Stone 01",
   "path": "/mokuro-reader/Dr%20Stone/Dr%20Stone%2001.cbz",
   "manifest": "/catalog/api/manifest?series=Dr%20Stone&volume=Dr%20Stone%2001",
   "jobs": [{"kind": "ocr", "id": "hayai-nova", "state": "running", "eta": "…Z", "progress": 0.42},
            {"kind": "layer", "id": "hayai-nova-ppocr", "state": "queued", "eta": "…Z", "progress": null}]}]}
```

It is priced from the same plan as the manifest, layers waiting for their
primary included. `state` is `running`, `queued` or `held`. `held` is null,
or `{"reason": "no-processor" | "paused" | "benchmarking"}`.
`next_check_after` follows the `recheck_after` rule over every job. There
are no machine names or errors in it. It is read with a library file's
rules and CORS, is listed by no PROPFIND, and refuses every write (405).
It carries `Cache-Control: no-cache` and a strong `ETag`, with a 304 on
`If-None-Match`, and is gzipped when accepted. It is rebuilt at most once
a second, and an ETA is republished only when it moves by a minute or
more, so a quiet queue keeps its ETag. 2,000 jobs build in about 120 ms
into 370 kB (20 kB gzipped).

A `.cbz` written over WebDAV (PUT, or MOVE/COPY into place) joins the OCR
queue as the request completes, instead of at the next poll. A PUT that
queued OCR answers with `X-Mokuro-Manifest` (that volume's manifest URL) and
`X-Mokuro-Recheck-After` (seconds, same rule), both exposed through CORS.
Pricing on the PUT uses the queue's cached pending list and gives up (300)
rather than walk the library or more than 300 queued items.

It is read with exactly the rules of the volume's `.cbz` (anonymous
download setting, credentials, 401 challenge) and gets the same CORS
headers, whether or not `catalog.enabled` is on. A missing volume is 404; a
path outside the library is 403.

### Queue

```yaml
queue:
  show_in_nav: false
  public_access: true
  display: normal
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `show_in_nav` | boolean | `false` | Show an OCR Queue button in the top navigation bar. |
| `public_access` | boolean | `true` | If `false`, queue status API access requires authenticated user credentials. |
| `display` | string | `normal` | How much the queue page shows, to every viewer: `minimal`, `normal` or `detailed` (below). Editable in the admin panel and applied at once, no restart. |

The display levels:

| Level | What the page shows |
|-------|---------------------|
| `minimal` | A compact card per connected machine: a line per running volume (percentage and finishing time) and the volume on deck. Then the first few pending volumes, how many wait in all, and when the queue ends. |
| `normal` | One card per machine: the volume it is reading with a progress bar, pages and finishing time, and the volume on deck as a single "Next" line. The pending list with a finishing time per volume, the run order, pending thumbnails, volumes skipped for missing pages, and failures with a generic reason ("engine error", "archive incomplete", "failed — will retry"). |
| `detailed` | `normal`, plus each running volume's stage pipeline (widths, devices, queue depths, busy share), the congestion verdict, the machine's real throughput on that layer and its startup, and one line per layer with what the machines reading it deliver together. For tuning. |

Whatever the level, raw error messages, log paths, processor names, hardware
labels and the OCR backend are sent to an authenticated **admin** only. Every
other viewer gets the reason category and an alias for each machine: "this
server", then "machine 1", "machine 2"… in the order the processors first
registered, or the processor's own `processor.public_name` when its
`processor.yaml` sets one. The server shapes the status payload per level and
per viewer, so a lower level never carries a field it does not show.

The page polls `GET /queue/api/status` about once a second (less often in a
hidden tab); an unchanged answer is a `304 Not Modified`. A logged-in
viewer's credentials are checked against the same rate limit as every other
login, and a wrong or stale password is answered exactly as a visitor is,
with an `X-Queue-Auth: failed` header on which the page drops its stored
login.

Set with `MOKURO_QUEUE_DISPLAY` or `mokuro-bunko config set queue.display minimal`.

### Database

```yaml
database:
  busy_timeout_ms: 5000
  lock_retries: 5
  retry_initial_delay_seconds: 0.05
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `busy_timeout_ms` | integer | `5000` | How long a SQLite statement waits on a lock (at least 100). |
| `lock_retries` | integer | `5` | Retries when another process (such as the admin CLI) holds the write lock. |
| `retry_initial_delay_seconds` | float | `0.05` | First retry delay; doubles on each retry. |

These rarely need changing.

### Dynamic DNS

```yaml
dyndns:
  enabled: false
  provider: duckdns      # or generic
  token: ""
  domain: ""
  update_url: ""         # generic provider only
  interval: 300          # seconds, at least 30
```

`mokuro-bunko dyndns setup` fills these in interactively.

### Updates

```yaml
update:
  check: true
  channel: stable
  manifest_url: "https://github.com/Gnathonic/mokuro-bunko/releases/latest/download/release.json"
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `check` | boolean | `true` | Look for new releases in the background (first check a minute after start, then every 12 hours) and show the result in the admin panel's **Updates** card (Status tab). With `false`, nothing is fetched unless you press **Check now**. |
| `channel` | string | `stable` | `stable` or `prerelease`. Note that GitHub's `releases/latest` never points at a pre-release, so `prerelease` sees nothing newer until a separate manifest URL is published. |
| `manifest_url` | string | the GitHub `releases/latest` `release.json` | Where the release manifest comes from. Change it for a mirror or a fork (a fork also needs its own signing key compiled in). |

The manifest is signed (ed25519, with a public key compiled into the
binary) and every archive's sha256 comes from it, so a compromised download
host cannot push a binary. What the **Update and restart** button does
depends on how mokuro-bunko was installed:

| Install | What happens |
|---|---|
| A tarball, `install.sh` as a user (`~/.local/lib/mokuro-bunko`), the Windows zip, portable folder or `install.ps1` | One click: download, verify, swap the executable, restart (on Windows `run.bat` restarts it). |
| `install.sh` as root with the system unit, a distro package, Homebrew, Nix | Notice only; update through the package manager or re-run `install.sh`. |
| Docker (`/.dockerenv`, or `MOKURO_INSTALL_KIND=docker`) | Notice with the image to pull. |
| Android | Notice only. |

`MOKURO_INSTALL_KIND` (`self`, `docker`, or a manager's name) overrides the
detection. `mokuro-bunko update check` and `update apply [--yes] [--restart]`
do the same from a terminal.

### OCR

The server OCRs every volume in the library in the background and writes the
results beside the archive: `<Volume>.mokuro` (the text layer every reader
uses), optionally more layers from other OCR engines
(`<Volume>.<name>.mokuro`), and a cover thumbnail `<Volume>.webp`. What runs,
in what order and on which machine is set here.

OCR needs a **full** build. In a lite build `ocr.local_processing` is
treated as `false`: the queue holds until a
[remote processor](#remote-ocr-processors) logs in.

```yaml
ocr:
  backend: auto
  poll_interval: 30
  concurrency: 1
  local_processing: true
  autobench: true
  upgrade:
    enabled: false
    replace: [mokuro-legacy, mokuro]
  generations:
    - name: hayai-nova
      engine: hayai-nova
      detector: ppocr-manga
      primary: true
```

That is also what an empty `ocr` section means: one `hayai-nova` generation,
run on this machine.

| Option | Type | Default | What it does | When to change it |
|--------|------|---------|--------------|-------------------|
| `backend` | string | `auto` | Which devices OCR on this machine uses: `auto`, `cuda`, `rocm`, `cpu`, or `skip` (see [Backends](#ocr-backends)). | To keep OCR off a GPU (`cpu`), or `skip` for a server that should run no OCR itself. |
| `poll_interval` | integer | `30` | Seconds between library scans for volumes missing a sidecar or a thumbnail. Also the base of the retry backoff for failed volumes. | Raise it on a very large library where a scan is expensive. |
| `concurrency` | integer | `1` | How many OCR jobs this machine runs at once (1–8). Not a pool size. At startup. | See [Concurrency](#concurrency). |
| `sessions` | boolean | `true` | Accepted so old files load, and ignored: models always stay loaded between volumes. | Never. |
| `local_processing` | boolean | `true` | Whether the machine serving the library runs OCR at all. At startup. | `false` on a small always-on server that leaves OCR to [remote processors](#remote-ocr-processors). |
| `autobench` | boolean | `true` | Benchmark a generation on a machine the first time it is seen there, and apply the widths found, before offering it volumes. At startup. | `false` if you prefer to run the derived settings and benchmark by hand. |
| `upgrade.enabled` | boolean | `false` | Replace old primary OCR with the current primary generation's (see [Replacing old OCR](#replacing-old-ocr)). | After moving from 0.5.x, when you want the whole library on the new engine. |
| `upgrade.replace` | list | `[mokuro-legacy, mokuro]` | Which existing primary sidecars may be replaced: an engine id, or `mokuro-legacy` for files written by mokuro itself. | To also re-run when you change the primary's recipe, list the primary's own engine. |
| `generations` | list | one `hayai-nova` row | The OCR recipes every volume gets a sidecar from, in the order the queue runs them. | To add OCR layers, change engines, or reorder. See below. |

How it all works underneath (scheduling, pipeline stages, benchmarks,
precision) is described in [OCR internals](ocr-internals.md). This page
covers what you set.

#### OCR backends

The recognizers (hayai-nova, paddle-manga) run on **libtorch** — the torch 0.5.2 used,
without Python — from a *backend pack* that `mokuro-bunko install-ocr` installs into
`<storage>/backends/`: `cu130` (NVIDIA, Linux and Windows), `rocm7.1` (AMD, Linux) or
`cpu`. ppocr-manga and the PP-OCR text detector run on ONNX Runtime on the CPU, as in
0.5.2. The Docker images carry their pack (`latest`: cpu, `latest-cuda`: cu130).

`ocr.backend` (and `serve --ocr`) picks the devices:

| Backend | Description |
|---------|-------------|
| `auto` | The installed pack's GPUs, else the CPU. |
| `cuda` | NVIDIA GPUs (the `cu130` pack): Turing (GTX 16xx / RTX 20xx) or newer, driver 580 or newer. Nothing else to install: the pack brings the CUDA libraries. |
| `rocm` | AMD GPUs on Linux (the `rocm7.1` pack): Radeon RX 6000 (gfx1030; RX 6600/6700 too: `HSA_OVERRIDE_GFX_VERSION=10.3.0` is set automatically), RX 7000, RX 9000. Needs `libnuma` from the system (Debian/Ubuntu: `libnuma-dev`). |
| `cpu` | CPU only (slower). |
| `skip` | This server runs no OCR of its own, the same as `local_processing: false`. |

`webgpu`, `directml` and `coreml` are still accepted: they name ONNX Runtime GPU
providers, which 0.7 releases do not include, so on a released build they leave only
the CPU (use `auto`).
`mokuro-bunko install-ocr --list` and `doctor` show which pack this machine wants and
which is installed. Without a pack, hayai-nova and paddle-manga are not offered on the
machine (ppocr-manga still is, and remote processors still work).

`skip` is not "no OCR at all": it means none on this machine. The server
still owns the queue and still serves cover thumbnails; a
[processor](#remote-ocr-processors) that logs in runs the OCR. Until one
does, the queue holds and the queue page says "No processor connected
since …". A server that must never have OCR done simply has no `processor`
account: that role is only ever given by an admin.

A provider that is not available falls back to the CPU, and the server log
says why. `mokuro-bunko doctor` shows what the build contains and whether
the models are on disk.

#### `ocr.generations`: the OCR recipes, in the order they run

A **generation** is one named recipe: an engine, the detector that feeds it,
the resolution it reads at, and how its pipeline is laid out on the hardware.
Every enabled row writes its own sidecar beside every archive, named after
the row.

```yaml
ocr:
  generations:
    - name: hayai-nova        # the primary layer: <Volume>.mokuro
      engine: hayai-nova
      detector: ppocr-manga
      primary: true

    - name: paddle-manga      # an extra layer: <Volume>.paddle-manga.mokuro
      engine: paddle-manga
      detector: ppocr-manga

    - name: ppocr-manga       # the small CPU engine, which also reads novels
      engine: ppocr-manga
      enabled: false          # kept, but not run
```

| Field | Default | Meaning |
|---|---|---|
| `id` | minted (`g-1`, `g-2`, …) | Internal and never shown. It keys the row's history, benchmarks and working directories, so renaming or reordering costs nothing but the file name. Leave it out for a new row; the server writes it back. |
| `name` | engine, or `<engine>-<detector>` | The row's label **and its file-name postfix**: `<Volume>.<name>.mokuro`. Rules below. |
| `primary` | `false` | Exactly one enabled row must have it. That row writes the bare `<Volume>.mokuro`, which readers take character counts from. Every OCR file of a volume carries the same volume id: the `.mokuro`'s when one exists, else the id readers already give a volume with no `.mokuro`. Any engine may be the primary one. |
| `enabled` | `true` | A disabled row queues nothing and writes nothing. Its name stays reserved. |
| `engine` | — (required) | One of the [engines](#ocr-engines). |
| `detector` | `ppocr-manga` | The [text detector](#ocr-detectors) for `hayai-nova` and `paddle-manga`. Ignored (and stored as `null`) for `ppocr-manga`, which brings its own. |
| `patch_budget` | `512` | `256`, `384` or `512`: the resolution `hayai-nova` reads each line at. Reaches no other engine. |
| `precision` | `auto-accuracy` | The row's [precision mode](#precision-modes), the same on every machine: `auto-accuracy`, `auto-balanced`, `auto-speed`, or `fp32` / `bf16` / `fp16` only. `ppocr-manga` fixes its own and ignores it. |
| `pools` | `{}` (derive) | Where each stage of this row runs and how wide. See [Pools](#pools-stages-and-devices). |

**The order is the setting.** The queue runs the rows top to bottom: every
volume gets the first row's sidecar before any volume gets the second row's,
so a fast layer reaches readers quickly while a slower one fills in behind
it. The order is a priority, not a dependency: a layer never waits for
its volume's primary `<Volume>.mokuro`, so machines with nothing earlier to
do (or that finish it sooner) run a volume's other rows while its first one
is still running. Within a row, series take turns and each series is read in
natural volume order (`Volume 2` before `Volume 10`, `第二巻` before `第十巻`).

**Names are file names.** A name must match `^[a-z0-9][a-z0-9-]{0,31}$`:
lowercase ASCII letters, digits and hyphens, at most 32 characters, starting
with a letter or digit. No dots, capitals, spaces or underscores: readers
would not recognise the file as a layer. `original`, `gcv`, `updated-ocr`
and anything starting with `tr-` are reserved by readers. Names must be
unique across all rows, enabled or not.

**Renaming a row** means its files are now looked for under the new name:
volumes are OCRed again under it, and the old files stay where they are. A
job already running finishes under the name it started with. **Deleting or
disabling a row** leaves its files alone too; every layer file goes when its
archive is deleted. To take over files that are already on disk (for example
`Volume 01.paddle-manga.mokuro`), name the row after them.

**Editing applies live.** Generations saved in the admin panel reach the
running worker at once. A running job is stopped (and not counted as a
failure) only when its row is removed, disabled, or changes engine, detector
or patch budget; renames, reorders and pool changes never stop a job.

**Volumes missing pages** get no other layers: when a volume arrives with a
`.mokuro` that names pages its archive does not contain, every layer would
have the same holes. This is checked as soon as the volume is found, before
any OCR runs. Replace the archive with a complete one and the other rows pick
it up. A volume without a `.mokuro` is never held back: its primary sidecar
is read from the archive itself.

Set the list in the admin panel (OCR → Generations), in the config file, or
as JSON:

```bash
mokuro-bunko config set ocr.generations '[{"name":"hayai-nova","engine":"hayai-nova","primary":true}]'
MOKURO_OCR_GENERATIONS='[{"name":"hayai-nova","engine":"hayai-nova","primary":true}]'
mokuro-bunko serve --generations '[{"name":"hayai-nova","engine":"hayai-nova","primary":true}]'
```

**Old configs.** A 0.5.x list that names the removed `mokuro` engine or the
removed `ctd`, `animetext` or `rtdetr` detectors still loads; it is migrated
and every change is logged and shown in the admin panel:

- a `mokuro` row is **retired**: kept (its name stays reserved) but never run,
  no longer primary, and written back disabled;
- a row on a removed detector moves to `ppocr-manga`;
- if no runnable primary is left, an existing `hayai-nova` row becomes the
  primary, or a new `hayai-nova` primary is added.

Existing `<Volume>.mokuro` files are untouched. See
[MIGRATING-0.7.md](MIGRATING-0.7.md).

> Configs from pre-release builds that still use `ocr.engines`,
> `ocr.detector`, `ocr.patch_budget` or `ocr.char_map` (or the matching
> `MOKURO_OCR_*` variables) are refused at startup with a message saying what
> to write instead.

#### Replacing old OCR

By default a volume that already has a `<Volume>.mokuro` is considered done,
whoever wrote it. Turn on `ocr.upgrade.enabled` to bring such volumes to the
current primary generation:

- A volume is a candidate when its primary sidecar's recipe family is listed
  in `ocr.upgrade.replace` and differs from the primary row's recipe. The
  family is the engine id, or `mokuro-legacy` for a file written by mokuro
  itself (no `ocr_engine` block).
- It is skipped when its archive is **missing pages**, or when a person
  **edited** the sidecar (an audit-log write by a user account, or a file
  newer than the OCR provenance row). An admin can force one edited volume.
- If another layer already holds the primary's recipe made from the current
  archive, its bytes become the new primary at once. Otherwise the volume
  gets an upgrade job, queued behind every ordinary job (a volume with no OCR
  always beats one with old OCR) and runnable on any machine.
- The old primary is kept as a layer named after its generation (`mokuro`
  for mokuro-legacy files; `-prev`, `-prev2`, … if that name is taken). A
  crash mid-way never leaves a volume without its primary. The `volume_uuid`
  is preserved, so reading progress is untouched, and the swap is audited as
  `ocr_sidecar_upgraded`.

The admin API serves a census and per-volume actions
(`GET /_admin/api/ocr/upgrade`, `POST …/upgrade/<volume>` with `force`,
`POST …/upgrade/<volume>/revert` to swap the kept layer back).

#### OCR engines

All engines are Apache-2.0. hayai-nova and paddle-manga run on libtorch (the backend
pack), ppocr-manga on ONNX Runtime on the CPU.

| Engine | What it is | Runs on | Use it for |
|--------|-----------|---------|------------|
| `hayai-nova` | [hayai-ocr v2.5 "Nova"](https://huggingface.co/JustANormalTinkerer/hayai-ocr-v2.5-nova) | card or CPU | The default. Better text than 0.5's mokuro engine, especially display lettering, sound effects and colour pages. Reads at the row's `patch_budget`. Needs a detector. |
| `paddle-manga` | [PaddleOCR-VL 1.6 + manga LoRA](https://huggingface.co/sorryhyun/paddleocr-vl-1.6-manga-lora) | card recommended | The most accurate and the slowest; about 2 GB of model files. Needs a detector. |
| `ppocr-manga` | [PP-OCRv6 manga](https://huggingface.co/Kellenok/PP-OCRv6_manga) line detector + CTC recognizer, 23 MB | CPU only | Reads lines rather than bubbles, removes furigana, and is the one engine that reads scanned **novel** pages. Brings its own detector. |

Sidecars from every engine, and every non-primary layer, carry an
`ocr_engine` block saying what produced them (engine, detector, generator
and, where they apply, the patch budget, precision and the model revisions).
The mokuro engine of 0.5.x no longer exists; sidecars it wrote stay as they
are.

#### OCR models

Models are not shipped in the archives. A full build downloads what the
configured generations need into `<storage>/models/` the first time they run,
verifies each file's sha256 and only then moves it into place: the PP-OCR ONNX
files, hayai-nova's and paddle-manga's host tables (tokenizer, embeddings), and
the **compiled libtorch packages** for this machine's devices
(`models/torch/<engine>/<precision>/<target>/`, e.g. `linux-cuda-sm_89`,
`linux-rocm-gfx1201`, `linux-cpu-x86_64-v3`, with the shared weights next to them).
`mokuro-bunko install-ocr` and `models download [--engine E]` fetch them up front
(for example before going offline); `models list|verify` inspect them. Sizes:
hayai-nova about 0.5 GB per precision, ppocr-manga about 23 MB, paddle-manga
about 2 GB per precision.

| Setting | Meaning |
|---|---|
| `MOKURO_MODELS_DIR` | A directory of model files to use instead of the store (also `MOKURO_PPOCR_MODELS`). Files found there are used as they are. |
| `MOKURO_MODELS_DOWNLOAD` | `0`, `false`, `no` or `off` forbids downloads (also `MOKURO_PPOCR_DOWNLOAD`). A model that is not on disk then makes the generation fail. |
| `MOKURO_TORCH_MODELS_DIR` | A directory laid out like `<storage>/models/torch/` searched first (development, air-gapped hosts). |
| `MOKURO_TORCH_PACK`, `MOKURO_BACKENDS_DIR` | Use this backend pack directory / look for packs here instead of `<storage>/backends/` (the Docker images set `MOKURO_TORCH_PACK`). |
| `MOKURO_TORCH_THREADS` | CPU threads of a CPU recognizer (default: the session's share of the physical cores). |

#### OCR detectors

`hayai-nova` and `paddle-manga` only read text; a detector finds it. The one
detector is `ppocr-manga` (Apache-2.0): rotated per-line boxes with furigana
separated, on the CPU. The page is read first by PP-OCRv6, then the row's
engine reads every line again and the two reads are merged line by line. It
is also the pairing to use for **novels**.

The 0.5 detectors `ctd` (GPL-3.0), `animetext` (GPL-3.0) and `rtdetr` were
removed; a row naming one is switched to `ppocr-manga` when the config loads.

#### Pools: stages and devices

Each engine reads a page in **stages**: detection, recognition, assembly.
Each stage has a pool of workers. A row's `pools` sets them for that row;
`{}` (the default) derives everything from the host, which is the right
starting point. None of these settings changes what is written, only how fast
it is written, so editing them never stops a running job.

```yaml
    - name: hayai-nova
      engine: hayai-nova
      pools:
        stage_workers: {detect: 2}          # pages the detect stage runs at once
        queue_capacity: {detect: 4}         # pages that may wait after it
        stage_device: {detect: cpu, engine: "gpu:0"}
```

| Key | Values | Meaning |
|---|---|---|
| `stage_workers` | `{stage: 1–64}` | How many workers a stage runs. A stage holding a model on a card runs one worker; for the `engine` stage a number above 1 instead runs that many recognizer copies on the card (up to 8), only worth it on a card with spare memory and compute. |
| `queue_capacity` | `{stage: 1–256}` | How many pages may wait in the queue a stage fills. Capacity is memory (a decoded page can be ~14 MB); the default is small on purpose. |
| `stage_device` | `{stage: cpu \| "gpu:<n>"}` | Where a stage's model runs. Absent means auto: card 0 when there is one, else the CPU. Only stages holding a model take one. |

Which stage names a row has depends on its engine; a name from another road
is refused when the config loads:

| Row | Stages | Take a device |
|---|---|---|
| `hayai-nova` / `paddle-manga` | `detect` → `engine` → `post` | `detect` (CPU only), `engine` |
| `ppocr-manga` | `detect` → `layout` | `detect` (CPU only) |

Some placements worth knowing:

- **Detector on the CPU, recognizer on the card** is the normal layout: the
  whole card goes to the recognizer and the detector is a CPU pool.
- **Two rows on two cards** (`gpu:0`, `gpu:1`) with `ocr.concurrency: 2` run
  side by side.
- **Everything on the CPU** is for a machine with no card.

The admin panel's pools table offers only the devices a machine really has,
and shows what each blank cell derives to. A machine's pools never carry a
precision: that is the row's [precision mode](#precision-modes). A
`precision` left in an older file's pools is read without error.

#### Precision modes

A row's `precision` is ONE mode for every machine that runs it (this server
and every processor); each machine works out what it means on its own card:

| Mode | What it picks |
|---|---|
| `auto-accuracy` (default) | What tested most accurate for the engine, among the formats the machine supports. Fixed, no benchmark involved. |
| `auto-balanced` | Gives up a little accuracy for a lot of speed. Each machine benchmarks the candidates and keeps the fastest. |
| `auto-speed` | The fastest format each card runs well, by benchmark. |
| `fp32`, `bf16`, `fp16` | That format only. |

For `auto-balanced` and `auto-speed`, each machine benchmarks the generation
automatically before its first volume, and again when the mode changes, and
keeps the fastest: it tries every candidate its card supports on the same
sample pages; two within 5% of each other count as a tie, and the tie goes to
the more accurate one. The pick is kept with the machine's benchmark of the
row; changing the row's mode, or the machine's candidates changing, makes it
stale and it is measured again. A machine whose pools you set by hand is
never width-tuned, but still gets this precision-only benchmark. Only with
automatic benchmarks off (`ocr.autobench: false`), or when a machine's
benchmark failed, does it run the first candidate its card supports.

Each device reports the formats it computes in (the backend pack's
`bt_devices`): GPUs fp32, fp16 and bf16 (as 0.5.2's torch probe said); the CPU
fp32, plus bf16 on CPUs with AVX512-BF16 (Zen 4/5, Sapphire Rapids and newer).
A format also needs its compiled package for the device; one without is "not
supported" there. On the CPU, `auto-accuracy` runs fp32, as 0.5.2 did; bf16 on
the CPU is an opt-in (force `bf16`). A **forced** mode (`fp32`, `bf16`, `fp16`) runs only on machines
that support it; every other machine is not eligible for the row and is never
offered its volumes. When no connected machine can run it, the row is
**held**: the admin card, the queue page (for admins) and `.mokuro-queue.json`
(jobs in state `held`) say "No connected machine can run …". `ppocr-manga`
fixes its own precision and ignores the mode. Each sidecar records the
precision it was read at.

#### Benchmark and tune

Every row in the admin panel (OCR → Generations) has a **Benchmark & tune**
button. It measures the row **as currently edited**, saved or not, on the
selected machine (this server, or a connected processor) and on pages sampled
from your own library: 32 by default (4–512), a few interior pages from each of
several volumes with the series taking turns, so covers and ads do not flatter
the result.

- It **pauses that machine's OCR** for the duration: jobs running there are
  stopped and simply run again afterwards; they are not recorded as
  failures. Other machines keep working. The queue page shows the machine as
  "Benchmarking …" (or "Auto configuring …" for an automatic benchmark).
- It loads the models once, as the row would run, and warms up on the first
  pages. The load appears only as "first page after X s"; it is never part of
  a speed.
- For an `auto-balanced` or `auto-speed` row it first runs one trial per format
  the machine's card supports (see [Precision modes](#precision-modes)); the
  fastest wins, and two within 5% count as a tie that goes to the more
  accurate format. A format with no compiled package for the card is skipped.
- It then tries pool widths, following the pipeline's own bottleneck: it
  widens the stage the pipeline says is in the way while that gains at least
  3%, then gives back workers that are not needed while the speed stays within
  1% of the best. The detector always runs on the CPU, so there is no
  placement to search. At most 8 width trials (plus the format trials) and
  15 minutes.
- Every speed is timed by when pages come out of the pipeline: each trial
  feeds the sample again, as one continuous run, until it has at least 20
  seconds of output to measure. A trial measured over less than 10 seconds is
  shown but flagged, and never decides anything.
- It reports pages per second, what a 200-page volume would take, how long
  the rest of the queue would take at that rate, how busy the GPU and CPU
  were during each trial (sampled on the machine that ran it), the process's
  peak memory and how far the card's VRAM use rose. **Apply** writes the
  widths it found into the row you are editing (or into that processor's
  settings); you still save.
- Benchmarks queue: several can be requested at once and run one at a time
  per machine; different machines' benchmarks run side by side.

The last result of each saved row on this server is kept in
`<storage>/.ocr-bench.json`; a processor's results are kept in its profile
(`<storage>/processors/`). A benchmark of an unsaved row is kept in memory
only, until a restart.

With `ocr.autobench: true`, a row that has never been measured on a machine
(or whose measurement no longer matches its precision mode or that machine's
card) is benchmarked there automatically before that machine is offered its
volumes, and the widths found are kept in that machine's profile
(`<storage>/processors/`), never in `config.yaml`. On this server that
happens only for a row whose pools table is empty; set any value in the
table and the table is used as written, though a balanced/speed row still
gets its precision-only benchmark. A machine whose automatic benchmark fails
runs the row untuned (and on its first supported format) rather than never.

#### Concurrency

`ocr.concurrency` is how many **volumes** this machine OCRs at once, each
with its own copy of the models. It is not how wide the stages inside one
volume run; that is a row's `pools`. Models stay loaded between volumes.

- Two jobs never run the same generation of a volume; different generations
  of one volume may run at once.
- One job already keeps several CPU cores busy, so about `cores / 3` is a
  useful ceiling on the CPU; on a card the limit is usually card memory,
  since every slot holds its own model. Values above 8 are refused.
- A GPU-bound engine gains least: the slots queue for the same card. Two
  cards are where it pays most.
- It takes effect at startup.

#### Remote OCR processors

OCR can run on a machine other than the one serving the library: typically
a small always-on server (the lite build) for the library and a stronger
computer, switched on when it is used, for the OCR. The processor logs in to
the library like any client (it dials out, so nothing has to be opened on it,
and it works behind NAT), receives archives, runs the OCR and sends the
sidecars back. The library keeps the volumes, the users, the sidecars and
the statistics. The library and its processors must both be 0.7: they speak
**protocol v3**.

**On the library server**, create an account with the `processor` role for
each processor:

```bash
mokuro-bunko admin add-user gpu-box --role processor    # asks for the password
```

(`admin restore-user` brings back a deleted account; a deleted name cannot be
added again.) If the library's own hardware should do no OCR, set
`ocr.local_processing: false` (or `ocr.backend: skip`; a lite build never runs
OCR locally): it then hands every generation to connected processors. With
local processing on, the library server is simply one more machine sharing
the queue.

**On the processor**, install a **full** build of the same release (no
Python, git or checkout needed), then let `processor setup` do the rest: it
checks the account against the library, writes `processor.yaml` and offers
to run the processor as a service (a systemd user unit on Linux, a launchd
agent on macOS, a Startup entry on Windows):

```bash
mokuro-bunko processor setup
```

The same steps one at a time, with a `processor.yaml` written by hand (start
from [`docs/processor.example.yaml`](processor.example.yaml)):

```bash
mokuro-bunko processor serve   --config processor.yaml
mokuro-bunko processor service --config processor.yaml --install   # start it with the machine
mokuro-bunko processor status  --config processor.yaml             # what it last did
```

The models a processor needs are downloaded the first time a generation
runs (see [OCR models](#ocr-models)). A processor with no models yet
registers anyway and shows as installing.

`processor.yaml`:

| Key | Default | Meaning |
|---|---|---|
| `library.url` | — (required) | The library's address, as a browser reaches it. A path prefix is kept. |
| `library.username`, `library.password` | — (required) | The `processor` account. Keep the file mode `600`. |
| `library.password_file` | unset | The first line of this file (relative to `processor.yaml`) replaces `library.password`. `MOKURO_PROCESSOR_PASSWORD` overrides both. |
| `library.tls_verify` | `true` | `true`, `false` (a self-signed certificate on a LAN), or the path to the certificate to trust. |
| `processor.name` | the hostname | How the library shows this machine to admins, and the name its settings are filed under. |
| `processor.public_name` | unset | What queue-page visitors see; unset, they see "machine 1", "machine 2"…. At most 64 characters. |
| `processor.max_sessions` | `1` | How many OCR pipelines this machine runs at once: one per card is the rule, like `ocr.concurrency`. |
| `processor.storage` | `$XDG_DATA_HOME/mokuro-bunko-processor` (`~/.local/share/...`) on Linux and macOS, `%LOCALAPPDATA%\mokuro-bunko-processor` on Windows | Logs, working directories, archives too big for memory, and the status file. One per processor; a second processor on the same storage refuses to start. |
| `processor.archive_memory_mb` | `2048` | RAM for the archives being read and the one on deck, shared by every session (in-memory files on Linux); an archive that does not fit goes to `storage`. `0` keeps every archive on disk. Other platforms always use `storage`. |

A 0.5 `ocr:` section in the file (`ocr.backend` chose a torch build) is
accepted and ignored. `processor setup --backend` picks the devices (`auto`,
`cuda`, `rocm`, `cpu`); run `mokuro-bunko install-ocr` on the processor machine
first (as 0.5.2 needed its OCR environment).

**How work is shared.** Each volume goes to the machine predicted to finish
it first, counting what that machine is already doing, whether it has to
load the model, and its measured speed on that generation, so a slower
machine may leave a volume for a faster one that will be free soon. If the
faster machine has not taken it within 15 seconds of when it was expected
to, anyone may. Until every machine has a measured speed for a row, the
first free machine simply takes the next volume.

**Per-machine settings.** Pools, devices and benchmarks belong to a machine:
widths tuned on a 16-core box mean nothing on a 48-core one. While a
processor is connected, a generation's pools table has a machine selector;
its Device select lists that machine's cards, **Save for …** stores the
values in that machine's profile (never in `config.yaml`), and
**Benchmark & tune** runs on it. A processor with no saved values uses the
generation's own table (or what autobench found). Changing a row's engine,
detector or patch budget makes every machine measure it afresh.

**When things go wrong.** A processor that disconnects mid-volume gives its
volumes back to the queue unrecorded, and reconnects with a backoff from
5 seconds to 5 minutes. If the library has no processor connected and does
no OCR itself, the queue holds and says so. Disabling, deleting, re-roling
or changing the password of a processor's account cuts it off within about
15 seconds; its failed login is shown in the admin panel's Processors card.
The Processors card also shows each machine's hardware, what it is running,
its real throughput per generation and how its downloads are going.

**Versions.** A processor of another protocol version is refused at
registration ("this server speaks protocol 3, not 2"). Update the library
and its processors together (see
[deployment](deployment.md#updating-a-library-and-its-processors)).

**Behind a reverse proxy**, the `/_processor/` paths need WebSocket upgrades,
request buffering off and no body size limit; see
[Remote OCR processors behind a proxy](deployment.md#remote-ocr-processors-behind-a-proxy).
[Deployment](deployment.md#remote-ocr-processors) has a complete walkthrough
for Linux, Windows, macOS and Docker.

#### `install-ocr`

`mokuro-bunko install-ocr` installs the OCR backend pack for this machine and
fetches the models, like 0.5.2's command installed its OCR environment:

```
mokuro-bunko install-ocr [--variant auto|cpu|cu130|rocm7.1] [--from DIR] [--dir DIR]
                         [--no-models] [--force] [--list]
```

`auto` (default) picks `cu130` for an NVIDIA driver 580 or newer, `rocm7.1` for
a supported AMD GPU on Linux, else `cpu`. The pack comes from this version's
release, checked against the signed release manifest; the `cu130` pack's NVIDIA
CUDA libraries are fetched from NVIDIA's own packages on PyPI (about 1.6 GB),
each file pinned by sha256. `--from DIR` installs from downloaded files (air-gapped
hosts); `--list` shows the detected hardware and the installed packs. 0.5.2's
`--backend cuda|rocm|cpu|auto` still works; `--engines`/`--detector` are ignored.
Unlike 0.5.2 it fails visibly (non-zero exit) when it cannot install.

#### OCR files and logs

| File | What it holds |
|---|---|
| `<storage>/logs/server.log` | The server log (rotated at 2 MiB, 5 backups), including OCR and queue messages. |
| `<storage>/.ocr-failures.json` | Failed volumes, per generation, with attempt counts. Retries back off from `poll_interval` up to 1 hour; replacing the archive clears its record. |
| `<storage>/.ocr-bench.json` | The last benchmark of each saved row. |
| `<storage>/.ocr-congestion.json` | The last few runs of each row, shown as the Congestion column. |
| `<storage>/processors/*.json` | Each machine's profile: hardware, per-generation pools and benchmarks (`@local.json` is this server's own). |
| `<storage>/backends/` | The OCR backend pack (`install-ocr`). |
| `<storage>/models/` | The downloaded models: compiled libtorch packages and PP-OCR ONNX files. |

## Compiled metadata files

The server compiles two files into the shared library and keeps them current:

| File | Contents |
| --- | --- |
| `<Series>/series.json` | The series' facts (external ids, titles, synonyms, tag, unit) plus an index of its volumes: uuid, title, page and character counts, mokuro version, spine width, archive size, freshness stamps, OCR hash and shelf offsets. |
| `catalog.json` (library root) | One entry per series folder with the same facts — name, mapping and search data only. |

Both are regenerated when the library changes and whenever a client submits an
update, and are rewritten only when their content actually changed, so clients can
cache them on size/mtime.

Each volume entry may also carry `mokuro_size`/`mokuro_modified` and
`cover_size`/`cover_modified`: the byte size and integer epoch-second mtime of the
`.mokuro` sidecar and the cover `.webp`, taken from a plain filesystem stat when the
entry is compiled. Either pair is omitted (never `null`) when its file doesn't
exist. A client uses these to decide whether its own cached copy is stale without
downloading anything: rebuild when the stamped size differs from what it has, or
the stamped `_modified` is strictly newer than what it stored; an older-or-equal
`_modified` at an equal size is fresh. Stamps are always whole seconds, never
sub-second, because a generic WebDAV client only ever sees second-precision
`Last-Modified` HTTP dates.

A volume entry may also carry `mokuro_sha256`, right after `mokuro_modified`: the
lowercase hex SHA-256 of the volume's primary OCR sidecar (`<volume>.mokuro`, else
`<volume>.mokuro.gz`; never an OCR layer `<volume>.<id>.mokuro`), taken over its
JSON bytes as stored, after gunzip for `.gz` — so the same JSON hashes the same
plain or compressed. It is omitted when the volume has no sidecar or the sidecar
does not parse. A reader that recorded the hash when it installed a volume's OCR
compares it with the published one and re-downloads only a `.mokuro` that changed
(a re-OCR, a newer mokuro, a new primary generation). Like the stamps it is index
data: it never moves the series' `updated_at`, and a hash in a client's `PUT` is
ignored. The hash is computed from the same read that compiles the entry and is
cached with it; entries cached by an older version gain it on the next background
metadata pass (one read of each sidecar, never on a request).

Clients do not write these files. A `PUT` of `<Series>/series.json` is accepted as
an update *request*: the facts are validated and merged (newest stamp wins), the
volume list in the request is ignored in favour of the server's own compilation,
and both files are regenerated. A body carrying only facts, with no volume list at
all, is an equally valid update. Writing `catalog.json`, or deleting/moving either
file, is refused for every account. Submitting an update is ownership-gated, not a
plain progress-write permission: an editor-tier account (or above) may update any
series, an uploader account only a series it uploaded, and a registered-only
account cannot submit updates at all. The account that submitted an accepted
update is recorded in the audit log.

## Environment Variables

Every config key can be set as `MOKURO_<SECTION>_<KEY>`, which overrides the
config file:

```bash
MOKURO_SERVER_HOST=127.0.0.1
MOKURO_SERVER_PORT=9000
MOKURO_SERVER_THREADS=2
MOKURO_REGISTRATION_MODE=invite
MOKURO_SSL_ENABLED=true
MOKURO_OCR_LOCAL_PROCESSING=false
MOKURO_UPDATE_CHECK=false
MOKURO_OCR_GENERATIONS='[{"name":"hayai-nova","engine":"hayai-nova","primary":true}]'
```

Booleans take `true`/`false` (or `1`/`0`, `yes`/`no`). Lists such as
`MOKURO_SERVER_TRUSTED_PROXIES` are comma-separated. `MOKURO_OCR_GENERATIONS`
is the JSON text of the whole list. `cors.allowed_origins` has no variable;
set it in the file or with `mokuro-bunko config cors-add`; `ocr.upgrade.*`
has none either. Bad values are refused at startup with the variable's name
in the message. Overrides are never written back into `config.yaml` by the
CLI or the admin panel.

Other variables:

| Variable | Default | Meaning |
|---|---|---|
| `MOKURO_CONFIG` | platform default | Config file path (the same as `--config`). |
| `MOKURO_HOST`, `MOKURO_PORT`, `MOKURO_STORAGE` | — | Short aliases for `server.host`, `server.port`, `storage.base_path`. |
| `MOKURO_LOG` | `info` (`debug` with `-v`) | The log filter (`tracing` syntax), for example `debug` or `info,bunko_server=debug`. |
| `MOKURO_NGINX_ACCEL` | unset | `1` when an nginx in front serves library downloads via `X-Accel-Redirect`. Set by the Docker images only when their nginx runs; never set it without that nginx. |
| `MOKURO_SETUP_TOKEN` | generated | The one-time token that lets first-run setup (`/setup?token=…`) be opened from a browser that is not on the server machine, e.g. under Docker bridge networking. Unset, the server creates `<storage>/.setup-token` and logs the setup URL at startup while no admin exists; the file is removed when setup completes. |
| `MOKURO_INSTALL_KIND` | detected | `self`, `docker` or a package manager's name; decides what the updater does (see [Updates](#updates)). |
| `MOKURO_MODELS_DIR`, `MOKURO_MODELS_DOWNLOAD` | — | A directory of model files to use; `0` forbids model downloads (see [OCR models](#ocr-models)). The 0.5 names `MOKURO_PPOCR_MODELS` and `MOKURO_PPOCR_DOWNLOAD` still work. |
| `MOKURO_PROCESSOR_CONFIG` | — | `processor.yaml` path for the `processor` commands (the same as `--config`). |
| `MOKURO_PROCESSOR_PASSWORD` | — | The processor's password; overrides `library.password` and `password_file`. |

The 0.5 variables `MOKURO_THREADS`, `MOKURO_DEBUG`, `MOKURO_EFT_TRACE`,
`MOKURO_BUNKO_OCR_ENV`, `MOKURO_BUNKO_OCR_ENGINES_ENV`,
`MOKURO_BUNKO_MOKURO_SPEC`, the `MOKURO_PPOCR_*` tuning variables and the
`MOKURO_OCR_STAGE_*` / `MOKURO_OCR_CPU_WORKERS` pipeline overrides are no
longer read. The Docker-only variables (`PUID`, `PGID`, `UMASK`,
`TAKE_OWNERSHIP`, `MOKURO_BACKEND_PORT`) are described in
[Deployment](deployment.md#docker).

## Example Configurations

### Local Development

```yaml
server:
  host: "127.0.0.1"
  port: 8080

registration:
  mode: "self"

cors:
  enabled: true
  allowed_origins:
    - "http://localhost:*"

ocr:
  # No OCR on this machine; the queue page says it is holding for a processor.
  backend: "skip"
```

### Production (Behind Reverse Proxy)

```yaml
server:
  host: "127.0.0.1"
  port: 8080

storage:
  base_path: "/var/lib/mokuro-bunko"

registration:
  mode: "invite"
  default_role: "registered"

cors:
  enabled: true
  allowed_origins:
    - "https://reader.mokuro.app"
    - "https://your-domain.com"

ssl:
  enabled: false  # Handled by reverse proxy

admin:
  enabled: true

ocr:
  backend: "cuda"   # the cu130 pack's NVIDIA GPUs; "auto" picks them as well
  poll_interval: 60
```

### Lite library server with a remote GPU processor

```yaml
server:
  threads: 1          # a one-core VPS
  cache_mb: 16

ocr:
  local_processing: false     # this box serves the library only
  generations:
    - name: hayai-nova
      engine: hayai-nova
      detector: ppocr-manga
      primary: true
    - name: paddle-manga
      engine: paddle-manga
      detector: ppocr-manga
```

Then create a `processor` account and run `mokuro-bunko processor setup`
on the GPU machine (see [Remote OCR processors](#remote-ocr-processors)).

### Public Read-Only Server

```yaml
server:
  host: "0.0.0.0"
  port: 8080

registration:
  mode: "disabled"

cors:
  enabled: true
  allow_credentials: false

admin:
  enabled: false

ocr:
  # No OCR on this machine. With no `processor` account, none anywhere: the
  # queue holds (and the queue page says so) for a processor that never comes.
  backend: "skip"
```

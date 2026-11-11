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
back to the same file, together with any values set by `MOKURO_*` variables
or `serve` flags (see [Environment Variables](#environment-variables)).
[`config.example.yaml`](../config.example.yaml) is a commented starting point.

## Configuration Options

### Server

```yaml
server:
  host: "0.0.0.0"  # Host to bind to
  port: 8080       # Port to listen on
  trusted_proxies: []  # Reverse proxies on other hosts, e.g. ["172.16.0.0/12"]
  threads: 0       # Async worker threads (0 = min(cores, 4))
  cache_mb: 32     # Memory budget of the in-memory caches
  serve_readers: true  # Serve the catalog and the reader WebDAV
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `host` | string | `"0.0.0.0"` | Network interface to bind to. Use `127.0.0.1` for local-only access. |
| `port` | integer | `8080` | TCP port for the server. |
| `trusted_proxies` | list | `[]` | Networks or addresses of reverse proxies whose `X-Real-IP` / `X-Forwarded-For` are believed. Only a loopback peer (such as the Docker image's own nginx) is trusted without an entry. Add one for a proxy on another host or container, or on the host in front of a published Docker port (the server then sees the Docker gateway address); otherwise every client is counted as the proxy and login rate limits are shared. With `MOKURO_NGINX_ACCEL=1` the bundled nginx already takes the client from `X-Forwarded-For` of private-range proxies. Set as `MOKURO_SERVER_TRUSTED_PROXIES=172.16.0.0/12,10.0.0.5`. |
| `threads` | integer | `0` | Worker threads of the runtime that serves requests, 0-256. `0` means the smaller of the CPU count and 4. Background work (metadata compiles, cache refreshes, thumbnails, OCR control) runs on a separate runtime of two threads and never takes these. Each connected processor costs a socket, not a thread, so this rarely needs raising. Replaces 0.5's `MOKURO_THREADS`. At startup. |
| `cache_mb` | integer | `32` | Byte budget, in MiB (1-65536), of the in-memory caches: half for WebDAV `Depth: infinity` listings, a quarter for the library index (a soft cap). Raise it for a very large library on a machine with spare RAM, lower it on a tiny host. At startup. |
| `serve_readers` | boolean | `true` | New in 0.8. `false`: readers do not connect to this server. `/catalog` (page, API and manifest) answers 404, and WebDAV is not served, except that processors can still `GET` archives under `/mokuro-reader/`; everyone else gets 403 there (401 when not signed in). Admin, setup, login, the queue page and `/_processor/*` stay. Use it for a cloud library that readers open directly (see [Library location](#library-location)). Set as `MOKURO_SERVE_READERS`. |

### Storage

```yaml
storage:
  base_path: "/var/lib/mokuro-bunko"
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `base_path` | string | Platform-specific | Base directory for all data storage. |
| `library` | mapping | local | Where the library lives; see [Library location](#library-location). |

Default paths:
- **Linux/macOS**: `$XDG_DATA_HOME/mokuro-bunko` (`~/.local/share/mokuro-bunko` when `XDG_DATA_HOME` is unset)
- **Windows**: `%LOCALAPPDATA%\mokuro-bunko`; the portable zip uses the `data\` folder next to `mokuro-bunko.exe`
- **Docker**: `/data` (the images set `MOKURO_STORAGE=/data`)

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
├── inbox/                   # Legacy 0.5 directory: created and must be writable, but unused
├── users/                   # Per-user reading progress
│   └── alice/volume-data.json, profiles.json, goals.json, positions.json,
│       history/<device>/…   # reading history (≤ 16 MiB a file, 256 MiB an account)
├── logs/                    # server.log (rotated to .1–.5), *-console.log from the desktop app/tray
├── processors/              # Per-machine OCR profiles (remote processors)
├── backends/                # OCR backend pack: libtorch for CUDA / ROCm / CPU (install-ocr; full build)
├── models/                  # Downloaded OCR models: compiled libtorch packages, PP-OCR ONNX (full build)
├── cloud/                   # Cloud library only: index, state, caches, upload spool, credentials
└── mokuro.db                # SQLite database
```

With a cloud library or a library folder you chose (`storage.library.backend`
other than `local`) Bunko's own `library/` folder is not used: the library is the
cloud folder (or `storage.library.path`), and `cloud/` holds Bunko's local state
for it.

#### Library location

New in 0.8. The library is `base_path/library/` (`local`, the default), a folder
you choose (`folder`: the one Mokuro Reader's Local Folder uses), or the Mokuro
Reader folder of a cloud account.

```yaml
storage:
  library:
    backend: local          # local | folder | gdrive | onedrive | mega | webdav
    folder: mokuro-reader   # the library folder in the cloud account
    path: null              # backend folder: the library folder, e.g. /srv/manga or D:\Manga
    webdav: {url: null, username: null, allow_http: false}
    cache_mb: 1024
    spool_gb: 4
    outbox_hold_mb: 2048
    outbox_max_gb: 8
    quiet_s: 120
    poll: {active_s: null, idle_s: null}
    drive_background_units_per_min: 20000
    status_files: true
    dedup: true
    progress_user: null
    google_project: release # release | beta | custom
    oauth:
      google: {client_id: null, client_secret: null, device_client_id: null, device_client_secret: null}
      microsoft: {client_id: null}
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `backend` | string | `local` | `local`, `folder` (a folder you choose), `gdrive` (Google Drive), `onedrive`, `mega` or `webdav`. `MOKURO_LIBRARY_BACKEND`. |
| `folder` | string | `mokuro-reader` | The library folder in the cloud account (the one Mokuro Reader uses). `MOKURO_LIBRARY_FOLDER`. With `backend: folder` it is only the name the library's top folder is known by, whatever it is called on disk: leave it as it is. |
| `path` | string | — | Required for `folder`: the library folder, a full path as the server sees it (`/srv/manga`, `D:\Manga`, `\\files.example.com\manga`; `~` is expanded). It may not be Bunko's `base_path`, hold it, or lie inside it, except in `base_path/library/` (so a library on this computer can become a folder library in place). Ignored by the other backends. `MOKURO_LIBRARY_PATH`. |
| `webdav.url` | string | — | The WebDAV server's folder URL; required for `webdav`. `http://` is refused unless `webdav.allow_http` is `true`, except for a server on this machine (`localhost`, `127.0.0.0/8`, `[::1]`). `MOKURO_WEBDAV_URL`. |
| `webdav.username` | string | — | `MOKURO_WEBDAV_USERNAME`. |
| `webdav.allow_http` | boolean | `false` | Allow a plain `http://` URL (the password then travels unencrypted). Not needed for a loopback host (`localhost`, `127.0.0.0/8`, `[::1]`). `MOKURO_WEBDAV_ALLOW_HTTP`. |
| `cache_mb` | integer | `1024` | Local cache of cloud files, in MiB. |
| `spool_gb` | integer | `4` | Room for archives being OCR'd, in GiB; at least 2 × OCR lanes × the largest archive. |
| `outbox_hold_mb` | integer | `2048` | New OCR claims wait while this many MiB of results are still uploading. At most `outbox_max_gb`. |
| `outbox_max_gb` | integer | `8` | Uploads beyond this many GiB in the outbox are refused. |
| `quiet_s` | integer | `120` | Seconds a removed file counts as recently removed (a reader replacing a file deletes it first). |
| `poll.active_s`, `poll.idle_s` | integer | provider default | How often the cloud folder is checked for changes, while busy and while idle. A library folder is watched, so changes usually show within seconds; these are its fallback (60 s and 300 s), for network shares and Docker Desktop, where the watcher sees nothing. |
| `drive_background_units_per_min` | integer | `20000` | Google Drive API quota units per minute for background work. |
| `status_files` | boolean | `true` | Write the OCR queue and server status files into the cloud folder for readers. |
| `dedup` | boolean | `true` | Google Drive and MEGA: merge folders that have the same name in the same place (two `mokuro-reader` folders, two copies of a series folder) the way Mokuro Reader does, and tell readers so they leave it to Bunko. On Google Drive a copy that looks empty to Bunko can still hold files added through the Drive website (Bunko, like Mokuro Reader, sees only the files made through Mokuro Reader's Google project); the merge moves that copy to Drive's trash with them, for 30 days, as Mokuro Reader's merge does. After a merge `volume-data.json`, `goals.json` and `positions.json` can stay as two copies until a reader reading the cloud folder directly writes them again; of two copies of any other file (folders and Bunko's own status files excepted, which are kept as both) the strictly newer stays and the other goes to the trash (on a tie, or when a time is unknown, the copy in the folder that is kept stays). |
| `progress_user` | string | — | When readers are served too: the Bunko user who owns the cloud folder's per-account files: the progress files, the reading history and the OCR corrections (`<Vol>.edit.mokuro`) Mokuro Reader keeps there. A reader that reads the cloud folder directly shares them with that account. With nobody chosen they belong to no one and nobody sees the corrections (the Storage tab counts them). Choosing another account moves no file: it gets the cloud folder's corrections as they are, the previous account's corrections kept on this server (`users/<u>/edits`) are not brought into the cloud folder, and correction files without an owner are looked for only when Bunko starts. |
| `google_project` | string | `release` | Which Mokuro Reader's Google project Bunko signs in with, so it sees that reader's Drive files: `release` (reader.mokuro.app) or `beta`. `MOKURO_GOOGLE_PROJECT`. Becomes `custom` by itself when `oauth.google` is set; `custom` without it is refused. |
| `oauth.google.*` | string | — | Your own Google OAuth clients, replacing both built-in ones: `client_id` + `client_secret` (Desktop client, browser sign-in on this machine) and `device_client_id` + `device_client_secret` ("TVs and Limited Input" client, sign-in with a code). Either pair may be left out; that sign-in flow is then unavailable. An id without its secret (or the reverse) is refused. |
| `oauth.microsoft.client_id` | string | — | Your own Microsoft app registration for OneDrive: a public client with the "Mobile and desktop" redirect `http://localhost` (any port is used at sign-in). |

All the numbers must be above 0. Passwords and tokens are never kept in
`config.yaml`: a WebDAV password comes from the file named by
`MOKURO_WEBDAV_PASSWORD_FILE` or from the sign-in, which is kept in
`cloud/credentials.json`; a `webdav.password` key is refused. The other
`storage.library` keys are set in the file or with
`mokuro-bunko config set storage.library.<key> <value>`.

#### Signing in to Google Drive or OneDrive

The setup wizard, the admin panel's Storage tab and
`mokuro-bunko library connect gdrive|onedrive` pick the sign-in by where you are:

- **A browser on the server's computer** (the wizard opened on `localhost`, or
  `library connect` in a desktop session): the provider's page opens in the
  browser and comes back to Bunko by itself.
- **Anywhere else** (Docker, a NAS, SSH, the wizard opened from another device):
  - Google Drive shows a code to enter at google.com/device on any device.
  - OneDrive shows the Microsoft sign-in address to open on any device. After
    you sign in, that browser ends on a page that does not load, at
    `http://localhost:<port>/bunko?code=…`. Copy that page's whole address and paste
    it back into the wizard, the Storage tab or the terminal (within 10 minutes).
    Bunko checks that the address belongs to this sign-in and redeems its code;
    the address is not logged or kept. A personal Microsoft account cannot sign in
    with a code, so the code is offered for work or school accounts only ("Use a
    sign-in code instead", or `--device`).

On the command line, `--browser`, `--device` or `--paste` choose the sign-in
instead. `--browser` without a desktop prints the address to open on this
computer; `$BROWSER` is used to open it when set. Pasting needs a terminal: run
`docker exec -it <container> mokuro-bunko library connect onedrive`.

#### A library folder you choose

`backend: folder` keeps the library in `storage.library.path`, read and written in
place: on this computer, a second disk or a mounted network share. Choose the same
folder as Mokuro Reader's **Local Folder** (Chrome or Edge on a desktop); the folder
itself is the library's top folder, as the `mokuro-reader` folder is in a cloud
account. Bunko treats it exactly like a cloud folder: it writes the reader's queue
file (`.mokuro-queue.json`) and its marker (`.mokuro-bunko.json`) at the top, and the
OCR files, covers, `series.json` and `catalog.json` next to the volumes. It is
distinct from `local`, whose library is Bunko's own `base_path/library/`.

```yaml
storage:
  library:
    backend: folder
    path: /srv/manga
```

`mokuro-bunko library connect folder --path /srv/manga` does the same and checks the
folder first (`--create` makes it when it is missing; only the last folder of the
path is made). In the setup wizard and the Storage tab, **Browse…** lists the
server's folders only in a browser on the server's own machine; from anywhere else,
type the path.

- **Switching** to another folder, to a cloud account or to This computer is a
  different library: Disconnect first. The old folder is left as it is.
- **Disconnect** leaves every file in the folder where it is; only the marker says
  that this Bunko let it go.
- **Removed files** go to the hidden `.mokuro-bunko-trash/` folder inside the
  library, renamed to `*.bunko-trashed`, so no reader lists them. They are kept 30
  days; until then they can be moved back by hand.
- **A missing folder** (an unplugged disk, a share that is down) makes the library
  offline: Bunko retries and removes nothing. A library folder that is gone while
  its parent is still there (deleted or renamed) is held as a changed library, as a
  cloud folder that disappears is.
- **Names**: the library's paths are case-insensitive and case-preserving on every
  host, as on Windows. On Windows, Bunko cannot write a name Windows refuses
  (`<>:"|?*`, a trailing dot or space, `CON`, `NUL`…): that write shows as failed in
  the Storage tab, and is not retried. A folder made case-insensitive on its own (`chattr +F`) inside a
  case-sensitive disk is not noticed.
- **Network shares** have no change notifications: changes show at
  `poll.idle_s`. Disks whose timestamps are coarse (FAT: 2 s) can miss two
  same-size changes by another program within one tick.
- **Docker**: mount the folder and point `MOKURO_LIBRARY_PATH` at the mount (see
  [deployment](deployment.md)). Bunko never changes the folder's owner: run it as
  that owner (`PUID`/`PGID`).
- **Windows service**: a service does not see mapped drive letters; use the share's
  UNC path (`\\files.example.com\manga`).

#### Upload verdicts

Every WebDAV `PUT` under `/mokuro-reader/` is written to a
hidden temporary file beside its destination (`.<name>.upload-*.tmp`) and
moved into place only once it checks out, so a failed upload never replaces
or deletes the file that was there, and a read during the upload sees the
old file. The body must be as long as its `Content-Length`; a `.cbz` must
also open as a zip with every member matching its CRC-32 (the check the OCR
processors run on their downloads — about 80–100 ms per 200 MB; encrypted
members are skipped). Its members must be stored or deflated, and it may not
inflate to more than 20 times its size (at least 256 MiB, at most 16 GiB).

- Success keeps the usual 201/204 and adds `X-Mokuro-Upload: verified` (an
  archive) or `stored` (any other file) and `X-Mokuro-Size: <bytes stored>`.
- A PUT may carry `Content-Digest` (RFC 9530; `sha-256` or `sha-512`,
  hashed as the body streams in). A malformed header is ignored as absent;
  unknown algorithms are skipped. A match adds
  `X-Mokuro-Digest-Verified: <algorithm>`.
- Failure answers JSON `{"ok": false, "reason", "detail", "retry"}`:
  `truncated` (422, retry), `corrupted-in-transit` (422, retry: the body
  does not match its digest; the zip is not checked), `not-an-archive`
  (422), `archive-refused` (422: another compression method, or inflating too
  far; re-pack it as an ordinary zip), `disk-full` (507), `forbidden`
  (401/403/429), `server-error` (500, retry; any other refused PUT, such as
  409 or 423, also answers `server-error` with its own status, with `retry`
  only for 5xx, 408 and 423; a cloud library that cannot take the upload right
  now (an outage, a provider pause, Bunko still claiming the library or another
  Bunko managing it) answers 503 with `Retry-After`, a file changed in the cloud
  since the client's `If-Match` 412, and a full cloud account `disk-full` 507), and
  `archive-damaged` (422). An archive whose digest matched but
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
| `default_role` | string | `"registered"` | Role given to self-registered accounts (`self` and `approval` modes): `registered`, `uploader`, `inviter` or `editor` (`writer` reads as `uploader`). An invited account gets its invite's role. |
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
| `uploader` | Yes | Own only | Yes (not into another account's series) | Own uploads and series | No | No |
| `editor` | Yes | Own only | Yes | Yes | No | No |
| `inviter` | Yes | Own only | Yes | Yes | Yes | No |
| `admin` | Yes | Own only | Yes | Yes | Yes | Yes |
| `processor` | Yes | No | No | No | No | No |

Series have owners. The account that creates a series folder owns it for good
(folders that existed before 0.8 belong to their only uploader, when every
volume in them has a recorded uploader; any other folder has no owner and stays
shared). For `uploader`:

- It adds nothing to a series another account owns. It adds new volumes to a
  series nobody owns, but no account becomes the owner that way.
- A file named after a volume (the volume's archive, its `.mokuro`, its cover,
  its layer files, anything else under its name) is written, replaced or
  deleted only by the account that uploaded that volume or by the series owner.
  A volume listed under a longer name (`Title.v01.cbz` beside `Title.cbz`) is
  its own uploader's, not the shorter one's.
- No uploader adds an archive named like a volume's correction or curated
  layer (`V.edit.cbz`, `V.curated.cbz` beside `V.cbz`), not even
  that volume's own uploader: it would hide those layers.
- The series owner replaces and deletes anything in its series, whoever added
  it, and the series folder itself; it is also the one that may update the
  series' `series.json`. A volume's uploader, or the series owner, may publish
  that volume's `curated` layer.
- Its own correction layers (`<Volume>.edit.mokuro`) are its own, in any series.

`/login/api/me` lists the series the account owns (`ownedSeries`) and those it
cannot add to (`addFiles.closedSeries`), without naming their owners, and the
volumes outside its own series whose `curated` layer it may publish (its uploads,
`curated.volumes`). Moving,
copying, locking or PROPPATCH of library files, its own included, needs `editor`
or higher. Editors and admins are not limited by series owners.

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
   Certificates are stored in `$XDG_DATA_HOME/mokuro-bunko/certs/` (default
   `~/.local/share/mokuro-bunko/certs/`); on Windows in
   `%LOCALAPPDATA%\mokuro-bunko\certs\` (the portable zip: `data\certs\`).

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
| `path` | string | `"/_admin"` | Kept for older files; leave it as is. The panel is always served at `/_admin` (only the desktop app's admin link reads this value). |

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
pages and runner build in its details. A result the library refused when
installing it is logged as `ocr_sidecar_rejected` with the reason: a file
that failed validation, a volume deleted or replaced while it was read, a
sidecar a processor sent under the wrong name, a destination locked by a
WebDAV write, or a failed move into place. A replaced volume or a locked
destination is OCRed again. Results refused while they are uploaded (a bad
header, too large, a sha256 mismatch, a claim no longer outstanding) appear
only in the server log. The actor is the processor's account, or
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
| `enrich_community` | boolean | `true` | Fetch ratings, tags and genres for series linked to an AniList or MAL id (no API key needed; MAL, through Jikan, gives no tags). Only while `enabled` is on. |

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

The manifest is read with exactly the rules of the volume's `.cbz` (anonymous
download setting, credentials, 401 challenge) and gets the same CORS
headers, whether or not `catalog.enabled` is on. A missing `series` or
`volume` parameter is 400, a series or volume that resolves outside the
library is 403, and a missing volume is 404.

#### OCR queue file

`GET /mokuro-reader/.mokuro-queue.json` (also `HEAD`) is the head of the
pending OCR queue, one entry per volume, for a reader to poll instead of a
timer per volume. It lists every volume with a job running, then at most the
next 100 waiting volumes in queue order. `pending_volumes` counts all waiting
volumes, so a reader can tell when the list is cut short (an unlisted
volume's timing is in its manifest):

```json
{"version": 1, "generated_at": "2026-09-28T16:00:00Z", "held": null, "next_check_after": 95,
 "pending_volumes": 0,
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
more, so ETA jitter alone does not change the ETag; a running job's
`progress` does, as pages complete. With the 100-volume cap, the file does
not grow with the queue.

A `.cbz` written over WebDAV (PUT, or MOVE/COPY into place) joins the OCR
queue as the request completes, instead of at the next poll. A PUT that
queued OCR answers with `X-Mokuro-Manifest` (that volume's manifest URL) and
`X-Mokuro-Recheck-After` (seconds, same rule), both exposed through CORS.
Pricing on the PUT uses the queue's cached pending list and gives up (300)
rather than walk the library or more than 300 queued items.

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
| `normal` | One card per machine: the volume it is reading with a progress bar, pages and finishing time, and the volume on deck as a single "On deck" line. The pending list with a finishing time per volume, the run order, pending thumbnails, volumes skipped for missing pages, and failures with a generic reason ("engine error", "archive incomplete", "interrupted — will retry"…). |
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
viewer is identified by the session token the page sends as
`Authorization: Bearer`. An unknown or expired token is answered exactly as
a visitor is, with an `X-Queue-Auth: failed` header on which the page drops
its stored login. Other clients can send `Authorization: Basic` instead; a
wrong password also gets `X-Queue-Auth: failed`. Basic credentials count
against the WebDAV login rate limit, and a client over it is served as a
visitor with `X-Queue-Auth: limited`.

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
| `lock_retries` | integer | `5` | How many times a step (BEGIN, COMMIT, a read) is tried while SQLite reports the database locked, the first try included (at least 1: no retries). |
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

### Analytics

New in 0.8. The admin panel's **Analytics** tab: what is downloaded and read and by
whom, what each kind of traffic costs, how healthy the server is, and which IPs crawl
the catalog. WebDAV counts like the web pages and JSON APIs: every request, whatever
its method (PROPFIND, GET, PUT, MOVE, LOCK, ...), is counted by protocol, method and
category.

```yaml
analytics:
  enabled: true
  flush_seconds: 60
  retention:
    ip_days: 30
    hourly_days: 90
    host_minute_days: 7
    crawl_days: 90
  crawl:
    window_hours: 6
    min_series: 20
    min_coverage: 0.25
  library_scan:
    interval_hours: 6
    page_formats: true
  geo:
    enabled: false
    url: "https://download.db-ip.com/free/dbip-country-lite-{year}-{month}.mmdb.gz"
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enabled` | boolean | `true` | Count requests. Off, the counting layer passes every request through untouched and nothing is written; what is stored stays viewable in the admin panel. |
| `flush_seconds` | integer | `60` | How often the in-memory counters are written to the database (5 to 3600). A crash loses at most this much. |
| `retention.ip_days` | integer | `30` | Days rows with a raw client IP are kept: the per-IP totals, the IPs an anonymous visitor came from, failed sign-ins per IP and username (their daily count is kept forever), the slowest requests (those at most 7 days) and a crawl episode's address. Afterwards only pseudonymous ids remain. Also how often the anonymous ids' salt is replaced. |
| `retention.hourly_days` | integer | `90` | Days the hourly traffic, latency and error rows are kept. Daily totals (errors included) are kept forever. The last two complete days keep their hourly rows whatever this says, because their daily totals are still being rebuilt from them. |
| `retention.host_minute_days` | integer | `7` | Days the per-minute memory and CPU samples are kept. |
| `retention.crawl_days` | integer | `90` | Days a crawl episode is kept after its last request. Its client IP is kept only `retention.ip_days`; afterwards the episode shows a salted id of the address instead. |
| `crawl.window_hours` | integer | `6` | The window the crawl detector looks at, per client IP (1 to 168). |
| `crawl.min_series` | integer | `20` | Distinct series an IP must touch in the window (1 to 1,000,000), by downloading a volume or by asking about one series (a `Depth: 1` or `infinity` listing of its folder, its catalog page or manifest; `series.json` and covers are not counted)... |
| `crawl.min_coverage` | number | `0.25` | ...that are at least this share of the catalog's series (above 0, at most 1), for an episode to open: an IP needs the larger of the two counts (with the defaults and a 30-series library, 20). Folder spellings of one title count as one series. Crawlers are reported only: nothing is blocked or slowed. |
| `library_scan.interval_hours` | integer | `6` | How often the library's file types are counted (1 to 720); the first scan runs 2 minutes after start, and **Rescan now** (`POST /_admin/api/analytics/library/rescan`) runs one at once. The scan only lists folders and reads file sizes, so on a cloud library it is answered from the store's index and downloads nothing. |
| `library_scan.page_formats` | boolean | `true` | Also count the page image formats inside archives (a library on this disk only; reads each archive's zip directory, never the pages, at most 20 archives a second). Each archive is read once and again only when its size or modification time changes, so a restart does not read the library again. |
| `geo.enabled` | boolean | `false` | Opt-in: download DB-IP's free *IP to Country Lite* database (CC BY 4.0, ≤ 64 MiB compressed and uncompressed) to `<storage>/analytics/country.mmdb`, refresh it monthly, and show a rough country per IP, for WebDAV and web requests alike. Loopback, private, link-local, CGNAT (`100.64.0.0/10`) and unique-local IPv6 addresses are `LAN`; an address the database does not place is `ZZ` (unknown). Lookups stay on this server. Off (or with `enabled: false`), nothing is downloaded and no country is recorded; none is either until a database has loaded, local addresses included. |
| `geo.url` | string | the DB-IP download | Where the database comes from, an `https://` URL (a redirect away from HTTPS is refused); `{year}` and `{month}` are filled in (the current month, else the previous one when this month's answers 404). A download is checked (it must open as a MaxMind DB and pass a full verification) before it replaces the stored file, so a failed or damaged one keeps the database there was; a failure is logged, shown in Analytics → Clients & geography, and tried again after 6 hours or on **Refresh database** (`POST /_admin/api/analytics/geo/refresh`). |

The data lives in `analytics.sqlite` beside `mokuro.db` (its own file: it never
slows the main database, and deleting it resets the analytics). Requests are counted
in memory and written in one transaction per `flush_seconds`; complete days are
rolled into daily totals once an hour. An account's traffic is attributed to the
account (`u:<name>`); an OCR processor's (`p:<name>`) counts as bandwidth but not as
downloads, reading, active users or crawling; an anonymous request is attributed to a
pseudonymous id, the start of a salted SHA-256 of its client IPv4 address or IPv6
`/64` (`a:<12 hex>`), so a visitor can be counted once without long-lived rows
keeping the address. The salt is replaced every `retention.ip_days` and the old one
deleted, so once the raw IPs are gone an id can no longer be traced back to its
address (a visitor is one id within such a period only); a day keeps at most 10,000
anonymous visitors apart, the rest count together as `a:other`. A download counts
toward popularity once most of the file (95 %) was sent that day, so a one-byte range
probe is no download. If `analytics.sqlite` is damaged (SQLite or its `quick_check`
says so) it is moved aside as `analytics.sqlite.broken-<time>` and a new one is made
(the moved file holds raw IPs too, so the hourly upkeep deletes it once it is
`retention.ip_days` old);
if it is healthy but cannot be opened (locked by another process, read-only) it is
left as it is and the analytics are kept in memory for that run. The client
IP is the one the server resolves, so `server.trusted_proxies` applies. Byte counts
are response and request **body** bytes (headers and TLS are not counted); behind the
image's nginx (`MOKURO_NGINX_ACCEL=1`) the archives nginx sends are counted from the
file's size as estimates. Every key is also editable in the admin panel (Analytics →
Settings, `PUT /_admin/api/settings/analytics`) and through `MOKURO_ANALYTICS_*`
variables, dots as underscores (`MOKURO_ANALYTICS_RETENTION_IP_DAYS=14`).

**Reading** is counted from the files Mokuro Reader syncs, whichever WebDAV client
writes them: about ten seconds after an account's `positions.json`,
`history/<device>/*.events` or `volume-data.json` is written (a PUT, or a MOVE into
place), the server reads the file back from that account's own tree and compares it
with what it saw before, so a file uploaded twice, another device's merged copy, a
volume listed twice in one file (its last entry counts, as in the reader), or the
reader's DELETE-then-PUT never counts twice. A new reading position is a *read*
of that volume on that day (a position first seen more than 48 hours old is not); the
reading history adds the time spent (one page view counts for 15 minutes at most) and
the pages read as Mokuro Reader's own statistics count them: each page once per read
of a volume (a restart reads it again), whichever device or upload brings it, so a
two-page spread or turning back never counts a page twice, and a page flicked past
faster than 1,500 characters a minute adds its time but not the page.
`volume-data.json` gives each account's progress and finished volumes for the
completion funnel, and the device names the reader shows. Volumes
are matched to the library's series by their volume id, else by the series the
reader names; a volume of no known series is shown as `(unknown)`. Files larger than
the reader writes (4 MiB for `positions.json`, 16 MiB for a history file, 32 MiB for
`volume-data.json`) and history files of a newer format are skipped.

**Crawl detection** counts, per client IP, the distinct series it touched in the
window: by downloading a volume (a `GET` answered 200 or 206), or by asking about one
series (a WebDAV `PROPFIND` of a series folder with `Depth: 1` or `infinity`,
`/catalog/api/series?name=`, a manifest). Mokuro Reader's normal sync does not count:
its one `Depth: infinity` listing of the whole library, the `series.json` files it
refreshes (every one on a first sync), the covers it shows, the `HEAD` before each
download and its own progress files are not signals, so reading a few series never
opens an episode, while an rclone or davfs2 mirror, a metadata walk over WebDAV or a
script walking the catalog does. One known false positive: when the reader's
`Depth: infinity` listing fails with an unexpected error (a timeout or a server error
on a large library), it lists that sync folder by folder instead, and that walk is
reported as a `metadata` episode. An episode is
`download` when the downloads alone cross the thresholds, `metadata` when nothing was
downloaded, `mixed` otherwise; it ends after a whole window without a request from
that IP. Detection is per IP (a crawl split over many addresses, each under
`min_series`, is not seen). At most 4,096 IPs are tracked (least recently used go
first, an open episode last) and their touches are capped at 300,000 (about 6 MiB).

**Server readings** are sampled every 15 s: the process's memory (RSS and peak), its
CPU time as a percent of one core (two busy cores read 200 %), open connections and
requests in flight (their highest per interval), and the machine's memory, or the
container's cgroup memory limit when that is lower. Linux and Android read `/proc`
(Docker and the musl build included), Windows and macOS their system calls; anywhere
else the panel says the platform does not report them. Four samples make a minute
(kept `retention.host_minute_days`), minutes make hours (kept `retention.hourly_days`).

The client mix (Analytics → Clients & geography) is read from each request's
`Origin` and `User-Agent`. A request whose `Origin` is the configured reader
(`catalog.reader_url`, or the hosted reader of the default `cors.allowed_origins`)
counts as the hosted Mokuro Reader; another cross-origin browser on WebDAV or
`/login/api/*` as a reader on another origin; Bunko's own pages as the web UI (its own
origin is decided as the CSRF check decides it: `Host` or `X-Forwarded-Host`, and a
`Host` without a port, as `proxy_set_header Host $host` sends it, matches any port;
a browser on an origin `cors.allowed_origins` refuses is Bunko's page too); WebDAV
requests from anything that is not a browser as WebDAV clients, named when the user
agent says which (rclone, davfs2, the Windows mini-redirector, Finder, Cyberduck, GNOME
Files, Kodi, Nextcloud, WinSCP), with curl, wget, Python, Go and Node.js tools and
crawler bots told apart too. Distinct visitors per app, client, browser or OS are
counted only within `retention.ip_days` (an anonymous visitor stands for an IP).

### Updates

```yaml
update:
  check: true
  auto: false
  channel: auto
  manifest_url: "https://github.com/Gnathonic/mokuro-bunko/releases/latest/download/release.json"
  # public_key: ""   # forks and test releases only; config file only
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `check` | boolean | `true` | Look for new releases in the background (first check a minute after start, then every 12 hours) and show the result in the admin panel's **Updates** card (Status tab). With `false` and `auto` off, nothing is fetched in the background; **Check now**, the desktop app's Settings page and `mokuro-bunko update check` still fetch. |
| `channel` | string | `auto` | Which releases count as updates. `auto` follows the installed build: a pre-release (such as `0.7.0-beta.1`) is on `prerelease`, so it sees the next beta or release candidate as well as the next stable release, and a stable release is on `stable`, so it only sees stable releases. `stable` or `prerelease` pins the channel whatever is installed. On `prerelease` with the GitHub URL, the newest published release that carries a `release.json` is looked up through GitHub's releases API (unauthenticated: 60 requests an hour per address), since `releases/latest` never points at a pre-release. A processor following its library (`processor.auto_update`) installs exactly the library's version and ignores the channel. |
| `auto` | boolean | `false` | Install a newer release on the channel by itself, at a quiet moment, then restart. See [Automatic updates](#automatic-updates). Also settable in the admin panel's **Updates** card. An install that cannot update itself (Docker, a package manager) ignores it, and the card says how to update it instead. |
| `manifest_url` | string | the GitHub `releases/latest` `release.json` | Where the release manifest comes from. Change it for a mirror or a fork (a fork also needs its own signing key: compiled in, or `public_key`). |
| `public_key` | string | empty (the compiled-in key) | The ed25519 public key (base64) release manifests are checked against, for a fork or a test release. Read **only from the config file**: no `MOKURO_UPDATE_*` variable, `config set`, admin API or app page can set it, and every start (server, CLI, processor) logs `UPDATES ARE TRUSTED FROM A NON-DEFAULT SIGNING KEY …`. Anyone who can write the config file can already replace the program, so this adds no new way in; an attacker who only controls the environment of a release build cannot use it. |

The manifest is signed (ed25519, with a public key compiled into the
binary) and every archive's sha256 comes from it, so a compromised download
host cannot push a binary. What the **Update and restart** button does
depends on how mokuro-bunko was installed:

| Install | What happens |
|---|---|
| A tarball, `install.sh` as a user (`~/.local/lib/mokuro-bunko`), the Windows zip, portable folder or `install.ps1` | One click: download, verify, swap the executable, restart (on Windows `run.bat` restarts it). |
| `install.sh` as root with the system unit, a distro package, Homebrew, Nix | Notice only; update through the package manager or re-run `install.sh`. |
| Docker (`/.dockerenv`, or `MOKURO_INSTALL_KIND=docker`) | Notice with the image to pull. |

`MOKURO_INSTALL_KIND=docker` or a manager's name (for example `apt`)
overrides the detection; `self` only skips the Docker check (a binary under
`/usr/bin`, `/usr/sbin`, a Homebrew `Cellar` or `/nix/store` still gets the
notice only). `mokuro-bunko update check` and `update apply [--yes] [--restart]`
do the same from a terminal. The button and `update apply` install a release the
same way the automatic update does (below): the program, its OCR backend pack and
its models together.

#### Automatic updates

Opt-in: `update.auto: true` on the library server (also the admin panel's
Updates card), `processor.auto_update: true` in a processor's `processor.yaml`
(`processor setup --auto-update`, the processor's pairing page, or its Settings
→ Updates page).

**A release is one unit.** An OCR backend pack is built by one mokuro-bunko
release (its `pack.json` says `bunko_version`; a pack built outside a release,
without one, is accepted as a development pack). Release X's program opens the
packs of the releases that build the same pack as X, from the oldest such
release up to X itself (0.7.4 and later, for 0.8); any other is refused with
`the backend pack is from mokuro-bunko A, this is B, which opens the packs of O
to B; run 'mokuro-bunko install-ocr' (with automatic updates on, an update
installs it)`, and `doctor`, `/control/status` and
the tray name the pack in use as `<variant> for <release>`. The models a release
uses are named by manifests compiled into its program, so new model files only
ever arrive with a new release. An update therefore installs, together: the
program (the tray is part of it), the release's pack for the variant installed
here unless the installed pack is one the release opens, and the release's
model files.

**Server.** With `auto` on, every check (12 h by default;
`MOKURO_UPDATE_CHECK_SECONDS` sets the period) that finds a newer release on
the channel starts an update:

1. New OCR work stops on every machine (the scheduler hands out no new claims);
   the volumes in flight (on this server and on remote processors) and the
   uploads finish. `/control/status` reports `update.state: "waiting"`, and
   the Updates card, the tray and the app dashboard show the update waiting.
2. The release's `release.json` (signature checked) and archive (sha256 checked)
   are downloaded; the new program is unpacked next to the running one but not
   installed. It then runs its own `update prefetch` (internal): it downloads and
   checks its pack for the installed variant into `backends/.staging-<variant>`
   (signature, sha256, every file; the host requirements such as the NVIDIA
   driver version; the free disk space), loads it (a GPU pack must find its GPU),
   and fetches its model files with it. When the installed pack is one it opens,
   it loads that one instead and downloads no pack. Nothing installed has changed
   yet.
3. The switch: the running program is copied aside (`.mokuro-bunko-previous`;
   on Windows the other program of the pair beside its own), the installed pack
   moves to `backends/.prev-<name>`, the staged pack takes its place, the
   program is replaced, and `<storage>/.update.json` records it. The server
   restarts. (A kept pack stays where it is and only the program switches; step 4
   then has no new pack to check, since step 2 loaded the kept one in the new
   release.)
4. The new release checks its pack in a child process (`install-ocr --probe`,
   the pack it will really open, with its real environment). If it loads, the
   kept copies and the model files the release no longer names are deleted, and
   the status says "Updated to X". If it does not, the program **and** the pack
   are rolled back, the previous release starts again and raises a `fail`
   problem ("The update to X was rolled back: its OCR backend failed to load on
   this machine: …"); X is marked in `.update-blocked.json` and not tried again
   automatically (install it by hand once the cause is fixed).

A failed attempt keeps the running release and is tried again after 10 min,
doubling to 12 h (`MOKURO_UPDATE_RETRY_SECONDS` sets the first wait); never twice
in a row without that wait. When only the owner can fix it, the status carries
a `fail` problem with the exact fix (`kind: "update"`), which the tray flags and
announces once with a desktop notification:

| Case | What the problem says to do |
|---|---|
| The pack needs a newer NVIDIA driver (`requires.nvidia_driver`) | Update the driver to the version named |
| Not enough disk space for the program or the pack | Free the space named |
| The release does not verify (bad signature) | Nothing is installed; check `manifest_url`/`public_key` |
| The new pack does not load here (pre-switch check) | Stay on this release; check `doctor` |
| Rolled back after the switch | Fix the cause, then install by hand |
| Three failed attempts in a row (downloads) | See the log |
| Docker or a package-managed install | Pull the image / update through the package manager |
| The GPU changed so another variant fits (recorded at `install-ocr` time in `backends/.hardware.json`) | `mokuro-bunko install-ocr --variant <variant>` |

How the restart happens, per launcher: under the tray (`MOKURO_LAUNCHER=tray`)
and under a systemd/launchd service or a terminal on Linux and macOS the process
`exec`s the new program (same pid: the tray and the service manager keep
supervising it; the tray shows "Updating to X…" and does not count it as a
crash); on Windows the process exits with code 75 under the tray or `run.bat`
(which start it again at once) and otherwise starts the new program and exits.

**Processor.** The library compares the version a processor registers with
(`host.version`) to its own and says so in the registration reply
(`version_mismatch`, [PROTOCOL.md](rust-port/PROTOCOL.md)). With `auto_update`
on and the library newer, the processor drains (it pauses itself after the
running volumes, with reason `update`: they finish and upload, unstarted claims
go back at once), installs exactly the library's release the same way (from
`<manifest_url base>/v<library version>/release.json`), restarts and reconnects.
A library that is older is only reported (a `fail` problem: a processor never
downgrades); with `auto_update` off a newer library is reported as a warning.
Its progress reaches the library (`update_status`) and shows in the admin
panel's processor list.

**Mirrors and test releases.** A version's manifest is derived from
`manifest_url` (which names the latest one): GitHub
`…/releases/latest/download/release.json` → `…/releases/download/v<version>/release.json`;
a URL with a `latest` path segment gets `v<version>` there; a URL containing
`{version}` gets the version substituted; otherwise `<dir>/v<version>/<file>`.
A test or fork release signed with another key needs `update.public_key` (see
above) in the config file of every machine that should trust it.

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
| `backend` | string | `auto` | Which devices OCR on this machine uses, and so which backend pack `install-ocr` and the Docker image install: `auto`, `cuda`, `rocm`, `cpu`, or `skip` (see [Backends](#ocr-backends)). | To keep OCR off a GPU (`cpu`), or `skip` for a server that should run no OCR itself. |
| `poll_interval` | integer | `30` | Seconds between library scans for volumes missing a sidecar or a thumbnail. Also the base of the retry backoff for failed volumes. | Raise it on a very large library where a scan is expensive. |
| `concurrency` | integer | `1` | How many OCR jobs this machine runs at once (1–8). Not a pool size. At startup. | See [Concurrency](#concurrency). |
| `sessions` | boolean | `true` | Accepted so old files load, and ignored: models always stay loaded between volumes. | Never. |
| `local_processing` | boolean | `true` | Whether the machine serving the library runs OCR at all. A file edit applies at the next start; the admin panel's Machine card applies it at once (refused when `MOKURO_OCR_LOCAL_PROCESSING` sets it). | `false` on a small always-on server that leaves OCR to [remote processors](#remote-ocr-processors). |
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
0.5.2. `serve` and `processor serve` install the pack themselves when local OCR needs
it and it is missing, **in the background**: they serve at once, show the progress (log,
admin panel, dashboard, tray, `/control/status`) and start local OCR when it is done
(see [Deployment](deployment.md#ocr-backend-install-ocr)); the full Docker image, which
carries no pack, relies on this.

`ocr.backend` (and `serve --ocr`) picks the devices, and with them the pack
`install-ocr` (without `--variant`) and the Docker image install on this machine:

| Backend | Description |
|---------|-------------|
| `auto` | The installed pack's GPUs, else the CPU. `install-ocr` and the background install pick the pack for the GPU found (`cu130` for NVIDIA with driver 580 or newer, `rocm7.1` for AMD, else `cpu`), but keep a variant picked by hand while the hardware stays the same (see [`install-ocr`](#install-ocr)). |
| `cuda` | NVIDIA GPUs (the `cu130` pack): Turing (GTX 16xx / RTX 20xx) or newer, driver 580 or newer. Nothing else to install: the pack brings the CUDA libraries. |
| `rocm` | AMD GPUs on Linux (the `rocm7.1` pack): Radeon RX 6000 (gfx1030; RX 6600/6700 too: `HSA_OVERRIDE_GFX_VERSION=10.3.0` is set automatically), RX 7000, RX 9000. Needs `libnuma` from the system (Debian/Ubuntu: `libnuma-dev`). |
| `cpu` | CPU only (slower). `install-ocr` installs the `cpu` pack even with a GPU present (an installed GPU pack is kept and runs on the CPU). |
| `skip` | This server runs no OCR of its own, the same as `local_processing: false`. The Docker image then installs nothing. |

With `cuda` or `rocm` and no such GPU usable here (none, an old driver, a GPU the
container was not given), `install-ocr` installs `cpu` and says why; run it again once
the GPU is there (the Docker image does so on its next start).

`webgpu`, `directml` and `coreml` are still accepted: they name ONNX Runtime GPU
providers, which 0.7 releases do not include, so on a released build they leave only
the CPU (use `auto`; `install-ocr` treats them as `auto`). `rocm` is the AMD GPU again,
as in 0.5.2 (early 0.7 builds read it as `webgpu`, which left an AMD GPU unused).
`mokuro-bunko install-ocr --list` and `doctor` show which pack this machine wants and
which is installed. Without a pack, hayai-nova and paddle-manga are not offered on the
machine (ppocr-manga still is, and remote processors still work).

`skip` is not "no OCR at all": it means none on this machine. The server
still owns the queue and still serves cover thumbnails; a
[processor](#remote-ocr-processors) that logs in runs the OCR. Until one
does, the queue holds and the queue page says "No processor connected
since …". A server that must never have OCR done simply has no `processor`
account: that role is only ever given by an admin.

A GPU that cannot be used (only the CPU pack installed, an old driver, a hidden
device) leaves the CPU. `mokuro-bunko doctor` shows which pack is in use and
which this machine would want, and whether the models and compiled packages
are on disk; the server log lists the devices the pack found when it loaded.

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
| `id` | minted (`g-1`, `g-2`, …) | Internal and never shown. It keys the row's history and benchmarks, so reordering or renaming keeps them. Leave it out for a new row: the next free `g-N` is minted when the config loads and written to `config.yaml` at the next save (admin panel, setup wizard or `config set`). Until then it follows the row's position, so do not reorder id-less rows. |
| `name` | `<engine>-<detector>` (e.g. `paddle-manga-ppocr-manga`), or the engine for `ppocr-manga` | The row's label **and its file-name postfix**: `<Volume>.<name>.mokuro`. A derived name gets `-2`, `-3`… on a clash. Rules below. |
| `primary` | `false` | Exactly one enabled row must have it. That row writes the bare `<Volume>.mokuro`, which readers take character counts from. Every OCR file of a volume carries the same volume id: the `.mokuro`'s when one exists, else the id readers already give a volume with no `.mokuro`. Any engine may be the primary one. |
| `enabled` | `true` | A disabled row queues nothing and writes nothing. Its name stays reserved. |
| `engine` | — (required) | One of the [engines](#ocr-engines). |
| `detector` | `ppocr-manga` | The [text detector](#ocr-detectors) of every engine but `ppocr-manga`: `ppocr-manga`, `ctd-lines`, or (for the engines that read blocks) `animetext-blocks`. Ignored (and stored as `null`) for the `ppocr-manga` engine, which brings its own. |
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

**Renaming a non-primary row** means its files are now looked for as
`<Volume>.<new-name>.mokuro`: volumes are OCRed again under it, and the old
files stay where they are. A job already running finishes under the name it
started with. Renaming the primary row only changes its label: its files are
always the bare `<Volume>.mokuro`, so nothing is OCRed again. **Deleting or
disabling a row** leaves its files alone too. Deleting an archive over WebDAV
also removes its layer files; deleting it directly on disk does not. To take
over files that are already on disk (for example
`Volume 01.paddle-manga.mokuro`), name the row after them.

**Editing applies live.** Generations saved in the admin panel reach the
running worker at once. A running job is stopped (and not counted as a
failure) only when its row is removed, disabled, or changes engine, detector
or patch budget; renames, reorders and pool changes never stop a job.

**Volumes missing pages** get no other layers: when a volume arrives with a
`.mokuro` that names pages its archive does not contain, every layer would
have the same holes. This is checked when the archive arrives and on each
periodic library walk, against the `.mokuro` beside it at that moment, so
upload the `.mokuro` first. Replace the archive with a complete one and the other rows pick
it up. A volume without a `.mokuro` is never held back: its primary sidecar
is read from the archive itself.

Set the list in the admin panel (Settings → OCR → Generations), in the config file, or
as JSON:

```bash
mokuro-bunko config set ocr.generations '[{"name":"hayai-nova","engine":"hayai-nova","primary":true}]'
MOKURO_OCR_GENERATIONS='[{"name":"hayai-nova","engine":"hayai-nova","primary":true}]'
mokuro-bunko serve --generations '[{"name":"hayai-nova","engine":"hayai-nova","primary":true}]'
```

**Old configs.** A 0.5.x list that names the removed `mokuro` engine or the
removed `ctd`, `animetext` or `rtdetr` detectors still loads; it is migrated
and every change is logged (and printed by `config` commands); the admin
panel marks a retired row "Retired: <reason>":

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
  itself (no `ocr_engine` block). A file of the primary's own recipe at an
  older `ocr_engine.revision` (absent = 1) is a candidate too, listed or not.
- It is skipped when its archive is **missing pages**, or when its recipe
  cannot be read.
- A change nobody marked does not protect a file: an outdated
  `<Volume>.mokuro` that a person or Mokuro Reader 1.10.0 changed without
  marking it edited is replaced like any other. A file Mokuro Reader marked
  **edited** (an `edited` block) is never replaced; when its OCR is outdated,
  the new OCR is written beside it as `<Volume>.<primary generation>.mokuro`
  (for example `Vol 1.hayai-nova.mokuro`) and becomes the main file only if
  the edit mark is removed.
- If another layer already holds the primary's recipe made from the current
  archive, and its seal (`ocr_engine.seal`) proves Bunko wrote it, its bytes
  become the new primary at once. Otherwise the volume gets an upgrade job,
  queued behind every ordinary job (a volume with no OCR always beats one with
  old OCR) and runnable on any machine.
- The new file **replaces** the old primary outright, in one atomic rename;
  no copy of the old file is kept and there is no undo (on a cloud library
  the old file goes to the provider's trash, or its version history where it
  keeps one). (To keep another recipe's OCR beside the primary, add a
  non-primary generation instead.) A crash mid-way never leaves a volume
  without its primary. The `volume_uuid` is preserved, so reading progress is
  untouched, and the replace is audited as `ocr_sidecar_upgraded`.
- The layers 0.7.0-beta.3 kept of replaced files (`<Volume>.mokuro.mokuro`,
  stamped `upgraded_from_primary`) are removed at startup, or at the next
  census, once the volume is upgraded; each removal is logged.

The admin API serves a census and a per-volume action
(`GET /_admin/api/ocr/upgrade`, `POST …/upgrade/<volume>` to upgrade that
volume now).

#### OCR engines

hayai-nova and paddle-manga run on libtorch (the backend pack); the other engines run
on ONNX Runtime on the CPU and need no backend pack.

| Engine | What it is | Runs on | Use it for |
|--------|-----------|---------|------------|
| `hayai-nova` | [hayai-ocr v2.5 "Nova"](https://huggingface.co/JustANormalTinkerer/hayai-ocr-v2.5-nova) | card or CPU | The default. Better text than 0.5's mokuro engine, especially display lettering, sound effects and colour pages. Reads at the row's `patch_budget`. Needs a detector. |
| `paddle-manga` | [PaddleOCR-VL 1.6 + manga LoRA](https://huggingface.co/sorryhyun/paddleocr-vl-1.6-manga-lora) | card recommended | The most accurate and the slowest; about 1.8 GB of model files in bf16/fp16, 3.6 GB in fp32, which the CPU and the default `auto-accuracy` mode run. Needs a detector. On the CPU about 40 s a page on 16 threads: there hayai-nova is the engine of choice. |
| `ppocr-manga` | [PP-OCRv6 manga](https://huggingface.co/Kellenok/PP-OCRv6_manga) line detector + CTC recognizer, 23 MB | CPU only | Reads lines rather than bubbles, removes furigana, and reads scanned **novel** pages (as do the other engines on the `ppocr-manga` detector). Brings its own detector. |
| `charspot-small` | [manga-ocr-charspot-small](https://huggingface.co/muscgab/manga-ocr-charspot-small), 44 MB | CPU only | Detects and classifies each character of a text block; no decoder, so it is fast on a CPU (about five times hayai-nova there), and it leaves furigana out by itself. Japanese only. Needs a detector, a line or a block one. Weights CC BY-NC-SA 4.0. |
| `nar-small` | [manga-ocr-nar-small](https://huggingface.co/muscgab/manga-ocr-nar-small), 213 MB | CPU only | Writes a text block in one pass (at most 64 characters). Japanese only. Needs a detector, a line or a block one. Weights CC BY-NC-SA 4.0. |

**Licences.** `hayai-nova`, `paddle-manga`, `ppocr-manga` and the `ppocr-manga`
detector are Apache-2.0 and are what a default configuration uses. The other models
are opt-in and under other terms:

| Model | Weights | Cargo feature |
|---|---|---|
| `ctd-lines` and `animetext-blocks` detectors | GPL-3.0 | `gpl-models` |
| `charspot-small`, `nar-small` engines | CC BY-NC-SA 4.0: non-commercial use, share alike | `nc-models` |

Nothing of an opt-in model is on the machine until a generation uses it: its files are
downloaded when the first session on it opens. The admin panel shows the licence
beside the name. The code that runs these models is part of this project and under its
licence; only the weights carry the terms above.

Both features are on in the release builds. A build without one (`xtask dist
--permissive-models`, or `cargo build --no-default-features --features ocr,tray`)
does not offer, download or run that class's models: a generation that names one is
held with the reason, on that machine. A server may still list such a generation for
[remote processors](#remote-ocr-processors) built with the feature.

Sidecars from every engine, and every non-primary layer, carry an
`ocr_engine` block saying what produced them (engine, detector, generator
and, where they apply, the patch budget, precision and the model revisions).
The mokuro engine of 0.5.x no longer exists; sidecars it wrote stay as they
are unless [Replacing old OCR](#replacing-old-ocr) is turned on.

#### OCR models

Models are not shipped in the archives. A full build downloads what the
configured generations need into `<storage>/models/`: `serve` and
`processor serve` fetch missing models (and the backend pack) in the
background as they start, and an engine that needs a file before then, or with
`MOKURO_OCR_AUTO_INSTALL=false`, downloads it on first use. Each file's
sha256 is verified before it is moved into place: the PP-OCR ONNX
files, hayai-nova's and paddle-manga's host tables (tokenizer, embeddings), and
the **compiled libtorch packages** for this machine's devices
(`models/torch/<engine>/<precision>/<target>/`, e.g. `linux-cuda-sm_80`,
`linux-rocm-gfx1201`, `linux-cpu-x86_64-v3`, with the shared weights next to them).
CUDA packages exist for sm_75 and sm_80; newer NVIDIA cards (sm_86, sm_89,
sm_120) use the sm_80 package.
`mokuro-bunko install-ocr` and `models download [--engine E]` fetch them up front
(for example before going offline); `models list|verify` inspect them. Sizes:
hayai-nova about 0.3 GB in bf16/fp16 and 0.6 GB in fp32, ppocr-manga about
23 MB, paddle-manga about 1.8 GB in bf16/fp16 and 3.6 GB in fp32.

| Setting | Meaning |
|---|---|
| `MOKURO_MODELS_DIR` | A directory of model files to use instead of the store (also `MOKURO_PPOCR_MODELS`). Files found there are used as they are. |
| `MOKURO_MODELS_DOWNLOAD` | `0`, `false`, `no` or `off` forbids downloads (also `MOKURO_PPOCR_DOWNLOAD`). A machine then offers only the engines and precisions whose files are on disk, and generations needing missing models stay queued rather than fail. |
| `MOKURO_TORCH_MODELS_DIR` | A directory laid out like `<storage>/models/torch/` searched first (development, air-gapped hosts). |
| `MOKURO_TORCH_PACK`, `MOKURO_BACKENDS_DIR` | Use only this backend pack directory / install packs here instead of `<storage>/backends/`, which is still searched after it. Neither is set by the Docker images (a pack built into a self-built image, `<exe dir>/backends`, is searched after `<storage>/backends/`). |
| `MOKURO_TORCH_THREADS` | CPU threads of a CPU recognizer (default: the session's share of the physical cores). |

#### OCR detectors

Every engine but `ppocr-manga` only reads text; a detector finds its lines.
Whichever detector found them, the lines are first read by PP-OCRv6's small
recognizer, then the row's engine reads every line again and the two reads are
merged line by line; the same layout groups the lines into blocks and orders them.

| Detector | What it is | Notes |
|---|---|---|
| `ppocr-manga` (default) | The PP-OCRv6 manga line detector (Apache-2.0) | Rotated per-line boxes with furigana separated, sound effects included. The pairing to use for **novels**. |
| `ctd-lines` | The line map of [comic-text-detector](https://github.com/dmMaze/comic-text-detector) (weights GPL-3.0, 95 MB) | Tight boxes on dialogue; leaves most sound effects and furigana out, so it finds fewer lines. Looks at the page at 1024 px, so small text on large pages can be missed. Opt-in; see [Licences](#ocr-engines). |
| `animetext-blocks` | [AnimeText YOLO12-x](https://huggingface.co/deepghs/AnimeText_yolo) (weights GPL-3.0, 237 MB) | A **block** detector: it boxes whole pieces of text (a balloon's lines together, a sound effect). Only for `charspot-small` and `nar-small`, which read a block at once; see below. The slowest of the three on a CPU (about a third of a second a page). Opt-in. |

All three run on the CPU.

**Lines or blocks.** With a line detector the page is read line by line: PP-OCRv6
reads each line, the row's engine reads it again and the two reads are merged.
`charspot-small` and `nar-small` are made to read a whole text block at once and are
given the layout's blocks rather than single lines; their text is then shared out to
the block's lines, and it stands as read (PP-OCRv6's read is used only where they read
nothing). With `animetext-blocks` no lines are detected at all: the detector's boxes are
the page's blocks, the engine reads each, and the lines are the ones the engine finds
in it: `charspot-small` knows where every character is and builds real lines;
`nar-small` does not, and each block becomes one line the size of the block.

The 0.5 detectors `ctd` (GPL-3.0), `animetext` (GPL-3.0) and `rtdetr` were
removed; a row naming one is switched to `ppocr-manga` when the config loads.
`ctd-lines` and `animetext-blocks` are not the 0.5 `ctd` and `animetext` under other
names: 0.5's `ctd` used comic-text-detector's own blocks and reading order, where
`ctd-lines` takes only its lines; 0.5's `animetext` made one line of every block
whatever the engine. A row that still says `ctd` or `animetext` therefore keeps reading
with `ppocr-manga`; choosing one of the new detectors is a new choice.

#### Pools: stages and devices

Each engine reads a page in **stages**: detection, recognition, assembly.
Each stage has a pool of workers. A row's `pools` applies on every machine
that runs the row; a machine's own values (its profile in
`<storage>/processors/`, saved from the admin panel or by an automatic
benchmark) replace it there, table by table. `{}` (the default) derives
everything from the host or from that machine's benchmark, which is the right
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
| `stage_workers` | `{stage: 1–64}` | How many workers a stage runs. For the `engine` stage on a card, a number above 1 (up to 8) runs that many threads over the one loaded model, only worth it on a card with spare compute; unset, it is 1, or 2 on a fast card in a host with 16 or more physical cores. On the CPU the engine stage stays at 1. |
| `queue_capacity` | `{stage: 1–256}` | How many pages may wait in the queue a stage fills. Capacity is memory (a decoded page can be ~14 MB); the default is small on purpose. |
| `stage_device` | `{stage: cpu \| "gpu:<n>"}` | Where a stage's model runs. Absent means auto: card 0 when there is one, else the CPU. Only stages holding a model take one. |

Which stage names a row has depends on its engine. A stage the row does not
have (such as a 0.5.2 stage name), or a `stage_device` for a stage without a
model, is dropped with a warning when the config loads; an unknown pools
key, an out-of-range number or a GPU for a CPU-only stage is refused:

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

In the admin panel, a processor's pools table offers only that processor's
own cards (plus Auto and CPU); this server's offers every card any known
machine reports. A blank Device cell shows what it resolves to ("Auto →
GPU 0"). A machine's pools never carry a precision: that is the row's
[precision mode](#precision-modes). A `precision` left under a row's `pools`
in an older file becomes the row's mode when the row has none (an invalid
one is refused); in a machine's profile it is dropped.

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
never width-tuned, but still gets this precision-only benchmark. With
automatic benchmarks off (`ocr.autobench: false`), or after a machine's
benchmark of the row failed, nothing is measured automatically: a machine
keeps its current pick if it has one, else runs the first candidate its card
supports.

Each device reports the formats it computes in (the backend pack's
`bt_devices`): GPUs fp32, fp16 and bf16 (as 0.5.2's torch probe said); the CPU
fp32, plus bf16 on CPUs with AVX512-BF16 (Zen 4/5, Sapphire Rapids and newer).
The auto modes use bf16 only where it runs natively: NVIDIA sm_80+ (Ampere and
newer) and AMD RDNA3/RDNA4 (gfx11xx/gfx12xx), never the CPU; elsewhere bf16 is
dropped from the auto candidates (a forced `bf16` still runs on any GPU).
A format also needs its compiled package for the device; one without is "not
supported" there. On the CPU, `auto-accuracy` runs fp32, as 0.5.2 did; bf16 on
the CPU is an opt-in (force `bf16`). A **forced** mode (`fp32`, `bf16`, `fp16`) runs only on machines
that support it; every other machine is not eligible for the row and is never
offered its volumes. When no connected machine can run it, the row is
**held**: the admin card and the queue page (for admins) say "No connected
machine can run …", and `.mokuro-queue.json` shows its jobs as `held`.
`ppocr-manga` fixes its own precision and ignores the mode. Sidecars from
hayai-nova and paddle-manga record the format they ran at in
`ocr_engine.precision`.

#### Benchmark and tune

Every row in the admin panel (Settings → OCR → Generations) has a **Benchmark & tune**
button. It measures the row **as currently edited**, saved or not, on the
selected machine (this server, or a connected processor) and on pages sampled
from your own library: 32 by default (4–512), a few interior pages from each of
several volumes with the series taking turns, so covers and ads do not flatter
the result.

- It **pauses that machine's OCR** for the duration: jobs running there are
  stopped and simply run again afterwards; they are not recorded as
  failures. Other machines keep working. The queue page shows the machine as
  "Benchmarking …" (or "Auto configuring …" for an automatic benchmark).
- It loads the models once, with the row's engine, mode and device pins, and
  warms up on the first few pages, which are discarded. The load appears only
  as "first page after X s"; it is never part of a speed. Widths start from
  the derived values, not the row's pools.
- For an `auto-balanced` or `auto-speed` row it first runs one trial per
  format of the mode that the machine's card can use (auto-balanced: bf16,
  fp32; auto-speed: bf16, fp16, fp32; bf16 only where native, see
  [Precision modes](#precision-modes)); the fastest wins, and two within 5%
  count as a tie that goes to the more accurate format. A format with no
  compiled package for the card is skipped.
- It then tries pool widths, following the pipeline's own bottleneck: it
  widens the stage the pipeline says is in the way while that gains at least
  3%, then gives back workers that are not needed while the speed stays within
  1% of the best. The detector always runs on the CPU, so there is no
  placement to search. At most 8 width trials (plus the format trials) and
  15 minutes.
- Every speed is timed by when pages come out of the pipeline: each trial
  feeds the sample again until it has 20 seconds of output, or after 8 feeds
  or at the 15-minute budget. A trial measured over less than 10 seconds is
  shown but flagged and never decides a width (a short format trial still
  picks the format).
- It reports pages per second, what a 200-page volume would take, how long
  the volumes still owing this generation a sidecar would take at that rate,
  and, where the machine can measure them, how busy the GPU and CPU were
  during each trial, the process's peak memory and how far the card's VRAM
  use rose. **Apply** writes the
  widths it found into the row you are editing (or into that processor's
  settings); you still save.
- Benchmarks queue: several can be requested at once and run one at a time
  per machine; different machines' benchmarks run side by side.

The last result of each saved row on this server is kept in
`<storage>/.ocr-bench.json`; a processor's results are kept in its profile
(`<storage>/processors/`). A benchmark of an unsaved row is kept in memory
only, until a restart.

With `ocr.autobench: true`, a row is benchmarked on a machine automatically,
before that machine is offered its volumes, when the machine has no entry for
the row's current recipe (engine, detector, patch budget), or when the stored
measurement no longer matches the row's precision mode or the formats the
machine's card supports. A machine that already has an entry without a
benchmark (it ran the row with autobench off, or pools were saved for it)
gets no width benchmark. The widths found are kept in that machine's profile
(`<storage>/processors/`), never in `config.yaml`. On this server the width
benchmark runs only for a row whose pools table is empty, though widths an
earlier automatic benchmark saved (`@local.json`) keep precedence over the
table until the row's recipe changes. In every case a balanced/speed row still
gets its precision-only benchmark. A machine whose automatic benchmark fails
runs the row untuned (and on its first supported format) rather than never.

#### Concurrency

`ocr.concurrency` is how many **volumes** this machine OCRs at once. Slots
running the same row on the same device share its loaded models; a different
engine, device, precision or patch budget loads its own copy. It is not how
wide the stages inside one volume run; that is a row's `pools`. Models stay
loaded between volumes.

- Two jobs never run the same generation of a volume; different generations
  of one volume may run at once.
- One job already keeps several CPU cores busy, so about `cores / 3` is a
  useful ceiling on the CPU. On a card, each extra slot of the same row adds
  only working memory (batches and pages in flight). Values above 8 are
  refused.
- A GPU-bound engine gains least: the slots queue for the same card. With the
  default `auto` placement every slot uses card 0; a second card is used
  only by rows pinned to it with `pools.stage_device.engine` (for example one
  row on `gpu:0`, another on `gpu:1`).
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

A processor fetches the models it is missing in the background, at startup
and again once the library names the engines it uses (see
[OCR models](#ocr-models)). It registers anyway and shows as installing
while that runs.

`processor.yaml`:

| Key | Default | Meaning |
|---|---|---|
| `library.url` | — (required) | The library's address, as a browser reaches it. A path prefix is kept. |
| `library.username`, `library.password` | — (required) | The `processor` account. Keep the file mode `600`. |
| `library.password_file` | unset | Used only when `library.password` is empty or absent: the first line of this file (relative to `processor.yaml`'s folder). `MOKURO_PROCESSOR_PASSWORD` overrides both. |
| `library.tls_verify` | `true` | `true`, `false` (a self-signed certificate on a LAN), or the path to the certificate to trust. |
| `processor.name` | the hostname | How the library shows this machine to admins, and the name its settings are filed under. |
| `processor.public_name` | unset | What queue-page visitors see; unset, they see "machine 1", "machine 2"…. At most 64 characters. |
| `processor.max_sessions` | `1` | How many OCR pipelines this machine runs at once: one per card is the rule, like `ocr.concurrency`. |
| `processor.storage` | `$XDG_DATA_HOME/mokuro-bunko-processor` (`~/.local/share/...`) on Linux and macOS, `%LOCALAPPDATA%\mokuro-bunko-processor` on Windows | Logs, working directories, archives too big for memory, the status file, the downloaded OCR models (`<storage>/models`, several GB), the backend pack (`<storage>/backends`) and update state. One per processor; a second processor on the same storage refuses to start. |
| `processor.archive_memory_mb` | `2048` | RAM for the archives being read and the one on deck, shared by every session (in-memory files on Linux); an archive that does not fit goes to `storage`. `0` keeps every archive on disk. Other platforms always use `storage`. |
| `processor.auto_update` | `false` | Follow the library: when it reports a newer version, finish the running volume, install exactly the library's release and restart (never a downgrade). See [Automatic updates](#automatic-updates). |
| `update.manifest_url`, `update.public_key` | GitHub latest, empty | A mirror or fork's latest `release.json`, and its signing key (from this file only; logged loudly). Used by `auto_update` and by the processor's OCR backend pack downloads. |

A 0.5 `ocr:` section in the file (`ocr.backend` chose a torch build) is
accepted and ignored. The processor uses every device its backend pack and
hardware can run; `MOKURO_OCR_BACKEND` (`cuda`, `rocm`, `cpu`) in its
environment narrows that (`processor setup --backend` is accepted for 0.5
scripts and only says so). `processor serve` installs a missing pack in the
background and is not available until it finishes (with
`MOKURO_OCR_AUTO_INSTALL=false` it reports the pack missing instead).
`install-ocr --processor` installs it into `processor.storage` beforehand,
and that is automatic on a machine with a `processor.yaml` and no library
configuration. The processor also looks in the default library storage's
`backends/`, after `MOKURO_BACKENDS_DIR` and its own storage (see
[deployment](deployment.md#3-the-processor-machine)).

**How work is shared.** Each volume goes to the machine predicted to finish
it first, counting what that machine is already doing, whether it has to
load the model, and its measured speed on that generation, so a slower
machine may leave a volume for a faster one that will be free soon. If the
faster machine has not taken it within 15 seconds of when it was expected
to, anyone may. A machine without its own speed for a row is priced from the
row's history or other machines' speeds; only when no speed is known for the
row anywhere, or only one session slot is active, does the first free
machine simply take the next volume.

**Per-machine settings.** Pools, devices and benchmarks belong to a machine:
widths tuned on a 16-core box mean nothing on a 48-core one. While a
processor is connected, a saved generation's card has a **Machine** select:
All machines (the default), this server, and each connected processor. Pick
a machine and its Device select lists that machine's cards, **Save for …**
stores the pools in that machine's profile (never in `config.yaml`), and
**Benchmark & tune** runs on it; with All machines the table is the row's
own and **Benchmark & tune** benchmarks every machine. A processor with no saved values uses the
generation's own table (or what autobench found). Changing a row's engine,
detector or patch budget makes every machine measure it afresh.

**When things go wrong.** A processor that disconnects mid-volume gives its
volumes back to the queue unrecorded, and reconnects with a backoff from
5 seconds to 5 minutes. If the library has no processor connected and does
no OCR itself, the queue holds and says so. Disabling, deleting, re-roling
or changing the password of a processor's account cuts it off within about
15 seconds; its failed login is shown in the admin panel's Processors card.
The Processors card also shows each machine's hardware, its pages a minute
per generation (the benchmark figure until it has finished a volume) and its
state: connected or offline, installing, paused, updating or on another
version, rows that cannot start, and archive downloads failing. What each
machine is working on is on the queue page.

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

With no `--variant`, it installs what `ocr.backend` asks for on this
hardware (`cuda`, `rocm` and `cpu` force that family); under `auto`, the
default, a variant picked by hand at the last install is kept while the
hardware is unchanged. `--variant auto` goes by the hardware alone: `cu130` for an NVIDIA driver 580 or newer, `rocm7.1` for a supported AMD
GPU on Linux, else `cpu`. The pack comes from this version's
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
| `<storage>/.ocr-congestion.json` | The last 5 runs of each row on this server, averaged in each generation card's congestion details. Processors keep theirs in their profiles. |
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

A series folder that loses its last `.cbz` loses its `series.json` too (the folder
itself stays), so readers stop showing a series that is gone. The series' facts stay
in the database, and the next upload into the folder compiles the file again with
them. A `series.json` placed there by hand, carrying facts Bunko has no record of,
is left alone. In a cloud library the file goes to the provider's trash (a version
Bunko wrote itself is deleted), and only while the library is online and in step:
not during a full sync, not while it waits for removals to be confirmed, not while
another Bunko holds the library. A pass held back that way looks again a minute
later, and keeps doing so until the library is back in step.

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
an update *request*: the facts are validated and merged (newest stamp wins), and
both files are regenerated. The server compiles the volume list itself; of each
listed volume it reads only `volume_uuid` and `offset`, the shelf offset (a listed
volume without `offset` has its offset cleared, an unlisted one keeps it), and a
top-level `spine_offset` replaces the stored one (`0` or `null` clears it; a body
without the key keeps it). Offsets apply whatever the
`updated_at` stamp, so a client sends back the offsets it read, or no volume list.
A body carrying only facts, with no volume list at
all, is an equally valid update. Writing `catalog.json`, or deleting/moving either
file, is refused for every account. Submitting an update is ownership-gated, not a
plain progress-write permission: an editor-tier account (or above) may update any
series, an uploader account only a series it owns (see User Roles; otherwise 403),
and a registered-only
account cannot submit updates at all. The account that submitted an accepted
update is recorded in the audit log.

## Environment Variables

Most config keys can be set as `MOKURO_<SECTION>_<KEY>`, which overrides the
config file (for the exceptions, see below):

```bash
MOKURO_SERVER_HOST=127.0.0.1
MOKURO_SERVER_PORT=9000
MOKURO_SERVER_THREADS=2
MOKURO_REGISTRATION_MODE=invite
MOKURO_SSL_ENABLED=true
MOKURO_OCR_LOCAL_PROCESSING=false
MOKURO_UPDATE_CHECK=false
MOKURO_ANALYTICS_ENABLED=false
MOKURO_ANALYTICS_RETENTION_IP_DAYS=14
MOKURO_OCR_GENERATIONS='[{"name":"hayai-nova","engine":"hayai-nova","primary":true}]'
```

Booleans take `true`/`false` (or `1`/`0`, `yes`/`no`). Lists such as
`MOKURO_SERVER_TRUSTED_PROXIES` are comma-separated. `MOKURO_OCR_GENERATIONS`
is the JSON text of the whole list. `cors.allowed_origins` has no variable;
set it in the file or with `mokuro-bunko config cors-add`; `ocr.upgrade.*`,
`update.public_key` and the `database` section have none either;
`storage.library` has only the short variables listed below. Bad values
are refused at startup with the variable's name in the message. The CLI's
config commands never write overrides back into `config.yaml`, but an admin
panel save or the setup wizard writes the whole running configuration,
overrides and `serve` flags included (behind the bundled nginx, the backend's
`127.0.0.1:8081` too).

Other variables:

| Variable | Default | Meaning |
|---|---|---|
| `MOKURO_CONFIG` | platform default | Config file path (the same as `--config`). |
| `MOKURO_HOST`, `MOKURO_PORT`, `MOKURO_STORAGE` | — | Short aliases for `server.host`, `server.port`, `storage.base_path`. |
| `MOKURO_SERVE_READERS` | — | Short alias for `server.serve_readers`. |
| `MOKURO_LIBRARY_BACKEND`, `MOKURO_LIBRARY_FOLDER` | — | `storage.library.backend` and `folder` (see [Library location](#library-location)). |
| `MOKURO_LIBRARY_PATH` | — | `storage.library.path`: the library folder of `backend: folder` (see [A library folder you choose](#a-library-folder-you-choose)). |
| `MOKURO_WEBDAV_URL`, `MOKURO_WEBDAV_USERNAME`, `MOKURO_WEBDAV_ALLOW_HTTP` | — | `storage.library.webdav.url`, `username`, `allow_http`. |
| `MOKURO_WEBDAV_PASSWORD_FILE` | — | A file holding the WebDAV library's password (never set in `config.yaml`). |
| `MOKURO_GOOGLE_PROJECT` | `release` | `storage.library.google_project`: `release` or `beta`. |
| `MOKURO_LOG` | `info` (`debug` with `-v`) | The log filter (`tracing` syntax), for example `debug` or `info,bunko_server=debug`. |
| `MOKURO_NGINX_ACCEL` | unset | On the full Docker image, `1` (or `true`/`yes`) starts the bundled nginx in front, which serves library downloads via `X-Accel-Redirect`; the server moves to `127.0.0.1:MOKURO_BACKEND_PORT`. Without nginx (a lite image, or a container not started as root) it is dropped with a warning. Outside Docker, set `1` only when your own nginx serves the `/internal-library/` location. |
| `MOKURO_INSTALL_KIND` | detected | `self`, `docker` or a package manager's name; decides what the updater does (see [Updates](#updates)). |
| `MOKURO_UPDATE_CHECK_SECONDS` | `43200` | The background release check's period (and, up to 60 s, its first delay), for mirrors that publish often and tests. |
| `MOKURO_UPDATE_RETRY_SECONDS` | `600` | The first wait after a failed automatic update (doubling to 12 h). |
| `MOKURO_MODELS_DIR`, `MOKURO_MODELS_DOWNLOAD` | — | A directory of model files to use; `0` forbids model downloads (see [OCR models](#ocr-models)). The 0.5 names `MOKURO_PPOCR_MODELS` and `MOKURO_PPOCR_DOWNLOAD` still work. |
| `MOKURO_PROCESSOR_CONFIG` | — | `processor.yaml` path for `processor serve`, `status` and `service` (the same as their `--config`); `install-ocr`, `doctor` and the tray find it there too. `processor setup` ignores it. |
| `MOKURO_PROCESSOR_PASSWORD` | — | The processor's password; overrides `library.password` and `password_file`. |
| `MOKURO_DEBUG` | unset | Any value but empty/`0`/`false`: one line per request on stderr (`[REQUEST] METHOD path -> status (seconds)`), as in 0.5.2. |
| `MOKURO_OCR_AUTO_INSTALL` | `true` | `false`: `serve` / `processor serve` never install a missing OCR backend by themselves (anywhere, not only in Docker); the missing backend is a problem with an Install button (dashboard, admin panel), or run `mokuro-bunko install-ocr`. 0.5.2's `OCR_AUTO_INSTALL=true` also keeps it on; `OCR_AUTO_INSTALL=false` is ignored. |

The 0.5 variables `MOKURO_THREADS` (use `MOKURO_SERVER_THREADS`), `MOKURO_EFT_TRACE`,
`MOKURO_BUNKO_OCR_ENV`, `MOKURO_BUNKO_OCR_ENGINES_ENV`,
`MOKURO_BUNKO_MOKURO_SPEC`, the `MOKURO_PPOCR_*` tuning variables and the
`MOKURO_OCR_STAGE_*` / `MOKURO_OCR_CPU_WORKERS` pipeline overrides are no
longer read. The Docker-only variables (`PUID`, `PGID`, `UMASK`,
`TAKE_OWNERSHIP`, `MOKURO_BACKEND_PORT`, `OCR_AUTO_INSTALL`) are described in
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

### Under a sub-path (`https://example.com/bunko/`)

The web pages (home, catalog, login, registration, account, queue, admin, setup) work
under a sub-path. The proxy strips the prefix, so Bunko sees `/catalog/` for
`https://example.com/bunko/catalog/`. Bunko never needs to know the prefix:

- every page gets a `<base href>` that leads from its own path back to the mount root
  (`./` at `/catalog`, `../` at `/catalog/`)
- every URL the pages and scripts use is relative to it
- redirects are relative too (`/` answers `302 setup` or `302 catalog/`)

The catalog's "open in reader" links are absolute URLs under the mount root.

Redirect the bare prefix to the prefix with a slash, so the pages' relative URLs
resolve under the mount:

```nginx
location = /bunko {
    return 301 /bunko/;
}

location /bunko/ {
    # The trailing slash on proxy_pass strips /bunko.
    proxy_pass http://127.0.0.1:8080/;
    proxy_set_header Host $host;
    proxy_set_header X-Real-IP $remote_addr;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;
    client_max_body_size 0;
    proxy_request_buffering off;
}
```

Add the proxy to `server.trusted_proxies` when it runs on another host. In Mokuro
Reader, enter the server as `https://example.com/bunko`: the reader looks for
`<mount>/login/api/me` first.

Not prefix-aware:

- WebDAV `PROPFIND` answers name their hrefs from the server root
  (`/mokuro-reader/...`).
- A remote OCR processor's registration hands out root paths (`/_processor/...`), so
  processors need a server at the root of its host.

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

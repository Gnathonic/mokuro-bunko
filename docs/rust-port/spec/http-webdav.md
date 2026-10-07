# HTTP server + WebDAV layer (mokuro-bunko 0.5.2 -> Rust server)

Subsystem: `server.py` (app assembly, startup, shutdown), `webdav/` (provider + resources),
`middleware/` (auth, cors, upload, propfind_cache, queue_file, fs_watcher, request_log,
security_headers), `security.py`, `static/`, plus the parts of **WsgiDAV 4.3.5** and **cheroot 11.1.2**
that the Python server silently inherits (the Rust server must re-implement those: they ARE the
protocol behaviour clients see).

Source root: `src/mokuro_bunko` (the Python 0.5.2 tree)
(paths below are relative to it). Tests: `../../tests` relative to that, i.e.
`tests` (the Python 0.5.2 tree).

Contents: 0 Conventions - 1 Process model/startup/shutdown - 2 App assembly, middleware order, route index - 3 Request primitives (path encoding, client IP, containment, limiter) - 4 Auth (authn, permission matrix, decision procedure) - 5 CORS - 6 Security/cache headers - 7 Static files and UI mounts - 8 WebDAV semantics (tree, properties, every method, conditionals, errors, audit, write locks, writers, nginx offload) - 9 Upload middleware (verdicts) - 10 PROPFIND cache - 11 Virtual queue file - 12 Watcher, invalidation, background timings - 13 Request log - 14 Quirks catalogue - 15 DROP list - 16 Test index - 17 Open questions.

Headline findings: (a) WsgiDAV 4.3.5 supplies most protocol behaviour and must be re-implemented (section 8); (b) three destructive MOVE/COPY/DELETE quirks (14.1-14.3) are verified and must be fixed, not ported; (c) PUT is staged, size/digest/zip-CRC verified, then atomically renamed, with a JSON verdict on failure (8.8, 9); (d) middleware order matters: UI routes run before auth, the PROPFIND cache runs after auth (2.1).

## 0. Conventions

* `file.py:N` = this repo, `src/mokuro_bunko/file.py` line N.
  `wsgidav:request_server.py:N` = WsgiDAV 4.3.5 (`site-packages/wsgidav`; pyproject only says
  `wsgidav>=4.0`, `cheroot>=10.0`, so the exact versions are not pinned; behaviours below were
  **verified by running** the real stack on 4.3.5 unless marked otherwise).
* Tags:
  * **KEEP**: replicate.
  * **FIX**: Python behaviour is a bug / data-loss hazard; the spec says what the Rust server must do instead.
  * **QUIRK**: odd but harmless or pinned by a test; replicate unless noted.
  * **DROP**: exists only for mokuro/manga-ocr, ctd, animetext, rtdetr (being removed) or only works
    around Python/GIL/cheroot/Windows problems. Do not port.
  * **OCR-QUEUE**: tied to the OCR work queue / processors (`OcrControl`); the HTTP contract is given so
    the owner of that subsystem can decide; with no queue the stated "no control" behaviour applies.
* "Environ" = the Python WSGI environ; for Rust read it as per-request extensions
  (section 2.3).
* Header names are case-insensitive on the wire; the casing shown is what Python emits.
* All statuses/headers shown for the stack were captured from a full `create_app()` run
  (`[VERIFIED]`).

---------------------------------------------------------------------------------------------

## 1. Process model, startup, shutdown

### 1.1 Server (cheroot) parameters `[KEEP the externally visible ones]`

`create_ssl_server` (`server.py:498-542`) is the ONLY server factory (also used for plain HTTP; there
is no separate non-SSL path).

| Setting | Value | Cite |
|---|---|---|
| Bind | `(config.server.host, config.server.port)`, defaults `0.0.0.0:8080`; `port` 0 allowed (random) | `server.py:522-524`, `config.py:47-69` |
| Worker threads | `MOKURO_THREADS` env, default **50**, fixed pool (cheroot `numthreads`, `max=-1`) | `server.py:525` |
| Socket/keep-alive timeout | cheroot default **10 s** (`Server.timeout`); idle keep-alive connections are reaped by cheroot's selector (`expiration_interval` 0.5 s) and do not hold a thread | cheroot `server.py:1561,1564` |
| Listen backlog | cheroot default 5 | cheroot `server.py:1551` |
| Request header / body size limits | **none** (`max_request_header_size = 0`, `max_request_body_size = 0`) | cheroot `server.py:1581-1584` |
| TCP_NODELAY | on | cheroot `server.py:1587` |
| Shutdown wait | cheroot `shutdown_timeout` 5 s | cheroot `server.py:1556` |
| Protocol | HTTP/1.1 only (no h2, no ALPN) | |
| Response `Server:` / `Date:` | cheroot adds `Server: Cheroot/<ver> WSGI Server` and a `Date:` if the app set none | cheroot behaviour |
| 100-continue | cheroot answers `HTTP/1.1 100 Continue` for `Expect: 100-continue` before the app runs (so a big PUT is announced OK even if auth later says 401) | cheroot behaviour (not re-verified) |
| Chunked request bodies | de-chunked by cheroot; WSGI then has no `CONTENT_LENGTH` (the upload writer treats this as "length unknown", section 8.9) | `webdav/resources.py:230-239` |
| TLS | `BuiltinSSLAdapter(cert_file, key_file)` (Python `ssl` defaults: TLS >= 1.2, system default ciphers) when `config.ssl.enabled` | `server.py:529-540` |

**Idle-socket consequence:** a client that stalls for more than ~10 s mid-request (slow upload, no
bytes) is cut by cheroot. The production image puts nginx in front (`proxy_request_buffering off`,
`client_max_body_size 2048M`, `proxy_read_timeout 300s`; `deploy/nginx-internal.conf.template`), so
the effective limits behind the bundled nginx are nginx's. Open question Q1: what read/idle timeouts the Rust server
should use.

TLS details:

* `ssl.enabled && ssl.auto_cert`: cert/key at `get_default_cert_paths()` =
  `$XDG_DATA_HOME|~/.local/share` (Windows: `%LOCALAPPDATA%`) + `/mokuro-bunko/certs/{cert.pem,key.pem}`
  (`ssl.py:22-34`). Generated at server construction iff **either** file is missing
  (`server.py:531-534`): RSA-2048, SHA-256, valid 365 days from now, subject `CN=localhost, O=mokuro-bunko`,
  SAN = `localhost`, `localhost`(hostname arg), `127.0.0.1`, `socket.gethostname()`, BasicConstraints
  `CA:FALSE` critical, key PEM TraditionalOpenSSL unencrypted (`ssl.py:37-112`). An existing auto-cert
  is **never** regenerated or expiry-checked (QUIRK: an expired auto cert starts fine).
* `ssl.enabled && !auto_cert`: `cert_file` / `key_file` (`~` expanded at validation).
* `SslConfig` refuses `enabled && !auto_cert` without both files (`config.py:187-193`).

### 1.2 `run_server` sequence (`server.py:592-1037`)

1. `_validate_startup_environment(config)` (`server.py:64-102`), on `ValueError`: print
   `Startup validation failed: <msg>` to stdout, `SystemExit(2)` (`server.py:602-606`). **KEEP**.
   * `config.storage.ensure_directories()`: mkdir -p `library`, `inbox`, `users`, and `library/thumbnails`
     (`config.py:98-103`). `thumbnails/` therefore exists in the library and shows up in PROPFIND of
     `/mokuro-reader/` (section 8.3) (QUIRK).
   * `_assert_writable_dir` for `base_path`, `library`, `inbox`, `users` (`server.py:50-61`): must exist, be
     a directory, and accept creating+deleting `<dir>/.mokuro-write-test` (content `ok`). Messages:
     `Required directory does not exist (<label>): <path>`, `Required path is not a directory (<label>): <path>`,
     `Directory is not writable (<label>): <path>`; labels `storage.base_path`, `storage.library_path`,
     `storage.inbox_path`, `storage.users_path`.
   * SSL disabled: done. SSL auto_cert: mkdir the cert/key parent dirs, require them writable
     (labels `ssl auto-cert directory`, `ssl auto-key directory`), done (**no** cert validation).
   * SSL explicit: `SSL certificate file not found: <path>` / `SSL private key file not found: <path>`
     if not regular files; then `validate_certificate_pair` (`ssl.py:196-246`): loadable pair
     (`SSL certificate/key validation failed: <exc>`), parseable cert (`Failed to parse certificate file: ...`),
     `SSL certificate has expired: <iso>`, `SSL certificate is not valid yet: <iso>`; the "expires within
     30 days" warning is computed and **discarded** (`server.py:100`). Only `errors[0]` is raised.
   Pinned by `tests/unit/test_startup_validation.py`.
2. `setup_logging(base_path, verbose)` (`logging_setup.py:33-89`): root logger DEBUG; console handler
   (stdout, INFO, DEBUG if `-v`); `RotatingFileHandler` `<base>/logs/<SERVER_LOG_NAME>`, 2 MiB x 5 backups,
   INFO+, utf-8, `delay=True`; failure to create the log dir only warns. `wsgidav`, `cheroot`, `urllib3`
   loggers forced to WARNING. `[KEEP semantics: console + rotating file]`.
3. OCR environment detection/installation, generation row pruning, `OcrControl` etc.
   (`server.py:615-898`): **[DROP]** (mokuro/engines installers, detectors `ctd`/`rtdetr`/`paddle`,
   `needs_mokuro_env`, GPU backend selection, `local_problems`). The only HTTP-visible outputs are
   `ocr_runtime` (admin API) and the `OcrControl` handle passed to `create_app`.
4. `create_ssl_server(...)` -> `create_app(...)` (section 2) + cheroot server (`server.py:898-900`).
5. `ThreadPoolWatchdog(server).start()` (`server.py:936-938`) **[DROP]**: replaces dead cheroot threads every
   5 s and clears `server.interrupt` (cheroot issue #375/#710, Windows socket errors). Python/cheroot only
   (`cheroot_watchdog.py`).
6. OCR worker construction/start, `probe_devices`, `set_cached_catalog` (`server.py:940-1018`) **[DROP / OCR-QUEUE]**.
   Note the worker is built even when no local OCR runs because it owns the queue; whether a Rust build
   keeps an OCR/processor queue is outside this spec.
7. `print("Press Ctrl+C to stop")` then `_start_server_resilient(server)` (`server.py:1019-1022`).
8. `_start_server_resilient` (`server.py:545-589`) **[DROP]**: re-implements cheroot's `serve()` loop so a
   worker thread setting `server.interrupt` does not stop the accept loop; starts cheroot's
   "UnservicableHandler" thread. Pure cheroot workaround.
9. Shutdown (`server.py:1023-1037`): only `KeyboardInterrupt` (Ctrl+C) is handled. `finally`:
   `watchdog.stop()`, `ocr_worker.stop()`, `remote.drop_all("the library server is shutting down")`,
   `shutdown_app(server.wsgi_app)`, `server.stop()`.
   **QUIRK: there is no SIGTERM handler anywhere in the server** (`grep signal` only hits processor/engine
   code). `docker stop` therefore kills the process without running any of this. **FIX for Rust**: handle
   SIGINT and SIGTERM identically (graceful stop).

`shutdown_app(app)` (`server.py:470-495`): idempotent (`hasattr` guards); order is pinned by
`tests/unit/test_server_shutdown_order.py`:
`library_watcher.stop()` -> `community_fetcher.stop()` -> `metadata_service.stop()` (BEFORE the PROPFIND
cache: a just-finished metadata pass fires `on_published` after releasing its lock; stopping the cache
first would let that late `schedule_refresh` arm an uncancellable timer) -> `propfind_cache.stop()` ->
`dyndns_service.stop()`. After `stop()` the cache's `schedule_refresh` is a no-op (`propfind_cache.py:407-425`).

### 1.3 Environment variables read by this subsystem

| Var | Meaning | Cite |
|---|---|---|
| `MOKURO_NGINX_ACCEL` | exactly `"1"` => offload library downloads (section 8.10) | `server.py:239` |
| `MOKURO_THREADS` | worker threads, default 50 | `server.py:525` |
| `MOKURO_DEBUG` | any value other than `""`, `0`, `false` (after strip) => request log (section 13) | `middleware/request_log.py:17-18,26` |
| `MOKURO_CONFIG`, `MOKURO_HOST`, `MOKURO_PORT`, `MOKURO_STORAGE`, `MOKURO_<SECTION>_<KEY>` | config overrides (config spec) | `config.py:660-710`, `__main__.py:38` |
| `XDG_DATA_HOME` / `LOCALAPPDATA` | auto-cert dir | `ssl.py:22-34` |

CLI `serve` defaults (`__main__.py`): `--host 0.0.0.0`, `--port 8080`, `--ocr auto`; a CLI value only
overrides the config when it differs from the default.

---------------------------------------------------------------------------------------------

## 2. App assembly: middleware stack and request flow

### 2.1 Stack (outermost first = order a request traverses)

Built inside-out in `create_app` (`server.py:165-467`). The code comment at `server.py:213-232`
lists the order inside-out; this is the verified request-order:

| # | Layer | Handles / does | Needs auth data? |
|---|---|---|---|
| 1 | `RequestLogMiddleware` (`middleware/request_log.py`) | debug log only | |
| 2 | `SecurityHeadersMiddleware` (`security_headers.py`) | adds headers to EVERY response (section 6) | |
| 3 | `CorsMiddleware` iff `config.cors.enabled` (`server.py:409-410`) | preflight answers + CORS headers (section 5) | |
| 4 | `StaticMiddleware` (`static/__init__.py`) | `/_static/*`, `/robots.txt` (section 7) | |
| 5 | `SetupWizardAPI` | `/setup*`, `GET /` -> 302 `/setup` while no admin exists | other spec |
| 6 | `HomePageAPI` | `/api/health`, `/api/stats`, `/_home/*`, `GET /` (browser) | other spec |
| 7 | `AccountAPI` | `/account*`, `/api/account/*` | other spec |
| 8 | `LoginAPI` | `/login*`, `/login/api/{check,token,me}`, `/api/nav/config` | other spec |
| 9 | `RegistrationAPI` | `/api/register*`, `/register*` | other spec |
| 10 | `QueueAPI` | `/queue*` | other spec |
| 11 | `CatalogAPI` | `/catalog*`, `GET /catalog/api/manifest` | other spec |
| 12 | `QueueFileMiddleware` | virtual `/mokuro-reader/.mokuro-queue.json` (section 11) | calls `AuthMiddleware.gate_read` |
| 13 | `UploadMiddleware` | rewrites the answer of PUT/MOVE/COPY/DELETE, adds `X-Mokuro-*` (section 9) | |
| 14 | `AuthMiddleware` | authn + authz for everything below (section 4) | sets `mokuro.*` |
| 15 | `MetadataAPI` | `PUT <Series>/series.json` | metadata spec |
| 16 | `ProcessorAPI` | `/_processor/*` (mounted unconditionally) | processor spec |
| 17 | `AdminAPI` iff `config.admin.enabled` | `/_admin*` | admin spec |
| 18 | `PropfindCacheMiddleware(ttl=120)` | Depth:infinity PROPFIND cache + write invalidation (section 10) | |
| 19 | `_nginx_accel_flag` iff `MOKURO_NGINX_ACCEL=1` | sets `mokuro.nginx_accel=True` | |
| 20 | `WsgiDAVApp(MokuroDAVProvider)` | all WebDAV semantics (section 8) | |

Consequences the Rust port must preserve:

1. Layers 5-11 sit **outside** `AuthMiddleware`; each does its own credential handling
   (Basic/Bearer through `authenticate_basic_header`, or its own limiter). The WebDAV limiter
   (`middleware/auth.py:27`) and the login-page limiter (`login/api.py:32`) are **separate in-memory
   instances**: failures against `/login/api/*` do not count toward WebDAV and vice versa.
2. Security headers and CORS (layers 2-3) wrap everything, including the UI/API routes and every
   error produced inside.
3. Admin/Processor/Metadata/DAV run only after `AuthMiddleware` authorised the request (its
   gate for `/_admin`/`/_processor` is section 4.4).
4. `UploadMiddleware` sees the final answer of the auth+DAV chain, so even a 401/403 from
   `AuthMiddleware` for a PUT is converted into the JSON verdict (section 9).
5. The PROPFIND cache is **inside** auth and **outside** WsgiDAV: only authorised requests reach it.
6. `app._propfind_cache`, `_library_index`, `_library_watcher`, `_metadata_service`, `_dyndns_service`,
   `_community_fetcher` are attached to the outermost object for `shutdown_app` / warm-up
   (`server.py:418-419,445-448,465`).

### 2.2 WsgiDAV configuration (`server.py:105-151`) and its inherited behaviour

```
provider_mapping {"/": MokuroDAVProvider}   # single share at "/", mount_path ""
http_authenticator {domain_controller None, accept_basic False, accept_digest False, default_to_digest False}
simple_dc.user_mapping {"*": True}          # => anonymous allowed: WsgiDAV performs NO auth of its own
dir_browser.enable False                    # GET on a collection => 403 (section 8.3.4)
lock_storage True                           # in-memory LockStorageDict + LockManager (section 8.3.9)
property_manager True                       # in-memory dead-property store (section 8.3.8)
add_header_MS_Author_Via True               # adds "MS-Author-Via: DAV" to OPTIONS
verbose 1, logging.enable_loggers []
```

Defaults inherited (`wsgidav:default_conf.py`): middleware stack is `Cors` (disabled: no `cors`
config), `ErrorPrinter`, `HTTPAuthenticator` (passes everything, sets `wsgidav.auth.user_name=""`),
`WsgiDavDirBrowser` (disabled), `RequestResolver`; `re_encode_path_info=True` (section 3.1);
`block_size` 8192 (GET read chunk / PUT write chunk); `honor_mtime_header=False` (so `X-OC-Mtime` is
ignored); `mutable_live_props=[]`; `suppress_version_info=False`; `default_charset utf-8`.

Because `wsgidav.auth.user_name` is `""` for every request, **WsgiDAV's lock "principal" is the empty
string for everyone** (section 8.3.9).

WsgiDAV end-of-response hotfix (`wsgidav:wsgidav_app.py:484-539`): every response it produces for a
body-bearing status (>=200, not 204/304, not HEAD) must carry a `Content-Length`, otherwise it adds
`Connection: close`; and it drains the unread request body (`read_and_discard_input`) so
Windows/Vista clients see the response. If the body was not fully consumed it also adds
`Connection: close`. Rust must either drain the body or close the connection when it answers before
reading a request body (this also applies to Python's own `AuthMiddleware` 401/403 answers, which are
produced above WsgiDAV and rely on cheroot to handle the unread body).

### 2.3 Per-request context keys (`mokuro.*`; "environ")

Set by `AuthMiddleware.__call__` (`middleware/auth.py:408-414`) for every request that reaches it:

| Key | Value |
|---|---|
| `mokuro.auth` | `AuthResult{authenticated, user, role, error, attempted_username}` |
| `mokuro.user` | `UserDict{id, username, role, status, notes, created_at}` or `None` |
| `mokuro.role` | role string; `"anonymous"` when not authenticated |
| `mokuro.username` | username or `None` |
| `mokuro.db` | the `Database` handle (WebDAV resources read it for audit/ownership writes) |

Other keys: `mokuro.nginx_accel` (bool, layer 19), `mokuro.archive_written` (Path of a written library
`.cbz`), `mokuro.archives_removed` (list of removed library Paths), `mokuro.upload` (`UploadOutcome`).
All set by resources, read by `UploadMiddleware` (section 9).

Roles (`middleware/auth.py:43-85`): `anonymous`, `registered`, `uploader`, `inviter`, `editor`, `admin`,
`processor` (legacy role names are normalised by the DB layer: `writer` -> `uploader`).

### 2.4 Full route index of the HTTP stack

Everything not listed falls through to WebDAV (-> 404 / 403 per section 8). "Spec" = which other spec
owns the payload shapes.

| Method + path | Answered by | Auth (HTTP gate) |
|---|---|---|
| `GET /robots.txt` | Static (7.2) | none |
| `GET /_static/<file>` | Static (7.1) | none |
| `GET /setup/api/status`, `POST /setup/api/complete`, `GET /setup`, `/setup/`, `/setup/<file>`; `GET /` -> `302 /setup` when setup needed and `Accept` has `text/html` | SetupWizardAPI | local-only while no admin (`setup/api.py:67-115`) |
| `GET\|OPTIONS /api/health`, `GET\|OPTIONS /api/stats` (other methods 405 JSON `{"error":"Method not allowed"}`), `GET /_home/<file>`, `GET /` (browser) -> `302 /catalog/` if `catalog.enabled && catalog.use_as_homepage`, else `index.html` | HomePageAPI (`home/api.py:101-141`) | none |
| `GET /account`, `/account/`, `/account/<file>`, `GET /api/account/stats`, `POST /api/account/password`, `POST /api/account/delete`, `OPTIONS /api/account/*` | AccountAPI | own (bearer/basic) |
| `POST /login/api/check`, `POST\|DELETE /login/api/token`, `GET /login/api/me`, `GET /api/nav/config`, `GET /login`, `/login/`, `/login/<file>` | LoginAPI | own |
| `/api/register` (POST/GET/OPTIONS), `/api/register/config` (GET/OPTIONS), `/register`, `/register/`, `/register/<file>` | RegistrationAPI | none |
| `/queue`, `/queue/`, `/queue/api/config`, `/queue/api/status`, `/queue/<file>` | QueueAPI | own (optional) |
| `/catalog`, `/catalog/`, `/catalog/api/*` (incl. `GET /catalog/api/manifest`), `/catalog/<file>` | CatalogAPI | own; manifest uses `AuthMiddleware.gate_read` |
| `GET\|HEAD /mokuro-reader/.mokuro-queue.json` (+ 405 for any write and MOVE/COPY onto it) | QueueFileMiddleware (11) | `gate_read` |
| `PUT <Series>/series.json` | MetadataAPI | `AuthMiddleware._authorize_put` |
| `/_processor/*` | ProcessorAPI | role `processor` |
| `/_admin*` | AdminAPI | role `admin` (`/_admin/api/invites*`: `MANAGE_INVITES`) |
| everything else | WebDAV (8) | `AuthMiddleware` (4) |

---------------------------------------------------------------------------------------------

## 3. Request-side primitives

### 3.1 Path encoding (`metadata/paths.py` helper, WsgiDAV hotfix)

cheroot delivers `PATH_INFO` percent-decoded and then decoded **latin-1** (PEP 3333). WsgiDAV applies
`re_encode_wsgi` (`str.encode("iso-8859-1").decode("utf-8")`, **no fallback**) as the first thing in
`WsgiDAVApp.__call__` (`wsgidav:wsgidav_app.py:431-432`), so every DAV path is the UTF-8 decoding of the
request path bytes. Middleware above WsgiDAV that needs to compare paths (auth, metadata) apply the
same transform to a local copy with `re_encode_wsgi_path` (`metadata/paths.py:115-135`), falling back
to the unchanged string on `UnicodeError`.

**Rust rule:** decode the request path once as percent-decoded UTF-8 bytes; all comparisons below are on
that Unicode string. A path that is not valid UTF-8: Python raises inside WsgiDAV -> cheroot answers a bare
500 for DAV paths and the auth layer silently uses the latin-1 string (QUIRK, no folder can match it).
Rust should answer `400`/`404`; no client depends on the 500.

### 3.2 Client IP and trusted proxies (`security.py:31-80`)

* `set_trusted_proxies(list_of_cidrs)` is called once in `create_app` (`server.py:188`) from
  `server.trusted_proxies` (`ServerConfig`: each entry must parse as an IP network or address,
  `strict=False`; default `[]`).
* `_is_proxy_peer(REMOTE_ADDR)`: true iff the peer parses as an IP and is **loopback** (always trusted;
  that is nginx in the same container) or inside a configured network. A private LAN address is NOT
  enough.
* `get_client_ip(environ)`: `remote = REMOTE_ADDR.strip()`. If the peer is not a proxy -> `remote`
  (proxy headers ignored). Else `X-Real-IP` (stripped, non-empty) wins; else the **rightmost** entry of
  `X-Forwarded-For` (split on `,`, stripped, non-empty); else `remote`.
* Users: rate-limit key `"<client_ip>:<username>"` (WebDAV and login page), the setup wizard's
  local-only check (`setup/api.py:178-182`: peer must be loopback AND the resolved client IP loopback),
  processor-refused-login messages (`invalid credentials from <ip>`).
* `is_loopback_ip(v)`: valid IP and `is_loopback`.
* Pinned by `tests/unit/test_client_ip.py`.

### 3.3 Path containment helpers (`security.py:12-28`)

* `is_within_path(path, base)`: `path.resolve().is_relative_to(base.resolve())`; false on `OSError/ValueError`.
* `safe_resolve_under(base, relative)`: `(base / relative).resolve()` (**follows symlinks**, collapses
  `.`/`..`), returns it iff `is_relative_to(base.resolve())`, else `None`. A symlink inside the library that
  points outside it therefore resolves to `None` (-> 404 for GET/PUT/DELETE of that path) while a
  directory listing built from `scandir` would still show it (QUIRK; section 8.2).
  Rust: canonicalise (resolve symlinks) the joined path and require it to stay under the canonical
  base; treat a failure to canonicalise a not-yet-existing leaf by canonicalising the existing prefix.

### 3.4 Auth attempt limiter (`security.py:91-139`)

`AuthAttemptLimiter(max_failures=10, window_seconds=300, block_seconds=900)`, one process-global
instance per consumer, keyed `"<ip>:<username>"`, monotonic clock, mutex.

* `allow_attempt(key)`: if `blocked_until > now` -> `(False, int(blocked_until-now)+1)`. Else drop
  failures older than 300 s; if `len(failures) >= 10` -> block for 900 s, clear failures, return
  `(False, 900)`. Else `(True, 0)`. (It also `setdefault`s a deque for every key it sees: unbounded
  growth by distinct keys; Rust should evict.)
* `record_failure(key)` appends now. `record_success(key)` forgets failures and any block.
* Effect: the first 10 wrong passwords answer `401`; the 11th and later within the block answer `429`
  with body `Too many failed attempts. Retry in 900s` (decreasing seconds). A block holds even if the
  right password is then supplied. `[VERIFIED]`. **No `Retry-After` header is sent** (QUIRK; Rust may add).

---------------------------------------------------------------------------------------------

## 4. Authentication and authorisation (`middleware/auth.py`)

### 4.1 Authentication (`AuthMiddleware.authenticate`, `auth.py:470-515`)

Reads `Authorization`:

1. `Bearer <token>` (prefix exactly `"Bearer "`): `database.resolve_auth_token(token.strip())`
   (token row + live user row; disabled/deleted/re-roled users are seen immediately; expired -> None).
   Success -> authenticated with the user's role. Failure -> `error = "Invalid or expired token"`.
   **No rate limiter** (tokens are 32 random bytes) (`auth.py:285-305`).
2. `Basic <b64>` (`parse_basic_auth_checked`, `auth.py:236-267`): base64 -> **UTF-8** decode (RFC 7617;
   the challenge advertises `charset="UTF-8"`); must contain `:`; split on the FIRST `:` (password may
   contain `:`). Header present-but-unusable (bad base64, not UTF-8, no colon) -> `error = "Invalid authorization header"`
   -> 401, **never** anonymous, **no limiter interaction**. A Latin-1 header is therefore malformed
   (`tests/integration/test_auth.py` "UTF-8-only Basic auth").
3. Anything else / absent (Negotiate, Digest, no header): anonymous, no error.
4. Valid Basic: `key = "<client_ip>:<username>"`; `allow_attempt(key)` false -> unauthenticated with
   `error = "Too many failed attempts. Retry in {n}s"` and `attempted_username`; else
   `database.authenticate_user(username, password)` (bcrypt; user must have `status == "active"`):
   success -> `record_success`, authenticated; failure -> `record_failure`, `error = "Invalid credentials"`,
   `attempted_username = username`. Exactly one password check per request (no Latin-1 fallback).

### 4.2 Permission model

```
READ            anonymous registered uploader inviter editor admin processor
WRITE_PROGRESS            registered uploader inviter editor admin
ADD_FILES                            uploader inviter editor admin
MODIFY_DELETE                                 inviter editor admin
MANAGE_INVITES                                inviter        admin
ADMIN                                                        admin
PROCESS                                                              processor
```
(`auth.py:43-85`; `processor` has only READ+PROCESS; `METHOD_PERMISSIONS` at `auth.py:88-101` is dead
code: nothing reads it.)

Predicates (`auth.py:114-165`), all on the path with `"/" + path.strip("/")` normalisation where noted:

* `is_progress_file(p)`: normalised `p` starts with `/mokuro-reader/` and the remainder is **exactly** one of
  `volume-data.json`, `profiles.json`, `goals.json` (case-sensitive; `Volume-Data.json` is a shared
  library file).
* `is_library_path(p)`: starts with `/mokuro-reader/`, remainder non-empty and not a per-user file name.
* `is_inbox_path`, `is_admin_path` (`startswith("/_admin")`), `is_processor_path`
  (`== "/_processor"` or `startswith("/_processor/")`).
* Compiled-metadata predicates (`metadata/paths.py:62-112`): lexically normalise the FULL path
  (`posixpath.normpath("/" + p.strip("/"))`, collapsing `//`, `.`, `..` BEFORE the prefix test); must
  start with `/mokuro-reader/`; remainder non-empty and not a per-user file;
  `is_catalog_file_path` = remainder equals `catalog.json` (ASCII case-insensitive; **root only**);
  `series_title_from_series_file_path` = remainder is exactly `<one folder>/series.json` (basename
  case-insensitive, folder part non-blank, no deeper nesting); `is_compiled_metadata_path` = either.
  Pinned by `tests/unit/test_metadata_paths.py` (incl. `//catalog.json`, `///`, `./`, `..` alias spellings
  all collapse onto the real file).

### 4.3 Decision procedure (`authorize`, `auth.py:517-761`)

Evaluated in this exact order for `method` = request method, `path` = `re_encode_wsgi_path(PATH_INFO)`,
`role` = `auth_result.role`. Result = allow, or `(status, message)`.

1. `OPTIONS` -> allow (always, for preflight; even with a bad credential).
2. Credential error present and not authenticated (`error` set): **401** with that error text, or **429**
   if the text contains `Too many failed attempts`. (So a wrong password on ANY non-OPTIONS request is
   401 even where anonymous access would have been fine.)
3. `/_processor` paths: role has `PROCESS` -> allow; else 401 `Authentication required` (unauthenticated)
   / 403 `Processor access required`.
4. `/_admin` paths: `GET`/`HEAD` whose path has no `/api/` substring -> allow (static admin shell; the
   AdminAPI enforces `/api/*` itself too). Otherwise required permission `ADMIN`, or `MANAGE_INVITES`
   for `/_admin/api/invites` and `/_admin/api/invites/*`; missing -> 401 `Authentication required` /
   403 `Admin access required` (`Invite management access required` for invites).
5. Compiled-file write guard: methods `DELETE MOVE COPY PROPPATCH MKCOL LOCK UNLOCK` with a compiled
   metadata `path` -> `_compiled_metadata_denied`: unauthenticated 401 `Authentication required`, else
   403 `Permission denied: this file is compiled by the server` (for **every** role).
6. `MOVE`/`COPY`: if the `Destination` header's path (`unquote` then `urlparse(...).path`, `None` on any
   `ValueError` or empty; `auth.py:172-206`) is a compiled metadata path -> same denial as 5.
   Only the Destination **path** is examined here (not the Destination's class; see QUIRK 14.1).
7. `GET`/`HEAD`/`PROPFIND`: authenticated -> allow. Anonymous:
   * `PROPFIND`: 401 `Authentication required` iff `registration.allow_anonymous_browse` is false.
   * `GET`/`HEAD` of a library path: 401 iff `allow_anonymous_download` is false.
   * `GET`/`HEAD` of a non-library path: 401 iff `allow_anonymous_browse` is false AND the path is
     exactly `/` or `/mokuro-reader` (no trailing slash form).
   Both flags are **read live from the shared config object on every request** (the admin API mutates
   them; `auth.py:384-400`; legacy `require_login` is folded into both at load, `config.py:491-498`).
   Defaults: both true.
8. `PUT` -> `_authorize_put` (4.4).
9. `MKCOL`: library path -> needs `ADD_FILES` (401 unauthenticated / 403 `Permission denied: cannot create directories`);
   **any non-library path -> 403 `Permission denied: unsupported target path` for everyone including
   anonymous** (`/`, `/mokuro-reader`, `/inbox/...`, per-user names).
10. `DELETE`:
    * progress file -> `_authorize_progress_write`.
    * `role == "uploader"` and library path and `database.can_user_delete_library_path(username, path)`
      -> allow (see 4.6).
    * else needs `MODIFY_DELETE`: 401 unauthenticated / 403 `Permission denied: cannot modify or delete files`.
11. `MOVE`/`COPY`: progress file (source path) -> `_authorize_progress_write`; else `MODIFY_DELETE`
    (same 401/403 texts as DELETE).
12. `LOCK`/`UNLOCK`: progress file -> `_authorize_progress_write`; else `MODIFY_DELETE` (401 / 403
    `Permission denied`).
13. `PROPPATCH`: `MODIFY_DELETE` (401 / 403 `Permission denied`).
14. Any other method (`POST`, `TRACE`, ...): allow here (WsgiDAV then answers 405).

`_authorize_progress_write` (`auth.py:889-921`): unauthenticated -> 401 `Authentication required to save progress`;
role lacks `WRITE_PROGRESS` (anonymous/processor) -> 403 `Permission denied: cannot save progress`;
username present -> allow (the file is always the caller's own; the path maps to
`users/<username>/<name>`); else 403 `Cannot write to other users' progress`.

### 4.4 `_authorize_put` (`auth.py:789-868`)

In order:

1. progress file -> `_authorize_progress_write` (any authenticated role with `WRITE_PROGRESS`).
2. `<Series>/series.json` (`is_series_file_path`): unauthenticated -> 401; has `MODIFY_DELETE` -> allow; role
   `uploader` with `can_user_edit_series(username, series_title)` (owns **every** tracked volume of the
   series; folded comparison NFC + whitespace collapse + lowercase, `database.py:326-`) -> allow; else
   403 `Permission denied: cannot submit metadata updates for this series`. (`registered` never.) The
   request is then answered by `MetadataAPI` (metadata spec), not WebDAV.
3. other compiled path (root `catalog.json`) -> `_compiled_metadata_denied`.
4. library path: needs `ADD_FILES` (401 / 403 `Permission denied: cannot add files`); AND if the role lacks
   `MODIFY_DELETE` and the PUT **replaces an existing physical file the caller does not own**
   (`_replaces_unowned`: `virtual_to_physical(path, username)` exists AND NOT
   `can_user_delete_library_path(username, path)`) -> 403 `Permission denied: cannot replace a file another account uploaded`.
   Untracked legacy files belong to nobody, so uploaders cannot overwrite them. Pinned by
   `tests/unit/test_uploader_replace.py`.
5. everything else (`/`, `/mokuro-reader` itself, `/inbox/*`, any non-reader path): **403**
   `Permission denied: unsupported target path`, even for anonymous. `[VERIFIED]` (`/inbox` is never
   writable over WebDAV.)

### 4.5 Error response format (`_error_response`, `auth.py:923-958`)

* Status line `"<code> <Unauthorized|Forbidden|Not Found|Method Not Allowed|Too Many Requests|Error>"`.
* `Content-Type: text/plain; charset=utf-8`, body = message UTF-8. **No `Content-Length`** is set here
  (cheroot frames it).
* 401 adds `WWW-Authenticate: Basic realm="mokuro-bunko", charset="UTF-8"`; if the failed credential was a
  Bearer token (`error == "Invalid or expired token"`): `WWW-Authenticate: Bearer realm="mokuro-bunko", error="invalid_token"`
  instead (so a browser page signing in by token does not get a Basic dialog).
* 403 and 429 carry no `WWW-Authenticate` (`tests/integration/test_auth.py` 401/403 headers).
* `on_processor_login_refused(user, ip)` is invoked (exceptions swallowed) when the refusal is 401/429, the
  path is a `/_processor` path and a Basic username was supplied (`auth.py:418-430`).

### 4.6 Ownership-dependent decisions (DB-backed, for the DB spec to implement)

`can_user_delete_library_path(username, "/mokuro-reader/<rel>")` (`database.py:1776-1805`): false unless
`<rel>` is non-empty and contains a `/` or a `.` (top-level bare names are never deletable by an
uploader); owner = `get_volume_owner(rel)` where the key is `rel` with the extension swapped to `.cbz`
(`.cbz`, `.mokuro`, `.mokuro.gz`, `.webp`, `.nocover` all map to the archive; other files -> `None`);
if none, an OCR layer file `<stem>.<layer>.mokuro[.gz]` (layer matches `^(?=[a-z0-9-]*[a-z])[a-z0-9-]{1,32}$`)
borrows the owner of `<stem>.cbz`; allowed iff `owner == username`. Byte-exact (no case/NFC folding).
Consequently an uploader can delete **files** they uploaded (and their sidecars/layers) but never a folder.

### 4.7 `gate_read(environ, start_response, path)` (`auth.py:441-468`)

For routes served outside `AuthMiddleware` that must decide exactly as a `GET` of a DAV file would
(catalog manifest, queue file): copies the request context with `REQUEST_METHOD=GET`, `PATH_INFO=path`,
runs `authenticate` + `authorize`; returns `None` if allowed, else the already-started 401/403/429
response from 4.5. Note it **does** consume limiter budget like a normal request.

Tests: `tests/integration/test_auth.py`, `tests/unit/test_permissions.py`,
`tests/unit/test_metadata_permissions.py`, `tests/unit/test_uploader_replace.py`.

---------------------------------------------------------------------------------------------

## 5. CORS (`middleware/cors.py`, installed iff `config.cors.enabled`)

Config (`config.py:138-175`): `enabled` (default true), `allowed_origins` (default
`https://reader.mokuro.app`, `http://localhost:5173`, `http://localhost:*`, `http://127.0.0.1:*`),
`allow_credentials` (default true).

* **Origin match** (`CorsConfig.is_origin_allowed`): pattern without `*` -> exact string equality
  (case-sensitive, no trailing-slash tolerance); pattern ending in `:*` -> origin must start with the
  pattern minus `:*`, followed immediately by `:` and then **only digits** (no port -> no match);
  a `*` anywhere else never matches. Disabled config matches nothing.
* **Non-preflight** (`CorsMiddleware.__call__`, `cors.py:214-241`): for every response, if `Origin` present
  and allowed append `Access-Control-Allow-Origin: <origin echoed>`, `Access-Control-Allow-Credentials: true`
  (if enabled), `Vary: Origin`, `Access-Control-Expose-Headers: Content-Length, Content-Type, DAV, ETag,
  Last-Modified, Location, Lock-Token, WWW-Authenticate, X-Mokuro-Manifest, X-Mokuro-Recheck-After,
  X-Mokuro-Upload, X-Mokuro-Size, X-Mokuro-Put, X-Mokuro-Digest-Verified`. Disallowed/absent origin: no
  CORS headers, request still served.
* **Preflight** = `OPTIONS` **with an `Origin` header**, on ANY path, never reaches the app
  (`cors.py:243-284`). Always `204 No Content`, body empty. Allowed origin ->
  `Access-Control-Allow-Origin`, `-Allow-Credentials`, `Vary: Origin`,
  `Access-Control-Allow-Methods: GET, HEAD, POST, PUT, DELETE, OPTIONS, PROPFIND, PROPPATCH, MKCOL, COPY, MOVE, LOCK, UNLOCK`,
  `Access-Control-Allow-Headers: Authorization, Content-Digest, Content-Type, Content-Length, Depth, Destination, If,
  If-Match, If-None-Match, If-Modified-Since, If-Unmodified-Since, Lock-Token, Overwrite, Range, Timeout,
  X-Requested-With`, `Access-Control-Max-Age: 3600`, and `Access-Control-Allow-Private-Network: true` iff the
  request had `Access-Control-Request-Private-Network: true` (case-insens). `Access-Control-Request-Method` /
  `-Headers` are not inspected. Disallowed origin: still `204` with no CORS headers.
* DAV paths (`is_dav_path`: `""`, `/`, `/mokuro-reader`, `/inbox`, `/mokuro-reader/*`, `/inbox/*`,
  `cors.py:80-86`) additionally get `X-Mokuro-Put: verified` on the preflight (even for a disallowed origin),
  and, when the origin is allowed, the `Access-Control-Expose-Headers` list above.
  `[VERIFIED]` and pinned by `tests/unit/test_upload_verdicts.py` (`TestPutCapability`).
* `OPTIONS` **without** `Origin` is a normal request: passes through to the app.
* `compile_origin_pattern` (`cors.py:89-98`) is dead code. The nginx front adds its own CORS only for
  nginx-generated errors and for the X-Accel location (`deploy/nginx-internal.conf.template`); see 8.10.

Tests: `tests/integration/test_cors.py`, `tests/unit/test_cors.py`.

---------------------------------------------------------------------------------------------

## 6. Security / caching headers (`middleware/security_headers.py`)

Applied to **every** response by wrapping `start_response`; a header the inner layers already set
(case-insensitive name match) is left alone.

Always appended if absent (`:27-36`):
`X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, `Referrer-Policy: no-referrer`,
`X-XSS-Protection: 1; mode=block`, `X-Robots-Tag: noindex, nofollow`. No CSP, no HSTS.

`Cache-Control` additions, only when the response has no `Cache-Control`:

1. `Content-Type` (media type before `;`, lowercased) `== application/json` -> `no-store`
   (covers `series.json`, `catalog.json` GETs, API JSON, upload verdicts).
2. media type starts with `image/` -> `private, max-age=86400` (covers `.webp/.jpg/.png/.gif` DAV files and the
   catalog covers).
3. request method `GET`/`HEAD` AND `PATH_INFO` (raw, lowercased) ends with `.mokuro`, `.mokuro.gz`, or `.cbz`
   -> `no-cache` (forces revalidation; the DAV ETag makes it a 304). Evaluated on the request path
   regardless of status (so a 404 for a `.cbz` also gets `no-cache`, and so does a 304).
   Rationale: files change in place under a Last-Modified browsers would treat as heuristically fresh.

`.mokuro`/`.mokuro.gz`/`.cbz` are thereby served `no-cache` + `ETag`; image files cacheable 1 day;
`series.json`/`catalog.json` never cached.

Tests: `tests/integration/test_security_headers.py`, `tests/unit/test_security_headers.py`.

### 6.1 Validator / caching matrix and Content-Length framing (summary, all `[VERIFIED]`)

| Response | `ETag` | `Last-Modified` | `Cache-Control` (final, after section 6) | `Content-Length` |
|---|---|---|---|---|
| DAV GET/HEAD `*.cbz`, `*.mokuro`, `*.mokuro.gz` | DAV tag `"<mtime:.6f>-<size>"` | RFC 1123 | `no-cache` | exact (range length for 206) |
| DAV GET/HEAD image (`.webp .jpg .jpeg .png .gif`) | DAV tag | yes | `private, max-age=86400` | exact |
| DAV GET/HEAD `*.json` (series/catalog/progress files) | DAV tag | yes | `no-store` | exact |
| DAV GET/HEAD any other file (`.nocover`, `.txt`, `.gz`, ...) | DAV tag | yes | none | exact |
| DAV GET/HEAD offloaded to nginx (8.10) | nginx's own | nginx's own | as above (nginx keeps upstream `Cache-Control`) | python sends `0`, nginx the real size |
| DAV `304` (conditional) | none | none | `no-cache` for the `.cbz/.mokuro` path suffixes, else none | `0` |
| DAV `PUT` 201/204 | DAV tag (**new** file's; Python gives the stale one on overwrite, FIX) | none | none | HTML length / `0` |
| `PROPFIND` 207 (live) | none | none | none | exact (XML bytes) |
| `PROPFIND` Depth:infinity cache hit | none | none | none | exact, after gzip when `Content-Encoding: gzip` |
| Auth refusal (`401/403/429`), `text/plain` | none | none | none | **absent** in Python (transfer framing by cheroot); Rust should set it |
| Upload failure JSON | none | none | `no-store` | exact |
| Queue file | sha-256 tag (`-gz` variant) | none | `no-cache` | exact (200 only) |
| `/_static/*` | none | none | `public, max-age=3600` | exact |
| UI pages (`/login/...` etc.) | none | none | `no-cache` | exact |
| `/robots.txt` | none | none | none | `26` |

WsgiDAV insists every body-bearing DAV response has a `Content-Length` (otherwise it adds `Connection: close`); keep exact
lengths on all DAV responses. Only `Date`, `Server` and (for bodies without a length) chunking are added by cheroot.

---------------------------------------------------------------------------------------------

## 7. Static files (`static/__init__.py`)

### 7.1 `GET /_static/<filename>` (`:53-56,74-130`)

Served from the package dir `static/` (files: `nav.js` 4.7 KB, `shared.css` 18.9 KB; the page bundles
reference them). Only `GET` (HEAD falls through to the app -> 404). Rules:

* empty filename -> falls through (`/_static/` -> app -> 404).
* `..` anywhere in the name or a leading `/` -> `404 Not Found` (`text/plain`, body `Not Found`).
* resolved path escaping the dir -> `403 Forbidden` (`Forbidden`); not an existing regular file -> `404`;
  read error -> `500 Error reading file`.
* `Content-Type` by extension: `.html text/html; charset=utf-8`, `.js application/javascript; charset=utf-8`,
  `.css text/css; charset=utf-8`, `.json application/json`, `.png image/png`, `.jpg/.jpeg image/jpeg`,
  `.webp image/webp`, `.ico image/x-icon`, `.svg image/svg+xml`, `.woff2 font/woff2`, `.woff font/woff`,
  else `mimetypes.guess_type`, else `application/octet-stream`.
* Headers: `Content-Type`, `Content-Length`, **`Cache-Control: public, max-age=3600`**; no ETag/Last-Modified
  (no conditional GET). Whole file read into memory.

### 7.2 `GET /robots.txt` (`:58-70`)

`200`, `text/plain; charset=utf-8`, body exactly `User-agent: *\nDisallow: /\n`, `Content-Length: 26`.
Outside auth; covered by CORS/security headers. Pinned by `tests/unit/test_static.py`.

### 7.3 Web UI mounts (each UI middleware serves its own `web/` dir)

Handled by the page owners (see `web-frontend-contract.md` for contents); the mounting rules that belong
to this layer:

| Mount | Dir (`src/mokuro_bunko/...`) | Index | Unknown file | `Cache-Control` | Cite |
|---|---|---|---|---|---|
| `/_home/<f>` | `home/web` (index.html, home.js, styles.css) | `GET /` for browsers | 404 JSON `{"error":"File not found"}` | `no-cache` | `home/api.py:127-130,280-345` |
| `/login/`, `/login/<f>` | `login/web` | `index.html` | 404 | `no-cache` | `login/api.py:87-93,400-430` |
| `/account/`, `/account/<f>` | `account/web` | `index.html` | 404 | `no-cache` | `account/api.py:101-106,245-270` |
| `/setup/`, `/setup/<f>` | `setup/web` | `index.html` | 404 | `no-cache` (also 403 when needs setup and non-local) | `setup/api.py:92-107,185-230` |
| `/register/<f>`, `/register/` | `registration/web` | `register.html` | 404 JSON | none | `registration/api.py:97-110,356-401` |
| `/catalog/`, `/catalog/<f>` | `catalog/web` | `index.html`; unknown file -> **index.html** (SPA fallback) | | `no-cache`; covers `public, max-age=3600` | `catalog/api.py:125-133,520-560` |
| `/queue/`, `/queue/<f>` | `queue/web` | `index.html` | 404 | `no-cache` | `queue/api.py:156-170,725-770` |
| `/_admin/`, `/_admin/<f>` | `admin/web` (admin.js = 203 KB) | `index.html`; unknown -> **index.html** | | `no-cache` | `admin/api.py:595-632` |

All are plain file reads into memory (no ETag); for Rust embed the assets at build time (rust-embed)
and serve with the same content types. Content-type tables differ slightly per module (some use
`MIME_TYPES`, registration uses `mimetypes.guess_type` only); using one table is acceptable.

---------------------------------------------------------------------------------------------

## 8. WebDAV semantics (provider + resources + inherited WsgiDAV)

`MokuroDAVProvider` (`webdav/provider.py`) exposes ONE share (`/`) over WsgiDAV. WsgiDAV supplies the
method handlers (`wsgidav:request_server.py`); bunko supplies the resources. The Rust server must
reproduce the combined behaviour documented here. DAV compliance class advertised: `DAV: 1,2`
(provider is writable and a lock manager is present). `provider.is_readonly()` is `False`
(`provider.py:121-123`).

### 8.1 Virtual tree and physical mapping

Physical layout: `<base>/library/` (shared), `<base>/inbox/` (never exposed), `<base>/users/<username>/`
(per-user progress). `PathMapper` (`webdav/resources.py:325-521`).

| Virtual path | Resource | Physical |
|---|---|---|
| `/` | virtual collection, displayname `mokuro-bunko`, only member `mokuro-reader` | none |
| `/mokuro-reader` | virtual **merged** collection | none |
| `/mokuro-reader/volume-data.json`, `.../profiles.json`, `.../goals.json` | file; **only visible to an authenticated user**; maps to that user's own file | `users/<username>/<name>` |
| `/mokuro-reader/<anything else>` | shared library file/collection | `library/<rel>` via `safe_resolve_under` |
| `/inbox`, `/inbox/*` | **nothing**: `get_resource_inst` returns `None` => GET/PROPFIND/DELETE 404 (PathMapper maps it but the provider never serves it) | |
| every other path | `None` => 404 | |

`MokuroDAVProvider.get_resource_inst` (`provider.py:44-119`): normalises the path with
`"/" + path.strip("/")`; username = `environ["mokuro.user"]["username"]` or none. Per-user name
(exact, case-sensitive match of the three names under the reader root): anonymous -> `None`;
user dir containment failure -> `None`; **file exists** -> file resource, else `None` (so a GET before
the first save is 404 and the file is created by PUT through the parent's `create_empty_resource`).
Library path: `virtual_to_physical` -> `None` if it escapes the library (traversal and
symlink-out), `is_dir()` -> folder resource, `exists()` -> file resource, else `None`.

Per-user isolation: two users PUT/GET the same URL and get different files. `users/<username>` is
created lazily by the first PUT of a progress file (`create_empty_resource`, `resources.py:1313-1317`).
Usernames are `^[a-zA-Z0-9_-]{3,32}$` (`validation.py`) so the username path component cannot traverse;
`get_user_file_path` additionally checks `users/<username>` resolves under `users/`.

A per-user filename physically present in `library/` (e.g. a pre-per-user `goals.json`) is **not listed**
under `/mokuro-reader` and is unreachable: those names always map to the user's own copy
(`resources.py:1163-1177`, regression test `test_propfind_root_survives_a_stray_per_user_file_in_the_shared_folder`).

The virtual file `/mokuro-reader/.mokuro-queue.json` exists nowhere on disk and is served
by `QueueFileMiddleware` (section 11); it is **not** listed by PROPFIND. A real library file of that name would
still be listed by PROPFIND but its GET/HEAD are shadowed by the virtual document and every write to it is 405.

The compiled metadata files `library/catalog.json` and `library/<Series>/series.json` are ordinary
physical files to this layer (written by the metadata subsystem); their write protection is in the
auth gate (4.3 step 5/6, 4.4 steps 2-3).

### 8.2 Resource model and properties

**Listing.** `get_member_names`:

* `/` -> `["mokuro-reader"]`.
* `/mokuro-reader` -> the per-user names that **exist** for the caller (iterated in sorted order:
  `goals.json`, `profiles.json`, `volume-data.json`), then `sorted()` names of `library/`'s entries
  (`os.scandir`; per-user names excluded).
* physical folder -> `os.scandir` order (unsorted, filesystem order). Nothing is filtered: hidden files,
  `thumbnails/`, `.nocover`, `.mokuro-write-test` and in-flight upload staging files
  (`.<name>.upload-*.tmp`, section 8.9) are all listed. (Rust: sorting is fine; hiding staging files
  would be an improvement, Q3.)
* `OSError` while listing -> empty list.
Child kind comes from the DirEntry (`is_dir(follow_symlinks=True)`), with `stat` pre-populated
(`follow_symlinks=True`). Listing therefore does **not** apply `safe_resolve_under`: a symlink pointing
out of the library is listed with the target's stat but its URL is 404 (3.3).

**Live properties.** Both resource classes override `get_property_names` with a **static** list (no
getter probing, no lock properties, no dead properties):

| Property | File | Folder | Value / format |
|---|---|---|---|
| `{DAV:}resourcetype` | yes | yes | folder: `<D:resourcetype><D:collection/></D:resourcetype>`; file: empty element `<D:resourcetype></D:resourcetype>` |
| `{DAV:}creationdate` | yes | yes | `st_ctime` (inode-change time on Linux) formatted `%Y-%m-%dT%H:%M:%SZ` UTC; virtual folders: *now* |
| `{DAV:}getcontentlength` | yes | no | decimal `st_size` |
| `{DAV:}getcontenttype` | yes | no | by extension, below |
| `{DAV:}getlastmodified` | yes | yes | RFC 1123 GMT (`Thu, 01 Oct 2026 18:10:08 GMT`) of `st_mtime`; virtual folders: *now* |
| `{DAV:}displayname` | yes | yes | file: file name; folder: `/` -> `mokuro-bunko`; virtual `/mokuro-reader` -> `mokuro-reader`; physical -> directory name; plain text (XML-escaped), not percent-encoded |
| `{DAV:}getetag` | yes | yes | file: `"<mtime:.6f>-<size>"` WITHOUT quotes in the XML text (e.g. `1790878208.881310-1`); folder: `<mtime:.6f>`; **virtual folders have none**: the property is reported in a `404 Not Found` propstat as an empty element `<D:getetag/>` (`[VERIFIED]`) |

`allprop` (default when no body) and an explicit `<prop>` listing both draw from these; the lock
properties `{DAV:}supportedlock` and `{DAV:}lockdiscovery` are **only returned when explicitly named**
(they are not in `allprop`), and any dead property (8.3.8) only when named.

File `Content-Type` (`resources.py:676-697`), by lowercased extension, after two compound checks
(`*.json.gz` -> `application/gzip`, `*.mokuro.gz` -> `application/gzip`):
`.cbz application/vnd.comicbook+zip`, `.cbr application/vnd.comicbook-rar`, `.zip application/zip`,
`.gz application/gzip`, `.json application/json`, `.jpg/.jpeg image/jpeg`, `.png image/png`,
`.gif image/gif`, `.webp image/webp`, **everything else (incl. `.mokuro`, `.nocover`, `.txt`, `.avif`)
`application/octet-stream`**. Collections have no content type.

`ETag` header form: the property value wrapped in double quotes: `ETag: "1790878208.881310-1"`.
Strong tag; comparison (`If-Match`/`If-None-Match`) strips `W/` and quotes (weak tags are accepted as equal;
`*` matches). `ETag`/`Last-Modified` are derived from one cached `stat` per resource instance.
Python formatting detail: `f"{st_mtime:.6f}-{st_size}"` (float seconds, 6 decimals).

**Hrefs** (`wsgidav:dav_provider.py:406-423`): UTF-8 percent-encoding of the path with safe set
`/ ! * ' ( ) , $ - _ | .`; collections end with `/`; the root is `/`. Responses are path-absolute.

### 8.3 Method behaviours

The generic WsgiDAV pipeline: method dispatch (`wsgidav:request_server.py:70-143`) -> `Depth` lowercased,
`Overwrite` uppercased -> handler. Methods dispatched: `OPTIONS HEAD GET PROPFIND PUT DELETE COPY MOVE MKCOL
PROPPATCH POST LOCK UNLOCK`; anything else (and `POST`) -> `405 Method Not Allowed` (HTML body, **no
`Allow` header**). Auth (section 4) runs before; it already rejected most unauthorised cases, so the
statuses below are those of an authorised caller.

#### 8.3.1 OPTIONS (`request_server.py:1373-1458`)

* `OPTIONS *` (path exactly `*`) -> `200`, `Content-Type: text/html; charset=utf-8`, `Content-Length: 0`,
  `DAV: 1,2`, `Date`, `MS-Author-Via: DAV`.
* Otherwise resolve the resource. Headers always: `Content-Type: text/html; charset=utf-8`,
  `Content-Length: 0`, `DAV: 1,2`, `Date`, `Allow`, `MS-Author-Via: DAV` (and, from
  `UploadMiddleware`, `X-Mokuro-Put: verified` for DAV paths, section 9).
  * existing collection (incl. `/`, `/mokuro-reader`): `Allow: OPTIONS, HEAD, GET, PROPFIND, DELETE, COPY,
    MOVE, PROPPATCH, LOCK, UNLOCK` (note: no PUT/MKCOL listed even though MKCOL under it is permitted).
  * existing file: `Allow: OPTIONS, HEAD, GET, PROPFIND, PUT, DELETE, COPY, MOVE, PROPPATCH, LOCK, UNLOCK` and
    `Accept-Ranges: bytes` unless `support_ranges()` is false (nginx offload).
  * non-existent path whose **parent resolves to an existing collection**: `Allow: OPTIONS, PUT, MKCOL`
    (so `OPTIONS /inbox` and `OPTIONS /mokuro-reader/S/nope.cbz` are `200`).
  * otherwise `404` (e.g. `OPTIONS /foo/bar`).
* A cross-origin `OPTIONS` carrying `Origin` never gets here (preflight, section 5). Auth always allows
  OPTIONS (even with bad credentials).

#### 8.3.2 PROPFIND (`request_server.py:275-371`)

1. Resource lookup: none -> `404`. (Anonymous callers and callers without a saved file get 404 for
   per-user names.)
2. `Depth`: missing -> `infinity` (RFC default; **no limit**, `allow_propfind_infinite=True`); lowercased
   value not in `{0,1,infinity}` -> `400`.
3. If-headers evaluated against the resource (`If-Match` etc., 8.4).
4. Body: empty -> allprop. Parsed XML (`defusedxml`); unparsable or root element not `{DAV:}propfind` -> `400`.
   Children: `allprop`, `name`, `prop` (a `<propname/>` element, the RFC spelling, is **not recognised**:
   it falls to "named with an empty list" and yields responses with just an `<href>` and no propstat
   (`[VERIFIED]`, QUIRK; Rust should implement RFC `propname`: names only, 200 propstat). `allprop`+`name`
   together -> `400`; `prop` after `allprop`/`name` -> `400`. `<include>` ignored.
5. Resource list (`get_descendants(depth, add_self=True)`): self, then for `1`: members; for `infinity`:
   **pre-order depth-first** (each child immediately followed by its subtree), in member order.
6. Per resource: `<D:response><D:href>` then one `<D:propstat>` per distinct status in this order: `200 OK`
   group first (properties in request/static order), then each error status in first-occurrence order
   (`404 Not Found` for unknown named properties and virtual-folder `getetag`). Each carries
   `<D:prop>` and `<D:status>HTTP/1.1 <code> <text></D:status>`. Error properties are empty elements.
   Dead properties in other namespaces declare their namespace on the `<D:response>`.
7. Response: `207 Multi-Status`, `Content-Type: application/xml; charset=utf-8`, `Date`, `Content-Length`.
   XML text: declaration `<?xml version='1.0' encoding='UTF-8'?>` + newline, root `<D:multistatus xmlns:D="DAV:">`,
   no pretty printing (the stdlib serialiser with prefix `D`; lxml is not installed; clients are
   prefix-agnostic).

Example (file, Depth 0) `[VERIFIED]`:
```
<?xml version='1.0' encoding='UTF-8'?>
<D:multistatus xmlns:D="DAV:"><D:response><D:href>/mokuro-reader/S/v1.cbz</D:href><D:propstat><D:prop><D:resourcetype></D:resourcetype><D:creationdate>2026-10-01T18:10:08Z</D:creationdate><D:getcontentlength>1</D:getcontentlength><D:getcontenttype>application/vnd.comicbook+zip</D:getcontenttype><D:getlastmodified>Thu, 01 Oct 2026 18:10:08 GMT</D:getlastmodified><D:displayname>v1.cbz</D:displayname><D:getetag>1790878208.881310-1</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>
```
`Depth: infinity` requests are intercepted by the PROPFIND cache (section 10) which **always answers allprop**
regardless of the request body. `Depth: 0/1` always hit the live filesystem.

#### 8.3.3 GET / HEAD (`request_server.py:1466-1617`)

1. Resource lookup: none -> `404`; request has a body (`Content-Length != 0`) -> `415`; `Depth` header present
   and != `0` -> `400`; collection -> **`403 Forbidden`** ("Directory browsing is not enabled", HTML), including `/`.
2. If-headers (8.4): may yield `304`/`412`.
3. `Content-Length` = file size; if size is 0 ranges are ignored. Ranges honoured only if
   `support_ranges()` (true unless nginx offload, 8.10) and `Range` present; `If-Range` (HTTP-date: valid only if
   `int(last_modified)` equals it; else entity-tag: stripped of `"`/space, must equal the current ETag
   value) failing -> ignore the range and send the full 200.
4. Range parsing (`wsgidav:util.py:1458-1529`): regex search per comma-separated spec (the `bytes=` unit is
   not enforced: `items=0-1` is accepted); `start >= size` -> `416` with `Content-Range: bytes */<size>`;
   `start-` => to EOF; `start-end` clamps `end` to size-1; `-N` suffix = last N bytes (clamped at 0);
   overlapping/adjacent ranges are merged; **only one range is ever served and it is the merged range with the
   highest start** (`bytes=0-0,5-9` -> `5-9`; `[VERIFIED]`); no spec parses -> `416` (no `Content-Range`).
   A satisfiable range -> `206 Partial Content` with `Content-Range: bytes <s>-<e>/<size>`, `Content-Length` = range length.
5. Response headers in this order: `Content-Length`, `Last-Modified` (RFC 1123), `Content-Type`, `Date`, `ETag`
   (quoted), `Accept-Ranges: bytes` (if ranges supported), [`Content-Range`]; then `finalize_headers` (nginx hook),
   then outer layers (CORS, security headers incl. section 6 `Cache-Control`).
6. `HEAD`: same headers, empty body. `GET`: body streamed in 8192-byte blocks.
7. A `304` answered by the conditional check carries only `Content-Length: 0` and `Date` (no ETag, no
   validators); the security layer may still add `Cache-Control: no-cache` for `.cbz/.mokuro` paths.

Test pins: `tests/integration/test_webdav_ops.py` (`TestGet`, `TestNginxAccelContentLength`).

#### 8.3.4 PUT (`request_server.py:682-774` + `resources.py:802-850`)

Order of checks and effects:

1. `Content-Encoding` header present -> `501`. `Content-Range` present -> `400`.
2. Target is a collection -> `405`. Parent does not resolve to an existing collection -> `409`
   (`PUT /mokuro-reader/NewSeries/x.cbz` without MKCOL first = 409; `PUT /inbox/x` = 409 at this layer but
   auth already answered 403; `PUT /mokuro-reader/../x` = 409).
3. If-headers vs the existing resource (`If-Match`, `If-None-Match: *` => `412` when it exists, ...).
4. New file: DAV-lock check on the parent, then `parent.create_empty_resource(name)`:
   * reader root parent + per-user name: needs a user (else error), creates `users/<u>/` (mkdir -p);
   * reader root parent + other name: `safe_resolve_under(library, name)`, `None` -> `403`; `library/` dir
     exists, so a **loose file directly under the library root is allowed** (`/mokuro-reader/x.cbz`);
   * physical folder parent: `safe_resolve_under(folder, name)`, `None` -> `403`. (No directories are created.)
   Existing file: DAV-lock check on it.
5. `begin_write` (below): path write-lock; on conflict `423 Locked` ("Resource is locked by another write
   operation") + audit `lock_conflict` (`operation: "write"`).
6. Body streamed (8192-byte reads) into the writer; `close()` runs the finalize/verify/commit sequence (8.9). Any
   exception: `end_write(with_errors=True)` aborts the writer (temp file discarded, path lock released) and the
   request fails with the exception's status (non-DAV exceptions -> `500`; `DAVError` keeps its code, e.g. `422`,
   `507`, `423`).
7. Success: `ETag: "<etag>"` header; `201 Created` (HTML body, `Content-Type: text/html; charset=utf-8`, e.g.
   `<title>201 Created</title>`) if the target did not exist, else `204 No Content` (`Content-Length: 0`).
   **BUG (FIX):** for an overwrite the ETag header is the *old* file's (the resource instance cached its
   `stat` during the If-header check before the write). A new file returns the correct tag (`[VERIFIED]`:
   replace of a 5-byte file by 11 bytes answered `"…889340-5"`, PROPFIND then showed `…889673-11`). Rust must
   return the new tag.
8. `UploadMiddleware` then adds `X-Mokuro-Upload`/`X-Mokuro-Size`/`X-Mokuro-Digest-Verified`/`X-Mokuro-Put`/
   `X-Mokuro-Manifest`/`X-Mokuro-Recheck-After` or rewrites a failure into JSON (section 9).

`begin_write` (`resources.py:802-842`): acquire `_PATH_WRITE_LOCKS` on the destination (non-blocking; section 8.7);
`existed_before = file_path.exists()`; `mkdir -p` the parent; fresh `UploadOutcome` stored in
`environ["mokuro.upload"]`; writer = `_ValidatedCbzWriter` iff the name's lowercase suffix is `.cbz`
(else `_AtomicFileWriter`), with `expected_size` (from `Content-Length`; `None` if absent/blank/non-numeric/negative)
and `expected_digest` (from `Content-Digest`, 8.9); wrapped as `_AuditedWriter(on_commit)` then
`_LockedWriter(release lock)`. The lock is released on `close()` (also when it raised) and on
`end_write(with_errors=True)`.

On a successful commit `_on_write_committed(existed_before)` (`resources.py:643-654`):

1. If the file is a `.cbz` under the library: `environ["mokuro.archive_written"] = path` (UploadMiddleware queues OCR
   after success; OCR-QUEUE).
2. If a DB, a library-relative path and an actor exist: `db.record_volume_upload(rel, actor, existed_before)`
   (`database.py:1704-1760`). Only a `.cbz` PUT can create a row: `INSERT (volume_key, uploader, last_modified_by,
   last_modified_at) ... ON CONFLICT(volume_key) DO UPDATE SET last_modified_by, last_modified_at` (`existed_before`
   does not change the SQL: both branches are identical). So a PUT over an **untracked** archive by a role that may
   replace it (`MODIFY_DELETE`) makes the writer its recorded uploader/owner; over a tracked one only the modifier
   fields move. A sidecar path (`.mokuro`, `.webp`, ...) only updates `last_modified_*` of an existing row, never
   creates one (so sidecar edits cannot capture a volume).
3. `_forget_ocr_records(db, rel, archive_too=False)`: a PUT of `*.mokuro`/`*.mokuro.gz` deletes that sidecar's
   provenance row (`forget_ocr_sidecar`); a `.cbz` PUT keeps its sidecar rows. (OCR-provenance, OCR-QUEUE.)
4. Audit event `edit` (existed) or `upload` (new): `target_type` `library` (`target_path`
   `/mokuro-reader/<rel>`) or, for per-user files, `progress` with the request path, `details {"existed_before": bool}`,
   actor = username. **This includes every progress-file save** (one audit row per PUT of
   `volume-data.json`!). Audit rows are pruned after 30 days (`database.py:1440+`).
If step 2-4 raise (e.g. DB locked) after the file was already replaced, the client gets `500` although the file
was stored (QUIRK).

Test pins: `tests/unit/test_atomic_writes_locks.py`, `tests/unit/test_upload_verdicts.py`,
`tests/integration/test_webdav_ops.py` (`TestPut`), `tests/unit/test_uploader_replace.py`.

#### 8.3.5 DELETE (`request_server.py:548-670`, `resources.py:852-890, 1442-1462`)

1. Lookup: none -> `404`. Request has a body -> `415`. `Depth`: collections accept only absent/`infinity`; files
   `0`/`infinity`; else `400`. If-headers vs resource. DAV-lock check on the parent.
2. **File** (`MokuroFileResource.delete`): missing file -> returns silently (still `204`). Else acquire the path lock
   (`423` + audit `lock_conflict`/`delete`). Then:
   * `_remember_primary_uuid` (OCR: if it is a bare `<Stem>.mokuro[.gz]` with `<Stem>.cbz` beside it, store its
     `volume_uuid` so a re-OCR keeps the volume's identity) **[OCR-QUEUE/DROP candidate]**;
   * if the name's lowercase ends with `.cbz`: delete **every sibling of the archive**, listed from the directory
     (`ocr/generations.py:1042-1069`): `<stem>.mokuro`, `<stem>.mokuro.gz`, `<stem>.webp`, `<stem>.nocover` (fixed
     names) plus every `<stem>.<layer>.mokuro[.gz]` where `<layer>` matches `^[a-z0-9-]{1,32}$` and
     `<stem>.<layer>` is **not** itself the stem of another archive in the directory (volume `Vol 01.5.cbz` owns
     `Vol 01.5.mokuro`, which must survive deleting `Vol 01.cbz`); split on the LAST dot. Unlink errors ignored.
   * `os.remove(file)`; `environ["mokuro.archives_removed"] += [path]` if it is a `.cbz` in the library
     (OCR cancel after success).
   * DB (`.cbz` + library-relative path): `forget_volume_upload(rel)`, `forget_volume_uuid(rel)`;
     `_forget_ocr_records(db, rel)` (archive => all sidecar rows of the volume; sidecar => its own row).
   * audit `delete` (`library` or `progress`).
   Deleting a non-`.cbz` file removes only that file (e.g. deleting `Vol.mokuro` is how a re-OCR is requested).
3. **Collection** (`MokuroFolderResource.delete`): `support_recursive_delete()` is true, so WsgiDAV first checks every
   descendant for DAV locks; with none, `resource.delete()`: if `folder_path` exists: path lock on the folder
   (`423` on any overlapping active write), DB: `forget_volume_uploads_under_prefix`, `forget_ocr_sidecars_under_prefix`,
   `forget_volume_uuids_under_prefix`; `shutil.rmtree`; `archives_removed += [folder]`; audit `delete`
   (`target_type library_folder`). Virtual folders (`/`, `/mokuro-reader`) have no `folder_path`: **no-op, still `204`**.
4. Success: `204 No Content`, `Content-Length: 0`. A failure on the root -> that status; several errors -> `207`
   multistatus of `<response><href/><status/></response>`.
See QUIRK 14.3 for the lock-conflict "wipe" path on virtual folders.

#### 8.3.6 MKCOL (`request_server.py:488-539`, `resources.py:1340-1373`)

Body present -> `415`; `Depth` present and != `0` -> `400`; path already exists -> `405` ("MKCOL can only be executed
on an unmapped URL"); parent not an existing collection -> `409`; DAV-lock check on the parent. Creation:
parent = reader root: `safe_resolve_under(library, name)` (`None` -> `403`) `mkdir(parents=True, exist_ok=True)`;
physical folder parent: same under it; audit `mkdir` (`details {"path": member_path}`, `target_type`
`library_folder`; for the reader root parent `target_path` is the new folder's path). `201 Created` (HTML body).
Only library paths pass the auth gate (4.3 step 9), i.e. `/mokuro-reader/<name>` and deeper under existing
folders. MKCOL on a per-user name -> auth 403 (and would 500 in the provider).

#### 8.3.7 COPY / MOVE (`request_server.py:782-1125`, `resources.py:892-991, 1375-1440`)

Preconditions in order: source missing -> `404`; no `Destination` -> `400`; `Overwrite` (default `T`) not `T`/`F`
-> `400`; request body (RFC 2518 propertybehavior) read and ignored; collection `Depth` default `infinity`
(allowed `0`/`infinity`; `MOVE` of a collection requires `infinity`, else `400`); non-collection: depth forced `0`
(`0`/`infinity` accepted, else `400`).
Destination parsing: `unquote` the header, `urlparse`; for a collection source, `dest_path = dest_path.rstrip("/") + "/"`.
Scheme (if present) must equal `wsgi.url_scheme` or `X-Forwarded-Proto`; host (if present) must equal `Host` or
`X-Forwarded-Host` (lowercased; port included in the string compare) -> else `502 Bad Gateway`
(`[VERIFIED]`: `https://localhost:8080/...` against an http server = 502; this is why
`nginx-internal.conf.template` forwards the edge's `X-Forwarded-Proto`). The path must start with `mount_path +
share_path + "/"` = `/` (always true) else `502`.
Then: destination parent must be an existing collection else `409`; If-headers on source and destination; DAV-lock
checks (move: source subtree `infinity` + source parent; destination parent if new; existing destination `infinity`);
`src_path == dest_path` -> `403 "Cannot copy/move source onto itself"`; destination inside source -> `403`;
destination exists and `Overwrite: F` -> `412`.
Success code: `201` if the destination did not exist else `204`, **except** a natively-handled single-file
`MOVE` (below) which always answers `204`.

Bunko-specific handlers (authoritative semantics):

* **File MOVE** (`handle_move`): destination resolved with `PathMapper.virtual_to_physical`; source and destination
  must be the same class (`library`->`library` or `progress`->`progress`, else `handle_move` returns False: QUIRK
  14.1). Locks `[src, dst]` (sorted by casefolded resolved path; `423` + audit on conflict). Steps:
  `_remember_primary_uuid`; `mkdir -p dst.parent`; `os.replace(src, dst)` (atomic; overwrites); `archives_removed += [src]`
  if `.cbz`; `archive_written = dst` if `.cbz` in library; DB `rename_volume_upload(old, new)` (ownership follows
  the file; upsert); `_forget_ocr_records(old)` (archive => all sidecar rows of the old volume) and
  `_forget_ocr_records(new, archive_too=False)`; audit `move` with `details {"destination": <header path>}`.
  **Sidecars are NOT moved** (they stay under the old name; pinned by
  `test_move_library_file_as_admin_preserves_sidecars_and_owner`). `204`.
* **File COPY** (generic loop -> `copy_move_single(is_move=False)`): locks `[dst]`; `mkdir -p dst.parent`;
  `shutil.copy2` (preserves mtime/permission bits); `archive_written = dst` if `.cbz`; DB `_forget_ocr_records(new, archive_too=False)`;
  audit `copy`. **No `record_volume_upload` for the copy (the copy is untracked/unowned; QUIRK)**. Sidecars not
  copied. `201` (new) / `204` (overwrite; WsgiDAV `delete()`s the existing destination file first, which also
  runs the file `delete` side effects, i.e. its sidecars go if it is a `.cbz`).
* **Folder MOVE**: if the destination exists (`Overwrite: T`) WsgiDAV first `delete()`s it (rmtree + DB forgets,
  as DELETE). Then `move_recursive`: requires `folder_path` set and destination in the library
  (`support_recursive_move`); locks `[src, dst]` (`423` on conflict); records `.cbz` volume paths under the folder;
  `mkdir -p dst.parent`; **`os.replace(src_dir, dst_dir)`** (whole tree, sidecars and all); `archives_removed += [src]`;
  DB: for each recorded volume `rename_volume_upload(old, new)`, `rename_ocr_sidecars_under_prefix`,
  `rename_volume_uuids_under_prefix` (if the new prefix is outside the library: `forget_*_under_prefix` instead);
  audit `move`. Pinned by `test_move_series_folder_as_admin_preserves_sidecars_and_owner`.
* **Folder COPY** (generic): `Depth: infinity` copies the tree pre-order (folders `mkdir -p`, files as File COPY);
  when the destination exists and is a collection too, destination members absent from the source are deleted first
  (RFC 9.8.4 no-merge); `Depth: 0` creates only the destination folder. Folder `copy_move_single` creates the
  destination directory only if it is a library path (`dest_type == "library"`).
* The auth gate only requires `MODIFY_DELETE` (or progress write for per-user *source*) and checks the
  Destination only for compiled metadata paths (4.3 step 6).
* `_resolve_destination_path` (file) requires source and destination to be the same class; folder requires a library
  destination. Reader-root/inbox/other destinations are not handled natively.
See QUIRK 14.1-14.3: several type-mismatch and virtual-folder cases are destructive in Python.

#### 8.3.8 PROPPATCH (`request_server.py:373-486`)

Auth: `MODIFY_DELETE` (never for compiled metadata). `Depth` absent -> `0`, other -> `400`. Resource missing -> `404`.
If-headers + DAV-lock check. Body must be `<D:propertyupdate>` of `set`/`remove` -> `<D:prop>` -> property elements
(`400` otherwise). Two passes (dry run then real). Live `{DAV:}` properties and the lock properties -> `403`
(`cannot-modify-protected-property` for locks); non-`DAV:` properties are stored in WsgiDAV's **in-memory**
`PropertyManager` keyed by the resource URL (lost on restart, **not** moved/removed with the resource, not
returned by `allprop` because the static property list omits them; returned only when named). Response `207`.
Nothing in the repo or reader uses this. **Proposal: DROP** (or answer `207` with `403` for every property); Q5.

#### 8.3.9 LOCK / UNLOCK (`request_server.py:1127-1371`)

Auth: progress files by `WRITE_PROGRESS` holders, otherwise `MODIFY_DELETE`; never on compiled metadata.
WsgiDAV's in-memory `LockManager(LockStorageDict)`; nothing is persisted; **default timeout 604800 s (1 week)**,
header `Timeout: Second-N | Infinite` (values over ~10 years => infinite).

* LOCK on an **existing** resource: `Depth` default `infinity` (only `0`/`infinity` else `400`); body
  `<D:lockinfo>` with `lockscope` (`exclusive`|`shared`), `locktype` `write`, optional `owner`
  (unknown child -> `400`); conflict -> `423` (XML `<error><no-conflicting-lock><href>`). Success `200 OK`,
  headers `Content-Type: application; charset=utf-8` (WsgiDAV typo), `Content-Length`, `Lock-Token:
  opaquelocktoken:<64 hex>` (no angle brackets), `Date`; body `<D:prop><D:lockdiscovery><D:activelock>` with
  `locktype`, `lockscope`, `depth`, optional `owner`, `timeout` (`Second-N` remaining or `Infinite`), `locktoken/href`,
  `lockroot/href` (`[VERIFIED]`).
* Refresh: empty body + `If: (<token>)`, depth forced 0 -> `200` with the discovery body (no `Lock-Token`);
  zero or several tokens -> `400`; token not on the URL -> `412`.
* `UNLOCK`: needs `Lock-Token` (`400` if missing; `<`/`>` stripped), no body (`415`), resource must exist (`404`),
  token must lock that URL (`409`), token's principal must be the caller (the principal is `""` for every request,
  so **anyone** may unlock anyone's lock), `204`.
* While a lock exists, writes (PUT/DELETE/MOVE/COPY/MKCOL/PROPPATCH) on the URL (or a depth-infinity parent) without
  the token in an `If:` header answer `423` with XML body (`Content-Type: application/xml; charset=utf-8`,
  `<ns0:error xmlns:ns0="DAV:"><ns0:no-conflicting-lock><ns0:href>…`).
* **QUIRKS:** locks are keyed by URL, not by file: user A's lock on `/mokuro-reader/volume-data.json` blocks user B's
  save of *their own* file at the same URL (423). `LOCK` of a non-existent path creates nothing, then fails with
  `500` after registering the lock (dangling lock for up to a week; `[VERIFIED]`).
Nothing in the repo or tests drives LOCK except the "compiled files refuse LOCK" tests. Proposal (Q4): either DROP
(advertise `DAV: 1`, answer `405`) or implement an in-memory lock table with the semantics above minus the quirks.

### 8.4 Conditional headers (WsgiDAV `_evaluate_if_headers`, `util.evaluate_http_conditionals`)

Evaluated on the target resource when it exists (`PROPFIND`, `GET/HEAD`, `PUT`, `DELETE`, `COPY/MOVE` source and
destination, `UNLOCK`, `LOCK`, `PROPPATCH`):

* `If-Match`: tags compared after stripping `W/` and quotes; `*` matches; no match -> `412`.
* `If-None-Match`: a match (or `*`) -> `304` for GET/HEAD, `412` for everything else. (`[VERIFIED]`:
  `PUT` with `If-None-Match: *` on an existing file = 412.)
* `If-Modified-Since` (only if there is no `If-None-Match` mismatch): `304` iff the client's date is **strictly
  later** than `int(last_modified)`. **BUG (FIX):** an equal date answers 200 (RFC says 304). Rust: `>=`.
* `If-Unmodified-Since`: `412` iff the client's date is strictly earlier than `int(last_modified)`.
* The DAV `If:` header (tagged/untagged lists, lock tokens, `Not`, ETags) is evaluated by WsgiDAV when present;
  implement at least lock-token and ETag conditions if LOCK is kept.

### 8.5 Error and status response format

All errors raised inside WsgiDAV go through `ErrorPrinter`:

* Status text table: 200 OK, 201 Created, 204 No Content, 304 Not Modified, 400 Bad Request, 401 Unauthorized,
  403 Forbidden, 404 Not Found, 405 Method Not Allowed, 409 Conflict, 412 Precondition Failed, 415 Media Type
  Not Supported, 416 Range Not Satisfiable, 423 Locked, 424 Failed Dependency, 500 Internal Server Error,
  501 Not Implemented, 502 Bad Gateway; any other code prints `<code> Status` (so a bunko `DAVError(422)` or
  `507` is `422 Status` / `507 Status` until `UploadMiddleware` rewrites PUT answers with the proper phrase).
* `304`/`204` errors: headers `Content-Length: 0`, `Date`, body empty.
* Others: `Content-Type: text/html; charset=utf-8`, `Content-Length`, `Date`, body = an HTML 4.01 page containing
  `<title>{status}</title>`, `<h1>{status}</h1>`, `<p>{status}: {context}</p>` and a footer naming WsgiDAV + timestamp
  (`[VERIFIED]`). Clients only read the status; Rust may emit any small `text/html` body. Errors with a DAV condition
  (lock conflicts) use `application/xml; charset=utf-8` with the `<D:error>` body shown above.
* Success codes without data (`201`, and `MKCOL`/`PUT`/`COPY`/`MOVE` created) also carry an HTML body
  (`<title>201 Created</title>…`), `Content-Length` correct; `204` carries `Content-Length: 0`.
* Unhandled Python exception -> `500` with the HTML page; `OSError(EACCES)` -> `403`.
* Everything also gets `Date` (RFC 1123 GMT).

### 8.6 Audit and ownership side effects (summary table)

All DB calls are best-effort inside the request thread (a DB failure surfaces as `500`; see 8.3.4). `actor` =
`mokuro.user.username` else `mokuro.username`.

| Operation | `volume_uploads` | OCR provenance / identities | Audit `action` (target_type) |
|---|---|---|---|
| PUT `.cbz` new / replace | insert row (uploader = writer) if none; else update `last_modified_by/at` only | none | `upload` / `edit` (`library`) |
| PUT sidecar (`.mokuro/.webp/...`) | updates `last_modified_by/at` of an EXISTING row only | `.mokuro[.gz]` -> forget that sidecar row | `upload`/`edit` (`library`) |
| PUT progress file | none | none | `upload`/`edit` (`progress`) |
| DELETE `.cbz` | forget row | forget all sidecar rows + volume uuid | `delete` |
| DELETE other file | none | sidecar -> forget its row | `delete` |
| DELETE folder | forget rows under prefix | forget sidecars + uuids under prefix | `delete` (`library_folder`) |
| MOVE file | rename row | forget old (all sidecars of old volume), forget new sidecar row | `move` |
| MOVE folder | rename each row | rename sidecar + uuid prefixes | `move` |
| COPY file | none (untracked copy) | forget new sidecar row | `copy` |
| MKCOL | none | none | `mkdir` (`library_folder`) |
| any 423 | none | none | `lock_conflict` with `{"operation": ...}` |

`details` JSON is compact (`separators=(",",":")`, ASCII-escaped). Resources only audit when `environ["mokuro.db"]`
exists (never in the PROPFIND cache's synthetic requests).

### 8.7 Per-path write locks (`resources.py:52-98, 290-322`)

Process-local registry `_PATH_WRITE_LOCKS` of held paths, **non-blocking**:

* key = tuple of **casefolded** parts of `path.resolve()`.
* `acquire(path)`: fails (False) if ANY held key conflicts, i.e. is equal to, an **ancestor** of, or a **descendant** of
  the candidate; else records and returns True. (A folder move blocks writes inside it and vice versa; `Series/Vol.CBZ`
  conflicts with `series/vol.cbz`.) `blocking=True` raises `ValueError`.
* `_try_acquire_all(paths)`: sorts by casefolded resolved string, acquires all-or-none (rolls back).
* `path_write_lock(path)` context manager for non-DAV writers (the metadata compiler writes `series.json`/`catalog.json`
  under the same lock so it cannot interleave with an upload or folder move): raises `DAVError(423)` when busy.
* Held by: `begin_write` (from first byte until close/abort), file `delete`, folder `delete`, file `handle_move`
  (`[src, dst]`), file `copy_move_single` (`[dst]` copy / `[src,dst]` move), `move_recursive` (`[src, dst]`).
* Conflict => `DAVError(423, "Resource is locked by another write operation")` + audit `lock_conflict`; **reads are not
  blocked and see the old file until the atomic replace** (pinned by `TestConcurrentRead`).
Rust: a `Mutex<Vec<Vec<String>>>` (or tree) with the same prefix/casefold conflict rule; release on drop.

### 8.8 Writers (`resources.py:1476-1852`): atomic, verified, digest-checked

`_AtomicFileWriter` (all non-`.cbz` files, including per-user files):

* Staging file `<dest_dir>/.<dest_name>.upload-<random>.tmp` (`mkstemp`, mode 0600) in the **destination directory**
  (same filesystem => atomic rename). Bytes are written as they arrive and hashed as they stream if a digest was announced.
* `write(data)`: `OSError` -> `_fail_on_os_error`.
* `close()` (called once at end of body): `flush` + `fsync` (errors `EINVAL/ENOTSUP/EROFS` on fsync are ignored,
  a failing flush is an error) -> close handle -> **size check** -> **digest check** -> `_verify()` (subclass) ->
  `os.replace(temp, dest)` -> on POSIX `chmod(dest, 0o666 & ~umask)` -> forget damage memory for the path ->
  `outcome.verdict = VERDICT`, `outcome.size = bytes stored`.
* Failure (`_reject`): marks closed, deletes the temp file, fills `outcome{status, reason, detail, retry}` and raises
  `DAVError(status, detail)`. The destination is untouched (old file survives; pinned `test_a_failed_replace_keeps_the_old_file`).
  `abort()` (client disconnected / error elsewhere): discard temp, destination untouched, no outcome.
* Error mapping: `OSError` errno in `{ENOSPC, EDQUOT}` -> `507 disk-full` ("The server's disk is full; the upload was not stored.", retry false);
  any other `OSError` -> `500 server-error` ("The server failed <writing the upload|saving the upload|checking the upload|reading the upload back|moving the upload into place>: <strerror>.", retry true).
* Size check: `expected_size` (the `Content-Length`) set and `!= staged size` (shorter OR longer) -> `422 truncated`
  ("Received N of M bytes; the upload was cut short.", retry true). No `Content-Length` (chunked) -> no check.
* Digest check: if `Content-Digest` parsed, SHA-256/512 of the staged bytes != expected -> `422 corrupted-in-transit`
  ("The upload does not match its sha-256 Content-Digest: it was damaged on the way here. Sending it again should work.", retry true).
  A match sets `outcome.digest_verified = "sha-256"|"sha-512"`.
* `Content-Digest` parsing (`parse_content_digest`, RFC 9530 dictionary of byte sequences): header comma-split; each
  member must match `^([a-z*][a-z0-9_.*-]*)=:([A-Za-z0-9+/]*={0,2}):$` (no parameters) and strict-base64 decode; a known
  algorithm (`sha-256`->32 bytes, `sha-512`->64 bytes) with the wrong length makes the **whole header ignored**
  (as absent); any malformed member ignores the whole header; unknown algorithm members are skipped; of the known ones
  the first in the order `sha-256`, `sha-512` is used; a repeated key keeps its last value. Header absent/blank -> none.

`_ValidatedCbzWriter` (`.cbz` suffix, any case; `VERDICT = "verified"`): after size+digest checks, **verify the staged archive**
before publishing:

1. Read the first 4 bytes.
2. Run `verify_archive(temp, limit=InflateLimit())` (`processor/archives.py:560-668`):
   * open the zip (central directory) - failure => `structural` error text `<ExcType>: <msg>`;
   * members = distinct names (a later entry shadows an earlier same-name entry; the resolved entry is the last);
     directories skipped;
   * **inflate limit decided before reading**: every non-directory member must be stored (0) or deflate (8) else
     refused ("its pages are compressed with bzip2, LZMA, …, which a reader cannot open; re-pack it as an ordinary (deflate) zip");
     total declared uncompressed size must be `<= min(16 GiB, max(256 MiB, 20 x archive_size))` else refused
     ("its pages declare X.X GiB for a Y.Y MiB archive, more than any volume inflates to");
   * every member read to its end in 1 MiB chunks, validating inflate stream, local-header name and CRC-32; members that
     raise `NotImplementedError/RuntimeError` (encrypted/unsupported) are skipped, not damaged; `BadZipFile, zlib.error,
     EOFError, OSError, ValueError` => damaged member name.
3. Outcome (in this precedence):
   * ok -> publish; `X-Mokuro-Upload: verified`.
   * `refused` -> `422 archive-refused`, detail `The archive was not accepted: <refusal>.`, retry **false**.
     (This reason is **not** in `middleware/upload.py`'s docstring but is emitted.)
   * `structural` and first bytes not `PK\x03\x04` / `PK\x05\x06` (incl. an empty body) -> `422 not-an-archive`
     ("The upload is not a zip archive, so it cannot be a .cbz.", retry false).
   * else damage text `describe()` = `not a readable zip (<structural>)` or the first five damaged member names
     `'a.jpg' fails its CRC-32 check` / `'a', 'b' fail their CRC-32 checks` (+ ` and N more` when >5):
       * a digest was verified -> `422 archive-damaged`, "The archive arrived intact (sha-256 matched) but is damaged: <damage>. Your copy of it is damaged; re-import this volume.", retry false;
       * else consult `DamageMemory` (per destination path string, signature `(staged size, structural, sorted damaged names)`,
         LRU capacity 256, TTL 3600 s on a monotonic clock, in memory): same signature as the last time at that path
         within TTL -> `422 archive-damaged` "The archive is damaged: <damage>. The same damage arrived twice, so your copy of it is damaged; re-import this volume.", retry false;
         otherwise (first time / different damage) -> `422 archive-damaged` "The archive is damaged: <damage>. It may have been damaged on the way here; sending it again may work.", retry **true**.
   Successful writes at a path clear its memory.
`.cbz` verification holds a worker thread and the path lock for its duration (hence the inflate bound).
Tests: `tests/unit/test_upload_verdicts.py`, `tests/unit/test_processor_archives.py`.

`UploadOutcome.verdict`: `verified` (a `.cbz`) / `stored` (anything else, including progress files and sidecars).

### 8.9 Sidecar-aware conventions the resources encode (for reference)

Names the delete/ownership logic treats as belonging to volume `<stem>.cbz`: `<stem>.mokuro`, `<stem>.mokuro.gz`,
`<stem>.webp`, `<stem>.nocover`, `<stem>.<layer>.mokuro[.gz]`. `normalize_volume_key_from_library_relative` maps
`.cbz/.mokuro/.mokuro.gz/.webp/.nocover` (case-insensitive) to `<same path>.cbz`; other extensions have no volume.
`.cbr/.zip/.rar` are never treated as volumes by these paths (only `.cbz` is). The `.mokuro` text is OCR output
**in the reader's format**; whether the Rust server keeps producing/accepting it is outside this spec (the WebDAV
layer stores it as an opaque file).

### 8.10 nginx X-Accel-Redirect offload `[KEEP the contract if nginx stays in front; moot if Rust serves files itself]`

Enabled by env `MOKURO_NGINX_ACCEL=1` (set by `deploy/docker-entrypoint.sh`, which also starts nginx and moves
Python to `MOKURO_BACKEND_PORT`/127.0.0.1). `_nginx_accel_flag` sets `mokuro.nginx_accel = True` on each request
(`server.py:239-248`). For a **GET/HEAD of a library file** (`MokuroFileResource`, `resources.py:727-790`):

* `_compute_accel_redirect`: only if the flag is set and `file_path.resolve()` is under `library/`
  (per-user files and anything resolving outside are streamed through Python as before). Value:
  `"/internal-library/" + quote(rel.as_posix(), safe="/")` (percent-encode everything except `/`; e.g.
  `Series Ω/Vol #1.cbz` -> `/internal-library/Series%20%CE%A9/Vol%20%231.cbz`).
* `support_ranges()` -> false (nginx satisfies `Range`; WsgiDAV therefore ignores `Range`/`If-Range` and advertises no
  `Accept-Ranges`).
* `get_content()` -> empty body (no file read).
* `finalize_headers`: remove any `Content-Length`, then append **`Content-Length: 0`** and
  **`X-Accel-Redirect: <path>`**. `Content-Length: 0` is mandatory: without a Content-Length WsgiDAV forces `Connection: close`,
  which tore down nginx's upstream keep-alive pool and caused sporadic 502s on unrelated requests (comment at
  `resources.py:774-785`). nginx replaces it with the real size (and `Content-Range` for ranges).
* The response that nginx finally sends carries nginx's own `ETag`/`Last-Modified` for the file, **not** the DAV ETag
  (the DAV ETag is still emitted by Python on the internal hop but is dropped); conditional handling for the DAV ETag
  still happens in Python first (a client holding a DAV ETag from PROPFIND and sending `If-None-Match` gets the 304
  from Python; one holding nginx's ETag gets nginx's).
* nginx internal location: `location /internal-library/ { internal; alias ${MOKURO_LIBRARY}/; add_header
  Access-Control-Allow-Origin $cors_origin always; add_header Access-Control-Allow-Credentials true always; add_header Vary Origin always; }`
  (re-attaches CORS that X-Accel drops; Python's CORS headers are NOT carried across; hence the duplicate-header
  warning in the template comment). `Cache-Control`, `Content-Type`, `Content-Disposition`, `Accept-Ranges` from the
  upstream response survive an X-Accel hop in nginx; the security headers from Python do not necessarily.
* nginx also: gzips `application/json` (>=1 KiB), `client_max_body_size 2048M`, `proxy_request_buffering off`,
  `/_processor/` with unbuffered chunked streaming and no body limit, converts nginx-generated 5xx into a CORS-bearing
  `503 mokuro-bunko: backend temporarily unavailable`, real-IP recovery from private ranges (`set_real_ip_from
  10/8,172.16/12,192.168/16`), forwards `Host`, `X-Real-IP`, `X-Forwarded-For`, `X-Forwarded-Proto` (preserving the
  edge's).
Rust: a single binary can `sendfile`/stream with range support directly and emit the normal headers; then none of this
is needed (open question Q2: keep nginx at all? If yes, Rust must emit exactly the header pair above).
Tests: `tests/unit/test_nginx_accel.py`, `tests/integration/test_webdav_ops.py::TestNginxAccelContentLength`,
`tests/unit/test_nginx_config.py`.

### 8.11 On-disk footprint of this layer (everything it creates, writes or removes)

| Path | When | Notes |
|---|---|---|
| `<base>/library/`, `inbox/`, `users/`, `library/thumbnails/` | startup (`ensure_directories`, also by `PathMapper.ensure_directories` and `create_app`) | mkdir -p; default permissions per umask |
| `<dir>/.mokuro-write-test` | startup validation, in `base`, `library`, `inbox`, `users` | write `ok` then unlink |
| `<base>/mokuro.db` | `create_app` | SQLite (DB spec) |
| `<base>/logs/<server log>` (+ rotated copies) | `run_server` | 2 MiB x 5 |
| `<ssl cert dir>/{cert.pem,key.pem}` | auto_cert first run | 1.1 |
| `users/<username>/` and `{volume-data,profiles,goals}.json` inside | first progress PUT | file content is opaque client JSON; written atomically |
| `<dest dir>/.<name>.upload-<rand>.tmp` | during a PUT; removed on success (renamed), abort or reject | mode 0600 while staged, so a crash can leave it behind; nothing cleans old ones |
| final PUT file mode | after `os.replace` | `0o666 & ~umask` (POSIX); no chmod on Windows |
| `library/<Series>/` | MKCOL; also `mkdir -p` of a MOVE/COPY destination parent | |
| sidecars removed with a `.cbz` | DELETE of the archive | `.mokuro`, `.mokuro.gz`, `.webp`, `.nocover`, `<stem>.<layer>.mokuro[.gz]` (8.3.5) |
| copies | COPY keeps source mtime/mode (`shutil.copy2`) | |
| moves | `os.replace` (same filesystem, atomic; cross-filesystem raises -> 500) | folder MOVE moves the whole tree incl. sidecars |
| audit/ownership rows | 8.6 | SQLite |

The metadata subsystem separately writes `library/catalog.json` and `library/<Series>/series.json` (atomically, under the same
path locks, 8.7).

---------------------------------------------------------------------------------------------

## 9. `UploadMiddleware` (`middleware/upload.py`): verdict headers and failure JSON

Sits directly above `AuthMiddleware` (section 2.1 #13). Behaviour by request method:

* `OPTIONS` on a DAV path (`is_dav_path`): pass through; append `X-Mokuro-Put: verified` to the answer
  (whatever its status; `upload.py:104-107,250-256`). Pinned: `TestPutCapability` (`/`, `/mokuro-reader`,
  `/mokuro-reader/`, `/mokuro-reader/S/`, a file path).
  Not added on non-DAV routes (`/catalog/api/manifest`, `/_admin/...`).
* Methods other than `PUT MOVE COPY DELETE`: pass through unchanged.
* `PUT MOVE COPY DELETE`: the whole inner response is **buffered** (all body chunks joined), then post-processed and
  re-emitted as a single body. With `succeeded = status[0] == "2"`:
  1. **PUT with a verdict path** (`PATH_INFO` (raw, un-re-encoded) starts with `/mokuro-reader/` or `/inbox/`, and
     is not a compiled-metadata path): `outcome = environ["mokuro.upload"]` (may be absent).
     * success and `outcome.verdict`: add `X-Mokuro-Upload: verified|stored`, `X-Mokuro-Size: <bytes stored>` and,
       if the body matched a `Content-Digest`, `X-Mokuro-Digest-Verified: sha-256|sha-512`. Status stays the DAV
       status (`201` / `204`).
     * failure (any non-2xx, from auth, WsgiDAV or the writer): **replace** the answer with the JSON verdict below.
  2. **PUT whose path lowercased ends in `.cbz`** (success or failure): append `X-Mokuro-Put: verified`.
  3. `succeeded` and `environ["mokuro.archives_removed"]` list: for each path call `OcrControl.archive_removed(path)`
     (cancel queued/running OCR; exceptions logged). **[OCR-QUEUE]**
  4. `succeeded` and `environ["mokuro.archive_written"]` (a `.cbz` Path): `OcrControl.archive_arrived(cbz)`
     (queue it immediately instead of waiting for the poll) and, **for PUT only**, compute the follow-up headers
     (below). **[OCR-QUEUE]**
  5. `MetadataAPI` answers for `<Series>/series.json` and `catalog.json` (compiled paths) are never given a
     verdict.

Failure JSON (`_failure`, `upload.py:164-206`), always these four fields, `json.dumps` default separators
(`", "`, `": "`), ASCII-escaped:
```
{"ok": false, "reason": <str>, "detail": <str>, "retry": <bool>}
```
Resolution of `(status, reason, detail, retry)`:

1. If the writer recorded an outcome with a `reason` and `status`: use them (`truncated`, `corrupted-in-transit`,
   `archive-damaged`, `archive-refused`, `not-an-archive` -> `422`; `disk-full` -> `507`; `server-error` -> `500`).
2. else by the inner status `code`: `401` -> `forbidden`, retry false, `Sign in to upload.`; `403` -> `forbidden`,
   retry false, `This account may not upload this file.`; `429` -> `forbidden`, retry false,
   `Too many failed sign-ins; wait and try again.`; `507` -> `disk-full`, `The server's disk is full.`, retry false;
   anything else (`405 409 412 415 423 501 ...`) -> `server-error`, retry = `code >= 500 or code in (408, 423)`,
   detail `The server could not store the upload (<code>).`.
3. Status line `"<code> " + http.HTTPStatus(code).phrase` (`422 Unprocessable Content` on Python >= 3.13,
   `Unprocessable Entity` earlier; `Error` for an unknown code). `Content-Type: application/json` and
   `Content-Length` replace the inner ones; every other inner header is **kept** (`WWW-Authenticate` on a 401, `Allow`,
   `Lock-Token`, ...). The security layer then adds `Cache-Control: no-store` (JSON).
`[VERIFIED]` bodies: `{"ok": false, "reason": "forbidden", "detail": "Sign in to upload.", "retry": false}` (anonymous PUT),
`{"ok": false, "reason": "not-an-archive", "detail": "The upload is not a zip archive, so it cannot be a .cbz.", "retry": false}`.

**Follow-up headers after a successful `.cbz` PUT** (`_archive_arrived`, `upload.py:219-247`; **[OCR-QUEUE]**; with no
`OcrControl`/worker none of this exists and the PUT answers only the verdict headers):
`series = <relative parent posix>` of the archive under `library/` (`.` for a loose root file), `volume = stem`.
`pending = control.volume_pending(cbz, series, volume, running_jobs, wait=0.005 s, max_items=300)`. None/empty -> no
headers. Else `X-Mokuro-Manifest: /catalog/api/manifest?series=<enc>&volume=<enc>` (`enc` = UTF-8 percent-encoding with
safe set `!*'()`) and `X-Mokuro-Recheck-After: <int>` = `recheck_after(pending, now)`: earliest non-null ETA minus now
plus 10 s margin, ceil, clamped to `[30, 3600]`; `300` when no entry is priced (`ocr/volume_outlook.py:26-30,68-89`).
Any exception is logged and **ignored** (the file is stored; queuing is also the poll's job). MOVE/COPY into place only
queue (no headers). Pinned by `tests/unit/test_upload_enqueue.py`.

---------------------------------------------------------------------------------------------

## 10. PROPFIND cache (`middleware/propfind_cache.py`)

Purpose: a `Depth: infinity` listing of a large library (tens of thousands of resources; the source comment cites
33k resources and 264k method calls) is expensive in Python. Observable semantics are listed first because the Rust
port may implement them without a cache (Q6).

### 10.1 Observable behaviour (KEEP)

1. Applies to `PROPFIND` with request header `Depth` **exactly** `infinity` (case-sensitive compare of the raw header;
   `Infinity` bypasses the cache but WsgiDAV still treats it as infinite; a missing header also bypasses
   the cache, `get("HTTP_DEPTH","0")`, and WsgiDAV then treats it as `infinity`). `Depth: 0/1` never cached.
2. Cache key = path with slashes stripped and re-prefixed (`/mokuro-reader/` and `/mokuro-reader` share a key). The three
   per-user file paths bypass the cache entirely.
3. The cached response is generated from a **synthetic** request: same path, empty body, so **every Depth:infinity
   PROPFIND is answered as `allprop` regardless of the client's body** (a `<prop>` list in the request is ignored). For
   keys `/` and `/mokuro-reader` the synthetic request is **anonymous** (no `Authorization`, role `anonymous`, no
   user) so the shared entry contains no per-user files.
4. **Per-user injection** on every hit (and on the first fill) for keys `/` and `/mokuro-reader` when the caller is
   authenticated: for each of `goals.json`, `profiles.json`, `volume-data.json` (sorted) run an internal Depth:0 PROPFIND
   as that user; if it answers `207`, take its `<D:response>` elements and **append** them at the end of the cached
   multistatus unless an identical `<D:href>` already exists. If nothing was inserted the cached bytes are served
   untouched; otherwise the XML is re-serialised with declaration `<?xml version='1.0' encoding='utf-8'?>`. Other
   paths get no injection. Pinned by `test_propfind_mokuro_reader_depth_infinity_authenticated_injects_progress`,
   `..._root_depth_infinity_authenticated_injects_progress`, `test_utf8_user_gets_propfind_cache_injection`.
5. Response: status `207`; stored headers minus `Content-Length`/`Content-Encoding`/`Transfer-Encoding`, plus
   `Vary: Accept-Encoding`, plus `Content-Encoding: gzip` (level 6) iff `"gzip"` appears anywhere in `Accept-Encoding`
   (no q-value parsing), plus `Content-Length`. The stored `Date` header is the generation time (stale on hits; QUIRK).
6. Only `207` results are cached; any other status (404 ...) is not cached and the request is executed again against
   WsgiDAV (double work).
7. Auth has already run: an anonymous `PROPFIND` with `allow_anonymous_browse=false` is rejected before the cache.

### 10.2 Lifecycle (implementation, `ttl=120.0` set in `server.py:251`, `stale_ttl=86400`, `max_entries=10`)

* `age < ttl` fresh: serve. `ttl <= age < stale_ttl`: serve the stale body and start ONE background refresh for the key
  (`propfind-refresh` daemon thread; a `_refreshing` set de-duplicates). `age >= stale_ttl` (24 h) or absent: block,
  generate, store.
* Eviction on insert of a NEW key when 10 are held: delete the entry with the oldest generation time.
* **Write invalidation:** any `PUT DELETE MKCOL MOVE COPY` that reaches this layer (i.e. was authorised, and is not a
  `series.json` PUT or queue-file request) calls `refresh_all()` **before** the write executes: a background refresh of
  every cached key from an anonymous synthetic request (stale entries stay servable meanwhile). RACE (QUIRK): the refresh
  may walk the tree before the write lands and then cache the pre-write listing as *fresh* for up to 120 s; the filesystem
  watcher's debounced refresh (below) heals it ~5 s after the last change. Rust should invalidate **after** the write
  commits (or skip caching).
* `schedule_refresh(delay=5.0)`: debounced `threading.Timer` (restarted by every call; fires `refresh_all` after `delay`
  seconds of quiet; prints `[PROPFIND-CACHE] Debounced refresh triggered` to stderr). Called by the filesystem watcher and
  by `on_metadata_published`. After `stop()` it is a no-op (`_stopped`), pinned by `tests/unit/test_propfind_cache.py`.
* `warm()` at startup (`server.py:423-424`): prints `Warming PROPFIND cache...`, then a `propfind-warm` daemon thread
  generates the anonymous `/mokuro-reader` Depth:infinity entry and prints
  `[PROPFIND-CACHE] Warmed /mokuro-reader: <MB> MB raw, <KB> KB gzip, <s>s` (or `Warm failed: _generate returned None`).
* `invalidate()` clears all entries (unused by the server).
* `_generate` runs the inner app with a copy of the environ restricted to primitive types plus an empty `wsgi.input`;
  therefore resources see **no** `mokuro.db` (no audit/DB), but do see the string `mokuro.username`/`mokuro.role` for
  non-listing keys.

GIL/threads note: the whole cache + background-refresh-thread design exists to keep Python worker threads from
re-walking the tree; **DROP the mechanism if the Rust listing is fast** (Q6), keep 10.1.

---------------------------------------------------------------------------------------------

## 11. Virtual queue file `GET /mokuro-reader/.mokuro-queue.json` (`middleware/queue_file.py`) **[OCR-QUEUE]**

One virtual JSON document under the WebDAV root that a reader polls instead of one request per volume.

Routing (`queue_file.py:161-195`), evaluated first-match:

1. `MOVE`/`COPY` whose `Destination` path equals `/mokuro-reader/.mokuro-queue.json` (any source) -> `405`.
2. `PATH_INFO` (exact string, not re-encoded) != the file path -> pass on.
3. `OPTIONS` -> pass on (normal DAV OPTIONS).
4. Any method other than `GET`/`HEAD` (PUT, DELETE, MOVE, COPY, PROPFIND, ...) -> `405 Method Not Allowed`,
   `Allow: GET, HEAD, OPTIONS`, `Content-Type: text/plain; charset=utf-8`, body
   `The OCR queue file is generated by the server and cannot be written.` (with `Content-Length`).
5. `GET`/`HEAD`: `AuthMiddleware.gate_read(path)` (exactly a `GET` of a library file: bearer/basic, limiter,
   `allow_anonymous_download`, 401 challenge). Refusal returned as-is.
6. Serve (below). CORS is the outer layer's (`tests/unit/test_queue_file.py::test_cors_like_a_library_file`). It is not a
   file: no PROPFIND lists it (`test_hidden_from_propfind`).

Document (`build_document`, `queue_file.py:64-95`; `json.dumps(..., separators=(",", ":"))`):
```
{"version": 1,
 "generated_at": "YYYY-MM-DDTHH:MM:SSZ",          # UTC, excluded from the ETag
 "held": null | {"reason": <plain code string>},
 "next_check_after": null | <int seconds>,        # recheck_after(all jobs, now) (section 9)
 "pending_volumes": <int>,                        # ALL waiting volumes; `volumes` lists running + only the next 100 waiting
 "volumes": [ {"series": s, "volume": v,
               "path": "/mokuro-reader/<enc s>/<enc v>.cbz",          # percent-encoded, safe "!*'()"
               "manifest": "/catalog/api/manifest?series=<enc>&volume=<enc>",
               "jobs": [ {"kind": ..., "id": ..., "state": "running"|"queued"|"held",
                          "eta": "...Z"|null, "progress": ...}, ... ] } ... ] }
```
`volumes`/`held`/`pending_volumes` come from `OcrControl.queue_document(running_jobs, wait=1.0)` (OCR spec). No machine
names, no errors/failure details. With **no OcrControl/worker**: `held null`, `volumes []`, `pending_volumes 0`,
`next_check_after null` (`[VERIFIED]`: `{"version":1,"generated_at":"2026-10-01T18:11:15Z","held":null,"next_check_after":null,"pending_volumes":0,"volumes":[]}`).

Caching (`_current`, `queue_file.py:197-221`), one lock:

* A built body younger than **1.0 s** (monotonic) is served again as is (a poll per second from many readers = one build).
* On rebuild: for each job whose previously *published* ETA is within **60 s** of the new one, keep the old ETA (an idle queued
  job's ETA drifts by the second and would otherwise change the ETag every poll). If `version`, `held` and `volumes` equal the
  last published document's (everything except `generated_at`, `next_check_after`, `pending_volumes` is compared; QUIRK:
  a change in only `pending_volumes`/`next_check_after` is not republished), the previous body/ETag/gzip are reused.
* ETag = `"` + first 32 hex of SHA-256 over `json.dumps(document without generated_at, sort_keys=True, separators=(",", ":"))` + `"`.
  The gzip representation (level 6) has its own tag: the same string with `-gz` inserted before the closing quote.

Response headers: `Content-Type: application/json`, `Cache-Control: no-cache`, `Vary: Accept-Encoding`, [`Content-Encoding: gzip` if
`gzip` is a substring of the lowercased `Accept-Encoding`], `ETag`, and (200 only) `Content-Length`. `If-None-Match`: comma-split,
stripped, **exact string** match against the representation's ETag (no `*`, no weak) -> `304 Not Modified` with the same headers and no
body/`Content-Length`. `HEAD`: same as 200 with empty body. (The security layer adds nothing further: it already has
`Cache-Control`; `application/json` + existing `Cache-Control` is left alone.)
Tests: `tests/unit/test_queue_file.py`.

---------------------------------------------------------------------------------------------

## 12. Filesystem watcher, invalidation graph, background threads and timings

### 12.1 `LibraryWatcher` (`middleware/fs_watcher.py`)

* `watchdog` Observer (inotify) on `library/` recursively, daemon thread, started in `create_app` (`server.py:440-445`);
  prints `[FS-WATCHER] Watching <path>` to stderr; if the `watchdog` package is missing prints
  `[FS-WATCHER] watchdog not installed; filesystem watching disabled` and runs without. `stop()` joins up to 5 s.
* Events handled: **created, deleted, moved** only (no `modified`/`closed`). `moved` fires the callback once per relevant
  side (src and dest if they differ): an atomic upload's staging-file -> final rename delivers only the destination.
* Relevance (`_is_relevant`): any **directory**; any file whose `Path.suffix` is one of `.cbz .mokuro .gz .webp` or whose name ends
  in `.mokuro.gz`. (`.json`, `.jpg`, `.nocover`, `.tmp` are irrelevant. The metadata compiler's own `.json` writes therefore do not
  re-trigger it.)
* No debounce of its own (debouncing is the PROPFIND cache's timer and the metadata service's scheduler).
* `classify_change(library_root, path)` (`:43-65`): path not under the root -> `("library", None)`; fewer than 2 relative parts
  (a top-level entry or the root) -> `("library", None)`; first part `thumbnails` -> `("ignore", None)`; else `("series", <first part>)`.
  Pinned by `tests/unit/test_fs_watcher.py`.

### 12.2 Invalidation graph

`on_library_change(path)` (`server.py:426-437`), called on the watcher thread for every relevant event:
1. `library_index.invalidate()` (shared `LibraryIndexCache`, TTL 30 s; catalog/home/queue read it) - **always**, even for `ignore`.
2. `propfind_cache.schedule_refresh(5.0)` - always.
3. `kind == "series"` -> `metadata_service.schedule_series_regeneration(<title>)`; `kind == "library"` ->
   `metadata_service.schedule_regeneration()`; `ignore` -> nothing.

`on_metadata_published()` (`server.py:255-268`; a compile changed compiled files): `library_index.invalidate()`,
`propfind_cache.schedule_refresh(5.0)`, and `queue_api.invalidate_skipped()` if an OCR control with a queue page exists.
It deliberately does not schedule another regeneration (feedback loop; the watcher also ignores `.json`).

Synchronous (request path): PUT/DELETE/MKCOL/MOVE/COPY -> `propfind_cache.refresh_all()` (10.2). Library mutations by DAV also surface
through the watcher.

### 12.3 Background services created by `create_app` and their timings

| Service | Start | Timing | Notes |
|---|---|---|---|
| `DynDNSService` | immediately iff `config.dyndns.enabled` | its own loop | other spec |
| `PropfindCache.warm` | end of `create_app` wiring | once, async | 10.2 |
| `LibraryWatcher` | `create_app` | continuous | 12.1 |
| `MetadataService.schedule_regeneration(delay=20.0)` | `create_app` | once, **20 s** after start ("after startup settles; the PROPFIND warm competes for the disk") | metadata spec |
| `MetadataService.start_periodic_rescan(6*3600)` | `create_app` | every **6 h** | catches missed events |
| per-series regeneration timers | watcher events | debounce in metadata service | metadata spec |
| `CommunityFetcher` | iff `catalog.enabled and catalog.enrich_community`; `metadata_service.on_external_ids_changed = fetcher.request_fetch` | hourly sweep + on id change | AniList/MAL |
| `PropfindCache` refresh threads / debounce `Timer` | on demand | 5 s debounce; 120 s TTL | 10.2 |
| `ThreadPoolWatchdog` | `run_server` | every 5 s | **DROP** |
| `OCRWorker` (+ processors registry) | `run_server` | poll interval `ocr.poll_interval` | **DROP / OCR-QUEUE** |

`tests/unit/test_server_shutdown_order.py::test_nothing_the_app_armed_outlives_shutdown` pins that `create_app` arms at least two
timers (20 s first pass + 6 h rescan) and that `shutdown_app` (twice) leaves no armed thread alive within 10 s.

---------------------------------------------------------------------------------------------

## 13. Request log (`middleware/request_log.py`)

Outermost layer. `_enabled` is decided **once at construction** from `MOKURO_DEBUG` (value not in `""`, `0`, `false`).
Disabled: transparent. Enabled: for each request records the status of the first `start_response` call and prints one
line to **stderr** after the inner app has *returned* its iterator (so timing excludes body streaming):
`[REQUEST] [<thread name>] <METHOD> <PATH_INFO> -> <status code> (<seconds:.3f>s)`; on an exception
`[REQUEST] [<thread>] <METHOD> <path> -> EXCEPTION (<s>s): <Type>: <message>` then re-raise. `PATH_INFO` is the raw latin-1 string.
`[DEBUG tooling only; KEEP as `tracing` spans if desired]`. Separately WsgiDAV logs one summary line per DAV request at
verbose>=3 (`verbose` is 1 here, so it does not).

---------------------------------------------------------------------------------------------

## 14. Python quirks catalogue (what to replicate vs fix)

Numbered for cross-reference. **[VERIFIED]** = reproduced against the real app/WsgiDAV 4.3.5.

| # | Quirk | Verdict |
|---|---|---|
| 14.1 | **MOVE/COPY between classes.** A file `MOVE` whose destination is another class (progress -> library path, or library file -> `/mokuro-reader/<per-user name>`, or anything -> `/foo`) makes `handle_move` return False; WsgiDAV then runs its generic path where `copy_move_single` silently does nothing (returns False, ignored) and the source is **deleted** anyway. `[VERIFIED]`: alice `MOVE /mokuro-reader/volume-data.json -> /mokuro-reader/S/x.cbz` = `201`, her progress file gone, nothing created. A library `.cbz` MOVE onto `/mokuro-reader/goals.json` deletes the volume (+ sidecars) AND the caller's own goals file. COPY of mismatched classes = `201` and nothing copied. | **FIX**: if source and destination are not the same class (`library` / `progress`) or a destination is not under the reader root, answer `403 Forbidden`, no side effects. |
| 14.2 | **File over collection.** `COPY\|MOVE <file> -> <existing collection>` with `Overwrite: T` (default): WsgiDAV deletes the destination collection first (RFC 4918 9.8.4). `[VERIFIED]`: `COPY /mokuro-reader/S/big.bin -> /mokuro-reader/S` removed the whole series folder `S` (the source was inside it) and answered 500. Requires `MODIFY_DELETE`. | **FIX**: refuse file<->collection destination mismatches with `409 Conflict`, no effects. |
| 14.3 | **Virtual-folder source.** `MOVE /mokuro-reader -> /anywhere` (also `/`): the virtual folder has no native move, so the generic loop runs `copy_move_single` (no-op) then deletes every descendant file and folder: **`201` and the whole library is gone** (`[VERIFIED]`, needs `MODIFY_DELETE`). `DELETE /mokuro-reader` is a no-op `204` ... unless some descendant carries a DAV lock, in which case everything unlocked is deleted file-by-file and the answer is `207` (`[VERIFIED]`). `COPY` of a virtual folder is harmless. | **FIX**: `MOVE`/`COPY`/`DELETE`/`PROPPATCH`/`LOCK` whose target is `/` or `/mokuro-reader` (any trailing-slash form) -> `403`. (`DELETE` currently `204`: keep 403 or 204, never wipe.) |
| 14.4 | **Stale `ETag` on PUT overwrite.** | **FIX** (8.3.4). |
| 14.5 | **`If-Modified-Since` equal date -> 200.** | **FIX** (8.4). |
| 14.6 | `Range`: unit not checked; multi-range collapses to the highest merged range; `bytes=` junk -> 416 without `Content-Range`. | replicate single-range; low priority. |
| 14.7 | Locks keyed by URL (cross-user collision on per-user files), principal `""`, `LOCK` of a missing path -> 500 + dangling lock, `Content-Type: application; charset=utf-8`. | Q4. |
| 14.8 | `PROPFIND` `<propname/>` (the RFC element) unrecognised -> empty responses. | implement properly (harmless). |
| 14.9 | `Depth:infinity` PROPFIND answers allprop whatever the body asks; cached `Date` is stale; `Depth: Infinity` bypasses the cache. | keep allprop; Date fresh. |
| 14.10 | `OPTIONS` of any not-yet-existing path under an existing collection (incl. `/inbox`) is `200 Allow: OPTIONS, PUT, MKCOL`. `OPTIONS` on `/inbox/x`: `404`. Collections advertise no `PUT`/`MKCOL`. | replicate (clients ignore). |
| 14.11 | 405 answers (`POST`, unknown methods) carry no `Allow`. | add `Allow` (harmless). |
| 14.12 | `201`/`MKCOL` responses carry an HTML body; `204` has none. | any small body is fine. |
| 14.13 | `library/thumbnails/` is created at startup and listed in PROPFIND of `/mokuro-reader/`; in-flight staging files (`.<n>.upload-*.tmp`) and dotfiles are listed. | Q3. |
| 14.14 | One audit row per successful PUT incl. each progress save; a DB error after the rename makes the PUT `500` although the file was stored. | decide in DB spec; at least do not 500 after commit. |
| 14.15 | Per-user names are matched case-sensitively; `Volume-Data.json` is a shared library file. | replicate. |
| 14.16 | `AuthMiddleware` error bodies have no `Content-Length` (cheroot frames them); 429 has no `Retry-After`. | add `Content-Length`; `Retry-After` optional. |
| 14.17 | Two independent auth limiters (WebDAV vs login/queue pages); limiter map grows per distinct key; failures from non-existent users count the same as existing ones. | Q7. |
| 14.18 | No SIGTERM handler. | **FIX** (1.2). |
| 14.19 | Non-UTF-8 request path => 500 from the DAV layer. | 400/404. |
| 14.20 | A symlink leaving the library is listed but 404 on access; a symlink inside to another in-library path works. | replicate resolve-and-contain. |
| 14.21 | COPY'd `.cbz` is untracked (no owner row); COPY over an existing `.cbz` deletes the destination first, which also deletes its sidecars and ownership. MOVE over an existing destination `os.replace`s without that cleanup. | replicate, or document. |
| 14.22 | `handle_move` for a library file requires `MODIFY_DELETE`, so `uploader` may rename nothing, even its own uploads. | replicate. |
| 14.23 | Anonymous GET of `/mokuro-reader/` (with trailing slash) bypasses the "browse disabled" 401 (only the exact strings `/` and `/mokuro-reader` are gated), then 403 "Directory browsing is not enabled". | harmless. |
| 14.24 | `QueueFile` stable-document test ignores `pending_volumes`/`next_check_after`. | OCR-QUEUE owner. |

---------------------------------------------------------------------------------------------

## 15. DROP list (do not port)

Server / startup:
* `server.py:615-898`, `904-1018`: OCR environment detection/installation (`OCRInstaller`, `EnginesInstaller`, mokuro env
  rebuilds, `needs_mokuro_env`, detector installs `ctd`/`rtdetr`/paddle, `local_problems`, `worker_generations`, GPU backend
  selection, `probe_devices`/`set_cached_catalog`), the `ocr_runtime` dict, `ocr.backend/generations/autobench` plumbing.
  (mokuro/manga-ocr, ctd, rtdetr, animetext only.)
* `cheroot_watchdog.py` (`ThreadPoolWatchdog`) and `_start_server_resilient` (cheroot worker-death workaround, Windows
  `WinError 10022`).
* `request` re-encode hacks `re_encode_wsgi_path` / WsgiDAV `re_encode_path_info` (Python's latin-1 PATH_INFO).
* PROPFIND cache machinery that exists only because Python is slow/threaded: `_copy_environ`, `_make_warm_environ`, the
  synthetic anonymous environ, refresh threads, `_refreshing` set (keep only the observable semantics of 10.1).
* `_LockedWriter` / `_AuditedWriter` file-object shims and `end_write(with_errors=True)` abort protocol (WsgiDAV never
  closes the writer on error); in Rust use a guard type.
* GIL-era `PUT_PRICE_WAIT = 5 ms` bounded wait (an in-memory sort at ~0.05 ms/item) - only meaningful with the OCR queue.
* Dead code: `METHOD_PERMISSIONS`, `parse_basic_auth` wrapper, `allow_anonymous` property (`auth.py:88-101,270-282,374-382`),
  `cors.compile_origin_pattern`, `Permission` pieces unused by DAV (`MANAGE_INVITES` only for `/_admin/api/invites`).
* `_remember_primary_uuid` (reads `volume_uuid` from a mokuro-produced `.mokuro`) and `db.remember/forget_volume_uuid` /
  `rename_volume_uuids_under_prefix` hooks in delete/move **unless** the OCR identity feature survives (OCR-QUEUE).
* The registry-of-generations coupling of sidecar naming (`ocr.generations`) - keep only the filename-grammar sweep of 8.3.5.
* nginx X-Accel (8.10) if the Rust server serves files itself.
* `MOKURO_DEBUG` request log in its current form (replace with structured tracing).

---------------------------------------------------------------------------------------------

## 16. Test index (what is pinned, where)

| Area | Tests |
|---|---|
| Startup validation | `tests/unit/test_startup_validation.py`, `tests/unit/test_ssl_validation.py`, `tests/integration/test_ssl.py` |
| Shutdown order / armed timers | `tests/unit/test_server_shutdown_order.py` |
| Watchdog (DROP) | `tests/unit/test_cheroot_watchdog.py` |
| Auth authn/authz | `tests/integration/test_auth.py`, `tests/unit/test_permissions.py`, `tests/unit/test_metadata_permissions.py`, `tests/unit/test_uploader_replace.py`, `tests/unit/test_non_ascii_ownership.py` |
| Client IP / proxies | `tests/unit/test_client_ip.py` |
| CORS | `tests/integration/test_cors.py`, `tests/unit/test_cors.py` |
| Security headers | `tests/integration/test_security_headers.py`, `tests/unit/test_security_headers.py` |
| robots/static | `tests/unit/test_static.py` |
| Path mapping | `tests/unit/test_path_mapping.py`, `tests/unit/test_metadata_paths.py` |
| WebDAV ops | `tests/integration/test_webdav_ops.py` |
| Atomic writes / path locks | `tests/unit/test_atomic_writes_locks.py` |
| Upload verdicts / digest / damage memory / zip bounds | `tests/unit/test_upload_verdicts.py`, `tests/unit/test_processor_archives.py` |
| Upload -> queue | `tests/unit/test_upload_enqueue.py` |
| nginx offload | `tests/unit/test_nginx_accel.py`, `tests/unit/test_nginx_config.py`, `tests/integration/test_webdav_ops.py::TestNginxAccelContentLength` |
| PROPFIND cache | `tests/unit/test_propfind_cache.py` (timer lifecycle), injection tests in `test_webdav_ops.py`/`test_auth.py` |
| Queue file | `tests/unit/test_queue_file.py` |
| FS watcher | `tests/unit/test_fs_watcher.py` |

Not covered by any test (so unpinned; decide freely): LOCK/UNLOCK success paths, PROPPATCH, COPY, Range details,
`If-*` header matrix, PROPFIND `name`/named modes, 14.1-14.3, per-user injection ordering, cache TTL numbers.

---------------------------------------------------------------------------------------------

## 17. Open questions

* **Q1 (timeouts/limits).** cheroot idles/stalls at 10 s, no body/header limits, 50 worker threads. What read/idle/keep-alive timeouts
  and max body should the Rust server enforce (nginx's `client_max_body_size 2048M`, `proxy_read_timeout 300s` are the bundled nginx config's values)?
* **Q2 (nginx).** Keep nginx + `X-Accel-Redirect` in front (then Rust must emit `Content-Length: 0` + `X-Accel-Redirect` exactly and ignore
  `Range`), or serve files directly with ranges/sendfile (then drop 8.10 and the `MOKURO_NGINX_ACCEL` flag)? Also: should the Rust
  server honour the CORS-in-nginx split, or emit its own on every path?
* **Q3 (listing hygiene).** Hide dotfiles, `.<n>.upload-*.tmp` staging files and `thumbnails/` from PROPFIND? Python lists them all.
* **Q4 (LOCK/UNLOCK).** Keep (class 2, `DAV: 1,2`, in-memory table, with the quirks removed) or DROP (`DAV: 1`, 405)? The reader appears not to
  use it, but desktop WebDAV clients (Finder/Explorer) often require LOCK to write. Nothing in repo/tests exercises it.
* **Q5 (PROPPATCH / dead properties).** Drop, 403-all, or implement persistently? Python is in-memory-only.
* **Q6 (PROPFIND cache).** Is a Depth:infinity cache needed in Rust, and if so should it invalidate after commit (not before) and per-subtree?
  Should `Depth:infinity` honour the request's `<prop>`/`<propname>` instead of always answering allprop?
* **Q7 (rate limiting).** One shared limiter for WebDAV + login + queue pages, eviction policy, and add `Retry-After`? Key stays `ip:username`
  or also per-IP?
* **Q8 (FIX decisions).** Confirm 14.1-14.3 (403/409 instead of data loss) and 14.4/14.5 are acceptable divergences from 0.5.2.
* **Q9 (OCR-queue coupling).** Sections 9 (follow-up headers), 11 (queue file) and 12.3 assume the OCR queue/processors survive in some form. If
  OCR moves out of the server entirely: keep `/mokuro-reader/.mokuro-queue.json` as the constant empty document above (reader polls
  it) and drop `X-Mokuro-Manifest`/`X-Mokuro-Recheck-After`; confirm.
* **Q10 (audit).** Keep an audit row per progress-file save (`edit`)? And should DB write failures after the atomic rename still fail the PUT?
* **Q11 (copies).** Should a COPY of a `.cbz` register an owner (currently untracked) and should COPY/MOVE carry sidecars?
* **Q12 (`DAV` ETag vs nginx ETag).** If nginx stays, whose ETag does the client see for downloads (nginx's), and is that acceptable for
  `If-None-Match` handled in Python?
* **Q13 (WsgiDAV version).** `wsgidav>=4.0` is unpinned; the spec was verified on 4.3.5. Differences in 4.0-4.3.4 (e.g. status text for
  `423/424`, `get_property_names` signature) were not checked.
* **Q14 (signals/exit).** Graceful shutdown semantics for SIGTERM/SIGINT (drain in-flight uploads? abort staging files?) and exit code on
  startup validation failure (Python: 2).
* **Q15 (HEAD on /_static, other static methods).** `HEAD /_static/x` currently 404s (only GET handled); acceptable?

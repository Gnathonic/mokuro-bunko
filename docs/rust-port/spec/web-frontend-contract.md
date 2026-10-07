# Web frontend contract (mokuro-bunko 0.5.2 -> Rust server)

Source of truth: `src/mokuro_bunko` (the Python 0.5.2 tree)
(commit 199cff5, 0.5.2). The static frontends are reused UNCHANGED, so the Rust server must
serve them byte-for-byte and answer every request below with the shapes the JS reads.
Everything here was derived from the JS/HTML (what the page does) cross-checked against the
Python handlers (what the server answers). Sections marked "JS reads" list ONLY the fields the
frontend consumes; the Python may send more (extra fields may be dropped or kept freely).

Contents: 1 Global conventions - 2 Static serving - 3 Templating - 4 Shared nav.js - 5 Pages -
6 Endpoint index - 7 Engine/detector references - 8 File inventory - 9 Port gotchas.

---------------------------------------------------------------------------------------------

## 1. Global conventions

### 1.1 Auth model used by every page
- NO cookies anywhere. No `Set-Cookie`, no `document.cookie`, no `credentials:` option on fetch.
- Sign-in = `POST /login/api/token` (JSON creds) -> bearer token. Pages keep it in
  `sessionStorage` (per tab): `mokuro_token` (string), `mokuro_user` (JSON `{username, role}`).
  Legacy key `mokuro_auth` is deleted on every nav.js load.
- Every authenticated fetch sends `Authorization: Bearer <token>`. Basic auth is never sent by
  the pages (the server must still accept Basic on `/login/api/me` etc. for the reader).
- localStorage keys (catalog only): `mokuro_catalog_title_lang` (native|english|folder),
  `mokuro_catalog_sort` (title|newest|densest|rating), `mokuro_reader_url` (user override URL).
- Page-level guards (client side only): account, admin redirect to `/login` when no token;
  queue redirects to `/login` on 401 or when `public_access=false` and no token.
- Bearer token invalid/expired/revoked: `/login/api/me` -> 401 `{authenticated:false,error}`;
  admin API (via auth middleware) -> 401; account API -> 401 `{error}`; queue status -> 200 as
  visitor + header `X-Queue-Auth: failed`.

### 1.2 Response header rules (SecurityHeadersMiddleware, applies to ALL responses)
Always add if absent: `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`,
`Referrer-Policy: no-referrer`, `X-XSS-Protection: 1; mode=block`,
`X-Robots-Tag: noindex, nofollow`. JSON (`application/json`) without Cache-Control gets
`Cache-Control: no-store`. `image/*` without Cache-Control gets `private, max-age=86400`.
There is NO Content-Security-Policy. The HTML relies on inline `onclick=`/`onsubmit=` handlers
(account, setup, admin, catalog markup) and inline `style=` - a CSP must not be added.
`/robots.txt` -> `User-agent: *\nDisallow: /\n` (text/plain).

### 1.3 JSON error convention
Nearly every handler errors as `{"error": "<message>"}` with a non-2xx status. The JS reads
`data.error` for display. Admin `apiPost/apiPut/apiDelete` call `response.json()` BEFORE
checking `ok`, so EVERY admin POST/PUT/DELETE response (including errors) MUST have a JSON
body (an empty or HTML body throws and the real message is lost). `apiGet` tolerates a non-JSON
error body. Register/setup/account/login read `await response.json()` unconditionally too.

### 1.4 Timestamps the JS parses
- `users[].created_at`, audit `created_at`, `/login/api/me.created_at`: SQLite
  `CURRENT_TIMESTAMP` UTC text `YYYY-MM-DD HH:MM:SS` (no zone). account.js does
  `new Date(created_at + 'Z')`; admin audit does `created_at.replace(' ','T') + 'Z'`; admin
  users table does bare `new Date(created_at)` (parsed as LOCAL time by browsers - Python-
  compatible behaviour; keep the format identical).
- Invites `created_at`/`expires_at`: Python `datetime.isoformat()` naive LOCAL time; the server
  compares `expires_at` to local `now()`. JS: `new Date(expires_at)`.
- Queue: `eta_at`, `queue_done_at` = ISO-8601 UTC strings; `since`, `until`, `at`,
  `disconnected_at`, `held.until`, `connected_since`, `last_at` = epoch SECONDS (float).
- Catalog `ocr-status.updated_at`: epoch seconds; `latest_volume_modified`: epoch seconds
  (used only as a sort key).

---------------------------------------------------------------------------------------------

## 2. Static serving (URL -> file), exactly as Python mounts it

All files are read from disk and returned verbatim (NO templating, see s3). Middleware order
(outermost first): RequestLog > SecurityHeaders > CORS(optional) > **StaticMiddleware** >
**SetupWizard** > **HomePage** > **Account** > **Login** > **Registration** > **Queue** >
**Catalog** > QueueFile > Upload > **Auth** > Metadata > (processor, **Admin**, WebDAV...).
So home/account/login/registration/queue/catalog/setup/static sit OUTSIDE auth (public, they
authenticate themselves); `/_admin/*` sits INSIDE auth.

| URL (GET) | File | Cache-Control | Not-found behaviour | Gate |
|---|---|---|---|---|
| `/_static/shared.css`, `/_static/nav.js` | `static/shared.css`, `static/nav.js` | `public, max-age=3600` | 404 text/plain `Not Found` (`..` or leading `/` -> 404; outside dir -> 403) | none |
| `/` (browser) | `home/web/index.html` | `no-cache` | n/a | see below |
| `/_home/<file>` | `home/web/<file>` (`styles.css`, `home.js`) | `no-cache` | 404 JSON `{"error":"File not found"}` | none |
| `/login`, `/login/` | `login/web/index.html` | `no-cache` | - | none |
| `/login/<file>` | `login/web/<file>` (`styles.css`, `login.js`) | `no-cache` | 404 text/plain `Not found` (403 text `Forbidden` if escapes dir) | none |
| `/register`, `/register/` | `registration/web/register.html` | NONE sent | - | none (disabled mode is handled client-side) |
| `/register/<file>` | `registration/web/<file>` (`styles.css`, `register.js`) | NONE sent; MIME from `mimetypes.guess_type` | 404 JSON `{"error":"File not found"}` | none |
| `/account`, `/account/` | `account/web/index.html` | `no-cache` | - | none (JS guard) |
| `/account/<file>` | `account/web/<file>` (`styles.css`, `account.js`) | `no-cache` | 404 text/plain `Not found` | none |
| `/setup`, `/setup/` | `setup/web/index.html` | `no-cache` | - | 403 JSON `{"error":"Setup is only allowed from localhost"}` when `needs_setup` and request is not loopback |
| `/setup/<file>` (not `api/`) | `setup/web/<file>` (`setup.css`, `setup.js`) | `no-cache` | 404 JSON `{"error":"Not found"}` | same loopback gate while setup needed |
| `/catalog`, `/catalog/` | `catalog/web/index.html` | `no-cache` | - | only when `catalog.enabled` (read live); else request falls through to the rest of the stack |
| `/catalog/<file>` | `catalog/web/<file>` (`styles.css`, `catalog.js`) | `no-cache` | **SPA fallback: unknown file -> `index.html` 200** | same |
| `/queue`, `/queue/` | `queue/web/index.html` | `no-cache` | - | none (page decides via `/queue/api/config`) |
| `/queue/<file>` | `queue/web/<file>` (`styles.css`, `queue.js`) | `no-cache` | 404 text/plain `Not found` | none |
| `/_admin`, `/_admin/` | `admin/web/index.html` | `no-cache` | - | static GET is ungated; JS redirects if no token |
| `/_admin/<file>` | `admin/web/<file>` (`styles.css`, `admin.js`) | `no-cache` | **SPA fallback: unknown file -> `index.html` 200**; 403 text `Forbidden` if escapes dir | static ungated; every `/_admin/api/*` is gated |

MIME types the Python sets (use the same): `.html text/html; charset=utf-8`,
`.js application/javascript; charset=utf-8`, `.css text/css; charset=utf-8`, `.json
application/json`, `.png/.jpg/.webp/.ico/.svg/.woff/.woff2` standard. Content-Length always set.

`/` handling (HomePageAPI, then SetupWizard in front of it):
1. SetupWizard: `GET /` with `Accept` containing `text/html` and `needs_setup` (no admin user
   exists) -> `302 Location: /setup`.
2. HomePage: `GET /` "browser request" (Accept contains `text/html`, OR no WebDAV-ish
   User-Agent [davfs, cadaver, cyberduck, webdav, gvfs, nautilus, finder, microsoft-webdav,
   litmus] and no `Depth` header) AND `catalog.enabled && catalog.use_as_homepage` ->
   `302 Location: /catalog/`; else serve home `index.html`. Non-browser `/` falls through to
   WebDAV.
3. `OPTIONS /api/health|/api/stats` -> 204 `Allow: GET, OPTIONS`; other methods -> 405
   `{"error":"Method not allowed"}`.

Security note for the Rust port: `static/__init__.py` lives in the served directory, so Python
would serve `/_static/__init__.py`. Serve ONLY an explicit allow-list (`shared.css`, `nav.js`).

`/_admin` base path: Python reads it from `admin.path` config (default `/_admin`), but
`admin/web/index.html` hard-codes `/_admin/styles.css`, `/_admin/admin.js`, and `admin.js`
hard-codes `API_BASE = '/_admin/api'`, and nav.js links `/_admin`. The Rust server must keep
the path `/_admin` (a non-default `admin.path` breaks the frontend today).

---------------------------------------------------------------------------------------------

## 3. Server-side template substitution

**None.** No Python code does `.replace()`, `format`, Jinja, nonce injection, or any rewriting
of any HTML/JS/CSS. All pages are pure static files; every dynamic value is fetched by JS from
a JSON endpoint. (Literal `{ip}`, `{domain}`, `{token}` in admin/index.html line 500-501 are
user-facing hint text, not placeholders.) Admin dyndns token is masked in JSON (`"****"`),
never in HTML. The only "injection" is that `window.location.origin` is used client-side
(account page "server URL", catalog manifest URL).

---------------------------------------------------------------------------------------------

## 4. Shared script `/_static/nav.js` (loaded by home, login, register, account, catalog, queue, admin; NOT setup)

Exposes `window.mokuroAuth` and `window.renderMokuroHeaderNav(currentKey)`; auto-runs on
`DOMContentLoaded` for the element `#header-nav[data-current]` (values: home, catalog, queue,
admin, account, login, register).

| Call | Method + URL | Request | Response fields read | Errors |
|---|---|---|---|---|
| signIn(username,password) | `POST /login/api/token` `Content-Type: application/json` | `{username, password, kind:"web", label:"web page"}` | `token` (string, stored), `user` (`{username,role}` stored as JSON). Requires `response.ok && token` | any non-ok/no token -> throws `Error(data.error \|\| 'Authentication failed')`; body parse failure tolerated |
| signOut() | `DELETE /login/api/token` header `Authorization: Bearer <t>` | none | ignored | swallowed; token cleared locally first |
| nav config | `GET /api/nav/config` (no auth) | - | `home_enabled`, `catalog_enabled`, `queue_show_in_nav`, `queue_public_access`, `registration_enabled` (all bool) | on any failure falls back to `{home_enabled:true, catalog_enabled:true, queue_show_in_nav:false, queue_public_access:true, registration_enabled:true}` |

Nav rendering rules (client): Home link if `home_enabled !== false`; Catalog if
`catalog_enabled`; Queue if `queue_show_in_nav && (queue_public_access || signed in)`;
signed-in = token AND `mokuro_user`: role `admin` -> "Admin" link `/_admin`, role `inviter` ->
"Invites" link `/_admin`, plus Account `/account` and a Logout button calling the page's global
`logout()`; else Login `/login` and (if `registration_enabled`) Register `/register`.
Valid roles seen by UI: `registered, uploader, editor, inviter, admin, processor`
(+ `anonymous` server-side).

`/api/nav/config` server logic: `catalog_enabled = catalog.enabled`; `home_enabled = !(catalog.enabled
&& catalog.use_as_homepage)`; `queue_show_in_nav = queue.show_in_nav`; `queue_public_access =
queue.public_access`; `registration_enabled = registration.mode != "disabled"`. Defaults when no
config: home true, catalog true, queue_show false, queue_public true, registration true. Always 200.

---------------------------------------------------------------------------------------------

## 5. Pages

Each page: URL, assets, calls (method, URL+query, body, response fields read, status/error
handling), auth, notes. Every page also loads `/_static/shared.css` (stylesheet, which uses
only an inline `data:` SVG for `url()`) and, except setup, `/_static/nav.js`. No images/fonts/
external hosts are loaded (inline `<svg>` only), except links out to `https://reader.mokuro.app`
(account deep link, catalog open-volume).

### 5.1 Home - `GET /`
Assets: `/_static/shared.css`, `/_home/styles.css`, `/_static/nav.js`, `/_home/home.js`.
Page-level: `updateNav()` -> `renderMokuroHeaderNav('home')`, `logout()` = `mokuroAuth.signOut()`
then `location.reload()`.

| Call | Auth | Response fields read |
|---|---|---|
| `GET /api/stats` (no headers) | none | `total_users`, `total_volumes`, `total_pages_read`, `total_characters_read` (numbers, animated count-up, K/M formatting), `total_reading_time_formatted` (string shown verbatim). Missing -> 0 / "0s". Non-ok or throw -> all stat tiles show `-`. |

Python `GET /api/stats` (always 200): `{total_users (non-deleted users), total_volumes (sum of
library volumes), total_pages_read:0, total_characters_read:0, total_reading_time_seconds:0,
total_reading_time_formatted:"0s", last_updated: <epoch int>}`; DB/index errors degrade to 0.
Also exists (not used by JS): `GET /api/health` -> `{status:"ok"|"degraded", uptime_seconds,
db_status, library_status, total_users, total_volumes, ocr:{backend, worker_alive, pending, failed}|null}`.

### 5.2 Login - `GET /login`
Assets: shared.css, `/login/styles.css`, nav.js, `/login/login.js`. (Defines global `logout()`.)

| Call | Body | Reads | Handling |
|---|---|---|---|
| `POST /login/api/token` (via `mokuroAuth.signIn`) | `{username,password,kind:"web",label:"web page"}` | `token`, `user.{username,role}` | success -> `location = '/'`; failure shows `error` text (e.g. "Invalid credentials" 401, "Too many failed attempts. Retry in {n}s" 429, "Missing credentials" 400), re-enables button |

Server contract `POST /login/api/token`: body JSON `{username,password,kind?,label?}` (also
accepts Basic header if body lacks creds); `kind` in `TOKEN_KINDS` (`web`,`reader`,`processor`;
unknown -> 400 `kind must be one of ...`); rate-limited per `<client-ip>:<username>` (429).
200 `{token, token_type:"Bearer", kind, expires_at, user:{username,role}}`. 401 `{error:"Invalid
credentials"}`. Web token lifetime 7 days.

### 5.3 Register - `GET /register` (also `/register/`)
Assets: shared.css, `/register/styles.css`, nav.js, `/register/register.js`. Defines `logout()`.

| Call | Auth | Reads | Handling |
|---|---|---|---|
| `GET /api/register/config` | none | `mode` (`self`\|`invite`\|`approval`\|`disabled`, default `self`) | non-ok/throw: stay `self`. `disabled` hides form; `invite` shows invite field (`required`) |
| `POST /api/register` JSON `{username, password, invite_code?}` (`invite_code` only sent when mode=invite and non-empty) | none | success = `response.ok`: `status` (`"pending"` -> pending-approval screen, else success screen) | 409 -> "Username already taken" on username field; 400 + `error` -> routed by case-insensitive substring: contains `invite` -> invite field, `username` -> username field, `password` -> password field, else banner; **Rust error texts must keep those words** (see below); 403 -> "Registration is not available."; other -> `error` or generic; network error -> banner |

Python: `GET /api/register` and `GET /api/register/config` both -> `{mode, enabled:(mode!="disabled"),
requires_invite:true (only in invite mode)}`. `POST /api/register`: 403 `Registration is
disabled`; 413; 400 `Invalid JSON body`; username/password validated (`Username is required`,
`Username must be 3-32 characters and contain only letters, numbers, underscores, and hyphens`,
`Password is required`, `Password must be at least 8 characters`, `Password must be at most N
characters`); self/invite -> 201 `{success:true, message:"Registration successful", username}`;
approval -> 201 `{success:true, message:"Registration submitted for approval", username, status:"pending"}`;
invite errors 400 `Invite code is required` / `Invalid or expired invite code`; duplicate -> 409
`Username already exists`. Client-side validation also enforces username `^[a-zA-Z0-9_-]+$` 3-32 and
password >= 8. Methods: `OPTIONS /api/register*` -> 204 `Allow` header; other methods 405.

### 5.4 Account - `GET /account`
Assets: shared.css, `/account/styles.css`, nav.js, `/account/account.js`. Defines `logout()`,
`copyServerUrl()`, `showDeleteModal()`, `hideDeleteModal()`, `deleteAccount()` (inline onclick).
Guard: no token or no `mokuro_user` -> `location = '/login'`. Shows `mokuro_user.username`/`role`
(role badge: admin warning, editor info, else success). Computes reader deep link
`https://reader.mokuro.app/#/cloud?server=<origin>&username=<name>` (no server call).

| Call | Headers | Body | Reads | Handling |
|---|---|---|---|---|
| `GET /login/api/me` | `Authorization: Bearer` | - | `created_at` (-> "Member since" date, `+'Z'`) | non-ok/throw: leave dash |
| `GET /api/account/stats` | Bearer | - | `volumes`, `pages_read`, `characters_read` (numbers, K/M format), `reading_time_formatted` (string) | non-ok/throw: dashes. Python currently returns zeros: `{volumes:0,pages_read:0,characters_read:0,reading_time_seconds:0,reading_time_formatted:"0s"}`; 401 `{error:"Authentication required"}` |
| `POST /api/account/password` | Bearer + `Content-Type: application/json` | `{current_password, new_password}` | `error` on failure; success body ignored | `!response.ok` -> shows `error` text. On success page immediately re-signs-in with `mokuroAuth.signIn(username, newPassword)` (a password change REVOKES ALL the user's tokens, so the old bearer is dead; the new `/login/api/token` call must work) |
| `POST /api/account/delete` | Bearer + JSON | `{password}` | `error` on failure | success -> `mokuroAuth.clear()` + `location = '/'` |

Python statuses: password: 401 `Authentication required` (no/bad token), 401 `Current password
is incorrect`, 400 `Missing required fields` / `Missing request body` / `Invalid request` /
password-validation text; 200 `{success:true}`. delete: 400 `Password confirmation required`, 401
`Password is incorrect`; on success deletes user, audit `self_delete_account`, `rmtree
<storage>/users/<username>`; 200 `{success:true}`. Both endpoints accept Bearer OR Basic
(`authenticate_basic_header`). `OPTIONS /api/account/*` -> 204 `Allow: GET, POST, OPTIONS`.

### 5.5 Setup wizard - `GET /setup`
Assets: shared.css, `/setup/setup.css`, `/setup/setup.js` (no nav.js). Links out to `/_admin`, `/`.
Steps are client-only (`step-welcome`, `step-admin`, `step-registration`, `step-done`).

| Call | Body | Reads | Handling |
|---|---|---|---|
| `GET /setup/api/status` (on load) | - | `needs_setup` (bool). `false` -> `location = '/'` | throw: continue wizard |
| `POST /setup/api/complete` JSON `{admin:{username,password}, registration:{mode}}` where `mode` is the checked radio (`disabled\|self\|invite\|approval`) | - | success = `resp.ok` (201); then it calls `POST /login/api/token` `{username,password,kind:"web",label:"web page"}` and on ok stores `token` + `user` in sessionStorage (`mokuro_token`, `mokuro_user`) | failure: `error` shown on admin step (`data.error \|\| 'Setup failed'`), goes back to step 1; fetch throw: "Connection error: ..." |

Python: `GET /setup/api/status` 200 `{needs_setup}`; 403 `{error:"Setup is only allowed from
localhost"}` when needed and not loopback (loopback = `REMOTE_ADDR` loopback AND forwarded
client IP, if any, loopback). `POST /setup/api/complete`: 403 same; 400 `Setup already completed`;
413; 400 `Empty body` / `Invalid JSON`; username/password validation 400; duplicate admin 409;
success **201** `{success:true, message:"Setup completed successfully"}`; sets `registration.mode`
if valid and persists config. `needs_setup` = no admin user in DB.

### 5.6 Catalog - `GET /catalog`
Assets: shared.css, `/catalog/styles.css`, nav.js, `/catalog/catalog.js`. Defines `showRoot()`,
`saveUserReaderUrl()` (inline handlers), `logout()`. Client routing: hash `#<encoded series name>`
selects a series; uses `history.pushState` (`/catalog`, `/catalog#<name>`), `popstate`,
`hashchange`. **No Authorization header is ever sent by catalog.js; all `/catalog/api/*` endpoints
read-only and public** (when `catalog.enabled`).

| Call | Query | Reads | Handling |
|---|---|---|---|
| `GET /catalog/api/config` | - | `reader_url` | via `res.json()` unconditionally; failure of the batch shows "Error loading catalog: msg" |
| `GET /catalog/api/library` | - | `series[]` (below) | idem (honours `Accept-Encoding: gzip`: Python gzips) |
| `GET /catalog/api/ocr-status` (loaded with the two above, then polled **every 2000 ms**) | - | `active` (bool), `series`, `volume` (strings, compared to current series/volume names), `percent` (number), `eta_seconds` (number, <0 or non-number = unknown), `updated_at` (epoch s; falls back to client receive time) | non-ok at load -> `{active:false}`; poll errors ignored |
| `GET /catalog/api/series?name=<name>` | `name` (required, URL-encoded series folder name) | `volumes[]` (below); body also has `name`,`cover` | non-ok -> "Error: Series not found" |
| `GET /catalog/api/cover?path=<relpath>` (used as `<img src>`) | `path` = value of `cover` field (path relative to library root) | image bytes | broken image if missing |

`series[]` fields read: `name` (folder; key, display fallback, search), `cover` (path or null),
`volume_count`, `titles` (object `{native?,romaji?,english?}`, used by the progression chain +
search), `tag` (string; appended `" (tag)"` after bracket-stripping, only for alt titles),
`community` (`{score (0-100 number), genres[string], tags, source}` - score shown as `★ x.x`,
genres drive chips/search/sort), `latest_volume_modified` (sort "newest"), `total_pages`,
`total_chars` (sort "densest" = chars/pages), `damaged_volumes` (int), `missing_pages` (int;
badge + "Missing pages" filter chip only when some series has `damaged_volumes>0`).
`volumes[]` fields read: `name`, `cover`, `ocr_pending` (bool -> "OCR pending" badge unless that
volume is the active OCR job), `missing_pages`, `page_count` (damage badge title). Client matches
`ocrStatus.series === series.name && ocrStatus.volume === volume.name`.

`GET /catalog/api/ocr-status` in Python = contents of `<storage>/.ocr-progress.json` (the first
running job flattened, plus `jobs:[]`) with `active:true`, else `{"active": false}`; job keys
include `series`, `volume`, `relative_cbz`, `percent`, `eta_seconds`, `updated_at`, `started_at`,
`status`, `done_pages`, `total_pages`.

Open-volume (no fetch; builds a URL and `window.open`): `readerUrl + '/#/upload?' +
URLSearchParams({cbz: <origin>/mokuro-reader/<series>/<volume>.cbz, manifest:
<origin>/catalog/api/manifest?series=<s>&volume=<v>, cover?: <cover path>})`. So the Rust server
must serve `GET /catalog/api/manifest?series=&volume=` (auth like the .cbz; this is a reader
contract) and WebDAV `/mokuro-reader/<series>/<volume>.cbz`. Server config `reader_url` default
`https://reader.mokuro.app`; user override in localStorage wins.

Other Python routes (unused by JS but part of the surface): `GET /catalog/api/series/<name>`,
`GET /catalog/api/cover/<path>`, 404 `{error:"Not found"}` for other `/catalog/api/*`. Series 404
`{error:"Series not found"}`, 403 `Forbidden`, 400 `Missing series name`. Cover: only
`.webp/.jpg/.jpeg/.png`, must be inside library, `Cache-Control: public, max-age=3600`.

### 5.7 Queue - `GET /queue`
Assets: shared.css, `/queue/styles.css`, nav.js, `/queue/queue.js`. Defines `window.logout`.
Behaviour: `GET /queue/api/config` first, then update nav, then (if `!public_access` and no token)
redirect `/login`, else start polling.

| Call | Headers | Reads | Handling |
|---|---|---|---|
| `GET /queue/api/config` | none | `public_access` (bool) (`show_in_nav`, `display` also sent) | any failure: update nav + start polling anyway |
| `GET /queue/api/status` polled: 1000 ms normally, 10 s when tab hidden, backoff `3000*2^(n-1)` ms capped 30 s after failures; `fetch(..., {cache:"no-store"})` | `Authorization: Bearer <t>` if signed in; `If-None-Match: <last ETag>` | see below | **401 -> `location='/login'`** (only when `public_access=false` and no valid viewer); **304 -> no-op (must be bodiless)**; response header `X-Queue-Auth: failed` -> drops stored login (`mokuroAuth.clear()`, ETag reset); other non-ok -> counts a failure; stores `ETag` header; parses JSON |

Python status endpoint rules: 401 `{error:"Authentication required"}` iff `!public_access` and viewer
has no valid role; otherwise 200 JSON `Content-Type: application/json; charset=utf-8`,
headers `ETag: "<level>-<a|v>-<sha256(body)[:20]>"` (`a` admin viewer, `v` otherwise),
`Cache-Control: private, no-cache`, `Vary: Authorization`, optional `X-Queue-Auth: failed|limited`
(bad/expired credentials or rate-limited: served as an anonymous visitor). `If-None-Match` matching
the current ETag -> `304` with the same headers, empty body. Body is `sort_keys` JSON. Config
`queue.display` (`minimal|normal|detailed`, default `normal`) is read live per request and echoed as
`level`. Admin-ness decides which fields exist (admin = role `admin`).

Payload JS reads (full shape in `queue/shape.py`; per level):
- Top (all levels): `level` (string; level change rebuilds the page), `queue_done_at` (ISO or null),
  `pending_count` (int), `processing_hold` (`{reason, since (epoch s), last?:{name, disconnected_at}}`
  or null; text "No processor connected since ..."), `machines[]`, `pending[]`.
- `machines[]`: `name` (key; admin sees real name, others alias "machine N"/"this server"), `state`
  (`running|loading|waiting|idle|held|configuring|standby`), `slots` (int lanes), `jobs[]`, `next[]`
  (`{series,volume,generation}`), `configuring` (`{generation|null, auto(bool)}`), `held`
  (`{reason, until, error?}`), `cannot_start[]` (`{generation, until, error?}`), `label` (admin only).
- `jobs[]` (min): `series`, `volume`, `generation`, `percent`, `eta_at`, `state`. Normal+: `status`
  (`starting|running|finalizing`), `done_pages`, `total_pages`, `eta_seconds`, `startup_seconds`,
  `host_busy`. Detailed+: `engine`, `detector` (shown as "engine . detector" text), `latency_seconds`,
  `startup_rough`, `throughput_pages_per_minute`, `pipeline` (`{verdict, bottleneck, stages[{key,name,
  device, workers, fused, busy_pct, blocked_pct, starved_pct, queue:{name,capacity,mean_depth,max_depth}|null}]}`).
- `pending[]` (min: `series,volume,generation,eta_at,rough`; normal+: `attempts`, `reason`, `returned`
  (`{machine, reason, at, class?, error?, count?}`); detailed adds `engine`,`detector`,...). Min level
  sends only the first 10; others at most 100. JS shows `pending_count - pending.length` as "+N more"
  (minimal).
- Normal+: `pending_thumbnails` (int; section hidden if not a number), `failed[]` (`series, volume,
  generation, attempts, reason`; admin adds `error`, `log_file`, `last_attempt_at`), `failed_count`,
  `skipped_missing_pages[]` (`series, volume, missing_pages, page_count, generations[]`),
  `generations[]` (`{id,name}`, shown as run order).
- Detailed only: `speed[]` (`{generation, pages_per_minute, machines}`).
- Admin only: `backend` (shown as ROCm/CUDA/CPU label), `held_rows[]` (`{generation, reason}`).
- JS never reads `paused_for_benchmark` but the server sends it.

### 5.8 Admin panel - `GET /_admin` (roles `admin`; role `inviter` sees Invites tab only)
Assets: shared.css, `/_admin/styles.css`, nav.js, `/_admin/admin.js`. All API under
`/_admin/api`. Auth: every request `Authorization: Bearer <token>`. Server gate:
`/_admin/api/*` requires role `admin`, except `/_admin/api/invites[/...]` which allows `admin` and
`inviter` (403 `{error:"Admin access required"}` / `"Admin or inviter access required"`;
unauthenticated -> 401 from auth middleware). Page redirects to `/login` if no token or on any 401.
Tabs: Users, Invites, Audit, Settings, Status, Connectivity. Inviters: JS removes every tab but
Invites and never calls users/settings/etc.

`apiError`: 401 -> `mokuroAuth.clear()` + `location='/login'`; else `Error(data.error||'Request failed')`
with `.status` and `.payload` (body). Toasts show `error`.

#### Users tab
| Call | Body | JS reads | Notes |
|---|---|---|---|
| `GET /_admin/api/users` | - | `users[]`: `username`, `role`, `status` (`active\|pending\|disabled\|deleted`), `notes`, `created_at` | Python also sends `id`. Row actions hidden for `deleted`; Approve for `pending`; Disable for `active` |
| `POST /_admin/api/users` | `{username, password, role}` role in `registered\|uploader\|editor\|inviter\|admin\|processor` | `error` | 201 `{success,user}`; 409 `already exists`; 400 validation |
| `PUT /_admin/api/users/<enc username>/role` | `{role}` | `error` | 200 `{success,user}`; 400 `Role is required`/`Invalid role...`; 404 |
| `PUT /_admin/api/users/<enc username>/notes` | `{notes}` (string) | `error` | 200 `{success,user}`; 404 |
| `POST /_admin/api/users/<u>/approve` | `{}` | `error` | 404 `not found or not pending` |
| `POST /_admin/api/users/<u>/disable` | `{}` | `error` | |
| `DELETE /_admin/api/users/<u>` | - | `error` | 200 `{success,message}`; 404 |

#### Invites tab (also used by inviters)
| Call | Body | JS reads |
|---|---|---|
| `GET /_admin/api/invites` | - | `invites[]`: `code`, `role`, `status` (`valid\|expired\|used`), `expires_at`, `used_by`, `invited_by` (also `created_at`) |
| `POST /_admin/api/invites` | `{role, expires}` role in `registered\|uploader\|editor\|inviter`; `expires` in `1h\|1d\|7d\|30d` | `invite.code` (201 `{success,invite:{...}}`); 400 `error` |
| `DELETE /_admin/api/invites/<enc code>` | - | `error`; 200 `{success,message}`; 404 `Invite not found` |

#### Audit tab (lazy, first opened)
`GET /_admin/api/audit?...` query params the JS sets (all optional): `q` (substring), `actor`,
`action` (**comma-separated multi-value allowed**, e.g. the fixed option
`ocr_sidecar_written,ocr_sidecar_rejected`), `target_type`, `since`, `until` (ISO instants
`Date.toISOString()`, since inclusive/until exclusive), `include_progress=1`, `cursor` (previous
`next_cursor`). Server also supports `limit` (default 50, max 200).
JS reads: `events[]` {`id`, `actor_username`, `action`, `target_path`, `target_username`, `details`
(string, shown truncated 160 + title), `created_at` (SQLite UTC text)}, `next_cursor` (string|null;
non-null shows "Load more"), `total` (int; first page only), `facets` {`actors[]`, `actions[]`,
`target_types[]`} (first page only; fills the selects). Errors: 400 `{error}` for bad cursor/limit/date.
UI state is kept in the URL hash `#audit?q=..&actor=..` (client only).

#### Settings tab
| Call | Body | JS reads |
|---|---|---|
| `GET /_admin/api/settings` | - | see below |
| `PUT /_admin/api/settings/registration` | `{mode, default_role, allow_anonymous_browse, allow_anonymous_download}` (the page sends both anon flags equal; `mode` in disabled\|self\|invite\|approval, `default_role` in registered\|uploader\|editor\|inviter) | `error` only |
| `PUT /_admin/api/settings/cors` | `{enabled, allowed_origins:[string]}` | `error` |
| `PUT /_admin/api/settings/catalog` | `{enabled, use_as_homepage, reader_url}` | `error` |
| `PUT /_admin/api/settings/queue` | `{show_in_nav, public_access, display}` display in `minimal\|normal\|detailed` | `error` (400 also has `field:"display"`) |
| `PUT /_admin/api/settings/ocr` | `{poll_interval: int>=1}` | `applied`, `restart_required`, `installing`, `reason` (toast text; `restart_required` -> warning style). 400 `error` |
| `PUT /_admin/api/settings/dyndns` | `{provider, domain, update_url, interval, token?}` (token only when typed; masked `****` never accepted back) | `error` |

`GET /_admin/api/settings` JS reads: `registration.{mode, default_role, allow_anonymous_browse,
allow_anonymous_download, require_login(legacy fallback)}`, `cors.{enabled, allowed_origins[]}`,
`catalog.{enabled, use_as_homepage, reader_url}`, `queue.{show_in_nav, public_access, display}`,
`ocr.{poll_interval, backend}`, `dyndns.{provider, domain, update_url, interval}`, and
`ocr_runtime` = `{available, configured_backend, installed, installed_backend, env_path,
engines_env_path, engines_installed, detector_ready, supported_backends[], cli_hint, driver_hint,
active_generations|generations|active_engines|engines (array of name strings or {name|id}),
local_processing(bool; false => "off here")}`. Python returns the whole `Config.to_dict()` plus
`ocr_runtime` (extra keys server/storage/ssl/admin/database/ocr.generations/... are ignored by JS;
the dyndns token is masked `"****"`). Reader URL presets in HTML: `https://reader.mokuro.app`,
`http://localhost:5173`, `https://mokuro-reader-tan.vercel.app`, else "Custom".

#### Settings > Processors card (polled every 15 s while Settings tab visible)
`GET /_admin/api/processors` (failure silently ignored) JS reads: `processors[]`
{`name`, `local`(bool), `host`{`cpu`,`gpu`}, `installing`, `connected_since`(epoch s), `label`,
`transfer`{`held_until`,`held_error`}, `cannot_start[]`{`generation`,`until`,`error`}},
`speed[]` {`name`, `local`, `connected`, `host`{cpu,gpu}|null, `layers[]`{`generation_id`,
`generation`, `pages_per_minute`, `volumes`, `last_at`, `bench_pages_per_minute`}}, `failed_logins[]`
{`username`,`reason`,`at`}, `last_disconnect`{`name`,`at`}|null, `local_processing`(bool),
`processing_hold`{`since`,`last`{`name`,`disconnected_at`}}|null. With no OCR/processors the
Rust server may return `{processors:[], speed:[], failed_logins:[], last_disconnect:null,
local_processing:<bool>, processing_hold:null}` and the card renders its "does no OCR" hint.

#### Settings > OCR Generations (ENGINE-HEAVY; see s7)
| Call | Body | JS reads |
|---|---|---|
| `GET /_admin/api/ocr/generations` | - | `generations[]`, `catalog`, `processors[]`, `local_processing`, `autobench`, `stats_pending` (below). On failure shows "Could not load generations: msg" and disables Save |
| `PUT /_admin/api/ocr/generations` | `{generations:[<row>...]}` row = `{id?, name, primary, enabled, engine, detector(null if engine owns detection), patch_budget(int\|null), precision?(only if engine takes modes), pools:{stage_workers:{stage:int}, queue_capacity:{stage:int}, stage_device:{stage:"cpu"\|"gpu:N"}}}` | Same body as GET (`adoptGenerations`) + `applied`, `restart_required`, `installing`, `reason`. Error 400 `{error, row:int\|null, field:string\|null}` -> inline on that row (`field` in name/engine/detector/id/...), then toast. 409/others -> toast |
| `GET /_admin/api/ocr/generations/stats` | - | `stats_pending` (true -> poll again every 3 s up to 60 tries), `generations` map id -> `{volumes_done, volumes_total, volumes_skipped, volumes_by_machine{machine:int}}`. Python waits <=5 s |
| `POST /_admin/api/ocr/generations/derive` | `{spec:{engine,detector,patch_budget,precision?,pools}, processor?:<name>}` | `road`, `stages[]` (below). 400 -> stages left alone |
| `PUT /_admin/api/ocr/generations/<id>/pools` | `{processor:<name>, pools:{stage_workers:{stage:int\|"auto"}, queue_capacity:{...}, stage_device:{...}}}` | `pools` (stored form, replaces the machine's pools) |
| `GET/POST/DELETE /_admin/api/ocr/generations/<key>/bench[?processor=<name>]` | POST body `{spec:{...as derive}, processor?, pages?}` -> **202**; GET -> state; DELETE cancels (400 if nothing queued) | bench state, below. A **404 on a real (non-`draft-`) id means "server has no bench endpoints"** (UI then hides all benchmark controls); 404 on a `draft-...` key is quiet; 409 = already queued (shown as note) |

Rust may return a trimmed/empty subset if OCR/benchmarks are dropped (see s7): minimum for a
non-broken page is `GET /ocr/generations` -> `{generations:[], catalog:{...empty lists...},
processors:[], local_processing:bool, autobench:bool, stats_pending:false}` and `GET .../stats`
-> `{stats_pending:false, generations:{}}`; bench endpoints may 404 for real ids (UI degrades).

`generations[]` fields JS reads: `id`, `name`, `primary`, `enabled`, `engine`, `detector`,
`patch_budget`, `precision`, `pools{stage_workers,queue_capacity,stage_device}`, `sidecar`,
`effective_detector`, `detector_locked`, `patch_budget_applies`, `precision_on`
(`{<machine 'local'|name>: {<mode>: {eligible, why, precision, bench:'pending'|'off'|'failed'|'done',
trials[{precision,pages_per_second}]}}}`), `precision_hold` (string|null), `road`, `stages[]`,
`volumes_done`, `volumes_total`, `volumes_skipped`, `volumes_by_machine`, `congestion`
(`{verdict,bottleneck,stages[{key,device,workers,busy_pct,blocked_pct,starved_pct}],
queues[{name,capacity,mean_depth,max_depth}],runs,last_run_at}`|null), `processor_pools`,
`processor_runs`(`{volumes,pages_per_second}`), `local_runs`, `processor_bench`, `processor_congestion`,
`local_pools`, `local_bench`(`{pages_per_second, at}`), `processor_stages{name:stages[]}`, `bench`
(last finished bench summary or null).
`stages[]` fields: `key`, `name`, `device`, `max_workers`, `derived_workers`, `derived_capacity`,
`devices_allowed[]`, `device_options[{id,label}]`, `device_locked_reason`, `workers_means`
(`pool|engine|copies`).
`catalog` fields: `engines[{id,label,monolithic,served,own_environment,own_detector,patch_budget,
precision,precision_modes[],devices}]`, `detectors[{id,label,devices}]`, `devices[{id,label}]`,
`patch_budgets[int]`, `precision_modes[{id,label}]`, `precision_default`, `name_pattern`
(regex source, JS builds a `RegExp`; fallback `^[a-z0-9][a-z0-9-]{0,31}$`), `reserved_names[]`
(`original,gcv,updated-ocr`), `reserved_prefixes[]` (`tr-`). `processors[]` (non-local only used):
`name`, `label`, `local`, `catalog.engines[]` (engine ids installed there).
Bench state JS reads: `state` (`idle|queued|running|done|failed|cancelled`), `position`,
`waiting_for_queue`, `autobench`, `processor`, `progress{trial,max_trials,stage_workers,pages,
pages_done,pass_index,pages_per_second,window_seconds}`, `error`, `started_at`, `finished_at`,
`tunable`, `spec`, `best{pages_per_second,seconds_per_page,speedup,stage_workers,queue_capacity,
stage_device,window_seconds,pages_measured,passes,short_window,same_as_spec,gpu_busy_pct,
cpu_busy_pct}`, `baseline{pages_per_second}`, `trials[{n,pages_per_second,stage_workers,
stage_device,note,verdict,bottleneck,accepted,window_seconds,passes,short_window,gpu_busy_pct,
cpu_busy_pct}]`, `estimates{volume_200_pages_seconds,remaining_seconds,remaining_pages}`,
`startup_seconds`, `peak_vram_mb`, `peak_rss_mb`, `host{cpu,gpu,backend}`, `sample{pages,volumes}`,
`preempted[{volume,generation}]`.

#### Status tab (poll every 30 s while visible)
`GET /_admin/api/status` JS reads: `uptime` (seconds float), `host`, `port`, `user_count`,
`volume_count`, `storage_path`, `disk_total`, `disk_used` (bytes; progress bar only if `disk_total>0`).
(Python also: `disk_free`, `stats:{}`.)

#### Connectivity tab
- `GET /_admin/api/tunnel/status` -> `{running, url|null, available}` (JS: `available` false hides
  controls; `running`, `url`). `POST /_admin/api/tunnel/start` `{}` -> 200 `{success,...status}` (400
  `error`, 500 `Tunnel service not available`); JS re-polls status at 3/8/15 s. `POST .../tunnel/stop`.
- `GET /_admin/api/dyndns/status` -> `{enabled, running, provider, domain, last_update, last_ip,
  last_error}` (JS reads `running, domain, provider, last_update, last_ip, last_error`); then also
  calls `GET /_admin/api/settings` for `dyndns.*`. `POST .../dyndns/start|stop` `{}`; `POST
  .../dyndns/test` `{}` -> `{success, ip?, error?}`.
- Python returns 500 `{error:"<service> not available"}` for start/stop when the service is absent.

---------------------------------------------------------------------------------------------

## 6. Endpoint index (endpoint -> pages that call it)

Public / self-authenticated (outside auth middleware):
| Endpoint | Used by |
|---|---|
| `GET /api/nav/config` | every page via nav.js (not setup) |
| `POST /login/api/token`, `DELETE /login/api/token` | nav.js (login, account[re-login], setup, all pages' logout) |
| `GET /login/api/me` (Bearer) | account (also the reader; contract: `authenticated` always present, `permissions`) |
| `POST /login/api/check` | no page (legacy; body `{username,password}` -> `{success,user}`) |
| `GET /api/stats` | home |
| `GET /api/register/config`, `POST /api/register` | register (`GET /api/register` alias, unused) |
| `GET /api/account/stats`, `POST /api/account/password`, `POST /api/account/delete` | account |
| `GET /setup/api/status`, `POST /setup/api/complete` | setup |
| `GET /catalog/api/config`, `/library`, `/ocr-status`, `/series?name=`, `/cover?path=` | catalog |
| `GET /catalog/api/manifest?series=&volume=` | link built by catalog (opened by the reader) |
| `GET /queue/api/config`, `GET /queue/api/status` | queue |
| `GET /api/health` | no page (probe) |

Admin (`/_admin/api/*`, admin role; invites also inviter):
`GET|POST /users`, `PUT /users/<u>/role`, `PUT /users/<u>/notes`, `POST /users/<u>/approve`,
`POST /users/<u>/disable`, `DELETE /users/<u>` (Users tab); `GET|POST /invites`, `DELETE
/invites/<code>` (Invites); `GET /audit` (Audit); `GET /settings`, `PUT /settings/registration|cors|
catalog|queue|ocr|dyndns` (Settings, Connectivity); `GET /processors`; `GET|PUT /ocr/generations`,
`GET /ocr/generations/stats`, `POST /ocr/generations/derive`, `PUT /ocr/generations/<id>/pools`,
`GET|POST|DELETE /ocr/generations/<key>/bench` (Settings > Generations); `GET /status` (Status);
`GET /tunnel/status`, `POST /tunnel/start|stop`, `GET /dyndns/status`, `POST /dyndns/start|stop|test`
(Connectivity). NOT called by any frontend but exist in Python: `POST /ocr/devices/refresh`.

Static assets: s2 table.

---------------------------------------------------------------------------------------------

## 7. Engine / detector references that must be removed or tolerated

**The JS has NO hard-coded engine or detector ids in logic.** Engines, detectors, devices and
precision modes are all data-driven from `GET /_admin/api/ocr/generations` -> `catalog`. The
only literals:

| Where | What | Action |
|---|---|---|
| `admin/web/index.html` lines 380-395 (Settings > OCR > `<details class="bench">` "Reference: quality and hardware of each setup") | Static table rows naming `mokuro`, `hayai-nova + ppocr-manga`, `paddle-manga + ppocr-manga`, `ppocr-manga`; static hint "Detectors: ppocr-manga (the default), or ctd as a GPL-3.0 opt-in..." plus precision modes prose | Pure display text, harmless if the Rust engines differ, but stale if `mokuro`/`ctd`/`paddle` are dropped. Since HTML is "reused unchanged" these will keep rendering; to remove them the HTML must be edited (decision for the porter). Mentions: mokuro, hayai-nova, paddle-manga, ppocr-manga, ctd, manga-ocr. (`animetext` and `rtdetr` do not appear anywhere in the HTML/JS.) |
| `admin/web/index.html` line 315 | `<dt>mokuro env</dt>` label (shows `ocr_runtime.env_path`) | Label only; keep sending `env_path` (or omit -> "not reported") |
| `admin.js` L1033-1035 comments; L2575-2581 `addGeneration()` | "A new row defaults to an engine of the ENGINES environment... The mokuro engines live in their own environment" -> picks `catalog.engines.find(e => !e.monolithic && !e.own_environment) \|\| catalog.engines[0]` and `detector = engine owns detection ? null : catalog.detectors[0]` | Tolerant: needs `catalog.engines` non-empty to add a row (`engines[0]` undefined -> `engine ? engine.id : ''`, still works but row invalid). Prefer a non-monolithic, non-`own_environment` first engine |
| `admin.js` L1406 comment; `genPrecisionModes` | Uses `catalog.engines[i].precision_modes`; empty array => no precision control (ppocr-manga case) | Data-driven |
| `admin.js` L2287 etc. | `stage.workers_means` values `engine`/`copies` special-case the **mokuro served road** and GPU recognizer copies | Tolerated: Rust may only ever emit `pool` |
| `admin.js` `genOwnsDetection`, `genDetectorLocked`, `genEffectiveDetector`, `genIsMonolithic` | Engine flags `monolithic`, `served`, `own_detector`, `own_environment`, `patch_budget` | Rust catalog can emit `false`/`null` for all |
| `queue/web/queue.js` comments only | `hayai-nova-ppocr`, `paddle-manga` appear in comments/examples, not logic | none |
| `queue.js` `recipeText(job)` | At `detailed`, shows `job.engine` and `job.detector` text | Free strings; `detector` is any id |
| Detector ids the Python catalog offers today | `ppocr-manga` (default, Apache), `ctd` (GPL-3.0 opt-in); `animetext` is in DETECTORS but `DISABLED_DETECTORS` (refused in rows, not offered) | `rtdetr` does NOT exist in 0.5.2 (it is the 0.6 default per project notes): the UI handles any id string, so adding it needs no JS change |
| Engines the Python catalog offers today | `mokuro` (served, mokuro env, `own_environment:true`, precision modes without bf16), `hayai-nova` (patch_budget true), `paddle-manga`, `ppocr-manga` (own detector, cpu only, `precision_modes:[]`) | To drop `mokuro`/`ctd`/`paddle-manga`: just omit from catalog; also reject saved rows naming them with 400 `{error,row,field:"engine"\|"detector"}` and decide migration of existing `config.yaml` rows |

Name-derived behaviour: `genDefaultName` makes a row name `<engine>` (engine owns detection) or
`<engine>-<detector>` truncated to 32, e.g. `hayai-nova-ctd`; names validated with
`catalog.name_pattern`, `reserved_names`, `reserved_prefixes`.
Sidecar naming displayed: `<Volume><row.sidecar_suffix>` via `sidecar` field (e.g. `Volume.hayai-nova.mokuro`).

---------------------------------------------------------------------------------------------

## 8. File inventory (bytes, from `wc -c`; total 518,588)

| File | Bytes |
|---|---|
| `static/shared.css` | 18,920 |
| `static/nav.js` | 4,721 |
| `home/web/index.html` / `home.js` / `styles.css` | 7,099 / 3,331 / 5,413 |
| `login/web/index.html` / `login.js` / `styles.css` | 2,992 / 1,430 / 1,451 |
| `registration/web/register.html` / `register.js` / `styles.css` | 5,600 / 8,141 / 3,212 |
| `account/web/index.html` / `account.js` / `styles.css` | 7,649 / 7,599 / 2,102 |
| `setup/web/index.html` / `setup.js` / `setup.css` | 6,588 / 4,232 / 2,675 |
| `catalog/web/index.html` / `catalog.js` / `styles.css` | 3,648 / 27,781 / 7,367 |
| `queue/web/index.html` / `queue.js` / `styles.css` | 6,084 / 54,595 / 31,225 |
| `admin/web/index.html` / `admin.js` / `styles.css` | 45,517 / 203,126 / 46,090 |

27 files, no images/fonts/binary assets (all icons inline SVG; one `data:` SVG in shared.css).
Everything in `web/` dirs is text (HTML/JS/CSS) and can be embedded with `include_dir`/`rust-embed`.
No `.map`, no minification. No file is generated at build time.

---------------------------------------------------------------------------------------------

## 9. Port gotchas (things the frontend silently depends on)

1. All JSON error bodies must be JSON (s1.3). Status codes that change UI behaviour: login/token
   401/429; register 409 and 400 (+word-matching on `invite`/`username`/`password`), 403; queue 401,
   304, `X-Queue-Auth`; admin 401 (-> login redirect), 404 on `/ocr/generations/<id>/bench`.
2. `POST /setup/api/complete` must return `resp.ok` (201 ok). `GET /setup/api/status` is polled
   unauthenticated and must stay reachable from loopback.
3. A password change must invalidate all of the user's bearer tokens (account.js depends on
   re-signing-in after `POST /api/account/password`); the JS re-signs-in itself, so the new token
   endpoint must succeed with the new password immediately.
4. Queue ETag/304 semantics (strong ETag, body-derived, per (level, admin)); `Vary: Authorization`;
   viewer failure never 401s unless `public_access=false`.
5. Queue and catalog polling hit hard (1 Hz / 0.5 Hz): keep them cheap.
6. Catalog serves SPA fallback (any unknown `/catalog/<x>` -> index.html with 200); so does
   `/_admin/<x>`. Other modules 404.
7. `Accept: text/html` vs WebDAV clients decide `/`; keep the same heuristic or `/` breaks DAV
   clients (PROPFIND on `/` must not return the home page).
8. `catalog.enabled` and `queue.display` and `queue.public_access` are read live per request;
   settings PUTs take effect without restart (also `registration.*`, `cors.*`).
9. Catalog `/library` is ungated public listing; `cover` values are library-relative paths
   (series folder / file) fed back to `/catalog/api/cover?path=`.
10. The Python `GET /api/account/stats` returns constants; `GET /api/stats` returns 0 for pages/
    chars/time. Keeping that is acceptable (the UI just shows 0).

# Desktop GUI: one web interface, local pages and the tray (0.7)

**Status:** built; describes the code of 0.7 (released as 0.7.0; first designed 2026-10-04,
reshaped in 0.7.0-beta.4). **Owner decisions:** browser UI + native tray; a library server is
set up and run from **its own web pages**, the same on the desktop, from another computer
and in Docker; the desktop app keeps only what is local (the first-launch chooser and a
processor's pages); *everything that is a flag in the CLI* has a home in a page, the tray
or a "CLI-only because …" row (GUI-COVERAGE.md); tray pause offers both "after this
volume" and "now" (plus timed pauses); tray on Windows, macOS and Linux.

## 1. Pieces

```
 mokuro-bunko tray (same program)            browser (default browser)
   ├─ menu: version, status, stats,            ├─ the library server's own pages, any machine:
   │   pause/resume, Open admin panel,         │     /setup (first run), /_admin (This server, …)
   │   Open processor, Open library,           └─ local pages, this machine only (127.0.0.1):
   │   Set up…, Start at login, Quit                 /app/ chooser, /app/setup/processor,
   └─ talks to ──► local control API  ◄───────────── /app/settings, /app/dashboard
                    (127.0.0.1, token)               (served by the control listener)
                         │
          mokuro-bunko serve  |  mokuro-bunko processor serve  |  mokuro-bunko gui
```

- **The library server's own pages** (`bunko-server`), the same locally, remotely and in
  Docker:
  - `/setup`, the first-run wizard: admin account, who may join, remote access (tunnel,
    Dynamic DNS, HTTPS, other web apps' CORS origins), OCR on this machine (a toggle, on
    when a usable GPU is found; backend Auto, CPU or the GPU found). `POST
    /setup/api/complete` takes `{admin, registration, remote, ocr}`; every answer is
    checked before the admin is made. Values the environment sets are shown read-only
    (`GET /setup/api/options` `pinned`, and the OCR step's `backend_locked`). Every
    `/setup` route, the reads included, answers only while no admin exists and only past
    the setup gate: localhost, the **setup code**, or a setup session the code earned.
    While no admin exists the server logs a new one-time code at every start (`First
    run: create the admin account at http://…/setup (setup code: XXXXX-XXXXX)`); from
    another address `/setup` asks for it (5 tries per address, one more every 12 s;
    30 wrong codes from everyone within a minute replace the code, logged anew, so a
    flood never locks the owner out), or a script sends `X-Setup-Code`. The code lives
    in memory and dies once an admin exists. Certificate files may be named only from
    the server itself (elsewhere: the self-signed certificate). `options` answers 410
    once an admin exists. Finishing signs the admin in and opens `/_admin#server`;
    HTTPS turned on restarts the server. Until an admin exists a new server does not
    start the OCR install by itself; the setup's choice does (when
    `MOKURO_OCR_AUTO_INSTALL` allows).
  - **Admin panel → This server** (`/_admin/api/machine/*`, admin only, CSRF-checked;
    `bunko_server::machine::Machine`, implemented by the binary's `server_machine.rs`):
    OCR on/off (`ocr.local_processing`), the hardware, the backend in use, the
    preference (`ocr.backend`; locked when `MOKURO_OCR_BACKEND` or `serve --ocr` sets
    it), install / reinstall / remove with live progress (the background installer),
    engines and models (download, verify, details), `doctor`, the server log. A backend
    switch installs what is missing; when another backend is already loaded in the
    process the server restarts to use it. Installs come only from the signed release
    or the `ocr-offline` folder next to the program; no page names a folder. One
    install at a time; 3 s between starts of an action (429).
- **Local control API** (in the existing binary): every long-running instance — `serve`
  (library server, incl. its local OCR), `processor serve`, and `gui` — listens on
  `127.0.0.1:<ephemeral>` and writes `<storage>/.control.json`
  `{role, pid, port, token, version, started_at}` (mode 0600; removed on clean exit). Requests
  carry `Authorization: Bearer <token>`. The token never goes into a URL (a URL ends up in
  the browser launcher's command line, which other local users can read, in the history and
  in terminal output): the browser gets a one-time `/app/login?c=<code>`, a single-use code
  valid for 5 minutes that a token holder asks for (`POST /control/login-code`), and the
  login sets an HttpOnly, SameSite=Strict cookie scoped to the loopback origin. Never bound
  to a non-loopback address. The tray discovers instances through the default storage dirs
  (server and processor) plus `MOKURO_STORAGE` / `processor.yaml`, and trusts a
  `.control.json` only when (on Unix) it is a regular file owned by the user with no
  group/other permission bits.
- **The local pages** (`/app/...`, `crates/mokuro-bunko/src/gui/`), mounted on every
  control listener: the first-launch **chooser** (`/app/`: **Library server** — pick the
  folder; a config is written with port 8080 or the next free one, the tray runs `serve`
  and the browser goes to its `/setup`, or to its admin panel when it is set up already;
  **Processor** — the pairing page), the processor's **pairing** (`/app/setup/processor`:
  the library and a tested login, this machine's name, sessions, working folder and
  automatic updates, the OCR choices; saving hands the processor to the tray and waits
  for it to connect), its **status** (`/app/dashboard`) and **settings** (`/app/settings`:
  processor, OCR backend & models with the install page `/app/setup/ocr`, logs,
  diagnostics, updates). A library server has no local settings pages: its setup and
  settings are its own pages above.
- **`mokuro-bunko gui`**: the control API + local pages with nothing running yet (first
  run, or setting up a stopped machine), opens the browser at its one-time sign-in link,
  and exits a few seconds after the chooser or the pairing page hands the browser on to
  a started instance (`/app/api/handoff`), or when no page has talked to it for 10 min.
  The tray runs it for "Set up…" and on a first run when nothing answers.
- **`mokuro-bunko tray`** (crate `bunko-tray`, in the program since 0.7.0-beta.3; a separate
  `mokuro-bunko-tray` executable before): on Linux a StatusNotifierItem + dbusmenu on the
  session D-Bus (ksni on zbus, pure Rust: no GTK or AppIndicator library, so the full
  binary still runs headless and in Docker); on Windows and macOS `tray-icon` + `muda` on a
  `tao` event loop. `mokuro-bunko` with no arguments starts the app: in a desktop session
  the tray (nothing set up: the chooser in the browser), headless what is configured
  (`serve`, `processor serve`, both) or `setup` (PACKAGING.md §1). Windows ships the program
  twice from one build: `mokuro-bunko.exe` (the app, GUI subsystem: no console window) and
  `mokuro-bunko-cli.exe` (console, for terminals). On macOS the app's main program is
  `mokuro-bunko`. The lite build has no tray.

## 2. Control API (contract — streams G2/G3 code against this)

All JSON. `GET /control/status`:

```json
{ "role": "processor|server|gui", "version": "0.7.0", "name": "beast",
  "state": "working|idle|installing|paused|pausing|connecting|disconnected|error|setup",
  "pause": {"mode": "after_volume|now|null", "until": "ISO-8601|null", "reason": "user|schedule|null",
            "since": "ISO-8601|null"},
  "library": {"url": "...", "connected": true, "queue_pending": 12, "error": "...|null"},
  "current": [{"volume": "Series/Vol 01", "engine": "hayai-nova", "precision": "bf16",
               "device": "gpu:0 RTX 4090", "pages_done": 37, "pages_total": 196,
               "pages_per_second": 4.2, "eta_seconds": 38}],
  "stats": {"today": {"volumes": 3, "pages": 512, "busy_seconds": 900},
            "total": {"volumes": 41, "pages": 7310},
            "rate_pages_per_minute": 252, "gpu_busy_percent": 71, "cpu_cores_busy": 6.5},
  "backend": {"pack": "torch-cu130-2.13.0", "devices": ["gpu:0 RTX 4090 sm_89"]},
  "problems": [{"severity": "warn|fail", "text": "...", "hint": "..."}],
  "urls": {"dashboard": "/app/dashboard", "library": "https://...", "logs_dir": "/path"},
  "managed": false, "can_pause": true,
  "update": {"state": "idle|available|waiting|downloading|installing|restarting|updated|failed|blocked",
             "version": "...", "from": "...", "message": "...", "auto": false, "since": "ISO-8601"},
  "install": {"state": "running|done|failed|missing", "stage": "checking|waiting|downloading|unpacking|libraries|models|done|failed",
              "percent": 42, "done_bytes": 0, "total_bytes": 0, "variant": "cu130", "pack": "...",
              "message": "...", "needs_owner": false, "action": "..."} }
```

(`update` and `install` are null when the instance has nothing to say: §4.)

- `POST /control/pause {"mode": "after_volume"|"now", "until": ISO-8601|null}` → 200 status.
- `POST /control/resume` → 200 status.
- `GET /control/events` (Server-Sent Events): a `status` event on every change, at most 2/s;
  the tray and dashboard use it (polling `status` every 2 s is the fallback).
- `POST /control/stop` (managed instances only: the tray's Quit).
- `POST /control/ocr-install`: start (or retry) the background OCR backend install
  (202 `{installing, status}`; 409 when the instance has none).
- `/app/...` serves the local pages (§1); `/app/api/...` their backend: the chooser's
  server start, the processor's pairing, test and settings, hardware, logs, and long
  work (install-ocr, models, doctor, update apply, …) run as this executable in a child
  process and streamed over SSE (`gui/jobs.rs`).

**As built (stream G1; additions to the contract above, all backward compatible):**

- Crate `bunko-control` (`crates/bunko-control`): `types` (the JSON above, `.control.json`,
  `read_control_file`, `read_private_file`; no features, what the tray links with
  `default-features = false`),
  `pause`, `activity`, `control::Control`, `http::{ControlListener, ControlServer}`.
  Integration: `Control::new(ControlConfig::new(role, name, VERSION, &storage))` →
  `ControlListener::bind()` → `listener.serve(control, Some(gui::app(role, listener.token(),
  listener.login_codes(), config_path, processor_config)))` → `server.shutdown().await` on
  exit. `serve` and
  `processor serve` do this through `crates/mokuro-bunko/src/control.rs`.
- Extra status fields: `pause.since` (when the pause began), `library.error` (why a
  processor is not connected: login refused, unreachable), `managed` (stop allowed),
  `can_pause` (false on the `gui` role and on a server that reads no OCR itself — lite build,
  `local_processing: false`; `POST /control/pause` there is 409). `state` is `error` when the
  library refused the processor's login. `stats.today.busy_seconds` sums the processing time of
  the volumes finished today; `today` resets at local midnight; counts persist in `<storage>/.stats.json`.
  `gpu_busy_percent` / `cpu_cores_busy` come from the bench utilisation sampler (sampling
  every 5 s, started on the first status read); `problems` are the doctor's WARN/FAIL rows
  (backend pack, disk, failing volumes), re-run every 2 min.
- `POST /control/pause` body also takes `"reason": "user"|"schedule"` (default user); a past
  `until` or a malformed one is 400. Pausing while paused keeps `since` and takes the new
  mode/until. Times come back whole-second RFC 3339 UTC.
- `GET /control/events`: `event: status`, `data: <status JSON>`; one at connect, then on
  change (at most one per 500 ms, identical statuses not repeated), and a refresh every 5 s
  (rates, ETA); the stream ends when the instance shuts down.
- `POST /control/stop`: 202 and the instance shuts down cleanly when it was started with
  `MOKURO_CONTROL_MANAGED=1` (the tray sets it on what it starts); 409 otherwise.
- `POST /control/login-code` → `{"code": "<32 hex>", "expires_in": 300}`: a sign-in code
  for `GET /app/login?c=<code>&next=/app/...`. Bearer only (the cookie is refused: 401, so a
  page cannot mint codes). A code works once, for 5 minutes; at most 64 wait (minting more
  drops the oldest). `/app/login` uses it up, sets the cookie `bunko_control_<port>` (value:
  the token; HttpOnly, SameSite=Strict, Path=/) and redirects 303 to `next` (under `/app/`
  only); a wrong, used or expired code is 401. `?t=<token>` is not accepted.
  `ControlServer::login_url` mints a code in-process (what `gui` opens); the tray asks for
  one and opens the link; the app's "open its dashboard" link for another instance is
  `/app/api/instances/{server|processor}/dashboard`, which asks that instance for a code
  with its bearer token and redirects to its login, so its token never reaches the browser.
- Auth: `Authorization: Bearer <token>`, or the login cookie (`bunko_control`, or
  `bunko_control_<port>` as `/app/login` sets it); the `Host` header must be a loopback
  name (DNS-rebinding guard) and a cookie-authenticated POST must carry our own `Origin`
  (CSRF). Wrong/missing token: 401; foreign Host/Origin: 403.
- `.control.json` also carries `url` (`http://127.0.0.1:<port>`) and `managed`. Readers
  build addresses from `port` (always `http://127.0.0.1:<port>`), never from `url`, and
  ignore a file that is a symlink, not owned by the user, or group/other-accessible (Unix).
  `gui` writes its file into the server's storage, else the processor's, else (both taken)
  a private per-user directory: `$XDG_RUNTIME_DIR/mokuro-bunko-gui` on Linux when set,
  otherwise `<local data dir>/mokuro-bunko/gui`, created 0700 (never the shared temp dir). One live
  owner per storage: a second instance does not overwrite a live one's file (it runs without
  the control API and says so in its log), except that `serve` / `processor serve` take over
  from a `gui` (setup) instance. `processor serve` only starts it when it holds the storage
  lock.
- `MOKURO_CONTROL=off` disables the listener on `serve` / `processor serve`.
- Admin `/_admin/api/processors`: each machine gains `"pause": {"paused": true, "until":
  "...|null", "reason": "...|null"}` or `null`; the admin panel shows "paused until HH:MM".

## 3. Pause semantics (processor and server-local OCR)

- **after_volume**: stop claiming new volumes/sessions; running volumes finish and upload.
  State `pausing` until idle, then `paused`.
- **now**: cancel running sessions locally; tell the library the claims are released so the
  volumes requeue at once (not after lease expiry); state `paused` within seconds.
- **until**: optional; the pause lifts itself at that time (tray presets: 1 h, until tomorrow
  08:00 local). Pause state persists across restarts (`<storage>/.pause.json`).
- **Protocol v3 additions** (both 0.7, additive): processor → library event
  `Availability {paused: bool, until: Option<String>, reason}` (the scheduler stops offering
  work to a paused processor and the admin panel's processor list shows "paused until …");
  processor → library `Released {claims: [...]}` (requeue now, no failure recorded). Library
  → processor nothing new. An admin can see but not override a processor's own pause.
  As built: wire forms `{"event":"availability","paused":true,"until":...,"reason":...}` and
  `{"event":"released","claims":[...]}`; `RegisterRequest.availability` (optional) lets a
  processor that restarts paused say so before it is offered anything. While paused a
  processor answers a `volume` op with `released` and an `open_session` with `exit`, and a
  bench with a fatal "paused by its owner" (the server also refuses to queue a bench on a
  paused machine: 409). A session that ends because of the pause is not blamed (no strike,
  no start-failure backoff).
- Library server with local OCR: the same pause applies to its in-process processor.

## 4. Setup and settings: where each thing lives

Flows (the chooser picks a role first; a machine can be both):

1. **Library server**: the chooser asks for the library folder and starts the server
   (port 8080 or the next free one; the tray runs it from then on), and the browser
   moves to the server's own `/setup` (§1): admin account, who may join, remote access
   (tunnel / Dynamic DNS / HTTPS / CORS: the `ssl`, `tunnel`, `dyndns`, `config
   cors-add` commands), OCR on this machine. From then on everything is in the admin
   panel: Settings, Status (→ Updates), Connectivity, This server. Docker, a NAS or a
   headless box start at `/setup` the same way, with the setup code.
2. **Processor**: the pairing page: library URL, login (user/password), connection test,
   name and public name, volumes at once, archive RAM, working folder, the library's
   certificate (TLS verify), the opt-in "Update this processor automatically when its
   library updates" (unchecked; `processor.auto_update`, CLI `--auto-update`) — today's
   `processor setup` — and the OCR choices (pack, a folder to install from, no models).
   Saving writes processor.yaml, hands the processor to the tray and waits for it to
   connect; its OCR backend installs in the background meanwhile.
3. **Starting with the machine**: the tray's **Start at login** checkbox (the tray then
   starts what `tray.json` lists). No page installs a service: `processor service
   --install`, `install.sh --systemd` / `--autostart` and `install.ps1 -Startup` stay
   commands.

**Settings** cover every CLI command and flag. The mapping lives in
`docs/rust-port/GUI-COVERAGE.md` (command/flag → the server's pages, a local page, a tray
item, or an explicit "CLI-only because …"), and a test (`gui::coverage`) walks the clap
command tree and fails if a flag has no row, or a row links a page, admin tab or tray item
that does not exist. What concerns the library server is on its own pages (setup, the
admin panel's tabs including This server); what is local to a processor is on
`/app/settings` (processor.yaml, OCR pack and models, logs, doctor, updates). Styling
reuses `web/_static/shared.css`; no front-end framework or build step (plain JS like the
existing pages).

**Automatic updates** are opt-in and off by default. The library server's `update.auto`
is the admin panel's Updates card switch (`POST /_admin/api/update/settings`); an install
that cannot update itself (Docker, a package manager) shows one line saying how to update
instead. The processor's `processor.auto_update` (in processor.yaml) is a checkbox on the
pairing page, in Settings → Processor and in Settings → Updates. `update.public_key` is
never settable from a page. The dashboard shows `status.update` as one line: "Updating to
X…" (waiting, downloading, installing, restarting), "Updated to X (from Y)", or "Update
to X failed: … — will retry"; a `blocked` update (needs the owner) is a problem in the
Problems panel instead.

**OCR backend install** (`status.install`, 0.7): `serve` and `processor serve` install a
missing OCR backend in the background (`crates/mokuro-bunko/src/ocr_install.rs`). The
dashboard's "OCR backend" panel shows the stage, a progress bar and the bytes while it
runs, and on a failure (or `missing`: automatic installs off) the message with a Retry /
Install button (`POST /control/ocr-install`); the admin panel shows the same in This
server and in Settings → OCR → Processors. While it runs, an instance that would
otherwise be `idle` has `state: "installing"` (a connection problem, a pause or a
running volume still win): the dashboard's state reads "Installing OCR backend", the
tray's status line "Installing OCR backend: NN%" (under another state it is a line of
its own), and the doctor's "no backend pack" problem is left to the install. A failure
is a `fail` problem (`kind: "ocr-install"`), so the icon asks for attention. The
server's setup applies its OCR step's choice itself; the processor's pairing page saves
its OCR choices (`<backends>/.install-request.json`, used by the next background
install) and starts the processor; as soon as it serves, the browser moves to its
dashboard, signed in through a single-use code from its control API, and the `gui`
process exits: from then on the processor's dashboard (serving, the install's progress,
"OCR ready", the first-run benchmark as `library.activity`) and the tray report on it.
When a tray runs, what the chooser or the pairing page starts is handed to it
(tray.json; a running tray picks up added roles, and adopts an instance started for it
with `MOKURO_CONTROL_MANAGED=1`), so the tray's Quit stops it.

## 5. Tray

Menu (state-dependent), as built in `bunko-tray`'s `model.rs` and `app/{sni,native}.rs`:
- A greyed "Mokuro Bunko <version>" line.
- Status line(s): role + state (e.g. "Working: Dr Stone 01 · 37/196 · 4.2 p/s",
  "Paused until 18:00", "Idle — 0 in queue", "Can't reach library"), plus "Installing
  OCR backend: NN%" while the background install runs.
- **Statistics ▸** today (volumes, pages), total, current rate, GPU busy, CPU, backend.
- **Pause after this volume**, **Pause now**, **Pause for 1 hour**, **Pause until tomorrow
  08:00**, **Resume** (only the applicable ones enabled).
- **Open admin panel** (`<library>/_admin`; the login page brings the admin back to it),
  **Open processor** (shown while a processor answers: its status page, signed in),
  **Open library** (the server's URL), **Set up…** (only while nothing runs and the tray
  runs nothing: the chooser).
- **Start at login** (checkbox), **Quit** (stops instances the tray started; a
  system-service instance keeps running and the item says so).
Icon states: idle, working, paused, attention (problem).
Supervision: the tray starts `processor serve`/`serve` as a child when `tray.json` lists
the role and no instance is up; restarts it on crash with backoff; never starts a second
instance when a service-managed one is running.

**As built (stream G3, `crates/bunko-tray`; since 0.7.0-beta.3 run as `mokuro-bunko tray`):**
- Discovery: `.control.json` (read with the same lenient types as `bunko-control`'s wire
  types; `tests/contract.rs` checks them against the real ones) in, in order: `--storage DIR`
  (repeatable), portable `data\`, `$MOKURO_STORAGE`, the storage of `$MOKURO_CONFIG` / the
  default `config.yaml` / the configs named by service files (systemd units, launchd plists,
  Windows Startup `.cmd`), the server default storage, the processor configs' storage, the
  processor default storage, `gui`'s private fallback (`$XDG_RUNTIME_DIR/mokuro-bunko-gui`
  on Linux, else `<data>/mokuro-bunko/gui`), and (Linux) `/var/lib/mokuro-bunko/storage`.
  A control file that is a symlink, not the user's, or group/other-accessible is ignored.
  Rescanned every 3 s; a control file counts once `GET /control/status` answers. Each
  instance is followed by SSE, else polled every 2 s; 3 failures in a row drop it.
- **Tray-managed running is `tray.json`** next to `config.yaml` (`~/.config/mokuro-bunko/`,
  `%LOCALAPPDATA%\mokuro-bunko\`; portable `data\tray.json`):
  `{"managed":[{"role":"server"|"processor","args":[...]}],"notifications":false}`. Without
  `args`: `serve`, or `processor serve --config <default processor.yaml>`. `install.ps1`
  writes `{"managed":[{"role":"server"}]}`. The tray starts its instances with the
  tray's own program (on Windows the app `mokuro-bunko.exe`, so no console window). The
  chooser's Library server and the processor's pairing page write their role's entry
  (the server's without `args` for the default config, else `["-c", <config>, "serve"]`;
  the processor's with `--config <its processor.yaml>`) and start the tray when none
  runs; a running tray picks up an added role. The tray's **Start at login** checkbox
  writes its login item (below). With no `tray.json`, no instance and no config at all,
  the tray opens the chooser once (`gui --open /app/`).
- Supervision: children get `MOKURO_CONTROL_MANAGED=1` (so `POST /control/stop` is allowed) and
  `MOKURO_LAUNCHER=tray` (after an update, on Windows the child exits 75 → restarted at
  once, never counted as a crash; on Linux/macOS it execs the new program in place, same
  pid, verified with a tray-run server 0.7.0-alpha.1 → alpha.2 and a rollback); console output goes to
  `<logs>/tray-<role>-console.log`. Backoff 1, 2, 4 … 60 s, reset after 60 s up. A role is
  not started while an instance of it answers or `systemctl [--user] is-active` /
  `launchctl print gui/<uid>/<label>` says its service runs. Quit: `POST /control/stop`, then
  SIGTERM, then kill after 30 s. Linux children get `PR_SET_PDEATHSIG(SIGTERM)`, so a tray
  that dies does not leave an unowned processor running.
- Menu as above; a `fail` problem puts the first one on its own line ("✖ …") and turns the
  icon to attention. Pause items follow `can_pause`; the timed pauses send `mode: "now"` with
  `until`. Open admin panel and Open library open the server's own URLs (the admin signs
  in there). Open processor opens the processor's `/app/dashboard` and Set up… the chooser
  `/app/`, each through an instance's control host (`POST /control/login-code`, then
  `/app/login?c=<code>&next=…`), else `mokuro-bunko gui --open <page>`.
- One tray per user (`.tray.lock` in the log folder). Logs: `<server storage>/logs/
  mokuro-bunko-tray.<date>.log` (portable `data\logs`; falls back to
  `<config>/mokuro-bunko/logs`, then the temp folder, when that cannot be created).
  Start at login: `~/.config/autostart/mokuro-bunko-tray.desktop` (`Exec=<mokuro-bunko> tray`),
  LaunchAgent `io.github.gnathonic.mokuro-bunko-tray` (the app's `mokuro-bunko`, `tray`), or
  Startup `Mokuro Bunko.lnk` (→ `mokuro-bunko.exe`).
- Linux: the icon is a pixmap per state (idle, working, paused, attention with "!"), and
  the SNI status is `NeedsAttention` with the attention icon; a left click opens the menu.
  Started before the panel (at login) it waits for the StatusNotifierWatcher. SIGTERM
  quits it as the menu's Quit does (its supervised instances stop cleanly).
- Automatic updates (opt-in, `status.update` / `problems[].kind == "update"`):
  - Status lines: `waiting`/`downloading`/`installing`/`restarting` show "Updating to X…"
  (waiting: "… (after the running volume)") in place of the state; `updated` adds
  "Updated to X" for 15 minutes after `since`; `failed` adds "⚠ Update to X failed — will
  retry". `fail` problems keep driving the attention icon and the "✖ …" line.
  - Restart grace: the tray remembers each role's last answered status. If the instance
  then stops answering, leaves discovery or its supervised slot is restarting (Windows
  exit 75; on Unix the instance exec()s and its control port/token change) while that
  status said `installing`/`restarting` less than 3 minutes ago, the menu says "Updating
  to X…" with no "not responding", no "stopped (exit code 75)" and no attention icon.
  After 3 minutes it is treated as an ordinary failure again. (`autoupdate::restart_grace`.)
  - Notifications: a `fail` problem of kind `update` raises ONE desktop notification
  ("Mokuro Bunko needs you", body = text + newline + hint) per distinct (role, text); it
  is repeated only if the problem goes away and comes back. These are "action needed"
  alarms and fire regardless of `tray.json`'s `notifications`. Linux: freedesktop
  `Notifications.Notify` through `gdbus call --session` (critical urgency; arguments are
  separate argv entries, GVariant-quoted), else `notify-send -u critical`, else a warning
  in the log. macOS: `osascript display notification`. Windows: a toast through
  `powershell -NoProfile -NonInteractive -WindowStyle Hidden` (Windows.UI.Notifications,
  the PowerShell AppUserModelID, no console window) -- **unverified on a real Windows
  machine**. Spawned off the UI thread; `launch::notify` (rate-limited, macOS only) is
  unchanged.
- Other notifications (library unreachable, OCR failure, update available; the
  `notifications` switch): not built. The menu has no update check of its own: the
  admin panel's Updates card and the processor's Settings → Updates have it.

## 6. Packaging

- Windows zip/installer: `mokuro-bunko.exe` (the app, GUI subsystem) and
  `mokuro-bunko-cli.exe` (console); the Start-menu "Mokuro Bunko" shortcut and
  `install.ps1 -Startup` start `mokuro-bunko.exe`.
- macOS: `Mokuro Bunko.app` (LSUIElement agent) whose main program is `mokuro-bunko`,
  sealed ad hoc; not notarized (quarantine note as today); the LaunchAgent starts its tray.
- Linux x86_64 full: `.desktop` entry + autostart entry (`mokuro-bunko tray`) + hicolor
  icons; `doctor` checks the session bus and a StatusNotifier host (GNOME: the
  AppIndicator extension).
- Docker: no tray (unchanged). Licence notices updated for the new crates.

As built: PACKAGING.md §1 "Desktop tray" (the tray is the `tray` feature of the program
since 0.7.0-beta.3: the archive layout per target, the Windows app + cli pair, the `.app`
bundle, xtask `--no-tray` for the lite and Docker builds, licences, install scripts, and
what `mokuro-bunko update` replaces besides the program).

## 7. Streams and gates (how it was first built, 2026-10-04)

The first build had a `/app` setup wizard for the library server too, a start-up step and
a service install page; 0.7.0-beta.4 moved the server's setup and settings to its own
pages (§1) and dropped the rest.

| stream | owns | gate |
|---|---|---|
| **G1 control** | control API + `.control.json` in serve/processor/gui, pause state machine + persistence, protocol `Availability`/`Released` + scheduler + admin processor list, stats aggregation | unit + integration tests (pause after/now/until, requeue without failure, restart keeps pause, token required, loopback only) |
| **G2 web app** | `mokuro-bunko gui`, `/app` wizard + settings + dashboard pages and `/app/api`, GUI-COVERAGE.md + coverage test, install progress | Playwright end-to-end on the desktop for each flow (library, processor, OCR install, autostart) against real binaries; coverage test green |
| **G3 tray + packaging** | `crates/bunko-tray`, icons, menus, supervision, autostart, packaging in xtask/scripts/deploy | runs on Linux (KDE/GNOME+AppIndicator), Windows (pimax), macOS (mac): screenshots of each menu state, pause/resume round trip against a real processor |

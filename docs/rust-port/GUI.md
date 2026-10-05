# Desktop GUI: setup wizard, settings, dashboards and tray (0.7)

**Status:** design, 2026-10-04. **Owner decisions:** browser UI + native tray; the wizard
covers library first run, processor pairing, OCR install with progress, start-with-machine,
and *everything that is a flag in the CLI*; tray pause offers both "after this volume" and
"now" (plus timed pauses); tray on Windows, macOS and Linux.

## 1. Pieces

```
 mokuro-bunko-tray (desktop binary)          browser (default browser)
   ├─ menu: status, stats, pause/resume,       ├─ wizard  /app/setup/...
   │   open dashboard/library/settings/logs    ├─ settings /app/settings/...
   ├─ start-at-login, quit                     └─ dashboard /app/dashboard
   └─ talks to ──► local control API  ◄──────────── same origin (served by it)
                    (127.0.0.1, token)
                         │
          mokuro-bunko serve  |  mokuro-bunko processor serve  |  mokuro-bunko gui
```

- **Local control API** (new, in the existing binary): every long-running instance — `serve`
  (library server, incl. its local OCR), `processor serve`, and the new `gui` command — listens
  on `127.0.0.1:<ephemeral>` and writes `<storage>/.control.json`
  `{role, pid, port, token, version, started_at}` (mode 0600; removed on clean exit). Requests
  carry `Authorization: Bearer <token>`; the browser gets a one-time `/app/login?t=<token>`
  that sets an HttpOnly, SameSite=Strict cookie scoped to the loopback origin. Never bound to a
  non-loopback address. The tray discovers instances through the default storage dirs
  (server and processor) plus `MOKURO_STORAGE` / `processor.yaml`.
- **`mokuro-bunko gui`**: starts the control API + app pages with no server/processor running
  (first run, or reconfiguring a stopped machine), opens the browser, exits when the wizard
  hands off to a started service or the tab is closed for 10 min. On Windows/macOS,
  double-clicking the binary with no arguments runs `gui` (CLI behaviour from a terminal
  unchanged: a TTY keeps today's help output).
- **`mokuro-bunko-tray`** (new crate `bunko-tray`, own binary): `tray-icon` + `muda` (+ `tao`
  event loop), MIT/Apache-2.0. Linux needs GTK 3 and libayatana-appindicator3 at run time
  (system libraries, dynamically linked; LGPL — acceptable per the licence stance), so the tray
  is a *separate* executable: the CLI/server binaries and Docker images never depend on GTK.

## 2. Control API (contract — streams G2/G3 code against this)

All JSON. `GET /control/status`:

```json
{ "role": "processor|server|gui", "version": "0.7.0", "name": "beast",
  "state": "working|idle|paused|pausing|connecting|disconnected|error|setup",
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
  "managed": false, "can_pause": true }
```

- `POST /control/pause {"mode": "after_volume"|"now", "until": ISO-8601|null}` → 200 status.
- `POST /control/resume` → 200 status.
- `GET /control/events` (Server-Sent Events): a `status` event on every change, at most 2/s;
  the tray and dashboard use it (polling `status` every 2 s is the fallback).
- `POST /control/stop` (managed instances only: the tray's Quit).
- `/app/...` serves the wizard/settings/dashboard pages; `/app/api/...` their backend
  (settings read/write, install progress via SSE, doctor, logs tail, service install).

**As built (stream G1; additions to the contract above, all backward compatible):**

- Crate `bunko-control` (`crates/bunko-control`): `types` (the JSON above, `.control.json`,
  `read_control_file`; no features, what the tray links with `default-features = false`),
  `pause`, `activity`, `control::Control`, `http::{ControlListener, ControlServer}`.
  Integration: `Control::new(ControlConfig::new(role, name, VERSION, &storage))` →
  `ControlListener::bind()` → `listener.serve(control, Some(gui::app(role, listener.token(),
  config_path, processor_config)))` → `server.shutdown().await` on exit. `serve` and
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
- Auth: `Authorization: Bearer <token>`, or the login cookie (`bunko_control`, or
  `bunko_control_<port>` as G2's `/app/login` sets it); the `Host` header must be a loopback
  name (DNS-rebinding guard) and a cookie-authenticated POST must carry our own `Origin`
  (CSRF). Wrong/missing token: 401; foreign Host/Origin: 403.
- `.control.json` also carries `url` (`http://127.0.0.1:<port>`) and `managed`. One live
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

## 4. Wizard and settings coverage

Flows (choose a role first; a machine can be both):
1. **Library server**: library folder, port/host, admin account (today's `/setup`), remote
   access (tunnel / dynamic DNS / HTTPS: `ssl`, `tunnel`, `dyndns` commands), OCR on this
   machine (§4.3), start with the machine.
2. **Processor**: library URL, login (user/password or token), connection test, name,
   max sessions, storage, TLS verify — today's `processor setup` — then §4.3 and start-up.
3. **OCR install with progress**: detected hardware (`install-ocr --list`), pack choice
   (`--variant`), download/verify/unpack progress, models download, `doctor`, first benchmark.
4. **Start with the machine**: `processor service --install` / systemd user unit / launchd
   agent / Windows Startup entry, plus a tray autostart entry and a Start-menu/desktop shortcut.

**Settings** must cover every CLI command and flag. The mapping lives in
`docs/rust-port/GUI-COVERAGE.md` (command/flag → page), and a test walks the clap command tree
and fails if a flag has no entry (or an explicit "CLI-only, because …" exemption, e.g.
`-v`). Pages the library server already has (admin panel: users, invites, settings, tunnel,
dyndns, generations, processors, updates) are linked, not duplicated; machine-local things
(OCR pack, models, processor.yaml, service, logs, doctor, update apply, SSL files) get new
`/app/settings` pages. Styling reuses `web/_static/shared.css`; no front-end framework or
build step (plain JS like the existing pages).

## 5. Tray

Menu (state-dependent):
- Status line(s): role + state (e.g. "Working: Dr Stone 01 · 37/196 · 4.2 p/s",
  "Paused until 18:00", "Idle — 0 in queue", "Can't reach library").
- **Statistics ▸** today (volumes, pages), total, current rate, GPU busy, backend/device.
- **Pause after this volume**, **Pause now**, **Pause for 1 hour**, **Pause until tomorrow**,
  **Resume** (only the applicable ones enabled).
- **Open dashboard**, **Open library** (server URL), **Settings…**, **Setup wizard…**,
  **Show logs**, **Check for updates** (uses the existing updater).
- **Start at login** (checkbox), **Quit** (stops instances the tray started; a
  system-service instance keeps running and the tray says so).
Icon states: idle, working, paused, attention (problem). Optional notifications: library
unreachable, OCR backend failure, update available (off by default).
Supervision: the tray starts `processor serve`/`serve` as a child when the machine is set up
for tray-managed running and no instance is up; restarts it on crash with backoff; never
starts a second instance when a service-managed one is running.

**As built (stream G3, `crates/bunko-tray`, binary `mokuro-bunko-tray`):**
- Discovery: `.control.json` (read with the same lenient types as `bunko-control`'s wire
  types; `tests/contract.rs` checks them against the real ones) in, in order: `--storage DIR`
  (repeatable), portable `data\`, `$MOKURO_STORAGE`, the storage of `$MOKURO_CONFIG` / the
  default `config.yaml` / the configs named by service files (systemd units, launchd plists,
  Windows Startup `.cmd`), the server default storage, the processor configs' storage, the
  processor default storage, `<temp>/mokuro-bunko-gui`, and (Linux) `/var/lib/mokuro-bunko/storage`.
  Rescanned every 3 s; a control file counts once `GET /control/status` answers. Each
  instance is followed by SSE, else polled every 2 s; 3 failures in a row drop it.
- **Tray-managed running is `tray.json`** next to `config.yaml` (`~/.config/mokuro-bunko/`,
  `%LOCALAPPDATA%\mokuro-bunko\`; portable `data\tray.json`):
  `{"managed":[{"role":"server"|"processor","args":[...]}],"notifications":false}`. Without
  `args`: `serve`, or `processor serve --config <default processor.yaml>`. `install.ps1`
  writes `{"managed":[{"role":"server"}]}`. The wizard's "Start with the machine" (G2)
  offers both: **"Run from the tray when I log in"** (recommended on a desktop with the tray
  program next to the CLI) writes this role's entry into `tray.json` (the server's without
  `args` for the default config, else `["-c", <config>, "serve"]`; the processor's with
  `--config <its processor.yaml>`), the tray's login item (as above) and restarts a running
  tray so it re-reads the file; **"As a background service"** (recommended headless) writes
  the service file. Picking one offers to remove the other for that role (service stopped
  and deleted; or the role taken out of `tray.json`, the tray restarted and its copy stopped). With no `tray.json`,
  no instance and no config at all, the tray opens the setup wizard once (`gui --open /app/`).
- Supervision: children get `MOKURO_CONTROL_MANAGED=1` (so `POST /control/stop` is allowed) and
  `MOKURO_LAUNCHER=tray` (an update exits 75 → restarted at once); console output goes to
  `<logs>/tray-<role>-console.log`. Backoff 1, 2, 4 … 60 s, reset after 60 s up. A role is
  not started while an instance of it answers or `systemctl [--user] is-active` /
  `launchctl print gui/<uid>/<label>` says its service runs. Quit: `POST /control/stop`, then
  SIGTERM, then kill after 30 s. Linux children get `PR_SET_PDEATHSIG(SIGTERM)`, so a tray
  that dies does not leave an unowned processor running.
- Menu as above; a `fail` problem puts the first one on its own line ("✖ …") and turns the
  icon to attention. Pause items follow `can_pause`; the timed pauses send `mode: "now"` with
  `until`. Pages open through the server's control host, else the processor's, else
  `mokuro-bunko gui --open <page>`: dashboard `/app/dashboard`, Settings… `/app/settings`
  (update available: `/app/settings#update`), Setup wizard… `/app/setup`, via
  `/app/login?t=<token>&next=…`. Check for updates runs `mokuro-bunko update check`.
- One tray per user (`.tray.lock` in the log folder). Logs: `<server storage>/logs/
  mokuro-bunko-tray.<date>.log` (portable `data\logs`; falls back to
  `<config>/mokuro-bunko/logs`, then the temp folder, when that cannot be created).
  Start at login: `~/.config/autostart/mokuro-bunko-tray.desktop`, LaunchAgent
  `io.github.gnathonic.mokuro-bunko-tray`, or Startup `Mokuro Bunko.lnk`.
- Not built: notifications (off by default in the spec).

## 6. Packaging

- Windows zip/installer: `mokuro-bunko-tray.exe` next to the CLI; Start-menu "mokuro-bunko"
  shortcut launches the tray; `install.ps1 -Startup` adds the tray to Startup.
- macOS: `mokuro-bunko.app` (LSUIElement agent) holding both binaries + icon; unsigned
  (quarantine note as today); launchd agent can start the tray.
- Linux x86_64 full and lite: `mokuro-bunko-tray` + `.desktop` entry + autostart entry +
  hicolor icons; `doctor` reports missing GTK/appindicator libraries with distro package names.
- Docker: no tray (unchanged). Licence notices updated for the new crates.

As built: PACKAGING.md §1 "Desktop tray" (archive layout per target, the musl lite archive
taking the manylinux-built tray, the `.app` with the CLI hard-linked inside, xtask
`--no-tray`/`--tray-bin`, licences, install scripts, and `mokuro-bunko update` replacing an
installed tray too).

## 7. Streams and gates

| stream | owns | gate |
|---|---|---|
| **G1 control** | control API + `.control.json` in serve/processor/gui, pause state machine + persistence, protocol `Availability`/`Released` + scheduler + admin processor list, stats aggregation | unit + integration tests (pause after/now/until, requeue without failure, restart keeps pause, token required, loopback only) |
| **G2 web app** | `mokuro-bunko gui`, `/app` wizard + settings + dashboard pages and `/app/api`, GUI-COVERAGE.md + coverage test, install progress | Playwright end-to-end on the desktop for each flow (library, processor, OCR install, autostart) against real binaries; coverage test green |
| **G3 tray + packaging** | `crates/bunko-tray`, icons, menus, supervision, autostart, packaging in xtask/scripts/deploy | runs on Linux (KDE/GNOME+AppIndicator), Windows (pimax), macOS (mac): screenshots of each menu state, pause/resume round trip against a real processor |

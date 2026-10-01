# mokuro-bunko 0.7 — mobile (Android app; why there is no iOS app)

**Status:** living document. Android: working APK, verified on an emulator (§6). iOS: not
shipped (§7).

## 1. What the Android app is

`Mokuro Bunko` (`app.mokuro.bunko`) turns a phone into a **library host**: it runs the
**lite** server (the same code as `mokuro-bunko serve --no-default-features`: WebDAV,
catalog, accounts, admin, the OCR scheduler for remote processors, the update notice)
inside the app, and shows the server's own web UI (setup wizard, catalog, admin panel)
in a WebView. Mokuro Reader on the same phone connects to `http://127.0.0.1:<port>/`;
other devices on the Wi-Fi or the phone's hotspot connect to `http://<phone ip>:<port>/`
when "Allow other devices" is on.

There is **no on-device OCR** in v1: OCR is done by processors on other machines
(`mokuro-bunko processor serve` against the phone's URL), exactly as for a lite server
on a NAS. Volumes wait in the queue until a processor connects.

| Path | What |
|---|---|
| `crates/bunko-android/` | the server as `libbunko_android.so` (cdylib; rlib for host tests): JNI entry points, lifecycle, config, logging |
| `packaging/android/` | the Kotlin app (Gradle 9.6.1 wrapper, AGP 9.4.1, no AndroidX), `build.sh`, `android_licenses.py` |
| `.github/workflows/release.yml` job `android` | builds and checks the APK; the `release` job attaches it |

## 2. Native library (`crates/bunko-android`)

JNI surface, class `app.mokuro.bunko.BunkoNative` (Kotlin `object`, `@JvmStatic external`):

| function | does |
|---|---|
| `start(storageDir, configPath, port, lan): String` | `"ok:<local url>"` or `"error:<message>"`. Blocks until the server accepts connections (≤ 15 s) or fails. A second call while running is a no-op. |
| `stop(): Boolean` | cancels `Services.stop` (the same token SIGTERM uses), waits for the graceful shutdown (≤ 5 s for in-flight requests) and the ordered service shutdown; returns whether a server was running |
| `isRunning(): Boolean` | the server thread is alive |
| `logTail(): String` | the last 300 log lines (shown on the settings screen) |
| `lastError(): String` | why the server last failed, or `""` |

- The server runs on its own OS thread with its own multi-thread tokio runtime
  (`min(cores, 4)` workers, as `serve`), built per start and shut down per stop, so
  start/stop/start in one process works (unit-tested). `Services::new` → `announce_setup`
  → `assemble` → `serve_router`, with `ServeOptions { flavor: "lite", local: None }`.
- Before starting it binds the port once to report "port in use" readably, and validates
  storage like `serve` (`validate_startup`).
- **Config**: `config.yaml` in the app's internal files dir. On first start it is
  written with `ocr.local_processing: false` and `server.cache_mb: 16`; on every start
  the three settings the native screen owns are applied — `storage.base_path`,
  `server.host` (`127.0.0.1`, or `0.0.0.0` with LAN access), `server.port` — and the
  file is rewritten only if one of them changed, so admin-panel edits survive.
- **Logging**: `tracing` → logcat (tag `mokuro-bunko`, via liblog's
  `__android_log_write`; no extra crate), `<storage>/logs/server.log` (2 MiB × 3
  backups), and the in-memory tail.
- A panic never crosses into the JVM (`catch_unwind` → error string).
- The app sets `HOME`, `XDG_DATA_HOME`, `XDG_CONFIG_HOME` (internal files dir) and
  `TMPDIR` (cache dir) before loading the library: an app process has no usable home
  or `/tmp`, and the server derives TLS auto-certificate and default paths from them.
- The lite graph cross-compiles for `aarch64-linux-android` and `x86_64-linux-android`
  with NDK 29 and no source changes: bundled SQLite (cc), ring (NDK clang via
  cargo-ndk), `notify` (inotify), `hostname`, `fs4`, rustls/webpki-roots. The `.so`
  needs only `liblog`, `libdl`, `libm`, `libc` (no `libc++_shared.so`) and its LOAD
  segments are 16 KiB aligned (NDK r28+ default), as Android 15+ devices with 16 KiB
  pages require. Size: 13.7 MB (arm64), 15.1 MB (x86_64), stripped.

## 3. The app (`packaging/android`)

- **MainActivity**: the WebView on `http://127.0.0.1:<port>/` (JavaScript + DOM storage
  on, file chooser for upload pages, other hosts open in the browser, back navigates the
  WebView via predictive back). Starts the server on open unless the user stopped it.
  Asks for `POST_NOTIFICATIONS` on Android 13+.
- **SettingsActivity**: status and Start/Stop; the URLs for Mokuro Reader (local and,
  with LAN access, every private IPv4 of Wi-Fi/hotspot/Ethernet/tethering interfaces;
  cellular/VPN interfaces are skipped) with Copy; port (1024–65535); LAN access switch;
  storage volume (phone storage or SD card, from `getExternalFilesDirs`, with free
  space) and the library path; "Apply and restart"; battery-optimisation status with a
  button to the system list; licences; the live log tail.
- **ServerService**: foreground service, type `specialUse` (§4), notification channel
  `server` (low importance, ongoing, with a Stop action and the LAN URL), a partial wake
  lock and a Wi-Fi lock (`WIFI_MODE_FULL_LOW_LATENCY`) while serving, `START_STICKY`.
  `start`/`stop` run on one background executor.
- **Network security config**: cleartext allowed only for `127.0.0.1` and `localhost`
  (the WebView); everything else keeps the platform default. The Rust side's outgoing
  HTTPS (update check, DynDNS) uses rustls and is unaffected.
- **Storage**: `<external files dir>/bunko/` (`library/`, the database, `logs/`), i.e.
  `Android/data/app.mokuro.bunko/files/bunko/library` — reachable over USB (MTP) to copy
  manga in, and by uploads through Mokuro Reader or the web UI. No storage permission is
  needed. `android:hasFragileUserData="true"` makes uninstall offer to keep it;
  `allowBackup="false"` (a library is far too big for Auto Backup).
- Icon: adaptive vector (open book with vertical text lines and a ribbon) with a
  monochrome layer for themed icons. Platform Material theme, light/dark, edge-to-edge
  insets handled by hand (no AndroidX: the only Java-side dependency is the Kotlin
  standard library).
- **Licences screen**: `THIRD-PARTY-LICENSES.md` is produced by `build.sh` with
  `xtask licenses --target aarch64-linux-android --flavor lite` (the collector the
  desktop archives use; it fails on copyleft) plus `android_licenses.py`, which appends
  the crates only the JNI library uses (`jni`, `jni-sys`, `cesu8`, `combine`, …) with
  their licence files and also fails on copyleft-only licences. `ANDROID-NOTICES.txt`
  covers the Kotlin standard library (Apache-2.0).
- Version: `versionName` = the workspace version; `versionCode` =
  `((major·100 + minor)·100 + patch)·100 + pre`, with pre = alpha.N → N, beta.N → 30+N,
  rc.N → 60+N, release → 99 (0.7.0-alpha.1 = 70001, 0.7.0 = 70099). minSdk 26,
  targetSdk/compileSdk 36. ABIs: arm64-v8a and x86_64 (32-bit devices are not built;
  `build.sh --abi armeabi-v7a` would add one).
- Updates: `bunko_update::InstallKind::detect()` is `Mobile` on Android, so the admin
  panel only shows a notice; the APK is installed over the old one (data kept when the
  signing key is the same). The APK is not in `release.json`.

### Why `specialUse` and not `dataSync`

Android 14 requires a foreground service type. A user-started HTTP/WebDAV server that
must stay up while the user reads on other devices matches none of the predefined
types: `dataSync` is for bounded transfers and, from Android 15, is limited to **6 hours
per 24 h** (`onTimeout`, then the service is stopped) — a library host would die every
evening. `connectedDevice` requires Bluetooth/USB/NFC-style permissions the app does not
use; `mediaPlayback` is wrong. `specialUse` has no time limit; the manifest declares
`PROPERTY_SPECIAL_USE_FGS_SUBTYPE` with the justification, which Google Play review
reads. Sideloaded builds need nothing more.

## 4. Building

```sh
# Prerequisites: Android SDK (platform 36, build-tools 36), NDK 29.0.14206865, JDK 17+,
# rustup targets aarch64-linux-android + x86_64-linux-android, cargo install cargo-ndk.
packaging/android/build.sh                 # release APK → dist/mokuro-bunko-<ver>-android.apk (+ .sha256)
packaging/android/build.sh --debug         # debug APK
packaging/android/build.sh --abi arm64-v8a # one ABI only
```

`build.sh` runs `cargo ndk -t arm64-v8a -t x86_64 --platform 26 -o app/src/main/jniLibs
build --release -p bunko-android`, the licence step, then `./gradlew assembleRelease`.
`ANDROID_HOME`, `ANDROID_NDK_HOME`, `CARGO_TARGET_DIR` and `CARGO_LOCKED=1` are honoured.
Gradle refuses to build without the native libraries (`checkRustLibs`). Native libraries
are stored uncompressed and page-aligned (mmap'd in place, no extraction): the APK is
~35 MB, a store would compress it for download.

### Signing

Without a release key the release APK is signed with the **debug key** so it installs
for testing. Do not publish such an APK: a later APK signed differently cannot be
installed over it (the user has to uninstall — choosing "keep app data" saves the
library).

1. Create a key once and keep it safe (losing it means users must reinstall):
   `keytool -genkeypair -v -keystore bunko-release.jks -alias bunko -keyalg RSA -keysize 4096 -validity 10000`
2. Locally: `packaging/android/keystore.properties` (git-ignored) with `storeFile`,
   `storePassword`, `keyAlias`, `keyPassword`; or the environment variables
   `BUNKO_ANDROID_KEYSTORE`, `BUNKO_ANDROID_KEYSTORE_PASSWORD`, `BUNKO_ANDROID_KEY_ALIAS`,
   `BUNKO_ANDROID_KEY_PASSWORD`.
3. CI: repository secrets `ANDROID_KEYSTORE_BASE64` (`base64 -w0 bunko-release.jks`),
   `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`, `ANDROID_KEY_PASSWORD`. Without
   them the job warns and ships a debug-signed APK.
4. Check: `apksigner verify --print-certs dist/mokuro-bunko-<ver>-android.apk`.

For Google Play, upload an AAB instead (`./gradlew bundleRelease`) with Play App Signing;
not wired yet.

### CI (`release.yml`, job `android`)

On by default (repository variable `BUILD_ANDROID=false` skips it). Ubuntu runner:
Rust with both Android targets, cargo-ndk, Temurin 21, `sdkmanager` installs
`platforms;android-36`, `build-tools;36.0.0`, `ndk;29.0.14206865`; optional keystore
from secrets; `build.sh --out dist-android`; then `apksigner verify`, `zipalign -c -P 16`
and a check that both `.so` files and the licence file are in the APK. The `release` job
waits for it (success or skipped) and uploads `mokuro-bunko-<ver>-android.apk` and its
`.sha256` as release assets — kept out of `dist/`, so they are not in `release.json`
or `SHA256SUMS`.

## 5. Limits and guidance

- **No on-device OCR.** Run a processor elsewhere. An ONNX Runtime Android build (NNAPI
  / XNNPACK) could come later; the models are large for phone storage and RAM.
- **Android may stop the server.** A foreground service with a wake lock survives
  screen-off and Doze on stock Android, but many OEM builds (Xiaomi, Huawei, Samsung,
  OnePlus, Oppo/Vivo…) kill background apps anyway. Tell users to:
  1. exempt Mokuro Bunko from battery optimisation (settings screen → "Open battery
     settings" → All apps → Mokuro Bunko → "Don't optimise"/"Unrestricted");
  2. on OEM skins, also allow "auto-start"/"background activity" and lock the app in
     recents (see dontkillmyapp.com for each vendor);
  3. keep the phone charging when it hosts for long periods.
  Android 12+ also forbids starting a foreground service from the background, so a
  `START_STICKY` restart after the process was killed may be refused (the app then shows
  the failure and stops cleanly); the battery-optimisation exemption lifts this
  restriction. There is no start-on-boot.
- **Mixed content.** Browsers block an `https://` page (reader.mokuro.app) from fetching a
  plain `http://` LAN address; `http://127.0.0.1` is allowed. From other devices use a
  native Mokuro Reader build, enable HTTPS (admin panel → SSL, self-signed: the device
  must trust it) or the tunnel. The settings screen says so.
- **Storage**: app-specific external storage. Since Android 11 file manager apps on the
  phone cannot browse `Android/data/`; USB (MTP) from a computer and uploads work.
  Uninstalling deletes it unless the user keeps the app data. A user-chosen folder
  would need "All files access" (`MANAGE_EXTERNAL_STORAGE`, restricted on Play) because
  the server needs real file paths, not SAF URIs. The file watcher uses inotify on the
  FUSE-backed shared storage; whether it sees files written over MTP was not tested
  (the server's full metadata pass at start-up picks them up in any case: stop/start
  the server after a large copy).
- Ports below 1024 need root; the app accepts 1024–65535. A port in use is reported.
- The phone's IP changes between networks; the settings screen and notification show
  the current one. No mDNS/Bonjour announcement yet.
- WebView downloads (e.g. a volume download link in the web UI) are not wired to
  `DownloadManager`.
- The admin panel's "update and restart" is not available (mobile installs update by
  APK/store); a restart request just stops the server and the app restarts it.

## 6. Verification (2026-10-01)

- `cargo test -p bunko-android` (host, 7 tests): config first write / admin edits kept /
  no rewrite when unchanged / URL formatting; log rotation and tail; the full lifecycle
  on a real port (start → `/api/health` 200, `/` → 302 `/setup` for `Accept: text/html`,
  `/setup` 200, `server.log` and `config.yaml` written → stop → port released → start
  again → stop; a taken port reported as an error); JNI port validation.
  `cargo clippy -p bunko-android --all-targets -- -D warnings` clean.
- `cargo ndk -t arm64-v8a -t x86_64 --platform 26 build --release -p bunko-android`
  with NDK 29.0.14206865: both `.so` link; `llvm-readelf` shows 16 KiB LOAD alignment,
  only system NEEDED libraries, and the five `Java_app_mokuro_bunko_BunkoNative_*` exports.
- `packaging/android/build.sh`: APK built (35.6 MB), `apksigner verify` (debug key),
  `zipalign -c -P 16` OK, both ABIs and the licence assets inside. `xtask licenses`:
  277 crates, no copyleft; 6 Android-only crates appended.
- Emulator (`Medium_Phone_API_36.1`, Android 16, x86_64, headless `-no-window`):
  install, cold launch; logcat shows the server starting on `127.0.0.1:8080` with
  storage in `Android/data/app.mokuro.bunko/files/bunko`; `adb forward` + host `curl`:
  `/api/health` 200 (`db_status ok`), `/setup` 200 ("Setup - Mokuro Bunko"), `/` 302 →
  `/setup`; the WebView renders the setup wizard and its JavaScript steps work;
  `dumpsys` shows the service foreground with type `0x40000000` (specialUse) and the
  notification; settings screen: LAN on + "Apply and restart" → the server rebinds
  `0.0.0.0:8080`, is reachable at the emulator's Wi-Fi address `10.0.2.16:8080`, and the
  notification shows that URL; reinstalling the new APK over the old one keeps the
  settings; the notification's Stop action shuts the server down gracefully (port closed,
  service and notification gone), Start on the settings screen brings it back; the
  licences screen shows the notices.
- Not verified: a physical phone, OEM battery killers, Doze over hours, the SD-card
  volume, MTP copy-in, the CI job itself (actionlint passes; it needs a runner).

## 7. iOS: not shipped

- **iOS does not allow long-running background servers.** An app is suspended a few
  seconds after it leaves the foreground; background modes (audio, VoIP, location,
  background fetch/processing tasks) are for those purposes only, and abusing one to
  keep a server alive is grounds for App Store rejection (Guideline 2.5.4). A library
  host that only works while the app is on screen is not useful to other devices.
- **Distribution**: the only practical channel is the App Store (sideloading needs a
  developer account and re-signing every 7 days for free accounts; EU alternative
  marketplaces still require notarization and the same background rules). Review would
  question a local HTTP server for "other devices".
- **Build host**: iOS builds need Xcode on macOS; no macOS machine is available to this
  project (CI macOS runners could build, but nothing could be tested or signed here).

What an iOS **companion** could be instead — a client, not a host:

- a reader-side app that connects to a bunko server elsewhere (Mokuro Reader in Safari
  already does this; a native wrapper would add offline caching and HTTPS-less LAN
  access via `NSAllowsLocalNetworking` plus the local-network permission prompt);
- or a foreground-only "share my library now" mode: the same Rust lite server compiled
  for `aarch64-apple-ios` as a static library (the dependency graph is portable; rusqlite
  bundled and ring build for iOS), serving only while the app is open, with
  `UIApplication.isIdleTimerDisabled` to keep the screen on;
- requirements either way: a Swift/SwiftUI shell, a C ABI (`cbindgen`) instead of JNI,
  Apple Developer Program membership, an Xcode/macOS build host (or CI macOS runner with
  signing secrets), App Store privacy labels, and the local-network usage description.

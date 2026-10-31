# Interop, page-loading and network testing (design)

Status: approved by the owner, 2026-10-26.

## 1. Why

0.8 splits one job across three parties: Bunko, Mokuro Reader and a cloud store (Google
Drive, OneDrive, MEGA or a WebDAV server). Each side has good tests of its own half, but
nothing runs the real reader against the real Bunko in CI, and no test has the reader and
Bunko working on one shared cloud folder. Page-loading speed and cloud traffic are not
measured anywhere; the latency targets in `0.8-remote-storage.md` §10 were never checked.

The trigger: on a live server with a large library, opening the catalog takes seconds, and
so does opening a series, even a small one. Both are answered from the database and the
in-memory index, so they should take milliseconds. Performance budgets exist to make that
a failing test, not an observation (§8.3).

The connections to cover:

| Connection | Today | Gap this design closes |
|---|---|---|
| Bunko WebDAV ↔ reader | Bunko emulates the reader's requests by hand; the reader tests against a stub WebDAV | The reader's real code against a real Bunko on every PR; CORS preflight (untested today) |
| Bunko HTTP API ↔ reader | Exact JSON pinned in Bunko; hand-written fixtures in the reader; two reader e2e specs need a manual Bunko | A shared contract both sides test against, so neither can drift silently |
| Bunko ↔ cloud | Mocks for Drive, OneDrive, MEGA (in test files); read-only live tests run by hand | One simulator shared with the reader; live write tests on a schedule |
| Reader ↔ cloud | All mocked. Gaps: Drive 403, 429, quota and revoked tokens; 412 conflicts on every provider | Reader against the simulator and the live accounts; the M7 shared-folder flows |
| Page loading, bandwidth, latency | One IndexedDB op-count test in the reader | Measured scenarios on shaped links, with hard budgets |

## 2. Decisions (owner, 2026-10-26)

- **D1. The rig lives in the Bunko repo.** A file names the reader commit it tests
  (`interop/reader.ref`). Bunko PRs run the rig against that reader. Reader PRs run the same
  rig against Bunko's latest published build. A break on either side fails that side's PR.
- **D2. A shared cloud simulator.** Bunko's Drive, OneDrive and MEGA mocks become one
  standalone HTTP server that Bunko and the reader both talk to in CI. The reader gains a
  test-only API base-URL override and token injection.
- **D3. Scheduled live runs on all three clouds.** Drive and OneDrive on the test accounts;
  MEGA on a new free account made for testing (never the account holding the real
  library). MEGA runs are sparing with bandwidth (§7.3).
- **D4. Hard performance budgets from the start.** A budget breach fails CI (§8).

## 3. Non-goals

- Testing generic WebDAV clients other than the reader (WebDAV is for Mokuro Reader only).
- Load or soak testing of a busy multi-user server.
- Browser coverage beyond Chromium in CI (Firefox/Safari stay manual for now).
- Any test against the owner's real libraries or real accounts.

## 4. Architecture

```
            ┌──────────── netsim (shaped links) ────────────┐
 Playwright │                                               │
 + reader ──┼──▶ Bunko (HTTP + WebDAV) ──▶ cloudsim / rclone │
 (pinned)   │         │                       ▲             │
            │         └── api_log, metrics ───┼─────────────┘
            └───────────────────────────────▶ cloudsim (reader direct)
                     rig runner: `cargo xtask interop <suite>`
```

Five pieces, all in this repo:

1. **Contract fixtures** (`interop/contract/`): JSON files both repos test against (§5).
2. **cloudsim** (new crate `bunko-cloudsim`, binary + library): the Drive v3, Graph
   (OneDrive) and MEGA endpoints that Bunko and the reader use, lifted from
   `bunko-remote/tests/{gdrive,onedrive}_mock.rs` and `bunko-mega`'s `mock` module, with
   their fault knobs exposed over a small control API (§6).
3. **netsim** (new crate `bunko-netsim`): a TCP and HTTP-CONNECT proxy that adds
   latency, jitter, bandwidth caps and connection resets per link. Pure Rust, so it runs
   the same on Linux, macOS and Windows without Docker or `tc` (§8.2).
4. **The rig runner** (`cargo xtask interop`): builds or downloads each party, starts them
   behind netsim, runs a suite, collects results and checks budgets.
5. **Scenarios**: Playwright specs (kept in this repo under `interop/e2e/`, run with
   Bunko's own Playwright install in `interop/`, `@playwright/test` pinned in
   `interop/package.json`, not the reader's) plus Rust-side scenarios for Bunko ↔ cloud.
   As built, this differs from the first design, which used the pinned reader's install:
   the specs, their fixtures and the Playwright version then move with this repo, and a
   reader bump never changes the harness under them.

Third-party pieces: `rclone serve webdav` (a single static binary, downloaded at a pinned
version) as the plain WebDAV cloud, and Chromium through Playwright.

## 5. Contract fixtures (layer 1: every PR, both repos, seconds to run)

Layout, format, sentinels and bless commands: `interop/contract/README.md`. Both sides
build on one seed (`interop/contract/seed.json`: an uploader, an account without write
rights, an admin; one series with and without OCR, a layer sidecar and a cover, one with
Japanese names).

**Bunko → reader.** A Bunko test (`crates/bunko-server/tests/contract_export.rs`) drives a
seeded full-stack server (`Services` + `assemble`, so the CORS, queue-file and WebDAV
layers are in) through everything the reader reads, and writes the answers to
`interop/contract/bunko/`. That is what the reader actually calls (checked against
`feat/cloud-bunko` on 2026-10-28); the other APIs (`/login/api/check`,
`/api/nav/config`, `/api/stats`, `/queue/api/status`, the catalog list, series, config
and OCR-status APIs) are not read by the reader and stay out:

- `GET /login/api/me` (Basic with the UTF-8 password, Bearer, a dead token, anonymous,
  a wrong password, an account without write rights, the 429 once the limiter is hot); `POST` and `DELETE /login/api/token`
  (the body `requestBunkoToken` sends; the 429 once the limiter is hot);
- `GET /catalog/api/manifest?series=&volume=` (with OCR, a layer and a cover; without;
  Japanese names; an unknown volume);
- `.mokuro-queue.json`: the HTTP copy and its `If-None-Match` 304, and the cloud copy
  (`bunko_remote::status::cloud_queue_doc`) with `done[]` and, cut to size,
  `truncated: true`. `done[]` and `truncated` exist only in the cloud copy;
- `.mokuro-bunko.json` as `active` and as `released`. The file has only those two
  states: "claiming" and "held" are both `active`, the difference being whether the
  settle wait has passed. `released` is written by a disconnect; a shutdown writes
  nothing;
- `series.json` and `catalog.json` as compiled;
- `OPTIONS /mokuro-reader/` as the reader sends it (its write-permission fallback reads
  `Allow`, its sign-in probe `X-Mokuro-Put`);
- PROPFIND: the server root (the reader's connection test), the library folder's
  Depth 0 `stat`, the Depth 0 `current-user-privilege-set` probe `checkWritePermissions`
  sends, Depth 1 of the library root and a series, Depth infinity of the library root;
  a conditional GET of a control file (304);
- PUT: a new volume with `Content-Digest` (`X-Mokuro-Put`, `-Upload`, `-Size`,
  `-Digest-Verified`, `-Manifest`, `-Recheck-After`), a sidecar, and the failure verdict JSON for a refused account and
  a stale `If-Match` (412);
- errors: 401 without credentials (Basic challenge), with a dead token (`Bearer …
  error="invalid_token"`) and with Latin-1 Basic bytes ("Invalid authorization
  header"; current readers send UTF-8), 409, 423, 429, 503
  `storage-lease-claiming` and 507. Only the WebDAV 503s carry `Retry-After`; the 401
  and 429 credential refusals do not, and the fixtures pin that;
- CORS has no fixtures of its own: `crates/bunko-server/tests/http_cors.rs` covers the
  rules (including an origin that is not allowed), and the replay below checks every
  reader request as a browser on the reader's origin sends it.

The test fails if the output differs from the committed files (the usual golden rule:
regenerate with `BUNKO_BLESS=1`, and the diff shows in the PR). The reader's unit tests
parse these files through its real parsers (`src/lib/interop/bunko-contract.test.ts`),
and its hand-written queue and marker documents start from Bunko's and may only edit
fields Bunko serves. The fixtures are read from a Bunko checkout (`BUNKO_REPO`, §9). The
one copy kept in the reader repo is a snapshot of those two documents, used when no
checkout is there; with one, a test checks both snapshots still equal the fixtures.
Bunko's key order is kept (the reader writes `series.json` byte for byte as Bunko does),
and the export is run twice in the test and must give the same bytes.

**Reader → Bunko.** The reader's `src/lib/interop/reader-requests.test.ts` records the
requests its own code sends (method, path, the headers that matter, the body; the
`Authorization` header as `basic:<user>` or `bearer:<user>`) into
`interop/contract/reader/`, each with the response fields and headers that code reads.
`crates/bunko-server/tests/contract_replay.rs` replays them against a fresh seeded server
and checks the status codes, fields (present, present but nullable, exact values) and
headers; hand-written records under `reader/legacy/` cover what older readers sent
(Latin-1 Basic). Every record goes as a browser on the reader's origin sends it: with
`Origin`, after a preflight when the method or a header is not simple; the preflight must
allow every header sent, and the answer must expose every header the reader reads.
Headers covered on purpose: `Authorization` (Basic with UTF-8 and Latin-1 bytes,
Bearer), `If-None-Match`, `Depth`, `Content-Digest` (which Bunko must report verified),
`Origin`, and the CORS preflight. The reader sends no `If-Match`, so none is recorded.

Only stable fields are kept (dates, ETags, tokens and random ids are replaced with
placeholders of the same type and format), so the fixtures change only when the contract
does.

## 6. cloudsim

- **Endpoints**: exactly those Bunko's drivers and the reader's providers call (to be
  listed per provider during planning from `bunko-remote/src/{gdrive,onedrive}` and the
  reader's `providers/*`), including resumable uploads, `changes.list` / delta, ranged
  `alt=media` reads, and MEGA's `cs` / `sc` / transfer URLs with real node crypto.
- **Auth**: issues tokens directly. A `POST /_sim/token` gives a token for a named
  account; revoking it, expiring it and returning `invalid_grant` are control calls. No
  real OAuth flow (the real sign-in pages stay a live-run concern).
- **State**: one in-memory store per account, shared by everything that talks to it, so
  a file the reader uploads is the file Bunko sees. Seeded from a fixture library.
- **Fault and quota knobs** (from the existing mocks): 429/503 with `Retry-After`, 401,
  403 `rateLimitExceeded` / `storageQuotaExceeded`, 412 on stale `If-Match`, dropped
  upload chunks, lost cursors, MEGA `EAGAIN` / over-quota / hashcash.
- **Accounting**: per-request log of method, class, Drive quota units, bytes in and out,
  served at `GET /_sim/log`. Budgets for API usage read this, not provider dashboards.
- The existing `bunko-remote` mock tests move onto cloudsim's library form, so there is
  one mock per provider, not two.

**As built (I2, crate `bunko-cloudsim`, library and binary):**

- **One port, one prefix per provider.** Drive v3 at `{base}/google` (a driver's
  `api_base`; uploads under `{base}/google/upload`), Graph v1.0 at `{base}/graph/v1.0`
  with the pre-authenticated download, upload-session and copy-monitor URLs at
  `{preauth}/graph/{preauth,upload,monitor}/{account}/{id}`, where `{preauth}` is a
  second listener (its own port, so its own origin, named in `GET /_sim/health`; the
  binary's `--preauth-host` renames it for jsdom, which drops a token only when the host
  name changes), so a client following the download redirect drops its token as it
  would for Microsoft's storage hosts; MEGA's gateway at `{base}/mega/`, OAuth refresh at
  `{base}/oauth/{google,microsoft}/token` (Google keeps its refresh token, Microsoft
  rotates it), the control API at `{base}/_sim/`. Every response carries permissive CORS.
  Writes are stamped on the wall clock the `Date` header shows (I3: Bunko's lease ages
  its marker by it; a clock of the simulator's own made every marker look stale). Stamps
  step a millisecond (MEGA: a second) past the previous one, and a `/_sim/node` time
  never moves the clock, so a burst or a future-dated node cannot stamp later markers
  ahead of `Date`.
- **MEGA stays in `bunko-mega`.** Its `mock` module (real node crypto, which needs the
  crate's internals) became a hub of accounts behind one gateway, picked by session id
  or login email; cloudsim mounts it. `MockMega::start()` still serves the crate's own
  tests.
- **Control API** (JSON): `GET health`; `POST token {provider, account}`,
  `token/revoke {token}`, `token/expire {token}`; `POST mega/account {account, email,
  password}`; `POST seed {provider, account, files: [{path, content_base64}]}`,
  `GET files?provider=&account=` (`[{path, size, sha256}]`); `POST node {provider,
  account, parent_id?|parent_path?, name, folder?, content_base64?, created?,
  modified?}` (`{id}`: a new node beside any of the same name, at the given RFC 3339
  times, so a scenario can stage the duplicate folders and old files path seeding
  cannot; OneDrive answers a duplicate name 409 `nameAlreadyExists` as Graph does;
  MEGA's one `ts` takes the modified time) and `GET tree?provider=&account=
  &include_trashed=1&include_deleted=1` (`[{id, parent_id, name, folder, size, sha256,
  created, modified, trashed}]`, trashed meaning in the trash or under a trashed folder,
  listed only on request; `include_deleted` adds what Drive and MEGA deleted for good,
  as it last was, marked `deleted`, so S6's `everNames` and S1/S9's `writesOutside`
  still see a file Bunko wrote and then deleted), neither in the request log; `POST fault {provider,
  account, method?, path_has, status, reason, retry_after?, times?}` (Drive and Graph;
  MEGA's error codes are knobs); `POST knobs {provider, account, <knob>: value}`;
  `GET`/`DELETE log` (`[{seq, provider, account, method, path, class, units, bytes_in,
  bytes_out, status}]`, `units` 1 per Drive request; MEGA entries add `error`, the code
  answered in the 200 body); `GET`/`DELETE violations` (`DELETE` answers the ones it
  cleared, so taking them is one call);
  `POST reset`. The crate's README has the knob list and a curl session.
- **Violations, not panics.** A request the simulator cannot accept (an unmodelled Drive
  `q`, a malformed body, an unknown endpoint, an unmodelled MEGA command) is a 4xx in the
  provider's shape plus a violation; Rust tests fail on drop if any are left, the
  reader's suites check after every test. What only Bunko's driver must send (`$top`,
  `keepRevisionForever=false`) is checked on every request under the per-account
  `strict_bunko` knob, which bunko-remote's tests turn on and the reader leaves off.
  The fixed test token `tok` opens the `default` account only; MEGA session ids are
  never reused, a reset included.
- **A full server on it.** `MOKURO_TEST_CLOUD_API_BASE_{GDRIVE,ONEDRIVE,MEGA}` and
  `MOKURO_TEST_CLOUD_TOKEN_URL_{GOOGLE,MICROSOFT}` point the stores and the connect
  probe at cloudsim in a build with the `testing` feature (`mokuro-bunko` has one); a
  build without it refuses to start while one is set, naming it.
  `MOKURO_TEST_CLOUD_TUNE` (`lease=on|off,lease_ttl_s=N,lease_settle_ms=N,
  retry_base_ms=N,queue_floor_ms=N`) sets a spawned server's store timing, so the rig's
  lease handover takes seconds, not a 60 s settle and a 15 min ttl; a tune the server
  cannot read stops it starting. `POST /_sim/mega/account` answers the account's saved
  session, which a rig server keeps in its credentials as a connected server does.
- **A processor with no models.** The same `testing` build has a hidden
  `mokuro-bunko processor-fake --url URL --username U --password P --storage DIR`
  (password also from `MOKURO_FAKE_PROCESSOR_PASSWORD`): it logs in, registers and
  holds its socket as `processor serve` does, over `bunko-processor`'s `FakePipeline`
  (one page per image), so the lite build the rig runs can OCR. `--engine ID`
  (repeatable; default every engine the build knows), `--page-delay-ms`,
  `--load-delay-ms` and `--max-sessions` shape it; `--hold-file PATH` holds each
  volume after it starts until the file exists (a rename during OCR, deterministic);
  `--engine-revision N` announces revision N for its engines and writes `revision: N`
  into its sidecars' `ocr_engine` block, right after the id (upgrade scenarios; without
  it, the build's own revision of each engine, as a real processor does);
  `--page-text TEXT` gives every page one text block reading TEXT (a block a reader can
  correct). A build without `testing` has no such command.
- **The reader** runs its real providers against the binary when `CLOUDSIM_URL` is set
  (`src/lib/interop/cloudsim/`, one file at a time; its CI job `interop` builds the
  simulator from Bunko and sets `REQUIRE_CLOUDSIM=1`, so the suites fail rather than
  skip without one). Worker transfers get the simulator's bases in their credentials (a
  Web Worker has its own copy of the hooks module). Drive's `gapi.client.drive` is replaced by a small REST shim in
  those tests, so the real gapi transport stays a live-run concern (I5). The base-URL
  overrides and the injected OneDrive token sit behind `CLOUD_TEST_HOOKS`
  (`MODE === 'test'`, or `MODE === 'interop'` from `vite build --mode interop`; never an
  environment variable a host could set), every guarded read written
  `CLOUD_TEST_HOOKS ? hook : real` so a production build folds them away;
  `scripts/check-no-test-hooks.mjs` checks the built code (it fails on a hooked build).

**Added in I3 (B8.1).** `POST /_sim/node` and `GET /_sim/tree` above stage what path
seeding cannot (the duplicate `mokuro-reader` roots of S1 and S9, the second spelling of
a series in S5) and show what reached the cloud, the trash included: `explicitly_trashed`
marks a node that went to the trash itself, apart from one under a trashed folder.
`GET /_sim/content?provider=&account=&id=` answers a node's stored bytes (MEGA's
decrypted), for a file the spec did not write. None of them is in the request log. The
rig uses them; the reader's own cloudsim suites (`src/lib/interop/cloudsim.ts`) do not
yet.

**Reader changes (in the reader repo, part of this work):**

- One base-URL constant per provider (Drive API, Drive upload, Graph, MEGA gateway)
  instead of the hard-coded addresses in `api-client.ts`, `google-drive-core.ts`,
  `util/cloud.ts`, `util/backup.ts` and `onedrive/constants.ts`. The default stays the real
  address; a test build reads overrides.
- Token injection for tests: a test hook that hands the Drive and OneDrive token managers
  a token instead of running Google Identity Services or MSAL.
- MEGA: point `megajs` at cloudsim's gateway (to confirm in planning that the pinned
  `megajs` accepts a gateway option; if not, a fetch shim in the test build).
- Performance marks (§8.1). All of the above compile to nothing in production builds.

**Bunko changes:** a test-only setting (`MOKURO_TEST_CLOUD_API_BASE_*`, refused unless the
`testing` feature is on) so a full server can use cloudsim; the drivers already accept
`api_base`.

## 7. Scenarios

### 7.1 Interop rig suites (layer 2: CI, real processes, no real cloud)

Every suite runs once on the `lan` link profile; the `perf` suite also runs on the
shaped profiles (§8.2).

| Suite | Parties | Checks |
|---|---|---|
| `dav` | reader ↔ Bunko (local library) | connect, list, open and read a volume, progress write and read-back, sidecar PUT, Basic (UTF-8 password) and Bearer auth, token renewal and logout revocation, CORS preflight from the reader's origin |
| `api` | reader ↔ Bunko | the reader's queue poller and pending view on a real queue (304s, `done[]`), login and account flows, nav config with readers off, error banners for 401/429/503 |
| `bridge` | reader ↔ Bunko ↔ cloudsim (each provider) and rclone | reader reads through Bunko from a cloud library; bridge PUT; 503 during the lease claim; restart resumes |
| `direct` | reader ↔ cloudsim (each provider) and rclone | reader alone: list, read, upload, 401 refresh, revoked token banner, 403 quota, 429 backoff, 412 conflict |
| `shared` | reader and Bunko on one cloudsim folder (each provider) and rclone | the M7 exit checks: Bunko progress for a volume the reader uploaded; sidecar pulled on `done[]`; reader stops writing `series.json`/`catalog.json` while the marker is fresh and resumes 2 × ttl after Bunko stops or at once on `released`; edits go up as `<Vol>.edit.mokuro` and never as the main sidecar; a Bunko case merge shows as a rename with progress kept; duplicate roots (two top-level `mokuro-reader` folders plus a nested one) converge into the canonical root whether Bunko, the reader or both merge them, with the nested root untouched and no `Suspect` |
| `cloud` | Bunko ↔ cloudsim and rclone (no reader) | the Rust-side Bunko ↔ cloud scenarios at process level: cold start, adoption, outbox under faults, lease handover, quota pauses |
| `perf` | all of the above, shaped | §8 |

The 0.8 crossplay scenarios run inside those suites, one spec each (their checks are in
the "As built" notes below; `docs/design/plans/2026-10-29-i3-rig.md` lists what each
needed on either side):

| Scenario | Specs | Suites and providers | Capability |
|---|---|---|---|
| S1 duplicate roots under a lease | `shared/dup-roots-lease` | `shared`; google, mega | `dedup` |
| S2 edit layers across accounts, following a rename and a delete | `dav/`, `bridge/edit-layers-accounts` | `dav`; `bridge` on every cloud | `edit-layers-personal`, `edit-layers-curated` |
| S3 history and positions across accounts | `dav/`, `bridge/`, `shared/history-accounts`, `shared/cloud-owner` | `dav`; `bridge` and `shared` on every cloud | `history`, `positions` |
| S4 a rename keeps provenance (no new queue entry over two walks) | `dav/`, `shared/rename-provenance` | `dav`; `shared` on every cloud | `provenance` |
| S5 case-variant merge | `shared/case-merge` | `shared`; google, mega, rclone | `case-merge-rename` |
| S6 a rename during OCR (no `_1` files) | `dav/`, `shared/rename-during-ocr` | `dav`; `shared` on every cloud | none |
| S7 Allow on first connect | `dav/allow-first-connect` | `dav` | none |
| S8 a spine offset cleared (no PUT on the next two syncs) | `dav/`, `shared/spine-offset-clear` | `dav`; `shared` on every cloud | none |
| S9 reader-only dedup, then a Bunko restart with no hold | `direct/dup-roots`; its restart in `shared/dup-roots-lease` | `direct` and `shared`; google, mega | `dedup` |

A spec that needs a capability (`test.use({ needs: [...] })`) FAILS when the reader's
driver lacks it, unless `interop/pending-reader-caps.txt` lists that capability: then it
is skipped and the results JSON reports the skip with its capability. A Bunko change
that needs a newer reader than `interop/reader.ref` lists the capability there until
the PR that bumps `reader.ref` empties it again. The file lists nothing today; I3 is
done only while it lists nothing.

**As built (I3, runner core):** `cargo xtask interop rig <suites> [--provider P,…]
[--bin B] [--cloudsim-bin C] [--reader-dir D | --reader-ref SHA] [--grep G] [--headed]
[--keep]`. Suites and providers expand to the pairs that apply (`dav` and `api` on
`local`, the others on `google`, `microsoft`, `mega` and `rclone`; `all` for either).
`--bin` must be a testing build (checked through `processor-fake --help`). `--grep`
keeps the scenarios and specs whose name contains G, as plain case-sensitive text in
every suite (Playwright gets it escaped, inside `/…/`, with `--pass-with-no-tests`). Each pair
writes `target/interop/rig/<suite>-<provider>.json` (scenarios with outcome, time and
the reason for a failure or skip) and copies every party's log to
`target/interop/rig/logs/<suite>-<provider>/`; its work folder is kept on failure or
with `--keep`. cloudsim's violations are taken after every scenario and fail it.
rclone is `v1.75.2`, downloaded into `target/interop/tools` and checked against its
pinned sha256 and size per OS before use (`rig/rclone.rs`); it serves with
`--allow-origin <reader origin>` for the browser suites; `--rclone-bin` takes an
installed rclone instead, checked to be the pinned release (`rclone version`).

**As built (I3, browser harness, `rig/reader.rs`, `rig/playwright.rs`, `rig/browser.rs`).**
The reader is `--reader-dir` (that checkout as it is) or, without it, the commit in
`interop/reader.ref` (`--reader-ref` overrides it), fetched once into
`target/interop/reader/<sha>` (`git init`, `git fetch --depth 1` of exactly that SHA).
With neither a `reader.ref` nor `--reader-dir`, the rig stops, naming both. It runs
`npm ci` where nothing is installed (a developer's own install is used as it is) and
`npx vite build --mode interop`, cached by the commit, a hash of a `--reader-dir`'s
uncommitted changes and the `package-lock.json` hash; the cache's stamp also records the
build output's file list, so another build in the same checkout forces a rebuild. It is
served once per run by `vite preview --mode interop --host 127.0.0.1 --port P
--strictPort`, P picked by the rig before rclone starts (rclone's CORS origin names it);
`node` runs the checkout's own vite directly, in the rig's process group, so Ctrl-C stops
it with every other party. It is ready once P serves this build's
`/_app/version.json` (SvelteKit's per-build stamp, compared byte for byte with the
build output's) while vite still runs: P is released before vite binds it, and any
200 would also take another run's preview of another reader that got the port first.
Each suite and provider then gets its own parties: `dav` and `api` a server on a local
library; `bridge` and `shared` a server on the cloud (cloudsim or rclone) holding its
lease; `direct` the cloud alone. Every server has the accounts `alice` (editor), `bob`
(uploader), `carol` (registered) and `ocr` (processor) beside its admin, and a held
`processor-fake` (`--page-text`). The library is `Rig Series` with `Vol 1` (with its sidecar) and
`Vol 2` (without). The specs (`interop/e2e/rig/<suite>/*.spec.mjs`, one worker, a fresh
browser context each) get READER_URL, BUNKO_URL, CLOUDSIM_URL, CLOUDSIM_PREAUTH_URL,
RCLONE_URL, RIG_PROVIDER, RIG_ACCOUNTS, RIG_WORKDIR and RIG_FAKEPROC_HOLD as C6 lists
them (unset when the suite has no such party), plus `RIG_CONTEXT` (JSON: the shared
cloud account, its MEGA or WebDAV sign-in, the seeded library, and a cloud server's
lease ttl; for `shared` and `dav`, `uploads`: archives the rig made for the specs to
import and upload, `rig.upload(rel)`) and, with a Bunko server, `RIG_CONTROL_URL`: the runner's
control port (`rig/control.rs`), whose `POST /bunko/restart {stale?, tune?}` stops the
suite's server and starts it again on the same data (`rig.restartBunko` in the
fixtures), answering once it serves HTTP; `POST /bunko/stop` leaves it down (SIGINT: no
`released`, its marker just stops moving) and `POST /bunko/start` starts it again
(`rig.stopBunko`, `rig.startBunko`). `stale` stays down until the marker it last wrote is past its ttl,
so it claims the library afresh; `tune` sets `MOKURO_TEST_CLOUD_TUNE` keys for that
process over the suite's own, and a `lease_settle_ms` not under the ttl is refused (a
claimant writes no heartbeat while it settles, so it would find its own marker stale
and claim again forever). The fixtures
(`interop/e2e/rig/fixtures.mjs`, the rule in `egress.mjs`) refuse every request off this
machine except `apis.google.com/js/api.js` and `accounts.google.com/gsi/client`, which
load as empty scripts (`stubbed_urls` in the results), and FAIL the spec a refused one
came from, naming its hosts (`refused_hosts` in the results): an interop reader that
reaches for a third party would need the network on CI, or talk to a real service in
place of cloudsim (the reader leaves out its web font and the shell's absolute
addresses for this, `src/lib/interop/hermetic.ts`); start Chromium
resolving no host but `127.0.0.1`, `localhost` and `::1` (IP literals included), so what
the route never sees — a WebSocket, a preconnect, a DNS prefetch — cannot leave either;
block service workers, whose fetches would bypass the route; set `__mokuroInteropConfig` v1 (cloudsim's
bases, a Drive or OneDrive token from `/_sim/token`, `gapiShim: true`) before any page
script; wait for `__mokuroInterop.apiVersion === 1` and check that every v1 method is
there (the driver API v1 below), so a reader that lost one fails at the gate, naming it,
not halfway through a spec; fail a spec on the cloudsim violations
recorded while it ran (those recorded before it started fail on their own, as
`cloudsim violations before <spec>`); and gate capabilities: a spec declares `test.use({ needs: [...] })`, and a
capability the driver does not list FAILS the spec unless `interop/pending-reader-caps.txt`
lists it (one per line, `#` comments), in which case the spec is skipped and reported
with that capability. Each of `dav/`, `bridge/` and `direct/` starts with a
`smoke.spec.mjs` that connects and lists the seeded library, the harness end to end: a
Basic sign-in to the server (on its own disk, or on the cloud holding its lease), or the
reader alone, signed in with the config's Drive or OneDrive token, cloudsim's MEGA
account or rclone's WebDAV. `npm run selftest` in `interop/` checks the gate, the route and the
resolver against a stub page, no reader needed, and fails if any spec but the one meant
to skip is skipped. Results: `target/interop/rig/<suite>-<provider>.json`
carries `suite`, `provider`, `bunko_sha`, `reader_sha` (`-dirty` for a changed
`--reader-dir`), `passed`, `failed`, `skipped: [{spec, capability, reason}]` (`reason`
the skip's own words, a `test.skip(cond, reason)` inside a test's included),
`duration_ms`, `pending_caps`, `refused_hosts` and `stubbed_urls`, with Playwright's
JUnit XML beside it and the parties' logs (the server's `serve.log`, `cloudsim.log`,
`processor-fake.log`, rclone's) under `logs/<suite>-<provider>/`; the reader's build
and preview logs are `reader-build.log` and `reader-preview.log`. A browser suite
folder with no specs fails its leg (every suite has specs, so a moved folder must not
pass by running nothing); with `--grep` it is reported skipped.

**The driver API v1** (`globalThis.__mokuroInterop`, the reader's
`src/lib/interop/rig-driver.ts`; `interop/e2e/rig/driver.mjs` `V1_METHODS`): C6's
twenty (`connectWebDAV`, `connectMega`, `connectDrive`, `connectOneDrive`, `logout`,
`sync`, `library`, `open`, `setPage`, `progress`, `renameVolume`, `renameSeries`,
`deleteVolume`, `setSpineOffset`, `editFirstBlock`, `publishCurated`, `selectLayer`,
`providerStatus`, `controlState`, `historyStatus`) and the six the suites added before
any reader was pinned: `showSeries` (the catalog card's click), `pendingOcr` (what a
volume's card shows of the server's queue), `idle` (no sync and no listing in flight),
`importVolume` and `backupVolume` (the upload modal's import; the card's Backup, to
the end of its run), and `settleSeriesWrites` (every metadata write the app queued run
now and finished: the listings' background work, the series backfills, the
`series.json` writes and then `catalog.json`'s, round again while any of it queued
more). Beside it a spec may call the reader's cloud test hooks,
`globalThis.__mokuroCloudTestHooks.setCloudTestToken(provider, token)`, to hand the
page a token re-minted after it loaded (`__mokuroInteropConfig` is read once, at
load). The set is v1 from the first `reader.ref` on: a method added after that comes
with `apiVersion` 2, or behind a capability. `providerStatus` reads what the app last
published (the store the banner and the read-only gates read) and never publishes it
itself, so a state the app keeps to itself fails the spec as the user would miss it.

**As built (I3, `dav` and `api`, on `local`).** While any route is on, Playwright
answers every CORS preflight itself, permissively, so a server whose CORS a browser
would refuse still passes. Both suites therefore run with `preflightsReachServers`
(fixtures.mjs): the requests are watched instead of routed (a host off the machine is
still refused by the resolver and still fails the spec; `blob:` and `data:` name none),
and the two Google scripts are answered empty by a CDP interception of those two URLs
alone. `npm run selftest` shows both sides: a stand-in DAV server sees the preflight in
that mode and never under the route. Specs ask the server what another client would see
through `interop/e2e/rig/bunko.mjs` (admin-created accounts, UTF-8 Basic, `/login/api/me`
under a token, a file as one account reads it); each spec that signs in makes its own
account, so the limiter and revocations stay its own.
- `dav/auth`: Basic with a UTF-8 password at connect (decoded off the wire), then the
  bearer token on every request; a reload resumes on the token alone; a token the server
  signed out is re-issued once (single-flight) and the requests retried; a token near its
  expiry is renewed at the next connect and the old one signed out; logout signs the
  token out on the server; the browser's preflights from the reader's origin are answered
  by Bunko itself (`Access-Control-Max-Age`, `X-Mokuro-Put` on DAV paths, which a made-up
  answer lacks) and allow every method and header asked for.
- `dav/library` (serial, one account): a volume opened is downloaded through its card's
  Download and its page image decodes; progress lands in the account's own
  `volume-data.json` (not another's) and a second browser context reads it back; an edit
  goes up as the account's `<Vol>.edit.mokuro`, the main sidecar byte for byte as it
  was, the edit a 404 to another account.
- `api/queue`: a connect polls the queue file at once; with `Vol 2` claimed and held by
  processor-fake, the next looks (the tab hidden and shown again) are 304s under
  `If-None-Match` and the volume's card shows the job; once the hold file exists the
  job leaves the file (a 200 under a new tag), the card clears and the image-only volume
  installed on the device gets the server's OCR through its manifest. A Bunko server's
  queue file has no `done[]` (that list is the cloud-direct queue's, `shared`):
  completion here is the volume leaving the file, and its manifest.
- `api/banners`: 401 at connect (`Authentication failed - check your credentials`,
  type `auth`); 401 mid-session after a password change elsewhere (`needsAttention`, and
  after a reload the pre-filled sign-in form); 429 past the limiter's ten failures
  (`Too many failed attempts - try again later`, transient); 503 + `Retry-After` fails
  the sync and keeps the session. A Bunko on its own disk has no 503 a spec can call up
  (its 503s are the cloud storage's waits, the `bridge` suite's), so that one is Bunko's
  outage answer made at the browser's door (`page.route`) for that spec.

**As built (I3, `direct`, on google, microsoft, mega and rclone).** The reader
alone, signed in as the smoke spec signs in (`interop/e2e/rig/cloud-direct.mjs`, which
also lists the cloud's files under `mokuro-reader/`: cloudsim's `/_sim/tree`, or
rclone's served directory). A spec that faults a write first waits for the reader to
settle (`__mokuroInterop.idle()`: no sync or listing in flight, the app's own after a
sign-in or a reader opening included), then faults the NEXT write of
`volume-data.json` once: Drive a PATCH of its id, Graph its `createUploadSession`, MEGA
the next `u` by `fail_command` (or `eagain`), rclone at the browser's door
(`page.route`, rclone having no fault knobs). `anotherDevice()` (fixtures.mjs) is a
second browser context in the same spec under the same rule, signed in afresh.
- `direct/library`: a volume opened is downloaded from the cloud and a page of it
  decodes; a page turn goes up in `volume-data.json` (its stored hash moves), the cover
  goes up beside the archive, and another device reads the page back.
- `direct/faults`: **401** (google only: an expired token, GIS handing a new one at
  the silent re-auth) the sync goes on with no banner and the other device sees the
  page; MSAL refreshes before expiry, a MEGA session does not expire and WebDAV sends
  its credentials each time, so those skip to the next spec. **Revoked** (a revoked
  token, MEGA `kill_session`, rclone answered 401 at the door) with nothing to write:
  the reconnect banner (`needsAttention`), the app idle behind it and still bannered
  after another sync; a reconnect clears it (Drive a new token, OneDrive MSAL's
  redirect: a reload under a new token, MEGA and rclone a sign-in). **Quota** (Drive
  403 `storageQuotaExceeded`, Graph 507 `insufficientStorage`, MEGA `-17`, WebDAV 507):
  the sync fails as storage full, still signed in with no banner, the cloud's copy
  unchanged (not on plain WebDAV, where the reader deletes before its PUT on purpose),
  the next sync lands it. **429** (Retry-After 1; MEGA two EAGAINs): waited out and
  landed in the same sync; skipped on rclone (the reader's WebDAV writes have no
  Retry-After rule). **412**: that sync fails without a banner, another device writes
  meanwhile, and the next sync leaves both devices with both pages; skipped on MEGA (no
  conditional write).
- The banner is read as the user sees it (`providerStatus`, the store the app
  published, never refreshed by the driver). That caught a reader bug on `microsoft`
  and `mega`: a refused token (Graph 401, MSAL asking for interaction) or a killed MEGA
  session set the provider's reconnect flag without publishing it, so no banner came up
  until something else refreshed the status. Drive's re-auth answers after the request
  returns, as GIS does, so a burst of 401s makes one re-auth (the rig's token client
  answered inside the call, and three 401s at once read as a refused grant); a 401 for
  a token a re-auth has since replaced asks for none.
- cloudsim's MEGA mock opens a new session on a password sign-in after
  `kill_session` (the killed id stays dead), so a reconnect can be tested.

**As built (I3, `bridge`, on google, microsoft, mega and rclone).** The reader signs in
to the server on the cloud (Basic, then its token) as an account each spec makes
(`interop/e2e/rig/bridge/bridge.mjs`); on cloudsim the cloud account is held to what
Bunko's driver must send (`strict_bunko`), since only Bunko talks to it. Nobody owns
this cloud's root (`progress_user` unset), so an account's own files and edits stay on
Bunko's disk. The owner case runs in `shared/cloud-owner` (below).
- `bridge/library`: a volume opened is downloaded through Bunko and a page of it
  decodes; the archive Bunko serves hashes as the cloud's copy does, and the page never
  sent a request to the cloud's origins. An editor's edit goes up to Bunko as its
  `<Vol>.edit.mokuro` and never reaches the cloud; its Publish (the bridge PUT of
  `<Vol>.curated.mokuro`) lands in the cloud, the bytes Bunko serves hashing as the
  cloud's.
- `bridge/lease` (serial): **the claim**: a restart with `stale` and a settle wait of
  half the ttl; once the Storage panel shows `claiming` with time left, a Publish
  answers 503 `storage-lease-claiming` with a Retry-After (every 503 the page got says
  so), the claim still running when it comes back, `failed` and no reconnect banner;
  once `held` (a higher epoch) the same Publish lands in the cloud. **The resume**: a
  plain restart reaches `held` with the same epoch and never a settle wait
  (`claiming_left_s`); the reader's next sync and Publish land with no 503, on the bearer
  token it had before, with no request to `/login/`.

**As built (I3, `shared`, on google, microsoft, mega and rclone).** The reader signs in
to the cloud directly (as `direct` does) while the suite's Bunko holds the same
library's lease, with a 45 s ttl (`browser.rs` `lease_tune`): the reader re-reads a
cloud marker every 60 s and calls one that has not moved for 2 × ttl stale, so the
rig's usual 12 s would read as Bunko gone between two looks. The specs keep the
reader looking (the tab hidden and shown, which makes its queue poller look at once)
and first wait until it has SEEN the marker move (`bunkoSeenAlive`): until then a reader
judges the marker by Bunko's own clock (ttl + 5 min). Who wrote a file is read off the
browser's own requests (`recordReaderWrites`, `reader-writes.mjs`), never off the cloud,
which Bunko writes too: WebDAV paths and MOVE destinations, Graph's `<name>:/content`
and upload sessions, Drive's metadata and the ids its writes answer, and the node handles
MEGA's `p` answers (its names are encrypted on the wire), looked up in `/_sim/tree`. An
upload session's content PUT (Drive's `upload_id`, Graph's `uploadUrl`) takes what the
request that opened it named, as a Worker's answer can be gone by the time it is read
and Chromium reports a Drive init whose body nobody read as failed (a write it can put
no name to is listed for the failure message); a file's contents
come from `/_sim/content` (rclone: its served directory). The reader
imports and uploads with the driver's `importVolume` and `backupVolume` (the card's
Backup, to the end of its run, metadata writes included), and reads what it wrote
only after `settleSeriesWrites`: the run's listing starts work of its own after the
run (a folder with archives and no `series.json` is backfilled), whose writes wait
out a debounce and decide at its end whether to write.
- `shared/ocr`: an image-only `Vol 3` the reader uploaded is queued by Bunko; the card
  shows the job from the holder's `.mokuro-queue.json`; once processor-fake lets go the
  queue file lists it in `done[]`, Bunko's sidecar is beside the archive and the
  volume's OCR is on the device.
- `shared/edits`: an edit goes up as `Vol 1.edit.mokuro` (an `edited` block, no
  `ocr_engine`); the reader writes no `Vol 1.mokuro`, through two more syncs, and the
  main is byte for byte as it was.
- `shared/writes` (serial, last: it ends with the library disconnected): while the
  marker is fresh, a new series' upload writes neither `series.json` nor `catalog.json`
  (Bunko compiles both), and still none once Bunko's two are in the cloud; Bunko stopped, the reader's verdict turns 2 × ttl after its
  last heartbeat (measured 88–89 s of a 90 s 2 × ttl, a heartbeat at most ttl / 3 before
  the stop) and the next upload writes both; the Storage panel's Disconnect writes
  `released` and the reader takes the files back at its next look (within about a second).
- cloudsim fidelity the suite needed: MEGA keeps a session (and its `sek`) per sign-in
  (one session for the whole account had a resumed Bunko decrypt nothing after the
  reader signed in, and hold on a "root change"), and models the thumbnail attribute
  megajs puts on an uploaded cover (`ufa`, `pfa`).

**As built (I3, scenarios S1 and S9).**
- The seed (`dup-roots.mjs`, plan X5) sits beside the suite's library root A, made when
  the suite started: an OLDER top-level `mokuro-reader` B (`/_sim/node` dates it 2020)
  and `Backup/mokuro-reader`, older still (2019) but nested, so never merged and still
  read. A gets `S1 Dup/Vol 1` with the NEWER sidecar; B gets an older `S1 Dup/Vol
  1.mokuro`, its own `S1 Dup/Vol 2` and a `volume-data.json` (Vol 2 on page 3); the
  nested root gets `S1 Nested/Vol 1`. Every volume carries its sidecar (an image-only
  one would be queued, and a held job holds a merge off), made from the library's own
  `Vol 1` (the archive's zip comment re-tagged so each has bytes of its own). Both
  mergers keep the oldest top-level folder of a group, so B, not A, is canonical: the
  C6 text's "A survives (top-level first)" was written for a nested B, which X5 rules
  out of the merge. The whole library therefore moves into B, and every spec after
  these reads it under another folder of the same name.
- What "trashed only once empty" can see: cloudsim's `/_sim/tree` marks a node
  `explicitly_trashed` when it went to the trash itself (Drive's own flag; MEGA: its
  parent is the rubbish bin; OneDrive: deleted, its parent not). A merged-away folder
  must be explicitly trashed and hold nothing that is not (on Drive a file trashed on
  its own keeps its parent: the losing copy of a same-named file, or a
  `volume-data.json` a reader's sync folded into another copy). No control file is
  ever in the trash.
- `direct/dup-roots` (S9, `direct` on `google` and `mega`; skipped on `microsoft` and
  `rclone`, which hold no two folders of one name). Each top-level root also holds a
  `.mokuro-queue.json` and A a `volume-data.json` (the library's Vol 1 on page 4). The
  reader's syncs merge A into B: the newer `Vol 1.mokuro` is live in B and the older
  one in the trash, both queue files are in B, the nested root is exactly as it was
  (same nodes, same bytes, nothing added), the reader lists `S1 Dup/Vol 2` and `S1
  Nested/Vol 1`, its progress has both copies' pages (4 and 3) and so has the one
  `volume-data.json` it leaves in B; after it opens a volume and syncs, every file
  written since the merge lies under B.
- `shared/dup-roots-lease` (serial; `shared` on `google` and `mega`). S1: Bunko's
  marker says `dedup`, so the reader steps aside (`controlState().held`); the
  processor-fake hold is released for the spec (the merge moves `Rig Series`, whose
  image-only volume would otherwise hold it off) and restored after. Bunko alone
  merges A into B with the reader not syncing: the checks above, both
  `volume-data.json` copies live in B, a marker that Bunko keeps beating in B (its
  lease followed the canonical root), the reader reading B's and the nested root's
  volumes and Bunko serving `S1 Nested/`; every file written since the merge lies
  under B, but for the index and covers Bunko compiles for the nested root's own
  series (it compiles them wherever a series lives), and A still holds nothing but
  what the merge trashed. Then three times a NEWER top-level root appears with a
  series of its own and a `volume-data.json` while the reader syncs back to back (the
  reader keeps stepping aside), and converges the same way, every root merged so far
  checked again after each. "Every file written since" counts trashed files too (a
  write into a merged-away root's old id lands in the trash with it; trashing moves
  no write time in cloudsim), from the whole second of the merge (MEGA stamps a node
  in whole seconds). What a race's new root was seeded with is left out only while it
  carries the stamp `/_sim/node` answered as it made the node (`seedNodes`,
  `asSeeded`; no tree read after the seed, which a write in between would already have
  moved): Drive keeps a file's id through a rewrite, so a later write into that
  `volume-data.json` counts again. The C6 text reads write parents off `/_sim/log`; its
  entries name a request path, and a Drive or MEGA upload's parent is in its body, so
  the tree is what can tell. The Storage panel, sampled every 500 ms through the whole
  spec, never shows `suspect`, and the page gets no 503. S9's end (C2-R5) follows:
  Bunko stopped (SIGINT), an older top-level root C appears, the reader's verdict
  turns after 2 × ttl and its syncs merge the library into C. Bunko started again reaches
  `steady`, `online` and `held` with no `suspect` sample, its marker beats in C (the
  marker is written into `roots()[0]`, so that is the canonical root; the admin API
  has no roots list), the reader trusts it again, and Bunko serves the library, C's
  series and the nested one. S9's reader-alone part lives in `direct` and its restart
  here: the `direct` suite runs no Bunko.
- What they caught (each fixed test-first, its rig run failing before): Bunko never
  saw a `mokuro-reader` folder made while it ran (it located roots only at startup),
  so S1 sat unmerged for 5 minutes on `google` (the feed now relocates on such a
  folder) and the race roots on `mega` (MEGA's feed reported nothing outside the
  tracked roots; it now reports a folder named like the root); with the reader's
  merge into a root Bunko had never seen, Bunko restarted `suspect` with
  `root_changed` (its vacated check now counts new roots as holders); and the
  reader's MEGA merge deleted an emptied folder permanently instead of putting it in
  the Rubbish Bin (S9 `direct` on `mega`: the folder was gone from the tree).

**As built (I3, scenarios S7 and S8).**
- `dav/allow-first-connect` (S7, on `local`): a fresh browser connects for the first
  time as a new editor, uploader, registered account, processor, and anonymously. The
  reader's `isReadOnly` is exactly "`/me` grants neither `canWriteProgress` nor
  `canAddFiles`" (the role table is pinned too: editor, uploader and registered write;
  processor and anonymous do not; no Bunko lets anonymous write progress), and
  `OPTIONS /mokuro-reader/`, sent from the page under the header the reader itself sends
  (its bearer token, or none), lists PUT and MKCOL exactly for the writers: the same
  Allow any other client gets under the password. With `/login/api/me` taken away at
  the browser's door, an uploader and a registered account still connect writable: the
  reader's OPTIONS sees `X-Mokuro-Put: verified` and skips the Allow verdict (R3.1).
  A read-only account's progress sync is refused its upload (the reader hides Sync
  then): noted on the spec, not asserted.
- `dav/spine-offset-clear` and `shared/spine-offset-clear` (S8; `dav` on `local`,
  `shared` on every cloud): the reader, holding a volume of the series, sets the spine
  offset to 3, so Bunko's `series.json` says 3; resets it to 0, so the file has no
  `spine_offset` key; then two syncs, each followed by
  `__mokuroInterop.settleSeriesWrites()` (every queued `series.json` write run now, every
  one in flight finished), write `series.json` zero times. On `dav` Bunko's file is its
  WebDAV GET and the writes are the page's own PUTs. On `shared` Bunko's file is what
  Bunko serves for the path once it is its own compile (every volume entry carries
  `matched_page_count`, which only the compiler writes; until Bunko ingests a reader's
  write it serves that file as it found it) and the cloud's one file of that name holds
  the same bytes; the writes are the reader's own (`recordReaderWrites`).

**As built (I3, scenarios S2 and S3).**
- `dav/edit-layers-accounts` and `bridge/edit-layers-accounts` (S2; `dav` on `local`,
  `bridge` on every cloud), from `layer-accounts.mjs`, serial, one fresh device per
  spec. The scenario seeds its own series over WebDAV (the seeded archive under other
  bytes, a sidecar with a new `volume_uuid`), so its rename and delete touch nothing
  another spec reads. The roles are the rig's: alice an editor (curated scope `all`;
  the spec's "uploader with edit rights"), bob an uploader, carol registered, each a
  fresh account. alice edits: her `<Vol>.edit.mokuro` reads 404 for bob and carol, the
  main's bytes are unchanged, and the file is her overlay `users/<u>/edits/<rel>` on
  Bunko's disk, never a library file (nor, on `bridge`, a cloud file). alice publishes:
  the manifest lists `curated` (and no `edit`) for bob and carol; bob's reader pulls
  it, selects it, and shows alice's text on the open page (`.textBox`). bob's edit on
  top forks into his own `edit` (`editFirstBlock` answers `layerId: 'edit'`); curated
  keeps its bytes. carol's own edit goes up; her Publish is refused by her reader, and
  her own PUT of the curated file by Bunko (403). alice, on a fresh device, waits until
  its layer sync has pulled `edit` and `curated` (the reader moves only the layer files
  its own layer rows vouch for: a rename while that pull is in flight leaves curated
  under the old name, which the spec once caught), then renames the volume: every
  account's edit reads under the new name and 404 under the old (overlays on disk
  moved), curated too, and the manifest of the new name lists `curated`; on `bridge`
  the cloud holds exactly the new main and curated. alice deletes it: every account's
  edit reads 404 and each overlay is gone from disk, curated too, and nothing of the
  volume is left in the cloud.
- `dav/history-accounts` and `bridge/history-accounts` (S3), from
  `history-accounts.mjs`, two fresh registered accounts. A page view is recorded when
  it ends, so each reader opens `Vol 1`, turns to a page and goes back to the series
  page before it syncs. alice's history month and `positions.json` are in her tree
  (`users/<u>/`, byte for byte) and not in the library (nor the cloud). bob, before
  reading, gets 404 at her paths; after, his reader knows none of her devices, his
  `positions.json` names his page and device, hers is unchanged; his PUT of her very
  history path is his own file and hers keeps its bytes. Then the admin seeds a series
  with its own sidecar (no OCR job) and once Bunko's `catalog.json` lists it (a
  metadata pass has run since), the catalog, the queue file and `history/series.json`
  name neither the history tree nor a device.
- `shared/history-accounts` (S3 cloud-direct, `shared` on every cloud): the reader on
  the cloud itself, beside a Bunko whose marker allows history, syncs its history and
  `positions.json` into the cloud folder. Two Bunko accounts list none of it
  (PROPFIND depth infinity), read 404 at its paths and no `positions.json`; alice's
  PUT at the reader's history path is hers, and the cloud keeps the reader's one file.
  After the seeded series is in Bunko's and the cloud's `catalog.json`, neither names
  `history`, the queue file names neither it nor the device, and no `series.json`
  appears in the history.
- `shared/cloud-owner` (S3 with a cloud-root owner, `shared` on every cloud): the
  Readers setting names an account `progress_user` (`PUT /_admin/api/storage/readers`,
  restored after). The direct reader syncs its history and `positions.json`; the owner
  reads that month file (the cloud's bytes) and that page through WebDAV. Then the
  owner's own reader (`anotherDevice`, signed in to Bunko) syncs: it lists the direct
  reader's device, and its page turn, its month file and a correction of the spec's
  volume (`<Vol>.edit.mokuro`) land in the cloud with the direct reader's position
  still in `positions.json`. Another account reads 404 and no `positions.json`, and its
  own correction at the same path is its own while the cloud keeps the owner's, once.
  Last the direct reader turns a page and syncs: `positions.json` holds its new page
  and the owner's, it lists the owner's device, and the owner's month file and
  correction are in the cloud byte for byte as the owner's reader left them (the
  direct reader wrote no `Vol 1.edit.mokuro`). The spec has no clobbering step of its
  own: once, by hand, a temporary change that wrote `positions.json` without the
  owner's entry before the direct reader's sync made it fail (`owner: null`), and was
  reverted. PENDING on OneDrive (`LOST_POSITIONS_PENDING`): the reader there pushes
  `positions.json` before it reads the cloud's copy (its cache, rebuilt whole by a
  listing begun before the push, lacks the file), so the owner's entry is lost; on
  exactly that outcome the spec runs every other check and reports itself skipped,
  until the reader's fix is in `interop/reader.ref`. The correction goes on a series the
  spec seeds itself, and whatever happens it leaves the cloud as it found it (the rig
  runs one spec after another with no reset, and `shared/edits` reads `Rig Series/Vol
  1.edit.mokuro`): the reader signs out, the admin removes the series and the other
  account its overlay, the owner writes the four root files and the history back
  through Bunko as they were (removing what was not there), and the spec fails unless
  the cloud then holds what it held before. Bunko answers a DELETE from its index, so a
  404 for a file the cloud still holds (the direct reader's write, not listed yet) is
  asked again, up to 60 s for the whole restore (`cloud-restore.mjs`); one that outlasts
  that is annotated `restore-unseen` and left to that last check.

**As built (I3, scenarios S4 and S6).**
- From `rename-ocr.mjs`; each scenario uploads series of its own through the reader
  (`RIG_CONTEXT.uploads` in `dav` and `shared`: `S4 Provenance/Vol 1-2`,
  `S6 Rename/Vol 1-2`), so its renames move nothing another spec reads. processor-fake
  stamps the library's engine (`hayai-nova`) at this build's revision; the C6 text's
  "engine `fake` 1.0" names no engine a server can be set to, so the block is
  `{id: hayai-nova, revision: 1, generator, precision}` and Bunko seals it. The rig
  starts it with `--page-text` (every page one text block), so a reader has a block to
  correct. Each spec drives the hold file itself (`RIG_FAKEPROC_HOLD`: removed to hold,
  written to release) and leaves it as it found it. Bunko's walks are counted by the
  admin-only `walks` of `/queue/api/status`, which only a testing build reports: "two
  passes after X" is "two walks that started after X finished", a condition, never a
  timer. On `dav` the reader is a new editor; on `shared` it is on the cloud itself,
  Bunko's marker seen moving.
- Both run on `dav` (`local`) and `shared` (every cloud; C6 asks for `google`).
- `dav/rename-provenance` and `shared/rename-provenance` (S4): two volumes uploaded image
  only; Bunko OCRs them and the reader pulls the OCR. Each main carries a sealed
  `ocr_engine`. `Vol 2` is edited (its first block): the account's `edit` layer goes up
  with its `edited` marker (`dav`: the editor's overlay; `shared`: the cloud file).
  Then the processor is held, so a job queued for either volume would stay in the queue,
  and the reader renames both volumes, then the series. Once Bunko lists the new names
  (its own PROPFIND) and the old folder is empty (its `series.json`, Bunko's compile of
  a factless series, is retired once no archive is left: on OneDrive that takes up to a
  minute), two walks on, the queue holds nothing
  for either series name; each main under the new names is
  `{title: <new series>, volume: <new title>}` with the block it was installed with,
  seal and all, and no `edited`; the edit layer reads under the new names with the
  same text and the same `edited` marker, and 404 under the old.
- `dav/rename-during-ocr` and `shared/rename-during-ocr` (S6), two specs. An image-only
  volume's job claimed and held; the reader (no OCR of it) renames it; released: the
  sidecar is `Vol 1 renamed.mokuro`, the queue settles and stays settled two walks on,
  and no file name ever ended `_<n>.mokuro[.gz]`. A volume whose OCR the reader holds,
  its sidecar then deleted by the admin so Bunko OCRs it again, held: the reader's
  rename writes the main at the new name while that job runs; released, the same
  checks, and the main is the reader's (the block it was installed with). "Ever": on a
  disk library the spec watches the library folder (`fs.watch`, recursive) while it
  runs, since Bunko's `_n` sweep removes a numbered copy it can prove is its own within
  a walk, and so on rclone (the directory it serves); on cloudsim, every node it kept,
  trashed ones included. Node's recursive watch on Linux reports no file already gone
  when it looks, so a copy that lives only a few milliseconds would pass unseen; Bunko's
  live until its sweep, a walk later.
- What they catch (each checked by undoing the fix in a scratch build, then restoring
  it): the reader moving the archive before uploading the main (R4.4) leaves a job
  queued for the renamed volume (S4); a reader regenerating the main without its block
  (R9.3) loses `ocr_engine` (S4); with that reader order and `collect` writing
  `<stem>_1` beside an occupied destination (before B4.4), `Vol 2 renamed_1.mokuro`
  appears, and is swept a second later (S6, through the watch). S4 also found a reader
  bug: a series whose volumes have layer files in the cloud could not be renamed (the
  gate read `Vol 2.edit.mokuro` as a cloud-only volume `Vol 2.edit`); on `mega`, both
  found that Bunko never saw a reader's rename: MEGA's `a` command carries the new
  attributes as `at` (megajs sends that, and the `u` packet echoes it), while Bunko's
  client sent, and its mock read, `attr`, so a megajs rename left the node with no
  attributes Bunko could decrypt, and Bunko's own renames did not send the field MEGA's
  clients use. Both now use `at`.
  Under load, S4 on `dav` also caught Bunko publishing a cover it made from an archive
  the reader had moved away meanwhile, under the old name: the old folder never
  emptied (`thumbs`: no cover for an archive that is gone).
- The driver reads the listing the provider cache holds now, not `cloudFiles`, whose
  incremental updates arrive on a 300 ms trailing edge: a `deleteVolume` right after
  `renameVolume` otherwise found the renamed volume not backed up.

**As built (I3, scenario S5).**
- `shared/case-merge` (S5, `shared` on `google`, `mega` and `rclone`; skipped on
  `microsoft`, where Graph refuses a second spelling of a name with 409, so there is
  nothing to merge). The reader needs `case-merge-rename`. The cloud first gets
  `kingdom/Vol 2` as another client would put it there (`/_sim/node`; on rclone a
  WebDAV `MKCOL` and `PUT`, since rclone's directory cache would not see files written
  to its disk for minutes); the reader downloads it and sets page 2. Then an older
  `Kingdom/` appears with `Vol 1` (`/_sim/node` dates it 2020; on rclone, which reports
  no creation time, Bunko breaks the tie by path, and `Kingdom` sorts first). Each
  volume carries its sidecar (`RIG_CONTEXT.uploads`: `Kingdom/Vol 1`, `kingdom/Vol 2`,
  ids `…051` and `…052`): an image-only volume would be queued for OCR, and a job the
  held processor-fake claims holds the merge off. Once Bunko's Storage panel lists
  one finished merge of `kingdom` into `Kingdom`, `Kingdom/` holds both volumes and
  `kingdom/` is gone (trashed; on rclone deleted), the reader syncs: its row for Vol 2
  takes the cloud's spelling with the same volume id, cloud path `Kingdom/Vol 2.cbz`
  and the same progress. After a second sync and two Bunko walks, no node made since
  the merge (cloudsim's tree, trashed nodes included) is a `kingdom` folder, and the
  trashed one holds exactly what it held at the merge (nothing made or moved into it);
  on rclone the reader sent no write naming `kingdom/` and the folder is still gone. The merge count for `Kingdom` stays at 1. The C6 text reads the writes off
  `/_sim/log`; its entries name a request path, and a Drive or MEGA upload's parent is
  in the body, so the tree is what can tell.
- What it catches: with the reader's adoption undone (a scratch build), the row stays
  `kingdom` and loses its cloud path, so the spec fails. Before the fix it also caught
  a Bunko bug: the cover worker made a cover for `Kingdom/Vol 2.cbz` after the merge
  had moved the archive but before it had moved `Vol 2.webp`. The folder then held two
  covers of that name, the merge recorded a `DupFile` conflict blaming a reader upload
  and did not trash the loser, and a second merge group trashed it (`google`, twice in
  two runs). `thumbs` now skips an archive while a merge is open on it
  (`merge_busy`), both before making the cover and again before publishing it, and
  looks at it again 5 s later.

The generated sidecar (`genlib::mokuro_doc`) carries `title_uuid`: the reader's import
refuses a `.mokuro` without it, so no download of a rig volume with OCR could install.

The `cloud` suite (`rig/cloud.rs`) runs one library of four volumes and a reader's
`series.json` through two server processes, in order, on each provider: **cold-start**
(the catalog lists the series, the bridge serves an archive whole and ranged, byte for
byte), **adoption** (the `series.json` tag is in the catalog, the lease marker is in
the folder), **outbox-faults** (two 503s on uploads, creates only on Drive so a lease heartbeat
cannot spend them; MEGA two `-3`s: a bridge PUT still lands, and cloudsim's log shows
both faults fired, MEGA's by the `error` it records), **ocr-results** (`processor-fake`
OCRs the volumes without a sidecar; the result reaches the cloud past the same two
faults, both seen to fire, and its bytes hash as the cloud's copy does),
**lease-handover** (a second
server stands by and answers writes 503 `storage-lease`; once the first stops, it
takes the library over and its PUT lands) and **quota-pause** (a full provider: a
PUT answers 507, the next one 507 without reaching the provider, health blocked by
`StorageFull`). The hold lifts at the first storage probe, five minutes on; the suite
does not wait for it. On rclone, outbox-faults and quota-pause are skipped (no fault
knobs). Measured locally (wall time, setup included): google 25–38 s, microsoft 22–34 s,
mega 87–100 s (one MEGA command a second, and a 30 s lease ttl so a heartbeat keeps
writes open). On rclone the suite first checks the CORS preflight a browser would send
from an allowed origin (`OPTIONS` asking for PROPFIND with `Authorization`): it must
answer 2xx without credentials, echo the origin and name `Authorization`, which a `*`
does not cover. rclone has run locally through `--rclone-bin` (cold start to
handover green, about 16 s); its pinned download has not run anywhere yet.

### 7.2 Live runs (layer 3: scheduled, real clouds)

- **Accounts**: Drive (the existing test Google account, in the develop/beta project),
  OneDrive (the existing throwaway Microsoft account), MEGA (a new free account created
  by the owner for testing). Credentials are GitHub Actions secrets: refresh tokens for
  Drive and OneDrive, a session string for MEGA.
- **Sandbox**: each run works inside `bunko-interop/<run-id>/` under the provider's
  `mokuro-reader/` folder, after a snapshot of the folder tree. The run deletes what it
  created, then diffs against the snapshot; a non-empty diff fails the run. Pre-existing
  JSON files are never edited.
- **Content**: the `shared` and `direct` checks above, plus `bridge` for Bunko, on a small
  fixture library.
- **Schedule**: Drive and OneDrive nightly, MEGA weekly. Manual dispatch also works for a
  pre-release check.
- **Token upkeep**: OneDrive rotates refresh tokens on use, so the run writes the new one
  back to the repository secret (needs a fine-grained token with secrets write, stored as a
  secret). A Google project in "testing" consent status expires refresh tokens after 7
  days; the live Drive run needs the develop project's consent screen in production
  status, or a weekly re-auth. To settle in planning.

### 7.3 MEGA bandwidth

Free MEGA accounts get a small transfer allowance that refills over several hours. The
MEGA run:

- uses a fixture library of about 5 MB total (two short volumes);
- downloads each byte at most once per run (Bunko's cache and the reader's are warm for
  the second pass);
- stops at the first over-quota answer and reports "skipped: transfer quota", which
  counts as a warning, not a failure, unless it repeats on two runs in a row;
- logs bytes moved per run so drift shows early.

## 8. Performance and network (layer 4)

### 8.1 Metrics

Reader (via `performance.mark`/`measure` at fixed points, read by Playwright):

- `open→first-page`: from opening a volume to the first page painted;
- `page-turn`: from the turn input to the next page painted, paged and continuous scroll;
- `volume-ready`: from opening to all pages available offline (where the reader downloads
  the whole archive);
- `catalog-ready`: from launch to the catalog showing covers for a 200-volume library.

Bunko (from the server, timed by the rig):

- **Large catalog, first priority.** On a library of 2,000 series and 20,000 volumes
  (generated: real-sized archives as sparse files, real covers, community details and
  `series.json` filled in), on a local library and on each cloud store:
  - `/catalog/api/library` and `/catalog/api/series` server time, warm and cold (just
    after a restart);
  - the catalog page in Chromium: first cards shown, all visible covers shown;
  - opening a small series (3 volumes) in that large catalog: request to rendered list;
  - the same views in the reader when it lists a Bunko library.

  These are answered from the database and the index, so their budgets are in
  milliseconds (§8.3). A test machine timing on 2026-10-26 (4,000 tiny local volumes,
  release build) gave a baseline: library list 1 ms, series 0.2 ms warm and 24 ms cold, catalog
  cards on screen in 190 ms, series view in 51 ms. The 24 ms was the library index walking
  every series folder inside the request once its 30 s lifetime ran out or a write (an OCR
  result, an upload) invalidated it; on a busy server that walk competes with the writes,
  which fits the intermittent multi-second stalls seen live. The index now answers from
  its last snapshot and refreshes in the background. The generated library must still add
  what the first one lacked (real archive sizes, metadata, concurrent writes) so that a
  return of the stall fails a budget.
- PROPFIND Depth 1 of 2,000 entries;
- page GET from a volume, cold and warm (central directory cached or not);
- bridge reads: small-file hit and miss, archive member hit and miss (the §10 table);
- cover and thumbnail GET.

Traffic (exact counts, from cloudsim's log and Bunko's `api_log`):

- API calls and Drive quota units per scenario: cold start of a 1,000-volume library, one
  idle hour (simulated clock), opening one volume, one OCR round trip;
- bytes downloaded per page shown (direct and bridge);
- requests per page shown.

### 8.2 Link profiles (netsim)

| Profile | Latency (one way) | Jitter | Down / up | Resets |
|---|---|---|---|---|
| `lan` | 0 ms | 0 | unlimited | none |
| `broadband` | 15 ms | 3 ms | 50 / 10 Mbit/s | none |
| `mobile` | 75 ms | 20 ms | 5 / 1 Mbit/s | none |
| `bad` | 150 ms | 50 ms | 1 / 0.5 Mbit/s | 1 % of connections |

netsim sits on each link: browser ↔ Bunko, browser ↔ cloud, Bunko ↔ cloud. The `bad`
profile is mainly a correctness check: timeouts, retries and resumable uploads must
finish, and no request may hang past its stated timeout.

### 8.3 Budgets (hard, per D4)

Budgets live in `interop/budgets.toml`, one entry per metric and profile, and changing
one is a visible edit in the PR that needs it.

- **Counts and bytes** (API calls, quota units, requests and bytes per page): exact
  numbers with a small fixed allowance (for example +2 calls), because they come out the
  same on every run.
- **Timings**: each scenario runs 5 times; the budget applies to the median, with 25 %
  headroom over the measured baseline. One retry of the whole scenario is allowed before
  failing, so a single noisy CI machine does not fail the build; two misses do.
- **Starting values**: the §10 targets where they exist (PROPFIND < 50 ms, small hit
  < 10 ms, and so on), else the first baseline measured on the CI runner, recorded with the
  commit that set it.
- **Catalog and series views** (lan profile, large catalog), whatever the store, since
  they come from the database and the index:

  | Metric | Budget |
  |---|---|
  | `/catalog/api/library` server time, warm | ≤ 25 ms |
  | `/catalog/api/library` server time, cold (first request after start) | ≤ 250 ms |
  | `/catalog/api/series` server time, warm / cold | ≤ 5 ms / ≤ 50 ms |
  | catalog page: first cards shown | ≤ 400 ms |
  | small series in a large catalog: rendered | ≤ 150 ms |
  | requests needed before first cards show | ≤ 10 (the page, 2 stylesheets, 2 scripts, 5 API calls in parallel; exact) |

  None of these may grow with the number of series beyond the list itself: the rig runs
  the series view at 200 and at 2,000 series, and fails if it is more than 20 % slower at
  2,000.
- Timing budgets run on the Linux CI runner only; macOS and Windows run the counts and
  bytes budgets.

Results are also kept as a JSON artifact per run, so trends are visible even while a
metric is within budget.

## 9. CI wiring

**Bunko repo** (`.github/workflows/interop.yml`):

- On every PR: layer 1 (part of `cargo test`), then the rig (`dav`, `api`, `bridge`,
  `direct`, `shared`, `cloud`, `perf`) on ubuntu against the pinned reader.
- On merge to `main` and the release branches: publish the Bunko build the reader's CI
  uses (a `nightly` GitHub release, Linux x86_64, replaced on each merge).
- Nightly and weekly: the live runs (§7.2).
- A PR that bumps `interop/reader.ref` runs the same rig, which is how a new reader is
  taken in.

**Reader repo** (`.github/workflows/interop.yml`, added in the reader PR):

- On every PR: check out the Bunko repo at `main` for `interop/` and the contract
  fixtures, download the `nightly` Bunko build, and run the same rig against the PR's own
  reader code.
- The reader's unit tests read `interop/contract/` from that checkout in place
  (`BUNKO_REPO=bunko`; nothing is copied or committed to the reader repo, so there is
  one copy). Since I1 this runs in the reader's existing `ci.yml` test job (a sparse
  checkout of `interop/contract` at the `BUNKO_REF` repository variable, with
  `BUNKO_CONTRACT_REQUIRED=1` so a missing checkout fails instead of skipping).

Run time target: the rig under 15 minutes per PR on ubuntu.

**As built (I3, nightly):** `.github/workflows/interop-nightly.yml` builds the testing
lite server and cloudsim on every push to `main` and `0.8/remote-storage`, nightly for
both, and on demand, and publishes them as the pre-release `nightly` (from `main`) or
`nightly-0.8`: `mokuro-bunko-testing-x86_64-linux`, `bunko-cloudsim-x86_64-linux` and
`nightly.json` (`{bunko_sha, branch, version, built_at}`). Before publishing it starts
the server with `MOKURO_TEST_CLOUD_API_BASE_GDRIVE` set and waits for `/api/health`
(a build without `testing` refuses to start; `--help` alone exits before that check).
The tag moves to the new
commit and the assets are replaced in place, so a reader run downloading them never
finds the release gone. The updater never offers them: their tags are not versions and
they carry no signed `release.json` (`bunko-update`'s
`prerelease_lookup_skips_the_nightly_test_builds`). The reader's repository variable
`BUNKO_REF` picks the tag: `0.8/remote-storage` → `nightly-0.8`, `main` → `nightly`.

**As built (I3, Bunko CI):** `.github/workflows/interop.yml` runs on every PR, on pushes
to `main` and `0.8/remote-storage`, and on demand; `catalog-perf` is unchanged.
- `rig-build` reads `interop/reader.ref` (one 40-hex SHA, the rig's own rule) and fails,
  naming the file, while it does not exist: it pins a PUSHED reader commit, so the owner
  commits it after pushing the reader. It builds the testing server, cloudsim and xtask,
  checks out the reader at that SHA and builds it as the rig does (`npm ci`,
  `npx vite build --mode interop`), runs `npm run selftest` in `interop/`, and uploads
  one tar (the two binaries and the reader with its install; an artifact alone keeps
  neither file modes nor symlinks).
- `rig` (needs `rig-build`, `fail-fast: false`), one leg per row, each running
  `cargo xtask interop rig <suites> --provider <p> --bin … --cloudsim-bin …
  --reader-dir reader` on what `rig-build` made, Chromium installed by
  `npx playwright install --with-deps chromium` in `interop/`, and `target/interop/rig/`
  uploaded as `rig-<leg>` whatever the outcome:

  | Leg | Suites | Provider |
  |---|---|---|
  | local | `dav,api` | `local` |
  | google, microsoft, mega, rclone | `bridge,direct,shared` | that one |
  | cloud | `cloud` | `all` |

  The rclone and cloud legs use the rig's pinned rclone download. The reader's build
  cache is per machine (rig/reader.rs), so a leg runs `vite build` once more over the
  install it unpacked. Run as CI runs them on a developer machine (the unpacked tar,
  the same commands): local 107 s, google 13 min (`shared` 11.7 of it), mega 16 min,
  cloud 3 min; the clouds' legs sit at the 15-minute target, `shared` being most of
  each, so the legs get 45 minutes. Doing so found two harness defects, both fixed:
  `recordRequests` waited forever on a request the old document had in flight at a
  reload (Chromium drops it at the commit with no `requestfailed`; dav/auth's reload
  spec timed out about one run in three), and `cloudFile` read whichever copy of a
  control file the tree listed first, though a root merge keeps both and Bunko goes on
  writing one (shared/ocr on mega read a stale queue file after S1); it now reads the
  newest.
- A PR that bumps `interop/reader.ref` runs the same legs: that is how a new reader is
  taken in.

**As built (I3, reader CI):** the reader's `.github/workflows/interop.yml` has a job
`rig` beside `cloudsim`, on PRs into `main`, `develop` and `feat/cloud-bunko`, nightly
(`30 4 * * *`, for `develop` and `feat/cloud-bunko`; a schedule runs the workflow file
as the default branch has it, so it starts once the job reaches `main`) and on demand.
A small `rig-refs` job lists the commits to test that have
`src/lib/interop/rig-driver.ts` (`hashFiles` on the checkout for a PR; `git cat-file` on
each branch for the schedule): with none, `rig` shows as skipped, not failed (a job's
own `if` cannot call `hashFiles`, which reads a checkout). `rig` maps `BUNKO_REF`
(default `0.8/remote-storage`) to its nightly and downloads the testing server and
cloudsim with `gh release download`; for another branch, or a nightly with an asset or
a `bunko_sha` missing, it builds both from source. Bunko is checked out at the
nightly's `bunko_sha` (else at `BUNKO_REF`), so the runner and the specs are the ones
the binaries were built with. It then runs `cargo xtask interop rig all --provider all`
with `--reader-dir` on the reader's own checkout and uploads `bunko/target/interop/rig/`
whatever the outcome.

## 10. Milestones

| M | Scope | Exit |
|---|---|---|
| I0 | The large-catalog scenario on its own (generated library, Rust-side timings plus one Playwright check of the catalog page and a series view), with the §8.3 catalog budgets enforced in CI; then find and fix the live slowness against it | The generated library reproduces the slow catalog and series views; after the fix, they meet their budgets and CI fails if they regress |
| I1 | Contract fixtures both ways (`interop/contract/`, §5); CORS preflight tests; reader unit tests moved onto them | A deliberate field rename on either side fails the other side's tests |
| I2 | cloudsim (Drive, OneDrive, MEGA) with control API; bunko-remote mock tests moved onto it; reader base-URL override and token injection | Bunko and the reader, each alone, pass their provider suites against cloudsim (built 2026-10-28, §6 "As built") |
| I3 | Rig runner, rclone, the `dav`, `api`, `bridge`, `direct`, `shared`, `cloud` suites; CI on both repos; nightly build. The browser rig's reader (`--mode interop`) must install the gapi REST shim instead of loading gapi from apis.google.com (§11) | Both repos' PRs run the rig; the M7 checks pass on every provider in cloudsim and on rclone. Built 2026-10-30 (§7.1 and §9 "As built", `plans/2026-10-29-i3-rig.md`): every suite and scenario S1–S9 green locally on every provider, rclone through an installed `v1.75.2`, `pending-reader-caps.txt` empty; the CI jobs have not run yet (nothing is pushed, and `reader.ref` waits for a pushed reader) |
| I4 | netsim, the reader's performance marks, the `perf` suite, budgets | Budgets enforced in CI; the §10 table checked and updated with measured numbers |
| I5 | Live runs with sandbox cleanup and token upkeep | A week of green nightly runs (Drive, OneDrive) and two weekly MEGA runs, each leaving the account as it found it |

I0 comes first and needs nothing else. I1 and I2 can run in parallel; I3 needs both; I4
needs I3 (and takes in I0's scenario); I5 needs I3.

## 11. Risks and open items

- **cloudsim fidelity**: a simulator only proves what it models. Live runs (I5) are the
  check; any live-only failure becomes a cloudsim case.
- **Reader test hooks shipping by accident**: the overrides must vanish from production
  builds; a build check in the reader's CI asserts the production bundle contains none of
  the hook names.
- **Google refresh-token lifetime** for the live Drive run (§7.2).
- **CI time and flakiness** of browser runs; the 15-minute target and the retry rule in
  §8.3 are the guard.
- **`megajs` gateway override**: confirmed; megajs 1.3.9 takes a `gateway` option, and
  the saved session keeps it (I2).
- **A hooked browser build still loads the real gapi** from apis.google.com: the hooks
  redirect Drive's REST and upload calls, not `gapi.client`. I3's browser rig must install
  the REST shim (`src/lib/interop/gapi-rest-shim.ts`) in place of loading gapi. Done in
  I3: the rig's config sets `gapiShim`, and the fixtures answer both Google scripts
  empty.
- **D26, a reader's Google logout signing Bunko out** (owner action): revocation being
  project-wide can only be seen on live accounts, so the owner checks it by hand on the
  live test accounts. The rig's `dav/auth` covers Bunko's own token revocation at logout,
  not Google's.

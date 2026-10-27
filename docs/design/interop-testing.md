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
5. **Scenarios**: Playwright specs (kept in this repo under `interop/e2e/`, run with the
   pinned reader's Playwright install) plus Rust-side scenarios for Bunko ↔ cloud.

Third-party pieces: `rclone serve webdav` (a single static binary, downloaded at a pinned
version) as the plain WebDAV cloud, and Chromium through Playwright.

## 5. Contract fixtures (layer 1: every PR, both repos, seconds to run)

**Bunko → reader.** A Bunko test (`crates/bunko-server/tests/contract_export.rs`) drives a
seeded server through every endpoint and file the reader reads, and writes the responses
to `interop/contract/bunko/`:

- `/login/api/{check,token,me}`, `/api/nav/config`, `/api/stats`, `/catalog/api/*`,
  `/queue/api/status`, the volume manifest;
- PROPFIND Depth 0 and 1 of the library root, a series and a volume folder;
- `.mokuro-queue.json` (with `done[]`, `truncated`), `.mokuro-bunko.json` in each lease
  state (`claiming`, held, `released`), `series.json`, `catalog.json`;
- error bodies the reader branches on (401 with each `WWW-Authenticate`, 403, 404, 409,
  412, 423, 429 with `Retry-After`, 503 `storage-lease-claiming`, 507).

The test fails if the output differs from the committed files (the usual golden rule:
regenerate with `BUNKO_BLESS=1`, and the diff shows in the PR). The reader's unit tests
replace their hand-written fixtures (`catalog/__fixtures__/mokuro-queue.ts` and friends)
with these files, read from the pinned Bunko copy (§9).

**Reader → Bunko.** The interop e2e run records every request the reader sends to Bunko
(method, path, headers that matter, body shape) into `interop/contract/reader/`.
`crates/bunko-server/tests/contract_replay.rs` replays them against a fresh server and
checks the status codes and response shapes. Headers covered on purpose: `Authorization`
(Basic with UTF-8 and Latin-1 bytes, Bearer), `If-Match`, `If-None-Match`, `Depth`,
`Origin`, and the CORS preflight `OPTIONS`.

Only stable fields are kept (no dates, ETags or ids; these are replaced with typed
placeholders), so the fixtures change only when the contract does.

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
| `shared` | reader and Bunko on one cloudsim folder (each provider) and rclone | the M7 exit checks: Bunko progress for a volume the reader uploaded; sidecar pulled on `done[]`; reader stops writing `series.json`/`catalog.json` while the marker is fresh and resumes 2 × ttl after Bunko stops or at once on `released`; edits go up as `<Vol>.edit.mokuro` and never as the main sidecar; a Bunko case merge shows as a rename with progress kept |
| `cloud` | Bunko ↔ cloudsim and rclone (no reader) | the Rust-side Bunko ↔ cloud scenarios at process level: cold start, adoption, outbox under faults, lease handover, quota pauses |
| `perf` | all of the above, shaped | §8 |

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
  | requests needed before first cards show | ≤ 8 |

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
- The reader's unit tests read `interop/contract/bunko/` from that checkout (the reader's
  test setup copies them in; they are not committed to the reader repo, so there is one
  copy).

Run time target: the rig under 15 minutes per PR on ubuntu.

## 10. Milestones

| M | Scope | Exit |
|---|---|---|
| I0 | The large-catalog scenario on its own (generated library, Rust-side timings plus one Playwright check of the catalog page and a series view), with the §8.3 catalog budgets enforced in CI; then find and fix the live slowness against it | The generated library reproduces the slow catalog and series views; after the fix, they meet their budgets and CI fails if they regress |
| I1 | Contract fixtures both ways; CORS preflight tests; reader unit tests moved onto them | A deliberate field rename on either side fails the other side's tests |
| I2 | cloudsim (Drive, OneDrive, MEGA) with control API; bunko-remote mock tests moved onto it; reader base-URL override and token injection | Bunko and the reader, each alone, pass their provider suites against cloudsim |
| I3 | Rig runner, rclone, the `dav`, `api`, `bridge`, `direct`, `shared`, `cloud` suites; CI on both repos; nightly build | Both repos' PRs run the rig; the M7 checks pass on every provider in cloudsim and on rclone |
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
- **`megajs` gateway override**: to confirm in planning.

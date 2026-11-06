# I0: large-catalog performance gate — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A CI job that builds Bunko, serves a generated 2,000-series / 20,000-volume library, times the catalog and series views (warm, cold, under write load, and in Chromium), and fails when a budget in `interop/budgets.toml` is missed.

**Architecture:** A new `xtask interop` subcommand holds the rig runner. It generates the library, runs the release `mokuro-bunko` binary as a child process with an isolated config, and drives it over loopback with a minimal std-only HTTP/1.1 client. Load comes from threads that PUT sidecars and PROPFIND the root over WebDAV, which is what invalidates the library index live. A Playwright script (`interop/e2e/catalog-perf.mjs`) times the catalog page and a series view. Results go to a JSON file and are checked against the budgets.

**Tech Stack:** Rust 2024 (xtask: anyhow, clap, serde, serde_json, zip, plus `toml` 0.8, which is already in Cargo.lock). Node 22 with `playwright` pinned in `interop/package.json`. GitHub Actions on ubuntu-latest.

**Spec:** `docs/design/interop-testing.md` §8.1 (large catalog), §8.3 (budgets), §10 (I0 row).

## Global Constraints

- Budgets live in `interop/budgets.toml`; changing one is a visible PR edit.
- Timings use the median of 5 runs. One retry of the whole scenario is allowed before failing.
- Timing budgets run on Linux only.
- Catalog budgets (lan, large catalog):
  - library ≤ 25 ms warm / ≤ 250 ms cold;
  - series ≤ 5 ms warm / ≤ 50 ms cold;
  - first cards ≤ 400 ms;
  - small series rendered ≤ 150 ms;
  - ≤ 8 requests before first cards;
  - series at 2,000 series no more than 20 % slower than at 200 (with a 1 ms floor, so sub-millisecond noise cannot fail it).
- No new crates outside Cargo.lock. No network in the Rust side. `catalog.enrich_community: false`, so nothing calls AniList.
- Never print the generated admin password; it is written to a 0600 file inside the run dir.
- No private deployment names in code, docs or commits.

## Review Focus

1. **A server that never finishes the first compile** (a broken build): the runner must time out with the server log tail, not hang CI. Pinned by `wait_ready` (a 300 s cap) in Task 2.
2. **A port already in use**: the runner picks a free port by binding `127.0.0.1:0`. Pinned in Task 2.
3. **Load threads still running when a measurement fails**: they stop through a shared flag and are joined before the server is killed. Pinned in Task 3.
4. **A budget key with no measured metric (a typo)**: this is an error, not a silent pass. Pinned by a unit test in Task 4.
5. **The stall coming back** (an index walk inside the request): the under-load series p95/max budget must fail on the pre-247a668 index. Checked by hand in Task 6.

---

### Task 1: Library generator

**Files:**
- Create: `crates/xtask/src/interop/mod.rs`, `crates/xtask/src/interop/genlib.rs`, `interop/fixtures/page.jpg` (a 1 KB JPEG)
- Modify: `crates/xtask/src/main.rs` (the `Interop` subcommand), `crates/xtask/Cargo.toml` (`toml = "0.8"`)

**Interfaces:**
- Produces: `genlib::generate(root: &Path, series: usize, volumes: usize) -> Result<Generated>`, where `Generated { library: PathBuf, small_series: String, volumes: usize }`. The series are `Series 0000`…, plus one `Small Series` with 3 volumes. Each volume is a stored-zip `.cbz` of 4 pages. Every other volume gets a `.mokuro` sidecar, so the compile has OCR text to count.

- [ ] Write a unit test: generating 3 series × 2 volumes gives 4 folders and 9 `.cbz` files, each a readable zip with 4 entries, and a `.mokuro` beside every other one.
- [ ] Implement with the `zip` crate (Stored). The sidecar JSON is the minimal shape the compiler reads (`version`, `title`, `volume`, `volume_uuid`, `pages[]` with `img_path`, `img_width`, `img_height` and one `blocks` entry with `lines`).
- [ ] `cargo test -p xtask interop::genlib` passes.

### Task 2: Server harness

**Files:** Create `crates/xtask/src/interop/server.rs`, `crates/xtask/src/interop/http.rs`

**Interfaces:**
- `http::request(addr, method, path, headers: &[(&str,&str)], body: &[u8]) -> Result<Reply { status: u16, body: Vec<u8>, elapsed: Duration }>`. Each call is one `Connection: close` request; elapsed runs from connect to the last byte.
- `server::Server::start(bin, run_dir, library) -> Result<Server>` writes a config (port from `127.0.0.1:0`, `catalog.enabled`, `enrich_community: false`, `ocr.local_processing: false`, `ocr.autobench: false`, `registration.mode: disabled`). It also sets isolated `XDG_*` and `TMPDIR`, spawns `serve`, waits for `/setup/`, completes setup with a random admin password (saved 0600, never printed), and returns `{ addr, auth_header }`.
- `Server::wait_ready(expected_series)` polls `/catalog/api/library` until every row has `total_pages`, capped at 300 s; on timeout it fails with the last 40 log lines.
- `Server::restart()` stops the server (SIGINT via `kill -INT` on unix, then a 20 s wait, then kill) and starts it again on the same dirs. The setup is already done.

- [ ] Unit-test `http` against a tiny `TcpListener` thread, both for status parsing and for a chunked body.
- [ ] Implement.

### Task 3: Catalog scenario

**Files:** Create `crates/xtask/src/interop/catalog.rs`

**Interfaces:** `catalog::run(opts) -> Result<Metrics>`, where `Metrics = BTreeMap<String, f64>` (ms or counts). The keys:
- `library_warm_ms`, `series_warm_ms`: the median of 30 requests after 5 warm-ups;
- `library_cold_ms`, `series_cold_ms`: the first request after `restart()` and readiness, taken without warming;
- `series_load_p95_ms`, `series_load_max_ms`, `library_load_p95_ms`: 40 s of sampling, which is past the 30 s index lifetime. Meanwhile one thread PUTs a `.mokuro` sidecar to a rotating volume over `/mokuro-reader/...` every 100 ms, and one thread PROPFINDs Depth 1 of `/mokuro-reader/` every 250 ms;
- `series_small_ratio`: the series median at 2,000 series ÷ at 200 series. This is a second, smaller library and server, compared with `max(a, b + 1 ms)` semantics in the check;
- browser metrics, from Task 5, when `--browser` is set.

Load threads share an `AtomicBool` stop flag, and they are joined on every exit path (a guard struct whose `Drop` sets the flag and joins).

- [ ] Implement it. A `--quick` flag (200 series × 3 volumes, 5 s of load) exists for local smoke runs.

### Task 4: Budgets, retries, results

**Files:** Create `interop/budgets.toml`, `crates/xtask/src/interop/budgets.rs`

**Interfaces:** `budgets::check(budgets: &Budgets, metrics: &Metrics) -> Vec<Breach>`. `Budgets` is `{ catalog: BTreeMap<String, f64> }` (maximums). An unknown key is a breach, with the reason "no such metric".

- [ ] Unit tests: a pass, a breach, an unknown key, and a metric with no budget (allowed, but reported).
- [ ] Wire the runner: run the scenario 5 times; per metric take the median (the counts are single-valued); check; on breach run all 5 again once; fail if it still breaches. Write `target/interop/catalog-perf.json` with every run's raw numbers.

### Task 5: Browser timings

**Files:** Create `interop/package.json`, `interop/package-lock.json`, `interop/e2e/catalog-perf.mjs`

The script reads `BUNKO_URL` and `SMALL_SERIES` and prints one JSON line: `catalog_first_cards_ms`, `catalog_requests_before_cards`, `series_rendered_ms`. "First cards" means the first `.series-card` (or the catalog's card selector, read from `web/catalog/catalog.js`) in the DOM. "Series rendered" means navigating to the small series' view, through to its volume list rendered. Each runs in a fresh browser context, so nothing is cached.

- [ ] The runner calls `node interop/e2e/catalog-perf.mjs` when `--browser` is set and merges the metrics in.

### Task 6: CI job and the regression check

**Files:** Create `.github/workflows/interop.yml`

The job runs on PR and on push to `main` and `0.8/**`, on ubuntu-latest:
1. `cargo build --release -p mokuro-bunko --no-default-features --locked`
2. setup-node 22
3. `npm ci` in `interop/`
4. `npx playwright install --with-deps chromium`
5. `cargo xtask interop catalog-perf --bin target/release/mokuro-bunko --browser`
6. upload `target/interop/` as an artifact.

- [ ] Run it locally in full and record the numbers.
- [ ] In a scratch worktree, restore `crates/bunko-library/src/index.rs` from `247a668^` (the inline walk) and run again. The under-load series budget must fail. Record both outputs.
- [ ] Push, and read the CI run. Set any budget that has no spec value (the under-load ones) from the CI baseline plus 25 %, in the same PR.
- [ ] CHANGELOG is not touched (a test-only change).

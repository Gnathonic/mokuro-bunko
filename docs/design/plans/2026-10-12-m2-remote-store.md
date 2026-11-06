# M2: RemoteStore, WebDAV backend and headless cloud mode — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Bunko server whose library lives on a WebDAV server: it indexes the remote `mokuro-reader/` folder, OCRs new volumes (locally or through processors), writes sidecars, covers, `series.json`/`catalog.json`, the queue file and the marker back, and survives restarts, outages and a reader writing the same folder.

**Architecture:** A new crate `bunko-remote` implements `bunko_store::LibraryStore` as `RemoteStore<B: Backend>`: a disposable `index.sqlite` fed by the backend's change feed, a durable `state.sqlite` (ledger, outbox, submissions, conflicts, lease), a small-file blob cache, an archive spool, and an outbox worker. The server mounts it at a synthetic root `<base>/cloud/<store-id>/library` when `storage.library.backend != local`. Everything above the shim (compiler, scheduler, thumbnails, DAV) runs unchanged except for the listed integration points.

**Tech Stack:** Rust 2024, tokio (a 2-worker store runtime), rusqlite 0.37 (bundled), reqwest 0.12 + rustls, quick-xml 0.37, sha2, unicode-normalization.

**Spec:** `docs/design/0.8-remote-storage.md` §5 (RemoteStore), §6 (Backend + WebDAV), §8 (OCR integration), §9 (status files), §10 (bridge), §11 (config), §13 (testing), §14 (M2 row), D1–D26.

## Global Constraints

- `RemoteStore` metadata methods never touch the network (index only); content/mutation methods `Handle::block_on` the store runtime and must never be called from inside an async task (debug-assert via a thread-local re-entry flag).
- `series.json`/`catalog.json` written over RemoteStore are byte-identical to LocalStore output (golden tests run on `STORE=fake`).
- Effective mtime (D3): `mtime_ns = trunc_G(base) + (H(pid‖cver) mod G)`, G from `BackendCaps::mtime_granularity`; Bunko never sets provider mtimes.
- Index names are NFC; lookup byte-exact; folding stays in PathCase; `caps().case_sensitive == true` for RemoteStore.
- `index.sqlite` is disposable (rebuild deletes it, `blobs/`, `spool/`); `state.sqlite`, `outbox/`, `submissions/` survive.
- Bunko never deletes a cloud file in reaction to a feed event (case merge excepted, task 11); provider trash for anything Bunko did not write (D16).
- No private deployment names in code, docs or commits.
- All new deps Apache/MIT compatible; nothing new outside the workspace list except `unicode-normalization` (already used).
- `cargo clippy --workspace --all-targets -D warnings` and `cargo test --workspace` stay green after every task.

## Review Focus

1. **A reader rewriting `series.json` while Bunko compiles** (lost update): Bunko must merge the reader's facts, never overwrite them silently — task 8 scenario test `reader_series_write_between_preflight_and_commit_is_merged`.
2. **Provider outage mid-OCR**: the result must survive in the outbox and land after recovery, and the queue must hold with `storage-offline` — task 5 `deferred_publish_survives_outage_and_restart`, task 10 `outage_holds_claims`.
3. **Restart with uploads in flight** (kill between UPLOADING and COMMITTED): exactly one copy of each OCR result in the cloud — task 5 `crash_at_each_state_yields_one_copy`.
4. **The reader deletes a series folder in the cloud**: Bunko forgets the volumes after the tombstone grace, keeps nothing stale in the queue, never recreates the folder — task 3 `cloud_delete_runs_hooks_after_grace` + task 5 `replay_never_recreates_missing_parent`.
5. **A large first sync (thousands of volumes)**: index build is streamed (no whole-tree RAM spike), and the scheduler does not block on network reads — task 3 `full_sync_10k_nodes_streams` and task 10 `arrivals_compute_in_background`.

---

## Crate layout

```
crates/bunko-remote/
  Cargo.toml                features: testing (FakeBackend), webdav (default)
  src/lib.rs                re-exports
  src/backend.rs            Backend trait + RNode, Cursor, ChangePage, RemoteBody, UploadTarget,
                            UploadSource, OpCtx, BErr, BackendCaps, AccountInfo, ServerTime
  src/fake.rs               FakeBackend (feature testing): profiles drive_like, webdav_cas,
                            webdav_plain, onedrive_like, mega_like; fault injection
  src/names.rs              NFC, fold, effective mtime (D3)
  src/index.rs              index.sqlite: schema, ingest, lookup, merged roots, duplicates
  src/state.rs              state.sqlite: meta, ledger, outbox, submission, conflict, local_row
  src/feed.rs               engine: LOCATE_ROOTS, FULL_SYNC, STEADY, tombstones, D19, events
  src/cache.rs              blob cache (≤ 4 MiB, content-addressed), spool (archives), zipdir cache
  src/file.rs               RemoteFile impls: cached small file, pinned ranged archive reader,
                            staging file (write side)
  src/outbox.rs             outbox worker: PREPARED → … → CONFIRMED, retries, coalescing, D22
  src/governor.rs           cost budget, priorities, Retry-After pause, health transitions
  src/store.rs              RemoteStore<B>: LibraryStore impl, the store runtime, re-entry guard
  src/status.rs             queue-file + marker writers, lease protocol (D9/D10)
  src/submissions.rs        foreign series.json/catalog.json capture (D8, D18)
  src/webdav/{mod,xml,probe}.rs   WebDAV driver (hand-rolled reqwest + quick-xml)
  tests/conformance.rs      bunko-store conformance suite over Fake profiles
  tests/scenarios.rs        crash points, lease, cloud-side changes, outage
  tests/webdav_bunko.rs     WebDAV driver against an in-process Bunko server
```

Server-side changes are listed per task (bunko-core config, bunko-server app/runtime/ocr/thumbs, bunko-dav read.rs).

---

### Task 1: `Backend` trait, core types, FakeBackend

**Files:** create `crates/bunko-remote/{Cargo.toml,src/lib.rs,src/backend.rs,src/fake.rs}`, `tests/fake_backend.rs`; root `Cargo.toml` workspace dep.

**Interfaces (produced, used by every later task):**

```rust
pub type Pid = String;                 // provider id; WebDAV: the path relative to the mount URL

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RNode {
    pub pid: Pid,
    pub parent: Option<Pid>,           // None only for a root candidate's own parent unknown
    pub name_raw: Vec<u8>,             // provider bytes (may be NFD)
    pub is_dir: bool,
    pub size: u64,
    pub base_mtime_ms: i64,            // the field the reader lists (D3 table)
    pub created_ms: i64,               // server-assigned; 0 when unknown
    pub version: String,               // changes on any metadata/content change
    pub cver: String,                  // changes on content change only
    pub pin: Option<String>,           // Drive headRevisionId / strong ETag
    pub hash: Option<(HashKind, String)>,
    pub trashed: bool,
}
pub enum HashKind { Sha256, Md5, QuickXor, Sha1, NextcloudSha1 }
pub struct Cursor(pub String);
pub enum ChangeItem { Upsert(RNode), Removed { pid: Pid } }
pub enum ChangePage { Page { items: Vec<ChangeItem>, next: Cursor, more: bool }, CursorLost }
pub struct ServerTime(pub i64 /* unix ms from the Date header */);
pub struct AccountInfo { pub id: String, pub label: String }
pub struct BackendCaps {
    pub cas_create: bool, pub cas_overwrite: bool, pub atomic_replace: bool, pub server_copy: bool,
    pub range: bool, pub pinned_read: bool, pub push: bool, pub hash_kind: Option<HashKind>,
    pub mtime_granularity_ns: i64, pub simple_upload_max: u64, pub chunk_multiple: u64,
    pub path_ids: bool,                // WebDAV: pid == path, moves change pid
}
pub enum UploadTarget<'a> { New { parent: &'a str, name: &'a str }, Replace { pid: &'a str, if_cver: Option<&'a str> } }
pub enum UploadSource<'a> { File { path: &'a std::path::Path, size: u64, sha256: [u8; 32] } }
pub struct OpCtx { pub op_id: String, pub resume: Option<String> /* persisted session URL etc. */ }
pub type RemoteBody = std::pin::Pin<Box<dyn futures_util::Stream<Item = std::io::Result<bytes::Bytes>> + Send>>;
#[derive(Debug)]
pub enum BErr { Throttled(Option<std::time::Duration>), Transient(String), AuthExpired, AuthRevoked,
    NotFound, PreconditionFailed, NameConflict, StorageFull, TransferQuota(Option<std::time::Duration>),
    CursorLost, TooLarge, InvalidName, Unsupported, Fatal(String) }

pub trait Backend: Send + Sync + 'static {
    fn caps(&self) -> BackendCaps;
    fn account(&self) -> impl Future<Output = Result<AccountInfo, BErr>> + Send;
    fn locate_roots(&self, name: &str) -> impl Future<Output = Result<Vec<RNode>, BErr>> + Send;
    fn create_root(&self, name: &str) -> impl Future<Output = Result<RNode, BErr>> + Send;
    fn start_cursor(&self) -> impl Future<Output = Result<Cursor, BErr>> + Send;
    fn list_all(&self, roots: &[Pid], sink: &mut (dyn FnMut(RNode) + Send)) -> impl Future<Output = Result<(), BErr>> + Send;
    fn list_dir(&self, dir: &str, sink: &mut (dyn FnMut(RNode) + Send)) -> impl Future<Output = Result<(), BErr>> + Send;
    fn find_named(&self, parent: &str, name: &str) -> impl Future<Output = Result<(Vec<RNode>, ServerTime), BErr>> + Send;
    fn changes(&self, c: &Cursor) -> impl Future<Output = Result<ChangePage, BErr>> + Send;
    fn wait_change(&self, c: &Cursor, max: std::time::Duration) -> impl Future<Output = Result<(), BErr>> + Send;
    fn stat(&self, pid: &str) -> impl Future<Output = Result<Option<RNode>, BErr>> + Send;
    fn read(&self, pid: &str, pin: Option<&str>, range: Option<(u64, Option<u64>)>) -> impl Future<Output = Result<RemoteBody, BErr>> + Send;
    fn upload(&self, to: UploadTarget<'_>, src: UploadSource<'_>, op: &mut OpCtx) -> impl Future<Output = Result<RNode, BErr>> + Send;
    fn mkdir(&self, parent: &str, name: &str, op: &OpCtx) -> impl Future<Output = Result<RNode, BErr>> + Send;
    fn update(&self, pid: &str, parent: Option<&str>, name: Option<&str>, if_cver: Option<&str>) -> impl Future<Output = Result<RNode, BErr>> + Send;
    fn copy(&self, pid: &str, parent: &str, name: &str) -> impl Future<Output = Result<RNode, BErr>> + Send;
    fn trash(&self, pid: &str, if_cver: Option<&str>) -> impl Future<Output = Result<(), BErr>> + Send;
    fn delete_permanent(&self, pid: &str) -> impl Future<Output = Result<(), BErr>> + Send;
    fn quota(&self) -> impl Future<Output = Result<Option<bunko_store::Capacity>, BErr>> + Send;
}
```

`FakeBackend` (feature `testing`): an in-memory tree behind a `parking_lot::Mutex`, a monotonically increasing change log (cursor = log index), a server clock (`tokio::time::Instant`-based ms), and a profile: `drive_like()` (opaque ids, duplicate names allowed, no CAS, pinned reads by revision), `webdav_cas()` / `webdav_plain()` (path ids, name-unique, case-sensitive; CAS on/off), `onedrive_like()` (ids, case-insensitive unique names, CAS), `mega_like()` (ids, replace = new node, push). Fault knobs: `set_latency`, `fail_next(op, BErr)`, `throttle_for(dur)`, `lose_cursor()`, `reorder_changes(bool)`, `duplicate_deliveries(bool)`. Test helpers that act as "the reader": `reader_put(path, bytes)`, `reader_delete(path)`, `reader_rename(from, to)`, `reader_mkdir(path)`.

- [ ] Step 1: tests in `tests/fake_backend.rs`: create/list/changes round trip; cursor monotonic; `lose_cursor` gives `CursorLost`; drive_like allows two files named `a` in one folder; webdav_plain path-id move changes pid; ranged read returns the slice; `fail_next` returns the injected error once.
- [ ] Step 2: run → fail (crate missing). Step 3: implement. Step 4: run → pass; clippy clean. Step 5: commit `bunko-remote: Backend trait and FakeBackend`.

### Task 2: names + index.sqlite

**Files:** `src/names.rs`, `src/index.rs`, `tests/index.rs`.

**Interfaces:**
```rust
pub fn nfc(raw: &[u8]) -> String;                  // lossy UTF-8 then NFC
pub fn fold(name: &str) -> String;                 // bunko_store fold rule: NFC + lowercase (same as bunko-dav fold_name)
pub fn effective_mtime_ns(base_ms: i64, granularity_ns: i64, pid: &str, cver: &str) -> i128; // D3
pub struct Index { /* rusqlite::Connection in WAL */ }
impl Index {
    pub fn open(path: &Path) -> rusqlite::Result<Index>;
    pub fn generation(&self) -> u64; pub fn bump_generation(&mut self) -> u64;
    pub fn meta_get(&self, k: &str) -> Option<String>; pub fn meta_set(&mut self, k: &str, v: &str);
    pub fn set_roots(&mut self, roots: &[RNode]);                // merged root (D5): oldest root = target
    pub fn upsert(&mut self, tx: &rusqlite::Transaction, n: &RNode, gen: u64, granularity_ns: i64) -> Vec<IndexChange>;
    pub fn remove(&mut self, tx: &rusqlite::Transaction, pid: &str) -> Vec<IndexChange>;
    pub fn sweep_older_than(&mut self, tx: &rusqlite::Transaction, gen: u64, keep: &dyn Fn(&str) -> bool) -> Vec<IndexChange>;
    pub fn lookup(&self, rel: &RelPath) -> Option<Entry>;       // byte-exact per segment
    pub fn children(&self, eid: i64) -> Vec<Entry>;
    pub fn variants(&self, parent_eid: i64, fold: &str) -> Vec<Entry>; // oldest created first (case_winner)
    pub fn entry_by_pid(&self, pid: &str) -> Option<Entry>;
    pub fn path_of(&self, eid: i64) -> Option<RelPath>;
    pub fn winner_node(&self, eid: i64) -> Option<RNode>;
}
pub struct Entry { pub eid: i64, pub parent: Option<i64>, pub name: String, pub is_dir: bool, pub size: u64,
                   pub mtime_ns: i128, pub cver: Option<String>, pub own: bool, pub winner_pid: Option<String>,
                   pub pending_op: Option<String>, pub members: u32, pub created_ms: i64 }
pub enum IndexChange { Added { rel: RelPath, is_dir: bool }, Removed { rel: RelPath, is_dir: bool },
                       Moved { from: RelPath, to: RelPath, is_dir: bool }, ContentChanged { rel: RelPath } }
pub struct RelPath(pub Vec<String>);   // NFC segments under the merged root
```
Schema exactly as spec §5.1 (`meta`, `entry`, `node`, `tombstone`, `blob`, `cached`, `zipdir`, `spool`, `api_log`). Rules: NFC-equal folders → one entry, union of children, winner = oldest `created_ms` then smallest pid; NFC-equal files → newest `created_ms` wins, others `role=2` (conflict `dup-file`); case variants are distinct entries; file vs folder same name → folder wins, file → conflict `file-vs-dir`; folder `mtime_ns` bumped to `max(prev+1000, now)` on child add/remove/rename; nested `mokuro-reader` → `nested-root` conflict.

- [ ] Tests: NFD name stored NFC with `name_raw` kept; duplicate folders union + winner; duplicate files newest-created wins and loser reported; case variants distinct + `variants()` oldest first; effective mtime deterministic, `int(mtime)` equals listed seconds; folder mtime bumps; move by pid emits `Moved`; sweep removes only older gens and respects `keep`.
- [ ] Commit `bunko-remote: index`.

### Task 3: feed engine, tombstones, events

**Files:** `src/feed.rs`, `tests/feed.rs`.

**Interfaces:** `pub struct Feed<B>` with `async fn run(self, shutdown)`; `pub enum EngineState { Init, LocateRoots, FullSync, IngestFacts, ClaimLease, Replay, Steady, Suspect }`; emits `bunko_store::WatchEvent` (absolute paths under the store root) to the store's sinks, and `CloudChange { Moved{from,to,is_dir}, Removed{rel,is_dir} (after tombstone expiry), ContentChanged{rel} , ForeignJson{rel, pid, cver} }` to a `CloudHooks` trait object the server installs (task 9). Implements spec §5.3 (FULL_SYNC mark & sweep: cursor taken before enumeration; sweep only after complete enumeration, children first; skips ledger-newer and `pending_op`; D19 >20 % removal → `Suspect`), STEADY batches applied with their cursor in one transaction, echo suppression on exact ledger `(parent_pid, name, cver)`, startup `Rescan`, tombstones with `quiet_s` and `recently_removed`, poll cadence active/idle per provider (configurable), `wait_change` for push backends.

- [ ] Tests (FakeBackend, paused tokio time): first sync emits Added for every entry and Rescan; reader upload → Added; reader rename folder → Moved hook immediately; reader delete → Removed hook only after grace; delete+recreate same pid within grace = move; `lose_cursor` → full resync with no spurious Removed; 25 % of files vanish in one pass → Suspect, nothing removed; own echo produces no event; `full_sync_10k_nodes_streams` (10k nodes, peak RSS of the process stays below a set bound or the sink is consumed incrementally — assert `list_all` sink is drained without collecting into a Vec in the engine); `cloud_delete_runs_hooks_after_grace`.
- [ ] Commit `bunko-remote: feed engine`.

### Task 4: RemoteStore read path (+ caches, spool, pinned reads)

**Files:** `src/cache.rs`, `src/file.rs`, `src/store.rs`, `tests/read_path.rs`.

**Interfaces:**
```rust
pub struct RemoteStoreConfig { pub root: PathBuf /* synthetic */, pub dir: PathBuf /* <base>/cloud/<id> */,
    pub folder: String, pub cache_mb: u64, pub spool_gb: u64, pub quiet_s: u64,
    pub poll_active: Duration, pub poll_idle: Duration, pub mount_id: u64 }
pub struct RemoteStore<B: Backend> { .. }
impl<B: Backend> RemoteStore<B> {
    pub fn open(cfg: RemoteStoreConfig, backend: B) -> io::Result<Arc<Self>>;   // starts the runtime + feed
    pub fn set_hooks(&self, hooks: Arc<dyn CloudHooks>);
    pub fn engine_state(&self) -> EngineState;
    pub fn wait_steady(&self, timeout: Duration) -> bool;                       // tests + startup
}
impl<B: Backend> bunko_store::LibraryStore for RemoteStore<B> { .. }          // caps().remote = true, case_sensitive = true
```
`metadata`/`read_dir`/`canonicalize` from the index (`Meta::Remote` with `dev = mount_id`, `ino = eid`, `own`); `read_link` → `InvalidInput`; `open`: files ≤ 4 MiB from the blob cache (filled by one pinned read, hash-verified when the provider has a hash), larger files a `RemoteFile` doing pinned ranged reads with the zip central-directory cache (`zipdir` table) and a 256 KiB block cache keyed `(pid, cver, block)`; `local_path`: progressive spool `spool/<pid>-<cver>.part`, size + hash verified on completion, pinned `SpoolPin`; `content_sha256` from the blob cache.

- [ ] Tests: metadata/read_dir of a fake tree; small read hits network once then cache; archive read of one zip member costs ranged reads only (count backend calls); a replace between CD fetch and member read is detected (pin mismatch → error, cache untouched); `local_path` spools once for two concurrent callers; read_dir order and names match the index; `bunko_store::fs` routing through a mounted RemoteStore works.
- [ ] Commit `bunko-remote: RemoteStore read path`.

### Task 5: state.sqlite, write path, outbox worker

**Files:** `src/state.rs`, `src/outbox.rs`, store write methods, `tests/write_path.rs`, `tests/scenarios.rs` (crash points).

Implements spec §5.7 and §5.10: `open_with` returns a local staging file under `<dir>/staging/` (a `RemoteFile` wrapping `std::fs::File`, published on `publish` — never on drop); `publish(staged, dest, intent)`: Sync → run PREPARED…COMMITTED inline (block_on) and return after COMMITTED; Deferred → PREPARED+STAGED (bytes into `outbox/<op>`, fsync, sha256/md5), index row with `pending_op` + `own`, emit WatchEvent, return `Published{op}`; worker drains HELD→PREFLIGHT→UPLOADING→COMMITTED→(SUPERSEDE_OLD)→CONFIRMED; coalescing by destination for `Compiler`/`Status`; `depends_on` (D22: compiler op for series S waits for non-compiler Deferred ops under S; `catalog.json` waits for every `series.json` op); PREFLIGHT conflicts per origin (§5.7 table: Ocr CreateNew → Discard with audit hook; Thumbs → drop ours; Compiler → submission + recompile; Upgrade → re-judge; Dav → 412); parent missing → never recreate (CONFLICT); ledger row at COMMITTED (incl. moves/trashes rewriting `(parent_pid,name)`); FAILED only for permanent errors; retries with backoff counted only while Online; startup replay table §5.10; `rename`/`remove_*`/`create_dir_all`/`copy` as Sync ops; barrier for MOVE/DELETE over pending ops under the path.

- [ ] Tests: Sync publish invisible to readers until COMMITTED; Deferred publish visible at once with `own`; `deferred_publish_survives_outage_and_restart`; `crash_at_each_state_yields_one_copy` (abort after each state transition, reopen, drain, assert exactly one remote file); `replay_never_recreates_missing_parent`; coalescing keeps only the newest `series.json`; D22 ordering (series.json leaves after the sidecar it references); OCR CreateNew vs a foreign sidecar → discard callback, no `_n` file; DELETE trashes (fake records trash vs permanent).
- [ ] Commit `bunko-remote: write path and outbox`.

### Task 6: governor and health

**Files:** `src/governor.rs`, wiring in store/feed/outbox, `tests/governor.rs`.

Cost budget per backend (configurable units/min), concurrency semaphore, priorities (interactive > lease/status > ocr spool > background), global `pause_until` from `Retry-After`, `Health` transitions (Online / Degraded{retry_at} / Offline / NeedsReauth / Blocked(StorageFull|TransferQuota) / Suspect / LeaseHeld), `bunko_store::interactive(|| ..)` latency class returning `Unavailable` instead of waiting > 2 s (implement as a thread-local in bunko-store: add `pub fn interactive<T>(f: impl FnOnce() -> T) -> T` and `pub fn is_interactive() -> bool` to `bunko_store`), `api_log` 24 h ring with per-class units/bytes.

- [ ] Tests: 429 with Retry-After pauses all classes; budget exhaustion delays background but not interactive; NeedsReauth on AuthRevoked stops provider calls; StorageFull stops writes and keeps the outbox.
- [ ] Commit `bunko-remote: governor and health`.

### Task 7: status files and lease

**Files:** `src/status.rs`, `tests/lease.rs`.

Queue file writer (spec §9.1): takes the server's queue document builder output (a `serde_json::Value` producer injected by the server), adds `instance_id`, `truncated`, `done[]` (fed by COMMITTED Ocr ops), quantises progress to 0.1, writes only on change (ignoring `generated_at`), floors per provider (WebDAV 30 s), 64 KiB cap, as a fenced Deferred op. Marker/lease (spec §9.2) exactly: rank `(epoch desc, claimed_at asc, instance_id asc)` over all fresh marker nodes (fresh by provider clock), CLAIM_LEASE (restart resume by `instance_id`+`last_boot_id`, clone detection, write with `epoch = max+1`, wait `max(60 s, 2×feed)`, re-list, loser deletes its own node), standby (`Health::LeaseHeld`), heartbeat every `ttl_s/3`, take over, released on disconnect; marker writes are direct unfenced Sync backend calls; outbox uploads fenced on `lease_ok_at` within `ttl_s/2`.

- [ ] Tests (two RemoteStores on one Fake, paused time, ±10 min skew): exactly one holder after one heartbeat; restart resumes; cloned state dir detected; take over wins; stale holder returning steps down; queue file not rewritten when only `generated_at` changes; `done[]` only after COMMITTED.
- [ ] Commit `bunko-remote: status files and lease`.

### Task 8: submissions, first adoption, cloud hooks contract

**Files:** `src/submissions.rs`, `tests/submissions.rs`; `CloudHooks` trait in `src/feed.rs`:

```rust
pub trait CloudHooks: Send + Sync {
    fn moved(&self, from: &Path, to: &Path, is_dir: bool);          // absolute synthetic paths
    fn removed(&self, path: &Path, is_dir: bool);                  // after tombstone expiry
    fn sidecar_overwritten(&self, path: &Path);                    // foreign new version of a .mokuro/.mokuro.gz/layer
    fn series_submission(&self, series_dir: &Path, bytes: &[u8]) -> SubmissionResult; // apply_series_update
    fn ocr_discarded(&self, path: &Path);                          // foreign sidecar at install time
}
pub enum SubmissionResult { Applied, Refused(String) }
```
Foreign `series.json` (cver not in ledger, or any `series.json` at a series key with no facts row — the hook answers that) → submission row in the feed transaction, bytes pinned under `submissions/`; while unapplied, Compiler ops for that series stay HELD; INGEST_FACTS on first adoption (every `series.json`, then `catalog.json` facts for series without one).

- [ ] Tests: `reader_series_write_between_preflight_and_commit_is_merged`; first adoption applies all existing series.json before any compile write; refused submission → conflict row, compile proceeds.
- [ ] Commit `bunko-remote: submissions and adoption`.

### Task 9: WebDAV driver

**Files:** `src/webdav/{mod.rs,xml.rs,probe.rs}`, `tests/webdav_bunko.rs`.

Hand-rolled on reqwest 0.12 + rustls + quick-xml 0.37: Basic auth; `locate_roots` = PROPFIND Depth 0 on `<url>/<folder>/`; `list_all` = Depth infinity when the probe allows, else recursive Depth 1 (streamed into the sink); targeted PROPFIND body (`resourcetype getcontentlength getlastmodified getetag` + `oc:checksums`); `changes` = poll: PROPFIND Depth 1 of the root and of folders whose ETag/lastmod changed (or every series folder when the probe shows nested changes don't propagate) diffed against a cursor snapshot stored as JSON in the cursor string's backing file (`<dir>/webdav-cursor.json`, cursor = generation number); `read` = GET with Range and `If-Match: <strong etag>`; `upload` = PUT (`If-Match`/`If-None-Match: *` when probed honoured; else PUT to `.<name>.bunko-<op>.tmp` + `MOVE Overwrite: T`); `mkdir` = MKCOL; `update` = MOVE (`Overwrite: F`); `trash` = DELETE; `quota` = `quota-available-bytes`. Probe at connect: Depth infinity, Range, If-Match (deliberately wrong tag must 412), `If-None-Match: *`, case behaviour via `.bunko-probe-Aa`, nested propagation, `X-Mokuro-Put`; a Bunko server is refused unless `allow_bunko_backend_for_tests`. `http://` refused unless `allow_http`. `pid` = path relative to the folder URL; `caps.path_ids = true`; mtime granularity 1 s.

Test target: an in-process Bunko server (the test harness in `crates/bunko-server/tests/` already starts one — reuse its support module) with a user who can write; `allow_bunko_backend_for_tests = true`. Run the conformance subset over it.

- [ ] Commit `bunko-remote: WebDAV driver`.

### Task 10: server integration (headless cloud mode)

**Files:** `crates/bunko-core/src/config.rs` (+ `storage.library` section per spec §11, env overrides), `crates/bunko-core/src/storage.rs` (skip library/thumbnail dirs and the writability probe for cloud), `crates/bunko-server/src/app.rs` + `library/runtime.rs` (build the backend from config, open `RemoteStore`, mount it, install `CloudHooks` impl that calls the existing DAV hook functions, wait for STEADY before the first compile), `ocr/` (scheduler `spooling` state for local claims on remote stores: `local_path` before submitting the `VolumeOp`; `archive_arrived` computes pages in `run_background` when the store is remote; `still_owed` checks `recently_removed`; D4 upgrade rule change incl. `pre-upgrade` layer per spec §8 — the edited skip removal and its test rewrites are their own commit), `thumbs/` (event-driven on remote, startup sweep skipped), `bunko-dav/src/methods/read.rs` stream branch (remote archive GET via `afs::File` stream variant — extend `bunko_store::afs::File` with a `Stream` variant), `server.serve_readers` flag (when false: no `/catalog` or reader WebDAV routes; processor archive GETs still served), admin status `library.storage` block, CLI `mokuro-bunko library status`.

- [ ] Tests: `outage_holds_claims`; `arrivals_compute_in_background`; end-to-end (in `crates/bunko-server/tests/cloud_headless.rs`): a Bunko with `backend: webdav` pointing at a second in-process Bunko (or the Fake via a test-only `backend: fake` hook) holding one un-OCR'd volume, the `bunko-processor` `FakePipeline` as local OCR → `.mokuro`, `.webp`, `series.json`, `catalog.json`, `.mokuro-queue.json` and `.mokuro-bunko.json` appear in the remote folder; restart mid-upload resumes; golden suite passes with `STORE=fake`.
- [ ] Commit per sub-area.

### Task 11: automatic case merge + `case_winner`

**Files:** `src/merge.rs`, PathCase hook in `crates/bunko-dav/src/path_case.rs` (use `store_for(parent).and_then(|s| s.case_winner(parent, fold))` before `min()`), `tests/case_merge.rs`. Exactly spec §5.9 "The merge (owner decision)".

- [ ] Tests: every bullet of the spec §13 case-merge list.
- [ ] Commit `bunko-remote: automatic case merge`.

### Task 12: M2 verification

- [ ] `cargo test --workspace` green; conformance green on all Fake profiles + WebDAV-over-Bunko; golden on `STORE=fake`.
- [ ] Manual: run `mokuro-bunko serve` with `storage.library.backend: webdav` against a second local Bunko holding a real volume and local CPU OCR; observe the sidecar appear in the remote folder; restart; kill -9 during upload; confirm one copy.
- [ ] CHANGELOG `[Unreleased]` entries; commit; PR.

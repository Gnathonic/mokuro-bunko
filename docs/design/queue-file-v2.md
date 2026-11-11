# OCR queue file v2: final spec

## 0. Executive summary

1. **No root queue file is stored anywhere.** Bunko writes no root queue file to Drive, OneDrive, MEGA, WebDAV or the local folder. Bunko's own WebDAV keeps serving today's v1 root file to released readers. That file is virtual and frozen, with its 100-job window.
2. **One file per series, listing every owed job.** Each series folder that owes OCR gets `<Series>/.mokuro-series-queue.json`. There is no window and no truncation.
   - A volume costs about 58 B (today: 554 B per job). The median file is about 0.9 KB.
   - The whole 9,500-volume queue is about 0.66 MB across about 600 files. As one v1 file it would be 5.3 MB.
3. **Library-wide state rides the lease marker.** That state is the hold reason, the held rows and an ETA "work clock". The marker `.mokuro-bunko.json` is already rewritten on every heartbeat (300 s; 1,200 s on MEGA), so this costs no extra writes.
4. **Waiting jobs carry a work position `w`, and ETA = clock(w).** A change in fleet speed, an overrun or a pause moves only the clock.
   - I simulated it with Bunko's real rate smoothing (α = 0.5) on 9,500 volumes.
   - With absolute ETAs under a "max(60 s, 25%)" tolerance, Bunko rewrites 200–790 files/h just because ETAs drift. With the clock that drops to 1–30 files/h.
5. **Bunko writes a series file only on a real change.** A real change is:
   - a job added or finished;
   - a job starting;
   - an ETA that leaves max(60 s, 25 % of the time left).

   Writes are held in a batch per backend window: Drive and OneDrive 120 s, MEGA 300 s, WebDAV 30 s, local folder 15 s. Each window has a cap, priorities and throttle back-off. Writes go through the existing coalesced Status outbox op, and only after the series' sidecar and `series.json` have committed.
6. **Bunko writes on Drive.** Expect about 2 writes per finished volume plus a few for drift:

   | Throughput | Writes per hour |
   |---|---|
   | about 20 volumes/h (either queue size) | 44–48 |
   | 40–60 volumes/h | 70–134 |

   With write-by-id each write is 1 call. Today Bunko makes about 60 writes/h at about 3 calls each, roughly 180 calls/h, and that covers 100 jobs. Upload bytes fall from 2–3.4 MB/h to about 0.05–0.15 MB/h.
7. **Reader cost.** The reader reads the queue only on the series page. There it does one conditional read per window, through a folder-aware `readControlFile` on all five providers, Local Folder included.
   - On Drive that is about 32–36 calls/h and a few KB.
   - Today it is 168–192 calls/h and 2–3.4 MB/h whenever any tab is visible.
8. **The reader no longer freezes.** It updates only the open series' slice of its store, and does nothing at all when a read is unchanged. That removes what caused the freeze, not just the bytes.
9. **How a finished volume reaches the reader.** The trigger is a job disappearing from a complete series file, or a new `done` entry. The reader then:
   - lists that one series;
   - refreshes `series.json`;
   - runs the existing upgrade pass and layer pull.

   The manifest re-check and the whole-account relist go away. The manifest stays only for deep links and old Bunkos.
10. **Rollout and compatibility.** The work is 4 Bunko PRs and 4 reader PRs.
    - Released readers keep working over WebDAV.
    - Reader 1.11 connected straight to a cloud library shows no queue labels. No released Bunko ever wrote the file it reads: v0.7.12 has no `publish_cloud_queue`, and v1.11.0 has `cloudSourceFor`.

---

## 1. Verdict on the three designs

### 1.1 Scores

Scores are 1–5.

| Goal | D1 stable-leaves | D2 reader-pull | D3 writer-budget |
|---|---|---|---|
| Complete queue | 5 | 5 | 5 |
| No reader freeze | 5 | 5 | 5 |
| Compact / bandwidth | 4 (root index + leaves) | 5 | 5 |
| Update-friendly (writes stay low) | **5** (work clock) | 2: absolute ETAs. Its estimate of 2–10 drift writes/h is off by about 50× under α = 0.5 | 3: absolute ETAs plus an adaptive tolerance; drift still dominates |
| Quota-friendly per backend | 4 | 3 | **5** (budget, priorities, back-off, fast path) |
| Generic provider file API | 3: adds the name to the listing allowlist | **5**: folder-aware control read, file kept out of listings | 3: adds the name to the listing allowlist |
| Simple and robust | 2 (root index, knots, seq-merge rules) | 4 | 3 |
| Compatibility | 5 | 4 | 4 |

**Base:** D3's write machinery (budget, priorities, aging, fast path, persisted state, provider clock).

**Grafts:**
- From D2: where the file lives, keeping it out of every listing, and the folder-aware control read.
- From D1: the work clock, but carried in the lease marker instead of a root index. The owner's default of "no root file" therefore holds.

### 1.2 Facts the decision rests on

I checked each against the code.

- **The reader keeps control files out of listings on purpose.** `util/sync/syncable-file.ts:95-104` says putting control files in the listing "would feed them to the listing caches, the sync diff and the managed-file matcher — some of which would try to download, move or delete them … pinned by tests". This rules out the allowlisting in D1 and D3.
  - Drive's `listFolder` filters by `isSyncableFile` (`google-drive-provider.ts:880`). Every `readControlFile` (`:463`, `onedrive:246`, `mega:1410`, `webdav:1778`) is root-only. Local Folder has none.
- **D3's ordering rule as written would deadlock.** `outbox.rs` `deps_done` makes `series.json` wait for every live non-Compiler op under its series, Status ops included (`o.origin != Origin::Compiler && under(..)`). If a series queue op also waits for an earlier `series.json`, the two wait on each other. The rule has to change (§6.6).
- **The marker is rewritten on every heartbeat.** See `status.rs:1048-1050` (`heartbeat_at` set, then `write_marker`) and `heartbeat_interval = ttl/3` (300 s; 1,200 s on MEGA). Fields added to the marker therefore cost bytes, not writes.
- **The rate model is noisy.** `bunko-sched/src/rate.rs:25` sets `SESSION_ALPHA = 0.5`. Each finished volume moves every later ETA by about 5–15 %, and that is the source of drift churn (§11.2).
- **Membership comes from one pass.** `Scheduler::owed_board()` (`sched/status.rs:633`) gives every owed (volume, row) in one pass, including jobs in backoff.
- **Finished OCR already arrives through the listing, but a listing must happen first.**
  - `series-open.ts` refreshes `series.json` from the cached listing and runs the upgrade pass (`startOcrUpgrade`).
  - `pullLayersForVolume` reads `listingFromCache`.
  - `series-index-sync.ts:213-227` deletes cached indexes for every folder missing from the map it is given. So a scoped pull must never hand it a partial map.
- **A new basename does not wake released readers.** MEGA's `onNode` fires only for `isBunkoControlFile(name)`, which is an exact match on the two root names (`mega-provider.ts:321-326`).
- **Marker parsing ignores unknown fields.** `parseBunkoMarker` checks only `version === 1` (`cloud-bunko-marker.ts:82-108`), so adding fields is safe for released readers.

---

## 2. What the root file is for

In v2, nothing.

| Need | Where v2 meets it |
|---|---|
| Labels and ETAs | The series file, read only while that series page is open. Only `VolumeItem` on `SeriesView` renders queue data. |
| Global hold, held rows, ETA clock, trust anchor | The marker's `series_queue` block. The reader already reads the marker first, and the marker names the lease holder. |
| Completion of watched volumes (this device's uploads, deep-link imports) | The content already arrives through the normal listing: `series.json` `mokuro_sha256` feeds the upgrade pass, and layer files feed layer sync. What is missing is **when** to list: only Drive refreshes its listing on a timer (change feed at each sync). The trigger is a read of that one series file (§8.6), so a global `done[]` is not needed. |
| Released readers over Bunko's WebDAV | The v1 root file, virtual and frozen, served from memory and never stored. |

---

## 3. Files, names, locations, case rules

| File | Where | Writer | Readers |
|---|---|---|---|
| `.mokuro-series-queue.json` (series file) | `<library root>/<Series folder>/`. The library root is `mokuro-reader/` on Drive, OneDrive, MEGA and WebDAV stores, and the store root on the local-folder backend (the folder the reader picks). | The lease holder, as an outbox Status op (§6) | v2 readers, through `readControlFile(name, {folder})` |
| Same name, virtual, at `/mokuro-reader/<Series>/.mokuro-series-queue.json` | Bunko HTTP (a local library and the WebDAV bridge of a cloud library). Rendered from memory and never listed. | — | v2 readers connected to a Bunko over WebDAV |
| `.mokuro-bunko.json` (marker) | Root | The lease heartbeat | Gains a `series_queue` block. `queue` becomes `null`. |
| `/mokuro-reader/.mokuro-queue.json` v1 | Bunko HTTP only, virtual and frozen | — | Released readers |
| Stored root `.mokuro-queue.json` | **Not written any more.** A leftover from a 0.8 pre-release is deleted once, on lease claim. | — | — |

### Why this name

- It does not fold to `.mokuro-queue.json`, so MEGA's exact-name hook in released readers stays silent, and so does `dedupKeepsEveryCopy`.
- It is not syncable (it is not `.cbz`, `.mokuro*`, an image or `series.json`). Released readers never list, move or download it.
- It matches none of Bunko's reserved names (`.bunko-probe-*`, `*.bunko-*.tmp`).
- It does not end in `series.json`.

### Case (NTFS semantics everywhere)

- **Bunko:**
  - writes exactly `.mokuro-series-queue.json`;
  - treats every spelling that folds to it as the same name (`is_fixed_name`);
  - deletes other spellings it finds in the folder (priority P3);
  - writes into the folder its index holds for the series, in the store's spelling and canonical root.
- **The reader:**
  - addresses the folder as the listing spells it (`resolveCloudFolderTitle`);
  - providers match folders fold-equal (NFC, then lowercase) across **every** library root and return every copy;
  - the reader keeps the trusted copy with the highest `rev`.
- **Keys:**
  - the series key is `normalizeSeriesKey(folder)`;
  - the volume key is `normalizeVolumeTitleKey(stem)`;
  - `watchedKeyStore` overrides still apply;
  - Bunko never owes two stems of one fold group, so no two keys in one file fold together. If they did, the reader keeps the first.

---

## 4. Formats and versioning

### 4.1 Stored series file

Compact UTF-8 JSON, not gzipped. Names are made up.

```json
{"v":2,
 "instance_id":"8d2c51f0e3a94b7e",
 "rev":48211,
 "at":1794398520,
 "clock_at":1794398400,
 "series":"Example Saga",
 "rows":[
  {"kind":"ocr","id":"hayai-nova","parts":[{"role":"recognizer","name":"Hayai Nova","version":"2.5"},{"role":"detector","name":"PP-OCR Manga","version":"0.3"}]},
  {"kind":"layer","id":"paddle","parts":[{"role":"recognizer","name":"PaddleOCR-VL","version":"0.3"}]}],
 "vols":{
  "Example Saga 03":[[0,"r",1794398340,1794398760]],
  "Example Saga 04":[[0,"q",1794400200],[1,"q",1795310000]],
  "Example Saga 05":[[0,"c"]],
  "Example Saga 06":[[0,"q",null]]},
 "done":{"Example Saga 01":1794395000,"Example Saga 02":1794398280}}
```

**Top-level fields**

| Field | Meaning |
|---|---|
| `v` | Exactly `2`. The reader shows nothing for that series otherwise. Unknown fields are ignored at every level. |
| `instance_id` | Bunko's `core.instance_id`, the same id as in the marker. It is the trust key. |
| `rev` | A per-instance counter persisted in `state.sqlite`. It is increased every time any series file is staged, and gaps are fine. The highest wins among duplicate copies. A lower `rev` than the reader already holds for that instance is a stale replica and is ignored. |
| `at` | Write time, in epoch seconds on the **store's clock** (§6.9). It is for information and the skew check only, and never decides a rewrite. |
| `clock_at` | The fit time of the clock under which this file's `w` values were computed (§4.4). |
| `series` | The folder spelling, for information only. The reader keys by the folder it read the file from. |
| `rows` | Only the rows this file refers to, in Bunko row order (primary first). `id` is the same value v1 put in a job's `id`. `parts` keeps the v1 shape, so `validParts` and `ocrFileLabel` are reused. |
| `vols` | **Every** owed (volume, row) of the series. The key is the archive stem as spelled. Keys are serialized in byte order, which is deterministic. |
| `done` | Stem → epoch seconds when that volume's last job settled (§5.3). It covers the last 24 h, with at most 64 entries, keeping the newest. |

**Job tuples**

| Tuple | State | Notes |
|---|---|---|
| `[r, "q", w]` | Queued | `w` is the integer work position, rounded to 60. It is `null` when unpriced, and also used for jobs in failure backoff. |
| `[r, "c"]` | Claimed, not started | No time. |
| `[r, "r", t0, e]` | Running | `t0` is the start and `e` the predicted finish (rounded to 60), both absolute store-clock seconds. `e` may be `null`. |

### 4.2 Marker addition (additive; released readers ignore it)

```json
"queue": null,
"series_queue": {"v":2,"file":".mokuro-series-queue.json","batch_s":120,
  "held":null,
  "held_rows":["paddle"],
  "clock":[[1794398400,1794398400],[1795400000,1795521000]],
  "clock_at":1794398400}
```

| Field | Meaning |
|---|---|
| `held` | `null` or `{"reason": …}` with today's codes. `storage-*` reasons can never reach a failing store; a stale marker is that signal. |
| `held_rows` | Row ids that no open lane can take now. Their jobs show as held, and their series files are **not** rewritten. |
| `batch_s` | This store's window W (§6.4). |
| `clock`, `clock_at` | The current work clock (§4.4). |

### 4.3 Virtual file over Bunko HTTP

It has the same body plus `"live":{"held":…,"held_rows":[…],"clock":[…],"clock_at":…,"batch_s":5}`, because a local library has no marker.

The reader rule is `live = file.live ?? marker.series_queue`. A stored copy never contains `live`.

### 4.4 The work clock

- `clock` is 2 to 16 knots `[w, t]`, strictly increasing in both coordinates.
- `ETA(w)` is linear interpolation between knots. Beyond either end it extrapolates with the slope of the end segment.
- Bunko v2.0 always writes **2 knots**, from the affine fit in §6.2. Readers must handle up to 16 so a later piecewise fit needs no format change.
- The first clock is the identity: `[[t0,t0],[t0+86400,t0+86400]]`.

### 4.5 Versioning

- The series file and the marker block carry `v: 2`.
- Additive changes keep 2.
- A breaking change gets a **new file name**, so older readers see no file rather than misread one.
- The v1 root virtual file keeps `version: 1` and its frozen shape.

---

## 5. Bunko: what goes into a series file

### 5.1 Membership

`owed_board()`, grouped by the series fold key. Every owed (volume, row) is listed. Nothing is cut and nothing is ranked globally.

### 5.2 State, from the current plan

| Situation | Tuple |
|---|---|
| Running card | `r` with `t0` = card start, `e` = plan `eta_at` |
| Claim without a card | `c` |
| Priced plan entry | `q` with `w = clock⁻¹(e)` under the clock the sink currently uses |
| Unpriced, or owed but outside the plan (backoff) | `q` with `null` |
| Row in `held_rows` | `q` with the previously published `w` kept unchanged |

Row-held jobs keep their old `w` because their ETA is hidden; the file does not change.

### 5.3 Settle rule

This replaces "a done entry never precedes its sidecar".

A job that is no longer owed is removed from series S only when S is **settled**:
- no live Deferred `Ocr` or `Compiler` op under S is left in the outbox; and
- no `schedule_series_regeneration` timer is pending for S (`library/runtime.rs:517`). This needs a new accessor `series_regeneration_pending(title)`.

Until then the job renders as `r`, with its published `t0` and `e = max(published e, now)`. The reader shows "finishing".

When the volume's last job leaves, `done[stem] = now`.

So by the time a job disappears, the store already has the sidecar **and** the `series.json` carrying its `mokuro_sha256`. A reader's upgrade pass then finds it on the first scoped listing.

### 5.4 Over HTTP on a local library

There is no outbox, so settled means only that no regeneration timer is pending for S.

---

## 6. Bunko write policy

### 6.1 Sinks

There is one renderer and two sinks. Each sink keeps its own published state: per series, the content hash, each job's published `w`/`e`/state, `done`, `rev`, and the clock it uses.

| Sink | Window W | Budget | Persisted |
|---|---|---|---|
| **Store** (any connected store, the local folder included) | Per backend (§6.4) | Yes | Yes (§6.7) |
| **HTTP** (virtual) | 5 s | None | No, in memory |

**Plan pricing replaces the 2 s loop.** `publish_cloud_queue` and `CLOUD_QUEUE_EVERY` are removed. Bunko prices the plan (`plan_queue`) only:
- (a) at a store flush when a series is dirty;
- (b) at a clock refit;
- (c) on an HTTP GET whose cached render is older than 5 s.

A priced plan is cached for 5 s per `queue_generation`.

Detecting membership and running-state changes needs no pricing; it is a diff of the owed board and the running set.

### 6.2 Clock refit

**When:**
- With a store: just before each lease heartbeat.
- Without a store: every 300 s.
- Also as part of any early heartbeat (§6.8).

It is skipped while the queue is globally held, or when fewer than 2 priced queued jobs have a published `w`.

**Fit:**
1. Take every queued, priced job of a non-held row that has a published `w_j`, and its current plan ETA `e_j`.
2. Run a weighted linear least squares of `e` on `w`, with weight `1/tol(e_j − now)²` (tol from §6.3). That gives slope `k = Σwt(w−w̄)(e−ē) / Σwt(w−w̄)²`, clamped to [0.1, 10], through the weighted means.
3. The knots are `(w_min, f(w_min))` and `(w_max, f(w_max))` over all published `w`.

**Handoff:**
- The store sink switches to the new clock only after the heartbeat carrying it returns `Held`. If the heartbeat fails, it keeps the old clock.
- The HTTP sink switches at once.
- Series written afterwards use the new clock.

**What it absorbs:** a fleet-wide speed change (slope), an overrun or a pause (intercept), and a lane-count change (slope). Each costs **0 series writes**.

**What it does not absorb:** a real reorder, such as a new generation row, a switch to by-volume mode, or a large arrival ahead. Those come out as P2 misfits and are rewritten under the budget.

### 6.3 Dirty test: what counts as a change

Tolerance: `tol(x) = max(60 s, 0.25 · max(0, x))`, where x is the published time left.

| Class | Triggers |
|---|---|
| **P0 hard** | A job added. A job removed (after settle). A new `done` entry. A volume renamed. The rows table changed (a new engine version in `parts`). First publish. Takeover or foreign `instance_id`. A feed event showing the file changed or deleted by anyone but this sink. |
| **P1 state** | `q`/`c` → `r`. `r`/`c` → `q` (claim released). Unpriced ↔ priced for a row that is not held. |
| **P2 drift** (judged only right after a confirmed clock switch) | A queued job with `|clock(w) − e| > tol(clock(w) − now)`. A running job with `e_live − e_pub > max(300 s, 0.5·(e_pub − t0))`. |
| **P3 tidy** | Delete a file with empty `vols` whose `done` tail has expired. Delete stray case variants. Delete the leftover root `.mokuro-queue.json`. |

**Never a trigger:**
- time passing;
- progress;
- `at`, `rev`, `clock_at`;
- the ETAs of held rows;
- the global hold.

This fixes finding 1 (the cloud copy was rewritten on every floor tick because `next_check_after` moved every second). v2 has no `next_check_after`. Both sinks use the same tolerance, which fixes finding 2.

**Any write refreshes everything in the file.** All of the file's ETAs, `w` and `done` are refreshed, so drift rides along on hard and state writes.

### 6.4 Batches, budget and order (store sink)

| Backend | Window W (= `batch_s`) | Cap B per window | Calls per write (with §6.6) |
|---|---|---|---|
| Local folder | 15 s | 100 | 0 network: write a temp file, then rename (atomic) |
| WebDAV | 30 s | 20 | 1 PUT (`If-Match` when the etag is known) |
| Google Drive | 120 s | 30 | 1 media PATCH by id; a create is 1 multipart create |
| OneDrive | 120 s | 15 | 1 PUT by id |
| MEGA | 300 s | 20 | Upload a new node, then delete the old one. Uploads do not use transfer quota. |

- `queue_floor` (`store.rs:148`) overrides W.
- At each window the sink stages up to B dirty series in one burst. On MEGA a burst gives a reader one debounced tree rebuild, not twenty.

**Order:**
1. P0, FIFO by age.
2. P1.
3. P2, soonest ETA first.
4. P3.

**Aging:**
- P2 waiting more than 6 W becomes P1.
- P1 waiting more than 6 W becomes P0.
- P3 waiting more than 1 h becomes P2.

**Back-off:** a throttle signal halves B for 3 windows, then B recovers by 1 per window. The signals are Drive 403 `rateLimitExceeded` or 429, OneDrive 429 or `Retry-After`, and MEGA EAGAIN.

**Worst-case staleness:** `ceil(backlog / B) · W`. A full first publish or takeover of 600 series takes about 40 min on Drive (nearest first), about 80 min on OneDrive and about 2.5 h on MEGA.

**Stage-time state:** between staging and commit, the dirty test compares against the staged state, so the same change is never staged twice. A failed op restores the dirty flag.

### 6.5 Outbox integration

- Each series file is one Put op: `Origin::Status`, `Durability::Deferred`, `Mode::Replace{base:None}`, destination `<S>/.mokuro-series-queue.json`.
- The existing coalescing by destination supersedes older staged copies (`stage_deferred`).
- Writes happen only while the fence is open, as today.

### 6.6 Changes in `bunko-remote`

**Dependencies (`outbox.rs` `deps_done`):**

| Op | Rule |
|---|---|
| Series queue file | Waits for **earlier** live Deferred `Ocr` and `Compiler` ops under S. Other series are not blocked. |
| `series.json` (Compiler) | Waits for live non-Compiler, **non-Status** ops under S. This change is required; without it the two deadlock. |
| Root `.mokuro-queue.json` (Status) | Rule removed. |

**Governor class:** series-file Status ops run in `ApiClass::Background`, so a 600-file pass can never delay a heartbeat. The marker is written directly by the lease and stays in `Lease`.

**Who may write** (`check_target`): a path of 2 or more segments whose last segment folds to the name is writable only by `Origin::Status`. The root rules are unchanged.

**Names** (`names.rs`): add `SERIES_QUEUE_FILE = ".mokuro-series-queue.json"`. The following all skip it:
- the index, compiler, OCR walk and cover loop;
- the orphan sweep. A folder that holds only it (with or without `series.json`) counts as empty.

A duplicate-folder merge trashes the loser's copy, because the file can be regenerated; this is not keep-both. The feed treats this sink's own commits as its own writes.

**Status fast path** (`drive_put`, separate PR B3):
- When the index has a `winner_pid` for the destination and that node's parent equals the destination parent, skip `find_named` and the `same_content` probe, and upload `Replace{pid, if_cver when cas_overwrite}`.
- On `NotFound` or `PreconditionFailed`, fall back to today's path.
- On WebDAV, skip the pre-stat and post-stat when the index holds an etag, and use the PUT response `ETag` (else one stat).
- On Drive every PATCH is a revision; Drive prunes them (not keepForever).

### 6.7 Persistence (`state.sqlite` of the store)

- **Table `series_queue`:**

  | Column | Content |
  |---|---|
  | `series_fold` | Primary key |
  | `folder` | Folder spelling |
  | `rev` | — |
  | `content_sha` | Hash of the body without `at`, `rev`, `clock_at` |
  | `jobs` | JSON: key → published tuple |
  | `done` | JSON |
  | `last_write_ms` | — |

- **Meta rows:** `series_queue_clock` (knots, `clock_at`) and `series_queue_rev`.
- **The published state is persisted at commit.** A restart rewrites nothing that is unchanged, and loses no `done` entries. The first refit after a restart absorbs the downtime shift in the clock.

### 6.8 Marker and lease

- **Marker content:** `Marker` gains `series_queue: Option<SeriesQueueBlock>` (serde default), filled at each heartbeat from the scheduler (held, `held_rows`, clock), and `queue` is set to `None`.
- **Early heartbeat:** a change of `held` or `held_rows` (or an unhold) requests one, with a refit.
  - It is debounced 30 s (60 s after an unhold, so the plan settles).
  - At most one early heartbeat per 120 s.
- **On becoming holder** (claim or takeover):
  - delete the root `.mokuro-queue.json` (P3, once);
  - mark every series with owed jobs P0 (foreign `instance_id`);
  - delete series files found in series that owe nothing (P3).
- **A holder that loses the lease:** its fence closes within ttl/2. Any file it landed carries the wrong `instance_id` and is overwritten by the new holder.

### 6.9 Store clock

The lease records `offset = server_now − local_now` at each `marker_set()` (`status.rs:807`), as the median of the last 5. Every time written to a store is `local + offset` (`at`, `t0`, `e`, `done`, the clock's `t`). The HTTP sink uses Bunko's own clock. This keeps a NAS without NTP from skewing ETAs.

### 6.10 Configuration

- `status_files` (default `true`; today it is parsed and never read) switches the store sink on or off. Off means no series files are written to the store; HTTP still serves them.
- `queue_floor` overrides W.
- No new keys.

### 6.11 Removed

- `publish_cloud_queue` and `CLOUD_QUEUE_EVERY`;
- `cloud_queue_doc`, `QUEUE_DOC_MAX_BYTES`, `QUEUE_WAITING_LIMIT`, `truncated`;
- `record_done` and the `queue_done` meta row;
- the root Status dependency rule.

`queue_window` stays only for the v1 HTTP file.

### 6.12 Doc fixes

- In `0.8-remote-storage.md`, the hold reason `transfer-quota` becomes `storage-transfer-quota`, matching `claim.rs:779`.
- §9.1 and D10 are rewritten to this spec.

---

## 7. How Bunko's WebDAV serves it

**Route.** `queue_file::middleware` matches `["mokuro-reader", <S>, <name>]` where `name` is `is_fixed_name(.., SERIES_QUEUE_FILE)`. `<S>` resolves case-insensitively to the library's series folder (PathCase).

**Responses:**

| Request | Response |
|---|---|
| GET or HEAD | The same read gate as a GET of a file in `/mokuro-reader/<S>/`. |
| GET, S is not a series folder this account can see | 404 |
| GET, existing folder with nothing owed | 200 with `vols:{}` and its `done` tail. A 404 for an existing series therefore means "older Bunko". |
| `If-None-Match` matching any tag in the list | 304 |
| PUT, DELETE, PROPPATCH, LOCK or MKCOL on it, and MOVE/COPY with it as source or Destination | 405 |
| PROPFIND | Lists neither the virtual file nor a stored copy (on a bridged cloud store). |
| OPTIONS | Passes through (CORS). |

**Body and headers:**
- The body is the HTTP sink's render plus `live`, cached per series and rebuilt at most every 5 s.
- `ETag` is `"` + the first 32 hex characters of sha256 over the compact body without `at` + `"`.
- gzip is used when `Accept-Encoding` has gzip and the body is at least 1 KiB, with the tag `"<tag>-gz"`.
- `Cache-Control: no-cache`, `Vary: Accept-Encoding`.
- The ETag is stable exactly when the dirty test says nothing changed. The clock moves it at most every 300 s.

**v1 root file:** unchanged in shape and window, as are the manifest endpoint and the 0.5.2-derived rules. The one fix is `same_queue`, which now compares the whole document except `generated_at` (finding 7). This means `next_check_after` and `pending_volumes` are no longer served stale.

**Bunko's admin queue page** reads the scheduler directly and is untouched.

---

## 8. Reader algorithm

### 8.1 Provider API (R1)

```ts
readControlFile(name: string, opts?: { ifNoneMatch?: string; folder?: string }): Promise<ControlFileResult>;
onControlFileChange?(listener: (name: string, folder?: string) => void): () => void;
```

- `folder` is the series folder as the listing spells it.
- The result holds every node named `name` in every folder fold-equal to `folder`, under every library root.
- Caches are keyed by `folder/name`.
- `SERIES_QUEUE_FILE_NAME` stays **out of** `isSyncableFile`, and `isBunkoControlFile` stays root-only.

| Provider | Unchanged check | Changed | Push |
|---|---|---|---|
| Drive | 1 `files.list` with `name='…' and ('<id>' in parents or …) and trashed=false`, fields `id, md5Checksum, modifiedTime, size`. Parent ids are the `parentId` of S's entries in the cached listing; no extra lookup. | +1 `alt=media`, without the size probe | — |
| OneDrive | 1 `GET root:/mokuro-reader/<S>/<name>` (per-segment `encodePath`), using cTag | +1 `/content` (2 HTTP requests with the redirect) | — |
| MEGA | 0 requests: a lookup in the live tree, where the parent folds to S and the grandparent is the library root. The version is handle plus timestamp. | 1 download of about 1 KB of transfer quota | `onNode` also fires for this name, passing the parent folder name; the reader filters to the open and watched series |
| WebDAV (plain, or a Bunko) | 1 GET with `If-None-Match` to `${base}/mokuro-reader/${enc(S)}/${enc(name)}`, encoding **each segment** (today the whole name goes through `encodeURIComponent`). Gives 304. | 1 GET with 200 | — |
| Local Folder | 0 network: `getDirectoryHandle(S)` (exact, else a fold-match among the entries), then `getFileHandle(name).getFile()`. The version is `${lastModified}:${size}`. | Same read | — |

**Local Folder** also gains a root `readControlFile`, so the marker can be read. Remove the `filesystem` early return (`server-ocr-queue.ts:1375`) and set `CLOUD_MIN_INTERVAL_S.filesystem = 15`.

### 8.2 Store and display (R2)

- **Store:** `seriesQueueStore` is a `Map<seriesKey, Map<volKey, PendingVolume>>`.
  - A read replaces only its own series' entry, immutably.
  - There is **no `set()`** on `'not-modified'`, on an equal version, or on equal bytes.
  - No map is ever built over the whole queue.
  - `VolumeItem` takes its own slice.
- **Display rules** (`describePendingOcr`, adapted):

  | Situation | Shown |
  |---|---|
  | `live.held` set | Every job "held (reason)", no ETAs |
  | Row in `held_rows` | "held" |
  | `q` with `w` | ETA = `clock(w)`; "soon" when 60 s or less, never negative |
  | `q` with `null` | "queued" |
  | `c` | "starting" |
  | `r` | "running", progress = `clamp((now − t0)/(e − t0), 0, 0.99)`; "finishing" past `e` |

  The existing 30 s `pendingOcrClock` redraws without I/O. ETAs are shown in coarse relative buckets.
- **Trust:**
  - A file with `live` is Bunko's own virtual file and is trusted as is.
  - Otherwise there must be a fresh marker whose holder has `series_queue`, and `file.instance_id` must equal `holder.instance_id`.
  - Among trusted copies the highest `rev` wins. A lower `rev` than the memo for that instance is ignored.
  - Torn JSON keeps the last good copy.
- **Clock source:** `file.live`, else the marker's `series_queue`. If `file.clock_at > live.clock_at`, re-read the marker, at most once a minute.

### 8.3 Per view

| View | Queue reads |
|---|---|
| App start | None. The marker read stays as today. Watches are scheduled (§8.7). |
| Library grid, cards, cloud screen, reader | None. |
| Marker poll (any view, tab visible) | `max(provider floor, ttl/3)`, so 300 s; MEGA is event-driven. It keeps `serverCompilesMetadata`, holder freshness and the clock current. The 60 s `keepAlive` queue loop is removed. |
| Series page S | The loop in §8.4 |
| Volume rows | Their slice. No reads of their own. |

### 8.4 Series page loop (hooked to the `SeriesView` mount effect, beside `openSeries`)

1. **Pick the source.**
   - If the connected WebDAV is a Bunko (`connectedBunkoQueueUrl()`), read the virtual file. A 404 for a series that exists means an older Bunko: use the kept v1 HTTP poller for that server for this session.
   - On a cloud or local-folder library, a fresh holder with `series_queue` is required, else show nothing.
2. Read with the memo's version token.
3. Parse, check trust, and publish S's slice only if it changed.
4. Detect completions (§8.6).
5. Repeat every `max(provider floor, batch_s)` (WebDAV 30 s, Drive and OneDrive 120 s, local folder 15 s, MEGA on event plus a free 10-minute safety read) while:
   - the page is mounted;
   - the tab is visible; and
   - the slice has jobs, or S holds a watched volume, or fewer than 2 reads have been taken.
6. On unmount, stop. The memo (token, parsed slice, read time) is kept, so going back to S within the interval costs no network.

### 8.5 Read cost (Drive, per hour, tab visible)

| | Today | v2 |
|---|---|---|
| No series page open | Marker and queue every ≥60 s: 60 + 60 lists + about 12 marker downloads + 36–60 queue downloads, which is **168–192 calls and 2–3.4 MB**. A full-size v1 file would be about 190–320 MB. | Marker only: **24 calls**, about 25 KB |
| One series page open | Same as above | **+32–36 calls** (30 lists + changed downloads), about 10–40 KB |
| Completion | Full relist per batch: 13+ `files.list` at 12.5k files | 1 `changes.list` per batch (§8.6) |
| MEGA transfer | The whole file per change | About 1 KB per change of the open series |

### 8.6 Completions (R3)

This replaces "absent from the truncated file, so ask the manifest" and "`done[]`, so run a full relist".

**Trigger,** per series:
- a job seen in the previous read is gone; or
- a `done` entry is newer than the memo's previous read. On a first read, that means newer than S's cached listing time.

**Action:** `unifiedCloudManager.refreshSeriesFolder(S)`, debounced 10 s, at least 60 s apart per series, one in flight.

1. **List S:**

   | Provider | How |
   |---|---|
   | Drive | `refreshFromChangeFeed()` (1 call) when the feed is usable, else `listFolder(S)` |
   | OneDrive, WebDAV, Local Folder | `listFolder(S)` |
   | MEGA | `listFolder(S)` (in-memory, 0 requests) |

2. Replace only the cached entries under `S/` with the result, through a new `replaceFolderEntries`. **Never** call `refreshSeriesIndexes` with a partial map, because it deletes other series' indexes.
3. Run `refreshSeriesIndexForSeries(S)`, which re-downloads `series.json` if its stamp moved.
4. Run `requestOcrUpgradePass(type, [{title: S, files}])` for the primary sidecar, through `mokuro_sha256`.
5. Run `pullLayersForVolume(uuid, type)` for each local row of S whose volume finished.
6. If neither the `series.json` stamp nor a layer file moved, retry at +60 s, +120 s and +240 s, then leave it to the next normal listing. The settle rule (§5.3) makes this rare.

**Removed:**
- `CloudCompletions` and its whole-account `relist()`;
- the root `cloudSourceFor`, its `keepAlive` loop, and the cloud `done-list` path;
- `pullCompletedVolume` for a connected Bunko.

**Kept:**
- The manifest pull, for deep links (an origin other than the connected library) and for the v1 fallback.
- `/catalog/api/manifest` on Bunko.

### 8.7 Watches (uploads, deep links)

- Watches are created as today and keep their 24 h lifetime.
- `saveWatches` is debounced by 1 s, which fixes the O(n²) rewrite.
- For a watched volume whose series page is not open, read its series file:
  - at creation + 2 W;
  - then at the soonest watched ETA + W;
  - and at most every 15 min per series, with at most 20 series in rotation.
- **Done or vanished:** run the scoped pull (§8.6) and drop the watch.
- **Absent and never seen after 2 reads:** run one scoped pull. A `.mokuro` newer than the watch completes it; otherwise keep the 15-minute reads until the watch expires.
- **Deep links:** the series file is read through the link's origin with today's confined auth. On completion the pull goes through the per-volume manifest, which is kept for this.

---

## 9. Compatibility and migration

| Reader \ Bunko | ≤ 0.7.x HTTP | 0.8 v2 over HTTP (local, or bridge) | 0.8 v2 on a cloud or local-folder store |
|---|---|---|---|
| ≤ 1.10 | As today | As today: the v1 virtual root and the manifest | No queue, as today |
| 1.11 (released) | As today | As today | No root file, so `trusted = null` and no labels. OCR still arrives through its listing and upgrade pass. Its `keepAlive` marker and queue reads continue until it upgrades (`readControlFile` returns null cheaply). |
| v2 | 404 on the series file, so the v1 HTTP poller is used (kept) | Series files (virtual) | Series files (stored) |

- A marker without `series_queue` (a 0.8 pre-release on a trial library) shows no labels; the v1 cloud-root reader code is deleted.
- Fixture order: Bunko blesses the v2 fixtures first; then the reader bumps `interop/reader.ref` and updates its fixture copies.

---

## 10. Failure modes

| Case | Handling |
|---|---|
| Torn update across many files | Files do not depend on each other. The clock in the marker versus the `w` in a file differs by at most one refit, which is within tolerance, and `clock_at` makes the reader fetch a newer marker. Ordering inside a series is guaranteed by the dependencies and the settle rule. |
| Torn single file (a non-atomic WebDAV PUT, or MEGA's two-node window) | A parse failure keeps the last good copy. With two nodes, the higher `rev` from the holder wins. The local folder writes by rename. |
| Two Bunkos | Only the fenced holder writes. Readers trust only the holder's `instance_id`. A former holder stops within ttl/2. The new holder rewrites at P0, soonest first, and deletes series files that are empty. A bridge that is not the holder serves `live.held = storage-lease`. |
| Clock skew | Bunko writes times on the store's clock (§6.9). Reader devices are assumed to run NTP. A past ETA shows "soon". |
| Series renamed or case-renamed | The file moves with the folder, and the reader keys by folder. Bunko sees the change in the owed board and rewrites at P0 under the new path. Folders that fold together: the reader reads all of them and takes the trusted copy with the highest `rev`. |
| Series with 1,000 volumes | 58 KB with one row, 78 KB with two. Parsing takes about 1–2 ms on a desktop and about 10 ms on a phone, only on that page and only on a change. In by-volume mode or a backlog in one series, it is rewritten about twice per finished volume: about 3–7 MB/h uploaded at 20–60 volumes/h. Splitting into chunks is deferred until a file exceeds 256 KB. |
| Budget exhausted or throttled | Priorities with aging, and B halved. The bound is `ceil(backlog/B)·W`. |
| Global pause, or a long pause then unhold | Shown from the marker through an early heartbeat. Series files untouched. The refit after unhold absorbs the shift. |
| New generation row, or a mass reorder | A real membership or order change: every affected series is rewritten once, paced and nearest first. Far ETAs can be stale for up to about 40 min on Drive meanwhile. |
| Restart | The published state and clock are persisted, so nothing is rewritten; the refit absorbs the downtime. |
| File deleted by a user or a sync tool | The feed event marks it P0. The fast path's 404 falls back to a create. Meanwhile the reader shows no labels. |
| Store offline, auth failure, quota | No writes, and the marker goes stale. The cloud reader shows nothing, as today. Over HTTP, `live.held = storage-*`. |
| Arrival Bunko has not judged yet | Absent and not in `done`, so nothing is shown. The watch schedule covers it (§8.7). |

---

## 11. Numbers

### 11.1 Sizes

Measured on synthetic files with this exact format: 34-character stems and the two-row table above.

| | Today (v1) | v2 |
|---|---|---|
| Per job | 554 B (measured) | About 58 B for a volume's first job, +23 B for each further job |
| 1 volume / 16 / 100 / 1,000 (1 job each) | — | 454 B / 1.3 KB / 6.2 KB / 58 KB (gzip 7 KB) |
| 454-volume queue | 56 KB per write, showing 100 jobs; 250 KB if complete | About 30 files, about 40 KB in total, about 1 KB each |
| 9,500-volume queue | 56 KB per write (about 1 % of the queue); 5.3 MB if complete | About 600 files, 0.66 MB in total; median 0.9 KB, largest (375 volumes) 22 KB |
| Marker | — | +about 150 B |

### 11.2 Writes and Drive calls per hour

The simulation is in `/tmp/claude-1000/-home-nathan-Projects-mokuro-webdav-library/8c8a8ad5-670c-4ad8-bdd6-80eb87f65616/scratchpad/qsim/sim4.py`, with the final runs in `/tmp/claude-1000/-home-nathan-Projects-mokuro-webdav-library/8c8a8ad5-670c-4ad8-bdd6-80eb87f65616/scratchpad/qsim/run6.py`. The model:

- 2 lanes;
- jobs round-robin across series, position-major, as `order_jobs` does;
- lognormal series sizes;
- per-volume estimate error 15 %, run-time noise 15–30 %;
- fleet speed random walk of 5 %/√h;
- the rate EWMA uses α = 0.5 (`SESSION_ALPHA`);
- the §6.3 rules, with P2 judged after each heartbeat;
- averaged over 4 runs of 12 h, with the first 2 h discarded.

| Queue | Throughput J | Absolute ETAs (D2/D3 style) | **v2 with clock** | Of which drift | Today (measured) |
|---|---|---|---|---|---|
| 454 volumes / 30 series, Drive W=120 | 19/h | 92 writes/h | **48/h** | 11 | 36–60 writes/h, always |
| | 38/h | 83/h | **70/h** | 10 | |
| 9,500 volumes / 600 series, Drive W=120 | 20/h | 544/h (215–790) | **44/h** | 5 | |
| | 60/h | 662/h | **134/h** | 23 | |
| 9,500, MEGA W=300 | 18–55/h | 196–237/h, cap-bound | **39–108/h** | 4–30 | |

- **Drive calls for Bunko's writes:** v2 makes **44–134 calls/h** for the complete queue with the fast path, or 132–400 without it. Today it makes about 108–180 calls/h (about 3 per write) for 100 jobs.
- **Upload bytes:** about 0.05–0.15 MB/h, against 2–3.4 MB/h today.
- **Write count:** roughly 2 writes per finished volume (its finish, and the next job's start) plus drift.
- **The clock** is what keeps a 9,500-volume queue from costing hundreds of writes per hour.

---

## 12. Tests

### 12.1 Bunko unit and integration tests

Run each test with `XDG_CONFIG_HOME` and `XDG_DATA_HOME` pointed at a temporary directory.

- **`bunko-sched` and `ocr/series_queue.rs`:**
  - rendering from the owed board: every state, unpriced, backoff as `null`, row-held keeping its `w`, only referenced rows, deterministic key order, the `done` window (24 h, 64 entries);
  - the settle rule: a job stays `r` while an `Ocr` op is live or a regeneration is pending;
  - the `tol` table at a time left of 0, 4 min, 1 h and 100 h;
  - the running overrun rule;
  - time alone: advancing 10 min with an unchanged plan gives 0 writes (regression for finding 1).
- **Clock:**
  - identity start;
  - a 20 % fleet speed change gives 0 series writes, and every job is within `tol` after the refit;
  - a 2 h pause then unhold gives 0 series writes;
  - a new generation row makes every series P0 once;
  - the slope clamp holds.
- **Budget:** the B cap, priority order, the three aging promotions, halving and recovery after a throttle signal, and the burst size per window.
- **`outbox.rs`:**
  - the queue op waits only for earlier `Ocr`/`Compiler` ops under S, and other series are not blocked;
  - deadlock regression: staging `series.json` then the queue op, and the queue op then `series.json`, both drain;
  - the queue op runs in the `Background` class.
- **`check_target`:** only Status may write the name, at depth 2 or more, in every fold; root rules are unchanged.
- **`write_path.rs`:**
  - a Drive Status write is 1 call in the governor api log, and a 404 falls back to a create;
  - WebDAV makes no pre-stat;
  - after a restart, 0 writes and `rev` stays monotonic;
  - takeover: the root leftover is deleted, foreign files are rewritten soonest first, empty foreign files are deleted;
  - `status_files: false` writes nothing to the store;
  - the marker carries `series_queue`;
  - the early heartbeat is debounced (at most 1 per 120 s);
  - the store-clock offset is applied.
- **`ocr_http.rs`:**
  - a series GET gives 200, then 304 with `If-None-Match`;
  - the `-gz` tag, HEAD, every fold of the name, 405 on PUT, DELETE, MOVE and COPY onto it;
  - 404 for an unknown series, 200 with an empty file for an existing one;
  - PROPFIND hides it;
  - the v1 root is byte-shape unchanged;
  - the `same_queue` fix.
- **Dedup and names:** a merge trashes the loser's series file; the orphan sweep ignores it.

### 12.2 Contract fixtures

In `interop/contract/bunko/`, exported by `contract_export.rs`:

- **New:**
  - `series-queue-stored.json`;
  - `series-queue-http.json` (with `live` and the ETag);
  - `series-queue-http-not-modified.json`;
  - `series-queue-http-empty.json`;
  - `series-queue-put-refused.json`;
  - `propfind-series-depth1.json` (the file is not listed).
- **Changed:** `marker-active.json` gains `series_queue`, with `queue: null`.
- **Retired:** `queue-file-cloud.json` and `queue-file-cloud-truncated.json`.
- **Unchanged:** `queue-file-http*.json`.
- **Seed:** add a layer row, plus one made-up series with running, queued, unpriced, claimed and row-held jobs and a finished volume.
- **Bless:** `BUNKO_BLESS=1 cargo test -p bunko-server --test contract_export` (`--test it contract_export::` once the single-binary layout reaches this branch).
- **Reader replay fixtures:** `series-queue-get.json` and `series-queue-get-if-none-match.json`, written with `READER_BLESS=1`.
  - Required: `v`, `instance_id`, `rev`, `at`, `rows.0.{kind,id}`, `vols.*.0.0`.
  - Nullable: the `w` and `e` slots, and `live.held`.

### 12.3 Reader tests

- **Parser and clock:**
  - `v === 2` only, unknown fields ignored, malformed jobs dropped;
  - clock interpolation and extrapolation with 2 and 3 knots.
- **Trust:**
  - `live`, versus marker holder plus `instance_id`;
  - highest `rev` across fold-equal folders;
  - a lower `rev` is ignored;
  - torn JSON keeps the last good copy.
- **Store:** one read changes only one slice, with no `set()` on `not-modified` or equal bytes.
- **`describePendingOcr`:** held, `held_rows`, progress interpolation, "finishing", "soon".
- **Providers:** each of the five `readControlFile({folder})` implementations:
  - WebDAV per-segment URL;
  - Drive query with parent ids from the cache;
  - OneDrive path;
  - MEGA parent fold and event with the folder;
  - Local Folder handles and token, plus the root marker.
- **Completions:**
  - a completion runs `refreshSeriesFolder`, never `fetchAllCloudVolumes()` without the change feed;
  - `refreshSeriesIndexes` is never called with a partial map.
- **Fallbacks:** the watch schedule, and the v1 fallback on a 404.
- **`syncable-file.test.ts`:** the new name is not syncable, and the root names are unchanged.
- **`bunko-contract.test.ts`:** drift checks for the new fixtures.

### 12.4 Rig

- **`shared/ocr.spec.mjs`** (connected straight to a cloud library):
  - opening one series reads the marker and exactly that series file, and no other series file;
  - the volume is queued in `vols`, then absent with a `done` entry;
  - the sidecar is pulled;
  - there is no root `.mokuro-queue.json` on the store.
- **Local-folder variant** of the same spec.
- **`api/queue.spec.mjs`:** v2 virtual 200 then 304, plus the v1 root for an old-reader client.
- **Request log:** no queue reads on the grid or library page.

---

## 13. Work split

| # | Repo | Chunk | Depends on |
|---|---|---|---|
| **B1** | Bunko | Core and HTTP: the render module `ocr/series_queue.rs` (owed board plus plan); the clock fit; the dirty test; HTTP sink plus virtual route, ETag, gzip and 304; v1 `same_queue` fix; lazy plan pricing; fixtures `series-queue-http*`. | #93 merged |
| **R1** | Reader | Provider API: `readControlFile({folder})` on all 5 providers; the MEGA event for the new name; Local Folder root and series reads; per-segment WebDAV encoding. | — (parallel with B1) |
| **R2** | Reader | Queue v2 client: parser; marker `series_queue`; clock; per-series store; series-page loop; display; trust; v1 HTTP fallback; remove the `keepAlive` queue loop. | B1 fixtures, R1 |
| **B2** | Bunko | Store sink: names and `check_target`; outbox dependency fix and `Background` class; budget scheduler; persisted state; settle rule and `done`; marker block and early heartbeat; store clock; takeover cleanup; wire `status_files`; remove the cloud v1 path; fixtures `series-queue-stored` and `marker-active`; doc §9.1/D10 rewrite plus the `storage-transfer-quota` doc fix. | B1 |
| **R3** | Reader | Completions and watches: `refreshSeriesFolder` and `replaceFolderEntries`; scoped hooks; watch schedule and debounced saves; remove `CloudCompletions` and the cloud-root code. | R2, B2 |
| **B3** | Bunko | Status write fast path (Drive by id, WebDAV without stats), with the governor-log test. | B2 |
| **R4** | Reader + rig | Bump `interop/reader.ref`; contract and replay fixtures; rig specs (cloud, local folder, HTTP, request-log assertions). | B2, R3 |

Order: B1 and R1 in parallel, then R2, then B2, then R3, then B3 and R4. A reader with B1 and R2 already works against a Bunko over WebDAV before any store change.

---

## 14. Owner decisions

Decided by the owner on 2026-11-11:

1. **Build it.** No stored root queue file; library-wide state rides the lease marker.
2. **Keep the frozen v1 root file and the manifest over Bunko's WebDAV through 0.8.x** (virtual, in memory, never stored). Remove it once the reader release with R2 is the minimum supported.
3. **Reader 1.11 connected straight to a cloud library gets no queue labels from 0.8.** No running-only v1 file is written to the cloud.
4. **Show "running"** in series files (about one write per volume start; free over HTTP).

Defaults the spec set, not reopened: tolerance `max(60 s, 25 % of time left)`; windows Drive/OneDrive 120 s, MEGA 300 s, WebDAV 30 s, local folder 15 s; the reader polls the marker every ttl/3; the file name `.mokuro-series-queue.json`.

# OCR scheduling and orchestration — behavioural spec (mokuro-bunko 0.5.2 → Rust 0.7)

Status: reverse-engineered from source at commit `199cff5` (release 0.5.2). Every
claim cites `file:line` relative to `src/mokuro_bunko/` unless prefixed `docs/`
or `tests/`. Line numbers are of that commit.

## 0. Scope, conventions, legend

**In scope** — the *orchestration* of OCR, not the model math:

| Python module | What it does | Lines |
|---|---|---|
| `ocr/watcher.py` | `OCRWorker`: library scan, what is owed, claims, slots, sessions, retries, failure records, holds, autobench gating, remote-slot integration, progress cards | 6206 |
| `ocr/processor.py` | `OCRProcessor`: per-slot driver of runner subprocesses, command lines, env, nice, per-volume subprocess road, sidecar install/normalise | 2303 |
| `ocr/session.py` | `OcrSession`: one `engine_runner.py --serve` subprocess, JSON lines both ways | 414 |
| `ocr/staging.py` | copies the runner `.py` files to disk once per content hash | 162 |
| `ocr/eta.py` | `RateModel` (pages/s + latency + startup), `plan_queue` (lane simulation), `earliest_finish_claim` | 1500 |
| `ocr/job_order.py` | `order_jobs` + `natural_key` | 233 |
| `ocr/congestion.py`, `ocr/pipeline_stats.py` (+ `engine_runner.summarize/pipeline_verdict`) | stage busy/blocked/starved readout, verdict, 5-run history | 418+121 |
| `ocr/throughput.py` | real delivered pages/min (display only) | 177 |
| `ocr/utilization.py` | 1 Hz GPU/CPU busy sampler (benchmarks only) | 332 |
| `ocr/control.py` | `OcrControl`: live settings apply, queue file document, holds codes | 659 |
| `ocr/volume_outlook.py` | per-volume `pending` + `recheck_after` | 89 |
| `queue/api.py`, `queue/shape.py`, `queue/state.py` | `/queue` page + `/queue/api/status` JSON, display levels, ETag/304 | 802+629+37 |
| `middleware/queue_file.py` | `/mokuro-reader/.mokuro-queue.json` virtual file | 231 |

Touched only where they feed scheduling: `ocr/remote/{session,registry,scheduler,profiles,protocol}.py`,
`ocr/bench.py` (the hold/pre-empt contract), `ocr/generations.py` (row model),
`ocr/precision.py`, `middleware/upload.py`, `catalog/api.py`.

**Legend used throughout**

- **DROP** — exists only for mokuro / ctd / animetext / rtdetr, or for the
  mokuro-CLI fallback. Do not port.
- **PY-ONLY** — complexity that exists because of Python (GIL, subprocess
  isolation of torch, two venvs, multiprocessing, staging scripts to disk). In an
  in-process Rust/ONNX design the behaviour it protects must be kept, but the
  mechanism can be replaced; the replacement is suggested.
- **KEEP** — observable behaviour; port it exactly (JSON shapes, file formats,
  ordering, numeric constants).
- **WIRE** — part of a protocol another program speaks (readers, processors,
  the queue page JS). Must stay byte/field compatible unless the protocol is
  versioned.

Terminology (as the code uses it):

- **Row / generation** — one `GenerationSpec` (`ocr/generations.py:177-391`): an
  immutable `id` (`g-1`, …), a renameable `name`, `engine`, `detector`,
  `patch_budget`, `pools`, `precision` mode, `primary`, `enabled`. Rows are kept
  in a list; **list order is priority** (`generations.py:961-968`).
- **Job** — `(archive_path, generation_id)`; Python type `tuple[Path, str]`. Never
  `(volume, engine)` (`docs/ocr-internals.md:27-30`).
- **Slot** — one concurrent worker (`_OcrSlot`, `watcher.py:371-412`). Local slots:
  `ocr.concurrency` of them (1..8, `config.py:271`). Remote slots: one per
  `max_sessions` of each connected processor (`watcher.py:3446-3451`).
- **Lane** — a slot as the ETA/EFT simulation sees it (same count).
- **Machine / hardware key** — `"local"` (`LOCAL_SLOT`, `watcher.py:123`) or the
  processor's **name** (not its registration id; `watcher.py:4111-4119`).
- **Rate key** — `RateModel` key: the row id for local evidence, `"<id>@<name>"`
  for a processor (`watcher.py:3598-3608`).
- **Scan** — one pass of the OCR loop that drains the queue to exhaustion
  (`watcher.py:4416-4457`).
- **Session** — one open runner process serving one row snapshot, fed volumes
  (`ocr/session.py:146-153`).
- **Claim** — a job marked in-flight and owned by a slot.

---

## 1. Persistent and shared state

### 1.1 Files under `<storage>` (KEEP unless noted)

| Path | Writer | Readers | Format / semantics | Cite |
|---|---|---|---|---|
| `.ocr-failures.json` | worker | worker (every claim!), queue API, `doctor` | object `failure_key → record`; written atomically (`.tmp` + `os.replace`), **deleted when empty**, `indent=2`, `ensure_ascii=False` | `watcher.py:1228-1255` |
| `.ocr-progress.json` | worker | queue API (`read_running_jobs`), catalog `/api/ocr-status` | first running card flattened at top level + `"jobs": [all cards]`; every card has `active: true`, `updated_at`; file **deleted** when nothing runs; atomic write | `watcher.py:1117-1160`, `queue/api.py:784-802` |
| `.ocr-heartbeat` | worker | health endpoint | text: `str(time.time())`; touched every loop and while a held slot waits | `watcher.py:1401-1406`, `4642-4644`, `6069` |
| `.ocr-congestion.json` | worker (`CongestionHistory.record`) | `RateModel` (history source), admin | `{gen_id: [record, … ≤5]}`; atomic; deleted when empty; `indent=2` | `congestion.py:197-274` |
| `.ocr-bench.json` | bench service | `RateModel` (bench source) | `{gen_id: {best:{pages_per_second}, baseline:{…}, startup_seconds, …}}` | `eta.py:812-833` |
| `processors/<name>.json`, `processors/@local.json` | worker (`record_run`), bench, registry | worker (pools, bench prior, autobench gating) | per-machine profile; `LOCAL_PROFILE = " local"` (leading space) maps to `@local.json` | `remote/profiles.py:104-105`, `363-418`, `571-624` |
| `.processing/<stem>_XXXX/` | processor | runner | per-volume scratch workspace (`tempfile.mkdtemp(prefix=f"{stem}_")`), deleted when the volume settles | `processor.py:658-671` |
| `.processing/runner-<sha16>/` | staging | runner subprocess | staged runner scripts; **PY-ONLY/DROP** | `staging.py:86-116` |
| `logs/ocr/<series>_<stem>[.<gen-name>].log` | processor/runner | queue page (admin link), failure records | per-volume log; primary row has no `.<name>` infix; series path joined with `_`, `[<>:"/\\|?*]`→`_` | `processor.py:1449-1483` |
| `logs/ocr/session.<gen-name>.<slot>.log` (+`.stderr`) | runner | humans | one per slot per row | `watcher.py:4671-4680`, `session.py:167` |

Atomic-write pattern everywhere: write `<name>.tmp`, `os.replace` onto the
target. A Rust port must keep atomicity (readers poll these files) — or, better,
see §27: most of these are intra-process IPC and can become in-memory state with a
write-through persisted copy.

### 1.2 Database (via callbacks wired in `server.py:953-999`)

- `missing_pages_lookup(cbz) -> int` = `metadata.compiler.missing_pages_now` —
  pages a supplied `.mokuro` names that the archive lacks (`server.py:964-966`).
- `page_count_lookup(cbz) -> int|None` = `cached_page_count` — metadata cache page
  count (`server.py:971-973`).
- `volume_uuid_lookup(rel_path) -> str|None` = `Database.remembered_volume_uuid`
  (`watcher.py:747-750`).
- Provenance writes (`ocr_sidecars` table) via `ProvenanceRecorder.written /
  rejected / forget` (`watcher.py:694-698`, `5503`, `5533-5544`, `5814-5829`,
  `6123-6124`). Out of scope here except *when* it is called.

### 1.3 In-memory worker state (lost on restart — KEEP that property)

All guarded by one re-entrant condition `self._lock = threading.Condition()`
(`watcher.py:905`). Key fields (`watcher.py:720-916`):

| Field | Type | Meaning |
|---|---|---|
| `_inflight_ocr` | set[Job] | claimed and not settled |
| `_attempted_ocr` | set[Job] | tried in this scan (one try per job per scan) |
| `_cancelled_ocr` | set[Job] | cancel issued (settings change/pre-empt/removal); outcome is not a failure |
| `_claim_owner` | Job → slot | who claimed it (for disconnect returns) |
| `_settling` | set[Job] | owner is writing the outcome right now; a disconnect must not take it back |
| `_job_stamps` | Job → (size, mtime_ns)\|None | archive identity at claim/restamp |
| `_last_served` | gen_id → series | round-robin cursor per row |
| `_queue_cache` | (monotonic, queue_generation, [pending entries]) | queue page list |
| `_queue_generation` | int | invalidation counter for `_queue_cache` (NOT the page state version) |
| `_candidate_walk`, `_candidate_epoch`, `_candidate_walk_seconds` | shared library walk cache | §3.4 |
| `_holds` | machine → count | counted holds (benchmarks) |
| `_stopped_generations` | set[(gen_id, machine)] | row given up for this scan on that machine |
| `_session_strikes` | (gen_id, machine) → int | consecutive sessions dying with 0 volumes completed |
| `_start_backoff` | (gen_id, machine) → `_StartBackoff` | runner-won't-start backoff across scans |
| `_breakers` | processor_id → `_DownloadBreaker` | download circuit breaker |
| `_download_returns` | Job → `_Returns` | counted download returns across scans |
| `_returned_by` | Job → set[processor_id] | who gave it back this scan |
| `_eft_deadlines` | Job → epoch | how long a volume left to a faster lane waits for it |
| `_left_logged` | set[(Job, machine)] | log-once memo (reset per scan) |
| `_autobench_asked/_failed/_inflight/_wanted` | sets/list | §14 |
| `_open_sessions` | set[session] | for shutdown and device-in-use |
| `_active_progress` | Job → card dict (insertion = start order) | §19 |
| `_active_slots` | list[slot] | slots of the running scan (empty between scans) |
| `rates` | `RateModel` | §18 |

---

## 2. Row model facts the scheduler depends on (`ocr/generations.py`)

- `enabled_generations(rows)` = rows with `enabled`, **in list order** — the only
  ordering function; queue order and OS priority both key on it
  (`generations.py:961-968`).
- `primary_generation` = first enabled row with `primary` (`generations.py:971-976`).
- `sidecar_suffix`: `.mokuro` if primary else `.<name>.mokuro`; `sidecar_paths(cbz)`
  = (`<cbz-without-suffix><suffix>`, same + `.gz`) — the `.gz` variant counts as
  done for every row (`generations.py:282-350`).
- `output_affecting()` = `(engine, effective_detector, patch_budget if engine has
  the knob else None)` — the cancel key (`generations.py:352-371`).
- `effective_detector` = engine's own detector if it brings one, else the
  configured one, else `DEFAULT_DETECTOR` (`generations.py:244-256`).
- `reported_detector` = None for monolithic/served rows (`generations.py:258-270`).
- `monolithic` ⇔ `road is None` (mokuro CLI) — **DROP**; `served` ⇔ engine has a
  `serve_module` (mokuro serve) — **DROP** (`generations.py:208-224`).
- `device_stage_keys` = model-bearing stages of the road (`generations.py:331-341`).
- `GenerationPools` = three maps `stage_workers`, `queue_capacity`,
  `stage_device`; `is_empty()` ⇔ all three empty (`generations.py:139-173`).
- `precision_pick`/`precision_why` exist only on the per-machine run copy, never
  stored (`generations.py:199-204`).
- `local_environment_problem(problems, row)`: mokuro-env rows → `problems["mokuro"]`;
  others → `problems["engines"]` or `problems["detector:<effective_detector>"]`
  (`generations.py:1015-1030`). **PY-ONLY** (two venvs + detector extras); in Rust
  this becomes "model files for this row are not present locally".

---

## 3. What is owed (the library scan)

### 3.1 Per-volume owed rows — KEEP

`OCRProcessor.missing_generations(cbz)` (`processor.py:528-557`):

1. `rows = [g for g in enabled_generations if needs_sidecar(cbz, g)]` where
   `needs_sidecar` = file exists, suffix lower `== ".cbz"`, and neither
   `sidecar_paths(cbz)` exists (`processor.py:514-519`).
2. **Missing-pages rule**: if any non-primary row is in `rows` AND the primary
   sidecar (`<stem>.mokuro` or `.mokuro.gz`) already exists AND
   `pages_short(cbz) > 0`, return only the primary rows of `rows` (i.e. none,
   since the primary exists) — layers are withheld from a volume whose supplied
   `.mokuro` names pages the archive lacks. Not a failure, no record, no backoff.
3. Otherwise return `rows` (list order). **No row waits for another**: layers may
   run before / concurrently with the primary on other machines, because every
   sidecar is stamped with the same `volume_uuid` (`processor.py:857-897`, see
   §11.4).

`pages_short` swallows every exception as 0 (`processor.py:559-574`).
`skipped_generations` = non-primary rows the volume lacks while short of pages
(only if primary exists) — used by the queue page list (`processor.py:576-584`,
`watcher.py:1641-1666`).

### 3.2 The walk — KEEP semantics, simplify mechanism

`_walk_candidates` = for every `library/**/*.cbz` that `is_file()`, for every row in
`missing_generations(p)`: `(p, row.id)` (`watcher.py:1487-1495`). Only `*.cbz`
(case-sensitive glob; `needs_sidecar` also checks `.lower()`). `.cbr/.zip/.rar` in
the library are never OCR'd.

### 3.3 Eligibility (failure backoff) — KEEP

`_eligible_ocr_jobs()` (`watcher.py:1444-1485`) — read-only, request-thread safe:

- no `library/` → `([], [])`;
- `candidates = _walked_candidates()`; `failures = _load_failures()`;
- for each candidate whose row still exists: `key = failure_key(rel, row)`;
  - no record → eligible;
  - `_replaced_since_failure(path, rec)` (archive `st_mtime > rec.last_attempt_at + 1.0`,
    1 s epsilon; unreadable stat → mtime 0, `watcher.py:1434-1442`) → eligible
    and `key` added to `stale_records`;
  - `now >= rec.last_attempt_at + retry_delay(rec.attempts)` → eligible;
  - else skipped (in backoff).
- returns `(eligible, stale_records)`. Only the worker's claim/scan passes
  `reset_stale_failures=True`, which deletes those stale keys
  (`watcher.py:1612-1614`, `1565-1571`).

### 3.4 Shared walk cache — PY-ONLY mechanism, KEEP freshness contract

`_walked_candidates` (`watcher.py:1497-1537`): one walk shared by every claim and the
queue page. Valid while `epoch` unchanged and age `<= 8 × last_walk_duration`
(`CANDIDATE_WALK_FACTOR=8.0`), but **never cached** when the last walk took
< 0.1 s (`CANDIDATE_WALK_CACHE_MIN_SECONDS`) (`watcher.py:130-138`). Single-flight
via `_candidate_walk_lock`; the result is stored only if the epoch did not move
during the walk. Kept current in place:

- `_forget_candidate(job)` on success (`watcher.py:1545-1550`) and on
  `not _still_owed` at claim (`watcher.py:2812-2816`);
- `archive_arrived` replaces that archive's entries with fresh
  `missing_generations` (`watcher.py:2269-2274`);
- `archive_removed` drops entries under the removed path (`watcher.py:2175-2180`);
- `_invalidate_candidates` (epoch++) on settings change (`watcher.py:1539-1543`,
  `1060`).

Because the cache may be up to minutes old, every claim re-checks the one job it
hands out: `_still_owed` = archive `is_file()` and neither sidecar path exists
(`watcher.py:1552-1563`).

The rationale in the docstring is explicitly GIL contention ("starved every
request of the interpreter", `watcher.py:1502-1505`). **Rust**: keep a library
index maintained incrementally (WebDAV hooks + periodic re-walk as backstop), keep
the per-claim `_still_owed` re-check; the 8× heuristic may be retained or replaced
by "re-walk every poll interval".

Memo tables `_path_keys`, `_rel_paths`, `_archive_pages_cache` are cleared whole at
200 000 entries (`PATH_KEY_CACHE_MAX`, `watcher.py:141`, `1257-1268`, `1573-1590`,
`1808-1811`).

### 3.5 Page counts — KEEP

- `_page_count(cbz)` = `page_count_lookup` if `int > 0`, else None; never raises
  (`watcher.py:1757-1773`).
- `_archive_pages(cbz)` = count of zip members that are not directories, not
  system files (`reader_compat.is_system_file`), with an image extension
  (`reader_compat.is_image_extension`); 0 or unreadable → None; memoised by
  `(path, size, mtime_ns)` (`watcher.py:1775-1813`).
- `_known_pages` = `_page_count or _archive_pages` (`watcher.py:1815-1817`). Used by
  pending entries, the EFT walk and running cards without `total_pages`.

---

## 4. Queue order — KEEP exactly (`ocr/job_order.py`)

The queue page shows the worker's list verbatim; ordering lives in one pure
function.

### 4.1 `natural_key(text)` (`job_order.py:100-149`, memoised 65 536)

1. `folded = NFKC(text).casefold()`.
2. Scan with regex `(?P<arabic>\d+)(?:\.(?P<fraction>\d+))?|(?P<kanji>[〇一二三四五六七八九十百千]+)`
   (`job_order.py:53-55`). `\d` is Unicode digits; each digit is converted with
   `int(ch)` individually.
3. Arabic match → number token `(0, int(arabic), fraction_digits.rstrip("0"))`.
   Fraction compared as a **string** after stripping trailing zeros (so `"25" <
   "5"` gives 10.25 < 10.5).
4. Kanji match → token only if `_kanji_value(run)` is not None **and**
   `_stands_as_number(folded, start, end)`; otherwise the run stays inside the
   surrounding text token.
5. Text between numbers → `(1, 0, text)`. Number sorts before text at the same
   position; a prefix sorts before a longer name (tuple comparison).

`_kanji_value` (`job_order.py:58-82`): all chars in `〇一…九` → positional digits
(`一〇`=10); else multiplicative: digits `一..九` (no `〇`), units `十=10 百=100 千=1000`
strictly decreasing left to right, each unit takes at most one digit, missing digit
= 1 (`十一`=11, `二十`=20, `百二`=102, `千九百`=1900); `十十`, `二三十`, `〇` in
multiplicative form → None.

`_stands_as_number` (`job_order.py:85-97`): true if char before is `第`, or char
after is in `巻話章集`; else true iff neither neighbour `isalnum()` (string ends,
spaces, punctuation).

Tie-break: `_name_key(name) = (natural_key(name), name)` (`job_order.py:152-154`).

### 4.2 `order_jobs(jobs, generation_rank, key, last_served)` (`job_order.py:157-233`)

- `key(job) -> (series, volume, generation_id)`.
- **position**: for each `(generation, series)`, sort that group's distinct
  volumes by `_name_key`; position = index. Positions are over the jobs *passed
  in* (pending only) — callers MUST exclude in-flight, already-attempted and
  backed-off jobs before calling (`watcher.py:1615`, docstring `job_order.py:182-190`).
- `cursor[g] = _name_key(last_served[g])`.
- sort key: `(rank.get(g, len(rank)), g, position, wrapped, _name_key(series), _name_key(volume))`
  where `wrapped = 0 if cursor is None or series_key > cursor else 1` — each row's
  round starts with the series **after** the one it served last, wrapping.
- Unranked generations run after ranked ones; generation id string breaks ties
  between unranked ones.

The worker's cursor `_last_served[gen_id] = series` is set **at claim time**,
whatever the outcome (`watcher.py:2830-2833`); in memory only.

`_generation_rank()` = `{row.id: index}` over enabled rows (`watcher.py:1425-1427`).

`_ocr_candidates(exclude, reset_stale_failures)` (`watcher.py:1592-1623`) =
eligible − exclude → `order_jobs(..., last_served=copy)`. `_upcoming_ocr_jobs` =
`_ocr_candidates(exclude=inflight ∪ attempted)` — the single source for claims
and the queue page (`watcher.py:1625-1639`).

---

## 5. Worker lifecycle and threads

### 5.1 Start / stop

`start(background=True)` (`watcher.py:6129-6168`):
1. mkdir `library/`.
2. Unless `thumbnails_only`: `_remove_corrupt_sidecars()` — every
   `library/**/*.mokuro*` file ending `.mokuro` or `.mokuro.gz` that is not
   parseable JSON (gz-aware) is deleted and its provenance row forgotten
   (`watcher.py:6108-6127`, `processor.py:711-725`).
3. Threads: `ocr-sidecar-worker` (`_run_ocr_loop`) and `ocr-thumbnail-worker`
   (`_run_thumbnail_loop`); `thumbnails_only` starts only the latter.

`stop()` (`watcher.py:6170-6201`): `_running=False`, `_stop_requested=True`,
bump + notify; `close()` every open session, wait 2 s each, kill stragglers; join
threads 5 s; kill whatever is still in `_open_sessions`. Volumes in flight are
**released, never failed** (see `_end_session` `blame_oldest`, §9.8, and the
`volume_failed`-during-stop branch `watcher.py:5174-5185`).

### 5.2 OCR loop (`watcher.py:6059-6075`)

```
while running:
    touch heartbeat
    if not every_machine_held() and not held_for_hardware():
        try: scan_ocr_once()  except: log "OCR scan error: …"
    wait_poll_interval()
```
`_wait_poll_interval` sleeps `poll_interval` in 0.1 s steps, ending early on stop
or when `_wake` is set (then clears it) (`watcher.py:6049-6057`).
`poll_interval` default 30 s (`config.py:335`; constructor default 30.0).

`_every_machine_held()` (`watcher.py:3399-3415`): over connected remote entries
"held" = `holds[name] > 0` or breaker open; plus local if local slots exist:
`holds["local"] > 0`. True iff the list is non-empty and all true.

`_held_for_hardware()` (`watcher.py:6077-6097`) = `processing_hold() is not None`
(local processing off and no processor connected), logging once per transition.
`processing_hold()` returns `{"reason":"no-processor","since":…,"last":{name,disconnected_at}?}`
where `since` = last disconnect time or worker start (`watcher.py:4355-4376`).

### 5.3 Scan (`_scan_ocr_once`, `watcher.py:4416-4457`)

1. `_prune_failure_records()` (§12.4).
2. Under lock reset per-scan state: `_attempted_ocr`, `_returned_by`,
   `_left_logged`, `_stopped_generations`, `_session_strikes`,
   `_autobench_asked`; bump queue generation.
3. Compute candidates (with stale-failure reset) only to log
   "Found N missing OCR sidecar(s) across V CBZ file(s) (generations, in run
   order: …; within a generation series take turns[; K at a time])".
4. `_drain_ocr_queue()`; finally clear `_attempted_ocr`, `_returned_by`, bump.

### 5.4 Drain (`watcher.py:4459-4490`)

`slots = _all_slots()` = local slots + one slot per `max(1, max_sessions)` of each
connected, non-local, not-installing processor (`watcher.py:3424-3451`).
`_active_slots = slots`. No slots → return (queue holds). If a remote registry
exists → `_supervise_slots(slots)`. Else slot 0 runs on the scan thread and slots
1.. on helper threads, joined at the end (one slot ⇒ no threads at all).

`_supervise_slots` (`watcher.py:4500-4561`) loops until every slot thread has
ended, every ~1 s (`SLOT_SUPERVISE_SECONDS`):
- **restarts** a slot whose loop ended (`running` False, not gone, its machine not
  held) once `_queue_generation` has moved past the value it exited at
  (`exited_generation`) — cost: one extra claim (`watcher.py:4527-4547`);
- **adds** slots for any processor that connected during the scan (keyed by
  `processor_id` in `seen`), unless its name is held; logs
  "`<label>` joined the running scan with N slot(s)" (`watcher.py:4548-4561`).

### 5.5 Slot loop (`_run_ocr_slot_loop`, `watcher.py:4598-4644`)

```
loop while not stop_requested:
    if slot gone (its processor dropped): return
    gen = queue_generation
    job = claim_next(slot)
    drain_autobench_requests()          # fire benchmark requests outside the lock
    if job:
        row = slot.generation
        if session_row(row, slot): run_session(slot, row, job)
        else:                      run_ocr_job(job, slot)      # per-volume road
        continue
    with lock:
        held = holds[slot.machine] > 0
        if not held and no job in flight anywhere and no autobench in flight
           and not slot.waiting_for_faster:
            return                          # queue drained for every slot
        if held or queue_generation == gen:
            lock.wait(5 s)                  # SLOT_IDLE_WAIT_SECONDS
    if held: touch heartbeat
```
`_run_ocr_slot` wraps it setting `slot.running=True/False`,
`waiting_for_faster=False`, `exited_generation=queue_generation` on exit
(`watcher.py:4577-4596`). A helper slot's exception is logged as
"OCR slot N stopped: …" and does not end the scan (`watcher.py:4563-4575`).

`_session_row(row, slot)` (`watcher.py:4646-4662`): remote slot → always session;
local → `sessions_enabled and not processor.runs_mokuro_cli(row)`.
`runs_mokuro_cli` = monolithic or (served and serve module missing) — **DROP**
(`processor.py:380-382`). In Rust the per-volume road (`ocr.sessions: false`) only
matters if kept as a fallback; see Open Questions.

Wakeups (`self._lock.notify_all()`): any finish/release, holds/releases,
autobench settled, breaker opened/closed, strike stop, disconnect, stop, and
first-time EFT leaves (§8.4). A slot that missed a wakeup notices because
`_queue_generation` moved and retries immediately.

---

## 6. `_queue_generation` vs page state version

Two counters, do not conflate (KEEP the distinction; it is what makes the queue
page cheap):

- `_queue_generation` (`watcher.py:829`, `1086-1115`): invalidates the cached
  pending list. `_bump_queue_generation(claimed=job)` keeps the cache but removes
  that job's entry; `keep_cache=True` keeps it unchanged (finish of a claimed job,
  which already left the list); plain bump drops it. **Does not** bump the page
  version.
- `queue_state: QueueStateVersion` (`queue/state.py`), shared with `OcrControl`
  and the queue page (`control.py:96-102`): bumped after every progress-file write
  (`watcher.py:1131-1137`), failure-file save (`1238-1243`), settings change
  (`1023`), pending list recomputed to something different (`1705-1708`),
  arrivals/removals that change the cached list (`2192`, `2316-2317`). Monotonic
  for the process; carries a random `epoch` hex (unused by readers here).

---

## 7. The claim — KEEP (`_claim`, `watcher.py:2646-2854`)

`claim_next(slot, generation_id=None)` returns a job; `claim_for_session(slot,
gen_id)` returns `(job|None, preempt: bool)` from the same single walk
(`watcher.py:2476-2530`). The walk/pricing happens **outside** the lock; the
take is a re-checked critical section so two slots never take one job.

### 7.1 Phase A — quick refusals (under lock)

Return `(None, False)` if: `holds[slot.machine] > 0` or stop requested; slot's
processor dropped (`_slot_gone`); slot's download breaker open
(`watcher.py:2654-2665`). Snapshot `unavailable = inflight ∪ attempted`.

### 7.2 Phase B — proposal (no lock)

1. `proposed = _ocr_candidates(exclude=unavailable, reset_stale_failures=True)`
   — ordered (§4).
2. `offered(gen_id)` memo per claim: `slot is None` → True; else row exists and
   `_slot_refusal(slot,row) is None` (`watcher.py:2670-2678`). `_slot_refusal`
   (`watcher.py:2532-2544`): local slot → `local_environment_problem` first, then
   `processor.can_run(row)`:
   - local `can_run` (`processor.py:384-405`): any `stage_device` pin other than
     `""/auto/cpu` that the probed device catalog does not know → refusal;
     else `precision.row_refusal(row, catalog)` (forced format unsupported).
   - remote `can_run` (`remote/registry.py:639-652`) =
     `scheduler.catalog_can_run(catalog, row, stage_device=<the machine's
     effective placement>)` (`remote/scheduler.py:51-92`): engine in
     `catalog.engines`; mokuro-env rows need `catalog.serves_mokuro` (**DROP**);
     else `effective_detector` in `catalog.detectors`; no pin to an unreported
     device; `row_refusal` against the processor's reported `gpus` formats.
3. For each distinct row in `proposed` that this slot is offered:
   remote slot → `remote_devices[id] = _remote_engine_device(entry,row)`; if
   `autobench and autobench_needed(entry,row)` → `unmeasured.add(id)`
   (`watcher.py:2686-2698`).
4. `backed_off = _backed_off_rows(slot, machine, rows)` (§9.10).
5. `left_to_warm = _rows_left_to_warm(slot, machine, proposed)` — only when
   `generation_id is None` (a slot opening a new session) (§8.6).
6. `(eft_skip, eft_left, eft_mine, eft_fresh) = _eft_left(slot, machine, proposed)`
   — also for session top-ups (§8).

### 7.3 Phase C — take (under lock)

1. Re-check hold/stop/slot-gone (the disconnect path takes the same lock, so a
   claim either lands before a disconnect and is returned by it, or never lands).
2. **Pre-empt** (only `report_preempt` with a `generation_id`): `preempt = True`
   iff some job in `proposed` has `rank < rank(generation_id)` and is not
   stopped on this machine, not backed off, not unmeasured, and is `offered`
   (`watcher.py:2743-2756`). (Only work *this* slot could take counts, else a
   processor that cannot run the earlier row would churn sessions.)
3. **Device spread** (`watcher.py:2766-2775`, `2587-2644`): `busy_devices` = the
   engine devices of sessions open **on this slot's machine** (resolved against
   that machine's cards); if `generation_id is None` and `busy_devices` non-empty,
   stable-partition `proposed` into jobs whose row's engine device is free first,
   then busy. Engine device = the row's `mokuro` stage if present else `engine`
   stage pin (default `auto`), resolved by `resolve_device(asked, gpu=has_gpu)`
   (`watcher.py:2546-2571`). **Note for Rust**: this exists so two rows pinned to
   two cards run side by side; keep.
4. **EFT override** (`watcher.py:2776-2788`): if the walk gave this slot a job
   (`eft_mine`): when topping up a session and `eft_mine` is a *different* row →
   `ordered = []` (the session drains; this machine should switch rows); else
   move `eft_mine` to the front.
5. Walk `ordered`, skipping: other rows when `generation_id` set; `(row, machine)`
   stopped; row backed off or left to warm; job in `eft_skip`; job in
   inflight/attempted; this processor returned it this scan (`_returned_by`);
   row gone; not `offered`; not `_still_owed` (also `_forget_candidate`); row
   `unmeasured` → `_want_autobench(entry,row)` (record only) and skip.
6. First survivor is **taken** (`watcher.py:2824-2841`): add to `_attempted_ocr`
   and `_inflight_ocr`; `_note_claim` (stamp `(size, mtime_ns)`); drop its EFT
   deadline; `slot.waiting_for_faster=False`; `_last_served[row] = series`;
   bump with `claimed=job`; `slot.job=job`, `slot.generation=row` (frozen
   snapshot), `_claim_owner[job]=slot`. Return `(job, preempt)`.
7. Nothing taken: `waiting = bool(eft_skip ∩ proposed)`;
   `slot.waiting_for_faster = waiting`; if `waiting and eft_fresh` →
   `notify_all()` (wake the machine a volume was just left to). Outside the lock,
   log "Leaving X (row) to Y: done there in ~Ts; Z would take ~Us" once per (job,
   machine) (`watcher.py:2842-2854`, `3063-3080`).

A `slot=None` claim (tests / legacy callers) skips all slot filters and EFT.

---

## 8. Local vs remote: "whichever finishes first" (earliest-finish list scheduling) — KEEP

Every slot of the running scan, local or remote, is a lane. When a lane asks
for work, the head of the queue is walked and each volume is assigned to the
lane that would **finish it first**; the asking lane takes the first volume
assigned to itself. Docs: `docs/ocr-internals.md:669-709`. Tests:
`tests/unit/test_eft_assign.py`, `test_eft_claim.py`, `test_warm_session_first.py`.

### 8.1 Constants (`eta.py:1319-1324`, `watcher.py:97-101`)

| Name | Value | Meaning |
|---|---|---|
| `EFT_MARGIN` | 0.10 | asker keeps a volume within 10 % of the best other lane… |
| `EFT_MARGIN_CAP_SECONDS` | 5.0 | …capped at 5 s… |
| `EFT_SLACK_SECONDS` | 2.0 | …plus 2 s |
| `EFT_LOOKAHEAD` | 256 | jobs walked per claim |
| `EFT_CLAIM_GRACE_SECONDS` | 15.0 | grace after the predicted start before anyone may take a left volume |
| `MOKURO_EFT_TRACE` env | `""`/`"0"` off | log every decision |

### 8.2 Lanes (`_eft_lanes`, `watcher.py:2981-3061`)

Returns None (→ plain first-come) when the asking slot is not in
`_active_slots`, when there are < 2 active slots, or when any lane's in-flight
work cannot be priced. For each active slot:

- skip if its machine is held, it is gone, it is not the asker and its loop is
  not `running`, or (remote) its breaker is open;
- `free_in` = Σ over its claimed in-flight jobs of: the card's `eta_seconds` if
  numeric, else `rate_for(row, machine).volume_seconds(max(0, total − done))`
  where `total = card.total_pages or _known_pages(archive)`; if neither rate nor
  total → **return None**;
- `warm` = row id of its open, non-closing session (None if none);
- `rows` = enabled rows among the walked ones that are not stopped on that
  machine, not start-backed-off, not refused by `_slot_refusal`, and not
  autobench-needed.

`EftLane(key=id(slot), machine, free_in, warm, rows)` (`eta.py:1327-1341`).

### 8.3 The walk (`earliest_finish_claim`, `eta.py:1369-1464`)

Inputs: `jobs = [(job, gen_id, pages|None)]` for the first 256 of `proposed`
(`pages = _known_pages`, `watcher.py:2881-2889`); `rate_for(gen, machine)`;
`startup_for(gen, machine) -> seconds` (§18).

```
median = round(median(known positive page counts)) or None
me = lane with key == asking; if none: return None
state[lane] = (free_in, warm)
for (job, g, pages) in jobs[:256]:
    if pages unknown/≤0: if median is None: return None; pages = median
    best=None; best_at=+inf; mine_at=None
    for lane in lanes where g in lane.rows:
        rate = rate_for(g, lane.machine)   (memoised per (g, machine))
        if rate is None: return None       # an unpriceable lane → first-come
        (free, warm) = state[lane]
        start = free if warm == g else free + startup_for(g, lane.machine)
        at    = start + rate.latency + pages / rate.pps
        if lane is me: mine_at = at
        elif at < best_at: best, best_at, best_starts = lane, at, start
                           best_busy = state[lane].free > 0
    if mine_at is not None and (best is None or
         mine_at <= best_at + min(best_at*0.10, 5.0) + 2.0):
        decision.mine = job; return decision           # asker takes it
    if best is None: continue                          # nobody may run it
    if mine_at is not None:
        decision.left.append(EftLeft(job, to=best.key, there=best_at,
                                     here=mine_at, starts=best_starts,
                                     busy_first=best_busy))
    state[best] = (best_at, g)                         # book it on that lane
return decision                                        # mine may be None
```

Notes: a lane that may not run the row does not compete; a job the asker cannot
run is booked on its best lane but **not** reported as "left"; the asker's own
state is never advanced (the walk stops at its first volume). Startup is charged
whenever the lane's (booked) row differs, so booking a lane switches its warm row.

### 8.4 Deadlines (`_eft_left`, `watcher.py:2858-2952`)

If the decision is None or leaves nothing → no skip, return `mine` (may be None).
Otherwise, under lock: drop deadlines of jobs no longer in `proposed`; for each
left job: `predicted = now + max(0, left.starts) + 15`; existing deadline `known`:

- `busy_first` (target lane has work first): `deadline = predicted if known is None else max(known, predicted)` — follows the moving prediction, never shrinks;
- idle target: `deadline = predicted if known is None else min(known, predicted)` — fixed clock, never pushed back.

Jobs with `now < deadline` go into `skip`; `fresh` = some job had no deadline
before. Skipped jobs keep the asker in `waiting_for_faster` so it stays in the scan
(`watcher.py:2842-2851`); the slot loop does not exit while
`waiting_for_faster` (§5.5); the supervisor restarts exited slots when the queue
moves (§5.4). The queue page reports such a machine as `standby` (§23.5).

### 8.5 Pricing callbacks (`_lane_pricing`, `watcher.py:1979-2020`)

Shared by EFT and the queue plan so "the machine a volume is predicted on is the
machine it is given to":

- `rate_for(gen, machine, observed_pages=0, observed_seconds=0)` =
  `rates.rate_on(gen, None if machine is None else rate_key(gen,machine),
  machine_prior=RateEstimate(bench.pages_per_second,"bench") if >0)`;
- `startup_for(gen, machine)` = `rates.startup_on(gen, rate_key, machine_prior=bench.startup_seconds)`;
- `bench` = that machine's profile row for the row's **current recipe, mode and
  supported formats** (`_machine_bench`, `watcher.py:2022-2055`; `LOCAL_PROFILE`
  for local), memoised per call.

### 8.6 Single volume left: prefer a warm session elsewhere (`_rows_left_to_warm`, `watcher.py:3089-3173`)

Only for a slot opening a **new** session. For each row that has exactly **one**
proposed job not in-flight/attempted, and for which some *other* machine (not
held, breaker closed) has an open, live, non-closing session: compute

- `mine = startup_on(g, my_key).seconds + my_rate.volume_seconds(pages)`
  (requires `pages = _page_count` — metadata cache only — and `my_rate`);
- for each warm machine with a rate: `theirs = Σ remaining of its in-flight jobs
  of that row (card eta or rate × remaining pages) + rate.volume_seconds(pages)`;
- if `theirs < mine` → leave the row (log once
  "Leaving X (row) to M: its warm session reads it in ~Ts; opening one on … would take ~Us").

The volume stays pending; the warm session's own top-up claims it.

---

## 9. Sessions (local) — the runner lifecycle

### 9.1 Constants (`watcher.py:103-147`)

| Name | Value |
|---|---|
| `SESSION_LOOKAHEAD` | 2 volumes submitted-but-unfinished per session |
| `SESSION_WEDGE_SECONDS` | 600 s with no event at all ⇒ wedged |
| `SESSION_POLL_SECONDS` | 1.0 s event poll |
| `SESSION_CRASH_LIMIT` | 2 consecutive no-volume deaths ⇒ row stopped on that machine for the scan |

### 9.2 Opening (`_run_session`, `watcher.py:4682-4758`)

1. `clock = _SessionClock(started_at=now)` — started **before** spawn; startup
   measured from "decided to open" (`watcher.py:4716-4719`, `302-316`).
2. `session = slot.processor.open_session(row, session_log)`; `OSError` →
   `_fail_session_start(first_job, …)` (§9.9).
3. Register: `slot.session = session`, `slot.job = None`, add to `_open_sessions`.
4. If `first_job` was cancelled meanwhile → `session.kill()`;
   `finish_ocr_job(first_job, ok=False)` (records nothing: cancelled branch).
5. `session.start()` False → if cancelled meanwhile: finish as cancelled; else
   take the error from the next event within 5 s (`spawn_failed.error`) or
   "runner would not start" → `_fail_session_start`.
6. `_submit_session_volume(first_job)`.

### 9.3 Main loop (`watcher.py:4760-4831`)

```
loop:
  if draining is None:
     draining = drain_reason(session, row, machine)    # §9.5; log once
  while draining is None and len(inflight) < 2 and
        (inflight empty or now >= next_claim_at):
     job, preempt = claim_for_session(slot, row.id)
     if job is None:
        if preempt: draining = "an earlier generation has work"
        else: next_claim_at = now + max(1.0, poll_interval)   # don't re-walk per page event
        break
     next_claim_at = 0
     if not submit(job): draining = "the runner stopped accepting volumes"; break
     if preempt: draining = "an earlier generation has work"; break
  if inflight empty: break                       # clean end
  ev = session.poll_event(1 s)
  if ev is None:
     wedged = now - last_event > 600
     if remote_session_lost(session, wedged): continue    # §16.4
     if wedged: kill; fatal_error = "the <row> runner stopped responding (no event for 600s)"; break
     continue
  last_event = now
  match ev.event:
     "exit"  → fatal_error = session_exit_error(...); break
     "fatal" | "spawn_failed" → set fatal_error (do not break; the exit follows)
     else    → completed += handle_session_event(ev)    # §9.6
finally: end_session(...); discard from _open_sessions; slot.session=job=generation=None
```

The empty-claim throttle means a session only re-walks the library once per
`poll_interval` while it has a volume in flight; when it has nothing in flight the
claim runs immediately (that claim IS the "is there more?" check — a volume that
arrives as the last one finishes keeps the session open).

### 9.4 Submitting (`_submit_session_volume`, `watcher.py:4933-4989`)

- `row = live row by id or the session's snapshot` — so a rename applies to
  volumes submitted after it; the row used is stored on the `_SessionJob`.
- `job_id = "v<n>"` from a process-wide counter (`watcher.py:4666-4669`).
- `volume = processor.prepare_session_volume(cbz, row, job_id)` (§11.1);
  `OSError` → `finish_ocr_job(ok=False, failure=str(e))`, returns True (session
  continues).
- local slot → `_restamp(job)` (archive stamp re-taken now: this is the file the
  runner reads).
- `session.submit(volume)` False → rmtree workspace;
  `release_ocr_job(reason="the runner closed before it took the volume",
  retry_this_scan=True)`; return False.
- Record `_SessionJob(job, generation=row, volume, slot=index, clock, owner=slot,
  hardware=machine, delivered = local)`; `begin_ocr_job(..., total_pages=_page_count,
  processor=label, machine, session_ready=clock.ready_at is not None,
  delivered)` (§19).

### 9.5 Drain reasons (`_session_drain_reason`, `watcher.py:4991-5016`)

In order: worker stopping; machine held; `(row, machine)` stopped; (remote)
breaker open; row removed or disabled; `not session.is_alive()` → "the runner
exited". Draining = stop submitting, finish accepted volumes, then close.

### 9.6 Event handling (`_handle_session_event`, `watcher.py:5018-5195`)

Returns 1 iff a volume completed and was installed.

| Event | Action |
|---|---|
| `ready` | log "row session ready in Xs[ on M][: pipeline]"; `rates.record_startup(rate_key(row, machine), startup_seconds)`; `clock.ready_at = now`; set `session_ready: true` on every in-flight card; `_note_start_success` (clears start backoff) |
| `stats` | `pipeline = summarize_event_stats(ev.pipeline)`; if `cpu_pressure` or `other_cpu` present: `host_busy = busy_reason(ev) is not None`; merge into every in-flight card |
| (others need `ev.id` ∈ inflight, else ignored) | |
| `fetch` (remote) | `state == "ready"` → `delivered = True` on entry and card; `_download_delivered` (§16.1). Any other state: progress only (resets wedge timer) |
| `volume_returned` (remote) | pop entry; rmtree workspace; `_judge_returned` (§16.2) |
| `volume_started` | `delivered = True`; `started_at = monotonic`; `total_pages = ev.pages`; refresh card |
| `page` | `done_pages = ev.done or 0`; `total_pages = ev.total` if truthy; `first_page_at = now` on the first event with `done > 0`; refresh card (§19.2) |
| `volume_done` | pop entry. `first_of_session = clock.completed == 0`; `clock.completed += 1`. If busy (§9.7): log "…ran on a busy host (…); its speed is not learned…"; else `rates.record_volume(rate_key, pages, seconds, first_of_session)`. Local → `_record_local_run(row, pages, seconds, contended)` → `profiles.record_run(LOCAL_PROFILE, congestion=None)`. Remote → `profiles.record_run(name, contended, pages, seconds, congestion=build_record(summary, volume, volume_pages, volume_seconds, volume_first))`. Then `_collect_session_volume` (§11.2) |
| `volume_failed` | pop entry. If stopping → `release_ocr_job("the worker is stopping")`; else `finish_ocr_job(ok=False, failure=(ev.error or "<row> could not read this volume", volume.log))`. rmtree workspace |

### 9.7 Busy-host judgement (`watcher.py:149-167`, `5410-5428`)

`busy_reason(ev)`: `cpu_pressure ≥ 0.6` → "CPU pressure N%"; else `other_cpu ≥ 0.5`
→ "other processes used N% of the CPU"; else None. A busy volume is still a
success; only its speed is not learned (not in `RateModel`, not in congestion
history, profile counts it under `runs.contended`). The runner computes
`cpu_pressure` from Linux PSI `/proc/pressure/cpu` `some avg10/100` and
`other_cpu` from `/proc/stat` minus the CPU of every runner process tree over the
volume window (`engine_runner.py:2157-2165`, `2336-2345`). **Rust**: an in-process
engine can compute `other_cpu` as host busy − own process CPU (getrusage), which is
simpler than walking `/proc` for runner trees.

### 9.8 Ending (`_end_session`, `watcher.py:5611-5740`) — KEEP accounting exactly

1. If not closing and alive → `close()`; wait 30 s else kill + wait 5 s; join reader 5 s.
2. `processor_left` = remote entry dropped. `cancelled` = any in-flight job in `_cancelled_ocr`.
3. If **never ready** and `fatal_error` and not processor_left and not cancelled and
   not stopping → `_note_start_failure(row, machine, signature, fatal_error)`
   (signature: remote → hash of the session's `row_spec` + processor_id; local →
   hash of `_local_run_row(row).to_dict()` + "local").
4. No in-flight volumes: if `fatal_error` and `completed == 0` and not
   processor_left → `_strike_session`. Return.
5. `error = fatal_error or "the <row> runner ended before it finished this volume"`.
   `blame_oldest = not stopping and not processor_left`.
   `environment = (remote and not ready) or precision_refused(fatal_error)`
   (`PRECISION_REFUSAL = "precision not available here"`, `engine_runner.py:385`).
   `oldest = order[0]` iff blame_oldest and not environment and `order[0]` still in
   flight and **delivered**.
6. For each in-flight job in submit order: oldest → `finish_ocr_job(ok=False,
   failure=(error, volume.log))` (a real failure with backoff); every other →
   `release_ocr_job(reason=error, retry_this_scan=True)`. rmtree each workspace.
7. If `not blame_oldest or cancelled` → return. `completed == 0` →
   `_strike_session`; else clear the `(row, machine)` strike count.

`_session_exit_error` (`watcher.py:5588-5609`): None (clean) when closing with
nothing in flight and code ∈ {0, None}, or not killed with nothing in flight and
code ∈ {0,None}; killed → `fatal_error` (may be None); else
"the <row> runner exited[ with status N]: <fatal_error or last stderr line> | before its volumes were finished".

`_strike_session` (`watcher.py:5742-5769`): `strikes[(row,machine)] += 1`; at
`≥ 2` add to `_stopped_generations`, bump, notify, log "Stopping <row>[ on M] for
this scan: N sessions in a row ended without finishing a volume (…)". Cleared at
the next scan.

### 9.9 A session that never started (`_fail_session_start`, `watcher.py:4892-4931`)

Log; unless the slot is gone: `_note_start_failure`. If remote **or** precision
refused → `release_ocr_job(retry_this_scan=True)` and (if not gone and not
precision) `_strike_session` — environment failures never blame a volume. Local
non-precision → `finish_ocr_job(ok=False, failure=error)` — **a real failure of
that one volume** (so the backoff throttles a broken local env).

### 9.10 Start backoff across scans (`watcher.py:250-266`, `3177-3276`)

Per `(gen_id, machine)`: `_StartBackoff(failures, until, error[:300], signature, name)`.
`_note_start_failure`: `failures = prev.failures+1` if same signature else 1;
`until = now + retry_delay(failures)` (same formula as volume retries, §12.3);
log "Not starting <row>[ on M] again for Ns: its runner could not start (k
time(s) in a row): …". `_backed_off_rows` drops an entry whose current signature
differs (row as that machine would run it changed → retry at once), and reports
rows with `now < until`. Cleared by `ready`. Exposed per machine as
`[{generation, until, failures, error}]` (`start_backoffs`).

### 9.11 Processor → runner subprocess (PY-ONLY; DROP the mechanism)

`OCRProcessor.open_session` → `OcrSession(row, session_command(row, log),
env=ocr_env(row), popen_kwargs=priority(is_backlog))` (`processor.py:1668-1689`).

`session_command` (`processor.py:1691-1733`):
`<engines-python> <staged>/engine_runner.py --serve --engine E --detector D
--generator "mokuro-bunko <ver>" --session-log L [--patches N]
[--mokuro-python P (served only, DROP)] [--stage-workers k=v,…]
[--queue-capacity k=v,…] [--stage-device k=v,…] [--precision MODE]
[--precision-pick F [--precision-why W]]`, where the row is first passed through
`run_row` = `_local_run_row` (profile pools for an unconfigured row, precision
pick; `watcher.py:3822-3848`). Stage maps are serialised `key=value` joined by `,`
sorted by key (`processor.py:153-175`). `--precision` omitted when the mode is the
default (`processor.py:178-195`).

`ocr_env(row)` (`processor.py:1888-1920`): copy of `os.environ` +
`PYTHONIOENCODING=utf-8`, `PYTHONUNBUFFERED=1`, `MOKURO_OCR_JOBS=<concurrency>`;
plus `HF_HUB_OFFLINE=1`/`TRANSFORMERS_OFFLINE=1` when every HF repo of the row is
fully cached and the operator set neither. **Rust**: `MOKURO_OCR_JOBS` becomes the
in-process host budget divisor (see `docs/ocr-internals.md:410-413`); HF env DROP.

OS priority (`processor.py:2225-2250`): `is_backlog_generation(row)` ⇔ row is not
the **first enabled row** (by id); backlog rows get `preexec_fn=os.nice(10)` (POSIX)
or `BELOW_NORMAL_PRIORITY_CLASS` (Windows). Benchmarks always run un-niced
(`processor.py:1804-1808`). **Rust**: keep the *policy* (head row normal, others
lower) via per-thread niceness (`setpriority(PRIO_PROCESS, tid, 10)` on Linux) of
the worker threads serving backlog rows, or a priority-aware executor. Open
question whether ONNX intra-op threads inherit it.

Staging (`staging.py`): runner `.py` files hashed (sha256 of name\0text\0…,
first 16 hex) and copied to `.processing/runner-<hash>/` once; other builds
pruned unless held by a live session (`hold/release_staged_runner`); processors
pin one build for their lifetime (`pin_runner`). **DROP entirely** in-process.
The *runner build* string (`mokuro-bunko <ver>, runner <hash>`) feeds provenance
(`watcher.py:4147-4177`) — Rust should replace it with a build id.

`OcrSession` (`session.py`): Popen with stdin/stdout pipes (text, utf-8,
`errors=replace`, line-buffered), stderr → `<session log>.stderr`; a daemon
reader thread parses stdout lines into a queue; non-JSON or non-object or missing
`event` string ⇒ counted `garbage_lines`, dropped (`session.py:379-399`). After
EOF the reader `wait()`s and enqueues `{"event":"exit","returncode":code}` —
always the last event, exactly once (`session.py:353-377`). Spawn failure ⇒
`spawn_failed` then `exit(None)` (`session.py:214-218`). `kill()` before start ⇒
the runner never starts and an `exit(None)` is queued (`session.py:192-198`); kill
during Popen ⇒ killed on creation (`session.py:219-227`). `close()` sends
`{"op":"close"}` then closes stdin (`session.py:265-283`). `stderr_tail()` = last
non-empty line of the last 4000 bytes, ≤300 chars (`session.py:326-338`).

**Rust in-process replacement**: a `Session` object owning the pipeline threads,
an mpsc of the same event enum, `submit(volume)`, `close()`, `kill()` (cancel
token), `exit` emitted exactly once on drop/finish. Keep the event vocabulary
because remote processors still speak it on the wire (§17).

---

## 10. The per-volume road (non-session) — mostly DROP

Used locally when `ocr.sessions: false` or the row runs the mokuro CLI.
`_run_ocr_job` (`watcher.py:5771-5839`) → `processor.process_library_ocr(cbz,
row)` (`processor.py:1279-1420`): extract the whole archive into a workspace
(dropping a top-level `<stem>.webp` thumbnail), run the runner CLI (`--input
<dir> --output <ws>/<stem><suffix> --cache-dir <ws>/_ocr/<id>/<stem> --stats-file
<ws>/_detect/<id>/pipeline.json …`, `processor.py:1563-1637`) or the mokuro CLI
(**DROP**), poll every 2 s (`processor.py:2056-2223`):

- progress = count of `*.json` under `<ws>/_ocr` (`processor.py:987-992`);
  `first_page_at` = first poll with `done > 0`;
- hard timeout 3600 s; no-progress (unchanged count) 600 s; "finalizing"
  (done ≥ total) longer than 900 s → accept if a valid sidecar exists else fail;
- cancel check each poll (`_cancel_now` = `_cancel_requested` or the worker's
  `_cancelled_ocr` membership, `processor.py:440-442`);
- non-zero exit → error from the log (last traceback line / loguru ERROR / "No
  module named"), `processor.py:1485-1523`; mokuro's "Processed successfully: d/t"
  with d<t ⇒ failure (`processor.py:1525-1540`) — **DROP** (mokuro);
- success → `_record_run_rate(done−1 pages over now − first_page_at)` into
  `RateModel` and the local profile (`processor.py:1050-1074`).

Then collect the sidecar, `publish_guard()` (archive still current) else
`last_discarded=True` → caller releases (`_DISCARDED`) and audits a rejection;
normalise; move beside the archive with a unique name; harvest
`pipeline.json` → `last_pipeline` congestion record.

The inbox path (`OCRProcessor.process`, `processor.py:1102-1257`;
`InboxWatcher`, `watcher.py:415-611`) is **not wired** by the server
(`OCRWorker.watcher` is never assigned; nothing calls `_on_new_file`) — see Open
Questions. Thumbnails (`_run_thumbnail_loop`, cover WebP 250×350 q85,
`.nocover` marker, `processor.py:586-656`) are out of scope but share the poll
interval.

**Rust**: the per-volume road only buys isolation (one subprocess per volume) and
mokuro-CLI compatibility; with in-process ONNX a "session of one volume" is the
same code path. Recommend dropping it; keep `ocr.sessions` as a no-op or remove.

---

## 11. Settling outcomes, collecting sidecars — KEEP

### 11.1 Per-volume paths fixed at submit (`prepare_session_volume`, `processor.py:1811-1850`)

`SessionVolume` (`session.py:79-143`): `id` (`v<n>`), `workspace` (fresh
`.processing/<stem>_XXXX`), `output = ws/<stem><row.sidecar_suffix>`,
`cache_dir = ws/_ocr/<row.id>/<stem>`, `detect_dir = ws/_detect/<row.id>`,
`log` (§1.1), `title` = series folder name (or stem when directly in
`library/`), `volume` = stem, `title_uuid = uuid5(NAMESPACE_DNS, title)`,
`volume_uuid = volume_uuid_for(cbz, row)`, `archive = cbz`, `archive_size =
st_size or None`, `stem` (remote-only). All derived from the row **as it is at
submit**, so a rename only affects later submits.

### 11.2 Collection (`_collect_session_volume`, `watcher.py:5485-5586`)

1. `_take_for_settling(job, owner)`; if the claim is no longer this slot's (a
   disconnect returned it) → log "Ignored a late … outcome …", rmtree, return
   False (`watcher.py:5983-5998`, `6008-6014`).
2. `_archive_still_current(job)` false (stamp `(size, mtime_ns)` differs from the
   one recorded at claim/restamp, or file gone; no recorded stamp ⇒ only existence
   matters, `watcher.py:2236-2243`) → audit rejection `_DISCARDED`, rmtree,
   `release_ocr_job(reason=_DISCARDED, retry_this_scan=True)`.
3. Read sidecar facts (provenance) **before** normalising; compute destination;
   `error = install_session_sidecar(...)` (`processor.py:1861-1886`):
   - not valid JSON (gz-aware) → "the <row> sidecar it wrote is not readable
     JSON" if it exists else "no valid <row> sidecar generated";
   - `_normalize_mokuro_metadata` (§11.4); move to
     `session_sidecar_destination` = row's plain/gz path beside the archive, or
     `_get_unique_path` when taken (`processor.py:1852-1859`, `2260-2281`:
     `<stem>_<n><suffix>` with `Path.stem/suffix`, i.e. `Vol.mokuro` →
     `Vol_1.mokuro`, `Vol.x.mokuro` → `Vol.x_1.mokuro`, `Vol.mokuro.gz` →
     `Vol.mokuro_1.gz`) — move failure → "could not move …".
   - an exception inside install releases `_settling` and re-raises.
4. error → audit rejection; success → provenance `written(...)`, congestion
   record (local, not contended) = `build_record(summarize_event_stats(ev.stats),
   volume=rel, volume_pages, volume_seconds, volume_first)`, card set to
   100 %/done.
5. `finish_ocr_job(job, row, ok=error is None, failure=(error, log), pipeline)`;
   rmtree workspace.

### 11.3 `finish_ocr_job` / `release_ocr_job` (`watcher.py:5907-5981`, `4378-4414`)

`finish_ocr_job(job, row, ok, failure, pipeline, slot)`:
- not owner → ignore late (log), return.
- `returned = not ok and slot is gone` (disconnect race).
- unless returned or cancelled: forget `_download_returns[job]`.
- `ok` → `_forget_candidate`, `_clear_ocr_failure`, `_record_congestion(row.id,
  pipeline)` (§21.4);
- returned → log "Returned … : <label> disconnected";
- cancelled → log "Skipped <row> for <file>: cancelled, not a failure of the volume";
- row still configured → `_record_ocr_failure` (§12);
- row gone → log "Skipped …: the generation is no longer configured".
- finally (lock): `_settle(job)` (remove from inflight, cancelled, stamps,
  settling, owner); if returned also drop from `_attempted_ocr`;
  `_bump_queue_generation(keep_cache = not returned)`; `notify_all`;
  `_clear_active_progress(job)`.

`release_ocr_job(job, row, reason, retry_this_scan=False, slot)`: "give back,
record **nothing**" — not-owner → ignore late; log "Returned <row> for <file> to
the queue: <reason>"; settle; if `retry_this_scan` drop from `_attempted_ocr`;
plain bump (cache dropped, the job reappears in pending); notify; clear card.

### 11.4 Sidecar normalisation & volume uuid (`processor.py:857-984`) — KEEP

`_normalize_mokuro_metadata(sidecar, cbz, row)` rewrites the JSON in place
(gz-aware, compact separators, `ensure_ascii=False`):
`title = series name`, `volume = stem`, `title_uuid = uuid5(DNS, title)`,
`volume_uuid = volume_uuid_for(cbz, row)`; for non-primary rows also stamp
`ocr_engine`: keep existing keys, `setdefault id=engine`,
`setdefault generator="mokuro-bunko <ver>"`, set `generation=row.name`.

`volume_uuid_for(cbz, row)` order: (if row is not primary) primary sidecar's
`volume_uuid`; remembered id from DB; oldest-mtime non-primary layer's
`volume_uuid` other than this row's own; `deterministic_uuid("<Series>/<stem>")`.
This is what allows layers to run before/concurrently with the primary.

### 11.5 Cancellation sources (all mark `_cancelled_ocr` **before** killing)

| Source | What is killed | Cite |
|---|---|---|
| settings change: row removed, disabled, or `output_affecting()` changed | every session of that row (whole session), every per-volume job of it | `watcher.py:960-1076` |
| benchmark pre-empt of a machine | every session and per-volume job on that machine; jobs also removed from `_attempted_ocr` so other machines may take them now | `watcher.py:3333-3397` |
| archive removed / replaced (stamp differs) | per-volume job; a session only if **all** its held claims are stale; otherwise the stale result is discarded at collection | `watcher.py:2155-2234` |

Pool/device/precision-pick/name/primary/order changes never cancel.

---

## 12. Failure records and retries — KEEP

### 12.1 Key (`failure_key`, `watcher.py:1270-1289`)

`rel = str(cbz relative to library/)` (OS separators; falls back to the absolute
path). Primary row → `rel`; other rows → `rel + "@" + row.name` (name, not id,
on purpose: human readable; a rename makes old records stop applying).

### 12.2 Record (`_record_ocr_failure`, `watcher.py:1339-1383`)

```json
{"series": "<rel parent or ''>", "volume": "<stem>", "generation": "<row name>",
 "engine": "<engine>", "detector": "<reported_detector|null>",
 "error": "<message or 'unknown error'>", "attempts": <prev+1>,
 "last_attempt_at": <epoch float>, "log_file": "<path|null>"}
```
Log: "OCR (<row>) failed for <rel> (attempt N, next retry in ~Ss): <error>".

### 12.3 Backoff (`_retry_delay_seconds`, `watcher.py:1394-1399`)

`delay(attempts) = min(poll_interval × 4^min(max(0, attempts−1), 16), 3600)`.
With the default 30 s: 30, 120, 480, 1920, 3600, 3600…

### 12.4 Clearing and pruning

- success → delete the key (`watcher.py:1385-1392`);
- archive replaced (mtime > last_attempt + 1 s) → eligible at once; the record
  is deleted by the next worker claim (`watcher.py:1477-1480`, `1612-1614`);
- `_prune_failure_records` at every scan and every settings change
  (`watcher.py:1291-1337`): keep a record iff (its `generation` field is not a
  non-empty string, or is a current row name) AND (its `series`/`volume` are not
  both strings, or some `library/<series>/<volume>.{cbz,cbr,zip,rar}` exists).
  Never parse the key.
- Download returns that reach the limit are recorded as failures
  ("download failed on N tries (…): class: error", §16.2).

Failed and cancelled runs never enter the congestion history or the rate model.

---

## 13. Live settings changes — KEEP

### 13.1 `OCRWorker.apply_settings(rows, poll_interval, local_unavailable)` (`watcher.py:960-1065`)

1. `rows or default_generations()`; reconfigure every in-use slot's processor
   (and the primary processor if no local slot uses it); `self.generations`.
2. `poll_interval` replaced if > 0.
3. Under lock: reset `_autobench_asked` and `_autobench_failed` (a new config is
   a new chance); bump queue generation; bump page version; compute
   `cancelled[row.id] = reason` for each distinct row snapshot held by an in-use
   slot via `_cancel_reason` ("the generation was removed from settings" |
   "the generation was disabled" | "its engine, detector or patch budget
   changed"); add all in-flight jobs of those rows to `_cancelled_ocr`; collect
   their sessions (kill) and per-volume jobs (cancel).
4. Outside: `cancel_active()` / `session.kill()` with log lines.
5. `CongestionHistory.prune(current ids)`; `_invalidate_candidates()`;
   `_prune_failure_records()`; log "OCR settings applied (generations, in run
   order: …)".

### 13.2 `OcrControl.apply(rows, poll_interval)` (`control.py:468-563`)

Returns `{"applied","installing","restart_required","reason"}`:

- no worker / thumbnails-only → `restart_required`, "OCR is disabled in this server process";
- local processing off → apply now, `applied`;
- compute environment problems (mokuro env missing → **DROP**; engines env
  missing; detector extras missing → background install thread, return
  `installing` with reason "installing <id> (<pkgs>); settings apply when it is
  ready" or "still installing …"); after install, failed detectors become
  `detector:<id>` problems and the settings apply anyway;
- otherwise apply with `local_unavailable = problems`; if problems:
  `applied + restart_required`, reason "<why> — until then only a connected
  processor runs <names>".

`_apply_now` also updates `queue_api.generations`, the engines installer's
detector set, `runtime["generations"/"active_generations"/"detectors"]`, and
re-resolves GPU use (`control.py:611-645`). **PY-ONLY**: venv/extras install
logic; in Rust "installing" = model download.

---

## 14. Holds, benchmark pre-emption, autobench gating — KEEP the contract

### 14.1 Holds (`watcher.py:3280-3331`)

Counted per machine (`_holds[machine]`), so two holders cannot un-hold each
other. `hold_queue(timeout=900, machine)`: increment, bump, notify, then wait
(0.25 s steps) until no in-flight job is on that machine or timeout; returns
whether quiet. Nothing is killed; sessions drain (§9.5). `release_queue`:
decrement/delete, bump, notify. `queue_held` = any count > 0.

`preempt_for_bench(timeout=900, machine)` → `(quiet, preempted)`
(`watcher.py:3333-3397`): increment hold; for every in-flight job on that machine
(owner's hardware): add to `_cancelled_ocr`, remove from `_attempted_ocr`
(visible to other machines immediately); kill those sessions, cancel those
per-volume jobs; wait until empty; `preempted = [{"generation": name, "volume":
stem}]` sorted by (path, gen id).

`BenchService` runs one line per machine; it calls `preempt_for_bench` once when
the first benchmark of the line starts and `release_queue` once when the line is
empty (`bench.py:1146-1197`, timeout `QUEUE_HOLD_TIMEOUT=3600`).

A held slot does not leave the scan: it waits (§5.5) and keeps the heartbeat
fresh. Claims on a held machine return nothing (§7.1).

### 14.2 Autobench gating (`autobench_kind`, `watcher.py:3640-3710`)

Returns `"full"`, `"precision"` or None for `(machine entry|None, row)`:

1. None if autobench off, no bench service, row monolithic (**DROP** rule), local
   with local processing off, or `(profile_name, row.id)` in `_autobench_failed`.
2. `supported` = machine's formats for the row's model device; `profile =
   profiles.row(name, row.id, recipe=output_affecting, mode=precision, supported)`
   (None if missing or recipe mismatch; `stale_bench` when the stored bench no
   longer matches mode/candidates/format — `remote/profiles.py:254-323`).
3. `hand_set` = local and `not row.pools.is_empty()` (`watcher.py:3763-3772`).
4. `full` if not hand_set and (profile None or stale_bench). Any current entry
   (even pools-only) ⇒ not full.
5. else `precision` if `_precision_pick_needed`: row's engine takes precision,
   mode ∈ balanced/speed, `supported` known, ≥ 2 usable candidates on that
   device, and no current pick in the profile's bench.
6. Local rows that would run the mokuro CLI → None (**DROP**).

`_want_autobench` records `(entry,row)` once per pair per scan, marks it
in-flight; `_drain_autobench_requests` (after the claim, outside the lock)
re-asks `autobench_kind`, logs "Benchmarking <row> on <label> before it runs
there" / "…'s precision (<mode>) … its pools stay as set", and calls
`bench.enqueue(row.id, spec_or_None, processor=machine, autobench=True,
precision_only, on_done=…)`; a refusal settles as "refused"
(`watcher.py:3970-4049`).

`_autobench_settled(name, gen, state, ran_on)` (`watcher.py:4051-4102`): remove
from in-flight; if not done: if the machine *left* (`ran_on.dropped`, else "no
entry of that name connected"; local never leaves) → may be asked again this scan;
else add to `_autobench_failed` → the row runs **untuned** there from now on
(until settings change/restart). Bump + notify. While any autobench is in
flight, idle slots do not exit (§5.5).

Rows awaiting a benchmark are skipped by that machine's claims and by its EFT lane
(`rows` set) but stay pending for others.

### 14.3 Per-machine run row

- Local: `_local_run_row(row)` = row itself if hand-set (pools non-empty) and no
  pick; else copy with profile pools (`machine_pools(profile.pools, own)` table by
  table, then `runner_pools` dropping `"auto"` widths/capacities) unless a pinned
  device is no longer reported (then the row's own table, logged once) and with
  `precision_pick/why` from the profile bench (`watcher.py:3774-3848`).
- Remote: `_remote_row_spec(entry,row)` = `row.to_dict()` with `pools =
  _remote_pools(entry,row)` and the machine's pick (`watcher.py:3480-3549`);
  `_reachable_placement` forces ORT-GPU-only detector stages to `cpu` on a
  processor whose onnxruntime reported no GPU provider (`watcher.py:3551-3595`).
  **Note for Rust**: the ORT-provider special case generalises to "every stage" once
  everything is ONNX; keep the "pin to a device the machine does not report ⇒ use
  the row's table, log once" rule.

### 14.4 Precision holds (`watcher.py:3900-3945`)

`precision_holds()`: for each enabled row with a **forced** mode (fp32/bf16/fp16)
that every current machine (local if processing locally + connected processors)
refuses → `{row.id: "No connected machine can run <mode>"}` (`precision.py:52-54`).
No machines → `{}` (the no-processor hold covers it). Held rows: no lane takes them
in the plan (§20), queue file state `held`, admin list `held_rows`.

---

## 15. WebDAV hooks: arrivals and removals — KEEP

Called by `middleware/upload.py` after a successful PUT/MOVE/COPY (arrival) or
DELETE/MOVE (removal) (`upload.py:150-156`, `207-226`), via `OcrControl`
(`control.py:197-213`).

### 15.1 `archive_arrived(cbz)` (`watcher.py:2245-2319`)

Path must resolve inside `library/` and end `.cbz` (case-sensitive).
1. `_cancel_stale_jobs(path == cbz)` — a replaced archive's running jobs are
   cancelled (§11.5).
2. Replace the archive's entries in the candidate walk with
   `missing_generations(cbz)`.
3. If a cached pending list exists (fresh **or stale**): rebuild it by key
   `(series, volume, gen_id)` — drop this volume's old entries, add
   `_pending_entry` for each owed job not in inflight/attempted, re-run
   `order_jobs(by_key, rank, last_served)` (keys are the tuples themselves) —
   and store it only if neither the cache object nor `_queue_generation` moved
   meanwhile; bump the page version if the list changed and was fresh.
4. Unless `processing_hold()` → set `_wake` (the OCR loop scans now).

The PUT response then prices the volume (§20.6): `control.volume_pending(cbz,
series, volume, running, wait=0.005, max_items=300)` (`upload.py:72-76`,
`219-247`) and sets `X-Mokuro-Manifest: <manifest url>` and
`X-Mokuro-Recheck-After: <seconds>` (`upload.py:78-79`).

### 15.2 `archive_removed(path)` (`watcher.py:2155-2192`)

`path` may be an archive or a folder. `gone(job_path)` = equality for a `.cbz`
(suffix lowercased) else ancestor test. Cancel stale running jobs; drop matching
entries from the candidate walk and the cached pending list (bump page version
if any dropped).

### 15.3 `pending_within(wait)` / `last_pending()` / `refresh_pending()` (`watcher.py:2076-2092`, `2325-2372`)

- `last_pending`: the cached list if its `queue_generation` is current (however
  old), else compute (`pending_jobs`).
- `pending_within(wait)`: cached-and-current → it; else start `pending_jobs()` on
  a daemon thread, join up to `wait`; if not done, return the stale cache or None.
  **PY-ONLY** mechanics (thread + join); Rust: spawn + timeout.
- `refresh_pending(max_age=5)`: recompute only if the cache is invalid or older
  than `_queue_max_age(max_age) = max(max_age, 4 × last pending walk seconds)`
  (`QUEUE_CACHE_WALK_FACTOR`, `watcher.py:2437-2448`).

`pending_jobs(max_age=5)` (`watcher.py:1668-1709`) is single-flight
(`_queue_compute_lock`), never writes (no stale-record reset), records the walk
duration, stores `(now, queue_generation_at_start, entries)` and bumps the page
version if the list differs from the previous one.

`_pending_entry(job, failures)` (`watcher.py:1711-1755`) — **WIRE** (queue page,
queue file, manifests):
```json
{"series": "<posix rel parent>", "volume": "<stem>", "generation": "<name>",
 "generation_id": "<id>", "engine": "<engine>", "detector": "<reported|null>",
 "pages": <int|null>,
 "attempts": <int>,                       // only if a non-stale failure record exists
 "returned": {"count","class","error","machine","at"}  // only if download returns exist
}
```

---

## 16. Remote processors as slots (scheduling-relevant parts only)

### 16.1 Download delivered (`_download_delivered`, `watcher.py:5206-5255`)

On `fetch{state:"ready"}`: restamp the job (the processor's verified download is
the file the result must match); forget the job's download returns; reset that
processor registration's breaker (`proven=True`, `consecutive=0`,
`open_until=0`, `hold=600`, `last_error=""`), and if it had been open: bump,
notify, log "<label> fetched an archive again; no longer held",
`entry.note_breaker(None)`. Log the transfer line (`requests`, `bytes`,
`mb_per_s`, `restarts`, `repairs`, `verdict`, `damaged`) at INFO if anomalous
(requests>1, restarts, repairs or verdict) else DEBUG.

### 16.2 A returned claim (`_judge_returned`, `watcher.py:5257-5339`)

`klass = ev.class or "local"` (≤40 chars), `error` ≤300 chars.
1. stopping → release.
2. archive gone → release "<label>: the archive is gone" (not retried this scan).
3. size differs from `volume.archive_size` → release "<label>: the archive changed
   after it was sent", retry this scan.
4. `klass ∈ {stalled, differs}` → `read_own_copy(path)` (sequential 1 MiB reads on
   a helper thread, 60 s timeout, `watcher.py:187-219`); unreadable → **record a
   failure** "the library cannot read its own copy of this archive: …".
5. `job_counted = _note_download_return(slot, klass, error)` — breaker: job counts
   only if the processor is `proven` and had `consecutive == 0` before;
   `klass == "changed"` never counts against the processor; otherwise
   `consecutive += 1`, and at ≥ 3 (and not already open) `open_until = now + hold`,
   `hold = min(2·hold, 3600)`, log "Holding <label> for N min: k archive
   downloads in a row failed (last: …)", bump, notify, `entry.note_breaker`
   (`watcher.py:5341-5378`).
6. `counted = job_counted and klass != "no_room"`; `_note_job_return` (a new file
   stamp resets the count; `machines` appended when counted) (`watcher.py:5380-5407`).
7. `counted and count ≥ 3` → **record a failure** "download failed on N tries
   (m1, m2, …): <class>: <error>".
8. else add processor to `_returned_by[job]` (not offered to it again this scan)
   and release with "<label> could not fetch the archive (<class>): <error>",
   retry this scan.

Breakers are keyed by **registration** (`processor_id`) and dropped on disconnect
(`watcher.py:4339-4341`); open breaker ⇒ no claims, no EFT lane, session drains.

### 16.3 Disconnect (`processor_disconnected(entry, reason)`, `watcher.py:4300-4353`)

Wired as `registry.on_drop` before the worker starts (`server.py:985-986`). Under
lock: every claim owned by a slot with that `processor_id` and **not settling** is
settled (forgotten) and removed from `_attempted_ocr` (re-offered this scan);
sessions of that entry collected; breaker dropped; bump; notify. Outside: `end()`
each session (its exit arrives on the next poll); clear the cards; log
"<label> disconnected (<reason>); N volume(s) back in the queue". Late outcomes
from the old slot are ignored via `_take_for_settling` (§11.2).

### 16.4 Silently gone processor (`_remote_session_lost`, `watcher.py:4845-4885`)

Remote session only, entry not already dropped. Drop the processor (→ §16.3) if:
its events body was never opened within `EVENTS_OPEN_SECONDS = 30` of
`open_session`, or the session is wedged (600 s silence) **and** the entry has
sent nothing (not even pings) for `EVENTS_SILENCE_SECONDS = 30`
(`remote/protocol.py:104-128`). A processor still pinging with a wedged runner is
treated like a local wedge (oldest delivered volume blamed).

### 16.5 RemoteSession duck-type (`remote/session.py:47-443`)

Same interface as `OcrSession`. Out: `open_session{sid, generation: row_spec}`,
`volume{sid, claim: <volume.id>, archive: <archives_root + library-relative
posix path>, sidecar_name: <output file name>, title, volume_title, title_uuid,
volume_uuid, size?}`, `cancel{sid, claim}`, `close_session{sid}`. At most
`MAX_OUTSTANDING_VOLUMES = 2` claims per session (wire guard,
`remote/session.py:44`). In: frames on the events body: `ping` (ignored),
`sidecar{id, name}+payload` (written atomically to `volume.output` **before** the
`volume_done` is queued; wrong name ⇒ audit rejection and end the session),
`exit` (one exit only), `fatal`/`spawn_failed` (remembered as `stderr_tail`),
`volume_done/volume_failed/volume_returned` (free the claim slot at once),
`fetch`, plus every runner event passed through. `kill()` = `cancel` each claim +
`close_session` + local `exit`. The wire protocol itself is specified elsewhere
(processor spec); this spec only requires the event vocabulary in §17.2.

---

## 17. Runner protocol — JSON lines over stdio (WIRE for processors; PY-ONLY locally)

Locally this is an artefact of running torch in a separate venv. In Rust the
local runner becomes in-process, but the **same vocabulary** must remain because
(a) remote processors forward it (§16.5) and (b) the worker logic is written
against it. Recommend an internal `enum RunnerEvent` with serde matching these
shapes.

### 17.1 Ops (server → runner stdin; one JSON object per line)

```json
{"op":"volume","id":"v12","workspace":"/abs","output":"/abs/<stem><suffix>",
 "cache_dir":"/abs","detect_dir":"/abs","log":"/abs/log",
 "title":"…","volume":"…","title_uuid":"…|null","volume_uuid":"…|null",
 "archive":"/abs/x.cbz", "stem":"…"?}          // or "input":"/abs/dir" instead of archive
{"op":"close"}
```
(`session.py:122-143`). Runner side (`engine_runner.py:8237-8265`): unreadable
line → warn, skip; `close` or EOF → finish accepted volumes, exit 0; ops `page`,
`end`, or `volume` with `pages:"stream"` ⇒ one `fatal` ("this runner takes
archives only; …") and stop reading (old processor); unknown op → warn; volume
after close → warn, ignored. Missing/duplicate id → logged only, **no event**.
Every accepted volume op yields exactly one `volume_started` and exactly one
terminal event (`engine_runner.py:8142-8169`); an op that throws while accepting
yields `volume_started{pages:0}` + `volume_failed{error}`; zero pages ⇒
`volume_failed "no page images found in <stem>.cbz|path"`.

### 17.2 Events (runner stdout → server; one JSON object per line, ASCII-escaped)

| Event | Fields | When | Cite |
|---|---|---|---|
| `ready` | `startup_seconds` (round 3), `weights` {repo: sha}, `stage_workers`, `queue_capacity`, `stage_device`, `pipeline` (graph line str) | models loaded | `engine_runner.py:8290-8298` |
| `fatal` | `error` | models would not load / pipeline died / load failure mid-run / old-op trip; non-zero exit follows | `8278-8284`, `8312-8324`, `8211-8227` |
| `volume_started` | `id`, `pages` | volume accepted and paged | `8198` |
| `page` | `id`, `done`, `total` | each page leaves the sink (in page order) | `7790` |
| `stats` | `pipeline` (raw snapshot, §21.1), `cpu_pressure`?, `other_cpu`? | ≤ every 2 s while pages flow (`PIPELINE_STATS_INTERVAL`) | `7807-7820` |
| `volume_done` | `id`, `pages` (results), `failed_pages`, `seconds` (round 3), `stats` (this volume's window report), `cpu_pressure`?, `other_cpu`? | sidecar written (tmp + rename) | `7975-7987` |
| `volume_failed` | `id`, `error` | volume over without sidecar (every page failed, abandon on session end) | `8017-8023`, `8329-8335` |
| *server-made* `exit` | `returncode` int\|null | stdout EOF / remote end; always last | `session.py:31-37` |
| *server-made* `spawn_failed` | `error` | could not spawn / remote disconnected before open | `session.py:214-218`, `remote/session.py:132-136` |
| *remote* `fetch` | `id`, `state` (`ready`/progress states), `requests`, `bytes`, `mb_per_s`, `restarts`, `repairs`, `verdict`, `damaged` | download progress | `watcher.py:5073-5080`, `5235-5251` |
| *remote* `volume_returned` | `id`, `class`, `error` | processor gives the claim back undelivered | `watcher.py:5081-5086` |

Volume `seconds` semantics: from `max(volume first fed, previous volume's end in
this session)` to now — the volumes of a session partition its time
(`engine_runner.py:7999-8015`). Unknown events are ignored by the server
(`session.py:57-61`).

---

## 18. Rate model (ETA) — KEEP exactly (`ocr/eta.py`)

### 18.1 Clock rule

A rate is **pages over time between page emissions**: `emission_rate(M, t) =
(M−1)/t` for `M ≥ 2, t > 0`, else None (`eta.py:146-158`). Model load / pipeline
fill never enter a rate; startup is charged separately once per session.

### 18.2 Constants (`eta.py:72-133`)

`SESSION_ALPHA=0.5`, `MIN_INFLIGHT_PAGES=4`, `BLEND_PRIOR_PAGES=16`,
`SESSION_PRIOR_PAGES=64`, `DEFAULT_STARTUP_SECONDS=20` (rough), `CACHE_TTL_SECONDS=5`,
`MAX_LATENCY_SECONDS=60`, `LATENCY_SAMPLES=8`. Source labels: `session`,
`session (opening volume)`, `history`, `bench`, `volume`; fitted sources get the
suffix `" (fit)"`; blends `"<a>+<b>"`. Startup sources `session`, `bench`, `default`.

### 18.3 Cost model

`RateEstimate(pages_per_second, source, volumes_observed, latency_seconds)`:
`seconds_for(p) = p/pps`; `volume_seconds(p) = latency + p/pps` (`eta.py:249-279`).

`fit_volume_cost(samples, alpha=0.5)` (`eta.py:188-238`): keep pairs with
pages>0, seconds>0; need ≥ 2 distinct page counts. Weights `w_i = (1−α)^(last−i)`
(newest = 1). Weighted least squares: `D = W·Σwx² − (Σwx)²`; `D ≤ 0` → None;
`slope = (W·Σwxy − Σwx·Σwy)/D`; `slope ≤ 0` → None; `intercept = (Σwy − slope·Σwx)/W`
clamped to `[0, 60]`; returns `(latency=intercept, seconds_per_page=slope)`.

### 18.4 Per-key session evidence (`_SessionRate`, `eta.py:301-361`)

`add(pages, seconds, steady)`: always `volumes += 1`, `pages += p`,
`last_at = now`, `recent.append((p,s))` (keep 20). If not steady (session's
opening volume): `opening_rate = r` or EWMA(α) of it, return. Else
`pages_per_second = r` (first) or `α·r + (1−α)·prev`; `steady += 1`;
`samples.append((p,s))` keep last 8.

`record_volume(key, pages, seconds, first_of_session)` — invalid numbers ignored
(`eta.py:395-434`). `record_startup(key, s)` stores the last measured startup
(`eta.py:436-442`). `forget(gen_id)` drops `gen_id` and `gen_id@*` keys
(`eta.py:444-454`) — note: never called by the worker in 0.5.2 (see Open Questions).

`_session_rate(entry)` (`eta.py:719-745`): fit over `samples` → rate+latency
(`session (fit)`); else steady EWMA (`session`, latency 0); else opening rate
(`session (opening volume)`); else None. `volumes_observed = entry.volumes`.

### 18.5 Priors

- history (`_history_rate`, `eta.py:747-810`): `.ocr-congestion.json[gen_id]`
  runs → pairs `(volume_pages or pages, volume_seconds or elapsed)`, steady-only
  (excluding `volume_first`) unless that leaves none; fit → `history (fit)`; else
  pooled Σp/Σs → `history`, latency 0.
- bench (`_bench_rate`, `eta.py:812-823`): `.ocr-bench.json[gen_id]` →
  `best.pages_per_second` else `baseline.pages_per_second` (>0), latency 0.
- Files re-read at most every 5 s; any error ⇒ `{}` (`eta.py:835-856`).

### 18.6 Combining (`_over_prior`, `eta.py:692-717`)

`session = _session_rate(entry)`; no session → prior; no prior → session; else
`w = session_pages/(session_pages + 64)`; `spp = w/session.pps + (1−w)/prior.pps`;
`latency = w·session.lat + (1−w)·prior.lat`; source = session's if `w ≥ 0.5` else
`"<session>+<prior>"`; `volumes_observed = session.volumes_observed`.

`_with_inflight(base, observed_pages, observed_seconds)` (`eta.py:551-573`):
`live = emission_rate(...)`; if `live` None or `observed_pages < 4` → base; base None
→ `RateEstimate(live, "volume", 0, 0)`; else `w = n/(n+16)`,
`spp = w/live + (1−w)/base.pps`, latency kept, source `"<base>+volume"`.

### 18.7 Which evidence for which machine

- `rate(gen)` = `_with_inflight(_over_prior(gen, history or bench), …)` (`eta.py:493-510`, `632-646`).
- `rate_on(gen, machine_key, machine_prior, observed…)` (`eta.py:512-549`,
  `648-690`):
  - local (`machine_key` None or == gen): `_over_prior(gen, history or bench or machine_prior)`;
  - remote `gen@name`: own session over `machine_prior` (its profile bench) →
    else this server's session over history → else the `gen@*` machine with the
    most session pages (its session, no prior) → else the row's bench; then the
    in-flight blend.
- `startup(gen)` = measured session startup → `.ocr-bench.json` `startup_seconds`
  (>0) → default 20 s rough (`eta.py:585-594`).
- `startup_on(gen, key, machine_prior)` (`eta.py:596-620`): local → `startup(gen)`,
  but if that is the default and `machine_prior > 0` → `(prior, bench)`;
  remote → its measured → its prior → `startup(gen)`.

### 18.8 Real throughput (display only, `ocr/throughput.py`)

Never used to predict. `throughput_of(samples)` = Σpages / (Σ seconds of samples
without an end + union length of `[end−s, end]` windows) (`throughput.py:71-116`).
`RateModel.throughput(key)` uses the last 20 `recent` pairs (no end times ⇒ plain
sum). `profile_throughput(runs)` prefers `runs.recent` (with `at`), then the
congestion records' volume pairs, then cumulative pages/seconds
(`throughput.py:145-177`).

`speed_report(running, within=6h)` (`watcher.py:2374-2435`) — **WIRE** (raw
status `speed`):
`[{generation, generation_id, machines:[{machine, pages_per_minute (1dp),
volumes, lanes, working}], combined_pages_per_minute (1dp | null)}]` for each
enabled row with evidence in the window or a running lane; `lanes` = running cards
of that (row, machine) whose status is not `starting`; `combined = Σ ppm × lanes`.

---

## 19. Progress cards (`.ocr-progress.json` entries) — WIRE

### 19.1 Opening a card (`begin_ocr_job`, `watcher.py:5841-5905`)

```json
{"started_at": <epoch>, "active": true, "updated_at": <epoch at write>,
 "generation": "<name>", "generation_id": "<id>", "processor": "<label>|null",
 "engine": "…", "detector": "…|null", "series": "<rel parent>", "volume": "<stem>",
 "relative_cbz": "<rel>", "percent": 0, "eta_seconds": null, "status": "starting",
 "session_ready": <bool>, "delivered": <bool>, "slot": <int>, "machine": "local|<name>",
 "total_pages": <int>?}
```
`started_at` is set when the card is first created (`watcher.py:1162-1171`).
Updates merge into the card (`_set_owned_progress` drops updates from a slot that
no longer owns the claim, `watcher.py:1173-1191`). The per-volume road's progress
callback adds `slot`; `status: "done"` clears the card; `"error"` leaves it
(`watcher.py:1199-1220`).

### 19.2 Session page progress (`_session_progress`, `watcher.py:5430-5483`)

`since_first = now − first_page_at` (0 if none); `estimate =
rates.rate_on(gen, rate_key, observed_pages=done, observed_seconds=since_first)`;
`(percent, eta_seconds, status) = _progress_metrics(done, total,
since_first, estimate.pps)` (`processor.py:995-1038`):

- `total ≤ 0` → `(None, None, "starting" if done ≤ 0 else "running")`;
- `done ≥ total` → `(100, 0, "finalizing")`;
- `done ≤ 0` → `(0, None, "starting")`;
- else `percent = min(99, int(100·done/total))`; `pps = rate or emission_rate(done, elapsed)`;
  `eta = int((total−done)/pps)` if pps else None; `"running"`.

Card fields written: `percent, eta_seconds, done_pages, total_pages (or null),
status, generation_id, slot, first_page_at, session_started_at (clock.started_at),
session_ready, delivered, rate_pages_per_second (4dp), latency_seconds (2dp),
rate_source`. Plus `pipeline` and `host_busy` from `stats` events (§9.6).

The queue page re-derives ETAs from these raw facts on every build (§20.2); the
card's own `eta_seconds` is used only as a hint by EFT lanes.

---

## 20. Queue plan (the lane simulation) — KEEP exactly

### 20.1 Inputs (`OCRWorker.queue_plan`, `watcher.py:1819-1858`)

- `running` = running cards (§23.4 shape) with `total_pages` filled from
  `_known_pages(library/<series>/<volume>.cbz)` when absent (`watcher.py:1884-1904`);
- `pending` = the scheduler's list (§15.3) minus running identities
  (`plan_items`, `watcher.py:1965-1977`);
- `lane_machines` = `["local"] × len(local slots)` + `[name] × max(1, max_sessions)`
  for each connected processor, in that order (`watcher.py:4225-4236`);
  `lane_count = max(1, len)`;
- `rate_for`, `startup_for` from §8.5; `startup_every_volume(gen, machine)` =
  local machine and a local slot exists and the row is not a session row (mokuro
  CLI) — **DROP** in Rust (always False) (`watcher.py:2057-2074`);
- `refusal_for(gen, machine)` = `_machine_precision_refusal` memoised;
  `hold_for(gen)` = `hold_reason(row.precision)` (`watcher.py:1860-1882`).

### 20.2 Running jobs → lanes (`lanes_from_running`, `eta.py:902-993`)

- Lanes `[Lane(machine=m) for m in lane_machines]` (or `lane_count` anonymous).
- Group running cards by `slot` (int) else `"job-<index>"`; process groups sorted
  by `(0, f"{slot:09d}")` for ints, `(1, key)` otherwise.
- Each group takes the first free lane of the card's `machine` (else the first
  free lane; else a **new** lane is appended).
- Within a group, cards are priced in order with a running `cursor`
  (`offset`): `finish = _price_running(card, offset=cursor, charge_startup=(index==0))`;
  an unpriceable card makes every later card of the lane
  `reason: "waiting behind a volume that cannot be timed"`.
- `lane.generation_id` = last card's row; `lane.free_in = cursor` or `+inf` if
  unknown; `used = True`.

`_price_running(card, …)` (`eta.py:1011-1099`) sets `rate_pages_per_second`,
`latency_seconds`, `rate_source` from `rate_for(gen, machine,
observed_pages=done, observed_seconds=now−first_page_at)`, then:

- status `error`/`done` → `eta_at=None`, return `offset` (holds no time);
- `done ≤ 0` → `status="starting"`; `spent = now − (session_started_at or
  started_at)`; charge `left = max(0, startup.seconds − spent)` only if
  `charge_startup and session_ready is not True`;
  `startup_seconds = round(left)` if `left ≥ 1` else None; `startup_rough`;
  no estimate or no total → ETA None, return None; else
  `finish = offset + left + estimate.volume_seconds(total)` (fill **and** pages);
- `done ≥ total` → `status="finalizing"`, `percent=100`, `eta_seconds=0`,
  `eta_at = now+offset`, return offset;
- else `status="running"`; `finish = offset + seconds_for(total − done)` (no
  latency once a page is out).
- `eta_seconds = int(round(finish))`, `eta_at = iso_utc(now + finish)`
  (`YYYY-MM-DDTHH:MM:SSZ`, `eta.py:135-143`).

### 20.3 Pending walk (`plan_queue`, `eta.py:1102-1266`)

```
median = round(median(positive pending pages)) or None
blocked_by = None
for i, item in pending:                       # in the scheduler's order
   if through is not None and i > through: break
   pages = item.pages or (median, rough=True)
   if blocked_by: no_prediction("waiting behind a volume that cannot be timed: " + blocked_by); continue
   open = [lane for lane in lanes if lane.machine is None or refusal_for(g, lane.machine) is None]
   if not open: no_prediction(hold_for(g) or "no connected machine can run <name>"); held=True; continue
   lane = earliest_finish_lane(open, g, pages) or min(open, key=free_in)
   est = rate_for(g, lane.machine)
   if est is None: blocked_by = "nothing has measured how fast <name> reads a page yet"; …; continue
   if pages is None: blocked_by = "no volume in the queue has a known page count"; …; continue
   if lane.free_in is inf: blocked_by = "a volume already running has not said how long it is yet"; …; continue
   start = lane.free_in + (startup if every_volume or lane.generation_id != g else 0)
   finish = start + est.volume_seconds(pages)
   lane.generation_id, lane.free_in, lane.used = g, finish, True
   item: eta_seconds=round(finish), eta_at, rate_source, latency_seconds (2dp), reason=None, pages, rough
done_in = max(lane.free_in for used lanes) unless truncated (through < len-1),
          blocked, no used lane, or any non-finite → done_at = None
```
`_earliest_finish_lane` (`eta.py:1274-1307`): None if pages None, or any lane has
infinite `free_in`, or any lane's machine has no rate; else the lane minimising
`free_in + startup(if row differs or every-volume) + volume_seconds(pages)`
(first minimum wins). `_no_prediction` sets `eta_seconds/eta_at/rate_source =
None`, `latency_seconds` default None, `reason` (`eta.py:1467-1472`). A held item
**does not** block the rest.

`QueuePlan(running, pending, done_in, done_at)` (`eta.py:892-899`).

### 20.4 `volume_plan` (`watcher.py:1906-1950`)

Prices one volume's owed jobs: with no pending list, adds this volume's
runnable, non-running owed rows that have no failure record as extra items. Runs
`plan_items`, finds the indices of this volume's items; if the last one is
≥ `max_items`, it is left unpriced (no walk); else `queue_plan(running, items,
through=last_index)`. Returns `plan.running + plan.pending`.

### 20.5 Per-volume outlook (`ocr/volume_outlook.py`) — WIRE (manifest + PUT)

`pending_entries(owed, series, volume, planned)` → for each owed row (primary
first, then list order; `owed_generations`, `watcher.py:2094-2103`):
`{"kind": "ocr"|"layer", "id": row.name, "eta": <first non-null eta_at for that
(series, volume, gen_id)> | null}` (`volume_outlook.py:33-65`).

`recheck_after(pending, now)`: None if empty; if no parsable `eta` → 300; else
`clamp(ceil(min(eta) − now) + 10, 30, 3600)` (`volume_outlook.py:24-89`).

### 20.6 `OcrControl.volume_pending(cbz, series, volume, running, wait, max_items)` (`control.py:215-257`)

None without a worker; `[]` if nothing owed; if `queue_hold()` is not None, no
pricing (all ETAs null); else `volume_plan(..., pending_within(wait))`
(exceptions → unpriced). Callers: PUT (wait 0.005 s, max 300 items), volume
manifest (wait 1.0 s, unbounded) (`catalog/api.py:32`, `237-252`).

---

## 21. Pipeline congestion readout — KEEP the verdict algorithm

### 21.1 Raw snapshot (runner `PipelineReport.as_dict`, `engine_runner.py:3387-3486`)

```json
{"elapsed_seconds": 12.345, "items": 40,
 "stages": [{"key","name","device","workers","items","busy_seconds",
             "blocked_seconds","starved_seconds","utilisation","device_bound"}],
 "queues": [{"name":"detect->engine","capacity","depth","max_depth","mean_depth",
             "depth_seconds","puts","gets","blocked_seconds","blocked_events",
             "starved_seconds","starved_events","fill"}],
 "bottleneck": "<stage key of max utilisation among stages with items>|null"}
```
Queues are named `<producer>-><consumer>`. A volume's window = report now minus
the report when its first page entered (stages/queues must line up, else the
whole report) (`engine_runner.py:3406-3433`, `7863-7875`).

### 21.2 `summarize(raw)` (`engine_runner.py:3526-3631`)

None unless `raw` is a dict with a non-empty `stages` list. Per stage:
`workers=int`, `fused = workers ≤ 0`, `pool_seconds = max(workers,1) × elapsed`,
`busy_pct = pct(busy_seconds)`, `blocked_pct`/`starved_pct = None if fused else
pct(…)` where `pct = round(clamp(100·s/pool_seconds, 0, 100), 1)` (0 if
`pool_seconds ≤ 0`), `queue` = the queue whose name starts with `"<key>->"`, shaped
`{name, capacity, mean_depth (2dp), max_depth, fill_pct (1dp)}`. Summary:
`{elapsed_seconds (1dp), items, stages, bottleneck (kept only if it names a
shown stage), verdict}`.

### 21.3 `pipeline_verdict(summary)` (`engine_runner.py:3634-3819`)

Constants `MIN_PAGES=8`, `WAIT_PCT=15`, `BUSY_PCT=85`.

1. No stage has a `queue` → None (serial fallback).
2. Segments: fold each fused stage into the previous segment (key
   `a+b`, busy summed capped 100, `device_bound` OR, items max). If no segments or
   `min(items) < 8` → None.
3. Starved candidate (walk from source): first `i ≥ 1` with
   `starved[i] ≥ 15` and `starved[i−1] < 15` and feeder not device-bound →
   `(starved, "<k> starved N% waiting on <f> — widen <f>", f)`.
4. Blocked candidate (walk from sink): first `i` from `len−2` down with
   `blocked[i] ≥ 15` and `blocked[i+1] < 15` and drain not device-bound →
   `(blocked, "<k> blocked N% waiting on <d> — widen <d>", d)`.
5. If any candidate → the one with the larger percentage (sentence, target).
6. Else busiest segment; `< 85` → None; device-bound →
   `"<k> busy N% on the <device> — it sets the pace and cannot be widened"`
   (target None); else `"<k> busy N% of W worker(s) — widen <k>"`.

The em dash and wording are shown verbatim in the UI and logs (KEEP strings).
`widen_target` returns the stage key (benchmark search).

### 21.4 Live and durable readouts

- Session: `stats.pipeline` → `summarize_event_stats` (accepts raw counters or an
  already summarised object with `busy_pct`, re-deriving a missing verdict)
  (`congestion.py:86-116`) → card `pipeline`.
- Per-volume road: `pipeline.json` at `<ws>/_detect/<row.id>/pipeline.json`,
  ignored when older than 30 s (`pipeline_stats.py:81-119`) — **DROP** with that road.
- `build_record(summary, volume, at, volume_pages, volume_seconds, volume_first)`
  (`congestion.py:119-194`): `{at, volume, pages: items, elapsed, verdict,
  bottleneck, stages: [{key,name,device,workers,fused,items,busy_pct,starved_pct,
  blocked_pct}], queues: [{name,capacity,mean_depth,max_depth}],
  volume_pages?, volume_seconds?, volume_first?: true}`.
- `CongestionHistory.record(gen_id, record, known_ids)` keeps the last 5 per row
  and prunes unknown rows in the same write (`congestion.py:241-262`).
- `average_runs(runs)` (`congestion.py:277-394`) → admin "Congestion" column:
  `{runs, last_run_at (iso), verdict, bottleneck, stages:[{key, workers,
  busy_pct, starved_pct, blocked_pct}] (means rounded to int), queues:[{name,
  capacity (int mean), mean_depth (2dp), max_depth (int mean)}]}`; verdict =
  `pipeline_verdict` over the averaged rows (each row's outbound queue matched by
  `"<key>->"` prefix); bottleneck = busiest non-fused stage with items.
  None for no usable runs.

Only **completed, local, uncontended** volumes enter the local history; remote
volumes go to that processor's profile (`runs.congestion`, last 5).

## 22. Utilisation sampler (benchmarks only, `ocr/utilization.py`)

1 Hz thread sampling GPU busy % (sysfs `card*/device/gpu_busy_percent` numeric
card order, else `nvidia-smi --query-gpu=utilization.gpu`, else None) and CPU busy
% (`/proc/stat` aggregate deltas, iowait counted idle); max 4096 samples;
`means(first, last)` = mean of samples inside the trial window, 1dp, None if none.
Device chosen from the row's engine → mokuro → detect pin (`first_gpu_device`).
Not used by the scheduler; port with the benchmark subsystem. **Rust**: sysfs
read is fine; consider NVML instead of shelling out.

---

## 23. Queue status page API (`queue/api.py`, `queue/shape.py`) — WIRE

### 23.1 Routes (`queue/api.py:148-173`)

| Method + path | Answer |
|---|---|
| `GET /queue`, `/queue/` | `queue/web/index.html` |
| `GET /queue/api/config` | `{"show_in_nav": bool, "public_access": bool, "display": level}` (no auth) |
| `GET /queue/api/status` | shaped status (§23.5); 401 `{"error":"Authentication required"}` when `queue.public_access` is false and the viewer has no role |
| `GET /queue/<file>` | static file from `queue/web/` (`..` or outside dir → 404 text); MIME by suffix (`.html/.js/.css`, else octet-stream); `Cache-Control: no-cache` |
| anything else | passed down the WSGI chain |

`queue.show_in_nav` default False, `queue.public_access` default True,
`queue.display` ∈ `minimal|normal|detailed` default `normal`, read live
(`queue/api.py:136-146`, `256-259`, `shape.py:42-75`).

### 23.2 Viewer (`queue/api.py:177-250`)

No DB or no `Authorization` → visitor. Bearer → `authenticate_bearer` every time
(never cached); failure → visitor with `failed`. Basic → cache keyed by
HMAC-SHA256(per-process random key, header) for 30 s on success / 60 s on
failure, invalidated when `database.users_version` changes; 256 entries (expired
evicted, else cleared). Unparsable → `failed`. Rate limiter
`AUTH_RATE_LIMITER` keyed `<client ip>:<username>`: refused → `limited` (not
recorded as failure); bad password → record failure + cache; good → record
success. `failed`/`limited` viewers get the visitor body plus header
`X-Queue-Auth: failed|limited`.

### 23.3 Caching, ETag, 304 (`queue/api.py:275-454`)

- Background refresh, single-flight, at most every 2 s (`REFRESH_SECONDS`);
  first poll of the process runs it inline. It calls
  `control.refresh_pending()`, takes the library snapshot (pending thumbnails and
  OCR counts → sha256 signature), and re-reads `skipped_missing_pages` when 10 s
  old (`SKIPPED_TTL_SECONDS`) but never sooner than 8× its last read duration
  (`SKIPPED_COST_FACTOR`); `invalidate_skipped()` forces it.
- Passive fingerprint (no walking): `(mtime_ns,size)` of `.ocr-progress.json` and
  `.ocr-failures.json`, snapshot signature, skipped signature, sorted processor
  ids, `processing_hold` JSON, `paused_for_benchmark` JSON, display level. A change
  bumps the shared page version.
- Bodies cached per `(level, admin)` against the version; builds single-flight per
  slot; while a build runs, others are served the previous body (or wait ≤ 30 s
  if none). Body = `json.dumps(payload, sort_keys=True)`; ETag
  `"<level>-<a|v>-<sha256(body)[:20]>"`; headers `ETag`, `Cache-Control: private,
  no-cache`, `Vary: Authorization`; `If-None-Match` (comma list) match → 304, no
  body.

### 23.4 Raw status (`raw_status`, `queue/api.py:456-552`) — internal, input to shaping

```
current            = first running card | null
current_jobs       = running cards (priced by the plan)       // _progress_entry shape
pending_ocr        = scheduler's pending list (priced)        // §15.3 + plan fields
queue_done_at      = plan.done_at | null
pending_thumbnails = snapshot count
failed             = [{series, volume, generation, engine, detector, error,
                       attempts (default 1), last_attempt_at, log_file}] sorted by (series, volume)
backend            = OCR backend string
generations        = [{id, name, engine, detector}] enabled rows in order
skipped_missing_pages = [{series, volume, missing_pages, generations:[names]}]
paused_for_benchmark  = bench.paused_for_benchmark() | null
processing_hold    = §5.2 | null
held_rows          = [{generation, reason}]
connected_machines = §25.2
speed              = §18.8
```
Running-card shape (`_progress_entry`, `queue/api.py:588-651`): `series, volume,
generation, engine, detector, percent (default 0), eta_seconds, done_pages
(default 0), total_pages, status (default "running"), generation_id, slot,
first_page_at, session_started_at, started_at, session_ready, delivered,
eta_at: null, rate_pages_per_second, latency_seconds, rate_source, processor,
machine`, plus `pipeline` when it has stages. `read_running_jobs` returns `[]`
unless the file's top level has `active` truthy; without `jobs` it is one job
(`queue/api.py:784-802`).

Without an OCR worker the pending list is synthesised from the library snapshot
(every enabled row each `.cbz` lacks, minus failed/running by `(series, volume,
generation name)`, ordered by `order_jobs`), every item with
`reason: "this server is not running OCR"` and null ETAs (`queue/api.py:665-732`).

### 23.5 Shaping per level (`shape_status`, `shape.py:490-629`)

Exposure rule: visitors never get raw errors, log paths, processor names/labels,
bench keys for drafts, or the backend. Machine names for visitors come from
`PublicNames`: `"this server"` for local, the processor's explicit `public_name`,
else `"machine N"` numbered in first-seen order for the process
(`shape.py:99-133`). Admins get real names (`display_name`: `"this server"` for
local).

Machines (`group_by_machine`, `shape.py:196-268`): connected machines first in
lane order (with `slots`, `held`, `held_error`, `held_until`, `configuring`,
`standby`, `cannot_start`), then machines only seen on cards. Cards grouped by
`(machine, slot)`; within a lane, sort by (has pages out first, then
`started_at`): the first is **active**, the rest **next** (on deck).
`slots = max(slots, len(active))`. State = best of active job states by rank
running < loading < waiting < idle; with no active job: `configuring` (a mapping)
> `held` > `standby` > `idle`.

`job_state(card)` (`shape.py:171-193`): status ≠ `starting` → `running`;
`session_ready is False` → `loading`; `True` → `waiting` if `delivered is False`
else `running`; missing flag → `loading` if `startup_seconds > 0` else `waiting`.

Common fields: `level, queue_done_at, pending_count (len of full list),
processing_hold {reason, since, last?{name (mapped), disconnected_at}},
paused_for_benchmark {queued, processor (mapped unless null/""/"local"), key &
generation (admin) | generation (visitor: only for saved rows)}`, admin-only
`held_rows`.

- `minimal`: `machines[{name, state, slots, jobs:[{series, volume, generation,
  percent, eta_at, state}], next:[{series, volume, generation}],
  configuring?}]`, `pending` = first 10 items `{series, volume, generation, eta_at,
  rough}`.
- `normal`: jobs add `status, done_pages, total_pages, eta_seconds,
  startup_seconds, host_busy`; machine adds `held {reason: "downloads failing",
  until, error (admin)}`, `cannot_start [{generation, until, error (admin)}]`,
  `label` (admin); `pending` = first 100 (`QUEUE_REPORT_LIMIT`) with `{series,
  volume, generation, eta_at, rough, attempts (default 0), reason, returned}`
  (`returned`: `{machine (mapped), reason: "download failed — will retry", at,
  class/error/count (admin)}`); `pending_thumbnails`; `failed[{series, volume,
  generation, attempts (≥1), reason: category, error/log_file/last_attempt_at
  (admin)}]`, `failed_count`; `skipped_missing_pages[{series, volume,
  missing_pages, page_count, generations}]`; `generations[{id, name}]`;
  `backend` (admin).
- `detailed`: jobs add `engine, detector, latency_seconds, startup_rough,
  pipeline {verdict, bottleneck, stages[{key,name,device,workers,fused,busy_pct,
  blocked_pct,starved_pct, queue{name,capacity,mean_depth,max_depth}|null}]}`,
  and `throughput_pages_per_minute` (that machine's real rate on that row);
  pending adds `engine, detector, pages, rate_source, latency_seconds`; payload
  adds `speed: [{generation, pages_per_minute (combined), machines (count
  working)}]`.

Never sent at any level: `rate_pages_per_second` (a fitted slope is not a speed).

`failure_reason(error)` (`shape.py:78-89`) on the lower-cased text, first match:
starts with `download failed` → "download failed — will retry"; contains any of
`zip, truncated, missing page, pages short, incomplete, corrupt, no images,
invalid image, cannot identify image, archive` → "archive incomplete"; any of
`disconnect, connection, timed out, timeout, went away, lost, closed before,
stopping` → "interrupted — will retry"; any of `out of memory, oom, cuda, hip,
rocm, exited, exit code, traceback, engine, runner, killed, signal, error,
exception` → "engine error"; else "failed — will retry".

---

## 24. Reader queue file `/mokuro-reader/.mokuro-queue.json` — WIRE (`middleware/queue_file.py`)

- Path `/<READER_ROOT>/.mokuro-queue.json`; `GET`/`HEAD` only; `OPTIONS` passes
  through (CORS); any other method, or `MOVE`/`COPY` whose Destination is this
  path → `405` with `Allow: GET, HEAD, OPTIONS` and a plain-text body; never
  listed by PROPFIND; read authorisation = same gate as a library file
  (`gate_read`) (`queue_file.py:161-195`).
- Rebuilt at most once per second (`REBUILD_SECONDS`). Document:
  ```json
  {"version": 1, "generated_at": "YYYY-MM-DDTHH:MM:SSZ",
   "held": {"reason": "no-processor|benchmarking|paused"} | null,
   "next_check_after": <recheck_after(all jobs, now)> | null,
   "pending_volumes": <count of waiting volumes in the whole queue>,
   "volumes": [{"series","volume","path": <reader file url of .cbz>,
                "manifest": <manifest url>,
                "jobs": [{"kind":"ocr|layer","id":"<row name>",
                          "state":"running|queued|held","eta":"…Z|null",
                          "progress": 0..1 (3dp) | null}]}]}
  ```
  (`queue_file.py:64-95`).
- ETA hysteresis: a job keeps its previously published `eta` when the new one is
  within 60 s (`ETA_HYSTERESIS_SECONDS`) (`queue_file.py:111-124`). If
  `version`, `held`, `volumes` all equal the previous document, the previous body
  and ETag are served again (so `generated_at`/`next_check_after` do not churn
  the ETag).
- ETag = `"<sha256(json(doc without generated_at, sort_keys, compact))[:32]>"`;
  gzip (level 6) when `Accept-Encoding` contains gzip, ETag suffixed `-gz`
  inside the quotes; `Cache-Control: no-cache`, `Vary: Accept-Encoding`;
  `If-None-Match` → 304.
- Content (`OcrControl.queue_document`, `control.py:313-412`): `running =
  _with_known_totals(read_running_jobs)`; `pending = pending_within(1.0) or []`;
  plan over `plan_items(pending, running)` (exceptions → unpriced). Volumes in
  order of first appearance (running first, then pending). For each listed volume:
  rows = owed rows (from `owed_by_volume`, the shared walk) ∪ rows of listed jobs;
  sorted (primary first, then rank). State: running if a running card matches;
  `held` if unpriced (not in the list, e.g. backoff), whole queue held, or the item
  is `held`; else `queued`. `eta = eta_at` unless held. `progress = done/total`
  for running. All running volumes are listed, waiting volumes only the first
  100; `pending_volumes` counts all waiting ones.
- `queue_hold()` (`control.py:296-311`): `no-processor` if `processing_hold`;
  else if every machine held → `benchmarking` if a benchmark holds the queue else
  `paused`; else None.

---

## 25. Other `OcrControl` surfaces (`ocr/control.py`)

### 25.1 Thin delegations (None/[]/{} without a real worker)

`refresh_pending, start_backoffs(machine), connected_machines, speed_report,
skipped_missing_pages, paused_for_benchmark, bench_service (built lazily by
bench_factory), last_pending, pending_jobs, processors (registry entries),
processing_hold, queue_plan, archive_arrived, archive_removed, held_rows,
autobench_failed ("local" for this server), precision_holds, generation_order`
(`control.py:104-311`, `414-417`).

### 25.2 `connected_machines()` (`watcher.py:4238-4298`) — WIRE (raw status)

`[{machine, slots, standby?: true, held?: "downloads", held_until?, held_error?,
cannot_start?: [{generation, until, failures, error}], configuring?: {…bench line}}]`
in lane order. `standby` = some running slot of that machine is
`waiting_for_faster` with no job and the machine is not held.

---

## 26. Concurrency model (as implemented)

- Threads: OCR loop (scan thread = slot 0 when no registry); one thread per extra
  slot; with a registry a supervisor plus one thread per slot; one stdout reader
  thread per local session; thumbnail loop; request threads (queue page, upload,
  manifests) that call read-only worker methods; bench line threads that call
  `preempt_for_bench`/`release_queue`; registry event threads that call
  `processor_disconnected`, `feed` remote events.
- One re-entrant `Condition` guards all worker state (`watcher.py:905`); code
  relies on re-entrancy in several places (`_forget_candidate` inside `_claim`,
  `_set_active_progress` inside `_set_owned_progress`, `_breaker_open`).
  Expensive work (walks, profile reads, pricing) is done **outside** the lock and
  re-validated inside. Lock order: worker lock is never held while calling
  `BenchService.enqueue` (which re-enters via `preempt_for_bench`) — hence the
  "record now, fire later" `_autobench_wanted` list (`watcher.py:3970-3983`).
- Additional locks: `_candidate_walk_lock` (single-flight walk),
  `_queue_compute_lock` (single-flight pending list), `RateModel._lock`,
  processor `_process_lock` (cancel vs start), session `_lock`, remote
  `entry.lock` (never taken while holding a RemoteSession `_lock`,
  `remote/session.py:66-72`), staging `_stage_lock`.

---

## 27. Python-only complexity and the in-process Rust/ONNX simplification map

| Python mechanism | Why it exists | Rust recommendation |
|---|---|---|
| Runner as a **subprocess** in a second venv; `engine_runner.py` staged to disk by content hash; held/pruned staged builds (`staging.py`, `processor.py:1639-1666`, `session.py:228-232`) | torch/transformers version split between mokuro and the engines; runner must run without `mokuro_bunko` installed | DROP. Engines are in-process ONNX sessions; a "session" is a pipeline object. Replace runner build hash with the binary build id in provenance |
| JSON-lines stdio protocol, fd-1 seizure, garbage-line counting, stderr file + tail, reader thread, `exit` synthesis (`session.py`, `engine_runner.py:7554-7613`) | process boundary; libraries printing to fd 1 | Internal channel of a typed `RunnerEvent` enum; keep the **same event set and payload fields** for remote processors (§17) |
| `serve_module_available` probe, `runs_mokuro_cli`, mokuro CLI road, `--mokuro-python`, log scraping (`Processed successfully`, traceback regexes), `_count_ocr_json_files` progress, `.mokuro.gz` collection, `startup_every_volume` | mokuro | DROP |
| Per-volume subprocess road (`process_library_ocr`, `_run_ocr_subprocess`, hard/no-progress/finalizing timeouts, `pipeline.json` polling) | `ocr.sessions: false` fallback + mokuro | DROP (a one-volume session is the same code). Keep a per-volume stall watchdog equivalent to the 600 s wedge |
| One `OCRProcessor` per slot because `_active_process`, `last_failure`, `last_pipeline`, `last_written`, `publish_guard`, `cancel_check` are single-valued (`watcher.py:371-378`, `processor.py:247-314`) | Python objects as mutable per-job context | Pass a per-job context/outcome value; cancellation = a cancel token per job/session |
| `MOKURO_OCR_JOBS` env so each runner budgets `(cores−1)/jobs/4` | runners cannot see siblings | A single process-wide thread budget / executor shared by all sessions |
| `os.nice(10)` / `BELOW_NORMAL_PRIORITY_CLASS` on backlog sessions | separate processes | Per-thread nice on Linux (`setpriority` on TID) for backlog pipelines, or priority-aware work stealing; keep "head row normal, others lowered" policy (open question for Windows/macOS) |
| `OMP_NUM_THREADS=1`, torch thread caps, HF offline env | torch/HF | DROP; configure ORT intra/inter-op threads |
| Detector adapters as subprocesses (ctd GPL boundary), detector page timeout, respawn ≤ 2 | licence boundary + torch | DROP (ctd/animetext/rtdetr are DROP); ONNX detectors in-process |
| `_walked_candidates` 8× cache, `_queue_max_age` 4× stretch, `pending_within` thread+join, background queue refresher, skipped-list pacing | library walks on network shares were starving request threads of the GIL | Keep a cached library index updated by WebDAV hooks + fs watcher, single-flight recompute, async timeouts; numeric factors may stay as heuristics |
| `.ocr-progress.json` as IPC from worker to request threads (catalog `/api/ocr-status`, queue page) | historically separate concerns; file atomically rewritten several times/s | Shared in-memory state (Arc<RwLock>); optionally persist for external tools. Queue page fingerprint then becomes a version counter only |
| `.ocr-failures.json` re-read on every claim and page build | simplicity | In-memory map + atomic write-through; keep file format for `doctor`/compat |
| `read_own_copy` on a helper thread with join timeout | blocking IO without async | `spawn_blocking` + timeout |
| Re-entrant `Condition` and "outside-lock walk, inside-lock recheck" | GIL + blocking IO | Non-reentrant `Mutex` + `Condvar` (or async `Notify`); restructure the re-entrant call sites; keep the recheck-under-lock pattern |
| `_SessionClock` / `first_of_session` / opening-volume exclusion | pipeline fill after a model load | KEEP (still true in-process: the first volume after a pipeline opens contains fill) |
| Two kinds of runner start failure (spawn vs not-ready) and `_StartBackoff` signatures | broken venvs, missing CUDA libs | Keep the backoff (missing model files, ORT provider failures still happen) but the signature can be "row as run + machine id" |
| `_devices_in_use` device spread across sessions | one torch model per GPU context | KEEP (ORT sessions on two GPUs) |
| Benchmarks via `--bench` subprocess, precision trials re-casting torch weights | torch | Out of scope; precision candidates for ONNX models will differ (fp32/fp16 model files) |

---

## 28. Places where docs and code disagree (as of 0.5.2)

1. `docs/ocr-internals.md:56-57` says "two jobs never run on the same volume at
   once"; the code deliberately allows a volume's generations to run
   concurrently on different slots (`watcher.py:2495-2501`,
   `processor.py:528-545`; test `test_ocr_concurrency.py::test_two_generations_of_one_volume_run_together`).
   The code is authoritative; the uuid stamping (§11.4) makes it safe.
2. `docs/ocr-internals.md:280-283` says `volume_done.stats` windows tile the
   session; the runner measures from when the volume's first page *entered* the
   pipeline, so windows overlap (`engine_runner.py:7863-7875`). `seconds` (not
   `stats`) is the partitioning quantity (`engine_runner.py:7999-8015`).
3. `docs/ocr-internals.md:104-105` says the primary row runs on an inbox upload
   before the archive moves into the library; the inbox watcher is never started
   by the server (`OCRWorker.watcher` unassigned, `watcher.py:786`; `server.py`
   never constructs `InboxWatcher`).
4. `congestion.py:30-33` says the adapter road's detector runs as a whole-volume
   subprocess before the pipeline; the detector is now a streaming stage
   (`docs/ocr-internals.md:185-204`). Stale docstring only.
5. `ocr-internals.md:82-87` describes the backoff as `poll_interval × 4^(attempts−1)`
   capped at an hour — matches code, but note the exponent is also clamped at 16
   (`watcher.py:1396-1399`).

---

## 29. Behavioural tests that pin this spec (port them)

`tests/unit/test_ocr_job_order.py` (30, natural sort + round robin),
`test_ocr_queue_order.py`, `test_ocr_failures.py` (18, keys/backoff/replace),
`test_ocr_concurrency.py` (41, slots/claims/priority/progress shape),
`test_ocr_sessions.py` (44, session loop, lookahead, crash blame, strikes, wedge,
pre-emption, device spread, holds/pre-empt), `test_warm_session_first.py` (6),
`test_eft_assign.py` (18, `earliest_finish_claim`), `test_eft_claim.py` (19,
deadlines/standby/restart), `test_ocr_eta.py` (94, rate model + plan),
`test_ocr_congestion.py`, `test_pipeline_stats.py`, `test_ocr_live_settings.py`
(15), `test_ocr_missing_pages_skip.py`, `test_queue_api.py`,
`test_queue_display.py`, `test_queue_file.py` (19), `test_queue_report_limit.py`,
`test_queue_skipped_pacing.py`, `test_session_ready_state.py`,
`test_upload_enqueue.py`, `test_remote_scheduler.py`, `test_processor_speed.py`,
`tests/web/test_queue_eta.py`, `test_queue_levels.py`, `test_queue_reflow.py`.

---

## 30. Open questions

1. **Per-volume road**: drop `ocr.sessions: false` and the one-subprocess-per-volume
   path entirely (recommended), or keep a flag that runs a one-volume session?
2. **Inbox**: the inbox OCR path is dead in 0.5.2 (§28.3) yet `/inbox/` PUTs are
   accepted. Remove inbox processing (and the `/inbox` WebDAV root?) or
   re-implement "primary row on upload, then move into the library"?
3. **Progress/failure files as IPC**: may the Rust port make `.ocr-progress.json`
   in-memory only? Does any external tool (doctor, health check, other
   processes) read it? `.ocr-heartbeat` is read by the health endpoint — keep?
4. **OS priority** of backlog rows in-process: is per-thread nice acceptable, and
   what about Windows/macOS? Does it matter once one process runs all rows?
5. **`RateModel.forget`** is never called by the worker (rename-safe ids mean
   removed rows' in-memory evidence just lingers until restart). Intentional?
   And `output_affecting` changes keep the old session evidence under the same id
   (only profiles/bench are recipe-checked) — should a recipe change reset rates?
6. **Local single-volume pricing** in `_rows_left_to_warm` uses `_page_count`
   (metadata cache only) while EFT uses `_known_pages` (cache or zip). Port the
   inconsistency or unify on `_known_pages`?
7. **`_eft_lanes` returns None if any lane's in-flight work is unpriceable**, which
   silently disables EFT for every lane (first-come). Keep, or price unknown lanes
   at the median?
8. **Remote protocol compatibility**: will Rust 0.7 processors still speak
   protocol 2 with the same runner event vocabulary (§17.2), or is a new protocol
   version planned? This decides whether `RunnerEvent` must be serde-identical.
9. **Precision modes for ONNX**: the PRECISION_POLICY/bench-pick machinery is
   torch-shaped (bf16 re-cast). Which modes survive, and does `autobench_kind ==
   "precision"` still exist?
10. **Device spread / `_reachable_placement`**: with every stage ONNX, should the
    ORT-provider fallback apply to all model stages (it currently applies only to
    `ORT_GPU_DETECTORS`)?
11. **Failure key separator**: keys use OS path separators (`str(relative_to)`),
    so a Windows server writes `Series\Vol.cbz`. Normalise to POSIX in Rust
    (migration of existing files needed) or keep?
12. **`_get_unique_path`** produces `Vol.mokuro_1.gz`-style names for gz and
    `Vol_1.mokuro` for plain sidecars; readers presumably ignore both. Keep the
    exact naming or switch to a clearer collision policy?
13. **Thumbnails loop** shares `poll_interval` and the worker object; does it
    belong in this subsystem for the port or with metadata?
14. **Hard-coded thresholds** (EFT 10 %/5 s/2 s/15 s grace, wedge 600 s, lookahead
    2, crash limit 2, busy-host 0.6/0.5, breaker 3/600 s/3600 s, return limit 3):
    keep verbatim (recommended, tests pin them) or make configurable?

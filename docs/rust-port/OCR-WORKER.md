# OCR worker design (bunko-server `ocr/`)

This is the 0.7 replacement for 0.5.2's `ocr/watcher.py`, `ocr/processor.py`,
`ocr/session.py`, `ocr/control.py`, `ocr/remote/*` and `queue/api.py`. The behaviour to
keep is in `spec/ocr-scheduling.md`, `spec/remote-processors.md` and
`spec/ocr-generations-bench.md`. The pure math is already in `bunko-sched`; the wire types
are in `bunko-proto`.

## What changes structurally

| 0.5.2 | 0.7 |
|---|---|
| One `threading.Condition` around ~40 fields, many threads (scan, slots, supervisor, readers) | One **scheduler actor**: a dedicated OS thread (`ocr-scheduler`) that owns all mutable scheduling state and handles its inbox one message at a time, plus a 1 s tick. Callers send a message and await a oneshot reply. No locks around scheduling state. |
| Local runner subprocesses speaking JSON over stdio; remote sessions speaking v2 frames | Every machine is a **processor link** that sends `bunko_proto::Op` and receives `Event`. Local OCR uses `bunko_processor::LocalProcessor` over channels; remote uses the v3 WebSocket. |
| Per-volume subprocess road (`ocr.sessions: false`) | Removed. `ocr.sessions` is accepted and ignored. |
| Shared walk cache with an 8× heuristic | An in-memory **owed index**: a full re-walk every `poll_interval`, kept current in between by WebDAV arrival/removal hooks and the library watcher. `still_owed` is re-checked at every claim. |
| `.ocr-progress.json` as IPC to the queue API | Progress cards live in memory. The queue API asks the actor for a snapshot. The file is no longer written (no reader outside the process). |
| `.ocr-failures.json`, `.ocr-congestion.json`, `.ocr-bench.json`, `processors/*.json` | Kept, in the 0.5.2 formats, because they persist across restarts and `doctor` reads the failures file. bunko-sched has the stores for the three dot files; `ocr/profiles.rs` has the one for `processors/*.json`. |

## Components

```
ocr/
  mod.rs        Module root
  control.rs    OcrControl: the public handle (cheap clone), lite/full aware; local
                processor wiring, /api/health block, PUT follow-up, volume outlook
  actor.rs      Runs the Scheduler on its own OS thread (inbox + 1 s tick)
  sched/mod.rs  Scheduler state, the Msg enum, handle(); Lane records (one per slot of
                each connected machine)
  sched/        claim.rs, session.rs (open, feed, drain), settle.rs, registry.rs
                (registrations, the local machine, names, account stamps, lanes),
                bench.rs, status.rs, availability.rs (a processor's pause)
  profiles.rs   processors/*.json
  owed.rs       Owed index: walk, missing-pages rule, arrivals/removals
  api.rs        /_processor/register, /_processor/{pid}/socket, results PUT, bench sample
  collect.rs    Install a sidecar: validate, normalise, move, provenance, audit
  upgrade.rs    Generation upgrade: census, direct replace, swap
  queue_api.rs  /queue, /queue/api/{config,status} (ETag/304, display levels)
  shape.rs      What the queue page sends per display level and viewer
  queue_file.rs /mokuro-reader/.mokuro-queue.json (60 s ETA hysteresis, gzip variant)
  admin.rs      `impl admin::OcrAdmin for OcrControl`
  types.rs      Shared types (job, archive stamp, library facts)
  pyjson.rs     Python `json.dumps` spellings for the wire and state files
```

- Outlooks: `Scheduler::volume_pending` (`sched/status.rs`) lists a volume's pending
  rows; `OcrControl::put_follow_up` and `volume_outlook` turn that into PUT follow-up
  headers and manifest outlooks, with the `recheck_after` math in `bunko_sched::outlook`.
- Benchmarks: the queue, hold/pre-empt and stored results are in `sched/bench.rs`;
  autobench gating is in `sched/claim.rs`. The measurement runs on the processor
  (`bunko_processor::bench`).
- Health: `OcrControl::health()` builds the `ocr` block of /api/health
  (`{backend, worker_alive, pending, failed, queued_jobs}`) for the `accounts::HealthSource`
  trait. It leaves `pending` null; the handler in `accounts/home.rs` fills it from the
  library index.
- There is no link trait. The scheduler sends `bunko_proto::Op`s through an mpsc sender
  on `Machine.ops`. For this server's own processor, `control.rs` connects it to
  `bunko_processor::LocalLink`'s channels; for a remote one, the WebSocket task in
  `api.rs` (`run_socket`) carries it over protocol v3.

### Actor messages (illustrative)

- Claiming is not a message: the scheduler calls `Scheduler::claim(lane_id,
  Option<generation>)` → `(Option<Job>, preempt)` itself when it fills an idle lane or
  tops up a session (spec §7 phases A–C; the EFT walk is cheap at ≤ 256 jobs).
- `SessionEvent { lane, event }`: every `bunko_proto::Event` from a link.
- `ProcessorJoined/Left { entry, reason }`, `ArchiveArrived/Removed`, `ApplySettings`,
  `Hold/Release/PreemptForBench`, `Snapshot { reply }` (queue page and admin),
  `VolumePending { cbz, reply }` (PUT and manifest outlook), `Tick` (every 1 s; re-walks
  each poll interval).

There are no lane tasks. Lanes are passive records owned by the scheduler (id,
processor, open session, waiting/idle flags). The scheduler claims jobs, opens, feeds,
drains and settles sessions, and sends Ops over each processor's op channel. Each
processor's frames come back as `Msg::Event { pid, event }`, from the socket reader
(remote) or a forwarding task (local). The actor decides drains,
strikes, backoff, collection, failure records and cancellation. All 0.5.2 accounting
in §9.8 and §11.3 lives in one place.

### Local processing

- Lite build (no `ocr` feature): `ocr.local_processing` is treated as false. The queue
  holds with `processing_hold() = {reason: "no-processor", …}` until a remote processor
  connects.
- Full build: the binary passes a factory that creates `LocalProcessor`s (bunko-processor)
  wired to the real engines (bunko-ocr, bunko-vlm, bunko-layout). Local lanes number
  `ocr.concurrency`. The machine key is `local`; its profile is `processors/@local.json`.

### Remote processors

- `api.rs` implements protocol v3 (`PROTOCOL.md`). The registry keeps v2's rules:
  names, reserved names, 4 entries per account, account-stamp re-check every 15 s,
  ghost sockets, re-register replaces, and `drop` returning claims unrecorded.
- A registration contributes `max_sessions` lanes while connected.
- Result uploads stream to `<storage>/.processing/<sid>/<claim>/result.mokuro.part`.
  The sidecar name the client sends must be the one the library chose for the claim
  (else 409 `rejected`); it is never used as a path. Once the body matches
  `x-mokuro-sha256`, the file is renamed to `result.mokuro`. Collection then checks it
  against `volume_done.sidecar_sha256` like a local result.

### Collection (spec §11)

Everything is validated and written by the library, behind the per-path write lock
shared with WebDAV (bunko-dav exposes it). The scheduler first drops a late result whose
claim the session no longer owns, then hands the rest to a helper thread. The steps, in
order:

1. Archive-still-current check (else the result is discarded).
2. The result exists and matches the announced sha256.
3. JSON validity.
4. Normalisation: title, volume, uuids and `ocr_engine` stamping (bunko-layout has the
   pure function).
5. Unique-path move (an upgrade job swaps instead).
6. Provenance row (`ocr_sidecars`).
7. Audit.
8. Metadata recompile hook (`sidecar_installed`).

The machine profile (`processors/*.json`) is updated when `volume_done` arrives, even if
the install later fails; a remote run's congestion record goes there. A local, uncontended
run is added to `.ocr-congestion.json` only after a successful install.

### Generation upgrade (0.6 design, carried into 0.7)

`ocr.upgrade.enabled` and `replace` (bunko-core config) control it. The census, direct
replace and generate-then-swap are specified in `spec/generation-upgrade.md`, a revision
of the 0.6 design: the new file replaces the old one outright, no layer is kept, there is
no revert (the beta.3 endpoint answers 410), and a volume a person edited is skipped
unless an admin forces it. The main use is moving `mokuro-legacy` bare files to the new
primary.

## Memory (lite build)

- The owed index has one entry per owed volume, keyed by library-relative path: its owed
  (and withheld) row ids as `Arc<str>`, size, mtime, page count and the upgrade flag.
  Volumes that owe nothing are not stored.
- Page counts come from the metadata cache, or a zip central-directory read when
  missing; a walk reuses the previous count while the archive's size and mtime are
  unchanged. The missing-pages rule reads the archive's directory and sidecar when the
  cache has no current entry. The walk never reads page bytes; only benchmarks copy
  pages, to build their sample.
- Result uploads stream to disk.

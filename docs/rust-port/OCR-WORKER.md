# OCR worker design (bunko-server `ocr/`)

This is the 0.7 replacement for 0.5.2's `ocr/watcher.py`, `ocr/processor.py`,
`ocr/session.py`, `ocr/control.py`, `ocr/remote/*` and `queue/api.py`. The behaviour to
keep is in `spec/ocr-scheduling.md`, `spec/remote-processors.md` and
`spec/ocr-generations-bench.md`. The pure math is already in `bunko-sched`; the wire types
are in `bunko-proto`.

## What changes structurally

| 0.5.2 | 0.7 |
|---|---|
| One `threading.Condition` around ~40 fields, many threads (scan, slots, supervisor, readers) | One **scheduler actor**: a tokio task that owns all mutable state and processes messages one at a time. No locks around scheduling state. |
| Local runner subprocesses speaking JSON over stdio; remote sessions speaking v2 frames | Every machine is a **processor link** that sends `bunko_proto::Op` and receives `Event`. Local OCR uses `bunko_processor::LocalProcessor` over channels; remote uses the v3 WebSocket. |
| Per-volume subprocess road (`ocr.sessions: false`) | Removed. `ocr.sessions` is accepted and ignored. |
| Shared walk cache with an 8× heuristic | An in-memory **owed index**: a full re-walk every `poll_interval`, kept current in between by WebDAV arrival/removal hooks and the library watcher. `still_owed` is re-checked at every claim. |
| `.ocr-progress.json` as IPC to the queue API | Progress cards live in memory. The queue API asks the actor for a snapshot. The file is no longer written (no reader outside the process). |
| `.ocr-failures.json`, `.ocr-congestion.json`, `.ocr-bench.json`, `processors/*.json` | Kept, with the same formats (bunko-sched has the stores), because they persist across restarts and `doctor` reads the failures file. |

## Components

```
ocr/
  mod.rs        OcrControl: the public handle (cheap clone), lite/full aware
  actor.rs      Scheduler actor: state + message loop
  owed.rs       Owed index: walk, missing-pages rule, arrivals/removals
  lanes.rs      Lane tasks: claim -> open session -> feed -> settle (one per slot)
  link.rs       ProcessorLink trait; LocalLink (channels), RemoteLink (WebSocket)
  registry.rs   Remote registrations, names, profiles, account stamps
  api.rs        /_processor/register, /_processor/{pid}/socket, results PUT, bench sample
  collect.rs    Install a sidecar: validate, normalise, move, provenance, audit
  queue_api.rs  /queue, /queue/api/{config,status} (ETag/304, display levels)
  queue_file.rs /mokuro-reader/.mokuro-queue.json (60 s ETA hysteresis, gzip variant)
  outlook.rs    Per-volume pending + recheck_after for PUT responses and manifests
  bench.rs      Benchmarks and autobench (can come after the rest)
  admin.rs      `impl admin::OcrAdmin for OcrControl`
  health.rs     The `ocr` block of /api/health
```

### Actor messages (illustrative)

- `Claim { lane, generation: Option<id>, reply }` → `(Option<Job>, preempt)`
  (spec §7 phases A–C; the EFT walk runs inside the actor and is cheap at ≤ 256 jobs).
- `SessionEvent { lane, event }`: every `bunko_proto::Event` from a link.
- `ProcessorJoined/Left { entry, reason }`, `ArchiveArrived/Removed`, `ApplySettings`,
  `Hold/Release/PreemptForBench`, `Snapshot { reply }` (queue page and admin),
  `VolumePending { cbz, reply }` (PUT and manifest outlook), `Tick` (poll interval: re-walk
  and prune).

Lane tasks hold no scheduling state. Each one only loops: ask for a claim, drive its
session via its link, and forward events to the actor. The actor decides drains,
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
- Result uploads stream to `<storage>/.processing/<sid>/<claim>/<name>.part`. They are
  verified against `x-mokuro-sha256` and the `volume_done.sidecar_sha256`, then collected
  like a local result.

### Collection (spec §11)

Everything is validated and written by the library, behind the per-path write lock
shared with WebDAV (bunko-dav exposes it). The steps, in order:

1. Archive-still-current check.
2. Owner check.
3. JSON validity.
4. Normalisation: title, volume, uuids and `ocr_engine` stamping (bunko-layout has the
   pure function).
5. Unique-path move.
6. Provenance row (`ocr_sidecars`).
7. Audit.
8. Congestion and profile record.
9. Metadata recompile hook.

### Generation upgrade (0.6 design, carried into 0.7)

`ocr.upgrade.enabled` and `replace` (bunko-core config) control it. Implement the census,
direct replace, generate-then-swap and revert as specified in
`../mokuro-webdav-library-worktrees/feat-0.6/docs/superpowers/specs/2026-09-29-generation-upgrade-design.md`
(copied to `docs/rust-port/spec/generation-upgrade.md`). The main use is moving
`mokuro-legacy` bare files to the new primary.

## Memory (lite build)

- The owed index stores one entry per (volume, owed row) with interned series strings.
  About 100 bytes per entry, so 1 MB for a 10k-volume library.
- No archive is read by the scheduler except for page counts. Those come from the
  metadata cache, or a zip central-directory read when missing, memoised by
  (path, size, mtime) in a bounded map.
- Result uploads stream to disk.

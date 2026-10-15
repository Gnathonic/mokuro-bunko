# Library filesystem access in the OCR subsystem (bunko-server `ocr/**`, bunko-processor)

All paths below are under `crates/`. Bunko-server paths are relative to `bunko-server/src/ocr/`.

**Not library I/O, so excluded:**
- `queue_file.rs` (`.mokuro-queue.json`) is a virtual in-memory document built by `current()` (l.162) and gzipped. It has no disk I/O. `refusal()` (l.227) only blocks writes and MOVE/COPY onto it. It must stay a virtual path in the store layer.
- `.processing` lives under `storage`, not the library tree. Those calls are listed as "spool" and don't need `LibraryStore`.
- `bunko-processor` `spool.rs`, `fetch.rs` and `pipeline.rs` touch no library paths. `spool.rs` is a private memfd/disk spool, and `fetch.rs` is the HTTP GET with Range/If-Range/strong ETag (`fetch.rs:371,774-775,848-891`). The only direct-path code is `local.rs`, which delegates to `session.rs:223-233`.

## 1. Call sites

| file:line | fn | category | purpose | local-only assumption |
|---|---|---|---|---|
| owed.rs:132-146 | `list_archives` | walk | every `*.cbz` (case-insensitive via `is_cbz_name`; the doc comment saying "case-sensitive" is stale); follows symlinks (l.140) | cheap recursive readdir; symlinks as files |
| owed.rs:74-75 | `compute` | stat | size/mtime/is_file of the archive | cheap stat; mtime_ns precision |
| owed.rs:53 | `row_done` | stat | `plain.exists() \|\| gz.exists()` per enabled row | 2 stats per row per volume |
| owed.rs:194 | `still_owed` | stat | re-check at claim time | stat |
| owed.rs:201-203 (via 159,182; settle.rs:621) | `archive_pages` | zip-random-access | page count via `Volume::open` | seekable file; central directory read |
| types.rs:91 | `stamp_of` | stat | `(size, mtime_ns)` claim pin | mtime as version identity |
| types.rs:141 | `FileFacts::missing_pages` | stat + read-whole | sidecar meta + `compile_volume` (reads sidecar and archive) | files readable |
| types.rs:164-176 | `StoreFacts` | other | delegates to `bunko_library::compiler` (`missing_pages_now`, `cached_page_count`, DB-backed). Its internal fs reads are not in scope but are a dependency. | |
| collect.rs:267 / 270 | `install` step 1 | stat | `stamp_of(cbz)` equals the claim's stamp, else `cbz.is_file()` | mtime/size equality |
| collect.rs:275 | `install` | stat | `req.result.is_file()` (spool) | n/a |
| collect.rs:101 | `sha256_file` | read-whole (stream) | hash of the spool result | spool only |
| collect.rs:~292 | `install` | read-whole | `load_sidecar(&req.result)` and `normalize_sidecar_file` (rewrites in spool) | spool only |
| collect.rs:116-121, 145-160 | `volume_uuid_for` / `sidecar_volume_uuid` | read-whole + stat | `load_sidecar` of `.mokuro(.gz)` and layers; `metadata().modified()` to pick the oldest layer | sidecar read; mtime ordering |
| collect.rs:184, 199 | `unique_path` | stat | `exists()` loop for `<stem>_<n>` | check-then-use race, tolerated only under the path lock |
| collect.rs:353 | `install` | stat | `plain.exists() \|\| gz.exists()` | |
| collect.rs:209 | `move_into_place` | rename | spool to library | atomic rename on the same fs |
| collect.rs:218-220 | `move_into_place` fallback | copy, atomic write (tmp+rename), delete | `.{name}.part` beside the destination, then rename, then remove source | tmp+rename atomicity; `.part` dotfile visible to walks |
| collect.rs:350 / 29 | `lock_with_patience` / `PathLocks` | other (lock) | in-process DAV write lock, 50x100ms | locks are in-process only, no cross-process fence |
| collect.rs:246-251 | `run` | delete (spool) | `remove_file` / `remove_dir_all` of the claim dir | spool |
| collect.rs:388 | `record_written` | stat | `stamp_of(cbz)` for provenance | |
| upgrade.rs:245 | `edited` | stat | bare `.mokuro` mtime vs the provenance row's `written_at + 2s` | mtime is trustworthy and monotonic |
| upgrade.rs:289 | `layer_candidates` | list-dir | siblings of the cbz, filtered by `volume_stems` / `layer_id_of_sidecar` | one readdir per call; name-based matching |
| upgrade.rs:73,322,656 | `classify`, `read_from_current_archive`, `kept_by_upgrade` | read-whole | `load_sidecar` of bare/layer | |
| upgrade.rs:318-340 | `read_from_current_archive` | zip-random-access | `reader_image_names(cbz)` | seekable archive |
| upgrade.rs:29, 639 | `read_sidecar` | read-whole | `read_capped` of layer and bare for byte-identity | |
| upgrade.rs:347 | `judge` | stat | `stamp_of(bare)` as the census cache key | stat-keyed cache |
| upgrade.rs:559 | `swap` | stat | `bare.exists()` | |
| upgrade.rs:571-575 | `swap` (direct) | copy, atomic write, rename | layer to `.{bare}.upgrade.tmp`, then rename over the bare file | rename-over-existing is atomic and replaces; case-folding collisions matter here |
| upgrade.rs:576 | `swap` (generated) | rename | `move_into_place(new, bare)`: spool result replaces the bare file | same as l.209 |
| upgrade.rs:570 / 667 | `swap`, `drop_kept_layer` | other (lock) | `try_lock(bare)` / `try_lock(layer)` | |
| upgrade.rs:638-642 | `direct_replace` | delete | `remove_file(layer)` | |
| upgrade.rs:670,684,691 | `bare_upgraded`, `drop_kept_layer` | stat, delete | `is_file`, `remove_file(layer)` | |
| upgrade.rs:698-699 | `drop_kept_layer` | delete | `remove_file(backup_of)` and `remove_dir(.upgrade-originals)` at `library.parent()/.upgrade-originals` (l.538-542) | assumes a writable sibling of the library root; breaks for a remote root |
| upgrade.rs:787,793 | `sweep_kept` | stat | `layer.is_file()`, `cbz.is_file()` | |
| settle.rs:35 | `read_own_copy` | read-whole (stream) | sequential 1 MiB reads of the library archive to diagnose a returned claim, 60s-class timeout | a full local read costs milliseconds |
| settle.rs:453 | return handling | stat | `metadata(job.path)`: NotFound means "archive gone"; size compared to `entry.archive_size` | |
| settle.rs:380, 478, 574 | `download_delivered`, return, `cancel_stale_jobs` | stat | `stamp_of` re-pins or compares to the claim stamp | |
| claim.rs:821; session.rs:164 | `take`, `submit` | stat | `stamp_of`; `size: stamp.map(...)`, `etag: None` (session.rs:~180) | |
| session.rs:~176-181 | `submit` | other | local: `archive` = absolute library path (`cbz.to_string_lossy()`); remote: `ARCHIVES_ROOT + rel` | in-place path handed to the in-process runner |
| bench.rs:187-198 | `has_archive` | walk | first-`.cbz` probe (called at bench.rs:591) | |
| bench.rs:207 | `build_sample` | walk | `list_archives` | |
| bench.rs:~246 | `build_sample` | zip-random-access | `Volume::open` per archive; page bytes copied into a stored zip | seekable archive |
| bench.rs:239 | `build_sample` | write-whole | sample at `out` (storage, not library) | spool |
| bench.rs:891,895,920,1007,1446 | sample lifecycle | mkdir, delete | storage dir | spool |
| admin.rs:105-114 | `stats_volumes` | walk | `list_archives` plus rel filter | |
| admin.rs:146 | stats loop | stat | `plain.exists() \|\| gz.exists()` per row per archive | O(rows x volumes x 2) stats |
| admin.rs:639 | upgrade endpoint | stat | `cbz.is_file()` | |
| settle.rs:610-621 | `archive_arrived` | stat, zip | `compute` plus `pages_of` | |
| api.rs:503-568 | result upload | mkdir, write, rename, delete | `.processing/<sid>/<claim>/result.mokuro.part`, then rename (spool) | spool |
| api.rs:644,669 | `bench_sample` | stat, open-stream/seek | serves the sample with Range (storage) | not library |
| control.rs:455 | `start_local` | mkdir | `.processing` (spool) | |
| settle.rs:194,313,332; session.rs:725,728 | claim cleanup | delete | `remove_dir_all` in `.processing` | spool |
| processor session.rs:225 | `Link::Local` | stat | `metadata(op.archive).is_file()` or return class `missing` | local path semantics |
| processor session.rs:254 | `deliver` (local) | read-whole | `sha256_file` of the result (client.rs:517) | spool |
| bunko-engines session.rs:268 | `run` | zip-random-access | `ArchivePages::open(archive)` then `.read(rel)` (l.309), directly on the library path | mmap/seek on the real file |
| bunko-dav read.rs:221-311 | archive GET | open-stream/seek/range | `File::open`, `file.metadata()`, `file_etag`, `If-Range`, `seek` (outside `ocr/`, but it is what remote processors fetch) | strong ETag derived from stat |

Not present anywhere in scope: watch/notify, canonicalize/same_file, mtime-set, mmap, hard_link.

## 2. Counts by category (approximate distinct call sites)

| category | count |
|---|---|
| stat/metadata/exists/is_file | ~28 |
| read-whole (`load_sidecar`, `read_capped`, sha256 and own-copy streams) | ~9 |
| zip-random-access | 5 (`owed.rs:201`, `upgrade.rs:~333`, `bench.rs:~246`, engines `session.rs:268`, `compute` via `pages_of`) |
| list-dir | 1 (`upgrade.rs:289`) |
| walk | 3 (`owed.rs:132`, `bench.rs:187`, `admin.rs:105` via `list_archives`) |
| rename/move | 4 (`collect.rs:209,219`, `upgrade.rs:573,576`) |
| copy | 2 (`collect.rs:218`, `upgrade.rs:571`) |
| delete (library tree) | 4 (`upgrade.rs:642,691,698,699`) |
| delete (spool) | ~12 |
| atomic write via tmp+rename | 2 (`collect.rs` fallback, `upgrade.rs` direct) |
| lock | 3 (`collect.rs:350`, `upgrade.rs:570,667`) |
| open-stream/seek/range | 1 in the library tree (DAV GET) |
| mkdir, symlink, watch, mtime-set | 0 in the library tree |

## 3. Hot paths

**Background (scheduler helper threads):**
- The periodic full walk (`sched/mod.rs:603`, every poll interval) runs `list_archives`. For each volume it runs `compute`, which costs 1 stat plus 2 stats per enabled row. It also calls `facts.missing_pages` and `facts.page_count`, and `archive_pages` on unknown volumes.
- When upgrades are enabled, the walk also runs `judge`. That is a stat, a `classify` read of the bare `.mokuro`, and, if the volume is not yet upgraded, `layer_candidates` (a readdir) plus a layer read per layer.
- Because of this the walk is O(volumes x rows) stats plus O(volumes) reads, so a remote store needs index-backed answers.

**Event-driven, per WebDAV write (`glue.rs:67`, `app.rs:285`):** `archive_arrived` runs `cancel_stale_jobs` (`stamp_of` per claim) and `compute` plus `pages_of`, once per PUT/MOVE/COPY of a `.cbz`. A bulk upload therefore triggers a zip directory read per file.

**Per claim (~once per volume):**
- `still_owed` runs per candidate at `claim.rs:794`, possibly several per scan.
- `stamp_of` runs at claim, submit, delivery and return.
- `install` runs end to end, plus `volume_uuid_for`, which can read several layers.

**Per admin request:** `stats_volumes` plus the `exists` loop (`admin.rs:119-146`) walks the whole library on each stats request, and `has_archive` runs per bench request.

**Not in the OCR subsystem:** nothing here runs per page view, and nothing here runs per PROPFIND. Those are in `bunko-dav` and `bunko-library`, which are out of scope.

**Per remote processor volume:** one archive GET (`read.rs:196+`).

## 4. Existing tests

**Unit tests inside the modules:**
- `owed.rs:208` covers the walk, case variants (`V1.CBZ`, `V2.Cbz`), `still_owed` and `.gz` handling.
- `collect.rs:441` covers `unique_path` only, with no test of `move_into_place`'s cross-fs fallback.
- `upgrade.rs:839` covers `classify` only.
- `sched/bench.rs:1669` covers sample names and apply tables, not `build_sample` I/O.
- `queue_file.rs` and `api.rs` (`parse_range`, result names) cover the virtual queue file and upload naming.

**Integration tests in `bunko-server/tests/`:**
- `ocr_sched.rs` is the main scheduler, walk, claim and collect harness. It uses `ocr_common/mod.rs`.
- `ocr_upgrade.rs`, `ocr_upgrade_edited.rs` and `admin_upgrade.rs` cover census, swap, direct replace and the edited-mtime heuristic.
- `ocr_http.rs`, `ocr_bench_remote.rs` and `ocr_pause.rs` cover result upload and the bench sample.
- `dav_path_case.rs` covers path case and is the closest test to the owner's case rule.
- `library_*.rs` cover the compile and facts layer.

**Processor tests (`bunko-processor/tests/`):** `remote.rs`, `sessions.rs`, `installing.rs` and `fetch.rs` cover the fetch, Range and ETag behaviour and the local link.

**Gaps:**
- Nothing exercises rename failure, the cross-device fallback, or two names that differ only in case.
- Nothing tests `drop_kept_layer`'s `.upgrade-originals` location.
- `read_own_copy` has no test.

## 5. Suggested `LibraryStore` operations

Eight operations cover everything above:

1. **`list(prefix, recursive) -> iterator<{rel, size, mtime_ns, kind, version_tag}>`**. Covers `list_archives`, `has_archive`, `stats_volumes` and `layer_candidates` (a one-directory listing). For RemoteStore it is served from the SQLite index.
2. **`stat(rel) -> Option<{size, mtime_ns, version_tag, is_file}>`**. Covers `stamp_of`, `compute`, `settle.rs:453`, `exists` and `is_file`. Add a batch form, `stat_many` or `siblings(cbz) -> {has_plain, has_gz}` per row, so `row_done` and the stats loop cost no round-trips.
3. **`read_small(rel, cap) -> bytes`**. Covers `load_sidecar` (decompress `.gz` above it), `read_capped` and `classify`.
4. **`open_read(rel) -> seekable reader`**. Covers `Volume::open` and `ArchivePages::open`. RemoteStore returns a Range-backed reader (bridge) or a spooled local copy.
5. **`fetch_to_local(rel, expect_stamp) -> LocalPath`**. For the OCR spool and the in-process LocalProcessor, replacing "archive is a library path" in `session.rs:~176` and `processor/session.rs:225`.
6. **`put_atomic(rel, source_path, mode={create_unique|replace}) -> final_rel`**. Covers `move_into_place`, `unique_path` and the swap's tmp+rename. The store owns uniqueness (`_n` suffix) and case-fold collision detection.
7. **`delete(rel)`**, plus `rename(from, to)` for case-only renames.
8. **`lock(rel) -> guard`** (the existing `PathWriteLocks`) and **`changed_since(cursor)`**. The second feeds `archive_arrived` and cache invalidation instead of the poll walk.

`LibraryFacts` (`missing_pages`, `page_count`) stays DB-backed. It should read through `read_small` and `open_read`, not paths.

### Three hardest call sites to abstract

1. **In-place archive path to the LocalProcessor** (`session.rs:~176-181`; `processor/session.rs:225-233`; `bunko-engines/session.rs:268,309`). The engine needs a real seekable file, and `size` and `etag` are the pin for later checks. A remote store must materialise the archive into a spool and carry the version tag through claim, return and install. Today `etag: None` and the pin is `(size, mtime_ns)`. Remote providers expose neither a POSIX mtime nor a strong ETag consistently. `stamp_of` equality is used at `collect.rs:267`, `settle.rs:574` and `claim.rs:821`, so the stamp must be redefined as the store's `version_tag`.
2. **Generation upgrade swap** (`upgrade.rs:559-576`, `edited` at `:245`, backups at `:538`). It depends on an atomic rename-over-existing, which most providers don't offer (Drive creates a new revision and MEGA/OneDrive may duplicate). It uses the local mtime as an "a person edited this" signal, compared against the `written_at + 2s` provenance row. Remote modification times and the sidecar's own write time are not comparable. `.upgrade-originals` sits outside the library root. The `.mokuro` / `.<layer>.mokuro` name matching in `layer_candidates` also needs case folding.
3. **`collect.rs` `install` plus `unique_path`** (`:184-222`, `:350-358`). The exists-then-rename sequence under an in-process lock is a TOCTOU window that only works on a single writer. On a remote store the provider (not Bunko) is the other writer, so the lock doesn't fence it. Case-only collisions between `Vol.mokuro` and `vol.MOKURO` need NFC+lowercase fold before the `_n` suffix is chosen. The `.part` fallback also leaves a visible dotfile in the index.
# Library-tree filesystem access in bunko-server (outside ocr/) and mokuro-bunko

All paths are under `crates/`. Abbreviations: `bs` = `bunko-server/src`, `mb` = `mokuro-bunko/src`, `bl` = `bunko-library/src`, `bd` = `bunko-dav/src`.

**Scope finding.** `bunko-server` has few direct library-tree calls. The heavy I/O sits behind crates it calls:
- `bunko-dav` handles every WebDAV GET, PUT, MOVE, COPY, DELETE and PROPFIND. It is wired at `bs/app.rs:359-376`.
- `bunko-library` does the tree walk, the compiler, the manifest and sidecar reads.
- The server has no rename/delete series or volume endpoints. Those happen only as WebDAV MOVE/DELETE in `bd/methods/write.rs`.
- There is no `migrate` command, and `mokuro-bunko` setup/doctor never read the library tree.
- I did not read the `bunko-library` or `bunko-dav` internals. The grep-count table below shows the surface that would need the same abstraction.

## 1. Call sites

Rows marked **[D]** are delegated: the server only calls into `bunko-library` or `bunko-dav`. Their inner calls are not enumerated here.

| file:line | fn | category | purpose | local-only assumption |
|---|---|---|---|---|
| bs/app.rs:43 → `bunko-core/src/storage.rs:39-45` | startup `ensure_directories` | mkdir (library, inbox, users, thumbnails) | create the layout | mkdir -p |
| bs/app.rs:50 → `storage.rs:48-63`, `69+` | `assert_writable_dir(library)` | stat + other (create_new probe file, then remove) | startup writability check | POSIX permissions; probe write/delete |
| bs/library/watcher.rs:115 | `LibraryWatcher::start` | mkdir | create the root before watching | none |
| watcher.rs:116-131 | `LibraryWatcher::start` | watch/notify (`RecommendedWatcher`, recursive) | detect out-of-band changes, feed the OCR queue and metadata | inotify; rename From/To/Both; `MaxFilesWatch` and overflow lead to rescan |
| watcher.rs:49-51 (used at 65, 85) | `stat_is_dir` | stat | is a created or renamed path a directory | cheap stat; rename-pair semantics |
| watcher.rs:43-47 | `vanished_is_dir` | none (name heuristic) | guess whether a removed path was a directory (no `.ext` means folder) | `.tmp` staging naming convention |
| bs/library/runtime.rs:318 | `library_resolved` | canonicalize | resolved root for classifying DAV-reported paths | symlinks; real paths |
| runtime.rs:743 | `on_library_write` | stat | classify a DAV-reported write | cheap stat; falls back to the heuristic when the path is gone |
| runtime.rs:261 | `LibraryRuntime::new` | **[D]** walk (`LibraryIndexCache`) | series/volume index with TTL and byte budget | cheap list-dir/stat of the whole tree |
| runtime.rs:267 | `MetadataService::new` | **[D]** walk, read-whole, atomic write, zip random access | compile series.json and catalog.json, scan `.cbz`/sidecars | atomic rename. The `.series.json.compile-x.tmp` convention is pinned at watcher.rs:171. Also DAV path locks (`DavPathLocks`, app.rs:233) |
| runtime.rs:766 | `on_series_put` | **[D]** write | client series.json PUT, goes through the compiler | per-path locks |
| runtime.rs:802, 809, 816 | `missing_pages_now`, `cached_page_count`, `cached_mokuro_sha256` | **[D]** zip random access, stat | OCR scheduler and manifest facts | cheap stat of size and mtime (cache identity); seekable `.cbz` |
| bs/library/util.rs:156 | `resolve` | canonicalize (loop up the parents) | `Path.resolve()` and containment checks. Called from catalog.rs:294, 295, 357, 358, 500, 501 | symlinks; real FS |
| bs/library/catalog.rs:306 | `get_series` | **[D]** list-dir (`damage_by_volume_title`) | page-count and missing-pages per volume | dir listing |
| catalog.rs:362 | `serve_cover` | stat | cover size and is_file | cheap stat |
| catalog.rs:377-380 | `serve_cover` | open-stream (whole file via `ReaderStream`; no Range) | stream a library image | seekable local file |
| catalog.rs:514 | `build_manifest` | **[D]** list-dir, stat, read | per-volume manifest: sidecar and layer presence | cheap stat; dir listing |
| bs/davhooks.rs:47 | `sidecar_volume_uuid` | **[D]** read-whole (`read_sidecar_bytes`, gzip, capped) | read volume_uuid after a DAV PUT or when a sidecar leaves | local read |
| bs/backend.rs:155 (`virtual_to_physical` at 86 is pure path mapping) | `physical_exists` | stat | DELETE authorization (auth/mod.rs:441) | cheap stat |
| bs/app.rs:359-376 | `dav_handler` | **[D]** all DAV I/O | list-dir, stat, open/range, write, tmp+rename, rename, copy, delete, mkdir. `MOKURO_NGINX_ACCEL` makes `bd/methods/read.rs:29`, `263-267` emit `X-Accel-Redirect` | POSIX rename and Range on a local file; nginx must see the same path |
| bs/app.rs:539-545 | startup | calls `remove_corrupt_sidecars` and `Thumbnails::spawn` | one-shot sweep and cover loop | none |
| bs/thumbs/mod.rs:59 | `needs_thumbnail` | stat (`is_file`) | cover needed? | cheap stat |
| thumbs:63, 64 | `needs_thumbnail` | stat (`exists` ×2: `.webp`, `.nocover`) | skip volumes with a cover or marker | stat; sibling files by name |
| thumbs:88 | `cover_image` | open | open the `.cbz` | local file |
| thumbs:92, 105, 113-116 | `cover_image` | mmap/zip-random-access (`ZipArchive`, `by_name`, `read_to_end`) | extract the first image | seekable `.cbz` (central directory plus one member). Whole-archive download is not needed |
| thumbs:136 | `ensure_thumbnail` | write-whole (empty `File::create`) | `.nocover` marker | none |
| thumbs:148-149 | `ensure_thumbnail` | atomic write (`.webp.tmp` then rename) | write the cover | atomic rename |
| thumbs:151 | `ensure_thumbnail` | delete (cleanup of the `.tmp`) | cleanup on failure | none |
| thumbs:164-175 | `walk` | list-dir/walk (recursive; follows file symlinks) | all `.cbz` and `.mokuro*` | cheap recursive list; `file_type` without stat |
| thumbs:195 | `stamp` | stat (size + mtime ns) | remember failed covers until the archive changes | mtime/size identity; cheap stat |
| thumbs:323, 347 | `check_sidecar` | open-stream (gzip + `BufReader`, capped), read-whole | validate sidecar JSON | local read |
| thumbs:374 | `remove_corrupt_sidecars` | stat | per-file guard | none |
| thumbs:389 | `remove_corrupt_sidecars` | delete | delete invalid sidecars | none |
| bs/admin/status.rs:10 | `disk_usage` | other (`statvfs` on `storage.base_path`) | disk gauges | a mounted filesystem. A cloud library has no quota here |
| status.rs:45-51 | `status` | list-dir (+ `is_dir` stat per entry) | "volume_count" = series folders | cheap list-dir |
| mb/cmd/setup.rs:183 | `setup` | mkdir (`ensure_directories`) | create storage dirs | mkdir |
| mb/cmd/doctor.rs:322 | `doctor` | mkdir (`ensure_directories`) | check dirs | mkdir |
| mb/cmd/doctor.rs:299 → `mb/gui/paths.rs:79-81` | `ensure_writable` | mkdir + other (probe) | writability | POSIX permissions |
| mb/gui/paths.rs:93, 108 | `storage_blocker`, `dir_writable` | stat | pick a storage folder | POSIX permissions |
| mb/gui/api.rs:442 | `ensure_writable` (`base_path`) | mkdir + probe | GUI setup | same |
| mb/cmd/doctor.rs:758 | `check_disk` | other (`available_space` on storage) | low-disk warning | local FS |

Storage-adjacent calls that are not in the library tree, excluded from the counts below:
- `mb/gui/setup.rs:301-314`, `list_dir`: folder picker (canonicalize, read_dir).
- `mb/cmd/doctor.rs:802`, `.ocr-failures.json`.
- `bs/accounts/account.rs:288`, `remove_dir_all` of the users directory.
- Embedded static assets are served from rust-embed (`catalog.rs:432`), not the filesystem.

Delegated surface in other crates (non-comment lines matching a grep for fs calls, i.e. `std::fs::`, `File::open`, `read_dir`, `rename`, `canonicalize`, `metadata(`). It shows how much a trait must also absorb:
- `bd/methods/write.rs`: 40
- `bd/upload.rs`: 14
- `bd/path_case.rs`: 14
- `bl/archive/mod.rs`: 13
- `bd/resource.rs`: 9
- `bl/fsutil.rs`: 5
- `bd/paths.rs`: 5
- `bl/sidecar.rs`: 4
- `bd/write_locks.rs`: 4
- `bl/archive/zipdir.rs`, `bl/service.rs`, `bd/hooks.rs`, `bd/methods/read.rs`: 3 each
- `bd/lib.rs`, `bd/sidecars.rs`: 2 each
- `bd/methods/put.rs`: 2
- `bl/index.rs`, `bd/propfind.rs`: 1 each

## 2. Counts per category (direct sites; each distinct fn/line is counted once)

| category | direct | delegated [D] |
|---|---|---|
| stat/metadata | 11 (watcher:50, runtime:743, catalog:362, backend:155, thumbs 59/63/64/195/374, status:45-51 per-entry, storage:48-55) | many (manifest, index, cache identity) |
| list-dir / walk | 3 (thumbs:164, status:45, plus the walk callers) | index, damage, manifest |
| read-whole | 2 (thumbs:347, davhooks:47 via bl) | compiler, sidecars |
| open-stream / seek / range | 3 (catalog:377, thumbs:323, thumbs:88) | DAV GET/Range |
| mmap / zip random access | 1 (thumbs:92-116) | `bl/archive`, page counts |
| write-whole | 1 (thumbs:136) | compiler, PUT |
| atomic write (tmp+rename) | 1 (thumbs:148-149) | compiler `.compile-x.tmp`, DAV uploads |
| rename/move | 0 direct (thumbs:149 is counted as atomic write) | DAV MOVE |
| copy | 0 | DAV COPY |
| delete | 2 (thumbs:151, 389) + 1 probe delete | DAV DELETE |
| mkdir | 6 (storage.rs:40-43, watcher:115, doctor:322, gui/paths:80, setup:183) | DAV MKCOL |
| watch/notify | 1 (watcher:116-130) | none |
| canonicalize/symlink | 2 (runtime:318, util:156) | `bd/paths.rs`, `bd/path_case.rs` |
| mtime-set | 0 | none found in scope |
| other | 3 (probe at storage.rs:69+, statvfs status:10, available_space doctor:758) | none |

## 3. Hot paths

**Per request**
- `/catalog/api/manifest` (catalog.rs:497-546), per volume view:
  - `resolve` ×3 (canonicalize loops), then `build_volume_manifest`, then `cached_mokuro_sha256`.
  - The response is `no-store`.
- `/catalog/api/series` (get_series, catalog.rs:292-337):
  - `resolve` ×2, an index snapshot, and `damage_by_volume_title` over the series folder.
- `/catalog/api/cover` (catalog.rs:353-389): `resolve` ×2, a stat, then a full-file stream. `Cache-Control` is `max-age=3600`.
- DELETE authorization: `physical_exists` (backend.rs:155, auth/mod.rs:441).
- `/_admin/api/status`: `statvfs` plus a `read_dir` of the root.
- Every DAV method and PROPFIND (cached and refreshed by `propfind_cache`), at `bd/*`.

**Per DAV write**
- `on_library_write` (runtime.rs:738-748): one stat, and `index.invalidate()`.
- This also queues a per-series recompile through the debounce.
- On a PUT, `sidecar_volume_uuid` reads the sidecar (davhooks.rs:47).

**Background**
- Watcher events: translate does a stat per Create or rename event, on the notify thread.
- Metadata passes (startup after 20 s, a 6 h periodic rescan, and a debounced pass 10 s after each change burst).
- The thumbnail loop: a full recursive walk plus 3 stats per `.cbz`, every `ocr.poll_interval`, on `spawn_blocking`. This is the highest steady-state load, and it runs forever.
- The one-shot corrupt-sidecar sweep at startup (a full walk plus a JSON parse of every sidecar).
- Startup mkdir and writability probes.

## 4. Existing tests

**bunko-server unit tests**
- `bs/thumbs/mod.rs:431-546`:
  - `covers_markers_and_retries`
  - `upper_case_archives_get_covers`
  - `corrupt_sidecars_are_removed`
  - `over_cap_sidecars_are_not_inflated_or_deleted`
- `bs/library/watcher.rs:144`, `relevance_and_kinds`: pure translate logic, no real inotify.
- `bs/library/util.rs:199`, `resolving`.
- `bs/accounts/account.rs:312-324`: users-dir symlink safety, not the library tree.

**bunko-server integration tests** (`bunko-server/tests/`)
- `library_compile.rs`:
  - startup pass writing series.json and catalog.json
  - `watcher_recompiles_a_series_after_a_volume_arrives`, a real inotify test (`watch: true`)
  - `dav_writes_schedule_the_series_without_the_watcher`
  - burst and cap debounce
  - store row parity
  - `case_variant_folders_each_keep_their_own_catalog_row`
- `library_catalog.rs`:
  - library listing, series page, cover, `manifest_is_gated_like_the_archive`, config, ocr-status
- `library_series_put.rs`
- `library_community.rs`
- `dav_path_case.rs`:
  - `put_into_a_case_variant_folder_lands_in_the_existing_one`
  - this is the owner's case-insensitive rule
- `differential_053.rs`: DAV parity with 0.5.3
- `admin_settings.rs:332-341`: status fields, `volume_count == 3`
- `ocr_http.rs`, `ocr_sched.rs` and similar, which exercise the OCR side.

**Other crates**
- `bunko-dav/tests/`:
  - `path_case.rs`
  - `move_copy_lock.rs`
  - `uploads.rs`
  - `propfind_cache.rs`
  - `webdav_ops.rs`
  - `differential.rs`
- `bunko-library/tests/`:
  - `service_behaviour.rs`
  - `golden_library.rs`
  - `vectors.rs`

**mokuro-bunko** (`mokuro-bunko/tests/`)
- `setup_cli.rs`
- `doctor_cli.rs`
- `healthcheck_cli.rs`
- `config_cli.rs`
- `mb/gui/paths.rs:276-311` (permission tests)

**Gaps**
- The 0.8 case-insensitive NFC+fold rule has tests only at the DAV layer (`dav_path_case.rs`, `bunko-dav/tests/path_case.rs`). Nothing covers it in the manifest, catalog, thumbs or watcher paths.
- The `resolve()` containment check is not covered for symlinks.
- `statvfs` and the status volume-count under a non-local store are untested.

## 5. Suggested minimal `LibraryStore` surface

The operation signatures use a normalized library-relative key, with NFC and lowercase fold applied inside the store:

1. `stat(key) -> Option<{is_dir, size, mtime, etag/version}>`
2. `list(key) -> Vec<{name_preserved, is_dir, size, mtime}>`, plus `walk(key, filter)` (can be built on `list`; the remote index backs it)
3. `read(key, cap) -> Bytes` for small files. Remote reads go through the local cache.
4. `open_read(key) -> RangeReader` (seek and Range). Local returns a file; remote returns a ranged HTTP reader or a spooled temp copy.
5. `write_atomic(key, source: Bytes | TempFile, expected_version?)`. Covers tmp+rename for local and write-through for remote.
6. `rename(from, to)`, which must handle case-only rename. `copy(from, to)`.
7. `delete(key, recursive)` and `mkdir(key)`.
8. `exists_cheap` (derived from `stat`), plus `capacity() -> Option<{total, free}>` for status/doctor.
9. `changes() -> Stream<WatchEvent>`. Local wraps notify; remote wraps the provider change feed. `Rescan` is shared by both.
10. `fetch_to_local(key) -> LocalPath` for the OCR spool, the cover extraction and anything needing `ZipArchive`.
11. `startup_check()` for the writability check and ensure-dirs.

The `resolve()`/`is_within` pair at `util.rs:139` and the canonicalize at `runtime.rs:318` become a pure key-normalization step and disappear from the I/O layer.

## Three hardest call sites to abstract

1. **The notify watcher (`bs/library/watcher.rs:116-131`, with `vanished_is_dir` at 43 and `translate` at 54).**
   - It depends on inotify event kinds, the `Rename From/To/Both` triple and kernel-queue overflow.
   - It guesses "was a directory" for removed paths from the name.
   - It relies on `.json` and `.tmp` writes being filtered so the compiler's own writes don't re-trigger it (watcher.rs:5, 161, 166-173).
   - Provider feeds differ: no rename atomicity, polling lag, no per-file `.tmp` staging.
   - Self-write suppression has to move from filename filters to write-origin tagging.
2. **`MetadataService`/`LibraryIndexCache` at `runtime.rs:261-267`, and the compiler's atomic tmp+rename of series.json/catalog.json.**
   - The compiler assumes cheap stat identity (size and mtime for cache keys), directory listings and POSIX atomic rename.
   - Compile locks are shared with the DAV layer (`DavPathLocks`, app.rs:233).
   - Zip central-directory reads (page counts, missing pages) need ranged or spooled access to archives.
   - All of it sits in `bunko-library`, so the real work is a trait threaded through that crate.
3. **The thumbnail loop plus corrupt-sidecar sweep (`thumbs/mod.rs:163-190`, `88-116`, `148-149`, `389`).**
   - A full-tree walk every `poll_interval` with 3 stats per archive is cheap locally and prohibitive against an API.
   - It needs a `.cbz` central directory via seek, and an atomic sibling cover write.
   - The `.nocover` marker is a sidecar file whose absence means "work to do". It must live in the index, or in a provider-side marker.
   - It deletes remote files on parse failure.
   - It uses mtime-and-size stamps for retry gating.

The DAV GET path (`bd/methods/read.rs:29`, `263-267`) is also hard, but it belongs to `bunko-dav`. X-Accel redirects a local path to nginx, so it cannot serve a remote-backed volume. The bridge must stream it with Range instead, or stage it locally.
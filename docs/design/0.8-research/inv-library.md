# Library filesystem inventory: bunko-library, bunko-core storage

Paths are relative to `crates/`. Scope findings first:

- `bunko-library/src/store.rs` has no filesystem access. It holds only the `MetadataStore` DB trait (`store.rs:171`). It is not the place to put `LibraryStore`.
- `bunko-library` has no thumbnail code. Thumbnails are in `bunko-server/src/thumbs/mod.rs`, listed under "Adjacent".
- `bunko-library` has no watcher. The watcher is in `bunko-server/src/library/watcher.rs`. `paths.rs:84-113` holds the pure `classify_change` and `is_relevant_change` functions it uses.
- `library_path` is resolved by `StorageConfig.base_path` (`bunko-core/src/config.rs:77`, set from `MOKURO_STORAGE` at `config.rs:1229`). `StorageLayout::library()` returns `base/library` (`storage.rs:16`). `config.rs:92` exposes it. `thumbnails()` is `library/thumbnails` (`storage.rs:25`, inside the library tree).
- `library_path` is a plain `PathBuf` passed by value into `MetadataService` (`service.rs:216-236`) and `LibraryIndexCache` (`index.rs:232`). Callers hand out `&Path` and join onto it.
- `bunko-dav/src/lib.rs:124` canonicalizes the library root once.

## 1. Call sites

Hot column: R = per request or PROPFIND or page view, B = background.

| file:line | fn | op | purpose | local-only assumption | Hot |
|---|---|---|---|---|---|
| fsutil.rs:49 | `stat` | stat | `fs::metadata`, follows symlinks. Wrapper for nearly every stat below. | cheap stat, symlink follow | R+B |
| fsutil.rs:53,57 | `is_file`, `is_dir` | stat | type checks | cheap stat | R+B |
| fsutil.rs:11-35 | `py_mtime` | stat (mtime f64) | cache key. Compared EXACTLY, `repr` embedded in keys. | ns-resolution mtime, stable | R+B |
| fsutil.rs:67 | `list_dir` | list-dir | listing. Skips non-UTF-8 names. | cheap readdir, no listing-order dependence | R+B |
| fsutil.rs:99-114 | `create_temp` | other (`create_new`, mode 0o666) | temp `.name.compile-xxx.tmp` beside target | POSIX perms and umask, same-dir temp | B |
| fsutil.rs:126 | `atomic_write_bytes` | mkdir (`create_dir_all`) | | | B |
| fsutil.rs:133-137 | `atomic_write_bytes` | atomic write (write, fsync, rename) | publishes series.json and catalog.json | atomic rename over existing file, fsync | B |
| fsutil.rs:140 | `atomic_write_bytes` | delete (`remove_file`) | temp cleanup on failure | | B |
| index.rs:91 | `scan_dir` | list-dir | one dir per walk step | | B (R on cache miss) |
| index.rs:97 | `scan_dir` | stat (`is_dir` per entry) | splits subdirs from files | cheap stat per entry (N+1) | B |
| index.rs:180 | `scan_library` | stat | root exists | | B |
| index.rs:184-198 | `scan_library` | walk (explicit stack) | whole-tree census. Skips hidden dirs. Builds `pending_ocr` and `pending_thumbnails` from names only (no per-file stat). | full-tree walk is cheap | B (R via `get_snapshot`) |
| index.rs:188 | `scan_library` | same_file/symlink (`symlink_metadata`) | symlinked dirs listed, not descended | symlinks | B |
| compiler.rs:45 | `iter_series_folders` | list-dir | top-level series | | B |
| compiler.rs:51,57,61 | `iter_series_folders`, `has_archive` | stat + list-dir | folder is a series if it holds a `.cbz` (one list-dir per top-level dir) | cheap list/stat | B |
| compiler.rs:68,73 | `archive_names` | list-dir + stat | `.cbz` names in a series | | B |
| compiler.rs:134 | `compile_volume` | stat | archive size | | B |
| compiler.rs:138 | `compile_volume` | read-whole (via `load_sidecar`) | parse `.mokuro` and hash | whole sidecar in RAM (cap 256 MiB) | B |
| compiler.rs:139 | `compile_volume` | mmap/zip-random-access (via `reader_image_names`) | read the zip central directory to count pages | seekable file | B |
| compiler.rs:356-359 | `volume_stats` | stat x2 + `primary_sidecar` | cache validity: cbz size and mtime, sidecar `name:size:mtime` | cheap stat, exact mtime | B |
| compiler.rs:464 | `compile_series_volumes` | stat | cover size and mtime, deliberately never cached | | B |
| compiler.rs:554,569 | `missing_pages_now` | stat + `primary_sidecar` | OCR scheduler damage check. `service.rs` calls it through `runtime.rs:802` (blocking). | | B (sched) |
| sidecar.rs:124 | `sidecar_siblings` | list-dir | files to delete with an archive | | R (DELETE) |
| sidecar.rs:146 | `primary_sidecar` | stat (up to 2) | `.mokuro` else `.mokuro.gz` | | R+B |
| sidecar.rs:177-183 | `read_capped` | open-stream + stat + read-whole | size-capped sidecar read. Used by `load_sidecar`, `davhooks.rs:47` and `thumbs/mod.rs:347`. | | R+B |
| manifest.rs:66 | `file_meta` | stat | per-sidecar metadata for the manifest | | R |
| manifest.rs:91 | `build_volume_manifest` | list-dir | series dir listing, then stat of cbz, ocr, cover, layers and series.json (`manifest.rs:140-190`) | cheap stat fan-out (about 5 plus layers) | R (`catalog.rs:514`) |
| manifest.rs:210 | `damage_by_volume_title` | read-whole (`read_to_string`) | reads our own series.json | read-after-write consistency | R (`catalog.rs:306`) |
| service.rs:157-158 | `write_if_changed` | stat + read-whole | skip the write when bytes are identical, to keep clients' size/mtime cache keys stable | mtime preserved when not written | B |
| service.rs:164 | `write_if_changed` | atomic write | publish | see `fsutil.rs:133` | B |
| service.rs:281 | `scan_folders` | list-dir | probe: "root unreadable" means skip the pass | mount-failure semantic | B |
| service.rs:293 | `publish_series` | stat | "never resurrect a deleted folder" | TOCTOU guard | B |
| service.rs:361,368 | `materialize_catalog_row` | list-dir + stat per entry | latest cbz mtime, cover pick | | B |
| archive/mod.rs:117 | `sniff_zip` | open-stream | magic bytes for `.cbr` | | B |
| archive/mod.rs:133 | `open_with_stem` | stat | file vs directory volume | | R+B |
| archive/mod.rs:269 | `walk_pages` | same_file/canonicalize | loop guard for directory volumes | symlinks, inode identity | B |
| archive/mod.rs:277,291 | `walk_pages` | walk (`read_dir` plus stat) | directory-of-images volumes | | B |
| archive/mod.rs:190 | `open_page` | open-stream | page file in a directory volume | | R |
| archive/zipdir.rs:325 | `list_entries` | mmap/zip-random-access | open file, seek-from-end for EOCD, then central directory (`zipdir.rs:184-203,346`) | seekable file | B (OCR, compile) |
| archive/zipdir.rs:457-506 | `open_member` | open-stream/seek | reopens the file per page, seeks to `header_offset`, streams and inflates with CRC check | seek and range read, a fresh `File::open` per page | R / OCR page read |
| bunko-core/storage.rs:40-43 | `ensure_directories` | mkdir | library, inbox, users, thumbnails | | startup |
| bunko-core/storage.rs:49,55 | `assert_writable_dir` | stat | | | startup |
| bunko-core/storage.rs:80-84 | `probe_writable` | other (`create_new` plus `remove_file`) | writability probe | POSIX perms | startup |

## Adjacent, outside the crate (will need the same abstraction)

- `bunko-server/src/library/watcher.rs:11,117`: `notify::recommended_watcher`, the only watch/notify user. It assumes inotify-like events and feeds `invalidate()`.
- `bunko-dav/src/lib.rs:124`: `canonicalize` on the root.
- `bunko-dav/src/path_case.rs:58-68,121,126`: `MetadataExt` (inode/dev) same-file check, `canonicalize`, `symlink_metadata`, and a `read_dir` scan for case folding. Case folding currently depends on the host filesystem.
- `bunko-dav/src/methods/write.rs`:
  - `:86` `remove_dir_all`
  - `:122` `remove_file`
  - `:538,634` `rename`
  - `:593-605` copy, then set mtime via `FileTimes`, then rename
  - `:496,740` `canonicalize`
- `bunko-dav/src/methods/read.rs:221`: `tokio::fs::File::open` for GET, with Range support.
- `bunko-dav/src/methods/put.rs:193,244`: stat for existed-before and etag.
- `bunko-server/src/thumbs/mod.rs`:
  - `:88` open cbz
  - `:136` create `.nocover` marker
  - `:148-151` write `.webp.tmp` then rename
  - `:164` `read_dir`
  - `:389` `remove_file`
- `bunko-ocr/src/pages.rs:92-130`: its own seekable `ZipArchive`.
- `bunko-processor/src/verify.rs:78` and `fake.rs:293`: zip over a file.

## 2. Counts

Counts are in-crate call sites from the table (a row with several calls counts once), and the bunko-core and adjacent rows are excluded.

| category | count |
|---|---|
| stat/metadata | 19 |
| list-dir | 10 |
| zip random access, seek, stream | 4 |
| read-whole | 4 |
| open-stream | 3 |
| atomic write (tmp, fsync, rename) | 2 |
| mkdir | 1 |
| delete | 1 |
| walk (explicit loops) | 2 |
| canonicalize/symlink/inode | 2 |
| other (`create_temp`) | 1 |
| rename (non-atomic) | 0 |
| copy | 0 |
| mtime-set | 0 |
| watch/notify | 0 |

In-crate rename, copy, mtime-set and watch counts are all 0. Those operations live in bunko-dav and bunko-server (see Adjacent).

The dominant pattern is stat and list-dir. That is the part a SQLite index replaces.

## 3. Hot paths

**Per request**
- `GET .../series/<name>` (`catalog.rs:301-306`) runs `get_snapshot` (cached) plus `damage_by_volume_title`, one read of series.json.
- `GET` volume manifest (`catalog.rs:514`) runs `build_volume_manifest`, which is one list-dir plus about 5 or more stats. This is the heaviest synchronous call.
- DELETE of a primary sidecar calls `sidecar_siblings` (`methods/mod.rs:265-269`, then `sidecar.rs:124`).
- Page views use `open_member` (`zipdir.rs:457`). That is a `File::open`, seek and inflate per page, with the central directory cached in `Volume` only per `Volume::open`.
- PROPFIND does not touch `bunko-library`. It uses bunko-dav's stat plus cache (`PropfindCache`).
- `get_snapshot` is request-triggered but rate-limited by TTL (30 s) and `RESCAN_FACTOR`. A cache miss does a whole-tree walk inline (`index.rs:292`).

**Background**
- metadata passes (`service.rs`) and `compile_series_volumes`
- `scan_library` on invalidation
- OCR scheduler `missing_pages_now`
- thumbnail generation
- watcher-driven recompiles

## 4. Existing tests

- `bunko-library/tests/golden_library.rs`: the strongest regression net. It runs against `tests/golden/library` fixtures and Python-generated expectations.
  - `full_passes_and_puts_are_byte_identical_to_python` (`:125`) covers compile and publish.
  - `a_warm_python_cache_is_read_back_without_recompiling` (`:203`) covers stat-key validity.
  - `library_index_matches_python` (`:368`) covers `scan_library`.
  - `manifests_match_python` (`:374`) covers the manifest.
  - `archive_listings_and_pages_match_python` (`:401`) covers the archive.
  - `directory_volumes_list_pages_like_python` (`:489`) covers directory volumes.
  - `damage_is_read_back_from_the_compiled_series_file` (`:517`) covers `damage_by_volume_title`.
- `bunko-library/tests/service_behaviour.rs`:
  - `compiled_files_get_umask_permissions` (`:207`)
  - `busy_and_unwritable_paths_are_skipped_and_retried` (`:171`)
  - `library_index_cache_ttl_and_invalidation` (`:228`)
  - `hooks_fire_outside_and_after_the_work` (`:99`)
- `bunko-library/tests/vectors.rs`:
  - `metadata_paths_and_watcher_routing_match_python` (`:391`) covers `classify_change`.
  - the layer split test (`:231`) covers sidecar naming.
- Unit tests in `bunko-library/src/sidecar.rs:327-440` (gzip bombs and caps) and `archive/mod.rs:395-456` (CRC, disguised cbr, missing file).
- `bunko-core/src/storage.rs:177-210` covers portable data and read-only directories.
- `bunko-dav/src/path_case.rs:341` covers case-insensitive filesystem detection.
- `bunko-server/src/thumbs/mod.rs:435-498` covers thumbnail temp files and sidecar reads.

Gaps:
- No test covers case-only renames.
- No test covers NFC-vs-NFD names.
- No test covers a library on a non-POSIX store.
- The golden tests assert byte-identical Python output, so a `RemoteStore` needs a fake in-memory `LibraryStore` that runs the same fixtures.

## 5. Suggested minimal `LibraryStore` surface

Paths are library-relative and case-folded.

1. `list(dir) -> Vec<Entry{name, kind, size, mtime_ns, etag/version}>`. This covers list-dir, `is_file`, `is_dir`, `stat`, and the `scan_dir` N+1 stats. Also add `stat(path) -> Option<Entry>`.
2. `read(path, cap) -> Bytes` for sidecars, series.json and catalog.json.
3. `read_range(path, off, len)` plus `len(path)`. A `Seek+Read` adapter over it feeds `zipdir::read_central_directory` (already generic over `Read+Seek` at `zipdir.rs:330`). `open_member` needs the same adapter.
4. `write(path, bytes, if_match: Option<etag>) -> Entry`, atomic as seen by readers. This replaces `atomic_write_bytes`.
5. `delete(path)`, `delete_tree(path)`, `rename(from, to)` (case-only must work), and `mkdir(path)`.
6. `fetch_local(path) -> LocalFile`. This is a spool for the OCR/zip consumers, as the archive consumer for `Volume` when a ranged read is too slow.
7. `changes() -> Stream<Change>` for watching, replacing notify. Plus a cheap `root_ok()` for the `service.rs:281` guard.

## 6. The three hardest call sites to abstract

1. **`archive/zipdir.rs:457-506` (`open_member`) and `zipdir.rs:325` (`list_entries`)**. These do random-access seek and re-open the file per page. On a remote store each page costs a ranged HTTP GET plus a re-fetch of the central directory. It needs either a spool or a cached `Volume`. `bunko-ocr/pages.rs:92` has a second independent zip reader that must follow.
2. **`volume_stats` (`compiler.rs:356-359`) with `py_mtime`/`sidecar_stat_key` (`fsutil.rs:11`)**. Cache validity compares exact float mtime and size, embedded in the DB key. Providers differ on mtime precision, mtime semantics and etag. The key must become an opaque version token, which changes the DB `series_entry_cache` columns `cbz_size` and `cbz_mtime`. The golden test `a_warm_python_cache_is_read_back_without_recompiling` pins the current format.
3. **`atomic_write_bytes` (`fsutil.rs:133-137`) plus `write_if_changed` (`service.rs:157-164`)**. The tmp, fsync and rename sequence assumes atomic replace and unchanged mtime when skipped. Drive and MEGA have no rename-over (Drive creates a new revision, WebDAV varies). The "unchanged bytes are not rewritten" rule must also read the old content, or compare a hash in the index. The `PathWriteLocks` guard (`service.rs:160`) is process-local and does not cover a second writer, such as the Reader app syncing the same cloud folder.

Honorable mention: `scan_library` (`index.rs:178`) is a whole-tree list-dir walk with N+1 `is_dir` stats. It becomes a SQL query on the remote index, so it is easy once the index exists.
# bunko-dav library filesystem inventory

Scope: `crates/bunko-dav/src`, with tests in `../tests`. Test-only I/O (`tempfile`, `fs::write` in `#[cfg(test)]`) is excluded.

Absent from the crate (confirmed by grep): no `notify`/inotify, mmap, `walkdir`, hard links or `set_len`. LOCK is in-memory only (`locks.rs`, `methods/lock.rs`), and dead props are in-memory (`propfind.rs:95-121`). Neither touches disk.

## Validators (what a remote backend must supply)
- **Source:** `Stat::from_meta` (`resource.rs:20-38`) takes only `meta.modified()` (split into secs and nanos), `meta.len()`, `is_dir`, and Unix `ctime`.
- **File ETag** (`resource.rs:~52`): `format!("{:.6}-{}", mtime_f64, size)`. This is mtime plus size, not an inode.
- **Folder ETag:** `"{:.6}"` of the folder's mtime.
- **Last-Modified:** `http_date(mtime_int)`, which truncates to whole seconds.
- **ctime:** carried in `Stat` but never used in the ETag.
- **Consumers of the ETag:**
  - GET and 304 handling (`read.rs:224-240`)
  - If-Range (`conditional.rs:284`)
  - If-Match and If-None-Match (`conditional.rs:46-74`)
  - PUT's response ETag (`put.rs:244-246`)
  - PROPFIND `getetag`
- **Remote requirement:** a remote backend needs `{size, mtime with sub-second precision, is_dir}` per entry, and mtime must be stable between listing and GET. Drive and OneDrive have ms-precision `modifiedTime`. MEGA stores a second-resolution mtime in the node attributes. Generic WebDAV has only `getlastmodified`, at 1s resolution. Folder mtimes on cloud providers do not bump when children change, but the local folder ETag relies on exactly that.

## 1. Call-site table

Abbreviations in the "Local-only assumption" column: **cheap-stat** = stat is cheap and synchronous, **POSIX-sem** = POSIX semantics, **atomic-rename** = rename is atomic.

| file:line | fn | category | purpose | local-only assumption |
|---|---|---|---|---|
| lib.rs:124 | `Dav::new` | canonicalize | canonical library root | real directory exists |
| lib.rs:125 | `Dav::new` | canonicalize | canonical users root | real directory |
| resource.rs:243 | `Roots::lookup` (Progress) | stat | per-user file `users/<u>/<name>` | cheap-stat |
| resource.rs:253 | `Roots::lookup` (Library) | stat | every request's resource resolution | cheap-stat, follows symlinks |
| resource.rs:355 | `members` (virtual reader root) | stat | the 3 per-user files, shown first | cheap-stat |
| resource.rs:377 | `list_dir` | list-dir | children of a collection | listing returns names only |
| resource.rs:392 | `list_dir` | stat | per-child stat, follows symlinks, skips dangling | N+1 cheap stats |
| resource.rs:420 | `Roots::walkable` | canonicalize | symlink-loop and escape guard for Depth: infinity | symlinks |
| propfind.rs:298 | `walk` | canonicalize | seed for the ancestor loop guard | symlinks |
| paths.rs:221 | `resolve_under` | symlink_metadata | per-component probe (realpath-like) | symlinks, cheap-stat |
| paths.rs:227 | `resolve_under` | symlink (read_link) | follows symlinks, max 40 | symlinks |
| path_case.rs:121 | `on_disk_name` | symlink_metadata | exact-name fast path (case-sensitive FS only) | cheap-stat |
| path_case.rs:126 | `on_disk_name` | list-dir | scan parent for case/NFC variants, once per path segment | listing, per-segment |
| path_case.rs:59-60 | `same_file` | same_file (dev+ino) | case-only MOVE detection, case probe | inode identity (Windows uses canonicalize, 68) |
| path_case.rs:86 | `is_case_sensitive` | same_file | probes the host FS once at startup | host FS case behaviour |
| read.rs:221 | `get` | open-stream | open the file for GET | seekable fd |
| read.rs:224 | `get` | stat (fstat) | validators of the bytes actually opened | fd-bound identity: fstat is consistent with the bytes, tolerating a concurrent rename-over |
| read.rs:311 | `get` | seek (Range) | `Range: start-` | seekable |
| read.rs:313 | `get` | open-stream | `ReaderStream(file.take(len))`, 64K chunks | streaming fd |
| mod.rs:268 | `primary_leaving` | stat (`is_file`) | does `<stem>.cbz` exist next to a leaving sidecar | cheap-stat |
| sidecars.rs:59 | `siblings` | list-dir | find the `.mokuro`, `.webp`, `.nocover` and `<stem>.<layer>.mokuro` sidecars of a cbz (DELETE) | listing |
| put.rs:193 | `put` | stat | `existed_before` (201 vs 204) | cheap-stat |
| put.rs:244 | `put` | stat | new ETag for the PUT response | stat after rename equals the stored object |
| upload.rs:212 | `Staging::drop` | delete | remove temp on abort | none |
| upload.rs:231 | `Staging::create` | mkdir | `create_dir_all(parent)` | none |
| upload.rs:242 | `Staging::create` | write stream (create_new) | `.{name}.upload-{rand}.tmp` next to dest, same directory as dest | same-FS temp |
| upload.rs:298 | `Staging::write` | write stream | streams the body into temp | local temp |
| upload.rs:309 | `Staging::commit` | other (fsync) | `sync_all`, EINVAL/ENOTSUP tolerated | fsync |
| upload.rs:321 | `commit` | stat | final staged size vs `Content-Length` | cheap-stat |
| upload.rs:355 / 524 / 532 | `verify_staged` | mmap/zip-random-access | open the temp file, `ZipArchive::new` (EOCD/central dir seeks), CRC-read every member | random-access seekable file, full local copy |
| upload.rs:629-638 | `read_up_to` | open-stream/seek | read 4-byte magic, seek 0 | seekable |
| upload.rs:369, 398-430 | `same_bytes` (`keep_if_identical`) | read-whole (streaming compare) | OCR sidecar unchanged re-send detection | reads the existing dest in full |
| upload.rs:382 | `commit` | atomic write (rename) | publish temp over dest | atomic-rename, overwrites, same FS |
| write.rs:79, 107 | `delete_resource` / `delete_file` | stat (`exists`) | skip if missing | cheap-stat |
| write.rs:86 | `delete_resource` | delete (`remove_dir_all`) | DELETE folder | recursive delete |
| write.rs:119 | `delete_file` | delete | delete each cbz sidecar from `siblings()` | non-atomic multi-file |
| write.rs:122 | `delete_file` | delete | delete the file; NotFound is tolerated | none |
| write.rs:180 | `mkcol` | mkdir | `create_dir_all` | idempotent mkdir -p |
| write.rs:325 | `copy_move` | same_file | case-only rename detection | inode identity |
| write.rs:491 | `same_physical` | same_file | src equals dest guard | inode identity |
| write.rs:496-503 | `physically_below` | canonicalize | MOVE/COPY folder into its own subtree | symlinks, nonexistent dest ancestors |
| write.rs:536 | `move_file` | mkdir | dest parent | none |
| write.rs:538 | `move_file` | rename | MOVE file; case-only rename is a plain rename on an insensitive FS | atomic-rename, same FS, case-preserving rename |
| write.rs:586 | `copy_one` | mkdir | dest parent | none |
| write.rs:593 | `copy_one` | copy | `fs::copy` to `.{n}.upload-copy{pid}.tmp` | full local copy |
| write.rs:594-605 | `copy_one` | mtime-set (+ stat) | `copy2`-style: preserve mtime and atime via `set_times`, so the copy's ETag equals the source's | set_times (not available on cloud) |
| write.rs:605 | `copy_one` | atomic write (rename) | publish the copy | atomic-rename |
| write.rs:608 | `copy_one` | delete | cleanup on error | none |
| write.rs:632 | `move_folder` | mkdir | dest parent | none |
| write.rs:634 | `move_folder` | rename | MOVE folder as ONE rename, children move with it | atomic directory rename (cloud: Drive/OneDrive yes, MEGA yes, generic WebDAV MOVE usually yes, but no atomicity guarantee) |
| write.rs:677 | `cbz_under` | walk (list-dir stack) | collect cbz paths before a folder MOVE, for DB hook renames | no symlink following (uses `file_type`) |
| write.rs:715 | `copy_folder` | mkdir | dest folder | none |
| write.rs:726 | `copy_folder` | walk (list-dir) | recursive COPY | listing |
| write.rs:737-743 | `copy_folder` | stat + canonicalize | symlink out-of-library guard | symlinks |
| write.rs:748 | `copy_folder` | stat (`exists`) | dest file exists, then `delete_file` | none |
| write.rs:772 | `prune_unmatched` | walk (list-dir) | delete dest members the source lacks | listing |
| write.rs:782-791 | `prune_unmatched` | stat | per-entry `is_dir` on both sides | N cheap stats |

Counts below are by call site, one per grep hit or sequence, including the Windows-only branch. Hits that share one table row (such as the three copy-folder stats) are counted as separate call sites.

## 2. Counts per category
- **list-dir / walk:** 8 call sites (`resource.rs:377`, `path_case.rs:126`, `sidecars.rs:59`, `write.rs:677`, `:726`, `:772`, plus the `walk_inner` driver and `copy_folder`'s stack). `walk_inner` and `copy_folder` do not call `read_dir` themselves; they drive `list_dir`.
- **stat / metadata:** about 17. The hot sites are `resource.rs:243, 253, 355, 392` and `read.rs:224`; the rest are `put.rs:193, :244`, `upload.rs:321`, `mod.rs:268`, `write.rs:79, :107, :737, :748, :782-791` and `path_case.rs:121`.
- **canonicalize / same_file / symlink:** 12 (`lib.rs:124, :125`, `resource.rs:420`, `propfind.rs:298`, `paths.rs:221, :227`, `path_case.rs:59, :68, :86`, `write.rs:325, :491, :496-503, :740`).
- **open-stream / seek / range:** 4 (`read.rs:221, :311, :313`, `upload.rs:629`).
- **mmap / zip-random-access:** 1 (`upload.rs:524-541`, the only seekable-archive access).
- **write stream (staging):** 3 (`upload.rs:242, :298, :309`).
- **atomic write (tmp+rename):** 2 (`upload.rs:382`, `write.rs:605`).
- **rename / move:** 2 (`write.rs:538`, `:634`).
- **copy:** 1 (`write.rs:593`).
- **delete / remove-dir:** 6 (`upload.rs:212`, `write.rs:86, :119, :122, :608`).
- **mkdir:** 7 (`upload.rs:231`, `write.rs:180, :536, :586, :632, :715, :743`).
- **mtime-set:** 1 (`write.rs:594-605`).
- **read-whole:** 1 (`same_bytes`). **watch/notify:** 0.

## 3. Hot paths
- **Per request (every method):** `lookup` (`resource.rs:253`), which calls `resolve_under` (`paths.rs:221-227`, one symlink_metadata per path component), plus the request-edge case rewrite.
  - The case rewrite is `PathCase::rewrite_request` / `canonical_path` → `canonicalize` → `on_disk_name`. The server crate runs it on all library paths and MOVE/COPY destinations (`bunko-server/src/app.rs:145-156`). It does one `read_dir` of the parent for each segment, so a 4-segment path is 4 directory listings per request. This is the largest per-request local-FS cost and is exactly what a remote index replaces.
  - `PathCase::may_rewrite` (`path_case.rs:~230`) is a cheap, no-FS filter.
- **Per PROPFIND:**
  - `list_dir` does one `read_dir` plus N stats (`resource.rs:377-392`). `walk_inner` then runs `walkable` (`canonicalize`, `resource.rs:420`) for every subfolder on Depth: infinity, and `walk` seeds with `canonicalize` (`propfind.rs:298`).
  - Depth: infinity for the anonymous allprop is served from the PROPFIND cache. `PropfindCache::generate` (`cache.rs:221-232`) calls `roots.lookup(key, None)`, then `propfind::walk(...Mode::AllProp)` on a blocking thread, compressed into a gzip prefix.
  - The cache is TTL/stale-while-revalidate, invalidated after writes (`invalidate_paths`, `cache.rs:357`), and has a debounced `schedule_refresh` and `refresh_all` (`cache.rs:378-430`).
  - Per-user progress is injected at response time, never cached (`resource.rs:355`).
- **Per GET (page view):** `lookup`, then open, fstat, seek and a 64K `ReaderStream` (`read.rs:221-313`). Range requests from the reader hit `seek` directly. With `nginx_accel` (`read.rs:29-44`) Bunko skips the open entirely and nginx serves the bytes and the Range.
- **Per PUT:** `lookup`, a stat (`put.rs:193`), then streaming staging, fsync, zip verify (reads the whole archive, CPU and IO heavy), optional `same_bytes`, rename, and a stat for the ETag. All of this is blocking work inside the request, behind a per-path write lock (`write_locks.rs`).
- **Per MOVE/COPY/DELETE:** `lookup` calls plus `canonicalize` (`physically_below`), a rename or copy, `cbz_under` for folder MOVE, and recursive `read_dir` for COPY or `prune_unmatched`.
- **Background:** PROPFIND cache refresh/warm (`cache.rs:334-430`), and startup `Dav::new` (`lib.rs:124`) plus the case probe (`path_case.rs:86`, `LibraryPathCanonicalizer::new`). The `DavHooks` callbacks (`archive_arrived`, `library_changed` in `hooks.rs`, fired from `Effects::fire`, `mod.rs:310-328`) are the bridge to the OCR scheduler. The scheduler's own library access is outside this crate.

## 4. Tests that would catch a regression
All in `crates/bunko-dav/tests/`:
- **`webdav_ops.rs`** is the broadest. It covers `propfind_*`, `get_files_and_errors`, `conditional_get`, `ranges`, `nginx_accel_offload`, `put_overwrite_returns_the_new_etag`, `put_library_file_records_ownership_and_queues`, `put_preconditions`, `put_ocr_sidecar_create_overwrite_and_unchanged_resend`, `delete_archive_takes_its_sidecars`, `delete_folder_and_errors`, `mkcol`, `physical_path_mapping`, `progress_files_are_per_user`, `stray_per_user_file_in_the_library_is_not_listed`, `listings_hide_staging_files_and_symlinks_stay_contained`, `depth_infinity_is_cached_and_injects_progress`, `cache_is_invalidated_after_a_write_commits`, and `options_answers`.
- **`path_case.rs`** covers the owner's case-insensitive rule: `existing_segments_take_their_on_disk_spelling`, `a_decomposed_spelling_matches_the_composed_folder` (NFC), `an_exact_spelling_wins_over_a_variant`, `a_rename_of_the_source_keeps_the_requested_spelling`, `traversal_is_never_resolved`, `the_kingdom_scenario`, `renaming_a_file_to_fix_its_case`, `renaming_a_folder_to_fix_its_case`, `a_case_fix_through_a_differently_spelled_parent`, `moving_onto_a_case_variant_of_another_file_is_moving_onto_it`, `copying_onto_its_own_case_variant_is_refused`, `get_and_propfind_find_a_case_variant`, `mkcol_and_put_of_a_case_variant_reach_the_existing_folder`, `a_new_folder_keeps_the_spelling_it_was_created_with`, and `a_destination_symlink_leading_outside_is_refused` (`#[cfg(unix)]`, line 254). It also probes the host FS via `host_is_case_sensitive`. These tests assume a real local filesystem; the `LibraryStore` trait needs a conformance suite run against both LocalStore and RemoteStore.
- **`uploads.rs`:** `a_new_archive_is_verified_and_a_replace_is_204`, `a_truncated_body`, `a_damaged_zip_then_the_same_damage_again`, `a_failed_replace_keeps_the_old_file`, `a_get_during_an_upload_sees_the_old_file_and_writes_conflict`, `an_aborted_upload_leaves_nothing_and_releases_the_lock`, `content_digest`, `inflation_bound_and_methods`, plus the in-source `upload.rs:850-872` tests for the inflate bound.
- **`move_copy_lock.rs`:** `move_library_file_preserves_sidecars_and_owner`, `move_series_folder_moves_everything`, `copy_file_and_folder`, `copy_move_preconditions`, `copy_or_move_onto_itself_by_another_spelling_is_refused`, `cross_class_moves_are_refused_without_effects`, `file_and_collection_never_replace_each_other`, the `lock_*` tests, and `proppatch_dead_properties`.
- **`propfind_cache.rs`:** `a_large_listing_streams_identically_in_both_encodings`, `implied_or_odd_case_infinity_is_served_from_the_cache`, `depth_one_of_a_big_folder_streams_compressed`, `budget_bounds_the_cache`, `stale_entries_are_served_while_one_refresh_runs`, `debounced_refresh_and_stop`, `per_user_paths_bypass_the_cache`.
- **`differential.rs`** (and `common/diff.rs`) compares against the Python 0.5.2 reference. It is skipped when `~/.cache/mokuro-bunko-demo/ref052/bin/python` is absent, so it is not a CI safety net.
- **Unit tests:** `paths.rs:384-398` (`resolve_under` and symlinks), `path_case.rs:339`, `sidecars.rs:101-113`, `resource.rs` `etag_formats_like_python`.
- **Gaps:**
  - No concurrency test covers a rename racing a GET beyond `a_get_during_an_upload_sees_the_old_file...`.
  - Nothing covers dangling symlinks except the listing test.
  - No test exercises a store that lacks `set_times`, atomic rename, or inode identity.

## 5. Suggested minimal `LibraryStore` surface

```
stat(path) -> Option<Entry{size, mtime_ns, is_dir, etag?}>      // replaces ~17 stats, exists, is_file
list(path) -> Vec<Entry{name, size, mtime, is_dir}>             // name AND metadata in one call; kills N+1
resolve_case(rel) -> Option<canonical rel>                      // replaces path_case on_disk_name + resolve_under
open_read(path, range) -> (Entry, AsyncRead)                    // fd-bound entry + bytes
begin_write(path, expected_size) -> StagedWrite{write, abort, commit_with_verify -> local temp path}
publish(staged, dest)                                           // atomic rename or provider upload
mkdir_all(path)
delete(path, recursive)
rename(src, dest)                                               // incl. case-only
copy(src, dest, preserve_mtime)
same_entry(a, b)                                                // inode / provider id
subtree_cbz(path)                                               // or list-recursive with a limit
spool(path) -> local seekable file                              // for zip verify / OCR
```

`canonicalize` and the symlink calls (`resolve_under`, `walkable`, `physically_below`, `copy_folder`'s guard) collapse into `resolve_case` plus a "path is inside the library" guarantee. Remote trees have no symlinks, so RemoteStore can make them trivially true.

**Hardest call sites to abstract:**
1. **`upload.rs:524-541`, `verify_staged` (`ZipArchive` over a seekable `File`).** The CBZ is verified against its CRCs by random-access reading of the entire archive (EOCD, central directory, every member). A remote store needs the staged upload spooled to local disk first, so Bunko will always need local scratch space of at least the largest archive. `begin_write` must therefore stay temp-file-based, with the provider upload as a `publish` step after verification. It cannot be a pure remote stream. The same constraint applies to OCR reading archives.
2. **`write.rs:634`, `move_folder` as a single `std::fs::rename` (plus `:538` and `cbz_under` at `:677`).** The code and the DB hooks (`rename_volume_upload`, `rename_ocr_sidecars_under_prefix`) assume the whole series moves atomically and that the children are enumerable before the move. Providers differ here: Drive and OneDrive re-parent or rename a folder with a single metadata update, which suits this. MEGA has a native move. Generic WebDAV MOVE is not guaranteed atomic and may be a copy-and-delete. Case-only renames are also provider-specific (Drive and OneDrive handle them by renaming in place, but a MOVE onto a case-folded "existing" name needs the `renames_source` / `same_file` logic at `write.rs:317-327`, which today uses inode identity). The remote index must give each entry a stable id for `same_entry` to replace that inode comparison.
3. **`read.rs:221-313`, GET with fd-bound validators plus `Stat::file_etag` (`resource.rs`).** Today the ETag is derived from an `fstat` of the exact fd being streamed, so validators match the bytes sent even with a concurrent rename-over. With a remote backend the ETag must be derived from the provider's own version tokens (revision id, md5/sha1, or modifiedTime plus size), not from locally observable mtime. The Range seek on a provider URL or an `open_read(range)` call also has to stay consistent with the entry returned for the same version. `copy_one`'s `set_times` (`write.rs:594-605`), which keeps the copy's ETag equal to the source's, has no equivalent on most providers, so the "copy preserves mtime" contract needs an explicit provider capability or a documented relaxation.

Honourable mention: per-request `PathCase::canonicalize` (`path_case.rs:126`). It does one `read_dir` per segment, which is cheap on a local disk and unaffordable over the network. It must be served from the SQLite index with NFC plus lowercase fold, and the index needs a "same folded name, different spelling" lookup for case-only rename.
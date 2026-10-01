# Spec: metadata compilation, library index, catalog, home (mokuro-bunko 0.5.2)

Source root: `src/mokuro_bunko/` (abbrev. `S/`). Tests: `tests/unit|integration`.
Citations are `file:line` relative to `S/` unless prefixed `tests/`.
"DROP" = Python/mokuro/ctd-specific, not needed in the Rust port. "OCR-DEP" = depends on the OCR subsystem spec (keep iff OCR queue/worker is kept).

Byte-compat target: the Mokuro Reader client parsers (`series-file.ts`, `catalog-file.ts`; per `metadata/schema.py:3-9`). Reader parsers ignore unknown keys and key order, but the server MUST be byte-deterministic because clients version caches on file size/mtime (contract section 4). A rebuild with no input change MUST produce identical bytes and MUST NOT touch the file.

---------------------------------------------------------------------------
## 1. Where files live

- Library root = `config.storage.library_path` (`<base>/library`). Served at `/mokuro-reader/...` (`PathMapper.READER_ROOT = "mokuro-reader"`, `webdav/resources.py:343`).
- `<library>/catalog.json` : root catalog (compiled). `metadata/service.py:218`.
- `<library>/<Series>/series.json` : one per series folder (compiled). `metadata/service.py:215`.
- A "series" = a TOP-LEVEL directory of the library that (a) does not start with ".", (b) is a dir (symlinks followed) and (c) directly contains at least one file whose name lower-cases to end `.cbz` (symlinks followed). Nested folders are NOT series. Folders sorted by raw name (`compiler.py:184-213`; test `test_nested_folders_are_not_series`, `test_a_missing_library_is_empty_not_an_error`).
- Per-user files (`volume-data.json`, `profiles.json`, `goals.json`) are NOT metadata (`paths.py:62-79`, `PathMapper.PER_USER_FILES`). A stale root `series-metadata.json` is inert.
- DB tables (SQLite) used here (`database.py:580-640, 681-698`):
  - `series_facts(series_key PK, series_title, external_ids JSON, titles JSON, synonyms JSON, tag, unit, facts_updated_at, spine_offset NUMERIC, volume_offsets JSON, updated_by, updated_at default datetime('now'))`
  - `series_entry_cache(volume_key PK, series_key, entry_json, cbz_size INT, cbz_mtime REAL, sidecar_key TEXT default '', computed_at)`; index `idx_series_entry_cache_series(series_key)`
  - `catalog_series(series_key PK, folder_name, cover_path, volume_count, latest_volume_modified REAL, total_pages, total_chars, missing_pages, damaged_volumes, scanned_at)` (missing_pages/damaged_volumes added by ALTER on old DBs, `database.py:700-710`)
  - `community_details(series_key PK, score REAL, tags JSON, genres JSON, source, fetched_at)`
  - `volume_identities(volume_key PK, volume_uuid, recorded_at)` (see 5)
  Open question Q1: does the Rust server share the existing SQLite file (then these schemas/cache-key formats are a compat surface) or start a new store?

### Identity folds
- `normalize_series_key(t)` = `re.sub(r"\s+"," ", t.strip()).lower()` (`reader_compat.py:118-147`).
- `normalize_volume_title_key(t)` = `normalize_series_key(NFC(t))` (`:150-157`). This is the key used for `series_facts.series_key`, `catalog_series.series_key`, `series_entry_cache.series_key`, and all folder resolution in the service.
- Known divergences from JS `trim/\s/toLowerCase` documented at `:122-146` (U+FEFF; U+001C-1F,U+0085; Python vs Node lowercase tables differ on 55 codepoints). Rust must pick Unicode-whitespace and lowercase rules explicitly; Python `str.strip()`/`\s` (Unicode `str.isspace` semantics) is the compat target for the server's own keys, JS is the target for placeholder uuids (Q2).
- `catalog.json` dedup/sort uses `normalize_series_key` WITHOUT NFC (`schema.py:221-237, 312-315`) while the DB key uses the NFC variant. Preserve both as-is.

---------------------------------------------------------------------------
## 2. `<Series>/series.json` exact format (version 2)

Emitter: `dump_series_file` (`metadata/schema.py:240-298`), serialisation `_dumps` (`:179-198`).

Serialisation rules (all pinned by tests/unit/test_metadata_schema.py):
- Compact separators `,` `:`; NO whitespace; no trailing newline. UTF-8, non-ASCII written RAW (`ensure_ascii=False`; `test_japanese_is_written_raw_not_escaped`).
- `allow_nan=False`: a NaN/Inf reaching the serialiser is an error (`test_non_finite_numbers_are_refused...`).
- A lone surrogate in a string is emitted as the literal 6-char escape `\udcff` (lowercase hex) (encode with `backslashreplace`; `schema.py:179-198`, `test_lone_surrogates_are_backslash_escaped_not_fatal`). Arises from non-UTF-8 folder names (Python `surrogateescape`). Rust: decide how to represent non-UTF-8 filenames (Q3). JSON string escaping otherwise follows Python json: `"`, `\`, and control chars < 0x20 escaped as `\n \r \t \b \f` or `\u00XX`; DEL and U+2028/2029 NOT escaped.
- Numbers: Python `repr` of floats, ints as ints (`-40` stays `-40`, never `-40.0`; offsets stored verbatim). A float that is integral serialises WITH `.0` (`"spine_offset":12.0`; accepted divergence from JS, `test_integral_float_spine_offset_keeps_the_decimal_point`). Float `repr` differs from JS/ryu for exponents (`1e-07` vs `1e-7`, `1e+16`). Rust must preserve int-vs-float of values parsed from JSON (serde_json Number does) (Q4).

Key order (fixed):
```
version, series_title, external_ids, titles, synonyms, [tag], [unit], [spine_offset], updated_at, volumes
```
volume entry key order:
```
volume_uuid, volume_title, page_count, [matched_page_count], character_count, mokuro_version,
[spine_width], [archive_size], [mokuro_size], [mokuro_modified], [mokuro_sha256],
[cover_size], [cover_modified], [offset]
```
Field semantics:
| key | rule |
|---|---|
| `version` | always `2` |
| `series_title` | the FOLDER name (not any .mokuro title) |
| `external_ids` | object; only keys from `("anilist","mal")` in that order, only present ones; values positive ints. Always present (maybe `{}`) (`schema.py:162-170`) |
| `titles` | object; keys from `("native","romaji","english")` in that order, only non-empty values. Always present |
| `synonyms` | array, always present (maybe `[]`), stored order, NOT re-filtered at dump |
| `tag` | only if `tag.strip()` non-empty; written STRIPPED |
| `unit` | only if exactly `"volumes"` or `"chapters"` |
| `spine_offset` | only if truthy (non-zero); written verbatim (int or float) |
| `updated_at` | facts stamp; `1970-01-01T00:00:00.000Z` when factless |
| `volumes` | array of volume entries, see below |

Volume entry:
| key | rule |
|---|---|
| `volume_uuid` | see 4 |
| `volume_title` | archive filename stem (`Path.with_suffix("")` = strip the LAST `.ext`; so `Vol 1.5.cbz` -> `Vol 1.5`) |
| `page_count` | see 4 |
| `matched_page_count` | present iff not None ("undetermined" => omitted, never `null`) (`:273-274`) |
| `character_count` | int |
| `mokuro_version` | string, `""` when no sidecar / not a string |
| `spine_width` | only if finite and > 0; passes through uncoerced (int stays int) |
| `archive_size` | only if > 0 |
| `mokuro_size`, `mokuro_modified` | present iff not None (0 is written). size = sidecar `st_size`; modified = `int(st_mtime)` (truncate) |
| `mokuro_sha256` | present iff exactly 64 lowercase hex |
| `cover_size`, `cover_modified` | present iff not None (0 written); from `<stem>.webp` stat, `int(st_mtime)` |
| `offset` | only if stored per-volume offset for that uuid is truthy; verbatim |

Volume order: sort key `(natural_sort_key(volume_title), volume_title)` AFTER dedup by `volume_uuid` (first occurrence in the input order wins; input order is the natural-sorted compile order) (`schema.py:201-218, 260-263`; `test_tied_natural_sort_keys_break_on_raw_title_text`, `test_duplicate_volume_uuid_keeps_only_the_first_occurrence`).

`natural_sort_key` (`reader_compat.py:163-205`): split title with `(\d+)` ; each non-empty part -> `(0, int(part))` if `part.isdecimal()` else `(1, casefold(strip_combining(NFKD(part))))`. Tuples compare lexicographically; shorter-prefix is smaller. `\d` = Unicode decimal digits (680 codepoints, UCD 15). Needs full Unicode `casefold`, NFKD and combining-class tests in Rust (not just `to_lowercase`).

Examples (verbatim from tests):
```json
{"version":2,"series_title":"Bakemonogatari","external_ids":{},"titles":{},"synonyms":[],"updated_at":"1970-01-01T00:00:00.000Z","volumes":[{"volume_uuid":"cfb5220c-57db-4008-9f44-e659d794e381","volume_title":"v01","page_count":187,"character_count":13247,"mokuro_version":"0.2.2","archive_size":1234}]}
```
```json
{"version":2,"series_title":"Dr Stone","external_ids":{"anilist":98416,"mal":103897},"titles":{"native":"Dr.STONE","romaji":"Dr. STONE"},"synonyms":["ドクターストーン"],"tag":"HD Scan","unit":"volumes","spine_offset":12.5,"updated_at":"2026-08-18T19:36:24.324Z","volumes":[{"volume_uuid":"u2","volume_title":"Volume 2","page_count":180,"character_count":9000,"mokuro_version":"0.2.2","spine_width":250.5,"archive_size":99,"mokuro_size":15000,"mokuro_modified":1700000100,"cover_size":4096,"cover_modified":1700000200},{"volume_uuid":"u10","volume_title":"Volume 10","page_count":200,"character_count":10000,"mokuro_version":"","offset":-40}]}
```
Full entry with matched count + hash (order): `..."page_count":187,"matched_page_count":180,"character_count":13247,"mokuro_version":"0.2.2","spine_width":250,"archive_size":1234,"mokuro_size":45210,"mokuro_modified":1723996800,"mokuro_sha256":"<64 hex>","cover_size":8192,"cover_modified":1723996900,"offset":7}`.
Factful-but-allowlist-empty case (pinned): external_ids `{"kitsune":7}` + unit `"chapters-ish"` still carries the real `updated_at` and writes `"external_ids":{},"titles":{},"synonyms":[]` (`test_has_facts_can_disagree_with_the_written_payload`).

---------------------------------------------------------------------------
## 3. Root `catalog.json` exact format (version 1)

Emitter `dump_catalog_file` (`schema.py:301-324`):
```
{"version":1,"updated_at":<newest entry stamp>,"series":[ {entry}, ... ]}
```
entry key order: `series_title, external_ids, titles, synonyms, [tag], [unit], updated_at` (same `_facts_payload` rules as series.json; NO spine_offset, NO volumes, NO hashes: `test_volume_data_never_leaks_into_the_catalog`).
- One entry per series folder (factless ones included, at epoch stamp).
- Sort key `(normalize_series_key(title), title)` (code point order); dedup by `normalize_series_key` BEFORE sort, first in input order wins (input order = folder name order).
- Top-level `updated_at` = max (string compare) of entry stamps, starting from `1970-01-01T00:00:00.000Z`; NEVER the clock. Empty library: `{"version":1,"updated_at":"1970-01-01T00:00:00.000Z","series":[]}`.
Example:
```json
{"version":1,"updated_at":"2026-08-18T19:36:24.324Z","series":[{"series_title":"Aria","external_ids":{},"titles":{},"synonyms":[],"updated_at":"1970-01-01T00:00:00.000Z"},{"series_title":"Dr Stone","external_ids":{"anilist":98416,"mal":103897},"titles":{"native":"Dr.STONE","romaji":"Dr. STONE"},"synonyms":["ドクターストーン"],"tag":"HD Scan","unit":"volumes","updated_at":"2026-08-18T19:36:24.324Z"}]}
```

Facts source for both files = the `series_facts` row for `normalize_volume_title_key(folder.title)`; missing row => `SeriesFacts()` (epoch, empty) and empty index (`service.py:130-140`). Stored row's `facts_updated_at` empty/NULL falls back to epoch (`service.py:696`).

---------------------------------------------------------------------------
## 4. Compiling a volume (`metadata/compiler.py`)

One entry per `*.cbz` (suffix match case-insensitive on `.cbz`; sorted by raw name; symlinks followed; dirs named like archives skipped via `is_file`) (`:216-225`). A sidecar without an archive is NOT a volume (`test_a_sidecar_without_an_archive_is_not_a_volume`); `.webp` covers and `.nocover` markers aren't volumes.

Sidecar choice (`:228-235`): `<stem>.mokuro` if it is a file, else `<stem>.mokuro.gz`, else none. Layer sidecars (`<stem>.<layer>.mokuro`) are never read for the entry (`test_an_ocr_layer_is_never_hashed_as_the_primary`). Note stem = `with_suffix("")`, and `Vol 1.5.mokuro` is the primary of `Vol 1.5`.

Sidecar load (`:280-307`): read bytes (gunzip if name ends `.gz`), `raw.decode("utf-8")` strict, `json.loads`; any of OSError/UnicodeDecodeError/ValueError/EOFError/zlib.error => unreadable; non-object JSON => unreadable. Unreadable => treated as image-only (below), NO hash. A leading BOM => decode OK but json.loads fails => unreadable. NOTE Python `json.loads` here accepts `NaN`/`Infinity` literals and duplicate keys (last wins) (Q5; serde_json rejects NaN). `mokuro_sha256 = sha256(raw).hexdigest()` over the (post-gunzip) bytes AS STORED, not re-serialised (`test_the_bytes_are_hashed_as_stored_not_re_serialized`, `test_the_same_json_hashes_the_same_plain_or_gzipped`). Hash only published when the sidecar parsed to an object; client-sent hashes are ignored (`test_a_client_sent_hash_is_ignored`). Hash is INDEX data, never moves the facts stamp.

Archive image listing `_archive_image_names` (`:310-344`): open zip central directory only; for each entry: skip `is_dir()` (name ends `/`), skip `is_system_file(name)`, skip when `trailing_extension(name)` (= `name.split(".")[-1].lower()` of the WHOLE path, quirk preserved) not in image set, skip when basename lower == `<archive_stem_lower>.webp` (embedded cover). Image ext set: `jpg jpeg png webp gif bmp avif tif tiff jxl` (`reader_compat.py:261-263`). System-file rules `:265-317`: any path segment starting `._`, ending `~`, or in {`__MACOSX .DS_Store .Trashes .Spotlight-V100 .fseventsd .TemporaryItems .Trash System Volume Information $RECYCLE.BIN Thumbs.db desktop.ini Desktop.ini RECYCLER RECYCLED .Trash-1000 .thumbnails .directory .dropbox .dropbox.cache .git .svn`}, or last-segment extension in `bak tmp temp`; backslashes treated as `/`. Result: `BadZipFile`/`EOFError` => `[]` (damaged: every page missing); other `OSError` => `None` (unknown; entry NOT cached).

Entry construction `_compile_volume` (`:386-486`):
- Image-only (no sidecar, or unreadable): `volume_uuid = deterministic_uuid(f"{series}/{volume_title}")`, `page_count = len(images)` (0 if archive unknown), `matched_page_count = page_count if archive readable else None`, `character_count 0`, `mokuro_version ""`, `archive_size = size or None`, `mokuro_size/modified` from the sidecar stat if a (corrupt) sidecar exists else None, no hash, no spine_width. (`test_a_corrupt_sidecar_degrades_to_image_only`, `test_image_only_row_records_none...`).
- Sidecar parsed:
  - `volume_uuid` = `data["volume_uuid"]` if a str with non-blank content (NOT trimmed in output), else the deterministic one.
  - `mokuro_version` = `data["version"]` if str else `""`.
  - `character_count` = `data["chars"]` if int (not bool) and > 0, else `count_page_chars(pages)` = sum over pages[].blocks[].lines[] (str lines only; malformed items skipped) of count of chars in the table `_COUNTED_RANGES` (`reader_compat.py:24-35`; port of client `countChars`: `[○◯々-〇〻ぁ-ゖゝ-ゞァ-ヺー]` + Script=Hiragana/Katakana/Han). Rust must reproduce this exact codepoint table (copy it; `tests/unit/test_metadata_reader_compat.py::TestCountChars` pins it).
  - `page_paths` = per page `img_path` if non-empty str else None; whole thing None if `pages` isn't a list OR no page has a usable img_path.
  - If `page_paths is None`: `page_count = len(pages) if pages is list else archive_image_count`; `matched_page_count = None` (omitted).
  - Else `page_count = len(page_paths)`; `matched_page_count = count_matched_pages(page_paths, image_names)` or None if archive unknown.
  - `spine_width` = `data["spine_width"]` if int/float (not bool) and > 0 (uncoerced), else None. (Dump drops non-finite.)
  - `archive_size = stat.size or None`; `mokuro_size`, `mokuro_modified=int(mtime)`; `mokuro_sha256`.
- Missing pages: `missing_page_count(page, matched) = 0 if matched is None else max(0, page - matched)` (`schema.py:109-126`). Not written to series.json; the catalog derives it. `matched` may exceed `page_count` (duplicate img_paths).

`count_matched_pages` (`reader_compat.py:360-425`), port of client `matchImagesToPages` counting half:
1. dedup archive paths preserving order; build `normalized_files[lower(path).replace("\\","/")]` and `stem_files[stem(path).lower()]` (later duplicates overwrite).
2. per page path: None => missing; exact normalized-path hit => matched (marks used; does NOT check used); else stem hit (`stem` = basename minus last ext with `lastDot > 0`) and that file not in `used` => matched; else missing.
3. `extra` = count of unique files not in `used`. If `missing>0 and missing==extra and matched/len(pages) < 0.5` => `matched += missing`.
(Tests `TestCountMatchedPages` cover exact, ext remap, case, backslash, fallback thresholds, duplicates.)

`deterministic_uuid(value)` (`reader_compat.py:91-115`): port of client `generateDeterministicUUID`. `normalized = value.lower().strip()`; iterate UTF-16 code units `u`: `h1 = to_int32(h1*33) ^ u` (start 5381), `h2 = to_int32(h2*33) ^ u` (start 52711), where `to_int32` wraps to signed 32-bit and the XOR result can be negative/ signed-int32-ish then masked `&0xFFFFFFFF` after the loop; `hex1/hex2` = 8-hex of each; `hash3 = hex(h1^h2)`, `hash4 = hex((h1+h2)&0xFFFFFFFF)`; `variant = hex(8 + int(hash3[0],16)%4)`; result `f"{hex1}-{hex2[:4]}-4{hex2[5:8]}-{variant}{hash3[1:4]}-{hash3[4:]}{hash4[:4]}"` (shape 8-4-4-4-8, quirk `hex2[5:8]` intentional). Input is `"<Series>/<volume stem>"`. Golden vectors in `tests/unit/test_metadata_reader_compat.py::TestDeterministicUUID`. Careful: `(h*33)` is done on the int32-wrapped value each step; port exactly.

### Entry cache (performance + semantics)
- Key `volume_key = f"{series}/{volume_title}.cbz"` (`:54-56`). Valid iff `cbz_size`, `cbz_mtime` (exact float) and `sidecar_key = f"{sidecar.name}:{st_size}:{st_mtime}"` (or `""` for no sidecar) all match (`database.py:2266-2295`). The cover stat is NEVER cached; re-statted every compile (`:261-277, 637-643`).
- Cached JSON fields: `volume_uuid, volume_title, page_count, matched_page_count, character_count, mokuro_version, spine_width, archive_size, mokuro_size, mokuro_modified, mokuro_sha256` (`_entry_to_dict`). Reads require keys `volume_uuid.. mokuro_modified` incl. `matched_page_count`, `mokuro_size`, `mokuro_modified` (missing key => miss => recompile = backfill); `mokuro_sha256` read with `.get`; a row without that KEY and with a sidecar gets its hash filled by re-reading just the sidecar (when `fill_hashes`, background passes only; request path PUT passes `fill_hashes=False`) and the row stored back; unreadable sidecar => leave unfilled, retry next pass (`:505-553, 556-635`).
- Not cacheable when the archive could not be opened this time (`image_names is None`).
- For Rust, a cache is an optimisation but pass semantics (no recompute of unchanged volumes, hash fill rules) are observable via tests; a different cache format is fine if Q1 resolves to "new store".
- OCR-DEP helpers on the cache: `cached_missing_pages`, `missing_pages_now`, `cached_page_count`, `cached_mokuro_sha256` (`:59-151`) feed the OCR worker/queue and the manifest (`cached_mokuro_sha256` is used by the manifest, see 8).

---------------------------------------------------------------------------
## 5. Stable volume ids

- Published id of a volume is whatever its primary `.mokuro` says (`volume_uuid`), else the deterministic path-derived id. It is never rewritten by bunko when a sidecar exists.
- `volume_identities` table (`database.py:681-698, 2054-2160`): remembers the id a primary `.mokuro` last carried, keyed by archive key (`S/V.cbz`; `.mokuro`, `.mokuro.gz`, `.webp`, `.nocover` paths map to the archive key, `database.py:285-302`). Written (a) whenever a compiled entry from a PARSED sidecar is put in the entry cache (identity only if `mokuro_sha256` present, or legacy row with `mokuro_size` and non-empty `mokuro_version`; image-only ids never stored) (`database.py:2054-2069, 2305-2312`), (b) when a primary sidecar is DELETED/moved while its archive remains (`webdav/resources.py:261-287`). `WHERE volume_uuid != excluded` upsert. Forgotten when the archive is deleted (`resources.py:886`) or folder deleted (`:1454`), renamed with the folder prefix (`:1431`), dropped on prefix removal (`:1434`).
- Consumer: `OCRProcessor.volume_uuid_for` (`ocr/processor.py:857-897`) order: primary sidecar's id -> remembered id -> an existing layer's id -> `deterministic_uuid("<series>/<stem>")`. OCR-DEP (so progress of a re-OCR'd volume keeps its id). The metadata compiler itself does NOT consult `volume_identities`.
- Deterministic placeholder equals the id the reader derives for image-only volumes so synced progress attaches.

---------------------------------------------------------------------------
## 6. Client-submitted series facts: PUT `<Series>/series.json`

### Routing / interception (`metadata/middleware.py`, `paths.py`)
- Only `PUT` with a path that is exactly `/mokuro-reader/<Series>/series.json` (one folder level; file name matched case-insensitively `series.json`; `<Series>` non-blank) is intercepted; every other method/path passes through (`middleware.py:54-64`, `paths.py:62-112`). Nested `A/B/series.json` and root `series.json` are NOT intercepted (they're ordinary files). Root `catalog.json` (case-insensitive) is the catalog path; nested `catalog.json` is an ordinary file.
- Path canonicalisation BEFORE matching: `posixpath.normpath("/" + path.strip("/"))` on the FULL path (collapses `//`, `.`, `..`) then require prefix `/mokuro-reader/` (`paths.py:62-79`); remainder in PER_USER_FILES excluded. Rust must apply the same lexical normalisation (so `/mokuro-reader//catalog.json`, `Dr Stone/./series.json`, `/x/../mokuro-reader/...` etc. all match; a `..` escape above root matches nothing). Tests: `test_metadata_paths.py::TestPathAliasNormalization, TestBoundaryDoubleSlashBypass`; integration `TestBoundarySlashBypass`.
- WSGI path is latin-1-decoded PATH_INFO; re-encode `path.encode("iso-8859-1").decode("utf-8")`, fall back to original on error (`paths.py:115-135`). (Rust: use the real percent-decoded UTF-8 path; N/A if not WSGI.)
- Middleware sits INSIDE auth (actor known, `environ["mokuro.username"]`), OUTSIDE the PROPFIND cache/DAV app (`server.py:309-314`). Order of checks and responses (text/plain; charset=utf-8, body = message):
  1. no service configured: 403 `Metadata files are compiled by the server`
  2. no/empty username: 401 `Authentication required`
  3. `CONTENT_LENGTH` absent: 411 `Content-Length required` (body not read)
  4. non-integer or <0: 400 `Invalid Content-Length`
  5. > 4 MiB (`4*1024*1024`): 413 `Metadata update too large` (body not read)
  6. read exactly Content-Length bytes
  7. `service.apply_series_update` raises Busy (pass lock not acquired in 10 s): 503 `Server is busy compiling metadata; retry shortly` + header `Retry-After: 30`
  8. audit (below)
  9. rejected: 400 `Invalid metadata update`; accepted: `204 No Content`, empty body (no Content-Type).
  (Test `test_a_missing_content_length_is_400` is misnamed: it sends garbage => 400; truly absent => 411.)
- Audit event (`middleware.py:130-145`): `log_audit_event(action="metadata_update"|"metadata_rejected", actor_username=user, target_type="library", target_path=<decoded virtual path>, details={"accepted": bool})`, failures swallowed (stderr `[METADATA] audit failed: ...`). Not logged for 401/403/411/400-length/413/503-busy-before... (503 returns before audit).
- Authorization (`middleware/auth.py:795-850`, `login/api.py:244-258`): anonymous 401; role with MODIFY_DELETE (inviter/editor/admin) -> any series; `uploader` -> only if `Database.can_user_edit_series(user, series)` (user is the sole recorded uploader of EVERY tracked volume in that series, folded by series-title key; untracked series = false); `registered` -> 403 `Permission denied: cannot submit metadata updates for this series`. Even if authorised, the service refuses PUT for a title whose folder doesn't exist now (400 from middleware).
- All other write verbs on compiled paths (DELETE, MOVE, COPY, PROPPATCH, MKCOL, LOCK, UNLOCK, and PUT to `catalog.json`; and MOVE/COPY with a compiled `Destination`): anonymous 401, everyone else 403 `Permission denied: this file is compiled by the server` (`auth.py:764-783, 600-623`). Deleting the series FOLDER stays allowed. Reads are normal library reads. `/login/api/me` advertises `permissions.metadata = {"scope":"all"} | {"scope":"owned","ownedSeries":[...]} | {"scope":"none"}`.
- Upload middleware does not give JSON verdicts for compiled paths (`middleware/upload.py:162`).

### Validation (`metadata/validate.py`) - untrusted body
`parse_series_update(bytes)` -> `None` (=> 400) or a `SeriesUpdate`:
1. utf-8 decode strict + JSON parse; `NaN/Infinity/-Infinity` constants rejected; must be an object. (Python accepts duplicate keys; last wins; arbitrarily large ints.)
2. `version` must be int 1 or 2 (bool rejected; `1.0`?? Python `1.0 in (1,2)` is True -> accepted; serde: decide, Q6).
3. `updated_at`: str -> `normalize_updated_at` else reject. Rule (`reader_compat.py:208-253`): strip; empty => reject; trailing `Z` -> `+00:00`; `datetime.fromisoformat` (Python 3.11+ grammar; broader than RFC3339, e.g. date-only, basic format, space separator) else reject; offsetless => UTC; if more than 300 s in the future => clamp to NOW; output `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC, ms truncated). String order == chronological order afterwards (years 0001-9999 only).
4. Facts: `external_ids`: for key in (anilist, mal) value must be int, not bool, > 0 (`98416.0` rejected). `titles`: for (native, romaji, english) non-blank str, kept UNTRIMMED. `synonyms`: list; keep str items that are non-blank, UNTRIMMED, order kept, duplicates kept. `tag`: stripped, blank => None. `unit`: exactly "volumes"/"chapters" else None. Unknown top-level keys, `series_title` and everything in `volumes` except `volume_uuid`/`offset` are ignored.
5. `spine_offset`: int/float (not bool), finite, non-zero => present (stored verbatim, no clamp; huge ints that overflow float conversion => dropped); else absent (silence, NOT reset). 0 == absent.
6. `volumes[]`: dict entries with non-blank str `volume_uuid`; first occurrence of a uuid wins; every listed uuid goes in `listed_uuids`; `offset` same validity as spine_offset recorded in `volume_offsets`.
Tests: `tests/unit/test_metadata_validate.py`.

### Merge (`metadata/merge.py`)
- FACTS (decided by `updated_at` string compare, stored stamp = row `facts_updated_at`): stored None => incoming. Incoming `has_facts()` (any raw external_ids/titles, any non-blank synonym, non-blank tag, truthy unit) => incoming wins iff `incoming.updated_at >= stored.updated_at` (ties keep incoming), else keep stored. Incoming factless => wins only if STRICTLY newer (explicit unlink), else stored kept. Winner replaces ALL fact fields wholesale (no field-level merge).
- INDEX (spine/volume offsets) merges by presence, independent of facts and never touches the facts stamp: `spine_offset` replaced iff present in payload, else inherited; for each listed uuid: set offset if given, else delete stored offset; unlisted uuids untouched (stale uuids for deleted volumes are retained).
- `changed = facts_changed or index_changed` where changed = `stored is None` or structural inequality. Tests `tests/unit/test_metadata_merge.py`.

### Service `apply_series_update(series_title, payload, actor)` (`metadata/service.py:452-541`)
1. no-op `False` after stop(); `series_key = normalize_volume_title_key(title)`; empty => False; parse fails => False.
2. Acquire `_pass_lock` with timeout `update_lock_timeout_seconds` (10 s) else raise Busy (=> 503).
3. Under lock: resolve the real folder whose `normalize_volume_title_key(folder.title) == series_key` (fresh scan; library root unreadable or no folder => return False, NOTHING stored). Stored row's `series_title` = FOLDER spelling, not the PUT's.
4. read stored, merge; `ids_changed = bool(new ids) and new ids != old ids` (an unlink does not nudge).
5. If `stored is None or result.changed` => upsert the row (incl. `updated_by=actor`, bookkeeping `updated_at=now`). Note the first PUT for a series is always stored even if factless/older.
6. republish this series + catalog (`_regenerate_series_locked(title, fill_hashes=False)`); an exception there is logged (`[METADATA] republish failed after an accepted update: ...`), full pass rescheduled in 5 s, and the PUT is STILL accepted (`True`).
7. Release lock. If `ids_changed` call `on_external_ids_changed(series_key)` (community fetch nudge). Then `_published(changed)` => `on_published` hook if >0 files changed. Return True.
- Accepted != changed: a losing/duplicate payload returns 204 with no rewrite (integration `test_retrying_the_same_put...touches_nothing` checks mtime unchanged).
- Rows are never deleted when a folder disappears; a re-appearing/renamed-back folder finds its row again (module docstring).
- `on_published` (`server.py:255-270`): invalidate library index, PROPFIND cache refresh in 5 s, queue page `invalidate_skipped` (OCR-DEP). It must NOT trigger a regeneration.

---------------------------------------------------------------------------
## 7. Recompile triggers, debounce, locking, atomic writes

Triggers (`server.py`):
- Startup: `schedule_regeneration(delay=20.0)` (`:451`).
- Periodic: `start_periodic_rescan(6*3600 s)` re-arms itself then calls `schedule_regeneration()` (debounced) (`service.py:596-620`).
- Filesystem watcher (watchdog, recursive on library) -> `on_library_change(path)` (`server.py:~430`): invalidates library index; PROPFIND refresh in 5 s; then `classify_change(library, path)` (`middleware/fs_watcher.py:43-65`): path outside root or `len(parts)<2` (top-level entry/folder appears/disappears/moves) => `"library"` => `schedule_regeneration()` (full pass; only this prunes deleted series); first part `thumbnails` => ignore; else `("series", parts[0])` => `schedule_series_regeneration(parts[0])`. Watcher relevance: any directory event; files with suffix `.cbz .mokuro .gz .webp` (and names ending `.mokuro.gz`); `.json` deliberately NOT watched (prevents self-trigger); moves deliver both src and dest (`fs_watcher.py:21-40`, tests `test_fs_watcher.py`).
- Client `series.json` PUT: synchronous republish of that series (+catalog) as in 6.
- Self-reschedules: `schedule_regeneration(delay=5.0)` on library root unreadable, busy path lock, unwritable folder/catalog (`service.py` 297,344,351,366,369,409,432,441,446,449).

Debounce (`service.py:543-594`): defaults `debounce_seconds=10`, `max_debounce_seconds=60`. Full-pass timer: first schedule sets `deadline = now+60`; each later call cancels and restarts with `min(base, max(0, deadline-now))` where `base = delay if given else 10`; fire clears timer+deadline. Per-series timers identical but keyed by `normalize_volume_title_key(title)` (latest raw title remembered in `_series_titles`). Fire logs `[METADATA] full pass fired` / `done (changed=N)` / `series regen fired: <title>`. Tests `TestDebounce`.

Locks (`service.py:107-120`): `_pass_lock` (per-series critical section; protects read-merge-persist-republish), `_full_pass_lock` (full passes never interleave), `_timer_lock`. A full pass takes `_pass_lock` PER SERIES with a 1 ms sleep between series so waiting PUTs get in (`:306-313`), re-checks `_stopped` each series (abort => skip prune+catalog). `stop()` sets stopped, cancels/joins timers, then acquires/releases `_full_pass_lock` and `_pass_lock` so it returns only when idle. After stop, all public entry points are no-ops. Hooks (`on_published`, `on_external_ids_changed`) are fired OUTSIDE `_pass_lock` (re-entrancy safe, `TestReentrantPublishHook`).

Full pass `regenerate_all` (`:273-371`): scan folders (library root unreadable => log `library root unreadable, skipping pass: <path>`, reschedule 5 s, return 0). For each folder (name order): lock; load facts row; `compile_series_volumes(fill_hashes=True)`; add to `keep` set of volume keys; `_materialize_catalog_row`; `_publish_series`. After all (not aborted): prune `series_entry_cache` to `keep`, prune `catalog_series` to folder keys, publish catalog. Returns count of files changed; `on_published` fired once if >0.
Single-series pass `regenerate_series` (`:373-450`): scans folders anyway to build the full catalog entry list (all facts rows), recompiles only the matching series (matched by `normalize_volume_title_key`), publishes series + catalog. Does not prune.

Publishing (`metadata/files.py`):
- `write_if_changed(path, data)`: if `path.read_bytes() == data` => no write, return False. Else acquire the DAV per-path write lock (`path_write_lock`, `webdav/resources.py:308-322`; busy ancestor/path => DAVError 423 => `MetadataWriteBusy`, skipped and rescheduled in 5 s) then `atomic_write_bytes`.
- `atomic_write_bytes`: mkdir parents; `mkstemp(prefix=f".{name}.compile-", suffix=".tmp", dir=<same dir>)`; write; flush; fsync; `os.replace`; cleanup temp on error; on POSIX chmod `0o666 & ~umask` (mkstemp's 0600 would break nginx-served downloads). No dir fsync.
- `_publish_series` skips (returns False) if the folder no longer exists at write time (don't resurrect deleted folders; `service.py:186-215`). OSError (e.g. a directory squatting at `series.json`/`catalog.json`) => log `skipped unwritable series folder: <t>: <err>` / `skipped unwritable catalog.json: <err>`, continue, reschedule 5 s.
- The compiled file's mtime/size are what clients key on, hence no-op rewrites forbidden.

`catalog_series` materialisation (`service.py:220-265`): per series one directory scan: `latest_volume_modified` = max `.cbz` st_mtime (float); covers = set of file names ending `.webp` (case-insens suffix, exact name stored); `cover_path` = `"<folder>/<volume_title>.webp"` for the FIRST volume (natural order) whose `<volume_title>.webp` exists in that set, else None; `volume_count=len(volumes)`, `total_pages=sum(page_count)`, `total_chars=sum(character_count)`, `missing_pages=sum(missing_pages)`, `damaged_volumes = count(missing_pages>0)`. Row key `normalize_volume_title_key(folder)`, `folder_name` = folder spelling, `scanned_at=now`. Test `TestCatalogMaterialization`.

---------------------------------------------------------------------------
## 8. `library_index.py` (LibraryIndexCache)

Used by: catalog `/catalog/api/series`, library fallback, home counts, health `pending`, OCR queue. Snapshot (frozen):
- `LibrarySnapshot{series: [SeriesSnapshot], pending_ocr: [(series, volume)], pending_thumbnails: int}`; `SeriesSnapshot{name, cover, volumes}`; `VolumeSnapshot{name, has_cbz, has_mokuro, has_mokuro_gz, cover, sidecars: sorted layer ids}`.
- Scan (`:139-230`): `os.walk(library)`, subdirs sorted, dirs starting with "." skipped (and their subtrees); current dir's name = relative posix path (so nested series names like `A/B`; the root itself has name `.` and is skipped since it starts with "."). Volumes = every `*.cbz` (suffix check on lowercase; stem = `name[:-4]`) in that dir; sidecar-only stems are NOT volumes. A dir with >=1 volume => a series entry; entries ordered by walk order (parent before children, siblings sorted by name; volumes sorted by stem, code-point order).
- `has_mokuro`/`has_mokuro_gz` = exact `<stem>.mokuro` / `<stem>.mokuro.gz` names present (case-sensitive here, unlike cbz suffix). `cover` = `"<series>/<stem>.webp"` if exact file present; series cover = first volume (sorted) with a cover.
- `pending_ocr` += (series, stem) for each volume with cbz and neither `.mokuro` nor `.mokuro.gz`. `pending_thumbnails` += 1 for each cbz lacking both `<stem>.webp` and `<stem>.nocover`.
- Layers (`sidecars`): for every file name, `split_layer_sidecar(name)` (`ocr/generations.py:1089-1113`): strip `.gz`, require `.mokuro`, split middle on LAST `.`, `cut>0`, layer id must match `^[a-z0-9-]{1,32}$` (LAYER_ID_RE); skip if `"<stem>.<layer>"` is itself an archive stem in the dir (decimal volume numbering `Vol 01.5`); group by stem. (Layer concepts are OCR-DEP.)
- OSError during walk or missing root => empty snapshot.
- Caching: TTL 30 s. `invalidate()` marks stale; if the last scan took < 0.1 s (`SLOW_SCAN_SECONDS`) the snapshot is dropped (next read rescans immediately), else the old snapshot is served until its age >= 4 x last scan duration (`RESCAN_FACTOR`). `get_snapshot_counted()` returns (snapshot, scans) atomically; `cached_snapshot()` never scans. Tests `test_library_index*.py`.

---------------------------------------------------------------------------
## 9. Catalog (`catalog/`)

Wiring: `CatalogAPI(app, storage_base_path=<library>, catalog_config, library_index, database, read_gate=auth_middleware, layer_order=lambda: [non-primary generation names], ocr_control)` (`server.py:346-355`). Config `CatalogConfig`: `enabled=False`, `reader_url="https://reader.mokuro.app"`, `use_as_homepage=False`, `enrich_community=True` (`config.py:205-212`); `enabled` is read live.

Routing (`catalog/api.py:106-169`), path = PATH_INFO:
- `GET /catalog/api/manifest` is served even when the catalog is disabled (gate = the archive's own read gate).
- Catalog disabled => pass through for everything else.
- `/catalog` or `/catalog/` => static `index.html`; `/catalog/api/library` GET; `/catalog/api/config` GET; `/catalog/api/ocr-status` GET; `/catalog/api/series?name=<n>` (missing => 400 `{"error":"Missing series name"}`) and `/catalog/api/series/<urlencoded name>` GET; `/catalog/api/cover?path=<p>` (missing => text 400 `Missing cover path`) and `/catalog/api/cover/<path>` GET; any other `/catalog/api/*` => 404 `{"error":"Not found"}`; `/catalog/<file>` => static (unknown file falls back to `index.html`; traversal 403). Static: `Cache-Control: no-cache`; MIME table `catalog/api.py:41-50`. Static assets in `catalog/web/` (catalog.js, index.html, styles.css) are part of the product surface; copy them.
- JSON responses use Python default `json.dumps` => separators `", "`/`": "` and `ensure_ascii=True` (non-ASCII as `\uXXXX`), `Content-Type: application/json`, status text map {200,400,403,404,500}. Gzip: when `environ` supplied (only `/library` and `/manifest` pass it), body >= 512 bytes and `Accept-Encoding` contains `gzip` => `gzip` level 6 + headers `Content-Encoding: gzip`, `Vary: Accept-Encoding`. Header order: [encoding headers], Content-Type, Content-Length, [extra]. Error text responses are `text/plain`.

### GET /catalog/api/config
`{"reader_url": "https://reader.mokuro.app"}` (live config value).

### GET /catalog/api/library (slim root payload)
Primary path (when `catalog_series` has rows; ordered by `folder_name` binary):
```json
{"series": [
  {"name": "Dr Stone", "path": "Dr Stone", "cover": "Dr Stone/Volume 01.webp",
   "volume_count": 2, "latest_volume_modified": 1723996800.5,
   "total_pages": 387, "total_chars": 20000, "missing_pages": 0, "damaged_volumes": 0,
   "titles": {"native": "Dr.STONE"}, "tag": "HD Scan",
   "community": {"score": 80.0, "tags": ["Science"], "genres": ["Adventure"], "source": "anilist"}}
]}
```
Key order: name, path, cover (null if none), volume_count, latest_volume_modified, total_pages, total_chars, missing_pages, damaged_volumes, then optional `titles` (only if the facts row has non-empty `titles` dict; whole stored dict, same allowlisted keys), `tag` (only if non-blank string; stored value), `community` (only if a `community_details` row exists). Facts lookup by `series_key` from `list_series_facts`; community from `list_community_details` (DB errors => silently omitted). `name`==`path`==folder name.
Fallback when the table is empty (first boot) and a library index exists: per snapshot series `{"name","path","cover","volume_count"}` (+titles/tag), no totals. If neither: `{"series": []}`.

### GET /catalog/api/series?name=... | /series/<name>
Exact match against the index snapshot (`series_by_name`, so nested names allowed; path escape => 403 `{"error":"Forbidden"}`; unknown => 404 `{"error":"Series not found"}`; no index/base path => 404 `{"error":"Not found"}`). Response:
```json
{"name":"Dr Stone","cover":"Dr Stone/Volume 01.webp","volumes":[
 {"name":"Volume 01","cover":"Dr Stone/Volume 01.webp","ocr_pending":false,"ocr_active":false,"page_count":187,"missing_pages":0},
 {"name":"Volume 02","cover":null,"ocr_pending":false,"ocr_active":true,"ocr_progress":{"percent":..,"eta_seconds":..,"status":..,"processed_pages":..,"total_pages":..}}
]}
```
- per volume key order: name, cover, ocr_pending, ocr_active, [ocr_progress], [page_count, missing_pages].
- `ocr_pending = has_cbz and not has_mokuro and not has_mokuro_gz`; if the volume is the active OCR job: `ocr_active:true`, `ocr_pending:false`, `ocr_progress` = those 5 fields from the matching progress entry. OCR-DEP: progress read from `<storage_base>/.ocr-progress.json` (parent of library dir) (`api.py:473-528`); active only when `data["active"]` truthy; entries from `data["jobs"]` list or the file itself; match `relative_cbz.casefold() == f"{series}/{volume}.cbz".casefold()`.
- `page_count`/`missing_pages` are READ BACK from the compiled `<series>/series.json` (not recomputed): map `volume_title -> (page_count, missing_page_count(page_count, matched_page_count))`; entries with non-str title or non-int page_count skipped; missing/corrupt file => no damage keys (`api.py:409-438`; tests `test_series_endpoint_reports_missing_pages_from_the_compiled_file`, `..._renders_without_a_compiled_file`, `..._ignores_a_corrupt_compiled_file`).
- series `cover` = first volume with a cover.
- `GET /catalog/api/ocr-status`: OCR-DEP; `{"active": false}` or the raw progress JSON file object.
- `GET /catalog/api/cover?path=...`: resolve under library (403 `Forbidden` on escape, 400 `Invalid path`), 404 if not a file, only `.webp .jpg .jpeg .png` (else 403), body raw bytes, `Cache-Control: public, max-age=3600`, errors `text/plain`.

### GET /catalog/api/manifest?series=&volume= (always on)
Per-volume file manifest for reader deep links (`catalog/api.py:171-252`, `catalog/manifest.py`).
Order of decisions: need `read_gate` + base path else 404 `{"error":"Not found"}`; missing `series`/`volume` => 400 `{"error":"Missing series or volume"}`; `gate_read(environ, ..., "/mokuro-reader/<series>/<volume>.cbz")` (same auth/anonymous-download/rate-limit/401-challenge as a GET of that archive); resolve paths (OSError => 400 `{"error":"Invalid path"}`); escape outside library => 403 `{"error":"Forbidden"}`; series_dir == library root, or volume containing `/` or `\` => 404 `{"error":"Volume not found"}`; no `<volume>.cbz` file => 404 `{"error":"Volume not found"}`. Success 200 with `Cache-Control: no-store` (and gzip if accepted).
Document (`manifest.py:147-160`, `tests/unit/test_catalog_manifest.py::test_a_full_volume`):
```json
{"version":1,"series":"Dr Stone","volume":"Dr Stone 01",
 "archive":{"url":"/mokuro-reader/Dr%20Stone/Dr%20Stone%2001.cbz","size":123,"modified":"2026-01-01T00:00:00Z"},
 "ocr":{"url":"/mokuro-reader/Dr%20Stone/Dr%20Stone%2001.mokuro","size":45,"modified":"...","sha256":"<hex>"},
 "layers":[{"id":"hayai-nova-ppocr","url":"...","size":67,"modified":"..."}],
 "cover":{"url":"...","size":8,"modified":"..."},
 "series_file":{"url":"/mokuro-reader/Dr%20Stone/series.json","size":9,"modified":"..."},
 "pending":[],"recheck_after":null}
```
- `ocr`: `<volume>.mokuro` if a file else `<volume>.mokuro.gz` if a file else null; `ocr.sha256` is added ONLY when `cached_mokuro_sha256` for that archive is current (served from cache, never computed on request).
- `cover` = `<volume>.webp` if file else null; `series_file` = `series.json` in the series dir if a file else null.
- `layers`: files named `<volume>.<id>.mokuro[.gz]` with valid layer ids (split on last dot, `LAYER_ID_RE`), plain preferred over `.gz` for the same id; a file belongs to the LONGEST archive stem + "." (names that start with another longer archive's `<stem>.` are excluded). Order: ids in `layer_order` (config's non-primary generation names, in order) first, remaining ids alphabetical.
- URL encoding: each segment `urllib.parse.quote(utf8_bytes, safe="!*'()")` i.e. JS `encodeURIComponent`-compatible (unreserved `A-Za-z0-9_.-~` + `!*'()`); `modified` = `st_mtime` UTC `%Y-%m-%dT%H:%M:%SZ` (seconds, no ms); `size` = st_size.
- `pending`/`recheck_after` OCR-DEP: `control.volume_pending(...)` (list of dicts with `eta` ISO strings) else `[]`; `recheck_after = None` if empty else `ceil(min(eta)-now)+margin` clamped [MIN,MAX] or UNPRICED const (`ocr/volume_outlook.py:68-89`). With OCR dropped: `pending:[]`, `recheck_after:null`.
- Manifest URL builder `manifest_url(series, volume)` = `/catalog/api/manifest?series=<enc>&volume=<enc>` (used by catalog.js links).

---------------------------------------------------------------------------
## 10. Community enrichment (`catalog/community.py`)

Started only if `config.catalog.enabled and config.catalog.enrich_community` (`server.py:459-466`); nudge hook `metadata_service.on_external_ids_changed = fetcher.request_fetch`.

- Candidates: from `series_facts` rows, skipping keys with a `community_details` row fresher than 7 days (`REFRESH_AGE`, `fetched_at` parsed ISO with `Z`; unparsable => not fresh) unless `force`; `only` restricts. Row with `external_ids.anilist` int => AniList batch; elif `external_ids.mal` int => Jikan ("mal-only"). (An AniList-linked series never uses MAL.)
- AniList: `POST https://graphql.anilist.co`, JSON body `{"query": Q, "variables": {"ids": [ids...]}}`, batches of up to 50 sorted ids (`ANILIST_BATCH_SIZE`). Query verbatim:
```
query ($ids: [Int]) {
  Page(page: 1, perPage: 50) {
    media(id_in: $ids, type: MANGA) {
      id
      meanScore
      genres
      tags { name rank }
    }
  }
}
```
  Response `payload["data"]["Page"]["media"]`; each media matched by `id` to a series key (ids not returned are simply not stored). Normalise: `score = float(meanScore)` or None (already 0-100); `genres` = non-empty strings; `tags` = names with numeric `rank >= 40` (TAG_RANK_FLOOR) in response order, first 10 (TAG_LIMIT). `source="anilist"`.
- Jikan: `GET https://api.jikan.moe/v4/manga/{mal_id}` one per series, sorted by series key; `payload["data"]`; `score = round(float(score)*10, 1)` or None; `genres` = names from `genres`, `themes`, `demographics` in that order; `tags=[]`; `source="mal"`.
- Request details (`_build_request`, `_http_json`): headers `Accept: application/json`, `User-Agent: mokuro-bunko/<package version or "dev">` (default Python-urllib UA gets 403 from Cloudflare), POST adds `Content-Type: application/json`; timeout 30 s; no retries within a cycle; any exception on a batch/series => logged `[COMMUNITY] AniList batch failed (N ids): err` / `Jikan fetch failed (mal ID): err` and skipped (retried next cycle).
- Rate limit: `request_gap_seconds = 2.0` pause after every AniList batch and after every Jikan fetch (interruptible by stop). No 429/Retry-After handling (Q7).
- Store: `upsert_community_details(series_key, score, tags, genres, source, fetched_at=utc now "%Y-%m-%dT%H:%M:%SZ")`; `tags`/`genres` JSON via `json.dumps` (default ensure_ascii). Success log `[COMMUNITY] updated community details for N series`.
- Loop (`:280-319`): thread `community-fetcher` daemon; first full cycle after 60 s; then every `poll_seconds=3600`. `request_fetch(key)` adds to a nudge set and wakes the loop immediately (even during the 60 s delay) => `run_once(only=keys, force=True)`; a nudge doesn't reset the full-cycle timer. Exceptions logged `nudge cycle failed:` / `cycle failed:`. `stop()` sets events, joins 5 s.
- Out of scope but note: community data is exposed ONLY via `/catalog/api/library` `community` key.

---------------------------------------------------------------------------
## 11. Home (`home/api.py`)

`HomePageAPI(app, catalog_config, database, library_index, storage_path=<base>, ocr_backend=config.ocr.backend, ocr_poll_interval=config.ocr.poll_interval)` (`server.py:392-400`). Handlers in order:
1. `/api/health`: GET => health; OPTIONS => `204` with `Allow: GET, OPTIONS`; other => 405 `{"error":"Method not allowed"}`.
2. `/api/stats`: same method handling.
3. `GET /_home/<file>` => static from `home/web/` (home.js, index.html, styles.css): `..` or leading `/` => 404 `{"error":"File not found"}`; escapes => 403 `{"error":"Forbidden"}`; missing => 404; `Cache-Control: no-cache`; MIME table (html, js, css, json, png, ico, svg, woff2, woff; else `mimetypes`, else octet-stream).
4. `GET /` browser request with catalog enabled AND `use_as_homepage` => `302 Found` `Location: /catalog/`.
5. `GET /` browser request => `index.html` from home static.
6. else pass through.
`is_browser_request`: `Accept` contains `text/html` => true; else if User-Agent (lowercased) contains any of `davfs cadaver cyberduck webdav gvfs nautilus finder microsoft-webdav litmus` => false; else if the `Depth` header value contains the substring "Depth" (effectively never; source quirk: it tests `"Depth" in environ["HTTP_DEPTH"]`) => false; else true.

JSON via default `json.dumps` (spaces after separators, ASCII-escaped), headers Content-Type + Content-Length only.
`GET /api/stats` (never 500; each count degrades to 0 on error):
```json
{"total_users": 3, "total_volumes": 10, "total_pages_read": 0, "total_characters_read": 0,
 "total_reading_time_seconds": 0, "total_reading_time_formatted": "0s", "last_updated": 1760000000}
```
`total_users` = users whose `status != "deleted"`; `total_volumes` = sum of volumes over index snapshot series; `last_updated` = `int(time.time())`.
`GET /api/health`:
```json
{"status":"ok","uptime_seconds":12,"db_status":"ok","library_status":"ok","total_users":3,"total_volumes":10,
 "ocr":{"backend":"...","worker_alive":true,"pending":0,"failed":0}}
```
`status` = `"ok"` unless a DB/library probe raised (then `"degraded"`, still HTTP 200). `db_status`/`library_status`: `"unavailable"` when the dependency is None (counts null), `"ok"`, or `"error"`. `uptime_seconds = int(now - start)` (start at construction). `ocr` (OCR-DEP) = `null` if no storage path; `{"backend":"skip","worker_alive":null,"pending":null,"failed":0}` when backend is `skip`; else worker_alive from `<base>/.ocr-heartbeat` (float epoch text; alive iff age < max(poll_interval*4,120); unreadable => null), `failed` = number of keys in `<base>/.ocr-failures.json` object (0 on error), `pending = len(snapshot.pending_ocr)` (null on error). Tests `tests/integration/test_home_stats.py`, `test_home_page.py`.

---------------------------------------------------------------------------
## 12. DROP / OCR-DEP summary
- DROP: nothing in these modules is mokuro-the-Python-library or ctd specific; the `.mokuro` JSON format itself (parsed fields: `version`, `volume_uuid`, `chars`, `pages[].img_path`, `pages[].blocks[].lines[]`, `spine_width`) is a reader-compat surface and MUST be kept. `ocr.processor.OCRProcessor.get_cover_path` is just `cbz.with_suffix(".webp")` (`ocr/processor.py:510`).
- OCR-DEP (keep only if OCR worker/queue is ported): `cached_missing_pages`, `missing_pages_now`, `cached_page_count` (`compiler.py:59-138`), `volume_identities` consumer `volume_uuid_for`, catalog `ocr_pending/ocr_active/ocr_progress`, `/catalog/api/ocr-status`, manifest `pending`/`recheck_after`/layer ordering, health `ocr` block, `queue_api.invalidate_skipped` in on_published. Detect-only data (`pending_ocr`, `sidecars`) in LibraryIndex feed these.

## 13. Open questions
- Q1: Shared SQLite DB with the Python server (migration/compat of `series_facts`, `series_entry_cache`, `catalog_series`, `community_details`, `volume_identities`) vs a fresh Rust store? Cache keys embed Python float `repr` of mtimes (`sidecar_key`) and `cbz_mtime REAL` exact compare.
- Q2: Unicode folding parity: `str.lower()`/`strip()` (Python UCD 15) vs JS vs Rust `to_lowercase`; `casefold`+NFKD for natural sort need `unicode-normalization`/ICU-like data. Is bit-parity with Python acceptable, or with Node (client)? They differ (documented at `reader_compat.py:122-146`).
- Q3: Non-UTF-8 folder/file names: Python maps to lone surrogates and emits `\udcXX` in JSON; Rust `OsString` has no equivalent. Define behaviour (skip, lossy, or replicate escape bytes).
- Q4: Float formatting in series.json (`spine_offset`, `offset`, `spine_width`): Python `repr` + keep-`.0` vs ryu/JS. Matters only for byte-identical republish of unchanged data and for any existing compiled files on disk (a format change rewrites every series.json once and changes mtime/size on all clients). Need a decision: replicate Python repr exactly, or accept one-time rewrite.
- Q5: Python `json.loads` leniency on `.mokuro` (NaN/Infinity, lone surrogates `\ud800`, duplicate keys last-wins, arbitrary-size ints) vs serde_json strictness changes which sidecars are "parsed" (hash/uuid/page counts) vs image-only. Replicate lenient parser?
- Q6: PUT validation edge: `version: 1.0` and `external_ids` `98416.0` (Python accepts `1.0 in (1,2)` but rejects `98416.0` for ids; client JS accepts both?). Pin desired behaviour. Also `fromisoformat` grammar breadth (date-only, `20260818T193624Z`, space separator) vs a stricter RFC3339 parser; and clamping (>5 min future => now).
- Q7: Community fetcher: no 429/backoff or Retry-After handling and no per-host separation (AniList 90 req/min, Jikan 3/s 60/min); keep as-is (2 s gap) or add handling? Also tags/genres localisation and `meanScore` of 0 treated as a score.
- Q8: `/api/health` + catalog JSON use Python default `json.dumps` spacing and `\u` escaping; does anything parse these byte-wise (probably not)? Rust may use compact UTF-8 JSON if the web JS only `JSON.parse`s them.
- Q9: Catalog fallback listing and `/series` use the recursive library index (nested names like `A/B`) while compile/materialised catalog only knows top-level series. `/catalog/api/series?name=A/B` works but `series.json` damage read-back uses `series_dir/series.json` which a nested dir lacks. Keep this inconsistency?
- Q10: `volume_offsets` for volumes that no longer exist are retained forever in the facts row (never pruned) and `series_facts` rows are never deleted when folders vanish; confirm Rust should keep this.
- Q11: Debounce / timing constants (10 s, 60 s cap, 5 s retry, 20 s boot delay, 6 h rescan, 10 s PUT lock wait, 4 MiB body cap, 30 s library TTL) are unconfigurable in Python; confirm they are fixed in Rust.
- Q12 (resolved): `test_a_missing_content_length_is_400` actually sends `content_length="not a number"` (400); header truly absent => 411 (tests/unit/test_metadata_middleware.py:215-250). Chunked PUTs are therefore refused with 411 and must not be read.

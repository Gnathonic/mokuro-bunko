# Catalog Distribution (mokuro-bunko server) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** mokuro-bunko compiles `<Series>/series.json` + root `catalog.json` server-side from the library's `.mokuro`/`.cbz` files plus a stored facts table, blocks raw writes to those compiled files, and accepts metadata updates by intercepting a scoped user's `series.json` PUT.

**Architecture:** A new `src/mokuro_bunko/metadata/` package owns everything: byte-parity primitives ported from the reader client, the two document schemas and their compact serializers, an untrusted-payload validator, the newest-facts-stamp-wins merge, a stat-cached compiler that turns a series folder into volume entries, and a `MetadataService` that writes the compiled files to disk (only when the bytes changed) and regenerates on a debounce. Facts, shelf offsets and the per-volume entry cache live in the existing SQLite database (schema v3). Two request-path changes carry the contract: `AuthMiddleware` gains the policy (a `series.json` PUT is an update *request*, authorized by ownership — `registered` never, `uploader` only for a series it owns, any `MODIFY_DELETE`-holding role for all of them; every other write to a compiled file stays 403 for every role), and a thin `MetadataAPI` WSGI middleware — mounted inside `AuthMiddleware`, outside `PropfindCacheMiddleware` — intercepts the accepted PUT so the DAV layer never sees it.

**Tech Stack:** Python 3.10+ (target `py310`), `uv` for the venv/runner, wsgidav 4 + cheroot, sqlite3 via `mokuro_bunko.database.Database`, pytest, ruff, mypy (strict). No new runtime dependencies.

**Spec:** `/home/nathan/Projects/mokuro-reader-worktrees/feat/series-metadata/docs/superpowers/specs/2026-08-23-catalog-distribution-design.md` (design + the 2026-08-23 retirement amendment) and its contract half `/home/nathan/Projects/mokuro-reader-worktrees/feat/series-metadata/docs/superpowers/plans/2026-08-23-catalog-distribution-bunko.md`. The byte-authoritative reference implementations of both file formats are the client's `src/lib/metadata/series-file.ts`, `catalog-file.ts`, `sanitize.ts` and `series-key.ts` in that same worktree.

## Global Constraints

Copied verbatim from the contract (`2026-08-23-catalog-distribution-bunko.md`, "Contract (binding for both sides)"). Every task's requirements implicitly include this section.

1. **Partitioning** (owed): `<Series>/series.json` and root `catalog.json` are metadata files — never treated as user progress `.json`. (Root `series-metadata.json` no longer exists; a stale one may be ignored outright.)
2. **Compiled `series.json`** — v2, compact JSON, exactly the reader's shape: `{version:2, series_title, external_ids, titles, synonyms, tag?, unit?, spine_offset?, updated_at, volumes:[{volume_uuid, volume_title, page_count, character_count, mokuro_version, spine_width?, archive_size?, mokuro_size?, mokuro_modified?, cover_size?, cover_modified?, offset?}]}`. `updated_at` = facts stamp (fact edits only); `1970-01-01T00:00:00.000Z` when bunko holds no facts for the series. Volume entries come from the `.mokuro` files (uuid/title/pages/chars/version; `spine_width` when known) plus `archive_size` (bytes of the `.cbz`, from a plain stat). `spine_offset` (percent, nominally ±50) and per-entry `offset` (px, nominally ±500) are the shelf alignment: INDEX fields, accepted and preserved verbatim from an intercepted PUT, never validated as facts and never allowed to move the facts stamp. The ranges are enforced by readers on parse, not by bunko (see §6). No per-page arrays. Readers ignore unknown keys; bunko must too.
   - **Freshness stamps (2026-08-24 addendum).** Four more optional per-entry fields, all producer-computed and never accepted from a PUT (same rule as `archive_size` — see §6: "everything in the `volumes` array... is IGNORED" except `offset`): `mokuro_size` + `mokuro_modified` are the `.mokuro`/`.mokuro.gz` sidecar's `stat()` taken at entry-build time (bytes, and integer epoch SECONDS — `int(st_mtime)`, truncated, never rounded, never milliseconds); `cover_size` + `cover_modified` are the same pair for the volume's cover sidecar (`<Volume>.webp`). Each pair is omitted entirely (never written as `null`) when its sidecar doesn't exist or its stat is unavailable — never a fabricated `0`. Seconds, not a float or milliseconds, specifically because bunko's own filesystem stat is a float (`st_mtime`) while a generic WebDAV client only ever observes second-precision `Last-Modified` HTTP dates; comparing at finer-than-second precision would never agree between the two and every freshness check would read as stale forever. Byte-parity applies exactly as it does to every other field here (see Global Constraints): bunko emits these fields identically to the reader's own compact JSON — same key names, same integer truncation, same omit-not-null rule.
   - **Staleness rule.** A client holding a previously-stored stamp for a volume's `.mokuro` (or its cover) rebuilds/re-fetches it when EITHER the entry's current size differs from the stored size, OR the entry's current `_modified` is strictly NEWER than the stored one. An older-or-equal `_modified` at an equal size is fresh and needs no action. An entry with no stamp at all (an older bunko, or a generic non-compiling WebDAV client) is treated as unconditionally stale exactly once — the resulting rebuild/re-fetch produces a real stamp, so a stampless entry self-heals to a stamped one on its own next compare.
3. **Compiled `catalog.json`** — root, compact: `{version:1, updated_at, series:[{series_title, titles, synonyms, tag?, unit?, external_ids?, updated_at}]}` — one entry per series folder, facts subset identical to that series' `series.json`, factless series included with just `series_title` + epoch stamp. Name/mapping/search data only.
4. **Serving.** Both files served with accurate `size`/`mtime` (clients version their caches on those). Regenerate on library change (archive add/remove/rename) and on every accepted update.
5. **Write blocking (scoped users).** Archives, covers, `catalog.json`: rejected. The rejection must be an ordinary error the client can ignore — clients treat metadata-write failure as best-effort and stay read-write for everything else.
6. **Intercepted `series.json` PUT.** An authorized user's PUT is an update REQUEST, not a file write. (2026-08-24 ruling on who is authorized, superseding "within the user's permission scope" below: `registered` never; `uploader` only for a series it owns outright via the existing upload-ownership table — an untracked/unowned folder is a 403, not a free-for-all; any role holding the modify/delete permission for any series; `anonymous` is 401 — see bunko's auth task. A PUT body carrying only facts, with no `volumes` array and no offsets, is an equally legitimate update, not a malformed one.):
   - Parse; validate ONLY the facts fields (`external_ids` ints, `titles`/`synonyms` strings, `tag` string, `unit` ∈ {volumes, chapters}, `updated_at` ISO). Unknown keys are IGNORED, and so is everything in the `volumes` array EXCEPT each entry's `offset` (the client's index is unauthoritative; bunko's own compilation wins). The alignment fields — top-level `spine_offset` and per-entry `offset`, matched by `volume_uuid` — are stored as index data and re-emitted by the compiler; they are never facts, so they never move the facts stamp and a PUT carrying only offsets is still "factless". Bunko does NOT clamp or range-check them: it preserves whatever it was sent verbatim, and every reader clamps on parse (±50 % / ±500 px) — one side owns the range rule, so the two can never disagree about what a stored value means.
   - Merge newest-facts-stamp-wins against bunko's stored facts for that series, once the actor is authorized per the rule above. A factless PUT with epoch stamp never clears facts (mirror of the reader's factless rules); a factless PUT with a strictly newer stamp is an explicit unlink.
   - On accept: persist facts, regenerate that `series.json` + `catalog.json`, respond success. On validation failure: reject; the client will silently retry later — idempotency required.
7. **Compilation advertisement.** The identity endpoint (already consumed by the reader's `webdav/identity.ts`) is the signal that this server compiles metadata: any in-contract answer (`authenticated` or `anonymous`) makes the client set `serverCompilesMetadata` and disable its own `series.json`/`catalog.json` production. Generic WebDAV servers (no identity endpoint) keep client-side production.
8. **Covers.** Per-volume cover sidecar (`<Series>/<Volume>.webp`) generated from the archive's first page when missing; scoped users cannot overwrite them.

Additional project-wide constraints:

- `requires-python = ">=3.10"`, ruff `target-version = "py310"`, `line-length = 100`, isort `known-first-party = ["mokuro_bunko"]`.
- `mypy src/` runs in **strict** mode in CI — every new function needs full annotations.
- No new entries in `[project.dependencies]`; the compiler and validator use only the stdlib.
- Baseline before this plan: `uv run pytest tests/unit tests/integration -q` collects **726** tests, all green (worktree `feat/catalog-distribution`, HEAD `89bc29a`).
- Never mention the public deployment domain (a third-party deployment of this project) in code, comments, tests, commits, or the changelog.

## Decisions taken in-repo (ambiguities the contract left open)

These are resolved here so no task has to re-litigate them. Each is repeated in the task that implements it.

- **Reuse vs supersede `catalog/`:** `src/mokuro_bunko/catalog/api.py` is the *public HTML catalog page* (`/catalog`, its own JSON API, cover proxy) and has nothing to do with the reader's `catalog.json`. It is **left untouched**; the new work lives in a separate `metadata/` package. `library_index.py` (`LibraryIndexCache`) is **not** reused by the compiler: it is a TTL cache that can be up to 30 s stale and indexes nested folders at any depth, whereas regeneration must see the filesystem as it is right now and only top-level series folders. The compiler does its own `os.scandir` walk.
- **PUT interception point:** a new `MetadataAPI` WSGI middleware mounted **inside** `AuthMiddleware` and **outside** `AdminAPI`/`PropfindCacheMiddleware`. Inside auth so `environ["mokuro.user"]`/`mokuro.role` are populated; outside the DAV app so wsgidav never opens a writer for the path. `MokuroFileResource.begin_write` is deliberately *not* the hook: the request is not a file write at all, and the writer protocol would force the answer into wsgidav's PUT status handling.
- **Interception is uniform across roles:** every authenticated user's `series.json` PUT is intercepted, including `uploader`/`editor`/`admin`. The contract allows either; uniform interception means bunko's compiled output can never disagree with the file on disk, and a raw write would be clobbered by the next regeneration anyway.
- **"Within the user's permission scope" (§6)** = an ownership check, not a flat permission gate (2026-08-24 ruling, supersedes the `WRITE_PROGRESS` design this bullet originally described): `registered` never reaches `MetadataAPI`; `uploader` only for a series it owns outright, via a new `Database.can_user_edit_series` built on the existing `volume_uploads` ownership table (no schema change); any `MODIFY_DELETE`-holding role for every series. An audit-log entry still names the actor regardless of which branch authorized them. See Task 11.
- **`catalog.json`'s own `updated_at`** is the MAX of its entries' facts stamps (epoch when every entry is factless), not `now`. The client documents it as informational ("the MERGE key is per entry"); a wall-clock stamp would change the bytes on every rebuild and defeat the size/mtime cache discipline of §4.
- **An empty library still gets a `catalog.json`** (`{"version":1,"updated_at":"1970-01-01T00:00:00.000Z","series":[]}`). The client's `buildCatalogFile` returns `undefined` for an empty catalog because a client only ever knows part of a library; bunko knows all of it, so serving the truth beats serving a stale file.
- **`updated_at` normalisation clamps the future** to `now + 5 min` exactly like the client's `normalizeUpdatedAt`, because the stamp decides merges by lexicographic comparison and a far-future value would otherwise win forever.
- **Facts rows outlive their folders.** A series folder that disappears drops out of the compiled files but keeps its facts row, so a restore gets its link back. A folder RENAME does not carry facts across (facts are keyed by normalized series title); the client republishes them under the new name on its next fact edit. Recorded as a known limitation. **Amendment, 2026-08-24 (Task 11 review round 3, controller-accepted):** the "or a re-upload" / pre-provisioning half of this bullet is retired — `MetadataService.apply_series_update` now refuses (400) a PUT whose title does not resolve to a folder that exists right now, so facts can no longer be published ahead of the upload. The "restore" half (a row surviving its folder's temporary absence; nothing deletes a `series_facts` row) is unchanged. Full trace in the Task 11 report.
- **Freshness-stamp field order (2026-08-24 addendum).** `mokuro_size`/`mokuro_modified`/`cover_size`/`cover_modified` sit in the volume entry directly after `archive_size` and before `offset`: grouped with the other file-stat facts (`spine_width`, `archive_size`), while `offset` stays the very last key — it is the one INDEX field in an otherwise all-facts entry, and keeping it last was already the existing convention. Pinned in the contract's §2 and in Task 11b's golden-byte tests.
- **Cover stat is never cached (2026-08-24 addendum).** Unlike `mokuro_size`/`mokuro_modified` (free — the sidecar is already `stat()`-ed once for the entry cache's key, see Task 11b), `cover_size`/`cover_modified` are `stat()`-ed fresh on every `compile_series_volumes` call, cache hit or miss, and applied to the returned entry via `dataclasses.replace` after the cache lookup. The entry cache exists to skip re-parsing a `.mokuro` (the expensive part), not to skip a `stat()` (cheap); caching the cover stat too would let a `.webp` that Task 12's cover worker generates well after an entry was cached go unseen in `series.json` until the archive or sidecar also happened to change.

---

## File Structure

**New package `src/mokuro_bunko/metadata/`** (one responsibility per file):

| File | Responsibility |
| --- | --- |
| `__init__.py` | Re-exports the public names other packages use (`MetadataService`, the path predicates). |
| `paths.py` | Virtual-path predicates: is this `<Series>/series.json`, the root `catalog.json`, a compiled file at all; which series does a path name. No I/O. |
| `reader_compat.py` | Byte-parity ports of the client's primitives: `count_chars`, `deterministic_uuid`, `normalize_series_key`, `normalize_volume_title_key`, `natural_sort_key`, `normalize_updated_at`. Pure. |
| `schema.py` | `SeriesFacts`, `VolumeEntry`, `SeriesIndexData`, `CatalogEntry` dataclasses + `dump_series_file` / `dump_catalog_file` (compact bytes, exact key order). Pure. |
| `validate.py` | `parse_series_update(payload) -> SeriesUpdate | None` — the untrusted-PUT boundary (§6). Pure. |
| `merge.py` | `merge_series_update(stored, update) -> MergeResult` — newest-facts-stamp-wins + factless/epoch rules + index-field rules. Pure. |
| `compiler.py` | Filesystem → volume entries: `.mokuro`/`.mokuro.gz` parsing, image-only fallback, `archive_size`, freshness stamps (`mokuro_size`/`mokuro_modified`/`cover_size`/`cover_modified`, Task 11b), the stat-keyed entry cache, `iter_series_folders`. |
| `files.py` | `write_if_changed(path, data) -> bool` + `atomic_write_bytes` under the shared per-path write lock. |
| `service.py` | `MetadataService`: apply an update, regenerate one series or all of them, debounce, post-write hook, shutdown. The only stateful object. |
| `middleware.py` | `MetadataAPI` WSGI middleware: intercept the accepted `series.json` PUT, answer 204/400/413. |

**Modified:**

- `src/mokuro_bunko/database.py` — schema v3: `series_facts` + `series_entry_cache` tables and their accessors (new "Series metadata operations" section, mirroring "Upload ownership operations"); plus `series_owners`/`can_user_edit_series`/`list_series_owned_by` in "Upload ownership operations" itself (Task 11 — no schema change).
- `src/mokuro_bunko/webdav/resources.py` — expose the existing per-path write-lock registry as a public `path_write_lock(path)` context manager (the compiled-file writer must take the same lock as DAV writes).
- `src/mokuro_bunko/middleware/auth.py` — §5/§6 policy: `series.json` PUT is ownership-gated (`registered` never, `uploader` only for a series it owns, any `MODIFY_DELETE`-holding role for all of them); every other write verb on a compiled metadata file is 403 for every role.
- `src/mokuro_bunko/login/api.py` — identity endpoint gains a `metadata` scope object mirroring the `series.json` PUT gate (Task 11).
- `src/mokuro_bunko/ocr/watcher.py` — `OCRWorker(thumbnails_only=True)` so covers are still generated when the OCR backend is `skip` (§8).
- `src/mokuro_bunko/server.py` — build the service, mount `MetadataAPI`, wire the regeneration triggers, start the cover-only worker, shut everything down.
- `CHANGELOG.md` — `[Unreleased]` entry.

**Tests** (mirroring the existing split: pure logic in `tests/unit/`, WSGI-level behaviour in `tests/integration/`):

`tests/unit/test_metadata_paths.py`, `test_metadata_reader_compat.py`, `test_metadata_schema.py`, `test_metadata_validate.py`, `test_metadata_merge.py`, `test_metadata_compiler.py`, `test_metadata_catalog.py`, `test_metadata_service.py`, `test_database_series_metadata.py`, `test_metadata_permissions.py`, `test_thumbnail_only_worker.py`; `tests/integration/test_metadata_distribution.py`.

---

### Task 1: Metadata path partitioning (contract §1)

The "owed patch". Today `PathMapper` treats a file as per-user progress only when its name is exactly `volume-data.json` or `profiles.json` **and** it sits directly under `/mokuro-reader/`, so a nested `series.json` already lands in the shared library — but nothing states that rule or protects it. This task states it, gives the rest of the plan its path vocabulary, and locks the partitioning down with regression tests.

**Files:**
- Create: `src/mokuro_bunko/metadata/__init__.py`, `src/mokuro_bunko/metadata/paths.py`
- Test: `tests/unit/test_metadata_paths.py`

**Interfaces:**
- Consumes: `mokuro_bunko.webdav.resources.PathMapper` (`READER_ROOT`, `PER_USER_FILES`).
- Produces: `SERIES_FILE_NAME`, `CATALOG_FILE_NAME`, `is_catalog_file_path(virtual_path: str) -> bool`, `is_series_file_path(virtual_path: str) -> bool`, `series_title_from_series_file_path(virtual_path: str) -> str | None`, `is_compiled_metadata_path(virtual_path: str) -> bool`.

- [ ] **Step 1: Write the failing test**

```python
"""Contract §1: compiled metadata files are library files, never progress."""

from __future__ import annotations

from pathlib import Path

from mokuro_bunko.metadata.paths import (
    is_catalog_file_path,
    is_compiled_metadata_path,
    is_series_file_path,
    series_title_from_series_file_path,
)
from mokuro_bunko.middleware.auth import is_library_path, is_progress_file
from mokuro_bunko.webdav.resources import PathMapper


class TestSeriesFilePaths:
    def test_series_sidecar_is_recognised(self) -> None:
        assert is_series_file_path("/mokuro-reader/Dr Stone/series.json")
        assert series_title_from_series_file_path(
            "/mokuro-reader/Dr Stone/series.json"
        ) == "Dr Stone"

    def test_basename_match_is_case_insensitive(self) -> None:
        assert is_series_file_path("/mokuro-reader/Dr Stone/Series.JSON")

    def test_nested_deeper_than_one_folder_is_not_a_sidecar(self) -> None:
        assert not is_series_file_path("/mokuro-reader/Dr Stone/extras/series.json")

    def test_root_series_json_is_not_a_sidecar(self) -> None:
        assert not is_series_file_path("/mokuro-reader/series.json")

    def test_other_json_in_a_series_folder_is_not_a_sidecar(self) -> None:
        assert not is_series_file_path("/mokuro-reader/Dr Stone/volume-data.json")


class TestCatalogFilePaths:
    def test_root_catalog_is_recognised(self) -> None:
        assert is_catalog_file_path("/mokuro-reader/catalog.json")
        assert is_compiled_metadata_path("/mokuro-reader/catalog.json")

    def test_nested_catalog_is_somebody_elses_file(self) -> None:
        assert not is_catalog_file_path("/mokuro-reader/Dr Stone/catalog.json")


class TestPartitioning:
    """The regression the contract asks for: metadata is never progress."""

    METADATA_PATHS = [
        "/mokuro-reader/Dr Stone/series.json",
        "/mokuro-reader/catalog.json",
    ]

    def test_metadata_paths_are_never_progress_files(self) -> None:
        for path in self.METADATA_PATHS:
            assert not is_progress_file(path)

    def test_metadata_paths_are_library_paths(self) -> None:
        for path in self.METADATA_PATHS:
            assert is_library_path(path)

    def test_metadata_paths_map_into_the_shared_library(self, tmp_path: Path) -> None:
        mapper = PathMapper(tmp_path)
        for path in self.METADATA_PATHS:
            physical = mapper.virtual_to_physical(path, username="alice")
            assert physical is not None
            assert physical.is_relative_to(mapper.library_path.resolve())
            assert not physical.is_relative_to(mapper.users_path.resolve())
            assert mapper.get_path_type(path) == "library"
            assert not mapper.is_per_user_file(path)

    def test_stale_root_series_metadata_json_is_an_ordinary_library_file(
        self, tmp_path: Path
    ) -> None:
        """Retired file: inert junk, never progress, never compiled."""
        mapper = PathMapper(tmp_path)
        path = "/mokuro-reader/series-metadata.json"
        assert not is_progress_file(path)
        assert not is_compiled_metadata_path(path)
        physical = mapper.virtual_to_physical(path, username="alice")
        assert physical is not None
        assert physical.is_relative_to(mapper.library_path.resolve())
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/unit/test_metadata_paths.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'mokuro_bunko.metadata'`

- [ ] **Step 3: Write the implementation**

`src/mokuro_bunko/metadata/__init__.py`:

```python
"""Server-side compilation of the reader's `series.json` / `catalog.json`."""

from mokuro_bunko.metadata.paths import (
    CATALOG_FILE_NAME,
    SERIES_FILE_NAME,
    is_catalog_file_path,
    is_compiled_metadata_path,
    is_series_file_path,
    series_title_from_series_file_path,
)

__all__ = [
    "CATALOG_FILE_NAME",
    "SERIES_FILE_NAME",
    "is_catalog_file_path",
    "is_compiled_metadata_path",
    "is_series_file_path",
    "series_title_from_series_file_path",
]
```

`src/mokuro_bunko/metadata/paths.py`:

```python
"""Which virtual paths carry compiled metadata (contract §1).

`<Series>/series.json` and the root `catalog.json` are METADATA files: they
live in the shared library, this server compiles them, and they must never be
confused with the per-user progress files (`volume-data.json`,
`profiles.json`) that `PathMapper` maps into a user's private directory.

A stale root `series-metadata.json` written by an older reader is inert — an
ordinary library file that nothing here looks at.
"""

from __future__ import annotations

from mokuro_bunko.webdav.resources import PathMapper

SERIES_FILE_NAME = "series.json"
CATALOG_FILE_NAME = "catalog.json"

_READER_PREFIX = f"/{PathMapper.READER_ROOT}/"


def _library_relative(virtual_path: str) -> str | None:
    """Library-relative part of a `/mokuro-reader/...` path, else None.

    Per-user files are excluded here, which is the partitioning rule itself:
    a path that maps into a user's private directory can never be metadata.
    """
    normalized = "/" + virtual_path.strip("/")
    if not normalized.startswith(_READER_PREFIX):
        return None
    relative = normalized[len(_READER_PREFIX):]
    if not relative or relative in PathMapper.PER_USER_FILES:
        return None
    return relative


def is_catalog_file_path(virtual_path: str) -> bool:
    """True for the ROOT catalog.json only; a nested one is somebody else's file."""
    relative = _library_relative(virtual_path)
    return relative is not None and relative.lower() == CATALOG_FILE_NAME


def series_title_from_series_file_path(virtual_path: str) -> str | None:
    """`/mokuro-reader/<Series>/series.json` -> `<Series>`, else None.

    Exactly one folder level: the reader stores one sidecar per series folder,
    and a deeper path is not a series the catalog knows about.
    """
    relative = _library_relative(virtual_path)
    if relative is None:
        return None
    head, separator, tail = relative.rpartition("/")
    if not separator or tail.lower() != SERIES_FILE_NAME:
        return None
    if "/" in head or not head.strip():
        return None
    return head


def is_series_file_path(virtual_path: str) -> bool:
    """True for `<Series>/series.json` directly under the reader root."""
    return series_title_from_series_file_path(virtual_path) is not None


def is_compiled_metadata_path(virtual_path: str) -> bool:
    """Any file this server compiles, and therefore owns."""
    return is_catalog_file_path(virtual_path) or is_series_file_path(virtual_path)
```

- [ ] **Step 4: Run test to verify it passes**

Run: `uv run pytest tests/unit/test_metadata_paths.py -q`
Expected: PASS (11 tests)

- [ ] **Step 5: Check lint and types**

Run: `uv run ruff check src/ && uv run mypy src/`
Expected: no findings

- [ ] **Step 6: Commit**

```bash
git add src/mokuro_bunko/metadata tests/unit/test_metadata_paths.py
git commit -m "feat(metadata): path predicates for compiled series.json/catalog.json"
```

---

### Task 2: Reader-parity primitives

Everything bunko compiles has to agree with the client byte for byte, so the client's small pure helpers get ported here — verified against Node running the originals. `character_count` is the sharp one: real upstream `.mokuro` files carry **no** `chars` key (checked against a live library: top-level keys are `version, title, title_uuid, volume, volume_uuid, pages`), so bunko must count Japanese characters itself with the reader's exact character class.

**Files:**
- Create: `src/mokuro_bunko/metadata/reader_compat.py`
- Test: `tests/unit/test_metadata_reader_compat.py`

**Interfaces:**
- Consumes: nothing.
- Produces: `count_chars(text: str) -> int`, `count_page_chars(pages: list[Any]) -> int`, `deterministic_uuid(value: str) -> str`, `normalize_series_key(title: str) -> str`, `normalize_volume_title_key(title: str) -> str`, `natural_sort_key(title: str) -> tuple[tuple[int, object], ...]`, `normalize_updated_at(value: object, now: float | None = None) -> str | None`, `iso_stamp(seconds: float) -> str`, `FUTURE_TOLERANCE_SECONDS`.

Note on `normalize_updated_at`: it parses with `datetime.fromisoformat` (plus `Z` handling), which is stricter than the client's `Date.parse`. Every stamp that reaches bunko is `toISOString()` output, and anything else is exactly what the contract wants rejected, so the narrower grammar is deliberate.

- [ ] **Step 1: Write the failing test**

```python
"""Parity with the reader client's pure helpers.

Every expected value here was produced by running the client's own
implementation under Node (`src/lib/util/count-chars.ts`,
`src/lib/util/series-extraction.ts` `generateDeterministicUUID`,
`src/lib/metadata/series-key.ts`, `src/lib/metadata/sanitize.ts`).
"""

from __future__ import annotations

from mokuro_bunko.metadata.reader_compat import (
    count_chars,
    count_page_chars,
    deterministic_uuid,
    natural_sort_key,
    normalize_series_key,
    normalize_updated_at,
    normalize_volume_title_key,
)


class TestCountChars:
    def test_matches_the_client_on_real_ocr_lines(self) -> None:
        assert count_chars("あたしはもうちょっとしたＣＯＮＡには") == 14
        assert count_chars("Hello, 世界!") == 2
        assert count_chars("カタカナとひらがな") == 9
        assert count_chars("123 ABC") == 0

    def test_counts_the_explicit_singles_the_client_lists(self) -> None:
        # ○ U+25CB, ◯ U+25EF, 々 U+3005 — and 〆 U+3006 rides along inside the
        # class's `々-〇` range, which is a quirk of the client worth pinning.
        assert count_chars("○◯々") == 3
        assert count_chars("〆") == 1
        assert count_chars("〜①") == 0

    def test_counts_halfwidth_katakana_and_cjk_extension_b(self) -> None:
        assert count_chars("ｱｲｳ") == 3
        assert count_chars("\U00020000") == 1

    def test_page_totals_sum_every_line_of_every_block(self) -> None:
        pages = [
            {"blocks": [{"lines": ["世界", "abc"]}, {"lines": ["ねこ"]}]},
            {"blocks": [{"lines": ["犬"]}]},
        ]
        assert count_page_chars(pages) == 5

    def test_malformed_pages_are_skipped_not_fatal(self) -> None:
        pages = [{"blocks": "nonsense"}, {"blocks": [{"lines": [1, "犬"]}]}, "junk"]
        assert count_page_chars(pages) == 1


class TestDeterministicUUID:
    def test_matches_the_client_for_placeholder_uuids(self) -> None:
        assert deterministic_uuid("Dr Stone/Volume 01") == "38d6c0d6-1bef-4134-a339-a1e254c6"
        assert deterministic_uuid("Bakemonogatari/v01") == "fd95c3db-a308-4539-9e9d-26e2a09e"
        assert deterministic_uuid("Series/Vol 1") == "964edad5-740a-4337-a244-29e20a59"

    def test_lowercases_and_trims_like_the_client(self) -> None:
        assert deterministic_uuid("  MiXeD Case / Vol 2  ") == "7f27f644-c177-46a6-be50-b0e2409f"


class TestKeys:
    def test_series_key_folds_case_and_whitespace(self) -> None:
        assert normalize_series_key("  Dr   STONE  ") == "dr stone"

    def test_volume_title_key_also_folds_unicode_composition(self) -> None:
        assert normalize_volume_title_key("Bände 1") == normalize_volume_title_key("Bände 1")


class TestNaturalSort:
    def test_orders_like_the_clients_numeric_collator(self) -> None:
        titles = ["Vol 10", "Vol 2", "vol 1", "Volume 3", "Extra"]
        assert sorted(titles, key=natural_sort_key) == [
            "Extra",
            "vol 1",
            "Vol 2",
            "Vol 10",
            "Volume 3",
        ]

    def test_is_total_and_stable_for_equal_keys(self) -> None:
        assert natural_sort_key("VOL 1") == natural_sort_key("vol 1")


class TestNormalizeUpdatedAt:
    def test_normalises_to_iso_with_milliseconds(self) -> None:
        assert normalize_updated_at("2026-08-18T19:36:24.324Z") == "2026-08-18T19:36:24.324Z"
        assert normalize_updated_at("2026-08-18T19:36:24Z") == "2026-08-18T19:36:24.000Z"

    def test_rejects_junk(self) -> None:
        assert normalize_updated_at("Aug 16 2020") is None
        assert normalize_updated_at(None) is None
        assert normalize_updated_at(1234) is None

    def test_clamps_the_far_future_to_now(self) -> None:
        now = 1_800_000_000.0  # 2027-01-15T08:00:00Z
        clamped = normalize_updated_at("2999-01-01T00:00:00.000Z", now=now)
        assert clamped == "2027-01-15T08:00:00.000Z"

    def test_tolerates_small_clock_skew(self) -> None:
        now = 1_800_000_000.0
        just_ahead = normalize_updated_at("2027-01-15T08:01:00.000Z", now=now)
        assert just_ahead == "2027-01-15T08:01:00.000Z"
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/unit/test_metadata_reader_compat.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'mokuro_bunko.metadata.reader_compat'`

- [ ] **Step 3: Write the implementation**

`src/mokuro_bunko/metadata/reader_compat.py`:

```python
"""Ports of the reader client's pure helpers, kept byte-identical.

Compiled files have to be indistinguishable from the ones the client writes,
so these mirror their TypeScript originals exactly rather than approximating
them. Each function names the source it was ported from; change one only
together with the other side.
"""

from __future__ import annotations

import bisect
import re
import time
import unicodedata
from datetime import datetime, timezone
from typing import Any

# Ported from `src/lib/util/count-chars.ts`. The client's character class is
# `[○◯々-〇〻ぁ-ゖゝ-ゞァ-ヺー\p{Script=Hiragana}\p{Script=Katakana}\p{Script=Han}]`;
# Python's `re` has no `\p{Script=...}`, so the class was expanded to its
# codepoint ranges by evaluating the regex over every codepoint under Node and
# recording the runs. Regenerate the table the same way if the class changes.
_COUNTED_RANGES: tuple[tuple[int, int], ...] = (
    (0x25CB, 0x25CB), (0x25EF, 0x25EF), (0x2E80, 0x2E99), (0x2E9B, 0x2EF3),
    (0x2F00, 0x2FD5), (0x3005, 0x3007), (0x3021, 0x3029), (0x3038, 0x303B),
    (0x3041, 0x3096), (0x309D, 0x309F), (0x30A1, 0x30FA), (0x30FC, 0x30FF),
    (0x31F0, 0x31FF), (0x32D0, 0x32FE), (0x3300, 0x3357), (0x3400, 0x4DBF),
    (0x4E00, 0x9FFF), (0xF900, 0xFA6D), (0xFA70, 0xFAD9), (0xFF66, 0xFF6F),
    (0xFF71, 0xFF9D), (0x16FE2, 0x16FE3), (0x16FF0, 0x16FF6), (0x1AFF0, 0x1AFF3),
    (0x1AFF5, 0x1AFFB), (0x1AFFD, 0x1AFFE), (0x1B000, 0x1B122), (0x1B132, 0x1B132),
    (0x1B150, 0x1B152), (0x1B155, 0x1B155), (0x1B164, 0x1B167), (0x1F200, 0x1F200),
    (0x20000, 0x2A6DF), (0x2A700, 0x2B81D), (0x2B820, 0x2CEAD), (0x2CEB0, 0x2EBE0),
    (0x2EBF0, 0x2EE5D), (0x2F800, 0x2FA1D), (0x30000, 0x3134A), (0x31350, 0x33479),
)
_RANGE_STARTS = tuple(start for start, _ in _COUNTED_RANGES)
_RANGE_ENDS = tuple(end for _, end in _COUNTED_RANGES)

FUTURE_TOLERANCE_SECONDS = 5 * 60


def _is_counted(char: str) -> bool:
    code_point = ord(char)
    index = bisect.bisect_right(_RANGE_STARTS, code_point) - 1
    return index >= 0 and code_point <= _RANGE_ENDS[index]


def count_chars(text: str) -> int:
    """Japanese characters in one OCR line (client: `countChars`)."""
    return sum(1 for char in text if _is_counted(char))


def count_page_chars(pages: Any) -> int:
    """Total over every line of every block (client: `getCharCount`).

    Defensive on shape: a `.mokuro` is foreign data that may have been
    hand-edited, and one malformed block must not fail a whole library scan.
    """
    total = 0
    if not isinstance(pages, list):
        return 0
    for page in pages:
        if not isinstance(page, dict):
            continue
        blocks = page.get("blocks")
        if not isinstance(blocks, list):
            continue
        for block in blocks:
            if not isinstance(block, dict):
                continue
            lines = block.get("lines")
            if not isinstance(lines, list):
                continue
            for line in lines:
                if isinstance(line, str):
                    total += count_chars(line)
    return total


def _to_int32(value: int) -> int:
    value &= 0xFFFFFFFF
    return value - 0x100000000 if value >= 0x80000000 else value


def _utf16_units(text: str) -> list[int]:
    """JS `charCodeAt` semantics: UTF-16 code units, not code points."""
    data = text.encode("utf-16-le", "surrogatepass")
    return [data[i] | (data[i + 1] << 8) for i in range(0, len(data), 2)]


def deterministic_uuid(value: str) -> str:
    """Port of the client's `generateDeterministicUUID` (djb2-xor pair).

    Image-only volumes have no `.mokuro` and therefore no real uuid; the
    client derives a placeholder uuid from `"<Series>/<Volume>"` with this
    function, so bunko MUST derive the same one or synced progress recorded
    against the placeholder would strand when the index arrives.

    The shape it produces is 8-4-4-4-8, not a real UUID — that is what the
    client emits, quirks (`hex2[5:8]` skipping a digit) included.
    """
    normalized = value.lower().strip()
    hash1 = 5381
    hash2 = 52711
    for unit in _utf16_units(normalized):
        hash1 = _to_int32(hash1 * 33) ^ unit
        hash2 = _to_int32(hash2 * 33) ^ unit
    hash1 &= 0xFFFFFFFF
    hash2 &= 0xFFFFFFFF
    hex1 = f"{hash1:08x}"
    hex2 = f"{hash2:08x}"
    hash3 = f"{(hash1 ^ hash2) & 0xFFFFFFFF:08x}"
    hash4 = f"{(hash1 + hash2) & 0xFFFFFFFF:08x}"
    variant = f"{8 + (int(hash3[0], 16) % 4):x}"
    return f"{hex1}-{hex2[:4]}-4{hex2[5:8]}-{variant}{hash3[1:4]}-{hash3[4:]}{hash4[:4]}"


def normalize_series_key(title: str) -> str:
    """Client: `normalizeSeriesKey` — trim, collapse whitespace, lowercase."""
    return re.sub(r"\s+", " ", title.strip()).lower()


def normalize_volume_title_key(title: str) -> str:
    """Client: `normalizeVolumeTitleKey` — the series fold plus NFC.

    A filename that round-tripped through a filesystem can come back
    decomposed while the JSON beside it stays composed: byte-different, same
    title.
    """
    return normalize_series_key(unicodedata.normalize("NFC", title))


_DIGIT_RUN = re.compile(r"(\d+)")


def natural_sort_key(title: str) -> tuple[tuple[int, object], ...]:
    """Order volume titles the way the client's collator does.

    The client sorts with `Intl.Collator(undefined, {numeric: true,
    sensitivity: 'base'})`. Full ICU parity is not reachable from the stdlib
    and is not needed: nothing downstream depends on the file's order (readers
    re-sort on read). What IS required is that the order be TOTAL and STABLE,
    so a rebuild that changed nothing produces the same bytes and therefore the
    same size/mtime (contract §3/§4). Digit runs compare numerically and sort
    before letters; text compares case- and accent-folded.
    """
    key: list[tuple[int, object]] = []
    for part in _DIGIT_RUN.split(title):
        if not part:
            continue
        if part.isdigit():
            key.append((0, int(part)))
        else:
            folded = "".join(
                char
                for char in unicodedata.normalize("NFKD", part)
                if not unicodedata.combining(char)
            ).casefold()
            key.append((1, folded))
    return tuple(key)


def normalize_updated_at(value: object, now: float | None = None) -> str | None:
    """Client: `normalizeUpdatedAt` — comparable ISO string, or None.

    `updated_at` decides merges by lexicographic comparison, so a non-ISO
    string ("Aug 16 2020" sorts above every ISO date) or a far-future value
    would win against every honest timestamp forever. Unparsable -> None
    (caller drops the payload); more than five minutes ahead -> clamped.
    """
    if not isinstance(value, str):
        return None
    text = value.strip()
    if not text:
        return None
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    try:
        parsed = datetime.fromisoformat(text)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    seconds = parsed.timestamp()
    reference = time.time() if now is None else now
    if seconds > reference + FUTURE_TOLERANCE_SECONDS:
        seconds = reference
    return iso_stamp(seconds)


def iso_stamp(seconds: float) -> str:
    """`Date.prototype.toISOString()` shape: UTC, exactly 3 decimals, `Z`."""
    moment = datetime.fromtimestamp(seconds, tz=timezone.utc)
    return moment.strftime("%Y-%m-%dT%H:%M:%S.") + f"{moment.microsecond // 1000:03d}Z"
```

- [ ] **Step 4: Run test to verify it passes**

Run: `uv run pytest tests/unit/test_metadata_reader_compat.py -q`
Expected: PASS (15 tests)

- [ ] **Step 5: Verify parity against a real library volume (evidence, not a claim)**

Run:

```bash
uv run python - <<'PY'
import json
from mokuro_bunko.metadata.reader_compat import count_page_chars
path = "/home/nathan/.local/share/mokuro-webdav/library/Bakemonogatari/v01.mokuro"
data = json.load(open(path))
print("pages", len(data["pages"]), "chars", count_page_chars(data["pages"]))
PY
```

Expected: `pages 187 chars 13247` — the same totals the client's `countChars` produces for that file under Node. If the machine has no such library, skip this step and rely on the unit vectors.

- [ ] **Step 6: Check lint and types**

Run: `uv run ruff check src/ && uv run mypy src/`
Expected: no findings

- [ ] **Step 7: Commit**

```bash
git add src/mokuro_bunko/metadata/reader_compat.py tests/unit/test_metadata_reader_compat.py
git commit -m "feat(metadata): port the reader's char count, uuid and key helpers"
```

---

### Task 3: Document schema and compact serializers (contract §2, §3)

The two output shapes, and the one place that turns them into bytes. Key order is fixed here so a rebuild that changed nothing produces identical bytes — that is what keeps `size`/`mtime` stable for every client's cache (§4).

**Files:**
- Create: `src/mokuro_bunko/metadata/schema.py`
- Test: `tests/unit/test_metadata_schema.py`

**Interfaces:**
- Consumes: `reader_compat.natural_sort_key`, `reader_compat.normalize_series_key`.
- Produces: `FACTLESS_UPDATED_AT`, `ID_KEYS`, `TITLE_KEYS`, `TRACKING_UNITS`, `SeriesFacts` (frozen dataclass: `external_ids: dict[str, int]`, `titles: dict[str, str]`, `synonyms: tuple[str, ...]`, `tag: str | None`, `unit: str | None`, `updated_at: str`; method `has_facts() -> bool`), `SeriesIndexData` (`spine_offset: float | None`, `volume_offsets: dict[str, float]`), `VolumeEntry` (`volume_uuid`, `volume_title`, `page_count`, `character_count`, `mokuro_version`, `spine_width: float | None`, `archive_size: int | None`), `dump_series_file(*, series_title, facts, index, volumes) -> bytes`, `dump_catalog_file(entries: Sequence[tuple[str, SeriesFacts]]) -> bytes`.

> Task 11b (2026-08-24, after this task shipped) extends `VolumeEntry` with four more optional fields — `mokuro_size`, `mokuro_modified`, `cover_size`, `cover_modified` — and the matching keys in `dump_series_file`'s entry shape. This line is left as originally drafted for historical accuracy; see Task 11b for the current field list.

- [ ] **Step 1: Write the failing test**

```python
"""Golden bytes for the two compiled documents (contract §2 and §3)."""

from __future__ import annotations

from mokuro_bunko.metadata.schema import (
    FACTLESS_UPDATED_AT,
    SeriesFacts,
    SeriesIndexData,
    VolumeEntry,
    dump_catalog_file,
    dump_series_file,
)

DR_STONE = SeriesFacts(
    external_ids={"anilist": 98416, "mal": 103897},
    titles={"native": "Dr.STONE", "romaji": "Dr. STONE"},
    synonyms=("ドクターストーン",),
    tag="HD Scan",
    unit="volumes",
    updated_at="2026-08-18T19:36:24.324Z",
)


class TestSeriesFacts:
    def test_empty_facts_are_factless_at_the_epoch(self) -> None:
        facts = SeriesFacts()
        assert not facts.has_facts()
        assert facts.updated_at == FACTLESS_UPDATED_AT == "1970-01-01T00:00:00.000Z"

    def test_any_single_fact_makes_it_factful(self) -> None:
        assert SeriesFacts(tag="HD Scan").has_facts()
        assert SeriesFacts(unit="chapters").has_facts()
        assert SeriesFacts(external_ids={"anilist": 1}).has_facts()
        assert SeriesFacts(titles={"native": "x"}).has_facts()
        assert SeriesFacts(synonyms=("x",)).has_facts()

    def test_blank_strings_are_not_facts(self) -> None:
        assert not SeriesFacts(tag="   ").has_facts()
        assert not SeriesFacts(synonyms=("", "  ")).has_facts()


class TestDumpSeriesFile:
    def test_factless_series_with_one_volume(self) -> None:
        data = dump_series_file(
            series_title="Bakemonogatari",
            facts=SeriesFacts(),
            index=SeriesIndexData(),
            volumes=[
                VolumeEntry(
                    volume_uuid="cfb5220c-57db-4008-9f44-e659d794e381",
                    volume_title="v01",
                    page_count=187,
                    character_count=13247,
                    mokuro_version="0.2.2",
                    archive_size=1234,
                )
            ],
        )
        assert data.decode("utf-8") == (
            '{"version":2,"series_title":"Bakemonogatari","external_ids":{},"titles":{},'
            '"synonyms":[],"updated_at":"1970-01-01T00:00:00.000Z","volumes":['
            '{"volume_uuid":"cfb5220c-57db-4008-9f44-e659d794e381","volume_title":"v01",'
            '"page_count":187,"character_count":13247,"mokuro_version":"0.2.2",'
            '"archive_size":1234}]}'
        )

    def test_full_facts_offsets_and_natural_volume_order(self) -> None:
        data = dump_series_file(
            series_title="Dr Stone",
            facts=DR_STONE,
            index=SeriesIndexData(spine_offset=12.5, volume_offsets={"u10": -40, "u2": 0}),
            volumes=[
                VolumeEntry("u10", "Volume 10", 200, 10000, ""),
                VolumeEntry("u2", "Volume 2", 180, 9000, "0.2.2", spine_width=250.5,
                            archive_size=99),
            ],
        )
        assert data.decode("utf-8") == (
            '{"version":2,"series_title":"Dr Stone",'
            '"external_ids":{"anilist":98416,"mal":103897},'
            '"titles":{"native":"Dr.STONE","romaji":"Dr. STONE"},'
            '"synonyms":["ドクターストーン"],"tag":"HD Scan","unit":"volumes",'
            '"spine_offset":12.5,"updated_at":"2026-08-18T19:36:24.324Z","volumes":['
            '{"volume_uuid":"u2","volume_title":"Volume 2","page_count":180,'
            '"character_count":9000,"mokuro_version":"0.2.2","spine_width":250.5,'
            '"archive_size":99},'
            '{"volume_uuid":"u10","volume_title":"Volume 10","page_count":200,'
            '"character_count":10000,"mokuro_version":"","offset":-40}]}'
        )

    def test_japanese_is_written_raw_not_escaped(self) -> None:
        data = dump_series_file(
            series_title="Dr Stone", facts=DR_STONE, index=SeriesIndexData(), volumes=[]
        )
        assert "ドクターストーン".encode("utf-8") in data
        assert b"\\u30c9" not in data  # not `ensure_ascii`-escaped

    def test_unknown_ids_titles_and_units_never_reach_the_file(self) -> None:
        facts = SeriesFacts(
            external_ids={"anilist": 1, "kitsune": 7},
            titles={"native": "x", "klingon": "y"},
            updated_at="2026-08-18T19:36:24.324Z",
        )
        text = dump_series_file(
            series_title="S", facts=facts, index=SeriesIndexData(), volumes=[]
        ).decode("utf-8")
        assert '"external_ids":{"anilist":1}' in text
        assert '"titles":{"native":"x"}' in text
        assert "kitsune" not in text and "klingon" not in text

    def test_rebuild_of_unchanged_input_is_byte_identical(self) -> None:
        args = dict(
            series_title="Dr Stone",
            facts=DR_STONE,
            index=SeriesIndexData(spine_offset=12.5, volume_offsets={"u2": 3}),
            volumes=[VolumeEntry("u2", "Volume 2", 180, 9000, "0.2.2")],
        )
        assert dump_series_file(**args) == dump_series_file(**args)  # type: ignore[arg-type]


class TestDumpCatalogFile:
    def test_entries_sorted_by_key_with_factless_series_included(self) -> None:
        data = dump_catalog_file([("Dr Stone", DR_STONE), ("Aria", SeriesFacts())])
        assert data.decode("utf-8") == (
            '{"version":1,"updated_at":"2026-08-18T19:36:24.324Z","series":['
            '{"series_title":"Aria","external_ids":{},"titles":{},"synonyms":[],'
            '"updated_at":"1970-01-01T00:00:00.000Z"},'
            '{"series_title":"Dr Stone","external_ids":{"anilist":98416,"mal":103897},'
            '"titles":{"native":"Dr.STONE","romaji":"Dr. STONE"},'
            '"synonyms":["ドクターストーン"],"tag":"HD Scan","unit":"volumes",'
            '"updated_at":"2026-08-18T19:36:24.324Z"}]}'
        )

    def test_file_stamp_is_the_newest_entry_stamp_not_the_clock(self) -> None:
        first = dump_catalog_file([("Aria", SeriesFacts())])
        second = dump_catalog_file([("Aria", SeriesFacts())])
        assert first == second
        assert b'"updated_at":"1970-01-01T00:00:00.000Z","series"' in first

    def test_an_empty_library_still_produces_a_catalog(self) -> None:
        assert dump_catalog_file([]).decode("utf-8") == (
            '{"version":1,"updated_at":"1970-01-01T00:00:00.000Z","series":[]}'
        )

    def test_volume_data_never_leaks_into_the_catalog(self) -> None:
        text = dump_catalog_file([("Dr Stone", DR_STONE)]).decode("utf-8")
        assert "volumes" not in text
        assert "spine_offset" not in text
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/unit/test_metadata_schema.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'mokuro_bunko.metadata.schema'`

- [ ] **Step 3: Write the implementation**

`src/mokuro_bunko/metadata/schema.py`:

```python
"""The two compiled documents, and the one place that turns them into bytes.

Shapes are fixed by the reader client (`src/lib/metadata/series-file.ts`,
`catalog-file.ts`); its parsers ignore unknown keys and key order, but the
BYTES matter here for a different reason: clients version their caches on the
file's size/mtime (contract §4), so a rebuild that changed nothing must
produce exactly the same bytes. Hence a fixed key order, a fixed volume order,
and no wall-clock stamps anywhere.
"""

from __future__ import annotations

import json
from collections.abc import Sequence
from dataclasses import dataclass, field
from typing import Any

from mokuro_bunko.metadata.reader_compat import natural_sort_key, normalize_series_key

#: The stamp of a document whose facts come from nowhere. It must never be
#: "now": every merge takes the newest facts stamp, so a freshly stamped empty
#: file would beat every real link. The epoch loses every comparison, which is
#: exactly what "no opinion" means.
FACTLESS_UPDATED_AT = "1970-01-01T00:00:00.000Z"

ID_KEYS: tuple[str, ...] = ("anilist", "mal")
TITLE_KEYS: tuple[str, ...] = ("native", "romaji", "english")
TRACKING_UNITS: tuple[str, ...] = ("volumes", "chapters")


@dataclass(frozen=True)
class SeriesFacts:
    """The shareable half of a series: what `catalog.json` carries verbatim."""

    external_ids: dict[str, int] = field(default_factory=dict)
    titles: dict[str, str] = field(default_factory=dict)
    synonyms: tuple[str, ...] = ()
    tag: str | None = None
    unit: str | None = None
    updated_at: str = FACTLESS_UPDATED_AT

    def has_facts(self) -> bool:
        """Does this say anything shareable? (client: `hasSeriesFacts`)"""
        return bool(
            self.external_ids
            or self.titles
            or any(synonym.strip() for synonym in self.synonyms)
            or (self.tag or "").strip()
            or self.unit
        )


@dataclass(frozen=True)
class SeriesIndexData:
    """Shelf alignment: INDEX data, never facts, never moves the facts stamp."""

    spine_offset: float | None = None
    volume_offsets: dict[str, float] = field(default_factory=dict)


@dataclass(frozen=True)
class VolumeEntry:
    """One compiled volume. Offsets are applied at dump time, by uuid."""

    volume_uuid: str
    volume_title: str
    page_count: int
    character_count: int
    mokuro_version: str
    spine_width: float | None = None
    archive_size: int | None = None


def _facts_payload(facts: SeriesFacts) -> dict[str, Any]:
    """Facts in canonical key order, unknown providers/languages dropped."""
    payload: dict[str, Any] = {
        "external_ids": {
            key: facts.external_ids[key] for key in ID_KEYS if key in facts.external_ids
        },
        "titles": {key: facts.titles[key] for key in TITLE_KEYS if facts.titles.get(key)},
        "synonyms": list(facts.synonyms),
    }
    tag = (facts.tag or "").strip()
    if tag:
        payload["tag"] = tag
    if facts.unit in TRACKING_UNITS:
        payload["unit"] = facts.unit
    return payload


def _dumps(payload: Any) -> bytes:
    """Compact, UTF-8, unescaped — the client's `JSON.stringify` output."""
    return json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def dump_series_file(
    *,
    series_title: str,
    facts: SeriesFacts,
    index: SeriesIndexData,
    volumes: Sequence[VolumeEntry],
) -> bytes:
    """Serialize `<Series>/series.json` (contract §2)."""
    payload: dict[str, Any] = {"version": 2, "series_title": series_title}
    payload.update(_facts_payload(facts))
    # A zero offset is a deliberate reset on the client and is never written.
    if index.spine_offset:
        payload["spine_offset"] = index.spine_offset
    payload["updated_at"] = facts.updated_at

    entries: list[dict[str, Any]] = []
    for volume in sorted(volumes, key=lambda item: natural_sort_key(item.volume_title)):
        entry: dict[str, Any] = {
            "volume_uuid": volume.volume_uuid,
            "volume_title": volume.volume_title,
            "page_count": volume.page_count,
            "character_count": volume.character_count,
            "mokuro_version": volume.mokuro_version,
        }
        if volume.spine_width:
            entry["spine_width"] = volume.spine_width
        if volume.archive_size:
            entry["archive_size"] = volume.archive_size
        offset = index.volume_offsets.get(volume.volume_uuid)
        if offset:
            entry["offset"] = offset
        entries.append(entry)
    payload["volumes"] = entries
    return _dumps(payload)


def dump_catalog_file(entries: Sequence[tuple[str, SeriesFacts]]) -> bytes:
    """Serialize the root `catalog.json` (contract §3).

    The file's own `updated_at` is the NEWEST entry stamp, never the clock: it
    is informational (the merge key is per entry), and a wall-clock value would
    change the bytes on every rebuild and have every client re-download a file
    that did not change.
    """
    ordered = sorted(entries, key=lambda item: normalize_series_key(item[0]))
    series: list[dict[str, Any]] = []
    newest = FACTLESS_UPDATED_AT
    for series_title, facts in ordered:
        entry: dict[str, Any] = {"series_title": series_title}
        entry.update(_facts_payload(facts))
        entry["updated_at"] = facts.updated_at
        newest = max(newest, facts.updated_at)
        series.append(entry)
    return _dumps({"version": 1, "updated_at": newest, "series": series})
```

- [ ] **Step 4: Run test to verify it passes**

Run: `uv run pytest tests/unit/test_metadata_schema.py -q`
Expected: PASS (12 tests)

- [ ] **Step 5: Check lint and types**

Run: `uv run ruff check src/ && uv run mypy src/`
Expected: no findings

- [ ] **Step 6: Commit**

```bash
git add src/mokuro_bunko/metadata/schema.py tests/unit/test_metadata_schema.py
git commit -m "feat(metadata): series.json and catalog.json document schema"
```

---

### Task 4: Validate an intercepted PUT (contract §6, first half)

The untrusted boundary. Only the facts fields are validated; everything in `volumes` except each entry's `offset` is discarded (bunko's own compilation wins), unknown keys are ignored, and the alignment numbers are preserved **verbatim** — no clamping, because readers own that rule.

**Files:**
- Create: `src/mokuro_bunko/metadata/validate.py`
- Test: `tests/unit/test_metadata_validate.py`

**Interfaces:**
- Consumes: `schema.SeriesFacts`, `schema.ID_KEYS`, `schema.TITLE_KEYS`, `schema.TRACKING_UNITS`, `reader_compat.normalize_updated_at`.
- Produces: `SeriesUpdate` (frozen dataclass: `facts: SeriesFacts`, `spine_offset: float | None`, `spine_offset_present: bool`, `volume_offsets: dict[str, float]`, `listed_uuids: frozenset[str]`), `parse_series_update(payload: bytes, *, now: float | None = None) -> SeriesUpdate | None`.

- [ ] **Step 1: Write the failing test**

```python
"""Contract §6: validate the facts, keep the offsets verbatim, ignore the rest."""

from __future__ import annotations

import json

from mokuro_bunko.metadata.validate import parse_series_update


def payload(**overrides: object) -> bytes:
    body: dict[str, object] = {
        "version": 2,
        "series_title": "Dr Stone",
        "external_ids": {"anilist": 98416},
        "titles": {"native": "Dr.STONE"},
        "synonyms": ["ドクターストーン"],
        "updated_at": "2026-08-18T19:36:24.324Z",
        "volumes": [],
    }
    body.update(overrides)
    return json.dumps(body).encode("utf-8")


class TestFacts:
    def test_accepts_a_well_formed_update(self) -> None:
        update = parse_series_update(payload(tag="HD Scan", unit="volumes"))
        assert update is not None
        assert update.facts.external_ids == {"anilist": 98416}
        assert update.facts.titles == {"native": "Dr.STONE"}
        assert update.facts.synonyms == ("ドクターストーン",)
        assert update.facts.tag == "HD Scan"
        assert update.facts.unit == "volumes"
        assert update.facts.updated_at == "2026-08-18T19:36:24.324Z"
        assert update.facts.has_facts()

    def test_drops_unknown_providers_languages_and_units(self) -> None:
        update = parse_series_update(
            payload(
                external_ids={"anilist": 98416, "kitsune": 7, "mal": "103897", "bad": -1},
                titles={"native": "Dr.STONE", "klingon": "x", "romaji": "  "},
                unit="chapters-ish",
            )
        )
        assert update is not None
        assert update.facts.external_ids == {"anilist": 98416}
        assert update.facts.titles == {"native": "Dr.STONE"}
        assert update.facts.unit is None

    def test_drops_blank_synonyms_and_a_blank_tag(self) -> None:
        update = parse_series_update(payload(synonyms=["", "  ", "x", 5], tag="   "))
        assert update is not None
        assert update.facts.synonyms == ("x",)
        assert update.facts.tag is None

    def test_ignores_unknown_top_level_keys(self) -> None:
        update = parse_series_update(payload(read_count=9, tracking={"last_pushed": {}}))
        assert update is not None
        assert update.facts.has_facts()

    def test_a_factless_payload_keeps_its_own_stamp(self) -> None:
        update = parse_series_update(
            payload(external_ids={}, titles={}, synonyms=[], updated_at="2026-08-19T00:00:00.000Z")
        )
        assert update is not None
        assert not update.facts.has_facts()
        assert update.facts.updated_at == "2026-08-19T00:00:00.000Z"


class TestRejection:
    def test_rejects_non_json_and_non_objects(self) -> None:
        assert parse_series_update(b"not json") is None
        assert parse_series_update(b"[]") is None
        assert parse_series_update(b"") is None
        assert parse_series_update(b"\xff\xfe") is None

    def test_rejects_unknown_versions(self) -> None:
        assert parse_series_update(payload(version=3)) is None
        assert parse_series_update(payload(version="2")) is None

    def test_rejects_a_missing_or_unparsable_stamp(self) -> None:
        assert parse_series_update(payload(updated_at="Aug 16 2020")) is None
        assert parse_series_update(payload(updated_at=None)) is None

    def test_rejects_nan_and_infinity(self) -> None:
        assert parse_series_update(b'{"version":2,"updated_at":"2026-08-18T19:36:24.324Z",'
                                   b'"spine_offset":NaN}') is None
        assert parse_series_update(b'{"version":2,"updated_at":"2026-08-18T19:36:24.324Z",'
                                   b'"spine_offset":Infinity}') is None

    def test_clamps_a_far_future_stamp_instead_of_trusting_it(self) -> None:
        update = parse_series_update(
            payload(updated_at="2999-01-01T00:00:00.000Z"), now=1_800_000_000.0
        )
        assert update is not None
        assert update.facts.updated_at == "2027-01-15T08:00:00.000Z"


class TestIndexFields:
    def test_offsets_are_preserved_verbatim_never_clamped(self) -> None:
        update = parse_series_update(
            payload(
                spine_offset=9999,
                volumes=[{"volume_uuid": "u1", "offset": -12345.5}],
            )
        )
        assert update is not None
        assert update.spine_offset == 9999
        assert update.spine_offset_present is True
        assert update.volume_offsets == {"u1": -12345.5}

    def test_an_absent_spine_offset_is_silence_not_a_reset(self) -> None:
        update = parse_series_update(payload())
        assert update is not None
        assert update.spine_offset_present is False
        assert update.spine_offset is None

    def test_a_listed_volume_without_an_offset_is_recorded_as_listed(self) -> None:
        update = parse_series_update(
            payload(volumes=[{"volume_uuid": "u1"}, {"volume_uuid": "u2", "offset": 4}])
        )
        assert update is not None
        assert update.listed_uuids == frozenset({"u1", "u2"})
        assert update.volume_offsets == {"u2": 4}

    def test_everything_else_in_a_volume_entry_is_discarded(self) -> None:
        update = parse_series_update(
            payload(
                volumes=[
                    {
                        "volume_uuid": "u1",
                        "volume_title": "LIES",
                        "page_count": 99999,
                        "character_count": 1,
                        "mokuro_version": "9.9",
                        "archive_size": 5,
                    }
                ]
            )
        )
        assert update is not None
        assert update.listed_uuids == frozenset({"u1"})
        assert update.volume_offsets == {}

    def test_junk_volume_entries_are_skipped_individually(self) -> None:
        update = parse_series_update(
            payload(volumes=["nope", {"volume_uuid": ""}, {"volume_uuid": "u1", "offset": "x"},
                             {"volume_uuid": "u2", "offset": 3}])
        )
        assert update is not None
        assert update.listed_uuids == frozenset({"u1", "u2"})
        assert update.volume_offsets == {"u2": 3}

    def test_a_non_list_volumes_key_is_not_fatal(self) -> None:
        update = parse_series_update(payload(volumes="nope"))
        assert update is not None
        assert update.listed_uuids == frozenset()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/unit/test_metadata_validate.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'mokuro_bunko.metadata.validate'`

- [ ] **Step 3: Write the implementation**

`src/mokuro_bunko/metadata/validate.py`:

```python
"""The untrusted boundary: a scoped user's `series.json` PUT (contract §6).

Anyone with an account can send this, so every field is re-validated. Only the
FACTS are validated as facts; the `volumes` array is the client's own index,
which bunko does not trust at all — the single thing read out of it is each
entry's `offset`, matched by `volume_uuid`.

The alignment numbers (`spine_offset`, per-entry `offset`) are stored VERBATIM.
Bunko deliberately does not clamp or range-check them: every reader clamps on
parse (±50 % / ±500 px), so one side owns the range rule and the two can never
disagree about what a stored value means.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any

from mokuro_bunko.metadata.reader_compat import normalize_updated_at
from mokuro_bunko.metadata.schema import ID_KEYS, TITLE_KEYS, TRACKING_UNITS, SeriesFacts


@dataclass(frozen=True)
class SeriesUpdate:
    """A validated update REQUEST — not a file, and not authoritative."""

    facts: SeriesFacts
    spine_offset: float | None
    #: Absence is silence (inherit what is stored); presence replaces.
    spine_offset_present: bool
    volume_offsets: dict[str, float]
    #: Volumes the payload named at all. An entry listed WITHOUT an offset is a
    #: positive statement ("this volume has no nudge") and clears a stored one.
    listed_uuids: frozenset[str]


def _reject_constant(name: str) -> Any:
    raise ValueError(f"unsupported JSON constant: {name}")


def _is_number(value: Any) -> bool:
    """A real, finite JSON number (`True` is an int in Python; exclude it)."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return False
    return value == value and value not in (float("inf"), float("-inf"))


def _facts_from(raw: dict[str, Any], updated_at: str) -> SeriesFacts:
    external_ids: dict[str, int] = {}
    raw_ids = raw.get("external_ids")
    if isinstance(raw_ids, dict):
        for key in ID_KEYS:
            value = raw_ids.get(key)
            if isinstance(value, int) and not isinstance(value, bool) and value > 0:
                external_ids[key] = value

    titles: dict[str, str] = {}
    raw_titles = raw.get("titles")
    if isinstance(raw_titles, dict):
        for key in TITLE_KEYS:
            value = raw_titles.get(key)
            if isinstance(value, str) and value.strip():
                titles[key] = value

    raw_synonyms = raw.get("synonyms")
    synonyms = tuple(
        value
        for value in (raw_synonyms if isinstance(raw_synonyms, list) else [])
        if isinstance(value, str) and value.strip()
    )

    raw_tag = raw.get("tag")
    tag = raw_tag.strip() if isinstance(raw_tag, str) and raw_tag.strip() else None

    raw_unit = raw.get("unit")
    unit = raw_unit if raw_unit in TRACKING_UNITS else None

    return SeriesFacts(
        external_ids=external_ids,
        titles=titles,
        synonyms=synonyms,
        tag=tag,
        unit=unit,
        updated_at=updated_at,
    )


def parse_series_update(payload: bytes, *, now: float | None = None) -> SeriesUpdate | None:
    """Validate a PUT body. `None` means "reject with an ordinary error"."""
    try:
        decoded = json.loads(payload.decode("utf-8"), parse_constant=_reject_constant)
    except (UnicodeDecodeError, ValueError):
        return None
    if not isinstance(decoded, dict):
        return None
    if decoded.get("version") not in (1, 2):
        return None

    updated_at = normalize_updated_at(decoded.get("updated_at"), now=now)
    if updated_at is None:
        return None

    spine_offset_present = _is_number(decoded.get("spine_offset"))
    spine_offset = float(decoded["spine_offset"]) if spine_offset_present else None

    volume_offsets: dict[str, float] = {}
    listed: set[str] = set()
    raw_volumes = decoded.get("volumes")
    if isinstance(raw_volumes, list):
        for raw_entry in raw_volumes:
            if not isinstance(raw_entry, dict):
                continue
            uuid = raw_entry.get("volume_uuid")
            if not isinstance(uuid, str) or not uuid.strip():
                continue
            listed.add(uuid)
            offset = raw_entry.get("offset")
            if _is_number(offset):
                volume_offsets[uuid] = offset

    return SeriesUpdate(
        facts=_facts_from(decoded, updated_at),
        spine_offset=spine_offset,
        spine_offset_present=spine_offset_present,
        volume_offsets=volume_offsets,
        listed_uuids=frozenset(listed),
    )
```

Note on `spine_offset`: it is kept as a `float` while per-volume offsets keep the JSON number's own type, so an integer nudge round-trips as `-40` rather than `-40.0` and the compiled bytes match what a client would have written.

- [ ] **Step 4: Run test to verify it passes**

Run: `uv run pytest tests/unit/test_metadata_validate.py -q`
Expected: PASS (16 tests)

- [ ] **Step 5: Check lint and types**

Run: `uv run ruff check src/ && uv run mypy src/`
Expected: no findings

- [ ] **Step 6: Commit**

```bash
git add src/mokuro_bunko/metadata/validate.py tests/unit/test_metadata_validate.py
git commit -m "feat(metadata): validate intercepted series.json updates"
```

---

### Task 5: Persist facts, offsets and the compiled-entry cache (schema v3)

Storage lives in the existing SQLite database, because `Database` owns the single WAL connection, the busy-timeout retry wrapper and the schema-version bookkeeping — reimplementing any of that beside it would be a second source of truth. Two tables: `series_facts` (one row per series, facts + shelf alignment + who last touched it) and `series_entry_cache` (one row per `.cbz`, the compiled `VolumeEntry` keyed by the source files' stat, so a full regeneration over a 12 000-volume library is a stat walk instead of a re-parse of gigabytes of `.mokuro`).

**Files:**
- Modify: `src/mokuro_bunko/database.py` (`SCHEMA_VERSION` at line 167; `_init_schema` around lines 296–336; new section after `rename_volume_upload`)
- Test: `tests/unit/test_database_series_metadata.py`

**Interfaces:**
- Consumes: nothing from earlier tasks (plain dicts keep `database.py` free of metadata imports, exactly as `UserDict`/`InviteDict` keep it free of API imports).
- Produces: `SeriesFactsRow` TypedDict, `Database.get_series_facts(series_key) -> SeriesFactsRow | None`, `Database.list_series_facts() -> list[SeriesFactsRow]`, `Database.put_series_facts(row: SeriesFactsRow) -> None`, `Database.get_cached_volume_entry(volume_key, cbz_size, cbz_mtime, sidecar_key) -> dict[str, Any] | None`, `Database.put_cached_volume_entry(volume_key, series_key, entry, cbz_size, cbz_mtime, sidecar_key) -> None`, `Database.prune_series_entry_cache(keep_volume_keys) -> int`.

- [ ] **Step 1: Write the failing test**

```python
"""Series facts + compiled-entry cache storage (schema v3)."""

from __future__ import annotations

from pathlib import Path

import pytest

from mokuro_bunko.database import Database, SeriesFactsRow


def make_row(**overrides: object) -> SeriesFactsRow:
    row: SeriesFactsRow = {
        "series_key": "dr stone",
        "series_title": "Dr Stone",
        "external_ids": {"anilist": 98416},
        "titles": {"native": "Dr.STONE"},
        "synonyms": ["ドクターストーン"],
        "tag": "HD Scan",
        "unit": "volumes",
        "facts_updated_at": "2026-08-18T19:36:24.324Z",
        "spine_offset": 12.5,
        "volume_offsets": {"u1": -40},
        "updated_by": "alice",
        "updated_at": "",
    }
    row.update(overrides)  # type: ignore[typeddict-item]
    return row


@pytest.fixture
def db(tmp_path: Path) -> Database:
    return Database(tmp_path / "test.db")


class TestSchema:
    def test_schema_version_is_three(self, db: Database) -> None:
        assert Database.SCHEMA_VERSION == 3
        with db._connection() as conn:
            assert conn.execute("SELECT version FROM schema_version").fetchone()[0] == 3

    def test_upgrading_an_existing_database_adds_the_tables(self, tmp_path: Path) -> None:
        path = tmp_path / "old.db"
        Database(path).create_user("alice", "password123", "registered")
        upgraded = Database(path)
        assert upgraded.get_user("alice") is not None
        assert upgraded.list_series_facts() == []


class TestSeriesFacts:
    def test_round_trips_every_field(self, db: Database) -> None:
        db.put_series_facts(make_row())
        stored = db.get_series_facts("dr stone")
        assert stored is not None
        assert stored["series_title"] == "Dr Stone"
        assert stored["external_ids"] == {"anilist": 98416}
        assert stored["titles"] == {"native": "Dr.STONE"}
        assert stored["synonyms"] == ["ドクターストーン"]
        assert stored["tag"] == "HD Scan"
        assert stored["unit"] == "volumes"
        assert stored["facts_updated_at"] == "2026-08-18T19:36:24.324Z"
        assert stored["spine_offset"] == 12.5
        assert stored["volume_offsets"] == {"u1": -40}
        assert stored["updated_by"] == "alice"
        assert stored["updated_at"]

    def test_missing_series_is_none(self, db: Database) -> None:
        assert db.get_series_facts("nothing") is None

    def test_put_replaces_the_row_wholesale(self, db: Database) -> None:
        db.put_series_facts(make_row())
        db.put_series_facts(
            make_row(tag=None, unit=None, spine_offset=None, volume_offsets={}, updated_by="bob")
        )
        stored = db.get_series_facts("dr stone")
        assert stored is not None
        assert stored["tag"] is None
        assert stored["unit"] is None
        assert stored["spine_offset"] is None
        assert stored["volume_offsets"] == {}
        assert stored["updated_by"] == "bob"

    def test_list_returns_every_series(self, db: Database) -> None:
        db.put_series_facts(make_row())
        db.put_series_facts(make_row(series_key="aria", series_title="Aria"))
        assert {row["series_key"] for row in db.list_series_facts()} == {"dr stone", "aria"}

    def test_corrupt_json_columns_degrade_to_empty(self, db: Database) -> None:
        db.put_series_facts(make_row())
        with db._connection() as conn:
            conn.execute("UPDATE series_facts SET titles = 'not json'")
        stored = db.get_series_facts("dr stone")
        assert stored is not None
        assert stored["titles"] == {}


class TestEntryCache:
    ENTRY = {"volume_uuid": "u1", "volume_title": "v01", "page_count": 187}

    def test_hit_only_when_every_stat_matches(self, db: Database) -> None:
        db.put_cached_volume_entry(
            "Dr Stone/v01.cbz", "dr stone", self.ENTRY, 1234, 100.5, "v01.mokuro:99:50.25"
        )
        assert db.get_cached_volume_entry(
            "Dr Stone/v01.cbz", 1234, 100.5, "v01.mokuro:99:50.25"
        ) == self.ENTRY
        assert db.get_cached_volume_entry(
            "Dr Stone/v01.cbz", 9999, 100.5, "v01.mokuro:99:50.25"
        ) is None
        assert db.get_cached_volume_entry(
            "Dr Stone/v01.cbz", 1234, 100.75, "v01.mokuro:99:50.25"
        ) is None
        assert db.get_cached_volume_entry("Dr Stone/v01.cbz", 1234, 100.5, "") is None
        assert db.get_cached_volume_entry("Other/v01.cbz", 1234, 100.5, "") is None

    def test_put_overwrites_a_stale_entry(self, db: Database) -> None:
        db.put_cached_volume_entry("Dr Stone/v01.cbz", "dr stone", self.ENTRY, 1, 1.0, "")
        db.put_cached_volume_entry(
            "Dr Stone/v01.cbz", "dr stone", {"volume_uuid": "u2"}, 2, 2.0, ""
        )
        assert db.get_cached_volume_entry("Dr Stone/v01.cbz", 2, 2.0, "") == {"volume_uuid": "u2"}

    def test_prune_drops_only_the_keys_not_kept(self, db: Database) -> None:
        db.put_cached_volume_entry("A/v1.cbz", "a", self.ENTRY, 1, 1.0, "")
        db.put_cached_volume_entry("A/v2.cbz", "a", self.ENTRY, 1, 1.0, "")
        db.put_cached_volume_entry("B/v1.cbz", "b", self.ENTRY, 1, 1.0, "")
        assert db.prune_series_entry_cache({"A/v1.cbz", "B/v1.cbz"}) == 1
        assert db.get_cached_volume_entry("A/v2.cbz", 1, 1.0, "") is None
        assert db.get_cached_volume_entry("A/v1.cbz", 1, 1.0, "") == self.ENTRY
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/unit/test_database_series_metadata.py -q`
Expected: FAIL — `ImportError: cannot import name 'SeriesFactsRow' from 'mokuro_bunko.database'`

- [ ] **Step 3: Add the TypedDict and bump the schema version**

In `src/mokuro_bunko/database.py`, after `AuditEventDict` (line 63), add:

```python
class SeriesFactsRow(TypedDict):
    """One series' shareable facts plus its shelf alignment (index data).

    `facts_updated_at` is the FACTS clock — the value that decides merges. It
    is not `updated_at`, which is this row's own bookkeeping stamp and moves
    whenever anything (including an offset) is written.
    """

    series_key: str
    series_title: str
    external_ids: dict[str, int]
    titles: dict[str, str]
    synonyms: list[str]
    tag: str | None
    unit: str | None
    facts_updated_at: str
    spine_offset: float | None
    volume_offsets: dict[str, float]
    updated_by: str | None
    updated_at: str
```

Change `SCHEMA_VERSION = 2` (line 167) to `SCHEMA_VERSION = 3`.

- [ ] **Step 4: Create the tables**

In `_init_schema`, immediately after the `volume_uploads` `CREATE TABLE` block:

```python
            conn.execute("""
                CREATE TABLE IF NOT EXISTS series_facts (
                    series_key TEXT PRIMARY KEY,
                    series_title TEXT NOT NULL,
                    external_ids TEXT NOT NULL DEFAULT '{}',
                    titles TEXT NOT NULL DEFAULT '{}',
                    synonyms TEXT NOT NULL DEFAULT '[]',
                    tag TEXT,
                    unit TEXT,
                    facts_updated_at TEXT NOT NULL,
                    spine_offset REAL,
                    volume_offsets TEXT NOT NULL DEFAULT '{}',
                    updated_by TEXT,
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                )
            """)

            conn.execute("""
                CREATE TABLE IF NOT EXISTS series_entry_cache (
                    volume_key TEXT PRIMARY KEY,
                    series_key TEXT NOT NULL,
                    entry_json TEXT NOT NULL,
                    cbz_size INTEGER NOT NULL,
                    cbz_mtime REAL NOT NULL,
                    sidecar_key TEXT NOT NULL DEFAULT '',
                    computed_at TEXT NOT NULL DEFAULT (datetime('now'))
                )
            """)
```

and next to the other index statements:

```python
            conn.execute("""
                CREATE INDEX IF NOT EXISTS idx_series_entry_cache_series
                ON series_entry_cache(series_key)
            """)
```

- [ ] **Step 5: Add the accessors**

At the end of `database.py`, after `rename_volume_upload`:

```python
    # Series metadata operations

    @staticmethod
    def _load_json_object(raw: Any, fallback: Any) -> Any:
        """Decode a JSON column, degrading to *fallback* on corruption.

        These columns are written by this class alone, but a half-written row
        or a hand-edited database must not take the whole metadata compiler
        down: a series whose facts cannot be read is a factless series.
        """
        if not isinstance(raw, str):
            return fallback
        try:
            decoded = json.loads(raw)
        except ValueError:
            return fallback
        return decoded if isinstance(decoded, type(fallback)) else fallback

    def _series_facts_from_row(self, row: sqlite3.Row) -> SeriesFactsRow:
        return SeriesFactsRow(
            series_key=str(row["series_key"]),
            series_title=str(row["series_title"]),
            external_ids=self._load_json_object(row["external_ids"], {}),
            titles=self._load_json_object(row["titles"], {}),
            synonyms=self._load_json_object(row["synonyms"], []),
            tag=row["tag"],
            unit=row["unit"],
            facts_updated_at=str(row["facts_updated_at"]),
            spine_offset=row["spine_offset"],
            volume_offsets=self._load_json_object(row["volume_offsets"], {}),
            updated_by=row["updated_by"],
            updated_at=str(row["updated_at"]),
        )

    def get_series_facts(self, series_key: str) -> SeriesFactsRow | None:
        """Stored facts for one series, keyed by normalized series title."""
        with self._connection() as conn:
            cursor = conn.execute(
                "SELECT * FROM series_facts WHERE series_key = ?", (series_key,)
            )
            row = cursor.fetchone()
            return self._series_facts_from_row(row) if row else None

    def list_series_facts(self) -> list[SeriesFactsRow]:
        """Every stored series, including ones whose folder is gone."""
        with self._connection() as conn:
            cursor = conn.execute("SELECT * FROM series_facts")
            return [self._series_facts_from_row(row) for row in cursor.fetchall()]

    def put_series_facts(self, row: SeriesFactsRow) -> None:
        """Insert or replace one series' facts and shelf alignment."""
        with self._connection() as conn:
            conn.execute(
                """
                INSERT INTO series_facts (
                    series_key, series_title, external_ids, titles, synonyms,
                    tag, unit, facts_updated_at, spine_offset, volume_offsets,
                    updated_by, updated_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'))
                ON CONFLICT(series_key) DO UPDATE SET
                    series_title = excluded.series_title,
                    external_ids = excluded.external_ids,
                    titles = excluded.titles,
                    synonyms = excluded.synonyms,
                    tag = excluded.tag,
                    unit = excluded.unit,
                    facts_updated_at = excluded.facts_updated_at,
                    spine_offset = excluded.spine_offset,
                    volume_offsets = excluded.volume_offsets,
                    updated_by = excluded.updated_by,
                    updated_at = datetime('now')
                """,
                (
                    row["series_key"],
                    row["series_title"],
                    json.dumps(row["external_ids"], ensure_ascii=False),
                    json.dumps(row["titles"], ensure_ascii=False),
                    json.dumps(row["synonyms"], ensure_ascii=False),
                    row["tag"],
                    row["unit"],
                    row["facts_updated_at"],
                    row["spine_offset"],
                    json.dumps(row["volume_offsets"], ensure_ascii=False),
                    row["updated_by"],
                ),
            )

    def get_cached_volume_entry(
        self,
        volume_key: str,
        cbz_size: int,
        cbz_mtime: float,
        sidecar_key: str,
    ) -> dict[str, Any] | None:
        """A previously compiled volume entry, if the sources are unchanged.

        Every stat is compared in Python rather than in SQL: floats compare
        exactly here (they round-trip through REAL unchanged) and a mismatch
        must be a miss, never an approximate hit.
        """
        with self._connection() as conn:
            cursor = conn.execute(
                "SELECT * FROM series_entry_cache WHERE volume_key = ?", (volume_key,)
            )
            row = cursor.fetchone()
        if row is None:
            return None
        if int(row["cbz_size"]) != cbz_size or float(row["cbz_mtime"]) != cbz_mtime:
            return None
        if str(row["sidecar_key"]) != sidecar_key:
            return None
        entry = self._load_json_object(row["entry_json"], {})
        return cast("dict[str, Any]", entry) if entry else None

    def put_cached_volume_entry(
        self,
        volume_key: str,
        series_key: str,
        entry: dict[str, Any],
        cbz_size: int,
        cbz_mtime: float,
        sidecar_key: str,
    ) -> None:
        """Remember a compiled volume entry against its sources' stat."""
        with self._connection() as conn:
            conn.execute(
                """
                INSERT INTO series_entry_cache (
                    volume_key, series_key, entry_json, cbz_size, cbz_mtime,
                    sidecar_key, computed_at
                ) VALUES (?, ?, ?, ?, ?, ?, datetime('now'))
                ON CONFLICT(volume_key) DO UPDATE SET
                    series_key = excluded.series_key,
                    entry_json = excluded.entry_json,
                    cbz_size = excluded.cbz_size,
                    cbz_mtime = excluded.cbz_mtime,
                    sidecar_key = excluded.sidecar_key,
                    computed_at = datetime('now')
                """,
                (
                    volume_key,
                    series_key,
                    json.dumps(entry, ensure_ascii=False),
                    cbz_size,
                    cbz_mtime,
                    sidecar_key,
                ),
            )

    def prune_series_entry_cache(self, keep_volume_keys: Iterable[str]) -> int:
        """Drop cache rows for volumes that no longer exist. Returns the count."""
        keep = set(keep_volume_keys)
        with self._connection() as conn:
            cursor = conn.execute("SELECT volume_key FROM series_entry_cache")
            stale = [
                (str(row["volume_key"]),)
                for row in cursor.fetchall()
                if str(row["volume_key"]) not in keep
            ]
            if not stale:
                return 0
            conn.executemany("DELETE FROM series_entry_cache WHERE volume_key = ?", stale)
            return len(stale)
```

Add `import json` and `Iterable` to the imports at the top of `database.py` (`from collections.abc import Iterable, Iterator`).

- [ ] **Step 6: Run the new tests and the existing database suites**

Run: `uv run pytest tests/unit/test_database_series_metadata.py tests/unit/test_database.py tests/unit/test_database_resilience.py -q`
Expected: PASS (10 new tests; the existing database suites unchanged and green)

- [ ] **Step 7: Check lint and types**

Run: `uv run ruff check src/ && uv run mypy src/`
Expected: no findings

- [ ] **Step 8: Commit**

```bash
git add src/mokuro_bunko/database.py tests/unit/test_database_series_metadata.py
git commit -m "feat(db): schema v3 with series facts and compiled-entry cache"
```

---

### Task 6: Merge rules (contract §6, second half)

Pure function, no I/O: stored state + validated update → new state. Facts merge newest-facts-stamp-wins with the client's factless rules; the shelf alignment merges independently and never moves the facts stamp.

**Files:**
- Create: `src/mokuro_bunko/metadata/merge.py`
- Test: `tests/unit/test_metadata_merge.py`

**Interfaces:**
- Consumes: `schema.SeriesFacts`, `schema.SeriesIndexData`, `schema.FACTLESS_UPDATED_AT`, `validate.SeriesUpdate`.
- Produces: `StoredSeries` (frozen dataclass: `facts: SeriesFacts`, `index: SeriesIndexData`), `MergeResult` (frozen dataclass: `facts`, `index`, `facts_changed: bool`, `index_changed: bool`, property `changed: bool`), `merge_series_update(stored: StoredSeries | None, update: SeriesUpdate) -> MergeResult`.

- [ ] **Step 1: Write the failing test**

```python
"""Contract §6: newest-facts-stamp-wins, factless rules, index independence."""

from __future__ import annotations

from mokuro_bunko.metadata.merge import StoredSeries, merge_series_update
from mokuro_bunko.metadata.schema import FACTLESS_UPDATED_AT, SeriesFacts, SeriesIndexData
from mokuro_bunko.metadata.validate import SeriesUpdate

OLD = "2026-08-01T00:00:00.000Z"
NEW = "2026-08-20T00:00:00.000Z"


def stored(facts: SeriesFacts, index: SeriesIndexData | None = None) -> StoredSeries:
    return StoredSeries(facts=facts, index=index or SeriesIndexData())


def update(
    facts: SeriesFacts,
    *,
    spine_offset: float | None = None,
    spine_offset_present: bool = False,
    volume_offsets: dict[str, float] | None = None,
    listed: frozenset[str] = frozenset(),
) -> SeriesUpdate:
    return SeriesUpdate(
        facts=facts,
        spine_offset=spine_offset,
        spine_offset_present=spine_offset_present,
        volume_offsets=volume_offsets or {},
        listed_uuids=listed,
    )


LINKED_OLD = SeriesFacts(external_ids={"anilist": 1}, updated_at=OLD)
LINKED_NEW = SeriesFacts(external_ids={"anilist": 2}, updated_at=NEW)


class TestFactsMerge:
    def test_first_update_for_an_unknown_series_is_stored(self) -> None:
        result = merge_series_update(None, update(LINKED_NEW))
        assert result.facts == LINKED_NEW
        assert result.facts_changed

    def test_newer_facts_win(self) -> None:
        result = merge_series_update(stored(LINKED_OLD), update(LINKED_NEW))
        assert result.facts == LINKED_NEW
        assert result.facts_changed

    def test_older_facts_lose(self) -> None:
        result = merge_series_update(stored(LINKED_NEW), update(LINKED_OLD))
        assert result.facts == LINKED_NEW
        assert not result.facts_changed

    def test_an_equal_stamp_keeps_the_incoming_copy(self) -> None:
        same = SeriesFacts(external_ids={"anilist": 1}, tag="HD Scan", updated_at=OLD)
        result = merge_series_update(stored(LINKED_OLD), update(same))
        assert result.facts == same
        assert result.facts_changed

    def test_a_round_trip_of_identical_facts_reports_no_change(self) -> None:
        result = merge_series_update(stored(LINKED_OLD), update(LINKED_OLD))
        assert not result.facts_changed
        assert not result.changed


class TestFactlessRules:
    def test_a_factless_epoch_update_never_clears_facts(self) -> None:
        result = merge_series_update(
            stored(LINKED_OLD), update(SeriesFacts(updated_at=FACTLESS_UPDATED_AT))
        )
        assert result.facts == LINKED_OLD
        assert not result.facts_changed

    def test_a_factless_update_with_the_same_stamp_never_clears_facts(self) -> None:
        result = merge_series_update(stored(LINKED_OLD), update(SeriesFacts(updated_at=OLD)))
        assert result.facts == LINKED_OLD
        assert not result.facts_changed

    def test_a_factless_update_with_a_strictly_newer_stamp_is_an_unlink(self) -> None:
        unlink = SeriesFacts(updated_at=NEW)
        result = merge_series_update(stored(LINKED_OLD), update(unlink))
        assert result.facts == unlink
        assert not result.facts.has_facts()
        assert result.facts_changed

    def test_facts_older_than_a_published_unlink_do_not_resurrect_the_link(self) -> None:
        """The stored row is factless but its stamp is a real unlink."""
        result = merge_series_update(stored(SeriesFacts(updated_at=NEW)), update(LINKED_OLD))
        assert not result.facts.has_facts()
        assert result.facts.updated_at == NEW
        assert not result.facts_changed

    def test_facts_newer_than_an_unlink_relink_the_series(self) -> None:
        result = merge_series_update(stored(SeriesFacts(updated_at=OLD)), update(LINKED_NEW))
        assert result.facts == LINKED_NEW
        assert result.facts_changed


class TestIndexFields:
    def test_offsets_apply_even_when_the_facts_lose(self) -> None:
        result = merge_series_update(
            stored(LINKED_NEW),
            update(LINKED_OLD, volume_offsets={"u1": -40}, listed=frozenset({"u1"})),
        )
        assert result.facts == LINKED_NEW
        assert not result.facts_changed
        assert result.index.volume_offsets == {"u1": -40}
        assert result.index_changed
        assert result.changed

    def test_an_offset_only_update_never_moves_the_facts_stamp(self) -> None:
        result = merge_series_update(
            stored(LINKED_OLD),
            update(
                SeriesFacts(updated_at=NEW),
                spine_offset=12.5,
                spine_offset_present=True,
                volume_offsets={"u1": 3},
                listed=frozenset({"u1"}),
            ),
        )
        # The factless payload IS an unlink here (strictly newer), which is the
        # rule; what must not happen is the OFFSETS moving the facts stamp.
        assert result.index.spine_offset == 12.5
        assert result.index.volume_offsets == {"u1": 3}

    def test_an_absent_spine_offset_inherits_the_stored_one(self) -> None:
        result = merge_series_update(
            stored(LINKED_OLD, SeriesIndexData(spine_offset=8)), update(LINKED_OLD)
        )
        assert result.index.spine_offset == 8
        assert not result.index_changed

    def test_a_present_spine_offset_replaces_the_stored_one(self) -> None:
        result = merge_series_update(
            stored(LINKED_OLD, SeriesIndexData(spine_offset=8)),
            update(LINKED_OLD, spine_offset=0, spine_offset_present=True),
        )
        assert result.index.spine_offset == 0
        assert result.index_changed

    def test_a_listed_volume_without_an_offset_clears_the_stored_one(self) -> None:
        result = merge_series_update(
            stored(LINKED_OLD, SeriesIndexData(volume_offsets={"u1": -40, "u2": 5})),
            update(LINKED_OLD, listed=frozenset({"u1"})),
        )
        assert result.index.volume_offsets == {"u2": 5}
        assert result.index_changed

    def test_a_volume_the_payload_never_mentions_is_untouched(self) -> None:
        result = merge_series_update(
            stored(LINKED_OLD, SeriesIndexData(volume_offsets={"u2": 5})),
            update(LINKED_OLD, volume_offsets={"u1": 1}, listed=frozenset({"u1"})),
        )
        assert result.index.volume_offsets == {"u1": 1, "u2": 5}

    def test_out_of_range_offsets_are_stored_verbatim(self) -> None:
        result = merge_series_update(
            None,
            update(
                LINKED_NEW,
                spine_offset=9999,
                spine_offset_present=True,
                volume_offsets={"u1": -12345.5},
                listed=frozenset({"u1"}),
            ),
        )
        assert result.index.spine_offset == 9999
        assert result.index.volume_offsets == {"u1": -12345.5}


class TestIdempotency:
    def test_applying_the_same_update_twice_changes_nothing_the_second_time(self) -> None:
        first = merge_series_update(
            None, update(LINKED_NEW, volume_offsets={"u1": 3}, listed=frozenset({"u1"}))
        )
        second = merge_series_update(
            StoredSeries(facts=first.facts, index=first.index),
            update(LINKED_NEW, volume_offsets={"u1": 3}, listed=frozenset({"u1"})),
        )
        assert second.facts == first.facts
        assert second.index == first.index
        assert not second.changed
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/unit/test_metadata_merge.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'mokuro_bunko.metadata.merge'`

- [ ] **Step 3: Write the implementation**

`src/mokuro_bunko/metadata/merge.py`:

```python
"""Merging an accepted update into the stored series state (contract §6).

Two independent merges happen here, and conflating them is the bug this file
exists to prevent:

* FACTS merge on the facts stamp — newest wins, ties keep the incoming copy
  (that is the same link round-tripping back), and a factless payload needs a
  STRICTLY newer stamp to win, because that is what an explicit unlink looks
  like and an epoch stamp means "no opinion".
* INDEX merge (the shelf alignment) by presence — a value the payload carries
  replaces the stored one, a value it omits is silence and inherits. It never
  touches the facts stamp, so a PUT carrying only offsets is still factless.

The facts rule below is the reader's `buildSeriesFile` rule rather than the
slightly looser `pickEntry` one: incoming facts must be at least as new as the
STORED STAMP even when the stored row is factless. bunko is the authority that
holds a published unlink, and a stale link arriving afterwards must not
resurrect it.
"""

from __future__ import annotations

from dataclasses import dataclass

from mokuro_bunko.metadata.schema import SeriesFacts, SeriesIndexData
from mokuro_bunko.metadata.validate import SeriesUpdate


@dataclass(frozen=True)
class StoredSeries:
    """What bunko currently holds for one series."""

    facts: SeriesFacts
    index: SeriesIndexData


@dataclass(frozen=True)
class MergeResult:
    facts: SeriesFacts
    index: SeriesIndexData
    facts_changed: bool
    index_changed: bool

    @property
    def changed(self) -> bool:
        """Did anything move? Drives "rewrite the files or not"."""
        return self.facts_changed or self.index_changed


def _merge_facts(stored: SeriesFacts | None, incoming: SeriesFacts) -> SeriesFacts:
    if stored is None:
        return incoming
    if incoming.has_facts():
        return incoming if incoming.updated_at >= stored.updated_at else stored
    # Factless: only a deliberate, strictly newer unlink wins.
    return incoming if incoming.updated_at > stored.updated_at else stored


def _merge_index(stored: SeriesIndexData | None, update: SeriesUpdate) -> SeriesIndexData:
    base = stored or SeriesIndexData()

    spine_offset = update.spine_offset if update.spine_offset_present else base.spine_offset

    offsets = dict(base.volume_offsets)
    for uuid in update.listed_uuids:
        if uuid in update.volume_offsets:
            offsets[uuid] = update.volume_offsets[uuid]
        else:
            # Listed without an offset: a positive statement that this volume
            # has no nudge, so a stored one is cleared.
            offsets.pop(uuid, None)
    return SeriesIndexData(spine_offset=spine_offset, volume_offsets=offsets)


def merge_series_update(stored: StoredSeries | None, update: SeriesUpdate) -> MergeResult:
    """Fold a validated update into the stored state."""
    facts = _merge_facts(stored.facts if stored else None, update.facts)
    index = _merge_index(stored.index if stored else None, update)
    return MergeResult(
        facts=facts,
        index=index,
        facts_changed=stored is None or facts != stored.facts,
        index_changed=stored is None or index != stored.index,
    )
```

- [ ] **Step 4: Run test to verify it passes**

Run: `uv run pytest tests/unit/test_metadata_merge.py -q`
Expected: PASS (18 tests)

- [ ] **Step 5: Check lint and types**

Run: `uv run ruff check src/ && uv run mypy src/`
Expected: no findings

- [ ] **Step 6: Commit**

```bash
git add src/mokuro_bunko/metadata/merge.py tests/unit/test_metadata_merge.py
git commit -m "feat(metadata): newest-facts-stamp-wins merge with factless rules"
```

---

### Task 7: Compile a series folder into volume entries (contract §2)

Filesystem → `VolumeEntry` list. One entry per `.cbz`, because a `.cbz` is what a reader can actually download; a stray sidecar without an archive is not a volume (the same rule `library_index.py` already applies). Volume titles come from the `.cbz` stem and the series title from the folder — never from the `.mokuro`'s own `title`/`volume` fields, which real files get wrong (a live library has `"title": "v01_h3rbbi_d"`), and which the reader matches against `.cbz` filenames anyway.

**Files:**
- Create: `src/mokuro_bunko/metadata/compiler.py`
- Test: `tests/unit/test_metadata_compiler.py`

**Interfaces:**
- Consumes: `schema.VolumeEntry`, `reader_compat.count_page_chars`, `reader_compat.deterministic_uuid`, `reader_compat.natural_sort_key`, `reader_compat.normalize_series_key`, `database.Database` (optional, for the entry cache).
- Produces: `SeriesFolder` (frozen dataclass: `title: str`, `path: Path`), `iter_series_folders(library_path: Path) -> list[SeriesFolder]`, `compile_series_volumes(series: SeriesFolder, *, database: Database | None = None) -> list[VolumeEntry]`, `volume_key_for(series_title: str, volume_title: str) -> str`.

- [ ] **Step 1: Write the failing test**

```python
"""Contract §2: a series folder compiles into the reader's volume entries."""

from __future__ import annotations

import gzip
import json
import zipfile
from pathlib import Path

import pytest

from mokuro_bunko.database import Database
from mokuro_bunko.metadata.compiler import (
    SeriesFolder,
    compile_series_volumes,
    iter_series_folders,
)


def write_cbz(path: Path, pages: int = 2) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w") as archive:
        for index in range(pages):
            archive.writestr(f"{index:03d}.jpg", b"fake image bytes")
        archive.writestr("notes.txt", b"not an image")


def mokuro_payload(**overrides: object) -> dict[str, object]:
    payload: dict[str, object] = {
        "version": "0.2.2",
        "title": "v01_h3rbbi_d",
        "title_uuid": "f944ebce-5b9e-41f0-b15f-1e637ee157f7",
        "volume": "v01",
        "volume_uuid": "cfb5220c-57db-4008-9f44-e659d794e381",
        "pages": [
            {"blocks": [{"lines": ["世界", "abc"]}]},
            {"blocks": [{"lines": ["ねこ"]}]},
        ],
    }
    payload.update(overrides)
    return payload


@pytest.fixture
def library(tmp_path: Path) -> Path:
    root = tmp_path / "library"
    root.mkdir()
    return root


class TestIterSeriesFolders:
    def test_lists_top_level_folders_that_hold_an_archive(self, library: Path) -> None:
        write_cbz(library / "Dr Stone" / "Volume 01.cbz")
        write_cbz(library / "Aria" / "v1.cbz")
        (library / "Empty").mkdir()
        (library / ".hidden").mkdir()
        write_cbz(library / ".hidden" / "x.cbz")
        write_cbz(library / "loose.cbz")
        assert [folder.title for folder in iter_series_folders(library)] == ["Aria", "Dr Stone"]

    def test_nested_folders_are_not_series(self, library: Path) -> None:
        write_cbz(library / "Dr Stone" / "extras" / "bonus.cbz")
        assert iter_series_folders(library) == []

    def test_a_missing_library_is_empty_not_an_error(self, tmp_path: Path) -> None:
        assert iter_series_folders(tmp_path / "nope") == []


class TestCompileVolumes:
    def test_reads_uuid_pages_version_and_counts_chars_itself(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "Volume 01.cbz")
        (series / "Volume 01.mokuro").write_text(
            json.dumps(mokuro_payload()), encoding="utf-8"
        )
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.volume_uuid == "cfb5220c-57db-4008-9f44-e659d794e381"
        assert entry.volume_title == "Volume 01"   # the .cbz stem, not the .mokuro's "volume"
        assert entry.page_count == 2
        assert entry.character_count == 4          # 世界 + ねこ, "abc" ignored
        assert entry.mokuro_version == "0.2.2"
        assert entry.spine_width is None
        assert entry.archive_size == (series / "Volume 01.cbz").stat().st_size

    def test_prefers_an_explicit_chars_total_when_the_file_carries_one(
        self, library: Path
    ) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(
            json.dumps(mokuro_payload(chars=13247)), encoding="utf-8"
        )
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.character_count == 13247

    def test_carries_the_readers_spine_width_extension(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(
            json.dumps(mokuro_payload(spine_width=250.5)), encoding="utf-8"
        )
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.spine_width == 250.5

    def test_reads_gzipped_sidecars(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        with gzip.open(series / "v1.mokuro.gz", "wt", encoding="utf-8") as handle:
            json.dump(mokuro_payload(), handle)
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_version == "0.2.2"
        assert entry.page_count == 2

    def test_image_only_volume_gets_an_empty_version_and_a_derived_uuid(
        self, library: Path
    ) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "Volume 01.cbz", pages=3)
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_version == ""
        assert entry.character_count == 0
        assert entry.page_count == 3               # images in the archive, notes.txt ignored
        # The same uuid the reader's placeholder derives, so progress attaches.
        assert entry.volume_uuid == "38d6c0d6-1bef-4134-a339-a1e254c6"

    def test_a_corrupt_sidecar_degrades_to_image_only(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "Volume 01.cbz", pages=3)
        (series / "Volume 01.mokuro").write_text("{ this is not json", encoding="utf-8")
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_version == ""
        assert entry.page_count == 3
        assert entry.volume_uuid == "38d6c0d6-1bef-4134-a339-a1e254c6"

    def test_a_sidecar_without_an_archive_is_not_a_volume(self, library: Path) -> None:
        series = library / "Dr Stone"
        series.mkdir()
        (series / "ghost.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        assert compile_series_volumes(SeriesFolder("Dr Stone", series)) == []

    def test_entries_come_back_in_natural_order(self, library: Path) -> None:
        series = library / "Dr Stone"
        for name in ("Volume 10", "Volume 2", "Volume 1"):
            write_cbz(series / f"{name}.cbz")
        titles = [e.volume_title for e in compile_series_volumes(SeriesFolder("Dr Stone", series))]
        assert titles == ["Volume 1", "Volume 2", "Volume 10"]

    def test_cover_sidecars_and_markers_are_not_volumes(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.webp").write_bytes(b"fake webp")
        (series / "v2.nocover").touch()
        (series / "series.json").write_text("{}", encoding="utf-8")
        assert [e.volume_title for e in compile_series_volumes(SeriesFolder("Dr Stone", series))] == [
            "v1"
        ]


class TestEntryCache:
    def test_a_cached_entry_is_used_instead_of_reparsing(
        self, library: Path, tmp_path: Path
    ) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        database = Database(tmp_path / "test.db")

        # Seed a deliberately wrong entry against the real stats: if the
        # compiler consults the cache, this is what comes back.
        cbz_stat = (series / "v1.cbz").stat()
        sidecar_stat = (series / "v1.mokuro").stat()
        database.put_cached_volume_entry(
            "Dr Stone/v1.cbz",
            "dr stone",
            {
                "volume_uuid": "cached",
                "volume_title": "v1",
                "page_count": 999,
                "character_count": 888,
                "mokuro_version": "cached",
            },
            cbz_stat.st_size,
            cbz_stat.st_mtime,
            f"v1.mokuro:{sidecar_stat.st_size}:{sidecar_stat.st_mtime}",
        )
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert entry.page_count == 999
        assert entry.mokuro_version == "cached"

    def test_a_changed_sidecar_invalidates_the_cache_and_is_rewritten(
        self, library: Path, tmp_path: Path
    ) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        database = Database(tmp_path / "test.db")

        first = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert first[0].character_count == 4

        (series / "v1.mokuro").write_text(
            json.dumps(mokuro_payload(chars=500)), encoding="utf-8"
        )
        second = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert second[0].character_count == 500

        # And the fresh value is what the cache now holds.
        cbz_stat = (series / "v1.cbz").stat()
        sidecar_stat = (series / "v1.mokuro").stat()
        cached = database.get_cached_volume_entry(
            "Dr Stone/v1.cbz",
            cbz_stat.st_size,
            cbz_stat.st_mtime,
            f"v1.mokuro:{sidecar_stat.st_size}:{sidecar_stat.st_mtime}",
        )
        assert cached is not None
        assert cached["character_count"] == 500
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/unit/test_metadata_compiler.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'mokuro_bunko.metadata.compiler'`

- [ ] **Step 3: Write the implementation**

`src/mokuro_bunko/metadata/compiler.py`:

```python
"""Turn a series folder on disk into the reader's volume index (contract §2).

One entry per `.cbz`: an archive is what a reader can download, and a sidecar
without one is not a volume (the same rule `library_index.py` applies). The
series title is the FOLDER name and the volume title is the archive's stem —
never the `.mokuro`'s own `title`/`volume`, which real-world files get wrong
and which the reader matches against `.cbz` filenames anyway.

Parsing every `.mokuro` on every regeneration would mean re-reading gigabytes
on a large library, so each compiled entry is cached against the stat of the
files it came from; a regeneration that changes nothing is a stat walk.
"""

from __future__ import annotations

import gzip
import json
import os
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Any

from mokuro_bunko.metadata.reader_compat import (
    count_page_chars,
    deterministic_uuid,
    natural_sort_key,
    normalize_series_key,
)
from mokuro_bunko.metadata.schema import VolumeEntry

if TYPE_CHECKING:
    from mokuro_bunko.database import Database

_IMAGE_SUFFIXES = frozenset(
    {".jpg", ".jpeg", ".png", ".gif", ".bmp", ".webp", ".tiff", ".tif"}
)


@dataclass(frozen=True)
class SeriesFolder:
    """A top-level library folder that holds at least one archive."""

    title: str
    path: Path


def volume_key_for(series_title: str, volume_title: str) -> str:
    """Library-relative key of a volume's archive — the entry cache's key."""
    return f"{series_title}/{volume_title}.cbz"


def iter_series_folders(library_path: Path) -> list[SeriesFolder]:
    """Top-level folders holding at least one `.cbz`, sorted by name.

    Deliberately NOT `LibraryIndexCache`: that is a 30-second TTL cache and it
    indexes nested folders at any depth, while a regeneration must see the
    filesystem as it is right now and only the one level the reader treats as
    a series.
    """
    folders: list[SeriesFolder] = []
    try:
        entries = sorted(os.scandir(library_path), key=lambda item: item.name)
    except OSError:
        return []
    for entry in entries:
        if entry.name.startswith(".") or not entry.is_dir(follow_symlinks=True):
            continue
        if _has_archive(Path(entry.path)):
            folders.append(SeriesFolder(title=entry.name, path=Path(entry.path)))
    return folders


def _has_archive(folder: Path) -> bool:
    try:
        with os.scandir(folder) as scan:
            return any(
                item.is_file(follow_symlinks=True) and item.name.lower().endswith(".cbz")
                for item in scan
            )
    except OSError:
        return False


def _archive_names(folder: Path) -> list[str]:
    try:
        with os.scandir(folder) as scan:
            return sorted(
                item.name
                for item in scan
                if item.is_file(follow_symlinks=True) and item.name.lower().endswith(".cbz")
            )
    except OSError:
        return []


def _sidecar_for(cbz_path: Path) -> Path | None:
    """`<stem>.mokuro`, else `<stem>.mokuro.gz`, else None."""
    base = cbz_path.with_suffix("")
    for suffix in (".mokuro", ".mokuro.gz"):
        candidate = Path(f"{base}{suffix}")
        if candidate.is_file():
            return candidate
    return None


def _stat_key(path: Path | None) -> str:
    """Compact identity of a sidecar for cache validation ("" = none)."""
    if path is None:
        return ""
    try:
        stat_result = path.stat()
    except OSError:
        return ""
    return f"{path.name}:{stat_result.st_size}:{stat_result.st_mtime}"


def _read_sidecar(path: Path) -> dict[str, Any] | None:
    """Parse a `.mokuro`/`.mokuro.gz`; None when unreadable or not an object."""
    try:
        if path.name.lower().endswith(".gz"):
            with gzip.open(path, "rt", encoding="utf-8") as handle:
                data = json.load(handle)
        else:
            with path.open("r", encoding="utf-8") as handle:
                data = json.load(handle)
    except (OSError, UnicodeDecodeError, ValueError, EOFError):
        return None
    return data if isinstance(data, dict) else None


def _count_archive_images(cbz_path: Path) -> int:
    """Page count for an image-only volume: images inside the archive."""
    try:
        with zipfile.ZipFile(cbz_path, "r") as archive:
            return sum(
                1
                for name in archive.namelist()
                if Path(name).suffix.lower() in _IMAGE_SUFFIXES
            )
    except (zipfile.BadZipFile, OSError, EOFError):
        return 0


def _positive_number(value: Any) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value) if value > 0 else None


def _compile_volume(series_title: str, cbz_path: Path, sidecar: Path | None) -> VolumeEntry:
    volume_title = cbz_path.with_suffix("").name
    data = _read_sidecar(sidecar) if sidecar is not None else None

    try:
        archive_size = cbz_path.stat().st_size
    except OSError:
        archive_size = 0

    if data is None:
        # Image-only (or an unreadable sidecar): the reader derives this uuid
        # for its placeholder, so deriving the same one keeps synced progress
        # attached when the index arrives.
        return VolumeEntry(
            volume_uuid=deterministic_uuid(f"{series_title}/{volume_title}"),
            volume_title=volume_title,
            page_count=_count_archive_images(cbz_path),
            character_count=0,
            mokuro_version="",
            archive_size=archive_size or None,
        )

    pages = data.get("pages")
    raw_uuid = data.get("volume_uuid")
    uuid = (
        raw_uuid
        if isinstance(raw_uuid, str) and raw_uuid.strip()
        else deterministic_uuid(f"{series_title}/{volume_title}")
    )
    raw_version = data.get("version")
    version = raw_version if isinstance(raw_version, str) else ""

    # Upstream `.mokuro` files carry no `chars` key (only files the reader
    # itself wrote do), so counting is the normal path, not the fallback.
    raw_chars = data.get("chars")
    if isinstance(raw_chars, int) and not isinstance(raw_chars, bool) and raw_chars > 0:
        character_count = raw_chars
    else:
        character_count = count_page_chars(pages)

    return VolumeEntry(
        volume_uuid=uuid,
        volume_title=volume_title,
        page_count=len(pages) if isinstance(pages, list) else _count_archive_images(cbz_path),
        character_count=character_count,
        mokuro_version=version,
        spine_width=_positive_number(data.get("spine_width")),
        archive_size=archive_size or None,
    )


def _entry_to_dict(entry: VolumeEntry) -> dict[str, Any]:
    return {
        "volume_uuid": entry.volume_uuid,
        "volume_title": entry.volume_title,
        "page_count": entry.page_count,
        "character_count": entry.character_count,
        "mokuro_version": entry.mokuro_version,
        "spine_width": entry.spine_width,
        "archive_size": entry.archive_size,
    }


def _entry_from_dict(raw: dict[str, Any]) -> VolumeEntry | None:
    try:
        return VolumeEntry(
            volume_uuid=str(raw["volume_uuid"]),
            volume_title=str(raw["volume_title"]),
            page_count=int(raw["page_count"]),
            character_count=int(raw["character_count"]),
            mokuro_version=str(raw["mokuro_version"]),
            spine_width=raw.get("spine_width"),
            archive_size=raw.get("archive_size"),
        )
    except (KeyError, TypeError, ValueError):
        return None


def compile_series_volumes(
    series: SeriesFolder,
    *,
    database: Database | None = None,
) -> list[VolumeEntry]:
    """Every volume of one series, in natural title order."""
    series_key = normalize_series_key(series.title)
    entries: list[VolumeEntry] = []

    for name in _archive_names(series.path):
        cbz_path = series.path / name
        volume_title = cbz_path.with_suffix("").name
        sidecar = _sidecar_for(cbz_path)
        sidecar_key = _stat_key(sidecar)
        try:
            cbz_stat = cbz_path.stat()
        except OSError:
            continue

        key = volume_key_for(series.title, volume_title)
        entry: VolumeEntry | None = None
        if database is not None:
            cached = database.get_cached_volume_entry(
                key, cbz_stat.st_size, cbz_stat.st_mtime, sidecar_key
            )
            if cached is not None:
                entry = _entry_from_dict(cached)

        if entry is None:
            entry = _compile_volume(series.title, cbz_path, sidecar)
            if database is not None:
                database.put_cached_volume_entry(
                    key,
                    series_key,
                    _entry_to_dict(entry),
                    cbz_stat.st_size,
                    cbz_stat.st_mtime,
                    sidecar_key,
                )
        entries.append(entry)

    entries.sort(key=lambda item: natural_sort_key(item.volume_title))
    return entries
```

- [ ] **Step 4: Run test to verify it passes**

Run: `uv run pytest tests/unit/test_metadata_compiler.py -q`
Expected: PASS (14 tests)

- [ ] **Step 5: Check lint and types**

Run: `uv run ruff check src/ && uv run mypy src/`
Expected: no findings

- [ ] **Step 6: Commit**

```bash
git add src/mokuro_bunko/metadata/compiler.py tests/unit/test_metadata_compiler.py
git commit -m "feat(metadata): compile series folders into volume entries"
```

---

### Task 8: Publish compiled bytes without churning mtime (contract §4)

Clients version their caches on `size`/`mtime`, so a rebuild that produced identical bytes must not touch the file. Writes go through the same per-path lock DAV writes use, so a compiled write can never interleave with a folder MOVE or an upload on the same path.

**Files:**
- Modify: `src/mokuro_bunko/webdav/resources.py` (add a public lock helper next to `_try_acquire_all`, around line 91)
- Create: `src/mokuro_bunko/metadata/files.py`
- Test: `tests/unit/test_metadata_files.py`

**Interfaces:**
- Consumes: `webdav.resources._PATH_WRITE_LOCKS` (via the new public wrapper).
- Produces: `webdav.resources.path_write_lock(path: Path) -> ContextManager[None]`; `metadata.files.MetadataWriteBusy` (Exception), `metadata.files.write_if_changed(path: Path, data: bytes) -> bool`, `metadata.files.atomic_write_bytes(path: Path, data: bytes) -> None`.

- [ ] **Step 1: Write the failing test**

```python
"""Compiled files are published atomically and only when they changed."""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from mokuro_bunko.metadata.files import MetadataWriteBusy, write_if_changed
from mokuro_bunko.webdav.resources import _PATH_WRITE_LOCKS


@pytest.fixture(autouse=True)
def _clean_global_locks() -> None:
    """The registry is module-global; keep tests independent."""
    _PATH_WRITE_LOCKS._locks.clear()


class TestWriteIfChanged:
    def test_creates_a_missing_file(self, tmp_path: Path) -> None:
        target = tmp_path / "series.json" 
        assert write_if_changed(target, b'{"version":2}') is True
        assert target.read_bytes() == b'{"version":2}'

    def test_creates_missing_parent_directories(self, tmp_path: Path) -> None:
        target = tmp_path / "Dr Stone" / "series.json"
        assert write_if_changed(target, b"{}") is True
        assert target.read_bytes() == b"{}"

    def test_identical_bytes_leave_the_file_untouched(self, tmp_path: Path) -> None:
        target = tmp_path / "catalog.json"
        write_if_changed(target, b"same")
        before = target.stat()
        os.utime(target, (before.st_atime, before.st_mtime - 60))
        stamped = target.stat().st_mtime

        assert write_if_changed(target, b"same") is False
        assert target.stat().st_mtime == stamped

    def test_changed_bytes_are_rewritten(self, tmp_path: Path) -> None:
        target = tmp_path / "catalog.json"
        write_if_changed(target, b"one")
        assert write_if_changed(target, b"two") is True
        assert target.read_bytes() == b"two"

    def test_no_temporary_files_are_left_behind(self, tmp_path: Path) -> None:
        target = tmp_path / "catalog.json"
        write_if_changed(target, b"one")
        write_if_changed(target, b"two")
        assert [p.name for p in tmp_path.iterdir()] == ["catalog.json"]

    def test_a_locked_path_raises_instead_of_writing(self, tmp_path: Path) -> None:
        target = tmp_path / "series.json"
        write_if_changed(target, b"one")
        assert _PATH_WRITE_LOCKS.acquire(target)
        try:
            with pytest.raises(MetadataWriteBusy):
                write_if_changed(target, b"two")
        finally:
            _PATH_WRITE_LOCKS.release(target)
        assert target.read_bytes() == b"one"

    def test_a_locked_ancestor_also_blocks(self, tmp_path: Path) -> None:
        folder = tmp_path / "Dr Stone"
        folder.mkdir()
        target = folder / "series.json"
        assert _PATH_WRITE_LOCKS.acquire(folder)
        try:
            with pytest.raises(MetadataWriteBusy):
                write_if_changed(target, b"{}")
        finally:
            _PATH_WRITE_LOCKS.release(folder)
        assert not target.exists()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/unit/test_metadata_files.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'mokuro_bunko.metadata.files'`

- [ ] **Step 3: Expose the write lock**

In `src/mokuro_bunko/webdav/resources.py`, extend the imports with `from collections.abc import Callable, Iterator` and `from contextlib import contextmanager`, then add after `_try_acquire_all`:

```python
@contextmanager
def path_write_lock(path: Path) -> Iterator[None]:
    """Hold the per-path write lock for a non-DAV writer.

    The compiled metadata files are written by the server itself, outside the
    DAV request path, but they live in the same tree: taking the same lock is
    what stops a regeneration from interleaving with an upload or a folder
    MOVE. Raises `DAVError(423)` when the path (or an ancestor) is busy.
    """
    if not _PATH_WRITE_LOCKS.acquire(path):
        raise DAVError(_HTTP_LOCKED, _LOCKED_MESSAGE)
    try:
        yield
    finally:
        _PATH_WRITE_LOCKS.release(path)
```

- [ ] **Step 4: Write `metadata/files.py`**

```python
"""Publishing compiled bytes to the library tree.

Two rules, both from contract §4: a file is rewritten ONLY when its bytes
changed (clients version their caches on size/mtime, so a no-op rewrite makes
every device re-download), and a rewrite is atomic (a reader mid-GET never
sees a half-written document).
"""

from __future__ import annotations

import os
import tempfile
from pathlib import Path

from wsgidav.dav_provider import DAVError

from mokuro_bunko.webdav.resources import path_write_lock


class MetadataWriteBusy(RuntimeError):
    """The path is locked by a DAV write; retry on the next regeneration."""


def atomic_write_bytes(path: Path, data: bytes) -> None:
    """Write *data* to *path* via a temp file in the same directory."""
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temp_name = tempfile.mkstemp(
        prefix=f".{path.name}.compile-", suffix=".tmp", dir=str(path.parent)
    )
    temp_path = Path(temp_name)
    try:
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temp_path, path)
    except BaseException:
        temp_path.unlink(missing_ok=True)
        raise
    if os.name != "nt":
        # mkstemp creates 0o600; match the umask like _AtomicFileWriter does,
        # or the file becomes unreadable to the download path under nginx.
        umask = os.umask(0)
        os.umask(umask)
        os.chmod(path, 0o666 & ~umask)


def write_if_changed(path: Path, data: bytes) -> bool:
    """Publish *data* unless the file already says exactly that.

    Returns True when the file was written.
    """
    try:
        if path.read_bytes() == data:
            return False
    except OSError:
        pass  # missing or unreadable: write it
    try:
        with path_write_lock(path):
            atomic_write_bytes(path, data)
    except DAVError as error:
        raise MetadataWriteBusy(str(path)) from error
    return True
```

- [ ] **Step 5: Run test to verify it passes**

Run: `uv run pytest tests/unit/test_metadata_files.py tests/unit/test_atomic_writes_locks.py -q`
Expected: PASS (7 new tests; the existing lock suite unchanged and green)

- [ ] **Step 6: Check lint and types**

Run: `uv run ruff check src/ && uv run mypy src/`
Expected: no findings

- [ ] **Step 7: Commit**

```bash
git add src/mokuro_bunko/webdav/resources.py src/mokuro_bunko/metadata/files.py \
        tests/unit/test_metadata_files.py
git commit -m "feat(metadata): atomic, mtime-stable publishing of compiled files"
```

---

### Task 9: The metadata service (contract §2, §3, §4, §6)

The one stateful object: it applies accepted updates, compiles and publishes both files, and debounces bursts. Everything it uses was built in Tasks 3–8.

**Files:**
- Create: `src/mokuro_bunko/metadata/service.py`
- Modify: `src/mokuro_bunko/metadata/__init__.py` (export `MetadataService`)
- Test: `tests/unit/test_metadata_service.py`

**Interfaces:**
- Consumes: `compiler.iter_series_folders`, `compiler.compile_series_volumes`, `compiler.volume_key_for`, `compiler.SeriesFolder`, `schema.*`, `validate.parse_series_update`, `merge.merge_series_update`, `merge.StoredSeries`, `files.write_if_changed`, `files.MetadataWriteBusy`, `database.Database`, `database.SeriesFactsRow`, `paths.CATALOG_FILE_NAME`, `paths.SERIES_FILE_NAME`, `reader_compat.normalize_series_key`.
- Produces: `MetadataService(library_path: Path, database: Database, *, on_published: Callable[[], None] | None = None, debounce_seconds: float = 10.0)` with `apply_series_update(series_title: str, payload: bytes, actor: str | None) -> bool`, `regenerate_series(series_title: str) -> bool`, `regenerate_all() -> int`, `schedule_regeneration(delay: float | None = None) -> None`, `stop() -> None`.

- [ ] **Step 1: Write the failing test**

```python
"""The service: compile, publish, apply updates, stay idempotent."""

from __future__ import annotations

import json
import os
import zipfile
from pathlib import Path

import pytest

from mokuro_bunko.database import Database
from mokuro_bunko.metadata.service import MetadataService
from mokuro_bunko.webdav.resources import _PATH_WRITE_LOCKS


@pytest.fixture(autouse=True)
def _clean_global_locks() -> None:
    _PATH_WRITE_LOCKS._locks.clear()


def write_volume(library: Path, series: str, volume: str, *, sidecar: bool = True) -> None:
    folder = library / series
    folder.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(folder / f"{volume}.cbz", "w") as archive:
        archive.writestr("000.jpg", b"fake image bytes")
        archive.writestr("001.jpg", b"fake image bytes")
    if sidecar:
        (folder / f"{volume}.mokuro").write_text(
            json.dumps(
                {
                    "version": "0.2.2",
                    "title": series,
                    "title_uuid": "t-uuid",
                    "volume": volume,
                    "volume_uuid": f"uuid-{volume}",
                    "pages": [{"blocks": [{"lines": ["世界"]}]}, {"blocks": []}],
                }
            ),
            encoding="utf-8",
        )


def series_update(**overrides: object) -> bytes:
    body: dict[str, object] = {
        "version": 2,
        "series_title": "Dr Stone",
        "external_ids": {"anilist": 98416},
        "titles": {"native": "Dr.STONE"},
        "synonyms": [],
        "updated_at": "2026-08-18T19:36:24.324Z",
        "volumes": [],
    }
    body.update(overrides)
    return json.dumps(body).encode("utf-8")


@pytest.fixture
def library(tmp_path: Path) -> Path:
    path = tmp_path / "library"
    path.mkdir()
    return path


@pytest.fixture
def service(library: Path, tmp_path: Path) -> MetadataService:
    return MetadataService(library, Database(tmp_path / "test.db"))


class TestRegeneration:
    def test_compiles_a_sidecar_per_series_and_one_catalog(
        self, service: MetadataService, library: Path
    ) -> None:
        write_volume(library, "Dr Stone", "Volume 01")
        write_volume(library, "Aria", "v1", sidecar=False)

        assert service.regenerate_all() == 3  # two sidecars + the catalog

        dr_stone = json.loads((library / "Dr Stone" / "series.json").read_text("utf-8"))
        assert dr_stone["version"] == 2
        assert dr_stone["series_title"] == "Dr Stone"
        assert dr_stone["updated_at"] == "1970-01-01T00:00:00.000Z"
        assert dr_stone["volumes"] == [
            {
                "volume_uuid": "uuid-Volume 01",
                "volume_title": "Volume 01",
                "page_count": 2,
                "character_count": 2,
                "mokuro_version": "0.2.2",
                "archive_size": (library / "Dr Stone" / "Volume 01.cbz").stat().st_size,
            }
        ]

        catalog = json.loads((library / "catalog.json").read_text("utf-8"))
        assert [entry["series_title"] for entry in catalog["series"]] == ["Aria", "Dr Stone"]
        assert catalog["series"][0]["updated_at"] == "1970-01-01T00:00:00.000Z"

    def test_image_only_series_still_gets_an_index(
        self, service: MetadataService, library: Path
    ) -> None:
        write_volume(library, "Aria", "v1", sidecar=False)
        service.regenerate_all()
        aria = json.loads((library / "Aria" / "series.json").read_text("utf-8"))
        assert aria["volumes"][0]["mokuro_version"] == ""
        assert aria["volumes"][0]["character_count"] == 0

    def test_a_second_pass_touches_nothing(
        self, service: MetadataService, library: Path
    ) -> None:
        write_volume(library, "Dr Stone", "Volume 01")
        service.regenerate_all()
        sidecar = library / "Dr Stone" / "series.json"
        catalog = library / "catalog.json"
        old = sidecar.stat().st_mtime - 60
        os.utime(sidecar, (old, old))
        os.utime(catalog, (old, old))

        assert service.regenerate_all() == 0
        assert sidecar.stat().st_mtime == old
        assert catalog.stat().st_mtime == old

    def test_a_deleted_series_drops_out_of_the_catalog(
        self, service: MetadataService, library: Path
    ) -> None:
        write_volume(library, "Dr Stone", "Volume 01")
        write_volume(library, "Aria", "v1")
        service.regenerate_all()

        for path in sorted((library / "Aria").iterdir()):
            path.unlink()
        (library / "Aria").rmdir()
        service.regenerate_all()

        catalog = json.loads((library / "catalog.json").read_text("utf-8"))
        assert [entry["series_title"] for entry in catalog["series"]] == ["Dr Stone"]

    def test_an_empty_library_publishes_an_empty_catalog(
        self, service: MetadataService, library: Path
    ) -> None:
        service.regenerate_all()
        assert (library / "catalog.json").read_text("utf-8") == (
            '{"version":1,"updated_at":"1970-01-01T00:00:00.000Z","series":[]}'
        )

    def test_a_locked_series_folder_is_skipped_not_fatal(
        self, service: MetadataService, library: Path
    ) -> None:
        write_volume(library, "Dr Stone", "Volume 01")
        assert _PATH_WRITE_LOCKS.acquire(library / "Dr Stone")
        try:
            service.regenerate_all()  # must not raise
        finally:
            _PATH_WRITE_LOCKS.release(library / "Dr Stone")
        service.stop()  # the skip scheduled a retry; do not let it fire mid-suite
        assert not (library / "Dr Stone" / "series.json").exists()

    def test_publish_hook_fires_only_when_something_changed(
        self, library: Path, tmp_path: Path
    ) -> None:
        calls: list[int] = []
        service = MetadataService(
            library, Database(tmp_path / "test.db"), on_published=lambda: calls.append(1)
        )
        write_volume(library, "Dr Stone", "Volume 01")
        service.regenerate_all()
        assert calls == [1]
        service.regenerate_all()
        assert calls == [1]


class TestApplyUpdate:
    def test_accepts_facts_and_republishes_both_files(
        self, service: MetadataService, library: Path
    ) -> None:
        write_volume(library, "Dr Stone", "Volume 01")
        service.regenerate_all()

        assert service.apply_series_update("Dr Stone", series_update(tag="HD Scan"), "alice")

        sidecar = json.loads((library / "Dr Stone" / "series.json").read_text("utf-8"))
        assert sidecar["external_ids"] == {"anilist": 98416}
        assert sidecar["titles"] == {"native": "Dr.STONE"}
        assert sidecar["tag"] == "HD Scan"
        assert sidecar["updated_at"] == "2026-08-18T19:36:24.324Z"
        # The client's index claims are ignored; bunko's compilation stands.
        assert sidecar["volumes"][0]["volume_uuid"] == "uuid-Volume 01"

        catalog = json.loads((library / "catalog.json").read_text("utf-8"))
        assert catalog["series"][0]["tag"] == "HD Scan"
        assert catalog["series"][0]["updated_at"] == "2026-08-18T19:36:24.324Z"
        assert "volumes" not in catalog["series"][0]

    def test_records_the_actor(self, service: MetadataService, library: Path) -> None:
        write_volume(library, "Dr Stone", "Volume 01")
        service.apply_series_update("Dr Stone", series_update(), "alice")
        row = service.database.get_series_facts("dr stone")
        assert row is not None
        assert row["updated_by"] == "alice"
        assert row["series_title"] == "Dr Stone"

    def test_rejects_junk_without_writing_anything(
        self, service: MetadataService, library: Path
    ) -> None:
        write_volume(library, "Dr Stone", "Volume 01")
        assert service.apply_series_update("Dr Stone", b"not json", "alice") is False
        assert service.database.get_series_facts("dr stone") is None
        assert not (library / "Dr Stone" / "series.json").exists()

    def test_reapplying_the_same_update_is_a_no_op(
        self, service: MetadataService, library: Path
    ) -> None:
        write_volume(library, "Dr Stone", "Volume 01")
        service.apply_series_update("Dr Stone", series_update(), "alice")
        sidecar = library / "Dr Stone" / "series.json"
        old = sidecar.stat().st_mtime - 60
        os.utime(sidecar, (old, old))

        assert service.apply_series_update("Dr Stone", series_update(), "alice") is True
        assert sidecar.stat().st_mtime == old

    def test_older_facts_are_accepted_but_do_not_win(
        self, service: MetadataService, library: Path
    ) -> None:
        write_volume(library, "Dr Stone", "Volume 01")
        service.apply_series_update("Dr Stone", series_update(), "alice")
        assert service.apply_series_update(
            "Dr Stone",
            series_update(external_ids={"anilist": 1}, updated_at="2026-08-01T00:00:00.000Z"),
            "bob",
        )
        sidecar = json.loads((library / "Dr Stone" / "series.json").read_text("utf-8"))
        assert sidecar["external_ids"] == {"anilist": 98416}

    def test_a_factless_epoch_update_never_clears_facts(
        self, service: MetadataService, library: Path
    ) -> None:
        write_volume(library, "Dr Stone", "Volume 01")
        service.apply_series_update("Dr Stone", series_update(), "alice")
        service.apply_series_update(
            "Dr Stone",
            series_update(
                external_ids={}, titles={}, updated_at="1970-01-01T00:00:00.000Z"
            ),
            "bob",
        )
        sidecar = json.loads((library / "Dr Stone" / "series.json").read_text("utf-8"))
        assert sidecar["external_ids"] == {"anilist": 98416}

    def test_offsets_ride_into_the_compiled_index_verbatim(
        self, service: MetadataService, library: Path
    ) -> None:
        write_volume(library, "Dr Stone", "Volume 01")
        assert service.apply_series_update(
            "Dr Stone",
            series_update(
                spine_offset=9999,
                volumes=[{"volume_uuid": "uuid-Volume 01", "offset": -12345.5}],
            ),
            "alice",
        )
        sidecar = json.loads((library / "Dr Stone" / "series.json").read_text("utf-8"))
        assert sidecar["spine_offset"] == 9999
        assert sidecar["volumes"][0]["offset"] == -12345.5

    def test_an_update_for_an_unknown_folder_is_still_stored(
        self, service: MetadataService, library: Path
    ) -> None:
        """A series uploaded moments later must find its facts waiting."""
        assert service.apply_series_update("Dr Stone", series_update(), "alice")
        assert service.database.get_series_facts("dr stone") is not None
        write_volume(library, "Dr Stone", "Volume 01")
        service.regenerate_all()
        sidecar = json.loads((library / "Dr Stone" / "series.json").read_text("utf-8"))
        assert sidecar["external_ids"] == {"anilist": 98416}
        # AMENDMENT, 2026-08-24 (Task 11 review round 3, controller-accepted):
        # this pre-provisioning test and the behavior it pinned are RETIRED.
        # apply_series_update() now refuses a title matching no existing
        # folder; the test above no longer reflects the shipped implementation
        # (see test_metadata_service.py's
        # test_an_update_for_an_unknown_folder_is_refused_and_never_stored).


class TestDebounce:
    def test_scheduled_regeneration_runs_once_after_the_quiet_period(
        self, library: Path, tmp_path: Path
    ) -> None:
        service = MetadataService(
            library, Database(tmp_path / "test.db"), debounce_seconds=0.05
        )
        write_volume(library, "Dr Stone", "Volume 01")
        for _ in range(5):
            service.schedule_regeneration()
        timer = service._timer
        assert timer is not None
        timer.join(timeout=5.0)
        assert (library / "Dr Stone" / "series.json").exists()
        service.stop()

    def test_stop_cancels_a_pending_pass(self, library: Path, tmp_path: Path) -> None:
        service = MetadataService(library, Database(tmp_path / "test.db"), debounce_seconds=5.0)
        write_volume(library, "Dr Stone", "Volume 01")
        service.schedule_regeneration()
        service.stop()
        assert service._timer is None
        assert not (library / "Dr Stone" / "series.json").exists()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/unit/test_metadata_service.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'mokuro_bunko.metadata.service'`

- [ ] **Step 3: Write the implementation**

`src/mokuro_bunko/metadata/service.py`:

> **AMENDMENT, 2026-08-24 (Task 11 review round 3, controller-accepted):** the
> module docstring below's "or a client that publishes facts before uploading
> the archives" clause describes pre-provisioning, which is RETIRED —
> `apply_series_update` now refuses a title matching no existing folder. The
> "restore" half (a row surviving its folder's temporary absence) stands. The
> shipped docstring in `src/mokuro_bunko/metadata/service.py` reflects the
> current, amended behavior; the block below is the plan's original draft and
> is left as drafted for historical accuracy.

```python
"""Compiling, publishing and updating the reader's metadata files.

This is the only stateful piece: it owns the debounce timer and the guarantee
that two regeneration passes never overlap. Everything it calls is pure or
filesystem-local.

Facts rows outlive their folders on purpose. A series that disappears drops out
of the compiled files but keeps its row, so a restore (or a client that
publishes facts before uploading the archives) finds its link waiting. A folder
RENAME does not carry facts across — they are keyed by normalized series title
— and the client republishes them under the new name on its next fact edit.
"""

from __future__ import annotations

import sys
import threading
from collections.abc import Callable
from pathlib import Path

from mokuro_bunko.database import Database, SeriesFactsRow
from mokuro_bunko.metadata.compiler import (
    SeriesFolder,
    compile_series_volumes,
    iter_series_folders,
    volume_key_for,
)
from mokuro_bunko.metadata.files import MetadataWriteBusy, write_if_changed
from mokuro_bunko.metadata.merge import StoredSeries, merge_series_update
from mokuro_bunko.metadata.paths import CATALOG_FILE_NAME, SERIES_FILE_NAME
from mokuro_bunko.metadata.reader_compat import normalize_series_key
from mokuro_bunko.metadata.schema import (
    FACTLESS_UPDATED_AT,
    SeriesFacts,
    SeriesIndexData,
    dump_catalog_file,
    dump_series_file,
)
from mokuro_bunko.metadata.validate import parse_series_update


def _log(message: str) -> None:
    print(f"[METADATA] {message}", file=sys.stderr, flush=True)


class MetadataService:
    """Owns `<Series>/series.json` and the root `catalog.json`."""

    def __init__(
        self,
        library_path: Path,
        database: Database,
        *,
        on_published: Callable[[], None] | None = None,
        debounce_seconds: float = 10.0,
    ) -> None:
        self.library_path = Path(library_path)
        self.database = database
        self.debounce_seconds = debounce_seconds
        self._on_published = on_published
        self._pass_lock = threading.Lock()
        self._timer_lock = threading.Lock()
        self._timer: threading.Timer | None = None

    # --- state -----------------------------------------------------------

    def _stored(self, series_key: str) -> StoredSeries | None:
        row = self.database.get_series_facts(series_key)
        if row is None:
            return None
        return StoredSeries(facts=_facts_of(row), index=_index_of(row))

    def _facts_for(self, series_key: str) -> SeriesFacts:
        row = self.database.get_series_facts(series_key)
        return _facts_of(row) if row else SeriesFacts()

    def _index_for(self, series_key: str) -> SeriesIndexData:
        row = self.database.get_series_facts(series_key)
        return _index_of(row) if row else SeriesIndexData()

    # --- publishing ------------------------------------------------------

    def _publish_series(self, folder: SeriesFolder) -> bool:
        """Write one series' sidecar. Returns True when the file changed."""
        series_key = normalize_series_key(folder.title)
        volumes = compile_series_volumes(folder, database=self.database)
        data = dump_series_file(
            series_title=folder.title,
            facts=self._facts_for(series_key),
            index=self._index_for(series_key),
            volumes=volumes,
        )
        return write_if_changed(folder.path / SERIES_FILE_NAME, data)

    def _publish_catalog(self, folders: list[SeriesFolder]) -> bool:
        entries = [
            (folder.title, self._facts_for(normalize_series_key(folder.title)))
            for folder in folders
        ]
        return write_if_changed(
            self.library_path / CATALOG_FILE_NAME, dump_catalog_file(entries)
        )

    def _published(self, changed: int) -> None:
        if changed and self._on_published is not None:
            self._on_published()

    # --- public API ------------------------------------------------------

    def regenerate_all(self) -> int:
        """Recompile every series and the catalog. Returns files written."""
        with self._pass_lock:
            folders = iter_series_folders(self.library_path)
            changed = 0
            keep: set[str] = set()
            for folder in folders:
                for volume in compile_series_volumes(folder, database=self.database):
                    keep.add(volume_key_for(folder.title, volume.volume_title))
                try:
                    changed += 1 if self._publish_series(folder) else 0
                except MetadataWriteBusy:
                    # A DAV write owns the path right now; the next trigger
                    # (or this pass's own reschedule) picks it up.
                    _log(f"skipped busy series folder: {folder.title}")
                    self.schedule_regeneration(delay=5.0)
            self.database.prune_series_entry_cache(keep)
            try:
                changed += 1 if self._publish_catalog(folders) else 0
            except MetadataWriteBusy:
                _log("skipped busy catalog.json")
                self.schedule_regeneration(delay=5.0)
        self._published(changed)
        return changed

    def regenerate_series(self, series_title: str) -> bool:
        """Recompile ONE series plus the catalog. Returns True when anything changed."""
        with self._pass_lock:
            folders = iter_series_folders(self.library_path)
            key = normalize_series_key(series_title)
            changed = 0
            for folder in folders:
                if normalize_series_key(folder.title) != key:
                    continue
                try:
                    changed += 1 if self._publish_series(folder) else 0
                except MetadataWriteBusy:
                    _log(f"skipped busy series folder: {folder.title}")
                    self.schedule_regeneration(delay=5.0)
            try:
                changed += 1 if self._publish_catalog(folders) else 0
            except MetadataWriteBusy:
                _log("skipped busy catalog.json")
                self.schedule_regeneration(delay=5.0)
        self._published(changed)
        return changed > 0

    def apply_series_update(
        self, series_title: str, payload: bytes, actor: str | None
    ) -> bool:
        """Contract §6: a PUT is an update REQUEST. True = accepted.

        Accepted does not mean "changed": a payload that loses the merge is
        still a valid request, and the client must be able to retry the same
        bytes forever without side effects.
        """
        series_key = normalize_series_key(series_title)
        if not series_key:
            return False
        update = parse_series_update(payload)
        if update is None:
            return False

        stored = self._stored(series_key)
        result = merge_series_update(stored, update)
        if stored is None or result.changed:
            self.database.put_series_facts(
                SeriesFactsRow(
                    series_key=series_key,
                    series_title=series_title,
                    external_ids=dict(result.facts.external_ids),
                    titles=dict(result.facts.titles),
                    synonyms=list(result.facts.synonyms),
                    tag=result.facts.tag,
                    unit=result.facts.unit,
                    facts_updated_at=result.facts.updated_at,
                    spine_offset=result.index.spine_offset,
                    volume_offsets=dict(result.index.volume_offsets),
                    updated_by=actor,
                    updated_at="",
                )
            )
        self.regenerate_series(series_title)
        return True

    def schedule_regeneration(self, delay: float | None = None) -> None:
        """Debounced full pass: resets on each call, fires after the quiet period."""
        with self._timer_lock:
            if self._timer is not None:
                self._timer.cancel()
            timer = threading.Timer(
                self.debounce_seconds if delay is None else delay, self._fire
            )
            timer.daemon = True
            self._timer = timer
            timer.start()

    def _fire(self) -> None:
        with self._timer_lock:
            self._timer = None
        try:
            self.regenerate_all()
        except Exception as error:  # noqa: BLE001 - a background pass must not die
            _log(f"regeneration failed: {error}")

    def stop(self) -> None:
        """Cancel a pending pass (shutdown)."""
        with self._timer_lock:
            if self._timer is not None:
                self._timer.cancel()
                self._timer = None


def _facts_of(row: SeriesFactsRow) -> SeriesFacts:
    return SeriesFacts(
        external_ids=dict(row["external_ids"]),
        titles=dict(row["titles"]),
        synonyms=tuple(row["synonyms"]),
        tag=row["tag"],
        unit=row["unit"],
        updated_at=row["facts_updated_at"] or FACTLESS_UPDATED_AT,
    )


def _index_of(row: SeriesFactsRow) -> SeriesIndexData:
    return SeriesIndexData(
        spine_offset=row["spine_offset"], volume_offsets=dict(row["volume_offsets"])
    )
```

Then extend `src/mokuro_bunko/metadata/__init__.py` with `from mokuro_bunko.metadata.service import MetadataService` and add `"MetadataService"` to `__all__`.

- [ ] **Step 4: Run test to verify it passes**

Run: `uv run pytest tests/unit/test_metadata_service.py -q`
Expected: PASS (17 tests)

- [ ] **Step 5: Check lint and types**

Run: `uv run ruff check src/ && uv run mypy src/`
Expected: no findings

- [ ] **Step 6: Commit**

```bash
git add src/mokuro_bunko/metadata tests/unit/test_metadata_service.py
git commit -m "feat(metadata): compile, publish and update service"
```

---

### Task 10: Intercept the PUT, and wire the service into the server (contract §6, §4)

The request-path half. `MetadataAPI` sits **inside** `AuthMiddleware` (so `environ["mokuro.username"]` is populated) and **outside** `AdminAPI`/`PropfindCacheMiddleware` (so wsgidav never opens a writer for the path). Interception is uniform across roles — an `uploader`'s raw write would be clobbered by the next regeneration anyway, and uniform handling means the file on disk can never disagree with what bunko compiled.

A PUT body carrying only facts — no `volumes` array, no offsets — is an equally legitimate update, not a malformed one: Task 4's validator treats `volumes` as optional and Task 6's merge updates facts independently of index data, so a facts-only body still applies its facts and republishes both files; the compiler alone owns volume entries regardless of what the body did or didn't send.

At this point only roles that already hold `ADD_FILES` (uploader/editor/admin) reach the middleware; `AuthMiddleware` still rejects a `registered` user's PUT with 403 — and that never changes: Task 11 adds an ownership check that lets `uploader` reach `MetadataAPI` only for a series it owns, and leaves every `MODIFY_DELETE`-holding role (inviter/editor/admin) unrestricted, but it does not open this gate for `registered`. The tree stays green either way.

**Files:**
- Create: `src/mokuro_bunko/metadata/middleware.py`
- Modify: `src/mokuro_bunko/server.py` (lines 215–312: the middleware stack, the change hooks, the shutdown block at 512–526)
- Test: `tests/unit/test_metadata_middleware.py`

**Interfaces:**
- Consumes: `paths.is_series_file_path`, `paths.series_title_from_series_file_path`, `service.MetadataService`.
- Produces: `MAX_UPDATE_BODY_BYTES`, `MetadataAPI(app, service: MetadataService | None = None)`.

- [ ] **Step 1: Write the failing test**

```python
"""The PUT interception middleware (contract §6)."""

from __future__ import annotations

import io
from typing import Any

from mokuro_bunko.metadata.middleware import MAX_UPDATE_BODY_BYTES, MetadataAPI


class StubService:
    def __init__(self, accepted: bool = True) -> None:
        self.accepted = accepted
        self.calls: list[tuple[str, bytes, str | None]] = []

    def apply_series_update(
        self, series_title: str, payload: bytes, actor: str | None
    ) -> bool:
        self.calls.append((series_title, payload, actor))
        return self.accepted


class StubApp:
    def __init__(self) -> None:
        self.calls = 0

    def __call__(self, environ: dict[str, Any], start_response: Any) -> list[bytes]:
        self.calls += 1
        start_response("200 OK", [("Content-Type", "text/plain")])
        return [b"downstream"]


def call(
    middleware: MetadataAPI,
    *,
    method: str = "PUT",
    path: str = "/mokuro-reader/Dr Stone/series.json",
    body: bytes = b'{"version":2}',
    username: str | None = "alice",
    content_length: str | None = None,
) -> tuple[str, list[tuple[str, str]], bytes]:
    captured: dict[str, Any] = {}

    def start_response(status: str, headers: list[tuple[str, str]]) -> None:
        captured["status"] = status
        captured["headers"] = headers

    environ: dict[str, Any] = {
        "REQUEST_METHOD": method,
        "PATH_INFO": path,
        "CONTENT_LENGTH": str(len(body)) if content_length is None else content_length,
        "wsgi.input": io.BytesIO(body),
        "mokuro.username": username,
        "mokuro.user": {"username": username} if username else None,
    }
    result = b"".join(middleware(environ, start_response))
    return captured["status"], captured["headers"], result


class TestPassthrough:
    def test_non_put_requests_pass_through(self) -> None:
        downstream = StubApp()
        service = StubService()
        status, _headers, body = call(
            MetadataAPI(downstream, service=service), method="GET"  # type: ignore[arg-type]
        )
        assert status == "200 OK"
        assert body == b"downstream"
        assert downstream.calls == 1
        assert service.calls == []

    def test_puts_to_other_paths_pass_through(self) -> None:
        downstream = StubApp()
        status, _headers, _body = call(
            MetadataAPI(downstream, service=StubService()),  # type: ignore[arg-type]
            path="/mokuro-reader/Dr Stone/Volume 01.cbz",
        )
        assert status == "200 OK"
        assert downstream.calls == 1

    def test_a_nested_series_json_is_not_intercepted(self) -> None:
        downstream = StubApp()
        call(
            MetadataAPI(downstream, service=StubService()),  # type: ignore[arg-type]
            path="/mokuro-reader/Dr Stone/extras/series.json",
        )
        assert downstream.calls == 1


class TestInterception:
    def test_accepted_update_answers_204_and_never_reaches_the_dav_app(self) -> None:
        downstream = StubApp()
        service = StubService()
        status, _headers, body = call(
            MetadataAPI(downstream, service=service)  # type: ignore[arg-type]
        )
        assert status.startswith("204")
        assert body == b""
        assert downstream.calls == 0
        assert service.calls == [("Dr Stone", b'{"version":2}', "alice")]

    def test_a_rejected_update_is_an_ordinary_400(self) -> None:
        service = StubService(accepted=False)
        status, headers, body = call(
            MetadataAPI(StubApp(), service=service)  # type: ignore[arg-type]
        )
        assert status.startswith("400")
        assert dict(headers)["Content-Type"].startswith("text/plain")
        assert b"metadata" in body.lower()

    def test_an_anonymous_put_is_401(self) -> None:
        service = StubService()
        status, _headers, _body = call(
            MetadataAPI(StubApp(), service=service), username=None  # type: ignore[arg-type]
        )
        assert status.startswith("401")
        assert service.calls == []

    def test_an_oversized_body_is_413_and_is_not_read(self) -> None:
        service = StubService()
        status, _headers, _body = call(
            MetadataAPI(StubApp(), service=service),  # type: ignore[arg-type]
            content_length=str(MAX_UPDATE_BODY_BYTES + 1),
        )
        assert status.startswith("413")
        assert service.calls == []

    def test_a_missing_content_length_is_400(self) -> None:
        service = StubService()
        status, _headers, _body = call(
            MetadataAPI(StubApp(), service=service), content_length="not a number"  # type: ignore[arg-type]
        )
        assert status.startswith("400")
        assert service.calls == []

    def test_without_a_service_the_write_is_refused_not_written(self) -> None:
        downstream = StubApp()
        status, _headers, _body = call(MetadataAPI(downstream, service=None))
        assert status.startswith("403")
        assert downstream.calls == 0
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/unit/test_metadata_middleware.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'mokuro_bunko.metadata.middleware'`

- [ ] **Step 3: Write the middleware**

`src/mokuro_bunko/metadata/middleware.py`:

```python
"""Intercepting a `series.json` PUT (contract §6).

A client PUTting `<Series>/series.json` is not writing a file — bunko compiles
that file — it is REQUESTING a metadata update. The request is answered here,
before the DAV app can open a writer for the path.

Placement matters: inside `AuthMiddleware` (so the actor is known) and outside
`PropfindCacheMiddleware` (so the DAV layer never sees the PUT). The cache
invalidation that a normal PUT would trigger is done instead by the service's
`on_published` hook, which fires only when the compiled bytes actually changed.
"""

from __future__ import annotations

import sys
from collections.abc import Callable, Iterable
from typing import TYPE_CHECKING, Any

from mokuro_bunko.metadata.paths import is_series_file_path, series_title_from_series_file_path

if TYPE_CHECKING:
    from mokuro_bunko.metadata.service import MetadataService

#: A `series.json` for a 1000-volume series is well under 300 KB; anything
#: past this is not a metadata update.
MAX_UPDATE_BODY_BYTES = 4 * 1024 * 1024

_STATUS_TEXT = {
    400: "400 Bad Request",
    401: "401 Unauthorized",
    403: "403 Forbidden",
    413: "413 Payload Too Large",
}


class MetadataAPI:
    """Answers `PUT <Series>/series.json` instead of letting it write."""

    def __init__(
        self,
        app: Callable[..., Iterable[bytes]],
        service: MetadataService | None = None,
    ) -> None:
        self.app = app
        self.service = service

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        if environ.get("REQUEST_METHOD") != "PUT":
            return self.app(environ, start_response)
        path = environ.get("PATH_INFO", "/")
        if not is_series_file_path(path):
            return self.app(environ, start_response)
        return self._handle_update(environ, start_response, path)

    def _handle_update(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        path: str,
    ) -> Iterable[bytes]:
        if self.service is None:
            # Fail closed: bunko owns this file even when compilation is off,
            # and a raw write would be silently replaced later.
            return self._text(start_response, 403, "Metadata files are compiled by the server")

        username = environ.get("mokuro.username")
        if not isinstance(username, str) or not username:
            return self._text(start_response, 401, "Authentication required")

        try:
            length = int(environ.get("CONTENT_LENGTH") or 0)
        except (TypeError, ValueError):
            return self._text(start_response, 400, "Invalid Content-Length")
        if length < 0:
            return self._text(start_response, 400, "Invalid Content-Length")
        if length > MAX_UPDATE_BODY_BYTES:
            return self._text(start_response, 413, "Metadata update too large")

        body = environ["wsgi.input"].read(length) if length else b""
        series_title = series_title_from_series_file_path(path)
        if series_title is None:  # pragma: no cover - guarded by is_series_file_path
            return self._text(start_response, 400, "Invalid metadata path")

        accepted = self.service.apply_series_update(series_title, body, username)
        self._audit(environ, path, username, accepted)
        if not accepted:
            return self._text(start_response, 400, "Invalid metadata update")

        start_response("204 No Content", [])
        return [b""]

    @staticmethod
    def _audit(
        environ: dict[str, Any], path: str, username: str, accepted: bool
    ) -> None:
        database = environ.get("mokuro.db")
        if database is None:
            return
        try:
            database.log_audit_event(
                action="metadata_update" if accepted else "metadata_rejected",
                actor_username=username,
                target_type="library",
                target_path=path,
                details={"accepted": accepted},
            )
        except Exception as error:  # noqa: BLE001 - auditing must never fail a request
            print(f"[METADATA] audit failed: {error}", file=sys.stderr, flush=True)

    @staticmethod
    def _text(
        start_response: Callable[..., Any], status_code: int, message: str
    ) -> list[bytes]:
        body = message.encode("utf-8")
        start_response(
            _STATUS_TEXT[status_code],
            [
                ("Content-Type", "text/plain; charset=utf-8"),
                ("Content-Length", str(len(body))),
            ],
        )
        return [body]
```

- [ ] **Step 4: Run test to verify it passes**

Run: `uv run pytest tests/unit/test_metadata_middleware.py -q`
Expected: PASS (9 tests)

- [ ] **Step 5: Wire it into the server**

In `src/mokuro_bunko/server.py`, add the imports:

```python
from mokuro_bunko.metadata.middleware import MetadataAPI
from mokuro_bunko.metadata.service import MetadataService
```

Replace the block that currently reads

```python
    propfind_cache = PropfindCacheMiddleware(app, ttl=120.0)
    app = propfind_cache
    library_index = LibraryIndexCache(config.storage.library_path, ttl=30.0)
```

with

```python
    propfind_cache = PropfindCacheMiddleware(app, ttl=120.0)
    app = propfind_cache
    library_index = LibraryIndexCache(config.storage.library_path, ttl=30.0)

    def on_metadata_published() -> None:
        """Compiled files changed on disk: refresh the listings that show them.

        Deliberately does NOT schedule another regeneration — that would feed
        itself forever. (The filesystem watcher ignores `.json` for the same
        reason: `_RELEVANT_SUFFIXES` has no `.json` entry.)
        """
        library_index.invalidate()
        propfind_cache.schedule_refresh(delay=5.0)

    metadata_service = MetadataService(
        config.storage.library_path,
        database,
        on_published=on_metadata_published,
    )
```

Mount the middleware immediately after the `AdminAPI` block, before `AuthMiddleware`:

```python
    # Wrap with metadata API (intercepts series.json PUTs as update requests).
    # Inside AuthMiddleware so the actor is known; outside the DAV app so the
    # PUT never opens a writer.
    app = MetadataAPI(app, service=metadata_service)
```

Extend the change hook near the bottom of `create_app`:

```python
    def on_library_change() -> None:
        library_index.invalidate()
        propfind_cache.schedule_refresh(delay=5.0)
        metadata_service.schedule_regeneration()
```

and after `propfind_cache.warm()`, publish the handle and schedule the first pass:

```python
    app._metadata_service = metadata_service  # type: ignore[attr-defined]
    # First compilation runs after startup settles (PROPFIND warm on a large
    # library is already competing for the disk).
    metadata_service.schedule_regeneration(delay=20.0)
```

In `run_server`'s `finally` block, next to the existing shutdown calls:

```python
        if hasattr(wsgi_app, "_metadata_service"):
            wsgi_app._metadata_service.stop()
```

Update the middleware-stack comment at the top of `create_app` so the numbered list mentions `MetadataAPI` between `AdminAPI` and `AuthMiddleware`.

- [ ] **Step 6: Run the full suite (nothing else may move)**

Run: `uv run pytest tests/unit tests/integration -q`
Expected: PASS — 726 baseline tests plus everything added so far

- [ ] **Step 7: Check lint and types**

Run: `uv run ruff check src/ && uv run mypy src/`
Expected: no findings

- [ ] **Step 8: Commit**

```bash
git add src/mokuro_bunko/metadata/middleware.py src/mokuro_bunko/server.py \
        tests/unit/test_metadata_middleware.py
git commit -m "feat(metadata): intercept series.json PUTs and wire the service"
```

---

### Task 11: Authorization policy for compiled files (contract §5, §6)

**Rewritten 2026-08-24.** The user overturned this task's original design (a flat `WRITE_PROGRESS` gate that let `registered` submit updates) before any of it was implemented. The replacement is ownership-gated, and touches three files because the identity endpoint is folded into the same task — it is the same policy surface, just read-only.

**The new rule for a `series.json` PUT**, entirely in `AuthMiddleware`:

- **`anonymous` is 401.** Unchanged.
- **`registered` is 403, always.** It keeps exactly the `WRITE_PROGRESS` + `is_progress_file` carve-out it already has for its own `volume-data.json`/`profiles.json` (unchanged, untouched by this task) and gains nothing here — ownership is never even checked for this role, since a `registered` account cannot upload volumes in the first place and so can never own a series either. This is a permanent design decision, not a placeholder: a later task must not "finish the job" by opening this gate.
- **`uploader` is authorized only for a series it owns outright.** Ownership is read from the `volume_uploads` table that already backs `record_volume_upload`/`get_volume_owner`/`can_user_delete_library_path` (`database.py` "Upload ownership operations", ~line 987) — no schema change. This task adds the series-level rule on top, as a new `Database.can_user_edit_series(username, series_title) -> bool`: an uploader owns a series when it owns **at least one** tracked volume in that folder **and no** tracked volume in that folder is owned by anyone else. A series with **no** tracked volumes at all — legacy content, or anything uploaded before ownership tracking existed — is a 403 for `uploader`, not a free-for-all: the user chose the safe default explicitly rather than let the first PUT silently claim an orphaned series.
- **Any role holding `MODIFY_DELETE` is authorized for every series, unconditionally.** That set is `inviter`, `editor` and `admin` (see `ROLE_PERMISSIONS`, `src/mokuro_bunko/middleware/auth.py` line ~47) — "editor+" everywhere else in this plan means exactly this set, and `inviter` counts even though this plan otherwise never mentions that role.

**Everything else is unchanged from the original design and remains correct:** every other write verb on a compiled file — `catalog.json` PUT/DELETE/MOVE/COPY/PROPPATCH, `series.json` DELETE/MOVE/COPY/PROPPATCH — is an ordinary 403 for **every** role, because bunko is the sole producer. It must stay an ordinary 403: the client treats metadata-write failure as best-effort and stays read-write for everything else. Archives and covers still need no new code — `registered` lacks `ADD_FILES`, so a `.cbz`/`.webp` PUT is already refused, and this task's series-ownership check does not extend `uploader`'s reach there either: a `.cbz`/`.webp` PUT is still gated by plain `ADD_FILES`, nothing about owning the series widens it. Tests pin all of this so it cannot regress.

**Third deliverable, same policy surface.** The identity endpoint (`login/api.py`'s `/login/api/me`, already consumed by the reader's `webdav/identity.ts`) gains a `metadata` object that mirrors this exact gate read-only, so the reader can label or disable its own per-series edit UI without probing with a real PUT: `{"scope": "all"}` for a `MODIFY_DELETE` holder, `{"scope": "owned", "ownedSeries": [...]}` for `uploader` (the folder names `can_user_edit_series` would let it edit — derived from its own `volume_uploads` rows), `{"scope": "none"}` for `registered` and `anonymous`. `ownedSeries` is present only when `scope` is `"owned"`.

**Files:**
- Modify: `src/mokuro_bunko/database.py` (new methods immediately after `can_user_delete_library_path`, ~line 1053, before `forget_volume_upload`)
- Modify: `src/mokuro_bunko/middleware/auth.py` (imports; a new block in `authorize` around line 394, before the read-operations block; the ownership-gated PUT block in `_authorize_put` around line 561 — neither exists yet, this task was never implemented before the 2026-08-24 ruling)
- Modify: `src/mokuro_bunko/login/api.py` (`_role_permissions`/`_get_me`, ~lines 126–140)
- Test: `tests/unit/test_database.py` (extend `TestAuditAndOwnership`), `tests/unit/test_metadata_permissions.py` (rewrite), `tests/integration/test_login_me.py` (extend)

**Interfaces:**
- Consumes: `paths.is_series_file_path`, `paths.is_compiled_metadata_path`, `paths.series_title_from_series_file_path`, `middleware.auth.Permission.MODIFY_DELETE`, `middleware.auth.check_permission`.
- Produces: `Database.series_owners(series_title: str) -> set[str]`, `Database.can_user_edit_series(username: str, series_title: str) -> bool`, `Database.list_series_owned_by(username: str) -> list[str]`; changes the outcome of `AuthMiddleware._authorize_put` for `series.json`; `LoginAPI._metadata_scope(role: str, username: str | None) -> dict[str, Any]`, wired into `_get_me`'s two 200 responses as a new `"metadata"` key.

- [ ] **Step 1: Write the failing test — ownership helpers**

Append to `tests/unit/test_database.py`'s `TestAuditAndOwnership` class:

```python
    def test_can_user_edit_series_sole_owner(self, temp_db: Database) -> None:
        temp_db.record_volume_upload("Dr Stone/Volume 01.cbz", "alice")
        temp_db.record_volume_upload("Dr Stone/Volume 02.cbz", "alice")
        assert temp_db.can_user_edit_series("alice", "Dr Stone") is True
        assert temp_db.can_user_edit_series("bob", "Dr Stone") is False

    def test_can_user_edit_series_mixed_ownership_is_false_for_everyone(
        self, temp_db: Database
    ) -> None:
        temp_db.record_volume_upload("Dr Stone/Volume 01.cbz", "alice")
        temp_db.record_volume_upload("Dr Stone/Volume 02.cbz", "bob")
        assert temp_db.can_user_edit_series("alice", "Dr Stone") is False
        assert temp_db.can_user_edit_series("bob", "Dr Stone") is False

    def test_can_user_edit_series_untracked_folder_is_false(
        self, temp_db: Database
    ) -> None:
        """No volume_uploads rows at all: the safe default is 403, not a free-for-all."""
        assert temp_db.can_user_edit_series("alice", "Legacy Series") is False

    def test_list_series_owned_by_returns_only_fully_owned_folders(
        self, temp_db: Database
    ) -> None:
        temp_db.record_volume_upload("Dr Stone/Volume 01.cbz", "alice")
        temp_db.record_volume_upload("Aria/v1.cbz", "alice")
        temp_db.record_volume_upload("Shared Series/v1.cbz", "alice")
        temp_db.record_volume_upload("Shared Series/v2.cbz", "bob")

        assert temp_db.list_series_owned_by("alice") == ["Aria", "Dr Stone"]
        assert temp_db.list_series_owned_by("bob") == []
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/unit/test_database.py -k "can_user_edit_series or list_series_owned_by" -q`
Expected: FAIL — `AttributeError: 'Database' object has no attribute 'can_user_edit_series'`

- [ ] **Step 3: Add the ownership helpers**

In `src/mokuro_bunko/database.py`, immediately after `can_user_delete_library_path` (ends ~line 1053) and before `forget_volume_upload`:

```python
    def series_owners(self, series_title: str) -> set[str]:
        """Distinct uploader usernames among a series folder's tracked volumes.

        `series_title` is the literal top-level library folder name — the
        same string `metadata.paths.series_title_from_series_file_path`
        returns. Matched the same way `forget_volume_uploads_under_prefix`
        matches a folder prefix: a plain `LIKE '<title>/%'`, case-insensitive
        for ASCII, NOT the lowercased `normalize_series_key` fold
        `series_facts` uses — this table's keys are library-relative paths,
        not folded series keys. A folder with no tracked volumes returns an
        empty set; the `LIKE` pattern is not escaped (matching the existing
        `forget_volume_uploads_under_prefix` precedent), but `can_user_edit_series`
        below fails closed on any over-match, since an unescaped `%`/`_` in a
        folder name can only ever pull in EXTRA owners, never remove the real
        ones — so it can produce a false negative, never a false grant.
        """
        prefix = series_title.strip("/")
        if not prefix:
            return set()
        with self._connection() as conn:
            cursor = conn.execute(
                "SELECT DISTINCT uploader_username FROM volume_uploads WHERE volume_key LIKE ?",
                (f"{prefix}/%",),
            )
            return {str(row["uploader_username"]) for row in cursor.fetchall()}

    def can_user_edit_series(self, username: str, series_title: str) -> bool:
        """True when `username` owns EVERY tracked volume in a series folder.

        The safe default for a folder with no ownership records — legacy
        content, or a series uploaded before ownership tracking existed — is
        False: an uploader may not claim an untracked series just by being
        the first to PUT its `series.json`. Only a role holding
        `Permission.MODIFY_DELETE` may edit an unowned/untracked series
        (enforced by the caller, `AuthMiddleware._authorize_put`).
        """
        owners = self.series_owners(series_title)
        return bool(owners) and owners == {username}

    def list_series_owned_by(self, username: str) -> list[str]:
        """Series folder names `username` may edit, per `can_user_edit_series`.

        Feeds the identity endpoint's `metadata.ownedSeries`. A folder where
        this user owns some but not all tracked volumes is excluded — it is
        not editable by them either, so it must not appear in their list.
        """
        with self._connection() as conn:
            cursor = conn.execute(
                "SELECT DISTINCT volume_key FROM volume_uploads WHERE uploader_username = ?",
                (username,),
            )
            folders = {
                str(row["volume_key"]).split("/", 1)[0]
                for row in cursor.fetchall()
                if "/" in str(row["volume_key"])
            }
        return sorted(folder for folder in folders if self.can_user_edit_series(username, folder))
```

- [ ] **Step 4: Run test to verify it passes**

Run: `uv run pytest tests/unit/test_database.py -q`
Expected: PASS (all cases in `TestAuditAndOwnership`, including the 4 new ones)

- [ ] **Step 5: Write the failing test — the PUT authorization policy**

Replace `tests/unit/test_metadata_permissions.py` in full:

```python
"""Contract §5/§6: who may write what under the compiled-metadata rules.

2026-08-24 ruling (overturns this task's original `WRITE_PROGRESS` design):
`series.json` PUT authorization is ownership-gated, mirroring the DELETE-time
precedent `can_user_delete_library_path` already sets for `uploader`.
`registered` never reaches it; `uploader` reaches it only for a series it
fully owns; any role holding `MODIFY_DELETE` (`inviter`/`editor`/`admin` —
see `ROLE_PERMISSIONS`) reaches it for every series.
"""

from __future__ import annotations

from typing import Any

import pytest

from mokuro_bunko.database import Database
from mokuro_bunko.middleware.auth import AuthMiddleware, AuthResult

SERIES_FILE = "/mokuro-reader/Dr Stone/series.json"
OTHER_SERIES_FILE = "/mokuro-reader/Aria/series.json"
CATALOG_FILE = "/mokuro-reader/catalog.json"
ARCHIVE = "/mokuro-reader/Dr Stone/Volume 01.cbz"
COVER = "/mokuro-reader/Dr Stone/Volume 01.webp"


@pytest.fixture
def middleware(temp_db: Database) -> AuthMiddleware:
    def app(environ: dict[str, Any], start_response: Any) -> list[bytes]:
        return [b""]

    return AuthMiddleware(app, temp_db)


def as_role(role: str) -> AuthResult:
    if role == "anonymous":
        return AuthResult(authenticated=False, role="anonymous")
    return AuthResult(
        authenticated=True,
        user={
            "id": 1,
            "username": role,
            "role": role,
            "status": "active",
            "notes": "",
            "created_at": "2026-01-01",
        },
        role=role,
    )


def authorize(middleware: AuthMiddleware, method: str, path: str, role: str) -> tuple[bool, int]:
    result = middleware.authorize(
        {"REQUEST_METHOD": method, "PATH_INFO": path}, as_role(role)
    )
    return result.authorized, result.status_code


class TestSeriesFilePutAnonymousAndModifyDelete:
    def test_anonymous_may_not(self, middleware: AuthMiddleware) -> None:
        assert authorize(middleware, "PUT", SERIES_FILE, "anonymous") == (False, 401)

    @pytest.mark.parametrize("role", ["inviter", "editor", "admin"])
    def test_every_modify_delete_role_may_edit_any_series_unconditionally(
        self, middleware: AuthMiddleware, role: str
    ) -> None:
        """`inviter` holds MODIFY_DELETE too (ROLE_PERMISSIONS) — it counts
        here even though it never uploads. Neither folder has an ownership
        row at all, and it is still authorized."""
        assert authorize(middleware, "PUT", SERIES_FILE, role) == (True, 200)
        assert authorize(middleware, "PUT", OTHER_SERIES_FILE, role) == (True, 200)

    def test_reading_it_is_unaffected(self, middleware: AuthMiddleware) -> None:
        assert authorize(middleware, "GET", SERIES_FILE, "anonymous")[0] is True
        assert authorize(middleware, "PROPFIND", SERIES_FILE, "anonymous")[0] is True


class TestSeriesFilePutRegisteredNeverReaches:
    def test_registered_is_403_for_an_untracked_series(
        self, middleware: AuthMiddleware
    ) -> None:
        assert authorize(middleware, "PUT", SERIES_FILE, "registered") == (False, 403)

    def test_registered_is_403_even_if_it_somehow_owns_the_series(
        self, middleware: AuthMiddleware, temp_db: Database
    ) -> None:
        """Ownership alone is never sufficient for this role: ADD_FILES-tier
        (uploader) or above is required before ownership is even checked."""
        temp_db.record_volume_upload("Dr Stone/Volume 01.cbz", "registered")
        assert authorize(middleware, "PUT", SERIES_FILE, "registered") == (False, 403)


class TestSeriesFilePutUploaderOwnership:
    def test_an_untracked_series_is_403_not_a_free_for_all(
        self, middleware: AuthMiddleware
    ) -> None:
        """No volume_uploads rows for the folder at all: the safe default."""
        assert authorize(middleware, "PUT", SERIES_FILE, "uploader") == (False, 403)

    def test_the_sole_owner_may_edit_its_series(
        self, middleware: AuthMiddleware, temp_db: Database
    ) -> None:
        temp_db.record_volume_upload("Dr Stone/Volume 01.cbz", "uploader")
        assert authorize(middleware, "PUT", SERIES_FILE, "uploader") == (True, 200)

    def test_a_non_owner_uploader_is_403(
        self, middleware: AuthMiddleware, temp_db: Database
    ) -> None:
        temp_db.record_volume_upload("Dr Stone/Volume 01.cbz", "someone-else")
        assert authorize(middleware, "PUT", SERIES_FILE, "uploader") == (False, 403)

    def test_ownership_is_per_series_not_global(
        self, middleware: AuthMiddleware, temp_db: Database
    ) -> None:
        temp_db.record_volume_upload("Dr Stone/Volume 01.cbz", "uploader")
        assert authorize(middleware, "PUT", SERIES_FILE, "uploader") == (True, 200)
        assert authorize(middleware, "PUT", OTHER_SERIES_FILE, "uploader") == (False, 403)

    def test_a_series_with_any_other_owner_is_403_for_this_uploader(
        self, middleware: AuthMiddleware, temp_db: Database
    ) -> None:
        """One volume owned by another user in the same folder blocks the
        whole series for this uploader — not a per-volume grant."""
        temp_db.record_volume_upload("Dr Stone/Volume 01.cbz", "uploader")
        temp_db.record_volume_upload("Dr Stone/Volume 02.cbz", "someone-else")
        assert authorize(middleware, "PUT", SERIES_FILE, "uploader") == (False, 403)


class TestCompiledFilesAreServerOwned:
    @pytest.mark.parametrize("role", ["registered", "uploader", "editor", "admin"])
    def test_nobody_may_put_the_catalog(self, middleware: AuthMiddleware, role: str) -> None:
        assert authorize(middleware, "PUT", CATALOG_FILE, role) == (False, 403)

    @pytest.mark.parametrize("method", ["DELETE", "MOVE", "COPY", "PROPPATCH"])
    @pytest.mark.parametrize("path", [SERIES_FILE, CATALOG_FILE])
    def test_nobody_may_delete_or_move_a_compiled_file(
        self, middleware: AuthMiddleware, method: str, path: str
    ) -> None:
        assert authorize(middleware, method, path, "admin") == (False, 403)

    def test_deleting_the_series_folder_itself_is_still_allowed(
        self, middleware: AuthMiddleware
    ) -> None:
        assert authorize(middleware, "DELETE", "/mokuro-reader/Dr Stone", "editor") == (
            True,
            200,
        )

    def test_a_nested_catalog_json_is_an_ordinary_library_file(
        self, middleware: AuthMiddleware
    ) -> None:
        assert authorize(
            middleware, "PUT", "/mokuro-reader/Dr Stone/catalog.json", "uploader"
        ) == (True, 200)


class TestScopedUsersStillCannotWriteContent:
    @pytest.mark.parametrize("path", [ARCHIVE, COVER])
    def test_a_registered_user_may_not_upload_archives_or_covers(
        self, middleware: AuthMiddleware, path: str
    ) -> None:
        """Contract §5 — already true via ADD_FILES; pinned so it stays true."""
        assert authorize(middleware, "PUT", path, "registered") == (False, 403)

    @pytest.mark.parametrize("path", [ARCHIVE, COVER])
    def test_an_uploader_still_may(self, middleware: AuthMiddleware, path: str) -> None:
        assert authorize(middleware, "PUT", path, "uploader") == (True, 200)

    def test_progress_writes_are_untouched(self, middleware: AuthMiddleware) -> None:
        assert authorize(
            middleware, "PUT", "/mokuro-reader/volume-data.json", "registered"
        ) == (True, 200)
```

- [ ] **Step 6: Run test to verify it fails**

Run: `uv run pytest tests/unit/test_metadata_permissions.py -q`
Expected: FAIL — none of this task's `auth.py` code exists yet, so `series.json`/`catalog.json` PUT still falls through to the generic library-file `ADD_FILES` gate, and DELETE/MOVE/COPY/PROPPATCH still falls through to the generic `MODIFY_DELETE` gate. Concretely: every `TestSeriesFilePutUploaderOwnership` case that expects 403 currently returns 200 (any `uploader` may PUT any series, tracked or not); `test_nobody_may_put_the_catalog`'s `uploader`/`editor`/`admin` cases currently return 200 for the same reason; `test_nobody_may_delete_or_move_a_compiled_file` currently returns 200 for `admin` (plain `MODIFY_DELETE`, no compiled-file check yet). Everything that happens to already match the new rule via a generic gate — `anonymous`, `registered`, the `MODIFY_DELETE`-unconditional PUT cases, `test_nobody_may_put_the_catalog[registered]`, archives/covers, progress writes — passes even before this step; that is expected, not a bug in the test.

- [ ] **Step 7: Add the policy**

In `src/mokuro_bunko/middleware/auth.py`, extend the imports:

```python
from mokuro_bunko.metadata.paths import (
    is_compiled_metadata_path,
    is_series_file_path,
    series_title_from_series_file_path,
)
```

Task 11 has not been implemented before this rewrite — nothing below exists in `auth.py` yet — so both insertions are new, not edits to prior Task 11 code.

In `authorize`, immediately after the `is_admin_path(path)` block and before the read-operations block, insert (unchanged from the original design — contract §5, not touched by the 2026-08-24 ruling):

```python
        # Compiled metadata files are produced by this server (contract §5):
        # no role may delete, move, copy or PROPPATCH one. A plain 403 is what
        # the client expects — it treats metadata writes as best-effort and
        # stays read-write for everything else. Folder-level operations are
        # unaffected: the path tested here is the file itself.
        if method in ("DELETE", "MOVE", "COPY", "PROPPATCH") and is_compiled_metadata_path(path):
            return AuthorizationResult(
                authorized=False,
                status_code=403,
                error="Permission denied: this file is compiled by the server",
            )
```

In `_authorize_put`, immediately after the `is_progress_file(path)` block, insert the ownership-gated PUT policy — this is the part the ruling actually changed:

```python
        # A `series.json` PUT is an update REQUEST, not a file write
        # (contract §6): MetadataAPI validates and merges it, and the DAV
        # layer never sees it. Authorization is ownership-gated, NOT the
        # WRITE_PROGRESS "edit your own data" gate that guards progress files
        # above (2026-08-24 ruling, overturning this task's original design):
        #   - anonymous -> 401
        #   - a MODIFY_DELETE holder (inviter/editor/admin) -> every series
        #   - uploader -> only a series it owns outright (Database.can_user_edit_series)
        #   - registered -> always 403; it stays limited to the progress/
        #     profile carve-out above and never gains series-metadata access
        if is_series_file_path(path):
            if not auth_result.authenticated:
                return AuthorizationResult(
                    authorized=False,
                    status_code=401,
                    error="Authentication required",
                )
            if check_permission(role, Permission.MODIFY_DELETE):
                return AuthorizationResult(authorized=True)
            if role == "uploader":
                series_title = series_title_from_series_file_path(path)
                username = auth_result.username
                if (
                    series_title is not None
                    and username is not None
                    and self.database.can_user_edit_series(username, series_title)
                ):
                    return AuthorizationResult(authorized=True)
            return AuthorizationResult(
                authorized=False,
                status_code=403,
                error="Permission denied: cannot submit metadata updates for this series",
            )

        # Every other compiled file (the root catalog.json) is server-owned.
        if is_compiled_metadata_path(path):
            return AuthorizationResult(
                authorized=False,
                status_code=403,
                error="Permission denied: this file is compiled by the server",
            )
```

- [ ] **Step 8: Run test to verify it passes**

Run: `uv run pytest tests/unit/test_metadata_permissions.py -q`
Expected: PASS (31 tests, counting the parametrized cases)

- [ ] **Step 9: Write the failing test — identity endpoint metadata scope**

In `tests/integration/test_login_me.py`, add `db.create_user("inv", "pass1234", "inviter")` to the `db` fixture (needed once, to exercise `inviter`'s `MODIFY_DELETE` membership through this endpoint too), then append a new class:

```python
class TestMeMetadataScope:
    """The `metadata` object added 2026-08-24: gates the reader's per-series edit UI."""

    @pytest.mark.parametrize(
        "username,role", [("edi", "editor"), ("adm", "admin"), ("inv", "inviter")]
    )
    def test_modify_delete_holders_get_scope_all(
        self, api: LoginAPI, username: str, role: str
    ) -> None:
        status, body = call_me(api, encoded_header(f"{username}:pass1234"))
        assert status == 200
        assert body["role"] == role
        assert body["metadata"] == {"scope": "all"}

    def test_registered_gets_scope_none(self, api: LoginAPI) -> None:
        status, body = call_me(api, encoded_header("reg:pass1234"))
        assert status == 200
        assert body["metadata"] == {"scope": "none"}

    def test_anonymous_gets_scope_none(self, api: LoginAPI) -> None:
        status, body = call_me(api)
        assert status == 200
        assert body["authenticated"] is False
        assert body["metadata"] == {"scope": "none"}

    def test_uploader_with_no_owned_series_gets_an_empty_owned_list(
        self, api: LoginAPI
    ) -> None:
        status, body = call_me(api, encoded_header("upl:pass1234"))
        assert status == 200
        assert body["metadata"] == {"scope": "owned", "ownedSeries": []}

    def test_uploader_sees_exactly_the_folders_it_owns(
        self, api: LoginAPI, db: Database
    ) -> None:
        db.record_volume_upload("Dr Stone/Volume 01.cbz", "upl")
        db.record_volume_upload("Aria/v1.cbz", "upl")
        # A folder `upl` only partly owns must not appear in its list.
        db.record_volume_upload("Shared/v1.cbz", "upl")
        db.record_volume_upload("Shared/v2.cbz", "edi")

        status, body = call_me(api, encoded_header("upl:pass1234"))
        assert status == 200
        assert body["metadata"] == {"scope": "owned", "ownedSeries": ["Aria", "Dr Stone"]}
```

- [ ] **Step 10: Run test to verify it fails**

Run: `uv run pytest tests/integration/test_login_me.py -q`
Expected: FAIL — `KeyError: 'metadata'`

- [ ] **Step 11: Add the identity endpoint scope**

In `src/mokuro_bunko/login/api.py`, add a new method right after `_role_permissions`:

```python
    def _metadata_scope(self, role: str, username: str | None) -> dict[str, Any]:
        """Contract-facing scope for the series.json/catalog.json write gate.

        Mirrors `AuthMiddleware._authorize_put`'s Task 11 policy exactly, so
        this endpoint can never advertise more (or less) than a real PUT would
        actually be allowed to do: a MODIFY_DELETE holder may edit any series,
        an uploader only the series it fully owns (`Database.can_user_edit_series`),
        everyone else (`registered`, anonymous) none.
        """
        if check_permission(role, Permission.MODIFY_DELETE):
            return {"scope": "all"}
        if role == "uploader" and username and self.db is not None:
            return {
                "scope": "owned",
                "ownedSeries": self.db.list_series_owned_by(username),
            }
        return {"scope": "none"}
```

In `_get_me`, add `"metadata": self._metadata_scope("anonymous", None)` to the anonymous 200 response, and `"metadata": self._metadata_scope(user["role"], user["username"])` to the authenticated 200 response — both alongside the existing `"permissions"` key, no other keys change.

- [ ] **Step 12: Run test to verify it passes**

Run: `uv run pytest tests/integration/test_login_me.py -q`
Expected: PASS (all existing cases plus the new `TestMeMetadataScope` class)

- [ ] **Step 13: Run the full regression suites**

Run: `uv run pytest tests/unit/test_database.py tests/unit/test_permissions.py tests/unit/test_metadata_permissions.py tests/integration/test_auth.py tests/integration/test_webdav_ops.py tests/integration/test_login_me.py tests/integration/test_security_headers.py tests/integration/test_cors.py -q`
Expected: PASS, unchanged outside the new/rewritten cases above

- [ ] **Step 14: Check lint and types**

Run: `uv run ruff check src/ && uv run mypy src/`
Expected: no findings

- [ ] **Step 15: Commit**

```bash
git add src/mokuro_bunko/database.py src/mokuro_bunko/middleware/auth.py \
        src/mokuro_bunko/login/api.py tests/unit/test_database.py \
        tests/unit/test_metadata_permissions.py tests/integration/test_login_me.py
git commit -m "feat(auth): ownership-gated series.json updates; identity endpoint metadata scope"
```

---

### Task 11b: Entry freshness stamps (contract §2 freshness-stamp addendum, user ruling 2026-08-24)

Four more optional fields on each compiled volume entry — `mokuro_size`/`mokuro_modified` (the `.mokuro`/`.mokuro.gz` sidecar's stat) and `cover_size`/`cover_modified` (the cover sidecar's stat) — so a client can tell whether its own cached copy of either file is stale without downloading it. This ruling landed after Tasks 1–11 shipped, so it extends the already-implemented `schema.py` (Task 3) and `compiler.py` (Task 7) in place rather than redoing them — the same pattern Task 11 itself used against Tasks 1–10. The mokuro stamp is free: `compile_series_volumes` already `stat()`s the sidecar once per volume, purely to build the entry cache's validation key (`_stat_key`, currently at `compiler.py:107`) — this task threads that SAME `os.stat_result` through to the compiled entry instead of discarding it, so no second syscall. The cover stamp has no existing stat to reuse (nothing in the compiler looks at cover files today) and is deliberately kept OUTSIDE the entry cache — see "Cover stat is never cached" in Decisions above.

**Files:**
- Modify: `src/mokuro_bunko/metadata/schema.py` (`VolumeEntry`, `dump_series_file`)
- Modify: `src/mokuro_bunko/metadata/compiler.py` (`_stat_key`, `_compile_volume`, `_entry_to_dict`, `_entry_from_dict`, `compile_series_volumes`; new `_sidecar_stat`, `_cover_stat`)
- Test: `tests/unit/test_metadata_schema.py` (extend), `tests/unit/test_metadata_compiler.py` (extend)

**Interfaces:**
- Consumes: `mokuro_bunko.ocr.processor.OCRProcessor.get_cover_path` (static, for the `<Volume>.webp` convention — no instance needed, no circular import: `ocr/` imports nothing from `metadata/`).
- Produces: `VolumeEntry` gains `mokuro_size: int | None`, `mokuro_modified: int | None`, `cover_size: int | None`, `cover_modified: int | None` (all default `None`). `dump_series_file`'s entry shape gains the four keys, positioned after `archive_size` and before `offset` (Decisions above). No change to `validate.py` or `merge.py`: like `archive_size`, these are producer-only fields already covered by the existing "everything in `volumes` except `offset` is ignored" PUT rule (contract §6) — a client cannot inject or spoof them, so nothing at the untrusted boundary needs to change.

- [ ] **Step 1: Write the failing schema tests**

Append to `tests/unit/test_metadata_schema.py`, a new class placed directly after `TestDumpSeriesFile` (it extends that class's own fixtures):

```python
class TestFreshnessStamps:
    def test_all_four_present_sit_after_archive_size(self) -> None:
        data = dump_series_file(
            series_title="Bakemonogatari",
            facts=SeriesFacts(),
            index=SeriesIndexData(),
            volumes=[
                VolumeEntry(
                    volume_uuid="cfb5220c-57db-4008-9f44-e659d794e381",
                    volume_title="v01",
                    page_count=187,
                    character_count=13247,
                    mokuro_version="0.2.2",
                    archive_size=1234,
                    mokuro_size=45210,
                    mokuro_modified=1723996800,
                    cover_size=8192,
                    cover_modified=1723996900,
                )
            ],
        )
        assert data.decode("utf-8") == (
            '{"version":2,"series_title":"Bakemonogatari","external_ids":{},"titles":{},'
            '"synonyms":[],"updated_at":"1970-01-01T00:00:00.000Z","volumes":['
            '{"volume_uuid":"cfb5220c-57db-4008-9f44-e659d794e381","volume_title":"v01",'
            '"page_count":187,"character_count":13247,"mokuro_version":"0.2.2",'
            '"archive_size":1234,"mokuro_size":45210,"mokuro_modified":1723996800,'
            '"cover_size":8192,"cover_modified":1723996900}]}'
        )

    def test_stamps_sit_after_archive_size_and_before_offset(self) -> None:
        # Extends TestDumpSeriesFile.test_full_facts_offsets_and_natural_volume_order:
        # same fixture, "u2" gains stamps, "u10" keeps its trailing `offset`.
        data = dump_series_file(
            series_title="Dr Stone",
            facts=DR_STONE,
            index=SeriesIndexData(spine_offset=12.5, volume_offsets={"u10": -40, "u2": 0}),
            volumes=[
                VolumeEntry("u10", "Volume 10", 200, 10000, ""),
                VolumeEntry(
                    "u2", "Volume 2", 180, 9000, "0.2.2", spine_width=250.5,
                    archive_size=99, mokuro_size=15000, mokuro_modified=1700000100,
                    cover_size=4096, cover_modified=1700000200,
                ),
            ],
        )
        assert data.decode("utf-8") == (
            '{"version":2,"series_title":"Dr Stone",'
            '"external_ids":{"anilist":98416,"mal":103897},'
            '"titles":{"native":"Dr.STONE","romaji":"Dr. STONE"},'
            '"synonyms":["ドクターストーン"],"tag":"HD Scan","unit":"volumes",'
            '"spine_offset":12.5,"updated_at":"2026-08-18T19:36:24.324Z","volumes":['
            '{"volume_uuid":"u2","volume_title":"Volume 2","page_count":180,'
            '"character_count":9000,"mokuro_version":"0.2.2","spine_width":250.5,'
            '"archive_size":99,"mokuro_size":15000,"mokuro_modified":1700000100,'
            '"cover_size":4096,"cover_modified":1700000200},'
            '{"volume_uuid":"u10","volume_title":"Volume 10","page_count":200,'
            '"character_count":10000,"mokuro_version":"","offset":-40}]}'
        )

    def test_a_zero_stamp_is_written_not_omitted(self) -> None:
        # Unlike spine_width/archive_size (truthy `> 0` checks), the four
        # stamps use `is not None`: a literal epoch mtime or an empty-file
        # size is 0, and a real (if practically impossible) stat value must
        # round-trip rather than silently vanish like a missing one would.
        volume = VolumeEntry(
            "u1", "v1", 1, 0, "", mokuro_size=0, mokuro_modified=0,
            cover_size=0, cover_modified=0,
        )
        text = dump_series_file(
            series_title="S", facts=SeriesFacts(), index=SeriesIndexData(), volumes=[volume]
        ).decode("utf-8")
        assert (
            '"mokuro_size":0,"mokuro_modified":0,"cover_size":0,"cover_modified":0'
        ) in text

    def test_stamps_are_omitted_not_nulled_when_absent(self) -> None:
        volume = VolumeEntry("u1", "Volume 1", 1, 1, "0.2.2")  # all four default None
        text = dump_series_file(
            series_title="S", facts=SeriesFacts(), index=SeriesIndexData(), volumes=[volume]
        ).decode("utf-8")
        for key in ("mokuro_size", "mokuro_modified", "cover_size", "cover_modified"):
            assert f'"{key}"' not in text
        assert "null" not in text
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/unit/test_metadata_schema.py -q`
Expected: FAIL — `TypeError: __init__() got an unexpected keyword argument 'mokuro_size'`

- [ ] **Step 3: Extend `schema.py`**

In `src/mokuro_bunko/metadata/schema.py`, extend `VolumeEntry`:

```python
@dataclass(frozen=True)
class VolumeEntry:
    """One compiled volume. Offsets are applied at dump time, by uuid."""

    volume_uuid: str
    volume_title: str
    page_count: int
    character_count: int
    mokuro_version: str
    spine_width: float | None = None
    archive_size: int | None = None
    mokuro_size: int | None = None
    mokuro_modified: int | None = None
    cover_size: int | None = None
    cover_modified: int | None = None
```

and extend the entry-building loop in `dump_series_file` — insert right after the existing `archive_size` block and before the `offset` computation:

```python
        if _is_spine_width(volume.spine_width):
            entry["spine_width"] = volume.spine_width
        if _is_archive_size(volume.archive_size):
            entry["archive_size"] = volume.archive_size
        # `is not None`, not truthy: 0 is a real (if practically impossible)
        # stat value and must round-trip, unlike a missing spine_width/size.
        if volume.mokuro_size is not None:
            entry["mokuro_size"] = volume.mokuro_size
        if volume.mokuro_modified is not None:
            entry["mokuro_modified"] = volume.mokuro_modified
        if volume.cover_size is not None:
            entry["cover_size"] = volume.cover_size
        if volume.cover_modified is not None:
            entry["cover_modified"] = volume.cover_modified
        offset = index.volume_offsets.get(volume.volume_uuid)
        if offset:
            entry["offset"] = offset
```

- [ ] **Step 4: Run test to verify it passes**

Run: `uv run pytest tests/unit/test_metadata_schema.py -q`
Expected: PASS (26 tests: 22 existing + 4 new)

- [ ] **Step 5: Write the failing compiler tests**

Append to `tests/unit/test_metadata_compiler.py`, a new class after `TestEntryCache`:

```python
class TestFreshnessStamps:
    def test_mokuro_stamps_come_from_the_sidecars_own_stat(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        sidecar_stat = (series / "v1.mokuro").stat()
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_size == sidecar_stat.st_size
        assert entry.mokuro_modified == int(sidecar_stat.st_mtime)
        assert isinstance(entry.mokuro_modified, int)  # truncated, not the raw float

    def test_no_sidecar_means_no_mokuro_stamps(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "Volume 01.cbz", pages=3)   # image-only, no .mokuro at all
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_size is None
        assert entry.mokuro_modified is None

    def test_a_corrupt_but_present_sidecar_still_gets_stamped(self, library: Path) -> None:
        # A stat is not a parse: a sidecar that fails to parse still has a
        # real mtime/size on disk, and that is exactly the freshness
        # information a client needs in order to know a retry is worthwhile.
        series = library / "Dr Stone"
        write_cbz(series / "Volume 01.cbz", pages=3)
        (series / "Volume 01.mokuro").write_text("{ this is not json", encoding="utf-8")
        sidecar_stat = (series / "Volume 01.mokuro").stat()
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_version == ""          # still degrades to image-only
        assert entry.mokuro_size == sidecar_stat.st_size
        assert entry.mokuro_modified == int(sidecar_stat.st_mtime)

    def test_cover_stamps_come_from_the_webp_sidecar(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.webp").write_bytes(b"fake webp bytes")
        cover_stat = (series / "v1.webp").stat()
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.cover_size == cover_stat.st_size
        assert entry.cover_modified == int(cover_stat.st_mtime)

    def test_no_cover_means_no_cover_stamps(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.nocover").touch()   # extraction was attempted and failed
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.cover_size is None
        assert entry.cover_modified is None

    def test_cover_stamp_is_fresh_even_on_a_cached_entry(
        self, library: Path, tmp_path: Path
    ) -> None:
        # `cover_size`/`cover_modified` are deliberately OUTSIDE the entry
        # cache (Decisions, 2026-08-24): the cache exists to skip re-parsing
        # the `.mokuro`, not to skip a stat(). A cover that appears well
        # after the entry was cached — exactly what happens when the cover
        # worker (Task 12) runs on its own schedule — must show up on the
        # very next compile, not wait for the archive or sidecar to change.
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        database = Database(tmp_path / "test.db")

        first = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert first[0].cover_size is None   # no cover yet; entry gets cached

        (series / "v1.webp").write_bytes(b"fake webp bytes")
        cover_stat = (series / "v1.webp").stat()

        second = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert second[0].cover_size == cover_stat.st_size
        assert second[0].cover_modified == int(cover_stat.st_mtime)
        # The rest of the (expensive) entry still came from the cache, not a
        # re-parse — proving the split didn't quietly disable the cache.
        assert second[0].mokuro_version == "0.2.2"

    def test_mokuro_stamps_round_trip_through_the_entry_cache(
        self, library: Path, tmp_path: Path
    ) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        database = Database(tmp_path / "test.db")
        sidecar_stat = (series / "v1.mokuro").stat()

        compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        cbz_stat = (series / "v1.cbz").stat()
        cached = database.get_cached_volume_entry(
            "Dr Stone/v1.cbz",
            cbz_stat.st_size,
            cbz_stat.st_mtime,
            f"v1.mokuro:{sidecar_stat.st_size}:{sidecar_stat.st_mtime}",
        )
        assert cached is not None
        assert cached["mokuro_size"] == sidecar_stat.st_size
        assert cached["mokuro_modified"] == int(sidecar_stat.st_mtime)

        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert entry.mokuro_size == sidecar_stat.st_size
        assert entry.mokuro_modified == int(sidecar_stat.st_mtime)
```

- [ ] **Step 6: Run test to verify it fails**

Run: `uv run pytest tests/unit/test_metadata_compiler.py -q`
Expected: FAIL — once Step 3 has landed, `VolumeEntry(..., mokuro_size=...)` no longer raises `TypeError`, but `compile_series_volumes` never sets the field, so every `assert entry.mokuro_size == ...` fails with `AssertionError: None == <int>`.

- [ ] **Step 7: Extend `compiler.py`**

Pull the sidecar's `stat()` out of `_stat_key` so it can be reused, and thread it into `_compile_volume`:

```python
def _sidecar_stat(path: Path | None) -> os.stat_result | None:
    """The one stat() of a sidecar, shared by the cache key and the entry.

    Was inlined inside `_stat_key`; pulled out so `compile_series_volumes`
    can stat the sidecar exactly once and use the SAME `stat_result` both to
    build the cache-validation key and to stamp `mokuro_size`/
    `mokuro_modified` on the compiled entry — never a second syscall.
    """
    if path is None:
        return None
    try:
        return path.stat()
    except OSError:
        return None


def _stat_key(path: Path | None, stat_result: os.stat_result | None) -> str:
    """Compact identity of a sidecar for cache validation ("" = none)."""
    if path is None or stat_result is None:
        return ""
    return f"{path.name}:{stat_result.st_size}:{stat_result.st_mtime}"
```

Add the cover-stat helper (new; nothing in the compiler looks at cover files today), and its import — `OCRProcessor` only for its `get_cover_path` staticmethod, no instance, no circular import (`mokuro_bunko.ocr.processor` imports nothing from `mokuro_bunko.metadata`):

```python
from mokuro_bunko.ocr.processor import OCRProcessor


def _cover_stat(cbz_path: Path) -> os.stat_result | None:
    """Stat of this volume's cover sidecar, fresh on every call.

    Deliberately UNCACHED, unlike the sidecar/archive stats above: the entry
    cache exists to skip re-PARSING a `.mokuro` (the expensive part), not to
    skip a stat() (cheap). The cover worker (Task 12) runs on its own
    schedule and can produce a `.webp` well after this volume's entry was
    cached — if the cover's freshness lived in the cache too, a newly
    generated cover would never surface in `series.json` until the archive
    or sidecar also changed. Always statting keeps `cover_size`/
    `cover_modified` honest on every regeneration, cache hit or not.
    """
    cover_path = OCRProcessor.get_cover_path(cbz_path)
    try:
        return cover_path.stat()
    except OSError:
        return None
```

Change the `dataclasses` import to also bring in `replace`:

```python
from dataclasses import dataclass, replace
```

Thread `sidecar_stat` into `_compile_volume` and stamp both return branches:

```python
def _compile_volume(
    series_title: str,
    cbz_path: Path,
    sidecar: Path | None,
    sidecar_stat: os.stat_result | None,
) -> VolumeEntry:
    volume_title = cbz_path.with_suffix("").name
    data = _read_sidecar(sidecar) if sidecar is not None else None

    try:
        archive_size = cbz_path.stat().st_size
    except OSError:
        archive_size = 0

    mokuro_size = sidecar_stat.st_size if sidecar_stat is not None else None
    mokuro_modified = int(sidecar_stat.st_mtime) if sidecar_stat is not None else None

    if data is None:
        # Image-only (or an unreadable sidecar): the reader derives this uuid
        # for its placeholder, so deriving the same one keeps synced progress
        # attached when the index arrives.
        return VolumeEntry(
            volume_uuid=deterministic_uuid(f"{series_title}/{volume_title}"),
            volume_title=volume_title,
            page_count=_count_archive_images(cbz_path),
            character_count=0,
            mokuro_version="",
            archive_size=archive_size or None,
            mokuro_size=mokuro_size,
            mokuro_modified=mokuro_modified,
        )

    pages = data.get("pages")
    raw_uuid = data.get("volume_uuid")
    uuid = (
        raw_uuid
        if isinstance(raw_uuid, str) and raw_uuid.strip()
        else deterministic_uuid(f"{series_title}/{volume_title}")
    )
    raw_version = data.get("version")
    version = raw_version if isinstance(raw_version, str) else ""

    raw_chars = data.get("chars")
    if isinstance(raw_chars, int) and not isinstance(raw_chars, bool) and raw_chars > 0:
        character_count = raw_chars
    else:
        character_count = count_page_chars(pages)

    return VolumeEntry(
        volume_uuid=uuid,
        volume_title=volume_title,
        page_count=len(pages) if isinstance(pages, list) else _count_archive_images(cbz_path),
        character_count=character_count,
        mokuro_version=version,
        spine_width=_positive_number(data.get("spine_width")),
        archive_size=archive_size or None,
        mokuro_size=mokuro_size,
        mokuro_modified=mokuro_modified,
    )
```

Extend the cache round-trip helpers so `mokuro_size`/`mokuro_modified` survive a cache hit (`cover_size`/`cover_modified` stay OUT of these — they are never cached, see `_cover_stat`):

```python
def _entry_to_dict(entry: VolumeEntry) -> dict[str, Any]:
    return {
        "volume_uuid": entry.volume_uuid,
        "volume_title": entry.volume_title,
        "page_count": entry.page_count,
        "character_count": entry.character_count,
        "mokuro_version": entry.mokuro_version,
        "spine_width": entry.spine_width,
        "archive_size": entry.archive_size,
        "mokuro_size": entry.mokuro_size,
        "mokuro_modified": entry.mokuro_modified,
    }


def _entry_from_dict(raw: dict[str, Any]) -> VolumeEntry | None:
    try:
        return VolumeEntry(
            volume_uuid=str(raw["volume_uuid"]),
            volume_title=str(raw["volume_title"]),
            page_count=int(raw["page_count"]),
            character_count=int(raw["character_count"]),
            mokuro_version=str(raw["mokuro_version"]),
            spine_width=raw.get("spine_width"),
            archive_size=raw.get("archive_size"),
            mokuro_size=raw.get("mokuro_size"),
            mokuro_modified=raw.get("mokuro_modified"),
        )
    except (KeyError, TypeError, ValueError):
        return None
```

Finally, wire `compile_series_volumes` to stat the sidecar once, pass it through, and apply the (always-fresh) cover stat regardless of whether the entry came from cache:

```python
def compile_series_volumes(
    series: SeriesFolder,
    *,
    database: Database | None = None,
) -> list[VolumeEntry]:
    """Every volume of one series, in natural title order."""
    series_key = normalize_series_key(series.title)
    entries: list[VolumeEntry] = []

    for name in _archive_names(series.path):
        cbz_path = series.path / name
        volume_title = cbz_path.with_suffix("").name
        sidecar = _sidecar_for(cbz_path)
        sidecar_stat = _sidecar_stat(sidecar)
        sidecar_key = _stat_key(sidecar, sidecar_stat)
        try:
            cbz_stat = cbz_path.stat()
        except OSError:
            continue

        key = volume_key_for(series.title, volume_title)
        entry: VolumeEntry | None = None
        if database is not None:
            cached = database.get_cached_volume_entry(
                key, cbz_stat.st_size, cbz_stat.st_mtime, sidecar_key
            )
            if cached is not None:
                entry = _entry_from_dict(cached)

        if entry is None:
            entry = _compile_volume(series.title, cbz_path, sidecar, sidecar_stat)
            if database is not None:
                database.put_cached_volume_entry(
                    key,
                    series_key,
                    _entry_to_dict(entry),
                    cbz_stat.st_size,
                    cbz_stat.st_mtime,
                    sidecar_key,
                )

        # Cover stat is never cached — see `_cover_stat`'s docstring.
        cover_stat = _cover_stat(cbz_path)
        entry = replace(
            entry,
            cover_size=cover_stat.st_size if cover_stat is not None else None,
            cover_modified=int(cover_stat.st_mtime) if cover_stat is not None else None,
        )
        entries.append(entry)

    entries.sort(key=lambda item: (natural_sort_key(item.volume_title), item.volume_title))
    return entries
```

- [ ] **Step 8: Run test to verify it passes**

Run: `uv run pytest tests/unit/test_metadata_compiler.py -q`
Expected: PASS (26 tests: 19 existing + 7 new)

- [ ] **Step 9: Run the full regression suites**

Run: `uv run pytest tests/unit/test_metadata_schema.py tests/unit/test_metadata_compiler.py tests/unit/test_metadata_service.py tests/integration/test_metadata_distribution.py -q`
Expected: PASS. `test_metadata_service.py`/`test_metadata_distribution.py`'s fixtures write real `.mokuro` files and exercise `compile_series_volumes` indirectly through `MetadataService.regenerate_all`/`regenerate_series`, so their compiled entries will now carry `mokuro_size`/`mokuro_modified`; none of their existing assertions pin an exact volume-entry key set, so this should not break anything — confirm rather than assume.

- [ ] **Step 10: Check lint and types**

Run: `uv run ruff check src/ && uv run mypy src/`
Expected: no findings. `os.stat_result` is a real stdlib type; `_sidecar_stat`/`_cover_stat`'s `-> os.stat_result | None` annotations satisfy strict mode without a cast.

- [ ] **Step 11: Commit**

```bash
git add src/mokuro_bunko/metadata/schema.py src/mokuro_bunko/metadata/compiler.py \
        tests/unit/test_metadata_schema.py tests/unit/test_metadata_compiler.py
git commit -m "feat(metadata): mokuro/cover freshness stamps on volume entries"
```

---

### Task 12: Covers when OCR is off (contract §8)

Cover sidecars already exist: `OCRProcessor.ensure_thumbnail` extracts the archive's first image and writes a 250×350 WebP next to the `.cbz` (with a `.nocover` marker when there is no image), and `OCRWorker` runs a thumbnail loop beside its OCR loop. The gap is that `run_server` starts `OCRWorker` **only** when `config.ocr.backend != "skip"`, so a server with OCR disabled never generates a cover — and the contract makes covers unconditional, because the reader now installs them onto materialized rows. The scoped-user half of §8 needs no code (`registered` lacks `ADD_FILES`, pinned by Task 11's tests). This task is only about making sure the worker that PRODUCES a `<Volume>.webp` runs unconditionally; Task 11b (already landed if the plan is executed in order) is what makes `compile_series_volumes` STAT that `.webp` and stamp `cover_size`/`cover_modified` onto the entry — no further change to `compiler.py` is needed here.

**Files:**
- Modify: `src/mokuro_bunko/ocr/watcher.py` (`OCRWorker.__init__` around line 234, `start` around line 455)
- Modify: `src/mokuro_bunko/server.py` (`run_server`, the OCR-worker block around lines 498–509)
- Test: `tests/unit/test_thumbnail_only_worker.py`

**Interfaces:**
- Consumes: `OCRProcessor.ensure_thumbnail`, `OCRProcessor.needs_thumbnail` (unchanged).
- Produces: `OCRWorker(storage_path, poll_interval=30.0, status_callback=None, thumbnails_only=False)`.

- [ ] **Step 1: Write the failing test**

```python
"""Covers are generated even when the OCR backend is disabled (contract §8)."""

from __future__ import annotations

import zipfile
from io import BytesIO
from pathlib import Path

import pytest

from mokuro_bunko.ocr.watcher import OCRWorker


def make_cbz(path: Path) -> None:
    Image = pytest.importorskip("PIL.Image")
    path.parent.mkdir(parents=True, exist_ok=True)
    buffer = BytesIO()
    Image.new("RGB", (600, 900), color=(30, 90, 150)).save(buffer, format="JPEG")
    with zipfile.ZipFile(path, "w") as archive:
        archive.writestr("001.jpg", buffer.getvalue())


def test_thumbnail_only_worker_never_starts_the_ocr_loop(tmp_path: Path) -> None:
    worker = OCRWorker(storage_path=tmp_path, poll_interval=0.1, thumbnails_only=True)
    worker.start(background=True)
    try:
        assert worker._ocr_thread is None
        assert worker._thumb_thread is not None
        assert worker._thumb_thread.is_alive()
    finally:
        worker.stop()


def test_thumbnail_only_worker_still_generates_covers(tmp_path: Path) -> None:
    cbz = tmp_path / "library" / "Dr Stone" / "Volume 01.cbz"
    make_cbz(cbz)

    worker = OCRWorker(storage_path=tmp_path, poll_interval=0.1, thumbnails_only=True)
    worker._scan_thumbnails_once()

    assert cbz.with_suffix(".webp").exists()


def test_a_normal_worker_still_starts_both_loops(tmp_path: Path) -> None:
    worker = OCRWorker(storage_path=tmp_path, poll_interval=0.1)
    worker.start(background=True)
    try:
        assert worker._ocr_thread is not None
        assert worker._thumb_thread is not None
    finally:
        worker.stop()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/unit/test_thumbnail_only_worker.py -q`
Expected: FAIL — `TypeError: __init__() got an unexpected keyword argument 'thumbnails_only'`

- [ ] **Step 3: Add the mode to `OCRWorker`**

In `src/mokuro_bunko/ocr/watcher.py`, extend the constructor signature and body:

```python
    def __init__(
        self,
        storage_path: Path,
        poll_interval: float = 30.0,
        status_callback: Callable[[str], None] | None = None,
        thumbnails_only: bool = False,
    ) -> None:
        """Initialize the OCR worker.

        Args:
            storage_path: Base storage path.
            poll_interval: How often to poll for new files.
            status_callback: Optional callback for status messages.
            thumbnails_only: Run only the cover-generation loop. Cover
                sidecars are part of the metadata contract and must exist even
                on servers whose OCR backend is `skip`; generating them needs
                Pillow, never the mokuro environment.
        """
```

and store `self.thumbnails_only = thumbnails_only` beside the other attributes.

In `start`, replace the body from `removed = self._remove_corrupt_sidecars()` down to the end with:

```python
        if not self.thumbnails_only:
            removed = self._remove_corrupt_sidecars()
            if removed:
                self._log(f"Removed {removed} corrupt mokuro sidecar file(s) at startup")

        self._running = True
        self._log(
            "Cover worker starting..." if self.thumbnails_only else "OCR worker starting..."
        )

        self._thumb_thread = threading.Thread(
            target=self._run_thumbnail_loop,
            daemon=True,
            name="ocr-thumbnail-worker",
        )

        if self.thumbnails_only:
            self._thumb_thread.start()
            self._log("Cover worker started in background (thumbnail loop only)")
            return

        if background:
            self._ocr_thread = threading.Thread(
                target=self._run_ocr_loop,
                daemon=True,
                name="ocr-sidecar-worker",
            )
            self._ocr_thread.start()
            self._thumb_thread.start()
            self._log("OCR worker started in background (sidecar + thumbnail loops)")
        else:
            self._thumb_thread.start()
            self._run_ocr_loop()
```

- [ ] **Step 4: Start it when OCR is disabled**

In `src/mokuro_bunko/server.py`'s `run_server`, replace the worker-start block

```python
    if config.ocr.backend != "skip" and selected_backend is not None and selected_backend != OCRBackend.SKIP:
        ocr_worker = OCRWorker(...)
        ocr_worker.start(background=True)
        print(...)
```

with

```python
    if (
        config.ocr.backend != "skip"
        and selected_backend is not None
        and selected_backend != OCRBackend.SKIP
    ):
        ocr_worker = OCRWorker(
            storage_path=config.storage.base_path,
            poll_interval=float(config.ocr.poll_interval),
            status_callback=lambda msg: print(f"[OCR] {msg}"),
        )
        ocr_worker.start(background=True)
        print(
            "OCR worker enabled "
            f"(configured={config.ocr.backend}, active={selected_backend.value}, "
            f"interval={config.ocr.poll_interval}s)"
        )
    else:
        # Covers are part of the metadata contract, so they are generated even
        # with OCR disabled: the thumbnail loop needs Pillow, not mokuro.
        ocr_worker = OCRWorker(
            storage_path=config.storage.base_path,
            poll_interval=float(config.ocr.poll_interval),
            status_callback=lambda msg: print(f"[COVERS] {msg}"),
            thumbnails_only=True,
        )
        ocr_worker.start(background=True)
        print("OCR disabled; cover generation worker enabled")
```

- [ ] **Step 5: Run the tests**

Run: `uv run pytest tests/unit/test_thumbnail_only_worker.py tests/unit/test_ocr_thumbnail_generation.py tests/unit/test_ocr_processor_progress.py -q`
Expected: PASS (3 new tests; the existing OCR suites unchanged)

- [ ] **Step 6: Check lint and types**

Run: `uv run ruff check src/ && uv run mypy src/`
Expected: no findings

- [ ] **Step 7: Commit**

```bash
git add src/mokuro_bunko/ocr/watcher.py src/mokuro_bunko/server.py \
        tests/unit/test_thumbnail_only_worker.py
git commit -m "feat(covers): generate cover sidecars even when OCR is disabled"
```

---

### Task 13: End-to-end through the real WSGI stack

Everything above is unit-level. This drives the assembled application — `create_app` with the real auth, metadata and DAV middleware — the way the reader client does: an authorized user PUTs a `series.json`, gets it validated/merged/regenerated, then reads `catalog.json` back; blocked writes come back as ordinary errors; the partitioning holds. (2026-08-24: "authorized" means the ownership-gated Task 11 rule, not "any scoped user" — see `TestAuthorizedUpdate` below, which exercises both the `uploader`-ownership path and the `MODIFY_DELETE` path, plus a dedicated `registered`-is-403 pin.) `TestServing.test_compiled_files_are_served_with_accurate_size` also pins that Task 11b's freshness stamps survive being served through the real WSGI stack, not just `compile_series_volumes` in isolation.

The fixture also **stops** the watcher, the PROPFIND cache timer and the metadata timer on teardown. The existing `app` fixture in `test_webdav_ops.py` does not, which is why a long combined run can exhaust inotify watches; the new file does not add to that.

**Files:**
- Create: `tests/integration/test_metadata_distribution.py`

**Interfaces:**
- Consumes: `mokuro_bunko.server.create_app`, `tests.integration.test_webdav_ops.WSGITestClient`, `tests.integration.test_webdav_ops.make_auth_header`, `app._metadata_service`.
- Produces: nothing (test-only).

- [ ] **Step 1: Write the test file**

```python
"""End-to-end metadata distribution through the assembled WSGI stack."""

from __future__ import annotations

import json
import zipfile
from pathlib import Path
from typing import Any, Generator

import pytest

from mokuro_bunko.config import Config, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.server import create_app
from tests.integration.test_webdav_ops import WSGITestClient, make_auth_header

READER = {"Authorization": make_auth_header("reader", "pass1234")}
UPLOADER = {"Authorization": make_auth_header("uploader", "pass1234")}
EDITOR = {"Authorization": make_auth_header("editor", "pass1234")}
ADMIN = {"Authorization": make_auth_header("admin", "pass1234")}

SERIES_PATH = "/mokuro-reader/Dr Stone/series.json"
CATALOG_PATH = "/mokuro-reader/catalog.json"


def write_volume(library: Path, series: str, volume: str, uuid: str) -> None:
    folder = library / series
    folder.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(folder / f"{volume}.cbz", "w") as archive:
        archive.writestr("000.jpg", b"fake image bytes")
    (folder / f"{volume}.mokuro").write_text(
        json.dumps(
            {
                "version": "0.2.2",
                "title": series,
                "title_uuid": "t-uuid",
                "volume": volume,
                "volume_uuid": uuid,
                "pages": [{"blocks": [{"lines": ["世界"]}]}],
            }
        ),
        encoding="utf-8",
    )


def update_payload(**overrides: object) -> bytes:
    body: dict[str, object] = {
        "version": 2,
        "series_title": "Dr Stone",
        "external_ids": {"anilist": 98416},
        "titles": {"native": "Dr.STONE"},
        "synonyms": [],
        "tag": "HD Scan",
        "unit": "volumes",
        "updated_at": "2026-08-18T19:36:24.324Z",
        "volumes": [],
    }
    body.update(overrides)
    return json.dumps(body).encode("utf-8")


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    base = tmp_path / "storage"
    (base / "library").mkdir(parents=True)
    (base / "inbox").mkdir()
    (base / "users").mkdir()
    write_volume(base / "library", "Dr Stone", "Volume 01", "uuid-volume-01")
    write_volume(base / "library", "Aria", "v1", "uuid-aria-v1")
    return base


@pytest.fixture
def database(storage: Path) -> Database:
    db = Database(storage / "mokuro.db")
    db.create_user("reader", "pass1234", "registered")
    db.create_user("uploader", "pass1234", "uploader")
    db.create_user("editor", "pass1234", "editor")
    db.create_user("admin", "pass1234", "admin")
    # `uploader` legitimately owns "Dr Stone" (2026-08-24 ownership-gated PUT
    # policy, Task 11): `TestAuthorizedUpdate` relies on this to exercise the
    # uploader-ownership authorization path, not just the MODIFY_DELETE path.
    db.record_volume_upload("Dr Stone/Volume 01.cbz", "uploader")
    return db


@pytest.fixture
def app(storage: Path, database: Database) -> Generator[Any, None, None]:
    application = create_app(Config(storage=StorageConfig(base_path=storage)))
    application._metadata_service.regenerate_all()
    yield application
    application._metadata_service.stop()
    application._propfind_cache.stop()
    application._library_watcher.stop()


@pytest.fixture
def client(app: Any) -> WSGITestClient:
    return WSGITestClient(app)


class TestServing:
    def test_compiled_files_are_served_with_accurate_size(
        self, client: WSGITestClient, storage: Path
    ) -> None:
        response = client.get(SERIES_PATH, READER)
        assert response.status_code == 200
        on_disk = (storage / "library" / "Dr Stone" / "series.json").read_bytes()
        assert response.content == on_disk
        assert dict(response.headers)["Content-Length"] == str(len(on_disk))

        document = json.loads(response.text)
        assert document["version"] == 2
        assert document["series_title"] == "Dr Stone"
        assert document["volumes"][0]["volume_uuid"] == "uuid-volume-01"
        assert document["volumes"][0]["character_count"] == 2
        # Freshness stamps (Task 11b): `write_volume` puts a real `.mokuro`
        # sidecar on disk, so its stat must come through into the compiled
        # entry; no `.webp` exists in this fixture, so the cover pair is
        # absent rather than nulled.
        mokuro_stat = (storage / "library" / "Dr Stone" / "Volume 01.mokuro").stat()
        assert document["volumes"][0]["mokuro_size"] == mokuro_stat.st_size
        assert document["volumes"][0]["mokuro_modified"] == int(mokuro_stat.st_mtime)
        assert "cover_size" not in document["volumes"][0]
        assert "cover_modified" not in document["volumes"][0]

    def test_the_catalog_lists_every_series_by_folder_name(
        self, client: WSGITestClient
    ) -> None:
        catalog = json.loads(client.get(CATALOG_PATH, READER).text)
        assert catalog["version"] == 1
        assert [entry["series_title"] for entry in catalog["series"]] == ["Aria", "Dr Stone"]
        # Factless series still get an entry, at the epoch.
        assert catalog["series"][0]["updated_at"] == "1970-01-01T00:00:00.000Z"


class TestAuthorizedUpdate:
    """2026-08-24: authorization is ownership-gated (Task 11), not a plain
    "any scoped user" gate. `UPLOADER` here owns "Dr Stone" via the
    `database` fixture's `record_volume_upload` call; `READER` (`registered`)
    never reaches `MetadataAPI` regardless of ownership."""

    def test_put_is_accepted_validated_merged_and_regenerated(
        self, client: WSGITestClient
    ) -> None:
        response = client.put(
            SERIES_PATH,
            update_payload(
                spine_offset=12.5,
                volumes=[
                    # Deliberate lies: only the offset survives.
                    {
                        "volume_uuid": "uuid-volume-01",
                        "volume_title": "NONSENSE",
                        "page_count": 9999,
                        "character_count": 1,
                        "mokuro_version": "9.9",
                        "offset": -40,
                    },
                    {"volume_uuid": "ghost", "volume_title": "Ghost", "page_count": 1,
                     "character_count": 1, "mokuro_version": ""},
                ],
            ),
            UPLOADER,
        )
        assert response.status_code == 204
        assert response.content == b""

        document = json.loads(client.get(SERIES_PATH, READER).text)
        assert document["external_ids"] == {"anilist": 98416}
        assert document["titles"] == {"native": "Dr.STONE"}
        assert document["tag"] == "HD Scan"
        assert document["unit"] == "volumes"
        assert document["updated_at"] == "2026-08-18T19:36:24.324Z"
        assert document["spine_offset"] == 12.5
        # bunko's own compilation wins, and the ghost volume never appears.
        assert [entry["volume_title"] for entry in document["volumes"]] == ["Volume 01"]
        assert document["volumes"][0]["page_count"] == 1
        assert document["volumes"][0]["offset"] == -40

        catalog = json.loads(client.get(CATALOG_PATH, READER).text)
        entry = next(item for item in catalog["series"] if item["series_title"] == "Dr Stone")
        assert entry["external_ids"] == {"anilist": 98416}
        assert entry["tag"] == "HD Scan"
        assert entry["updated_at"] == "2026-08-18T19:36:24.324Z"
        assert "volumes" not in entry

    def test_retrying_the_same_put_is_accepted_and_touches_nothing(
        self, client: WSGITestClient, storage: Path
    ) -> None:
        client.put(SERIES_PATH, update_payload(), UPLOADER)
        compiled = storage / "library" / "Dr Stone" / "series.json"
        before = compiled.stat().st_mtime_ns

        assert client.put(SERIES_PATH, update_payload(), UPLOADER).status_code == 204
        assert compiled.stat().st_mtime_ns == before

    def test_an_invalid_payload_is_an_ordinary_400(
        self, client: WSGITestClient, storage: Path
    ) -> None:
        compiled = storage / "library" / "Dr Stone" / "series.json"
        before = compiled.read_bytes()

        response = client.put(SERIES_PATH, b"not json at all", UPLOADER)
        assert response.status_code == 400
        assert compiled.read_bytes() == before

    def test_an_anonymous_put_is_rejected(self, client: WSGITestClient) -> None:
        assert client.put(SERIES_PATH, update_payload(), {}).status_code == 401

    def test_a_registered_user_is_rejected_end_to_end(
        self, client: WSGITestClient, storage: Path
    ) -> None:
        """2026-08-24 ruling: `registered` never reaches `MetadataAPI`,
        ownership or not — pinned here at the full WSGI-stack level, not just
        in the unit-level `test_metadata_permissions.py` policy tests."""
        compiled = storage / "library" / "Dr Stone" / "series.json"
        before = compiled.read_bytes()

        response = client.put(SERIES_PATH, update_payload(), READER)
        assert response.status_code == 403
        assert compiled.read_bytes() == before

    def test_an_editor_is_intercepted_without_needing_ownership(
        self, client: WSGITestClient, storage: Path
    ) -> None:
        """A `MODIFY_DELETE`-holding role (`editor` here) needs no ownership
        row at all — unlike the `UPLOADER` cases above, which rely on the
        `database` fixture's ownership grant over "Dr Stone"."""
        assert client.put(SERIES_PATH, update_payload(), EDITOR).status_code == 204
        compiled = json.loads(
            (storage / "library" / "Dr Stone" / "series.json").read_text("utf-8")
        )
        # Not the raw bytes that were sent: the compiled index is still bunko's.
        assert compiled["volumes"][0]["volume_uuid"] == "uuid-volume-01"


class TestBlockedWrites:
    def test_nobody_may_write_the_catalog(
        self, client: WSGITestClient, storage: Path
    ) -> None:
        compiled = storage / "library" / "catalog.json"
        before = compiled.read_bytes()
        for headers in (READER, UPLOADER, EDITOR, ADMIN):
            assert client.put(CATALOG_PATH, b'{"version":1}', headers).status_code == 403
        assert compiled.read_bytes() == before

    def test_nobody_may_delete_a_compiled_file(
        self, client: WSGITestClient, storage: Path
    ) -> None:
        assert client.delete(SERIES_PATH, ADMIN).status_code == 403
        assert client.delete(CATALOG_PATH, ADMIN).status_code == 403
        assert (storage / "library" / "Dr Stone" / "series.json").exists()

    def test_a_scoped_user_may_not_write_archives_or_covers(
        self, client: WSGITestClient
    ) -> None:
        assert client.put(
            "/mokuro-reader/Dr Stone/Volume 02.cbz", b"nope", READER
        ).status_code == 403
        assert client.put(
            "/mokuro-reader/Dr Stone/Volume 01.webp", b"nope", READER
        ).status_code == 403

    def test_a_scoped_user_can_still_write_their_own_progress(
        self, client: WSGITestClient, storage: Path
    ) -> None:
        """The whole point of §5: metadata rejection is not read-only mode."""
        response = client.put(
            "/mokuro-reader/volume-data.json", b'{"progress":true}', READER
        )
        assert response.status_code in (200, 201, 204)
        assert (storage / "users" / "reader" / "volume-data.json").exists()


class TestPartitioning:
    def test_metadata_never_lands_in_a_users_private_directory(
        self, client: WSGITestClient, storage: Path
    ) -> None:
        client.put(SERIES_PATH, update_payload(), READER)
        client.put("/mokuro-reader/volume-data.json", b'{"progress":true}', READER)

        user_files = sorted(p.name for p in (storage / "users" / "reader").iterdir())
        assert user_files == ["volume-data.json"]
        assert (storage / "library" / "Dr Stone" / "series.json").exists()

    def test_a_stale_root_series_metadata_json_is_inert(
        self, client: WSGITestClient, storage: Path
    ) -> None:
        stale = storage / "library" / "series-metadata.json"
        stale.write_text('{"version":1,"series":{}}', encoding="utf-8")

        assert client.get("/mokuro-reader/series-metadata.json", READER).status_code == 200
        catalog = json.loads(client.get(CATALOG_PATH, READER).text)
        assert "series-metadata.json" not in [
            entry["series_title"] for entry in catalog["series"]
        ]


class TestAdvertisement:
    def test_the_identity_endpoint_still_answers_in_contract(
        self, client: WSGITestClient
    ) -> None:
        """Contract §7: this answer is what makes the client stop compiling."""
        anonymous = json.loads(client.get("/login/api/me").text)
        assert anonymous["authenticated"] is False
        assert set(anonymous["permissions"]) == {
            "canWriteProgress",
            "canAddFiles",
            "canModifyDelete",
        }

        authenticated = json.loads(client.get("/login/api/me", READER).text)
        assert authenticated["authenticated"] is True
        assert authenticated["permissions"]["canWriteProgress"] is True
        assert authenticated["permissions"]["canAddFiles"] is False


class TestRegenerationTriggers:
    def test_a_library_change_schedules_a_recompilation(
        self, app: Any, storage: Path
    ) -> None:
        """The watcher's hook is wired to the service (contract §4)."""
        write_volume(storage / "library", "Dr Stone", "Volume 02", "uuid-volume-02")
        app._library_watcher.on_change()
        assert app._metadata_service._timer is not None
        app._metadata_service.stop()

        app._metadata_service.regenerate_all()
        document = json.loads(
            (storage / "library" / "Dr Stone" / "series.json").read_text("utf-8")
        )
        assert [entry["volume_title"] for entry in document["volumes"]] == [
            "Volume 01",
            "Volume 02",
        ]
```

- [ ] **Step 2: Run it**

Run: `uv run pytest tests/integration/test_metadata_distribution.py -q`
Expected: PASS (16 tests). If the cross-module import of `WSGITestClient` fails because of pytest's import mode, copy the `WSGITestClient`/`WSGIResponse`/`make_auth_header` definitions into the new file instead — they are ~60 lines and test-only.

- [ ] **Step 3: Commit**

```bash
git add tests/integration/test_metadata_distribution.py
git commit -m "test: end-to-end metadata distribution through the WSGI stack"
```

---

### Task 14: Full verification, docs and changelog

**Files:**
- Modify: `CHANGELOG.md` (new `## [Unreleased]` section above `## [0.1.8]`)
- Modify: `docs/configuration.md` (the `### OCR` section, line 186; new `## Compiled metadata files` section before `## Environment Variables`, line 221)

- [ ] **Step 1: Run the complete suite**

Run: `uv run pytest tests/unit tests/integration -q`
Expected: PASS. The 726 baseline tests must all still pass; the new/extended files add about 181 more (11 + 15 + 12 + 16 + 10 + 18 + 14 + 7 + 17 + 9 + 25 + 11 + 3 + 15 — the `11` is Task 11b's additions to the already-counted `test_metadata_schema.py`/`test_metadata_compiler.py`). Record the final count in the commit message.

- [ ] **Step 2: Lint and type-check exactly as CI does**

Run: `uv run ruff check src/ && uv run mypy src/`
Expected: no findings (CI runs `ruff check src/` and `mypy src/`)

- [ ] **Step 3: Prove the compiled output round-trips through the client's parser**

The client is the consumer of record, so check a compiled file against its own parser rather than trusting the schema tests. In the reader worktree
`/home/nathan/Projects/mokuro-reader-worktrees/feat/series-metadata`, run:

```bash
cd /home/nathan/Projects/mokuro-reader-worktrees/feat/series-metadata
npx vitest run src/lib/metadata/series-file.test.ts src/lib/metadata/catalog-file.test.ts
```

then paste one compiled `series.json` and one compiled `catalog.json` (produced by
`MetadataService.regenerate_all()` over a fixture library) into a scratch test that calls
`parseSeriesFile(JSON.parse(text))` / `parseCatalogFile(JSON.parse(text))` and assert both
return a document (not `undefined`) whose `updated_at`, `external_ids`, `titles`,
`volumes[].volume_uuid` and `volumes[].offset` match the bytes bunko wrote. Delete the
scratch test afterwards — it is a one-time cross-repo check, not a fixture to maintain.

Expected: both parse, with every field preserved. If either returns `undefined`, the
compiled shape is wrong and the schema task must be fixed before merging.

- [ ] **Step 4: Add the changelog entry**

At the top of `CHANGELOG.md`, above `## [0.1.8] - 2026-07-08`:

```markdown
## [Unreleased]

### Added
- **Server-side compilation of the reader's metadata files.** mokuro-bunko now compiles `<Series>/series.json` (v2: series facts plus an index of the series' volumes — uuid, title, page and character counts, mokuro version, spine width, archive size, freshness stamps, shelf offsets) and a root `catalog.json` (name/mapping/search data for every series folder) from the library's own `.mokuro` and `.cbz` files. Both are regenerated when the library changes and served with accurate size/mtime, so clients can cache them; a rebuild that changes nothing rewrites nothing. Character counts are computed with the reader's own counting rules, and volumes with no OCR sidecar are indexed as image-only with the uuid the reader derives for them.
- **Metadata updates from accounts that cannot write to the library.** A `series.json` PUT is treated as an update REQUEST: the facts fields are validated, merged newest-stamp-wins against the server's store (a factless payload never clears a link unless it is strictly newer — an explicit unlink), and both files are regenerated. Shelf offsets ride along as index data and never move the facts stamp. Repeating an identical update is a no-op, so a client can retry safely.
- **Cover sidecars are generated even when OCR is disabled** (`backend: skip`), since the reader now installs them onto volumes it has not downloaded.
- **Freshness stamps on each volume entry.** `series.json` volume entries optionally carry `mokuro_size`/`mokuro_modified` and `cover_size`/`cover_modified` — integer byte sizes and integer epoch seconds from a plain `stat()` of the `.mokuro` sidecar and the cover `.webp`, omitted (not `null`) when either doesn't exist. Clients use these to detect a stale local copy without downloading anything: a size mismatch, or a strictly newer `_modified` than what they have stored, means re-fetch.

### Changed
- Compiled metadata files are owned by the server: `catalog.json` cannot be written by any account, and neither compiled file can be deleted, moved or copied. Rejections are ordinary 403s — a client that treats metadata writes as best-effort keeps full read/write access to everything else.
```

- [ ] **Step 5: Document it**

In `docs/configuration.md`, in the `### OCR` section after the backend table, add:

```markdown
Cover thumbnails (`<Volume>.webp`, generated from each archive's first page) are
produced regardless of the backend — including `skip` — because readers use them
for volumes they have not downloaded. Only the OCR sidecar generation follows the
`backend` setting.
```

and before `## Environment Variables`, add:

```markdown
## Compiled metadata files

The server compiles two files into the shared library and keeps them current:

| File | Contents |
| --- | --- |
| `<Series>/series.json` | The series' facts (external ids, titles, synonyms, tag, unit) plus an index of its volumes: uuid, title, page and character counts, mokuro version, spine width, archive size, freshness stamps and shelf offsets. |
| `catalog.json` (library root) | One entry per series folder with the same facts — name, mapping and search data only. |

Both are regenerated when the library changes and whenever a client submits an
update, and are rewritten only when their content actually changed, so clients can
cache them on size/mtime.

Each volume entry may also carry `mokuro_size`/`mokuro_modified` and
`cover_size`/`cover_modified`: the byte size and integer epoch-second mtime of the
`.mokuro` sidecar and the cover `.webp`, taken from a plain filesystem stat when the
entry is compiled. Either pair is omitted (never `null`) when its file doesn't
exist. A client uses these to decide whether its own cached copy is stale without
downloading anything: rebuild when the stamped size differs from what it has, or
the stamped `_modified` is strictly newer than what it stored; an older-or-equal
`_modified` at an equal size is fresh. Stamps are always whole seconds, never
sub-second, because a generic WebDAV client only ever sees second-precision
`Last-Modified` HTTP dates.

Clients do not write these files. A `PUT` of `<Series>/series.json` is accepted as
an update *request*: the facts are validated and merged (newest stamp wins), the
volume list in the request is ignored in favour of the server's own compilation,
and both files are regenerated. A body carrying only facts, with no volume list at
all, is an equally valid update. Writing `catalog.json`, or deleting/moving either
file, is refused for every account. Submitting an update is ownership-gated, not a
plain progress-write permission: an editor-tier account (or above) may update any
series, an uploader account only a series it uploaded, and a registered-only
account cannot submit updates at all. The account that submitted an accepted
update is recorded in the audit log.
```

- [ ] **Step 6: Commit**

```bash
git add CHANGELOG.md docs/configuration.md
git commit -m "docs: compiled metadata files, cover generation and changelog"
```

- [ ] **Step 7: Final green check**

Run: `uv run pytest tests/unit tests/integration -q && uv run ruff check src/ && uv run mypy src/`
Expected: all green. Do not report the feature complete without this output in hand.

---

## Out of scope / follow-ups

- **Deployment (contract task 9).** Rebuilding and redeploying the unraid container is deliberately NOT in this plan: the compose file and unraid template on that box diverge from the repo, so it must follow the recorded recipe (project memory `project_mokuro_bunko_deploy`: rsync source to a fresh build dir, `docker build -f deploy/Dockerfile.unraid`, verify the version inside the image, retag, recreate from the `runlike` command, wait for `health=healthy`). Bump `pyproject.toml`'s version and verify against a live scoped account plus a real reader client as part of that work.
- **Facts do not follow a folder rename.** They are keyed by normalized series title; a renamed folder compiles factless until a client republishes. Carrying facts across a MOVE would mean hooking `move_recursive`, which is a separate change.
- **Per-series ACL added 2026-08-24 (Task 11).** "Within the user's permission scope" is an ownership check built on the existing `volume_uploads` model, not a flat permission gate: `uploader` may edit only a series it owns outright; any `MODIFY_DELETE`-holding role may edit any series; `registered` may never submit a metadata update. A series with no tracked volumes is 403 for `uploader` (safe default, not a free-for-all). `Database.can_user_edit_series`'s folder-prefix match is a plain unescaped `LIKE '<title>/%'` (same pattern `forget_volume_uploads_under_prefix` already uses) — a folder literally named with `%`/`_` could over-match, but the ownership check's AND-of-all-owners semantics fails closed on any mismatch (extra matched rows owned by someone else deny access rather than grant it), so this is a correctness sharp edge, not a privilege-escalation path.
- **Progress-file handling** beyond the partitioning guarantee, and any reader-client work (see `2026-08-23-catalog-distribution-client.md`), stay out per the contract.

## Self-review

Checked after writing, against the contract and the spec's amendment sections.

**Spec coverage**

| Contract clause | Task(s) |
| --- | --- |
| §1 partitioning (+ stale `series-metadata.json`) | 1, 13 |
| §2 compiled `series.json` shape, index fields verbatim | 3, 4, 7, 9, 11b |
| §2 freshness stamps + staleness rule (2026-08-24 addendum) | 11b, 13 |
| §3 compiled `catalog.json`, stable ordering, factless entries | 3, 9 |
| §4 accurate size/mtime, regenerate on change and on update | 8, 9, 10, 13 |
| §5 write blocking as an ordinary error | 11, 13 |
| §6 intercepted PUT: validate / merge / regenerate / idempotent | 4, 6, 9, 10, 11, 13 |
| §7 compilation advertisement (no code change; regression-tested) | 13 |
| §8 cover sidecars, not overwritable by scoped users | 11, 12, 11b |
| Bunko tasks 1–8 of the contract's own list | 1 → 12 respectively |
| Contract task 9 (deploy) | out of scope, recorded above |

**Type consistency** — `SeriesFacts`, `SeriesIndexData`, `VolumeEntry`, `SeriesUpdate`, `StoredSeries`, `MergeResult`, `SeriesFactsRow`, `SeriesFolder` keep the same field names everywhere they appear; `write_if_changed` returns `bool` in both its definition and every call site; `apply_series_update(series_title, payload, actor)` has the same signature in the service, the middleware and the stub used to test the middleware.

**Known sharp edges deliberately accepted**

- `natural_sort_key` is not ICU: it is required to be total and stable, not identical to `Intl.Collator`. Nothing downstream depends on the file's order.
- The counted-character table is Unicode-version-bound (generated from Node's ICU). The CJK/kana ranges that matter are stable; a mismatch in an exotic plane would move a character count by one on a not-installed volume.
- A `.mokuro` that is corrupt at compile time yields a derived uuid; if it is later repaired the uuid changes, and a client that already materialized the old one relies on its own stranded-row cleanup.

## Execution handoff

Plan complete and saved to `docs/superpowers/plans/2026-08-24-catalog-distribution.md`. Two execution options:

1. **Subagent-Driven (recommended)** — a fresh subagent per task, review between tasks, fast iteration (REQUIRED SUB-SKILL: `superpowers:subagent-driven-development`).
2. **Inline Execution** — execute tasks in this session with checkpoints (REQUIRED SUB-SKILL: `superpowers:executing-plans`).


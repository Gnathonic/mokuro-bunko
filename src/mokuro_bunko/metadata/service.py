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

import os
import sys
import threading
from collections.abc import Callable, Sequence
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
    VolumeEntry,
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
        self._stopped = False

    # --- state -----------------------------------------------------------

    def _stored(self, series_key: str) -> StoredSeries | None:
        row = self.database.get_series_facts(series_key)
        if row is None:
            return None
        return StoredSeries(facts=_facts_of(row), index=_index_of(row))

    def _row_for(self, series_key: str) -> tuple[SeriesFacts, SeriesIndexData]:
        """Facts + index for *series_key* in one DB read.

        Defaults to empty when nothing is stored. One call replaces what used
        to be two separate `get_series_facts` reads (once for facts, once for
        index) at every publish site.
        """
        row = self.database.get_series_facts(series_key)
        if row is None:
            return SeriesFacts(), SeriesIndexData()
        return _facts_of(row), _index_of(row)

    def _resolve_folder_title(self, series_key: str) -> str | None:
        """The FOLDER's own spelling of *series_key*, if it exists right now.

        A PUT's URL segment can be any case/whitespace variant that folds to
        the same key — that spelling is the request's, not the library's, and
        must not end up in a stored row that other code (a future facts
        listing, in particular) reasonably expects to read as the folder's
        real name.
        """
        folders = self._scan_folders()
        if folders is None:
            return None
        for folder in folders:
            if normalize_series_key(folder.title) == series_key:
                return folder.title
        return None

    # --- scanning ----------------------------------------------------------

    def _scan_folders(self) -> list[SeriesFolder] | None:
        """Every series folder, or None when the library root isn't readable.

        `iter_series_folders` itself treats a missing/unreadable directory
        exactly like a genuinely empty one — the right contract for a one-off
        scan (and pinned as such by its own test), but wrong for a background
        pass: a transient mount failure (the directory unmounted, a
        permission error) must abort the pass, not be read as "the whole
        library was just emptied out".
        """
        try:
            with os.scandir(self.library_path):
                pass
        except OSError:
            return None
        return iter_series_folders(self.library_path)

    # --- publishing ------------------------------------------------------

    def _publish_series(
        self,
        folder: SeriesFolder,
        *,
        facts: SeriesFacts,
        index: SeriesIndexData,
        volumes: Sequence[VolumeEntry],
    ) -> bool:
        """Write one series' sidecar. Returns True when the file changed."""
        data = dump_series_file(
            series_title=folder.title, facts=facts, index=index, volumes=volumes
        )
        return write_if_changed(folder.path / SERIES_FILE_NAME, data)

    def _publish_catalog(self, entries: Sequence[tuple[str, SeriesFacts]]) -> bool:
        return write_if_changed(self.library_path / CATALOG_FILE_NAME, dump_catalog_file(entries))

    def _published(self, changed: int) -> None:
        if changed and self._on_published is not None:
            self._on_published()

    # --- public API ------------------------------------------------------

    def regenerate_all(self) -> int:
        """Recompile every series and the catalog. Returns files written."""
        with self._pass_lock:
            folders = self._scan_folders()
            if folders is None:
                _log(f"library root unreadable, skipping pass: {self.library_path}")
                self.schedule_regeneration(delay=5.0)
                return 0
            changed = 0
            keep: set[str] = set()
            catalog_entries: list[tuple[str, SeriesFacts]] = []
            for folder in folders:
                series_key = normalize_series_key(folder.title)
                facts, index = self._row_for(series_key)
                catalog_entries.append((folder.title, facts))
                # Compiled once here and reused for the sidecar below — this
                # used to run twice per series per pass (once to build the
                # keep-set, again inside the publish call).
                volumes = compile_series_volumes(folder, database=self.database)
                for volume in volumes:
                    keep.add(volume_key_for(folder.title, volume.volume_title))
                try:
                    changed += (
                        1
                        if self._publish_series(folder, facts=facts, index=index, volumes=volumes)
                        else 0
                    )
                except MetadataWriteBusy:
                    # A DAV write owns the path right now; the next trigger
                    # (or this pass's own reschedule) picks it up.
                    _log(f"skipped busy series folder: {folder.title}")
                    self.schedule_regeneration(delay=5.0)
            # Whole-library keep-set, gathered above regardless of whether
            # each series actually needed republishing — a series that
            # published clean this pass must not lose its cache row.
            self.database.prune_series_entry_cache(keep)
            try:
                changed += 1 if self._publish_catalog(catalog_entries) else 0
            except MetadataWriteBusy:
                _log("skipped busy catalog.json")
                self.schedule_regeneration(delay=5.0)
        self._published(changed)
        return changed

    def regenerate_series(self, series_title: str) -> bool:
        """Recompile ONE series plus the catalog. Returns True when anything changed."""
        with self._pass_lock:
            changed = self._regenerate_series_locked(series_title)
        self._published(changed)
        return changed > 0

    def _regenerate_series_locked(self, series_title: str) -> int:
        """The guts of `regenerate_series`. Caller must hold `_pass_lock`.

        Split out so `apply_series_update` can run its read-merge-persist
        step and this republish inside the SAME critical section, without
        re-entering the (non-reentrant) `_pass_lock` that `regenerate_series`
        itself acquires.

        Returns the changed-file count and deliberately does NOT fire
        `on_published` — that must happen in the caller, AFTER `_pass_lock`
        is released. `on_published` is the cache-invalidation seam a caller
        can re-enter the service from (another regeneration, `stop()`), and
        `_pass_lock` is not reentrant: firing the hook while still holding it
        would wedge the calling thread permanently the moment such a hook
        shows up.
        """
        folders = self._scan_folders()
        if folders is None:
            _log(f"library root unreadable, skipping pass: {self.library_path}")
            self.schedule_regeneration(delay=5.0)
            return 0
        key = normalize_series_key(series_title)
        changed = 0
        catalog_entries: list[tuple[str, SeriesFacts]] = []
        for folder in folders:
            series_key = normalize_series_key(folder.title)
            facts, index = self._row_for(series_key)
            catalog_entries.append((folder.title, facts))
            if series_key != key:
                continue
            volumes = compile_series_volumes(folder, database=self.database)
            try:
                changed += (
                    1
                    if self._publish_series(folder, facts=facts, index=index, volumes=volumes)
                    else 0
                )
            except MetadataWriteBusy:
                _log(f"skipped busy series folder: {folder.title}")
                self.schedule_regeneration(delay=5.0)
        try:
            changed += 1 if self._publish_catalog(catalog_entries) else 0
        except MetadataWriteBusy:
            _log("skipped busy catalog.json")
            self.schedule_regeneration(delay=5.0)
        return changed

    def apply_series_update(self, series_title: str, payload: bytes, actor: str | None) -> bool:
        """Contract §6: a PUT is an update REQUEST. True = accepted.

        Accepted does not mean "changed": a payload that loses the merge is
        still a valid request, and the client must be able to retry the same
        bytes forever without side effects.

        The whole read-merge-persist-republish sequence runs under
        `_pass_lock`. Without that, two concurrent PUTs for the same series —
        the same owner's several devices, in particular, syncing metadata at
        the same time — interleave as read/read/write/write: whichever write
        lands last wins outright, even when its own facts stamp is OLDER,
        because it was merged against a stale snapshot rather than the
        other's just-committed write. That defeats `merge.py`'s entire
        newest-stamp-wins contract while telling both callers they succeeded.
        """
        series_key = normalize_series_key(series_title)
        if not series_key:
            return False
        update = parse_series_update(payload)
        if update is None:
            return False

        changed = 0
        with self._pass_lock:
            stored = self._stored(series_key)
            result = merge_series_update(stored, update)
            if stored is None or result.changed:
                resolved_title = self._resolve_folder_title(series_key) or series_title
                self.database.put_series_facts(
                    SeriesFactsRow(
                        series_key=series_key,
                        series_title=resolved_title,
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
            try:
                changed = self._regenerate_series_locked(series_title)
            except Exception as error:  # noqa: BLE001 - facts are already durable; the
                # caller must never see an accepted, persisted update reported
                # as a failure just because publishing itself blew up.
                _log(f"republish failed after an accepted update: {error}")
                self.schedule_regeneration(delay=5.0)
        # Outside `_pass_lock` (matching `regenerate_all`/`regenerate_series`)
        # AND outside the `try/except` above: a hook that re-enters the
        # service (another regeneration, `stop()`) must not deadlock on the
        # non-reentrant lock, and a hook's own exception must propagate as
        # the hook's failure, not get logged and retried as a republish one.
        self._published(changed)
        return True

    def schedule_regeneration(self, delay: float | None = None) -> None:
        """Debounced full pass: resets on each call, fires after the quiet period."""
        with self._timer_lock:
            if self._stopped:
                return
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
            if self._stopped:
                return
        try:
            self.regenerate_all()
        except Exception as error:  # noqa: BLE001 - a background pass must not die
            _log(f"regeneration failed: {error}")

    def stop(self) -> None:
        """Cancel a pending pass and wait for any pass already in flight.

        Safe to call more than once, and safe to race `schedule_regeneration`
        from another thread — both mutate `_timer`/`_stopped` under
        `_timer_lock`.

        Two things can otherwise outlive a naive `stop()`: a timer whose wait
        already elapsed, so `cancel()` cannot stop it (`_fire` re-checks
        `_stopped` itself, under the same lock, to close that window); and a
        pass that had already started — via `_fire`, or a concurrent direct
        `regenerate_all`/`regenerate_series`/`apply_series_update` call —
        which still holds `_pass_lock` until it finishes. Joining the timer
        thread handles the first case; acquiring and releasing `_pass_lock`
        afterwards handles the second, which is why `stop()` does not return
        until nothing is left running.
        """
        with self._timer_lock:
            self._stopped = True
            timer = self._timer
            self._timer = None
        if timer is not None:
            timer.cancel()
            timer.join()
        with self._pass_lock:
            pass


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

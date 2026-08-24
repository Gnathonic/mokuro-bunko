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
        self._stopped = False

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
        try:
            self.regenerate_all()
        except Exception as error:  # noqa: BLE001 - a background pass must not die
            _log(f"regeneration failed: {error}")

    def stop(self) -> None:
        """Cancel a pending pass (shutdown). Safe to call more than once."""
        with self._timer_lock:
            self._stopped = True
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

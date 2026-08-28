"""Compiling, publishing and updating the reader's metadata files.

This is the only stateful piece: it owns the debounce timer and the guarantee
that two regeneration passes never overlap. Everything it calls is pure or
filesystem-local.

An existing `series_facts` row outlives its folder disappearing — nothing
here ever deletes one — so a folder RENAMED back to a spelling an old row
still matches, or a temporarily-removed folder simply reappearing, finds its
facts waiting again with no extra step (a rename to a genuinely NEW name
does not carry the old row across; the client republishes under the new
name on its next fact edit).

A row may only ever be CREATED or UPDATED through `apply_series_update` (a
client's PUT) for a title that resolves, via the identity fold used
throughout this module (`normalize_volume_title_key` — NFC-normalize, then
`normalize_series_key`'s whitespace-collapse-and-lowercase), to a folder
that exists RIGHT NOW. Task 11 review round 3 (the "F9"/N2-residual fix):
this module used to accept a PUT for ANY title, real folder or not, so a
client could pre-provision facts ahead of an upload — but the identity fold
it used to decide "is this the same series as this real folder" was
`normalize_series_key` alone, WITHOUT the NFC step, so an NFD-spelled PUT
title and its NFC-spelled real folder (the common case for a name that
round-tripped through a filesystem) were treated as two DIFFERENT series.
The PUT still returned 200 "accepted", but the accepted row's identity
matched no real folder, and the real folder's own sidecar was never
republished with it — an update that looked successful and silently went
nowhere. A title that resolves to no existing folder — under the aligned
fold — is now refused (400) instead of silently parked.
"""

from __future__ import annotations

import os
import sys
import threading
import time
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
from mokuro_bunko.metadata.reader_compat import normalize_volume_title_key
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
        max_debounce_seconds: float = 60.0,
    ) -> None:
        self.library_path = Path(library_path)
        self.database = database
        self.debounce_seconds = debounce_seconds
        # A debounce that resets on every call starves under a sustained
        # stream of events (a client uploading volume after volume never
        # leaves a quiet window). Resets extend a pending timer only up to
        # this many seconds past its first scheduling; then it fires anyway.
        self.max_debounce_seconds = max_debounce_seconds
        self._on_published = on_published
        self._pass_lock = threading.Lock()
        self._timer_lock = threading.Lock()
        self._timer: threading.Timer | None = None
        self._deadline: float | None = None
        self._series_timers: dict[str, threading.Timer] = {}
        self._series_titles: dict[str, str] = {}
        self._series_deadlines: dict[str, float] = {}
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

        A PUT's URL segment can be any case/whitespace/Unicode-composition
        variant that folds to the same `normalize_volume_title_key` key —
        that spelling is the request's, not the library's, and must not end
        up in a stored row that other code (a future facts listing, in
        particular) reasonably expects to read as the folder's real name.

        `None` also when the library root can't be scanned right now (a
        transient mount failure) — the caller cannot verify a folder exists
        either way, and per the invariant this class enforces (module
        docstring), "cannot verify" and "does not exist" get the same
        answer: no row is created or updated on a guess.
        """
        folders = self._scan_folders()
        if folders is None:
            return None
        for folder in folders:
            if normalize_volume_title_key(folder.title) == series_key:
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
        """Write one series' sidecar. Returns True when the file changed.

        Final review F3: `folder` came from an `iter_series_folders` scan
        that ran earlier in this pass, and an ordinary DELETE of the whole
        series folder can race the (up to 10s-debounced) publish that
        follows -- routine under a bulk delete. `atomic_write_bytes`'s
        `parents=True` mkdir would otherwise happily resurrect the folder
        containing nothing but a freshly-written `series.json`: no `.cbz`,
        so the next scan skips it forever, and nothing ever prunes the
        ghost. Checked right here, immediately before the write, rather
        than back when `folder` was scanned: there is nothing to publish
        into a folder that is gone RIGHT NOW, so skip it (unlike
        `_publish_catalog` below, whose parent is the library root, which
        this class already treats as required to exist for a pass to run
        at all -- see `_scan_folders`).
        """
        if not folder.path.is_dir():
            return False
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
        """Recompile every series and the catalog. Returns files written.

        F5 (final review): a no-op after `stop()`. `schedule_regeneration`
        was already gated on `_stopped`, but a DIRECT call to any of the
        three public entry points here was not -- a caller (the filesystem
        watcher's callback, a request already in flight) racing `stop()`
        could still run a whole pass and fire `on_published` after `stop()`
        had returned, arming a `PropfindCacheMiddleware` refresh nothing was
        left to cancel. This narrows, rather than fully closes, that
        window -- `stop()` does not hold `_pass_lock` while flipping
        `_stopped`, so a caller that already read `_stopped` as `False`
        can still slip through; the matching `PropfindCacheMiddleware`
        stopped-gate is the belt-and-suspenders half of this fix.
        """
        if self._stopped:
            return 0
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
                series_key = normalize_volume_title_key(folder.title)
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
                except OSError as error:
                    # Defense in depth (Task 11 review round 1, F3): the
                    # auth layer now refuses MKCOL on a compiled path for
                    # every role, but that closes the gate going forward —
                    # it does not undo a directory already squatting where
                    # a sidecar belongs (planted before this hardening
                    # shipped, or by anything outside the DAV auth path).
                    # Unguarded, `atomic_write_bytes`'s `os.replace` raises
                    # `IsADirectoryError` here, which used to escape this
                    # loop and abort the WHOLE pass — every series after the
                    # poisoned one silently stopped publishing. Skip only
                    # this folder; the pass continues and the catalog still
                    # updates.
                    _log(f"skipped unwritable series folder: {folder.title}: {error}")
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
            except OSError as error:
                # Same defense in depth as the per-folder loop above (Task
                # 11 review round 2, N4): the round-1 fix guarded only the
                # per-series publish, leaving a directory squatting at the
                # ROOT `catalog.json` free to raise `IsADirectoryError` and
                # abort the pass after every series sidecar had already
                # published successfully. Skip just the catalog write; the
                # series sidecars this pass already wrote stand.
                _log(f"skipped unwritable catalog.json: {error}")
                self.schedule_regeneration(delay=5.0)
        self._published(changed)
        return changed

    def regenerate_series(self, series_title: str) -> bool:
        """Recompile ONE series plus the catalog. Returns True when anything changed.

        F5 (final review): a no-op after `stop()`, same reasoning as
        `regenerate_all`.
        """
        if self._stopped:
            return False
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
        key = normalize_volume_title_key(series_title)
        changed = 0
        catalog_entries: list[tuple[str, SeriesFacts]] = []
        for folder in folders:
            series_key = normalize_volume_title_key(folder.title)
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
            except OSError as error:
                # F4 (final review): this helper used to catch only
                # MetadataWriteBusy, unlike regenerate_all's identical loop
                # above -- a directory squatting at THIS series' sidecar
                # path raised IsADirectoryError straight out of
                # regenerate_series/apply_series_update. Same hardening,
                # same reasoning: skip only this folder.
                _log(f"skipped unwritable series folder: {folder.title}: {error}")
                self.schedule_regeneration(delay=5.0)
        try:
            changed += 1 if self._publish_catalog(catalog_entries) else 0
        except MetadataWriteBusy:
            _log("skipped busy catalog.json")
            self.schedule_regeneration(delay=5.0)
        except OSError as error:
            _log(f"skipped unwritable catalog.json: {error}")
            self.schedule_regeneration(delay=5.0)
        return changed

    def apply_series_update(self, series_title: str, payload: bytes, actor: str | None) -> bool:
        """Contract §6: a PUT is an update REQUEST. True = accepted.

        Accepted does not mean "changed": a payload that loses the merge is
        still a valid request, and the client must be able to retry the same
        bytes forever without side effects.

        Refused (`False`, no side effects at all — no row created or
        updated) when the payload is unparseable, OR when *series_title*
        does not resolve, via `normalize_volume_title_key`, to a folder
        that exists right now (module docstring; Task 11 review round 3):
        a `series_facts` row may only be created or updated for a real
        folder's own identity, never for a title no folder currently
        shares — including while the library root itself can't be scanned,
        since this method has no way to tell "doesn't exist" from "can't
        check right now" apart in that case (`_resolve_folder_title`).

        The whole resolve-read-merge-persist-republish sequence runs under
        `_pass_lock`. Without that, two concurrent PUTs for the same series —
        the same owner's several devices, in particular, syncing metadata at
        the same time — interleave as read/read/write/write: whichever write
        lands last wins outright, even when its own facts stamp is OLDER,
        because it was merged against a stale snapshot rather than the
        other's just-committed write. That defeats `merge.py`'s entire
        newest-stamp-wins contract while telling both callers they succeeded.

        F5 (final review): also a no-op after `stop()`, same reasoning as
        `regenerate_all` -- a PUT that reaches this method after shutdown
        has begun must not persist a row or schedule further work.
        """
        if self._stopped:
            return False
        series_key = normalize_volume_title_key(series_title)
        if not series_key:
            return False
        update = parse_series_update(payload)
        if update is None:
            return False

        changed = 0
        with self._pass_lock:
            resolved_title = self._resolve_folder_title(series_key)
            if resolved_title is None:
                return False
            stored = self._stored(series_key)
            result = merge_series_update(stored, update)
            if stored is None or result.changed:
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
        """Debounced full pass: resets on each call, fires after the quiet period.

        Resets are capped: the timer never drifts more than
        `max_debounce_seconds` past the first scheduling since the last fire.
        """
        with self._timer_lock:
            if self._stopped:
                return
            now = time.monotonic()
            if self._timer is None or self._deadline is None:
                self._deadline = now + self.max_debounce_seconds
            else:
                self._timer.cancel()
            base = self.debounce_seconds if delay is None else delay
            timer = threading.Timer(
                min(base, max(0.0, self._deadline - now)), self._fire
            )
            timer.daemon = True
            self._timer = timer
            timer.start()

    def schedule_series_regeneration(self, series_title: str, delay: float | None = None) -> None:
        """Debounced single-series recompile — the volume-upload trigger.

        A client PUTting volume files (`.cbz`, `.mokuro`, covers) never sends
        a `series.json` for the DAV layer to intercept, so this is how those
        uploads reach the compiler: each write reschedules its own series,
        the burst coalesces, and `regenerate_series` republishes that series
        plus the catalog. Timers are per series — one series' stream never
        defers another's — and resets are capped like the full pass above.
        """
        key = normalize_volume_title_key(series_title)
        with self._timer_lock:
            if self._stopped:
                return
            now = time.monotonic()
            pending = self._series_timers.get(key)
            if pending is None or key not in self._series_deadlines:
                self._series_deadlines[key] = now + self.max_debounce_seconds
            else:
                pending.cancel()
            self._series_titles[key] = series_title
            base = self.debounce_seconds if delay is None else delay
            timer = threading.Timer(
                min(base, max(0.0, self._series_deadlines[key] - now)),
                self._fire_series,
                args=(key,),
            )
            timer.daemon = True
            self._series_timers[key] = timer
            timer.start()

    def _fire(self) -> None:
        with self._timer_lock:
            self._timer = None
            self._deadline = None
            if self._stopped:
                return
        try:
            self.regenerate_all()
        except Exception as error:  # noqa: BLE001 - a background pass must not die
            _log(f"regeneration failed: {error}")

    def _fire_series(self, key: str) -> None:
        with self._timer_lock:
            self._series_timers.pop(key, None)
            self._series_deadlines.pop(key, None)
            title = self._series_titles.pop(key, None)
            if self._stopped or title is None:
                return
        try:
            self.regenerate_series(title)
        except Exception as error:  # noqa: BLE001 - a background pass must not die
            _log(f"series regeneration failed: {title}: {error}")

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
            timers = [self._timer] if self._timer is not None else []
            timers.extend(self._series_timers.values())
            self._timer = None
            self._deadline = None
            self._series_timers.clear()
            self._series_titles.clear()
            self._series_deadlines.clear()
        for timer in timers:
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

"""The service: compile, publish, apply updates, stay idempotent."""

from __future__ import annotations

import json
import os
import shutil
import threading
import time
import unicodedata
import zipfile
from pathlib import Path

import pytest

from mokuro_bunko.database import Database, SeriesFactsRow
from mokuro_bunko.metadata import service as metadata_service
from mokuro_bunko.metadata.compiler import volume_key_for
from mokuro_bunko.metadata.reader_compat import normalize_volume_title_key
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

        sidecar_stat = (library / "Dr Stone" / "Volume 01.mokuro").stat()
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
                "mokuro_size": sidecar_stat.st_size,
                "mokuro_modified": int(sidecar_stat.st_mtime),
            }
        ]

        catalog = json.loads((library / "catalog.json").read_text("utf-8"))
        assert [entry["series_title"] for entry in catalog["series"]] == ["Aria", "Dr Stone"]
        assert catalog["series"][0]["updated_at"] == "1970-01-01T00:00:00.000Z"

    def test_a_directory_squatting_at_series_json_does_not_abort_the_whole_pass(
        self, service: MetadataService, library: Path
    ) -> None:
        """Task 11 review round 1 (F3), defense in depth: the auth layer now
        refuses MKCOL on a compiled path for every role, but that only
        closes the gate going forward — it does not undo a directory
        already squatting where a sidecar belongs (planted before this
        hardening shipped, or by anything outside the DAV auth path).
        Unguarded, `IsADirectoryError` used to escape the per-folder loop
        and abort the whole pass, so every series scanned AFTER the
        poisoned one silently stopped publishing. 'Dr Stone' sorts before
        'Zzz Series' so this actually exercises that ordering.
        """
        write_volume(library, "Dr Stone", "Volume 01")
        write_volume(library, "Zzz Series", "v1", sidecar=False)
        (library / "Dr Stone" / "series.json").mkdir()

        changed = service.regenerate_all()

        # The poisoned folder didn't crash the pass and wasn't clobbered.
        assert (library / "Dr Stone" / "series.json").is_dir()
        # The series scanned AFTER it still published.
        zzz = json.loads((library / "Zzz Series" / "series.json").read_text("utf-8"))
        assert zzz["series_title"] == "Zzz Series"
        # And the catalog still covers both, including the poisoned one.
        catalog = json.loads((library / "catalog.json").read_text("utf-8"))
        assert [entry["series_title"] for entry in catalog["series"]] == [
            "Dr Stone",
            "Zzz Series",
        ]
        assert changed == 2  # Zzz Series sidecar + catalog; Dr Stone's write failed

    def test_a_directory_squatting_at_catalog_json_does_not_abort_the_pass(
        self, service: MetadataService, library: Path
    ) -> None:
        """Task 11 review round 2 (N4): the round-1 OSError hardening only
        guarded the per-folder sidecar publish, leaving a directory
        squatting at the ROOT `catalog.json` free to raise
        `IsADirectoryError` and abort the pass AFTER every series sidecar
        had already published successfully."""
        write_volume(library, "Dr Stone", "Volume 01")
        (library / "catalog.json").mkdir()

        changed = service.regenerate_all()

        # The poisoned catalog path didn't crash the pass and wasn't clobbered.
        assert (library / "catalog.json").is_dir()
        # The series sidecar, published before the catalog write, still landed.
        sidecar = json.loads((library / "Dr Stone" / "series.json").read_text("utf-8"))
        assert sidecar["series_title"] == "Dr Stone"
        assert changed == 1  # Dr Stone sidecar only; the catalog write failed

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

    def test_prune_keeps_cache_rows_for_every_still_present_series(
        self, service: MetadataService, library: Path
    ) -> None:
        """Whole-library keep-set (contract carry-forward #4): a series that
        published clean this pass (nothing about it changed) must not lose
        its cache row just because a SIBLING series lost a volume."""
        write_volume(library, "Dr Stone", "Volume 01")
        write_volume(library, "Dr Stone", "Volume 02")
        write_volume(library, "Aria", "v1")
        service.regenerate_all()

        def cache_keys() -> set[str]:
            with service.database._connection() as conn:
                rows = conn.execute("SELECT volume_key FROM series_entry_cache").fetchall()
                return {str(row["volume_key"]) for row in rows}

        assert cache_keys() == {
            volume_key_for("Dr Stone", "Volume 01"),
            volume_key_for("Dr Stone", "Volume 02"),
            volume_key_for("Aria", "v1"),
        }

        (library / "Dr Stone" / "Volume 02.cbz").unlink()
        (library / "Dr Stone" / "Volume 02.mokuro").unlink()
        service.regenerate_all()

        assert cache_keys() == {
            volume_key_for("Dr Stone", "Volume 01"),
            volume_key_for("Aria", "v1"),
        }

    def test_publish_series_does_not_resurrect_a_folder_removed_before_the_write(
        self, service: MetadataService, library: Path
    ) -> None:
        """F3 (final whole-branch review): `_publish_series` must not
        resurrect a series folder that vanished between the scan and the
        write -- there is nothing left to publish into."""
        from mokuro_bunko.metadata.compiler import SeriesFolder
        from mokuro_bunko.metadata.schema import SeriesFacts, SeriesIndexData

        folder = SeriesFolder(title="Dr Stone", path=library / "Dr Stone")
        assert not folder.path.exists()

        changed = service._publish_series(
            folder, facts=SeriesFacts(), index=SeriesIndexData(), volumes=[]
        )

        assert changed is False
        assert not folder.path.exists()

    def test_a_series_folder_deleted_mid_pass_is_not_resurrected(
        self, service: MetadataService, library: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """F3, reproduced through the real `regenerate_all` pass: a DELETE of
        the whole series folder racing an in-flight regeneration -- exactly
        what a bulk delete during the debounce window produces. Before the
        fix, the folder came back holding only `series.json`, was never
        republished again (no `.cbz` -> skipped by the next scan) and
        nothing pruned it."""
        write_volume(library, "Dr Stone", "Volume 01")
        write_volume(library, "Aria", "v1")

        real_compile = metadata_service.compile_series_volumes

        def compile_then_delete_dr_stone(folder: object, *, database: Database) -> object:
            volumes = real_compile(folder, database=database)  # type: ignore[arg-type]
            if getattr(folder, "title", None) == "Dr Stone":
                shutil.rmtree(folder.path)  # type: ignore[union-attr]
            return volumes

        monkeypatch.setattr(
            metadata_service, "compile_series_volumes", compile_then_delete_dr_stone
        )

        service.regenerate_all()

        assert not (library / "Dr Stone").exists()  # not resurrected
        catalog = json.loads((library / "catalog.json").read_text("utf-8"))
        assert [entry["series_title"] for entry in catalog["series"]] == [
            "Aria",
            "Dr Stone",
        ]

    def test_a_busy_skip_is_retried_by_the_next_scheduled_pass(
        self, service: MetadataService, library: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        real_schedule = MetadataService.schedule_regeneration

        def fast_schedule(self: MetadataService, delay: float | None = None) -> None:
            real_schedule(self, 0.05 if delay is not None else delay)

        monkeypatch.setattr(MetadataService, "schedule_regeneration", fast_schedule)

        write_volume(library, "Dr Stone", "Volume 01")
        assert _PATH_WRITE_LOCKS.acquire(library / "Dr Stone")
        try:
            service.regenerate_all()
        finally:
            _PATH_WRITE_LOCKS.release(library / "Dr Stone")
        assert not (library / "Dr Stone" / "series.json").exists()

        timer = service._timer
        assert timer is not None
        timer.join(timeout=10.0)
        assert (library / "Dr Stone" / "series.json").exists()
        service.stop()


class TestUnreadableLibrary:
    def test_a_removed_library_root_aborts_the_pass(
        self, library: Path, tmp_path: Path
    ) -> None:
        calls: list[int] = []
        service = MetadataService(
            library, Database(tmp_path / "test.db"), on_published=lambda: calls.append(1)
        )
        write_volume(library, "Dr Stone", "Volume 01")
        service.regenerate_all()
        assert calls == [1]

        def cache_keys() -> set[str]:
            with service.database._connection() as conn:
                rows = conn.execute("SELECT volume_key FROM series_entry_cache").fetchall()
                return {str(row["volume_key"]) for row in rows}

        cached_before = cache_keys()
        assert cached_before

        shutil.rmtree(library)
        assert not library.exists()

        assert service.regenerate_all() == 0
        assert not library.exists()  # not recreated
        assert calls == [1]  # on_published did not fire for the aborted pass
        assert cache_keys() == cached_before  # nothing pruned
        service.stop()


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

    def test_reapplying_the_same_update_leaves_the_facts_row_untouched(
        self, service: MetadataService, library: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """MergeResult contract: nothing is persisted when nothing changed.

        Reads the actual row (not just file mtimes) and, since SQLite's
        `datetime('now')` bookkeeping stamp has only second resolution and
        this whole test runs in well under a second, also spies on the write
        call directly so a reintroduced unconditional write is still caught
        even when it would not move `updated_at` far enough to notice.
        """
        write_volume(library, "Dr Stone", "Volume 01")
        assert service.apply_series_update("Dr Stone", series_update(), "alice")
        before = service.database.get_series_facts("dr stone")
        assert before is not None

        write_calls: list[SeriesFactsRow] = []
        real_put = Database.put_series_facts

        def spying_put(self: Database, row: SeriesFactsRow) -> None:
            write_calls.append(row)
            real_put(self, row)

        monkeypatch.setattr(Database, "put_series_facts", spying_put)

        assert service.apply_series_update("Dr Stone", series_update(), "alice") is True

        assert write_calls == []
        after = service.database.get_series_facts("dr stone")
        assert after == before

    def test_a_case_and_whitespace_variant_title_resolves_to_the_real_folder(
        self, service: MetadataService, library: Path
    ) -> None:
        write_volume(library, "Dr Stone", "Volume 01")
        assert service.apply_series_update("dr  STONE", series_update(tag="HD Scan"), "alice")
        sidecar = json.loads((library / "Dr Stone" / "series.json").read_text("utf-8"))
        assert sidecar["tag"] == "HD Scan"

    def test_stored_series_title_uses_the_folders_spelling_not_the_puts(
        self, service: MetadataService, library: Path
    ) -> None:
        write_volume(library, "Dr Stone", "Volume 01")
        assert service.apply_series_update("dr  STONE", series_update(), "alice")
        row = service.database.get_series_facts("dr stone")
        assert row is not None
        assert row["series_title"] == "Dr Stone"

    def test_a_blank_title_is_rejected(self, service: MetadataService, library: Path) -> None:
        assert service.apply_series_update("   ", series_update(), "alice") is False
        assert service.database.list_series_facts() == []

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

    def test_an_update_for_an_unknown_folder_is_refused_and_never_stored(
        self, service: MetadataService, library: Path
    ) -> None:
        """Task 11 review round 3 (F9 / N2 residual, controller invariant):
        a `series_facts` row may only be created or updated for a title
        that resolves to a REAL, currently-existing folder. Pre-provisioning
        facts ahead of an upload (this test's old name and behavior, back
        when the module docstring called it deliberate) is no longer
        accepted — see the module docstring for the full account."""
        assert service.apply_series_update("Dr Stone", series_update(), "alice") is False
        assert service.database.get_series_facts("dr stone") is None
        assert service.database.list_series_facts() == []
        # No folder ever existed for it, so a later scan finds nothing either.
        service.regenerate_all()
        assert not (library / "Dr Stone").exists()

    def test_an_nfd_spelled_put_resolves_onto_the_real_nfc_folder(
        self, service: MetadataService, library: Path
    ) -> None:
        """The reviewer's exact probe (Task 11 review round 3): a folder
        named with NFC-composed Unicode ('Pokémon') and a PUT whose URL
        segment happens to be NFD-decomposed (the same text to a human and
        to the reader, byte-different on disk) must resolve onto the SAME
        real folder — not create a second, orphaned identity no folder
        shares, and not silently vanish without republishing the real
        folder's sidecar."""
        nfc_title = unicodedata.normalize("NFC", "Pokémon")
        nfd_title = unicodedata.normalize("NFD", "Pokémon")
        assert nfc_title != nfd_title  # sanity: genuinely different byte sequences

        write_volume(library, nfc_title, "Volume 01")

        assert service.apply_series_update(nfd_title, series_update(), "alice") is True

        # Stored under the FOLDER's own (NFC) identity, not the NFD spelling.
        key = normalize_volume_title_key(nfc_title)
        row = service.database.get_series_facts(key)
        assert row is not None
        assert row["series_title"] == nfc_title
        # And no separate orphan row exists under the raw NFD key either.
        nfd_key = normalize_volume_title_key(nfd_title)
        assert nfd_key == key  # sanity: the aligned fold treats them as one key

        # And it was actually republished onto the real folder's sidecar —
        # not silently accepted while nothing on disk changed.
        sidecar = json.loads((library / nfc_title / "series.json").read_text("utf-8"))
        assert sidecar["external_ids"] == {"anilist": 98416}

        # The auth-layer ownership grant (a separate identity: `volume_uploads`,
        # not `series_facts`) is unaffected by any of this, and still
        # advertises the folder's own NFC spelling — the full "ownedSeries
        # unchanged" leg of the invariant, checked end to end rather than
        # assumed from the fold matching in isolation.
        service.database.record_volume_upload(f"{nfc_title}/Volume 01.cbz", "alice")
        assert service.database.can_user_edit_series("alice", nfc_title) is True
        assert service.database.can_user_edit_series("alice", nfd_title) is True
        assert service.database.list_series_owned_by("alice") == [nfc_title]

    def test_a_squatting_directory_at_the_target_series_does_not_abort_regenerate_series(
        self, service: MetadataService, library: Path
    ) -> None:
        """F4 (final review): `_regenerate_series_locked` used to catch only
        `MetadataWriteBusy`, asymmetric with `regenerate_all`'s hardening
        for the identical `IsADirectoryError` condition. `regenerate_series`
        has no production caller today, but `apply_series_update` shares
        this exact locked helper, so an update PUT for a series with a
        squatted sidecar used to raise straight out of this method after
        the facts row was already durably persisted."""
        write_volume(library, "Dr Stone", "Volume 01")
        (library / "Dr Stone" / "series.json").mkdir()

        changed = service.regenerate_series("Dr Stone")  # must not raise

        # The squatted series write itself failed (skipped, not clobbered),
        # but the catalog is written for the first time in this same call
        # and does count as a change -- `changed` is about the WHOLE call,
        # not just the one squatted folder.
        assert changed is True
        assert (library / "Dr Stone" / "series.json").is_dir()  # not clobbered
        catalog = json.loads((library / "catalog.json").read_text("utf-8"))
        assert catalog["series"][0]["series_title"] == "Dr Stone"
        service.stop()

    def test_a_republish_failure_after_a_persisted_accept_still_returns_true(
        self, service: MetadataService, library: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """F8: the facts are already durably stored by the time the republish
        step runs, so a caller must never see that accepted update reported
        as a failure just because publishing itself blew up — the exception
        is caught, logged, a retry pass is scheduled, and `True` still comes
        back (this was M23 in the re-review: implemented but untested)."""
        write_volume(library, "Dr Stone", "Volume 01")

        def boom(self: MetadataService, series_title: str) -> int:
            raise RuntimeError("disk gone")

        monkeypatch.setattr(MetadataService, "_regenerate_series_locked", boom)

        assert service.apply_series_update("Dr Stone", series_update(), "alice") is True

        row = service.database.get_series_facts("dr stone")
        assert row is not None
        assert row["external_ids"] == {"anilist": 98416}
        assert not (library / "Dr Stone" / "series.json").exists()  # publish never happened

        timer = service._timer
        assert timer is not None  # a retry pass was scheduled
        service.stop()

    def test_concurrent_puts_do_not_lose_the_newer_facts_or_an_offset(
        self, library: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Reproduces the reviewer's F1 finding: without `_pass_lock` held
        across the WHOLE read-merge-persist-republish sequence, two threads
        PUTting concurrently interleave as read/read/write/write, and
        whichever write lands last wins outright — even carrying an OLDER
        facts stamp and dropping the other client's offset — while both
        callers are told the update was accepted.

        An artificial delay inside `_stored` (the read step), held under
        `_pass_lock` when the fix is in place, forces genuine contention: an
        unprotected implementation would let the second thread's read (and
        therefore its write) race in underneath the first thread's sleep.
        The two payloads target different volumes, so the correct outcome —
        regardless of which thread's PUT actually lands first, since the
        merge itself is order-independent — carries BOTH offsets.
        """
        service = MetadataService(library, Database(tmp_path / "test.db"))
        write_volume(library, "Dr Stone", "Volume 01")
        write_volume(library, "Dr Stone", "Volume 02")

        real_stored = MetadataService._stored

        def slow_stored(self: MetadataService, series_key: str) -> object:
            result = real_stored(self, series_key)
            time.sleep(0.05)
            return result

        monkeypatch.setattr(MetadataService, "_stored", slow_stored)

        start = threading.Barrier(2)
        results: dict[str, bool] = {}

        def call(name: str, payload: bytes, actor: str) -> None:
            start.wait(timeout=5.0)
            results[name] = service.apply_series_update("Dr Stone", payload, actor)

        older = threading.Thread(
            target=call,
            args=(
                "bob",
                series_update(
                    external_ids={"anilist": 111},
                    updated_at="2026-08-10T00:00:00.000Z",
                    volumes=[{"volume_uuid": "uuid-Volume 01", "offset": -22}],
                ),
                "bob",
            ),
        )
        newer = threading.Thread(
            target=call,
            args=(
                "alice",
                series_update(
                    external_ids={"anilist": 999},
                    updated_at="2026-08-20T00:00:00.000Z",
                    volumes=[{"volume_uuid": "uuid-Volume 02", "offset": -33}],
                ),
                "alice",
            ),
        )
        older.start()
        newer.start()
        older.join(timeout=5.0)
        newer.join(timeout=5.0)

        assert results == {"bob": True, "alice": True}
        row = service.database.get_series_facts("dr stone")
        assert row is not None
        assert row["external_ids"] == {"anilist": 999}
        assert row["facts_updated_at"] == "2026-08-20T00:00:00.000Z"
        assert row["volume_offsets"] == {
            "uuid-Volume 01": -22,
            "uuid-Volume 02": -33,
        }


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

    def test_a_burst_of_schedule_calls_triggers_exactly_one_pass(
        self, library: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        service = MetadataService(
            library, Database(tmp_path / "test.db"), debounce_seconds=0.05
        )
        write_volume(library, "Dr Stone", "Volume 01")

        calls: list[int] = []
        real_regenerate_all = MetadataService.regenerate_all

        def counting_regenerate_all(self: MetadataService) -> int:
            calls.append(1)
            return real_regenerate_all(self)

        monkeypatch.setattr(MetadataService, "regenerate_all", counting_regenerate_all)

        for _ in range(5):
            service.schedule_regeneration()
        timer = service._timer
        assert timer is not None
        timer.join(timeout=5.0)

        assert calls == [1]
        service.stop()

    def test_series_schedule_regenerates_only_that_series(
        self, library: Path, tmp_path: Path
    ) -> None:
        service = MetadataService(
            library, Database(tmp_path / "test.db"), debounce_seconds=0.05
        )
        write_volume(library, "Dr Stone", "Volume 01")
        write_volume(library, "Frieren", "Volume 01")
        service.schedule_series_regeneration("Dr Stone")
        key = normalize_volume_title_key("Dr Stone")
        timer = service._series_timers[key]
        timer.join(timeout=5.0)
        assert (library / "Dr Stone" / "series.json").exists()
        assert not (library / "Frieren" / "series.json").exists()
        assert (library / "catalog.json").exists()
        service.stop()

    def test_a_burst_of_series_schedules_triggers_exactly_one_regen(
        self, library: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        service = MetadataService(
            library, Database(tmp_path / "test.db"), debounce_seconds=0.05
        )
        write_volume(library, "Dr Stone", "Volume 01")

        calls: list[str] = []
        real_regenerate_series = MetadataService.regenerate_series

        def counting(self: MetadataService, series_title: str) -> bool:
            calls.append(series_title)
            return real_regenerate_series(self, series_title)

        monkeypatch.setattr(MetadataService, "regenerate_series", counting)

        for _ in range(5):
            service.schedule_series_regeneration("Dr Stone")
        timer = service._series_timers[normalize_volume_title_key("Dr Stone")]
        timer.join(timeout=5.0)

        assert calls == ["Dr Stone"]
        service.stop()

    def test_series_debounce_resets_are_capped(
        self, library: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Continuous rescheduling (a client uploading volume after volume)
        must not starve the regen forever: the cap forces a fire."""
        service = MetadataService(
            library,
            Database(tmp_path / "test.db"),
            debounce_seconds=0.05,
            max_debounce_seconds=0.2,
        )
        fired = threading.Event()
        monkeypatch.setattr(
            MetadataService, "regenerate_series", lambda self, title: fired.set() or True
        )
        deadline = time.monotonic() + 2.0
        while not fired.is_set() and time.monotonic() < deadline:
            service.schedule_series_regeneration("Dr Stone")
            time.sleep(0.02)
        assert fired.is_set(), "capped debounce never fired under continuous resets"
        service.stop()

    def test_full_pass_debounce_resets_are_capped(
        self, library: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        service = MetadataService(
            library,
            Database(tmp_path / "test.db"),
            debounce_seconds=0.05,
            max_debounce_seconds=0.2,
        )
        fired = threading.Event()
        monkeypatch.setattr(
            MetadataService, "regenerate_all", lambda self: fired.set() or 0
        )
        deadline = time.monotonic() + 2.0
        while not fired.is_set() and time.monotonic() < deadline:
            service.schedule_regeneration()
            time.sleep(0.02)
        assert fired.is_set(), "capped debounce never fired under continuous resets"
        service.stop()

    def test_stop_cancels_pending_series_timers(
        self, library: Path, tmp_path: Path
    ) -> None:
        service = MetadataService(library, Database(tmp_path / "test.db"), debounce_seconds=5.0)
        write_volume(library, "Dr Stone", "Volume 01")
        service.schedule_series_regeneration("Dr Stone")
        service.stop()
        assert service._series_timers == {}
        assert not (library / "Dr Stone" / "series.json").exists()

    def test_series_schedule_after_stop_does_not_arm_a_timer(
        self, library: Path, tmp_path: Path
    ) -> None:
        service = MetadataService(library, Database(tmp_path / "test.db"), debounce_seconds=5.0)
        service.stop()
        service.schedule_series_regeneration("Dr Stone")
        assert service._series_timers == {}

    def test_stop_cancels_a_pending_pass(self, library: Path, tmp_path: Path) -> None:
        service = MetadataService(library, Database(tmp_path / "test.db"), debounce_seconds=5.0)
        write_volume(library, "Dr Stone", "Volume 01")
        service.schedule_regeneration()
        service.stop()
        assert service._timer is None
        assert not (library / "Dr Stone" / "series.json").exists()

    def test_schedule_after_stop_does_not_revive_the_timer(
        self, library: Path, tmp_path: Path
    ) -> None:
        service = MetadataService(library, Database(tmp_path / "test.db"), debounce_seconds=5.0)
        service.stop()
        service.schedule_regeneration()
        assert service._timer is None

    def test_a_just_fired_timer_does_nothing_after_stop(
        self, library: Path, tmp_path: Path
    ) -> None:
        """A timer whose wait already elapsed (`cancel()` cannot win that
        race) must still no-op: `_fire` re-checks `_stopped` itself."""
        service = MetadataService(library, Database(tmp_path / "test.db"), debounce_seconds=5.0)
        write_volume(library, "Dr Stone", "Volume 01")
        service.schedule_regeneration()
        service.stop()
        service._fire()  # simulates the timer thread having already started
        assert not (library / "Dr Stone" / "series.json").exists()

    def test_stop_blocks_until_a_running_pass_finishes(
        self, library: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        service = MetadataService(
            library, Database(tmp_path / "test.db"), debounce_seconds=0.01
        )
        write_volume(library, "Dr Stone", "Volume 01")

        real_write_if_changed = metadata_service.write_if_changed

        def slow_write_if_changed(path: Path, data: bytes) -> bool:
            time.sleep(0.15)
            return real_write_if_changed(path, data)

        monkeypatch.setattr(metadata_service, "write_if_changed", slow_write_if_changed)

        service.schedule_regeneration()
        time.sleep(0.1)  # let the debounce fire and enter the slow series.json write
        service.stop()

        # stop() must not return until the WHOLE in-flight pass (both files,
        # written sequentially) is done: if it returned early, the catalog
        # (written second) would still be missing here.
        assert (library / "Dr Stone" / "series.json").exists()
        assert (library / "catalog.json").exists()


class TestStoppedGate:
    """F5 (final review): the three public pass entry points must no-op
    after `stop()` -- not just `schedule_regeneration`, which already had
    this gate. Narrows (does not fully close -- see the docstrings in
    service.py) the shutdown window where a request still in flight could
    otherwise publish and fire `on_published` after `stop()` has returned."""

    def test_regenerate_all_is_a_no_op_after_stop(
        self, library: Path, tmp_path: Path
    ) -> None:
        calls: list[int] = []
        service = MetadataService(
            library, Database(tmp_path / "test.db"), on_published=lambda: calls.append(1)
        )
        write_volume(library, "Dr Stone", "Volume 01")
        service.stop()

        assert service.regenerate_all() == 0

        assert calls == []
        assert not (library / "Dr Stone" / "series.json").exists()
        assert not (library / "catalog.json").exists()

    def test_regenerate_series_is_a_no_op_after_stop(
        self, library: Path, tmp_path: Path
    ) -> None:
        calls: list[int] = []
        service = MetadataService(
            library, Database(tmp_path / "test.db"), on_published=lambda: calls.append(1)
        )
        write_volume(library, "Dr Stone", "Volume 01")
        service.stop()

        assert service.regenerate_series("Dr Stone") is False

        assert calls == []
        assert not (library / "Dr Stone" / "series.json").exists()

    def test_apply_series_update_is_a_no_op_after_stop(
        self, library: Path, tmp_path: Path
    ) -> None:
        calls: list[int] = []
        service = MetadataService(
            library, Database(tmp_path / "test.db"), on_published=lambda: calls.append(1)
        )
        write_volume(library, "Dr Stone", "Volume 01")
        service.stop()

        accepted = service.apply_series_update("Dr Stone", series_update(), "alice")

        assert accepted is False
        assert calls == []
        assert service.database.get_series_facts("dr stone") is None
        assert not (library / "Dr Stone" / "series.json").exists()


class TestReentrantPublishHook:
    """N1: `on_published` is Task 10's cache-invalidation seam, and a hook is
    free to re-enter the service (trigger another regeneration, call
    `stop()`). `_pass_lock` is a plain `threading.Lock` — not reentrant, no
    timeout — so the hook MUST fire only after the lock that guarded the
    pass which triggered it has already been released. Every reproduction
    here runs the risky call on a background thread and joins it with a
    bounded timeout: if the fix regresses, the assertion fails cleanly
    instead of hanging the whole test suite.
    """

    def test_a_hook_that_triggers_another_regeneration_does_not_deadlock(
        self, library: Path, tmp_path: Path
    ) -> None:
        write_volume(library, "Dr Stone", "Volume 01")

        def hook() -> None:
            service.regenerate_series("Dr Stone")

        service = MetadataService(library, Database(tmp_path / "test.db"), on_published=hook)

        finished = threading.Event()

        def run() -> None:
            service.apply_series_update("Dr Stone", series_update(), "alice")
            finished.set()

        thread = threading.Thread(target=run, daemon=True)
        thread.start()
        thread.join(timeout=5.0)
        assert finished.is_set(), "a re-entrant on_published hook deadlocked on _pass_lock"
        service.stop()

    def test_a_hook_that_calls_stop_does_not_deadlock(
        self, library: Path, tmp_path: Path
    ) -> None:
        write_volume(library, "Dr Stone", "Volume 01")

        def hook() -> None:
            service.stop()

        service = MetadataService(library, Database(tmp_path / "test.db"), on_published=hook)

        finished = threading.Event()

        def run() -> None:
            service.regenerate_series("Dr Stone")
            finished.set()

        thread = threading.Thread(target=run, daemon=True)
        thread.start()
        thread.join(timeout=5.0)
        assert finished.is_set(), "on_published calling stop() deadlocked on _pass_lock"

    def test_a_hook_re_entering_via_apply_series_update_does_not_deadlock(
        self, library: Path, tmp_path: Path
    ) -> None:
        write_volume(library, "Dr Stone", "Volume 01")

        def hook() -> None:
            service.apply_series_update("Dr Stone", series_update(tag="from hook"), "hook")

        service = MetadataService(library, Database(tmp_path / "test.db"), on_published=hook)

        finished = threading.Event()

        def run() -> None:
            service.regenerate_all()
            finished.set()

        thread = threading.Thread(target=run, daemon=True)
        thread.start()
        thread.join(timeout=5.0)
        assert finished.is_set(), "a re-entrant on_published hook deadlocked on _pass_lock"
        service.stop()

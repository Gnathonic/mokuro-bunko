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

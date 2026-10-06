"""Library paths behave as on NTFS: case-insensitive, case-preserving.

Prod 2026-10-06: an upload spelled `kingdom/` created a second folder beside
`Kingdom/` on the case-sensitive host, and the catalog then showed only the
stray folder's one volume. On NTFS that request lands in `Kingdom/`; these
pin that the server now behaves the same on any filesystem -- and that
renaming to fix a folder's case still works.
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import Config, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.server import create_app
from mokuro_bunko.webdav.path_case import LibraryPathCanonicalizer
from mokuro_bunko.webdav.provider import MokuroDAVProvider
from tests.integration.test_webdav_ops import (
    WSGITestClient,
    make_auth_header,
    make_valid_cbz_bytes,
)

ADMIN = {"Authorization": make_auth_header("admin", "pass1234")}
UPLOADER = {"Authorization": make_auth_header("uploader", "pass1234")}


@pytest.fixture
def test_storage(tmp_path: Path) -> Path:
    storage = tmp_path / "storage"
    library = storage / "library"
    (library / "series").mkdir(parents=True)
    (library / "series" / "vol1.cbz").write_bytes(b"volume 1")
    (library / "manga1.cbz").write_bytes(b"fake cbz content 1")
    (library / "manga2.cbz").write_bytes(b"fake cbz content 2")
    return storage


@pytest.fixture
def test_db(test_storage: Path) -> Database:
    db = Database(test_storage / "mokuro.db")
    db.create_user("uploader", "pass1234", "uploader")
    db.create_user("admin", "pass1234", "admin")
    return db


@pytest.fixture
def client(test_storage: Path, test_db: Database) -> WSGITestClient:
    return WSGITestClient(create_app(Config(storage=StorageConfig(base_path=test_storage))))


def library_names(storage: Path) -> list[str]:
    return sorted(entry.name for entry in (storage / "library").iterdir())


class TestRequestsReachTheExistingSpelling:
    def test_put_into_a_case_variant_folder_lands_in_the_existing_one(
        self, client: WSGITestClient, test_storage: Path, test_db: Database
    ) -> None:
        response = client.put(
            "/mokuro-reader/SERIES/vol2.cbz", content=make_valid_cbz_bytes(), headers=UPLOADER
        )
        assert response.status_code in (200, 201, 204)
        assert (test_storage / "library" / "series" / "vol2.cbz").is_file()
        assert "SERIES" not in library_names(test_storage)
        assert test_db.get_volume_owner("series/vol2.cbz") == "uploader"

    def test_a_new_folder_keeps_the_spelling_it_was_created_with(
        self, client: WSGITestClient, test_storage: Path
    ) -> None:
        # The reader's order: make the series folder (a 405 means it exists),
        # then upload into it.
        assert client.request("MKCOL", "/mokuro-reader/Kingdom", headers=UPLOADER).status_code == 201
        assert client.request("MKCOL", "/mokuro-reader/kingdom", headers=UPLOADER).status_code == 405
        for path in ("/mokuro-reader/Kingdom/v01.cbz", "/mokuro-reader/kingdom/v80.cbz"):
            response = client.put(path, content=make_valid_cbz_bytes(), headers=UPLOADER)
            assert response.status_code in (200, 201, 204)
        assert sorted(p.name for p in (test_storage / "library" / "Kingdom").iterdir()) == [
            "v01.cbz",
            "v80.cbz",
        ]
        assert "kingdom" not in library_names(test_storage)

    def test_get_and_propfind_find_a_case_variant(self, client: WSGITestClient) -> None:
        response = client.get("/mokuro-reader/Series/VOL1.cbz")
        assert response.status_code == 200
        assert response.content == b"volume 1"

        listing = client.request("PROPFIND", "/mokuro-reader/SERIES/", headers={"Depth": "1"})
        assert listing.status_code == 207
        assert "vol1.cbz" in listing.text

    def test_mkcol_of_a_case_variant_is_an_existing_folder(
        self, client: WSGITestClient, test_storage: Path
    ) -> None:
        response = client.request("MKCOL", "/mokuro-reader/SERIES", headers=UPLOADER)
        assert response.status_code == 405
        assert "SERIES" not in library_names(test_storage)

    def test_a_non_ascii_case_variant_is_rewritten_in_wsgi_form(
        self, client: WSGITestClient, test_storage: Path
    ) -> None:
        """PATH_INFO arrives as latin-1-decoded UTF-8 bytes (PEP 3333)."""
        (test_storage / "library" / "Élan").mkdir()
        wsgi_path = "/mokuro-reader/élan/第80巻.cbz".encode().decode("iso-8859-1")
        response = client.put(wsgi_path, content=make_valid_cbz_bytes(), headers=UPLOADER)
        assert response.status_code in (200, 201, 204)
        assert (test_storage / "library" / "Élan" / "第80巻.cbz").is_file()
        assert "élan" not in library_names(test_storage)

    def test_a_case_variant_cannot_sidestep_volume_ownership(
        self, client: WSGITestClient, test_storage: Path, test_db: Database
    ) -> None:
        """An uploader may replace only their own volumes; spelling the path
        differently used to make it a NEW file in a new folder instead."""
        test_db.create_user("uploader2", "pass1234", "uploader")
        client.request("MKCOL", "/mokuro-reader/Owned", headers=UPLOADER)
        first = client.put(
            "/mokuro-reader/Owned/v1.cbz", content=make_valid_cbz_bytes(), headers=UPLOADER
        )
        assert first.status_code in (200, 201, 204)

        other = client.put(
            "/mokuro-reader/OWNED/V1.cbz",
            content=make_valid_cbz_bytes(),
            headers={"Authorization": make_auth_header("uploader2", "pass1234")},
        )
        assert other.status_code == 403
        assert library_names(test_storage).count("Owned") == 1
        assert "OWNED" not in library_names(test_storage)

        own = client.put(
            "/mokuro-reader/OWNED/V1.cbz", content=make_valid_cbz_bytes(), headers=UPLOADER
        )
        assert own.status_code in (200, 201, 204)
        assert sorted(p.name for p in (test_storage / "library" / "Owned").iterdir()) == ["v1.cbz"]


class TestCaseOnlyRenames:
    def test_renaming_a_folder_to_fix_its_case(
        self, client: WSGITestClient, test_storage: Path, test_db: Database
    ) -> None:
        test_db.record_volume_upload("series/vol1.cbz", "uploader")
        (test_storage / "library" / "series" / "vol1.mokuro").write_text("{}", encoding="utf-8")

        response = client.request(
            "MOVE",
            "/mokuro-reader/series",
            headers={
                **ADMIN,
                "Destination": "http://localhost:8080/mokuro-reader/Series",
                "Overwrite": "T",
                "Depth": "infinity",
            },
        )
        assert response.status_code in (201, 204)
        assert "Series" in library_names(test_storage)
        assert "series" not in library_names(test_storage)
        renamed = test_storage / "library" / "Series"
        assert sorted(p.name for p in renamed.iterdir()) == ["vol1.cbz", "vol1.mokuro"]
        assert test_db.get_volume_owner("Series/vol1.cbz") == "uploader"
        assert test_db.get_volume_owner("series/vol1.cbz") is None

    def test_renaming_a_file_to_fix_its_case(
        self, client: WSGITestClient, test_storage: Path
    ) -> None:
        response = client.request(
            "MOVE",
            "/mokuro-reader/series/vol1.cbz",
            headers={
                **ADMIN,
                "Destination": "http://localhost:8080/mokuro-reader/series/Vol1.cbz",
                "Overwrite": "F",
            },
        )
        assert response.status_code in (201, 204)
        assert sorted(p.name for p in (test_storage / "library" / "series").iterdir()) == [
            "Vol1.cbz"
        ]

    def test_a_case_fix_through_a_differently_spelled_parent(
        self, client: WSGITestClient, test_storage: Path
    ) -> None:
        response = client.request(
            "MOVE",
            "/mokuro-reader/SERIES/vol1.cbz",
            headers={
                **ADMIN,
                "Destination": "http://localhost:8080/mokuro-reader/Series/VOL1.cbz",
                "Overwrite": "F",
            },
        )
        assert response.status_code in (201, 204)
        assert library_names(test_storage).count("series") == 1
        assert sorted(p.name for p in (test_storage / "library" / "series").iterdir()) == [
            "VOL1.cbz"
        ]

    def test_moving_onto_a_case_variant_of_another_file_is_moving_onto_it(
        self, client: WSGITestClient, test_storage: Path
    ) -> None:
        refused = client.request(
            "MOVE",
            "/mokuro-reader/manga1.cbz",
            headers={
                **ADMIN,
                "Destination": "http://localhost:8080/mokuro-reader/MANGA2.cbz",
                "Overwrite": "F",
            },
        )
        assert refused.status_code == 412
        assert (test_storage / "library" / "manga2.cbz").read_bytes() == b"fake cbz content 2"

    def test_copying_onto_its_own_case_variant_is_refused(
        self, client: WSGITestClient, test_storage: Path
    ) -> None:
        response = client.request(
            "COPY",
            "/mokuro-reader/manga1.cbz",
            headers={
                **ADMIN,
                "Destination": "http://localhost:8080/mokuro-reader/MANGA1.cbz",
                "Overwrite": "T",
            },
        )
        assert response.status_code == 403
        assert "MANGA1.cbz" not in library_names(test_storage)


class TestCaseInsensitiveHost:
    """NTFS/APFS hosts: the filesystem itself opens `Kingdom` as `kingdom`."""

    def test_a_moves_case_variant_destination_is_not_an_existing_resource(
        self, test_storage: Path
    ) -> None:
        """wsgidav deletes an existing MOVE destination first (`Overwrite: T`);
        on a case-insensitive host that destination IS the source. A hardlink
        reproduces "two spellings, one file" on a case-sensitive test host."""
        series = test_storage / "library" / "series"
        os.link(series / "vol1.cbz", series / "VOL1.cbz")
        provider = MokuroDAVProvider(test_storage)
        environ: dict[str, Any] = {
            "REQUEST_METHOD": "MOVE",
            "PATH_INFO": "/mokuro-reader/series/vol1.cbz",
            "wsgidav.provider": provider,
        }
        assert provider.get_resource_inst("/mokuro-reader/series/VOL1.cbz", environ) is None
        assert provider.get_resource_inst("/mokuro-reader/series/vol1.cbz", environ) is not None

        environ["REQUEST_METHOD"] = "GET"
        assert provider.get_resource_inst("/mokuro-reader/series/VOL1.cbz", environ) is not None

    def test_a_case_fix_destination_keeps_its_spelling(self, test_storage: Path) -> None:
        provider = MokuroDAVProvider(test_storage)
        library = (test_storage / "library").resolve()
        mapper = provider.path_mapper
        assert mapper.destination_to_physical("/mokuro-reader/Series/") == library / "Series"
        assert (
            mapper.destination_to_physical("/mokuro-reader/series/VOL1.cbz")
            == library / "series" / "VOL1.cbz"
        )
        assert mapper.destination_to_physical("/mokuro-reader/../escape") is None

    def test_a_destination_symlink_leading_outside_is_refused(
        self, client: WSGITestClient, test_storage: Path, tmp_path: Path
    ) -> None:
        """The last segment is not resolved (see above), but a COPY writes
        THROUGH a symlink at the destination: where it leads must still be
        inside the library, as `virtual_to_physical` required."""
        outside = tmp_path / "outside.cbz"
        outside.write_bytes(b"untouched")
        (test_storage / "library" / "series" / "link.cbz").symlink_to(outside)
        mapper = MokuroDAVProvider(test_storage).path_mapper
        assert mapper.destination_to_physical("/mokuro-reader/series/link.cbz") is None

        # wsgidav reports a refused per-resource copy as done (it ignores
        # `copy_move_single`'s False), so the status says nothing here: what
        # matters is that nothing was written through the link.
        client.request(
            "COPY",
            "/mokuro-reader/manga1.cbz",
            headers={
                **ADMIN,
                "Destination": "http://localhost:8080/mokuro-reader/series/link.cbz",
                "Overwrite": "T",
            },
        )
        assert outside.read_bytes() == b"untouched"
        assert (test_storage / "library" / "series" / "link.cbz").is_symlink()


class TestCanonicalizer:
    @pytest.fixture(params=[True, False], ids=["case-sensitive", "case-insensitive"])
    def canonicalizer(self, request: pytest.FixtureRequest, tmp_path: Path) -> Any:
        (tmp_path / "Kingdom").mkdir()
        (tmp_path / "Kingdom" / "第01巻.cbz").write_bytes(b"")
        return LibraryPathCanonicalizer(tmp_path, case_sensitive=request.param)

    def test_existing_segments_take_their_on_disk_spelling(self, canonicalizer: Any) -> None:
        assert canonicalizer.canonicalize("kingdom/第01巻.CBZ") == "Kingdom/第01巻.cbz"
        assert canonicalizer.canonicalize("KINGDOM/") == "Kingdom/"
        assert canonicalizer.canonicalize("kingdom/New Volume.cbz") == "Kingdom/New Volume.cbz"
        assert canonicalizer.canonicalize("Other/kingdom") == "Other/kingdom"

    def test_a_decomposed_spelling_matches_the_composed_folder(self, tmp_path: Path) -> None:
        (tmp_path / "Pokémon").mkdir()  # composed é
        canonicalizer = LibraryPathCanonicalizer(tmp_path, case_sensitive=True)
        decomposed = "POKÉMON/v1.cbz"
        assert canonicalizer.canonicalize(decomposed) == "Pokémon/v1.cbz"

    def test_an_exact_spelling_wins_over_a_variant(self, tmp_path: Path) -> None:
        """A case-sensitive library that already holds both keeps both reachable."""
        (tmp_path / "Kingdom").mkdir()
        (tmp_path / "kingdom").mkdir()
        canonicalizer = LibraryPathCanonicalizer(tmp_path, case_sensitive=True)
        assert canonicalizer.canonicalize("kingdom/x") == "kingdom/x"
        assert canonicalizer.canonicalize("Kingdom/x") == "Kingdom/x"
        assert canonicalizer.canonicalize("KINGDOM/x") == "Kingdom/x"

    def test_a_rename_of_the_source_keeps_the_requested_spelling(
        self, canonicalizer: Any
    ) -> None:
        assert (
            canonicalizer.canonicalize("KINGDOM/", keep_last_if_variant_of="Kingdom")
            == "KINGDOM/"
        )
        assert (
            canonicalizer.canonicalize("kingdom/第01巻.CBZ", keep_last_if_variant_of="Kingdom/第01巻.cbz")
            == "Kingdom/第01巻.CBZ"
        )
        # Not the source: resolved as usual.
        assert canonicalizer.canonicalize("KINGDOM/", keep_last_if_variant_of="Other") == "Kingdom/"

    def test_traversal_is_never_resolved(self, canonicalizer: Any) -> None:
        assert canonicalizer.canonicalize("kingdom/../KINGDOM") == "kingdom/../KINGDOM"

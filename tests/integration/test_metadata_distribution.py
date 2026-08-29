"""End-to-end metadata distribution through the assembled WSGI stack."""

from __future__ import annotations

import json
import zipfile
from collections.abc import Generator
from pathlib import Path
from typing import Any

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

    def test_nobody_may_lock_a_compiled_file(self, client: WSGITestClient) -> None:
        """F6 (final review): LOCK used to fall through to the generic
        MODIFY_DELETE gate, so an editor/admin got a real 200 -- an
        unhonored promise of exclusivity, since the compiler ignores DAV
        locks and will rewrite the file anyway."""
        for headers in (READER, UPLOADER, EDITOR, ADMIN):
            assert client.request("LOCK", SERIES_PATH, headers).status_code == 403
            assert client.request("LOCK", CATALOG_PATH, headers).status_code == 403

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


BOUNDARY_CATALOG_PATHS = [
    "/mokuro-reader//catalog.json",
    "/mokuro-reader///catalog.json",
]
BOUNDARY_SERIES_PATH = "/mokuro-reader//Dr Stone/series.json"


class TestBoundarySlashBypass:
    """Final whole-branch review, F1+F2: `PUT /mokuro-reader//catalog.json`
    (a doubled slash right after the reader root, and its triple-slash
    cousin) used to skip `_library_relative`'s prefix test entirely — the
    library-relative tail began with `/`, which the old code refused on a
    "never reachable" theory that was empirically false (wsgidav's own
    resolver absorbs the extra slash via `"/" + path.strip("/")` before the
    real filesystem resolver is ever asked). That let every ADD_FILES role
    overwrite the raw compiled catalog with unvalidated bytes, bypassing
    `MetadataAPI`, and turned the same spelling's MKCOL into an unhandled
    500 and its MOVE `Destination` into a source-destroying no-op. These
    are exactly the reviewer's reproduced spellings, end to end through the
    real WSGI stack (auth -> metadata interception -> DAV)."""

    @pytest.mark.parametrize("path", BOUNDARY_CATALOG_PATHS)
    @pytest.mark.parametrize(
        "headers,expected_status",
        [
            (UPLOADER, 403),
            (EDITOR, 403),
            (ADMIN, 403),
            (READER, 403),
            ({}, 401),
        ],
    )
    def test_put_the_boundary_spelling_is_refused_not_written(
        self,
        client: WSGITestClient,
        storage: Path,
        path: str,
        headers: dict[str, str],
        expected_status: int,
    ) -> None:
        compiled = storage / "library" / "catalog.json"
        before = compiled.read_bytes()

        response = client.put(path, b'{"pwned":true}', headers)

        assert response.status_code == expected_status
        assert compiled.read_bytes() == before

    @pytest.mark.parametrize("path", BOUNDARY_CATALOG_PATHS)
    @pytest.mark.parametrize("headers", [UPLOADER, EDITOR, ADMIN])
    def test_mkcol_the_boundary_spelling_403s_instead_of_500ing(
        self, client: WSGITestClient, path: str, headers: dict[str, str]
    ) -> None:
        response = client.request("MKCOL", path, headers)
        assert response.status_code == 403

    def test_move_destination_onto_the_boundary_catalog_spelling_is_refused(
        self, client: WSGITestClient, storage: Path
    ) -> None:
        decoy = storage / "library" / "Dr Stone" / "decoy.json"
        decoy.write_text('{"decoy":true}', encoding="utf-8")
        catalog = storage / "library" / "catalog.json"
        before_catalog = catalog.read_bytes()

        response = client.request(
            "MOVE",
            "/mokuro-reader/Dr Stone/decoy.json",
            {**EDITOR, "Destination": "/mokuro-reader//catalog.json"},
        )

        assert response.status_code == 403
        assert decoy.exists()  # source not destroyed
        assert catalog.read_bytes() == before_catalog  # not clobbered

    def test_move_destination_onto_the_boundary_series_spelling_is_refused(
        self, client: WSGITestClient, storage: Path
    ) -> None:
        decoy = storage / "library" / "Dr Stone" / "decoy.json"
        decoy.write_text('{"decoy":true}', encoding="utf-8")
        series = storage / "library" / "Dr Stone" / "series.json"
        before_series = series.read_bytes()

        response = client.request(
            "MOVE",
            "/mokuro-reader/Dr Stone/decoy.json",
            {**EDITOR, "Destination": BOUNDARY_SERIES_PATH},
        )

        assert response.status_code == 403
        assert decoy.exists()
        assert series.read_bytes() == before_series

    def test_put_the_boundary_series_spelling_is_intercepted_like_the_ordinary_one(
        self, client: WSGITestClient
    ) -> None:
        """The series sidecar's boundary spelling is a `MetadataAPI` update
        request, not a bare-denied compiled write — pinned separately from
        the catalog cases above, which ARE bare-denied for every role."""
        response = client.put(
            BOUNDARY_SERIES_PATH, update_payload(tag="from boundary"), UPLOADER
        )
        assert response.status_code == 204

        document = json.loads(client.get(SERIES_PATH, READER).text)
        assert document["tag"] == "from boundary"


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
        # `permissions` also carries a `metadata` scope object nested inside
        # it (Task 11, 2026-08-24 ruling: `identity payload nests under
        # permissions.metadata`, not a body-level sibling) — the brief's
        # 3-key set predates that change; landed code adds a 4th key here.
        assert set(anonymous["permissions"]) == {
            "canWriteProgress",
            "canAddFiles",
            "canModifyDelete",
            "metadata",
        }
        assert anonymous["permissions"]["metadata"] == {"scope": "none"}

        authenticated = json.loads(client.get("/login/api/me", READER).text)
        assert authenticated["authenticated"] is True
        assert authenticated["permissions"]["canWriteProgress"] is True
        assert authenticated["permissions"]["canAddFiles"] is False


class TestRegenerationTriggers:
    def test_a_library_change_schedules_a_recompilation(
        self, app: Any, storage: Path
    ) -> None:
        """The watcher's hook is wired to the service (contract §4).

        Forces a synchronous pass directly, rather than waiting out the
        10s debounce, by calling `regenerate_all()` itself instead of
        letting the already-scheduled timer fire. Deliberately does NOT
        call `stop()` first (F5, final review: `stop()` now permanently
        disables `regenerate_all` too, not just the debounce timer) — the
        `app` fixture's teardown cancels the still-pending timer either way.
        """
        write_volume(storage / "library", "Dr Stone", "Volume 02", "uuid-volume-02")
        app._library_watcher.on_change(
            str(storage / "library" / "Dr Stone" / "Volume 02.cbz")
        )
        assert app._metadata_service._series_timers != {}

        app._metadata_service.regenerate_all()
        document = json.loads(
            (storage / "library" / "Dr Stone" / "series.json").read_text("utf-8")
        )
        assert [entry["volume_title"] for entry in document["volumes"]] == [
            "Volume 01",
            "Volume 02",
        ]

"""Unit tests for catalog API volume metadata."""

from __future__ import annotations

import json
from pathlib import Path

from mokuro_bunko.catalog.api import CatalogAPI


def _start_response_capture():
    state: dict[str, object] = {}

    def _start_response(status: str, headers: list[tuple[str, str]]) -> None:
        state["status"] = status
        state["headers"] = headers

    return state, _start_response


def _read_json_response(chunks: list[bytes]) -> dict[str, object]:
    return json.loads(b"".join(chunks).decode("utf-8"))


def test_series_endpoint_marks_ocr_pending(tmp_path: Path) -> None:
    """Volumes with CBZ and no mokuro sidecar are marked pending (series view)."""
    library = tmp_path / "library"
    series = library / "Series A"
    series.mkdir(parents=True)

    (series / "vol1.cbz").write_bytes(b"cbz")
    (series / "vol2.cbz").write_bytes(b"cbz")
    (series / "vol2.mokuro").write_text("sidecar")

    api = CatalogAPI(
        app=lambda e, s: [],
        storage_base_path=str(library),
        enabled=True,
    )
    state, start_response = _start_response_capture()
    body = _read_json_response(api._get_series(start_response, "Series A"))

    assert state["status"] == "200 OK"
    volumes = body["volumes"]
    vol1 = next(v for v in volumes if v["name"] == "vol1")
    vol2 = next(v for v in volumes if v["name"] == "vol2")
    assert vol1["ocr_pending"] is True
    assert vol2["ocr_pending"] is False


def test_library_root_is_slim_counts_not_volume_lists(tmp_path: Path) -> None:
    """The root listing carries per-series counts, never nested volumes —
    at production scale the nested form was ~3 MB of JSON the root view
    never rendered (it shows name, cover, count)."""
    library = tmp_path / "library"
    series = library / "Series A"
    series.mkdir(parents=True)
    (series / "vol1.cbz").write_bytes(b"cbz")
    (series / "vol2.cbz").write_bytes(b"cbz")

    api = CatalogAPI(
        app=lambda e, s: [],
        storage_base_path=str(library),
        enabled=True,
    )
    state, start_response = _start_response_capture()
    body = _read_json_response(api._list_library(start_response))

    assert state["status"] == "200 OK"
    entry = body["series"][0]
    assert entry["volume_count"] == 2
    assert "volumes" not in entry


def test_library_includes_titles_from_series_facts(tmp_path: Path) -> None:
    from mokuro_bunko.database import Database

    library = tmp_path / "library"
    (library / "Dr Stone").mkdir(parents=True)
    (library / "Dr Stone" / "v01.cbz").write_bytes(b"cbz")
    (library / "Unlinked").mkdir()
    (library / "Unlinked" / "v01.cbz").write_bytes(b"cbz")

    db = Database(tmp_path / "test.db")
    db.put_series_facts(
        {
            "series_key": "dr stone",
            "series_title": "Dr Stone",
            "external_ids": {"anilist": 98416},
            "titles": {"native": "Dr.STONE", "english": "Dr. Stone"},
            "synonyms": [],
            "tag": None,
            "unit": None,
            "facts_updated_at": "2026-08-18T19:36:24.324Z",
            "spine_offset": None,
            "volume_offsets": {},
            "updated_by": None,
            "updated_at": "",
        }
    )

    api = CatalogAPI(
        app=lambda e, s: [],
        storage_base_path=str(library),
        enabled=True,
        database=db,
    )
    state, start_response = _start_response_capture()
    body = _read_json_response(api._list_library(start_response))

    assert state["status"] == "200 OK"
    by_name = {s["name"]: s for s in body["series"]}
    assert by_name["Dr Stone"]["titles"] == {"native": "Dr.STONE", "english": "Dr. Stone"}
    assert "titles" not in by_name["Unlinked"]


def test_library_serves_from_the_materialized_table_when_populated(tmp_path: Path) -> None:
    """Once a pass has materialized `catalog_series`, the listing is a DB read —
    the filesystem walk never runs on the request path."""
    from mokuro_bunko.database import Database

    library = tmp_path / "library"
    library.mkdir()  # deliberately EMPTY: entries must come from the table

    db = Database(tmp_path / "test.db")
    db.upsert_catalog_series(
        {
            "series_key": "dr stone",
            "folder_name": "Dr Stone",
            "cover_path": "Dr Stone/v01.webp",
            "volume_count": 3,
            "latest_volume_modified": 1_756_400_000.0,
            "total_pages": 570,
            "total_chars": 42000,
        }
    )

    api = CatalogAPI(
        app=lambda e, s: [],
        storage_base_path=str(library),
        enabled=True,
        database=db,
    )
    state, start_response = _start_response_capture()
    body = _read_json_response(api._list_library(start_response))

    assert state["status"] == "200 OK"
    entry = body["series"][0]
    assert entry["name"] == "Dr Stone"
    assert entry["cover"] == "Dr Stone/v01.webp"
    assert entry["volume_count"] == 3
    assert entry["latest_volume_modified"] == 1_756_400_000.0
    assert entry["total_pages"] == 570
    assert entry["total_chars"] == 42000


def test_library_falls_back_to_the_filesystem_when_the_table_is_empty(tmp_path: Path) -> None:
    from mokuro_bunko.database import Database

    library = tmp_path / "library"
    series = library / "Series A"
    series.mkdir(parents=True)
    (series / "vol1.cbz").write_bytes(b"cbz")

    api = CatalogAPI(
        app=lambda e, s: [],
        storage_base_path=str(library),
        enabled=True,
        database=Database(tmp_path / "test.db"),  # no pass has run yet
    )
    state, start_response = _start_response_capture()
    body = _read_json_response(api._list_library(start_response))

    assert state["status"] == "200 OK"
    assert [s["name"] for s in body["series"]] == ["Series A"]
    assert body["series"][0]["volume_count"] == 1


def test_library_gzips_when_the_client_accepts_it(tmp_path: Path) -> None:
    import gzip as gzip_mod

    library = tmp_path / "library"
    for i in range(40):  # enough series to clear the compression floor
        folder = library / f"Series {i:02d}"
        folder.mkdir(parents=True)
        (folder / "v01.cbz").write_bytes(b"cbz")

    api = CatalogAPI(
        app=lambda e, s: [],
        storage_base_path=str(library),
        enabled=True,
    )

    state, start_response = _start_response_capture()
    chunks = api._list_library(
        start_response, environ={"HTTP_ACCEPT_ENCODING": "gzip, deflate"}
    )
    raw = b"".join(chunks)
    headers = dict(state["headers"])
    assert headers.get("Content-Encoding") == "gzip"
    body = json.loads(gzip_mod.decompress(raw).decode("utf-8"))
    assert len(body["series"]) == 40

    # Without the header the payload stays identity-encoded.
    state2, start_response2 = _start_response_capture()
    plain = b"".join(api._list_library(start_response2, environ={}))
    assert dict(state2["headers"]).get("Content-Encoding") is None
    assert json.loads(plain.decode("utf-8"))["series"] == body["series"]


def test_series_ignores_sidecar_only_stems(tmp_path: Path) -> None:
    """Sidecar-only stems should not appear as catalog volumes."""
    library = tmp_path / "library"
    series = library / "Series B"
    series.mkdir(parents=True)

    (series / "vol3.mokuro.gz").write_text("gzip-sidecar")

    api = CatalogAPI(
        app=lambda e, s: [],
        storage_base_path=str(library),
        enabled=True,
    )
    state, start_response = _start_response_capture()
    body = _read_json_response(api._get_series(start_response, "Series B"))

    assert state["status"] == "404 Not Found"
    assert body["error"] == "Series not found"


def test_unicode_series_lookup_via_query_string(tmp_path: Path) -> None:
    """Unicode series names are resolved via query-string endpoint."""
    library = tmp_path / "library"
    series_name = "D046-158 チーズスイートホーム"
    series = library / series_name
    series.mkdir(parents=True)
    (series / "v01.cbz").write_bytes(b"cbz")

    api = CatalogAPI(
        app=lambda e, s: [],
        storage_base_path=str(library),
        enabled=True,
    )
    state, start_response = _start_response_capture()
    environ = {
        "PATH_INFO": "/catalog/api/series",
        "REQUEST_METHOD": "GET",
        "QUERY_STRING": "name=D046-158%20%E3%83%81%E3%83%BC%E3%82%BA%E3%82%B9%E3%82%A4%E3%83%BC%E3%83%88%E3%83%9B%E3%83%BC%E3%83%A0",
    }
    body = _read_json_response(list(api._handle_api(environ, start_response, environ["PATH_INFO"], "GET")))

    assert state["status"] == "200 OK"
    assert body["name"] == series_name
    assert len(body["volumes"]) == 1


def test_unicode_cover_lookup_via_query_string(tmp_path: Path) -> None:
    """Unicode cover paths are served via query-string endpoint."""
    library = tmp_path / "library"
    series_name = "D046-158 チーズスイートホーム"
    series = library / series_name
    series.mkdir(parents=True)
    cover = series / "v01.webp"
    cover.write_bytes(b"RIFFxxxxWEBP")

    api = CatalogAPI(
        app=lambda e, s: [],
        storage_base_path=str(library),
        enabled=True,
    )
    state, start_response = _start_response_capture()
    environ = {
        "PATH_INFO": "/catalog/api/cover",
        "REQUEST_METHOD": "GET",
        "QUERY_STRING": "path=D046-158%20%E3%83%81%E3%83%BC%E3%82%BA%E3%82%B9%E3%82%A4%E3%83%BC%E3%83%88%E3%83%9B%E3%83%BC%E3%83%A0%2Fv01.webp",
    }
    body = list(api._handle_api(environ, start_response, environ["PATH_INFO"], "GET"))

    assert state["status"] == "200 OK"
    assert body[0] == b"RIFFxxxxWEBP"


def test_ocr_status_endpoint_reads_progress_file(tmp_path: Path) -> None:
    """OCR status endpoint returns current progress JSON."""
    library = tmp_path / "library"
    library.mkdir(parents=True)
    progress = library.parent / ".ocr-progress.json"
    progress.write_text(
        json.dumps({
            "active": True,
            "series": "Series C",
            "volume": "v01",
            "percent": 42,
            "eta_seconds": 180,
        }),
        encoding="utf-8",
    )

    api = CatalogAPI(
        app=lambda e, s: [],
        storage_base_path=str(library),
        enabled=True,
    )
    state, start_response = _start_response_capture()
    body = _read_json_response(api._get_ocr_status(start_response))

    assert state["status"] == "200 OK"
    assert body["active"] is True
    assert body["volume"] == "v01"


def test_series_active_ocr_volume_clears_pending(tmp_path: Path) -> None:
    """Active OCR volume is marked active and not pending."""
    library = tmp_path / "library"
    series = library / "Series D"
    series.mkdir(parents=True)
    (series / "v01.cbz").write_bytes(b"cbz")
    progress = library.parent / ".ocr-progress.json"
    progress.write_text(
        json.dumps({
            "active": True,
            "relative_cbz": "Series D/v01.cbz",
            "percent": 33,
            "eta_seconds": 90,
            "status": "running",
        }),
        encoding="utf-8",
    )

    api = CatalogAPI(
        app=lambda e, s: [],
        storage_base_path=str(library),
        enabled=True,
    )
    state, start_response = _start_response_capture()
    body = _read_json_response(api._get_series(start_response, "Series D"))

    assert state["status"] == "200 OK"
    assert len(body["volumes"]) == 1
    volume = body["volumes"][0]
    assert volume["ocr_active"] is True
    assert volume["ocr_pending"] is False
    assert volume["ocr_progress"]["percent"] == 33



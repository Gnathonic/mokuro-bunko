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


def test_library_marks_ocr_pending(tmp_path: Path) -> None:
    """Volumes with CBZ and no mokuro sidecar are marked pending."""
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
    body = _read_json_response(api._list_library(start_response))

    assert state["status"] == "200 OK"
    series_list = body["series"]
    assert isinstance(series_list, list)
    volumes = series_list[0]["volumes"]

    vol1 = next(v for v in volumes if v["name"] == "vol1")
    vol2 = next(v for v in volumes if v["name"] == "vol2")
    assert vol1["ocr_pending"] is True
    assert vol2["ocr_pending"] is False


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


def test_library_cache_not_mutated_by_ocr_overlay(tmp_path: Path) -> None:
    """OCR overlay should not mutate cached library payload between requests."""
    library = tmp_path / "library"
    series = library / "Series E"
    series.mkdir(parents=True)
    (series / "v01.cbz").write_bytes(b"cbz")

    progress = library.parent / ".ocr-progress.json"
    progress.write_text(
        json.dumps({
            "active": True,
            "relative_cbz": "Series E/v01.cbz",
            "percent": 50,
        }),
        encoding="utf-8",
    )

    api = CatalogAPI(
        app=lambda e, s: [],
        storage_base_path=str(library),
        enabled=True,
    )

    # Prime cache and apply overlay.
    state1, start_response1 = _start_response_capture()
    first = _read_json_response(api._list_library(start_response1))
    assert state1["status"] == "200 OK"
    assert first["series"][0]["volumes"][0]["ocr_active"] is True

    # Remove progress file, then ensure cached base data isn't stuck active.
    progress.unlink()
    state2, start_response2 = _start_response_capture()
    second = _read_json_response(api._list_library(start_response2))
    assert state2["status"] == "200 OK"
    assert second["series"][0]["volumes"][0]["ocr_active"] is False

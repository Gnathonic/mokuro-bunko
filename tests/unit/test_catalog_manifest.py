"""The per-volume manifest the catalog's reader link points at.

`GET /catalog/api/manifest?series=<name>&volume=<name>` names every file
the reader should fetch for one volume: its archive, its primary OCR, each
extra OCR layer, its cover and the series file. It is read with exactly the
rules of the volume's own `.cbz` (the same `AuthMiddleware` decides both).
"""

from __future__ import annotations

import hashlib
import io
import json
import os
import urllib.parse
from collections.abc import Callable, Iterable
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.catalog.api import CatalogAPI
from mokuro_bunko.catalog.manifest import build_volume_manifest, reader_file_url
from mokuro_bunko.config import CatalogConfig, CorsConfig, RegistrationConfig
from mokuro_bunko.database import Database
from mokuro_bunko.metadata.compiler import SeriesFolder, compile_series_volumes
from mokuro_bunko.middleware import auth as auth_module
from mokuro_bunko.middleware.auth import AuthMiddleware
from mokuro_bunko.middleware.cors import CorsMiddleware
from mokuro_bunko.security import AuthAttemptLimiter
from tests.integration.test_webdav_ops import make_auth_header

MTIME = 1_790_000_000  # 2026-09-21T12:53:20Z


def _touch(folder: Path, name: str, body: bytes = b"x") -> Path:
    path = folder / name
    path.write_bytes(body)
    os.utime(path, (MTIME, MTIME))
    return path


def _stamp() -> str:
    return datetime.fromtimestamp(MTIME, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _names(manifest: dict[str, Any]) -> dict[str, Any]:
    """Each entry's file name only (the last URL segment, decoded)."""

    def name(entry: dict[str, Any] | None) -> str | None:
        if entry is None:
            return None
        return urllib.parse.unquote(entry["url"].rsplit("/", 1)[1])

    return {
        "archive": name(manifest["archive"]),
        "ocr": name(manifest["ocr"]),
        "layers": [(layer["id"], name(layer)) for layer in manifest["layers"]],
        "cover": name(manifest["cover"]),
        "series_file": name(manifest["series_file"]),
    }


# --------------------------------------------------------------------------
# The document
# --------------------------------------------------------------------------


class TestDocument:
    def test_a_full_volume(self, tmp_path: Path) -> None:
        series = tmp_path / "Dr Stone"
        series.mkdir()
        _touch(series, "Dr Stone 01.cbz", b"c" * 123)
        _touch(series, "Dr Stone 01.mokuro", b"m" * 45)
        _touch(series, "Dr Stone 01.hayai-nova-ppocr.mokuro", b"l" * 67)
        _touch(series, "Dr Stone 01.webp", b"w" * 8)
        _touch(series, "series.json", b"s" * 9)

        manifest = build_volume_manifest(series, "Dr Stone", "Dr Stone 01")

        stamp = _stamp()
        base = "/mokuro-reader/Dr%20Stone/"
        assert manifest == {
            "version": 1,
            "series": "Dr Stone",
            "volume": "Dr Stone 01",
            "archive": {"url": base + "Dr%20Stone%2001.cbz", "size": 123, "modified": stamp},
            "ocr": {"url": base + "Dr%20Stone%2001.mokuro", "size": 45, "modified": stamp},
            "layers": [
                {
                    "id": "hayai-nova-ppocr",
                    "url": base + "Dr%20Stone%2001.hayai-nova-ppocr.mokuro",
                    "size": 67,
                    "modified": stamp,
                }
            ],
            "cover": {"url": base + "Dr%20Stone%2001.webp", "size": 8, "modified": stamp},
            "series_file": {"url": base + "series.json", "size": 9, "modified": stamp},
        }

    def test_a_bare_archive_has_nulls_and_no_layers(self, tmp_path: Path) -> None:
        series = tmp_path / "S"
        series.mkdir()
        _touch(series, "v1.cbz")

        manifest = build_volume_manifest(series, "S", "v1")

        assert manifest is not None
        assert manifest["ocr"] is None
        assert manifest["layers"] == []
        assert manifest["cover"] is None
        assert manifest["series_file"] is None

    def test_no_archive_is_no_volume(self, tmp_path: Path) -> None:
        series = tmp_path / "S"
        series.mkdir()
        _touch(series, "v1.mokuro")
        _touch(series, "v1.webp")

        assert build_volume_manifest(series, "S", "v1") is None
        assert build_volume_manifest(series, "S", "v2") is None
        assert build_volume_manifest(tmp_path / "missing", "missing", "v1") is None

    def test_a_directory_named_like_an_archive_is_not_one(self, tmp_path: Path) -> None:
        series = tmp_path / "S"
        (series / "v1.cbz").mkdir(parents=True)

        assert build_volume_manifest(series, "S", "v1") is None

    def test_urls_escape_exactly_like_encode_uri_component(self, tmp_path: Path) -> None:
        """The catalog builds the cbz path with `encodeURIComponent` per
        segment, which leaves `A-Z a-z 0-9 - _ . ! ~ * ' ( )` alone."""
        name = "Vol (1) it's #1? 50%&more+ 世界!~*"
        series = tmp_path / "A/B"
        series.mkdir(parents=True)
        _touch(series, f"{name}.cbz")

        manifest = build_volume_manifest(series, "A/B", name)

        assert manifest is not None
        assert manifest["archive"]["url"] == (
            "/mokuro-reader/A%2FB/"
            "Vol%20(1)%20it's%20%231%3F%2050%25%26more%2B%20%E4%B8%96%E7%95%8C!~*.cbz"
        )
        assert reader_file_url("A/B", "series.json") == "/mokuro-reader/A%2FB/series.json"


class TestAssignment:
    def test_the_longest_stem_wins(self, tmp_path: Path) -> None:
        """`Vol 1.5.mokuro` is `Vol 1.5`'s primary, not layer `5` of `Vol 1`."""
        series = tmp_path / "S"
        series.mkdir()
        for name in (
            "Vol 1.cbz", "Vol 1.mokuro", "Vol 1.hayai.mokuro", "Vol 1.webp",
            "Vol 1.5.cbz", "Vol 1.5.mokuro", "Vol 1.5.hayai.mokuro.gz", "Vol 1.5.webp",
        ):
            _touch(series, name)

        assert _names(build_volume_manifest(series, "S", "Vol 1")) == {
            "archive": "Vol 1.cbz",
            "ocr": "Vol 1.mokuro",
            "layers": [("hayai", "Vol 1.hayai.mokuro")],
            "cover": "Vol 1.webp",
            "series_file": None,
        }
        assert _names(build_volume_manifest(series, "S", "Vol 1.5")) == {
            "archive": "Vol 1.5.cbz",
            "ocr": "Vol 1.5.mokuro",
            "layers": [("hayai", "Vol 1.5.hayai.mokuro.gz")],
            "cover": "Vol 1.5.webp",
            "series_file": None,
        }

    def test_without_the_longer_archive_the_file_is_a_layer(self, tmp_path: Path) -> None:
        """Assignment is to ARCHIVE stems: no `Vol 1.5.cbz`, no `Vol 1.5`."""
        series = tmp_path / "S"
        series.mkdir()
        _touch(series, "Vol 1.cbz")
        _touch(series, "Vol 1.5.mokuro")

        manifest = build_volume_manifest(series, "S", "Vol 1")

        assert _names(manifest)["layers"] == [("5", "Vol 1.5.mokuro")]
        assert manifest["ocr"] is None

    def test_a_prefix_without_the_dot_is_another_volume(self, tmp_path: Path) -> None:
        series = tmp_path / "S"
        series.mkdir()
        _touch(series, "Vol 1.cbz")
        _touch(series, "Vol 10.cbz")
        _touch(series, "Vol 10.mokuro")
        _touch(series, "Vol 10.webp")

        manifest = build_volume_manifest(series, "S", "Vol 1")

        assert manifest["ocr"] is None
        assert manifest["cover"] is None
        assert manifest["layers"] == []


class TestPlainOverGzip:
    def test_the_primary_prefers_plain(self, tmp_path: Path) -> None:
        series = tmp_path / "S"
        series.mkdir()
        _touch(series, "v1.cbz")
        _touch(series, "v1.mokuro.gz")
        _touch(series, "v1.mokuro")

        assert _names(build_volume_manifest(series, "S", "v1"))["ocr"] == "v1.mokuro"

    def test_the_primary_falls_back_to_gzip(self, tmp_path: Path) -> None:
        series = tmp_path / "S"
        series.mkdir()
        _touch(series, "v1.cbz")
        _touch(series, "v1.mokuro.gz", b"gz" * 5)

        manifest = build_volume_manifest(series, "S", "v1")

        assert _names(manifest)["ocr"] == "v1.mokuro.gz"
        assert manifest["ocr"]["size"] == 10

    def test_a_layer_prefers_plain_and_is_listed_once(self, tmp_path: Path) -> None:
        series = tmp_path / "S"
        series.mkdir()
        _touch(series, "v1.cbz")
        _touch(series, "v1.alpha.mokuro.gz")
        _touch(series, "v1.alpha.mokuro")
        _touch(series, "v1.beta.mokuro.gz")

        assert _names(build_volume_manifest(series, "S", "v1"))["layers"] == [
            ("alpha", "v1.alpha.mokuro"),
            ("beta", "v1.beta.mokuro.gz"),
        ]


class TestLayerIds:
    @pytest.mark.parametrize(
        "bad",
        [
            "Hayai",  # uppercase
            "hayai_nova",  # underscore
            "a" * 33,  # too long
            "",  # `v1..mokuro`
            "hé",  # not ASCII
        ],
    )
    def test_an_id_outside_the_grammar_is_no_layer(self, tmp_path: Path, bad: str) -> None:
        series = tmp_path / "S"
        series.mkdir()
        _touch(series, "v1.cbz")
        _touch(series, f"v1.{bad}.mokuro")

        assert build_volume_manifest(series, "S", "v1")["layers"] == []

    def test_the_grammar_edges_are_layers(self, tmp_path: Path) -> None:
        series = tmp_path / "S"
        series.mkdir()
        _touch(series, "v1.cbz")
        _touch(series, "v1." + "a" * 32 + ".mokuro")
        _touch(series, "v1.-x-.mokuro")
        _touch(series, "v1.9.mokuro")

        ids = [layer["id"] for layer in build_volume_manifest(series, "S", "v1")["layers"]]
        assert ids == ["-x-", "9", "a" * 32]

    def test_other_files_are_ignored(self, tmp_path: Path) -> None:
        series = tmp_path / "S"
        series.mkdir()
        for name in ("v1.cbz", "v1.nocover", "v1.ocr.log", "v1.mokuro.bak", "v1.jpg", "notes.txt"):
            _touch(series, name)

        manifest = build_volume_manifest(series, "S", "v1")

        assert manifest["ocr"] is None
        assert manifest["layers"] == []
        assert manifest["cover"] is None


class TestLayerOrder:
    def test_configured_order_first_then_alphabetical(self, tmp_path: Path) -> None:
        series = tmp_path / "S"
        series.mkdir()
        _touch(series, "v1.cbz")
        for layer in ("mid", "alpha-a", "zeta", "alpha-b"):
            _touch(series, f"v1.{layer}.mokuro")

        manifest = build_volume_manifest(
            series, "S", "v1", layer_order=["zeta", "gone", "alpha-b"]
        )

        assert [layer["id"] for layer in manifest["layers"]] == [
            "zeta", "alpha-b", "alpha-a", "mid",
        ]

    def test_no_configured_order_is_alphabetical(self, tmp_path: Path) -> None:
        series = tmp_path / "S"
        series.mkdir()
        _touch(series, "v1.cbz")
        for layer in ("c", "a", "b"):
            _touch(series, f"v1.{layer}.mokuro")

        assert [layer["id"] for layer in build_volume_manifest(series, "S", "v1")["layers"]] == [
            "a", "b", "c",
        ]


class TestOneListing:
    def test_one_scandir_and_no_per_file_stat(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """One directory listing per request; only the listed files are stat'ed."""
        series = tmp_path / "S"
        series.mkdir()
        _touch(series, "v1.cbz")
        _touch(series, "v1.mokuro")
        for index in range(50):
            _touch(series, f"other {index}.cbz")
            _touch(series, f"other {index}.mokuro")

        calls: list[str] = []
        real_scandir = os.scandir

        def counting_scandir(path: Any) -> Any:
            calls.append(str(path))
            return real_scandir(path)

        stats: list[str] = []
        real_stat = os.stat

        def counting_stat(path: Any, *args: Any, **kwargs: Any) -> Any:
            stats.append(str(path))
            return real_stat(path, *args, **kwargs)

        monkeypatch.setattr("mokuro_bunko.catalog.manifest.os.scandir", counting_scandir)
        monkeypatch.setattr("mokuro_bunko.catalog.manifest.os.stat", counting_stat)

        manifest = build_volume_manifest(series, "S", "v1")

        assert manifest is not None
        assert calls == [str(series)]
        assert stats == []  # sizes come from the listing's own entries


# --------------------------------------------------------------------------
# The route
# --------------------------------------------------------------------------


def _inner(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    start_response("418 I'm a teapot", [("Content-Type", "text/plain")])
    return [b"inner"]


@pytest.fixture(autouse=True)
def _private_rate_limiter(monkeypatch: pytest.MonkeyPatch) -> None:
    """AUTH_RATE_LIMITER is process-wide; the wrong-password test must not feed it."""
    monkeypatch.setattr(auth_module, "AUTH_RATE_LIMITER", AuthAttemptLimiter())


@pytest.fixture
def library(tmp_path: Path) -> Path:
    root = tmp_path / "library"
    series = root / "Dr Stone"
    series.mkdir(parents=True)
    _touch(series, "Dr Stone 01.cbz", b"c" * 3)
    _touch(series, "Dr Stone 01.mokuro")
    return root


@pytest.fixture
def database(tmp_path: Path) -> Database:
    db = Database(tmp_path / "mokuro.db")
    db.create_user("reader", "pass1234", "registered")
    return db


def _api(
    library: Path,
    database: Database,
    *,
    anonymous_download: bool = True,
    catalog_enabled: bool = True,
    layer_order: Callable[[], Iterable[str]] | None = None,
    with_metadata: bool = False,
) -> CatalogAPI:
    gate = AuthMiddleware(
        _inner,
        database,
        registration_config=RegistrationConfig(allow_anonymous_download=anonymous_download),
    )
    return CatalogAPI(
        app=gate,
        storage_base_path=str(library),
        catalog_config=CatalogConfig(enabled=catalog_enabled),
        read_gate=gate,
        layer_order=layer_order,
        database=database if with_metadata else None,
    )


def _get(
    app: Callable[..., Iterable[bytes]],
    query: dict[str, str] | str,
    headers: dict[str, str] | None = None,
    method: str = "GET",
    path: str = "/catalog/api/manifest",
) -> tuple[str, dict[str, str], bytes]:
    environ: dict[str, Any] = {
        "REQUEST_METHOD": method,
        "PATH_INFO": path,
        "QUERY_STRING": query if isinstance(query, str) else urllib.parse.urlencode(query),
        "SERVER_NAME": "localhost",
        "SERVER_PORT": "8080",
        "wsgi.input": io.BytesIO(b""),
        "wsgi.errors": io.StringIO(),
        "wsgi.url_scheme": "http",
    }
    for key, value in (headers or {}).items():
        environ["HTTP_" + key.upper().replace("-", "_")] = value
    state: dict[str, Any] = {}

    def start_response(status: str, response_headers: list[tuple[str, str]], exc_info: Any = None) -> None:
        state["status"] = status
        state["headers"] = response_headers

    body = b"".join(app(environ, start_response))
    return state["status"], dict(state["headers"]), body


VOLUME = {"series": "Dr Stone", "volume": "Dr Stone 01"}
READER = {"Authorization": make_auth_header("reader", "pass1234")}
WRONG = {"Authorization": make_auth_header("reader", "nope")}


class TestRoute:
    def test_serves_the_manifest_as_uncached_json(self, library: Path, database: Database) -> None:
        status, headers, body = _get(_api(library, database), VOLUME)

        assert status == "200 OK"
        assert headers["Content-Type"] == "application/json"
        assert headers["Cache-Control"] == "no-store"
        manifest = json.loads(body)
        assert manifest["version"] == 1
        assert manifest["archive"]["url"] == "/mokuro-reader/Dr%20Stone/Dr%20Stone%2001.cbz"
        assert manifest["ocr"]["url"] == "/mokuro-reader/Dr%20Stone/Dr%20Stone%2001.mokuro"

    def test_layer_order_comes_from_the_live_setting(self, library: Path, database: Database) -> None:
        series = library / "Dr Stone"
        _touch(series, "Dr Stone 01.b.mokuro")
        _touch(series, "Dr Stone 01.a.mokuro")
        order = ["b"]

        api = _api(library, database, layer_order=lambda: list(order))
        _, _, body = _get(api, VOLUME)
        assert [layer["id"] for layer in json.loads(body)["layers"]] == ["b", "a"]

        order[:] = ["a", "b"]
        _, _, body = _get(api, VOLUME)
        assert [layer["id"] for layer in json.loads(body)["layers"]] == ["a", "b"]

    @pytest.mark.parametrize(
        "query",
        [
            {"series": "Dr Stone", "volume": "Dr Stone 02"},
            {"series": "Nope", "volume": "Dr Stone 01"},
            {"series": "Dr Stone", "volume": "sub/Dr Stone 01"},
        ],
    )
    def test_an_unknown_volume_is_404(self, library: Path, database: Database, query: dict[str, str]) -> None:
        status, _, _ = _get(_api(library, database), query)
        assert status.startswith("404")

    @pytest.mark.parametrize(
        "query",
        [
            {"series": "..", "volume": "secret"},
            {"series": "../outside", "volume": "v1"},
            {"series": "Dr Stone", "volume": "../../outside/v1"},
            {"series": "/etc", "volume": "passwd"},
        ],
    )
    def test_a_path_escaping_storage_is_403(
        self, library: Path, database: Database, query: dict[str, str]
    ) -> None:
        outside = library.parent / "outside"
        outside.mkdir(exist_ok=True)
        _touch(outside, "v1.cbz")
        _touch(library.parent, "secret.cbz")

        status, _, _ = _get(_api(library, database), query)
        assert status.startswith("403")

    @pytest.mark.parametrize("query", ["series=Dr+Stone", "volume=x", ""])
    def test_a_missing_parameter_is_400(self, library: Path, database: Database, query: str) -> None:
        status, _, _ = _get(_api(library, database), query)
        assert status.startswith("400")

    def test_served_even_with_the_catalog_page_off(self, library: Path, database: Database) -> None:
        """Its gate is the archive's read gate, not the catalog page toggle."""
        status, _, _ = _get(_api(library, database, catalog_enabled=False), VOLUME)
        assert status == "200 OK"

    def test_other_catalog_routes_still_follow_the_toggle(self, library: Path, database: Database) -> None:
        status, _, _ = _get(
            _api(library, database, catalog_enabled=False), {"name": "Dr Stone"},
            path="/catalog/api/series",
        )
        assert status.startswith("418")  # passed through to the inner app

    def test_without_a_read_gate_there_is_no_manifest(self, library: Path) -> None:
        """Fail closed: a CatalogAPI built without the auth gate never serves one."""
        api = CatalogAPI(app=_inner, storage_base_path=str(library), enabled=True)
        status, _, _ = _get(api, VOLUME)
        assert status.startswith("404")


class TestAccess:
    """The manifest answers exactly as `GET /mokuro-reader/<series>/<volume>.cbz` would."""

    def test_anonymous_when_downloads_are_open(self, library: Path, database: Database) -> None:
        status, _, _ = _get(_api(library, database), VOLUME)
        assert status == "200 OK"

    def test_anonymous_is_challenged_when_downloads_need_a_login(
        self, library: Path, database: Database
    ) -> None:
        status, headers, _ = _get(_api(library, database, anonymous_download=False), VOLUME)
        assert status.startswith("401")
        assert headers["WWW-Authenticate"].startswith('Basic realm="mokuro-bunko"')

    def test_a_signed_in_reader_is_served_when_downloads_need_a_login(
        self, library: Path, database: Database
    ) -> None:
        status, _, _ = _get(_api(library, database, anonymous_download=False), VOLUME, READER)
        assert status == "200 OK"

    def test_wrong_credentials_are_401_even_when_downloads_are_open(
        self, library: Path, database: Database
    ) -> None:
        status, _, _ = _get(_api(library, database), VOLUME, WRONG)
        assert status.startswith("401")

    def test_access_is_decided_before_existence(self, library: Path, database: Database) -> None:
        """Like the cbz: an unauthorised caller learns nothing about which volumes exist."""
        api = _api(library, database, anonymous_download=False)
        status, _, _ = _get(api, {"series": "Dr Stone", "volume": "nope"})
        assert status.startswith("401")


class TestCors:
    ORIGIN = {"Origin": "https://reader.mokuro.app"}

    def test_an_allowed_origin_gets_the_file_cors_headers(
        self, library: Path, database: Database
    ) -> None:
        app = CorsMiddleware(_api(library, database), CorsConfig())
        status, headers, _ = _get(app, VOLUME, self.ORIGIN)

        assert status == "200 OK"
        assert headers["Access-Control-Allow-Origin"] == "https://reader.mokuro.app"
        assert headers["Access-Control-Allow-Credentials"] == "true"
        assert "Access-Control-Expose-Headers" in headers

    def test_another_origin_gets_none(self, library: Path, database: Database) -> None:
        app = CorsMiddleware(_api(library, database), CorsConfig())
        _, headers, _ = _get(app, VOLUME, {"Origin": "https://evil.example"})
        assert not any(key.startswith("Access-Control-") for key in headers)

    def test_a_preflight_is_answered(self, library: Path, database: Database) -> None:
        app = CorsMiddleware(_api(library, database, anonymous_download=False), CorsConfig())
        status, headers, _ = _get(app, VOLUME, self.ORIGIN, method="OPTIONS")
        assert status == "204 No Content"
        assert headers["Access-Control-Allow-Origin"] == "https://reader.mokuro.app"


class TestOcrSha256:
    """`ocr.sha256`: the metadata pass's hash of the primary, from its cache only."""

    @staticmethod
    def _primary(library: Path, body: bytes) -> Path:
        path = library / "Dr Stone" / "Dr Stone 01.mokuro"
        path.write_bytes(body)
        return path

    @staticmethod
    def _compile(library: Path, database: Database) -> None:
        compile_series_volumes(SeriesFolder("Dr Stone", library / "Dr Stone"), database=database)

    def test_carries_the_compiled_hash(self, library: Path, database: Database) -> None:
        body = json.dumps({"version": "0.2.2", "pages": []}).encode("utf-8")
        self._primary(library, body)
        (library / "Dr Stone" / "Dr Stone 01.hayai-nova.mokuro").write_bytes(b"{}")
        self._compile(library, database)

        _, _, raw = _get(_api(library, database, with_metadata=True), VOLUME)
        manifest = json.loads(raw)
        assert manifest["ocr"]["sha256"] == hashlib.sha256(body).hexdigest()
        # Layers are never hashed.
        assert all("sha256" not in layer for layer in manifest["layers"])

    def test_absent_before_the_pass_has_hashed_it(
        self, library: Path, database: Database, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        self._primary(library, json.dumps({"version": "0.2.2", "pages": []}).encode("utf-8"))

        def no_read(path: Path) -> object:
            raise AssertionError(f"the manifest must never read a sidecar: {path}")

        monkeypatch.setattr("mokuro_bunko.metadata.compiler._load_sidecar", no_read)
        _, _, raw = _get(_api(library, database, with_metadata=True), VOLUME)
        assert "sha256" not in json.loads(raw)["ocr"]

    def test_absent_once_the_primary_changed_since(
        self, library: Path, database: Database
    ) -> None:
        path = self._primary(library, json.dumps({"version": "0.2.2", "pages": []}).encode())
        self._compile(library, database)
        path.write_bytes(json.dumps({"version": "0.3.0", "pages": [], "x": 1}).encode())

        _, _, raw = _get(_api(library, database, with_metadata=True), VOLUME)
        assert "sha256" not in json.loads(raw)["ocr"]

    def test_no_ocr_no_hash(self, library: Path, database: Database) -> None:
        (library / "Dr Stone" / "Dr Stone 01.mokuro").unlink()
        self._compile(library, database)
        _, _, raw = _get(_api(library, database, with_metadata=True), VOLUME)
        assert json.loads(raw)["ocr"] is None

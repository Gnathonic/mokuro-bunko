"""The volume manifest through the assembled server: the `.cbz`'s rules, exactly.

Every case asks the same question twice -- once of the archive over WebDAV,
once of its manifest -- and requires the same answer.
"""

from __future__ import annotations

import io
import json
import urllib.parse
from collections.abc import Generator
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import CatalogConfig, Config, RegistrationConfig, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.middleware import auth as auth_module
from mokuro_bunko.security import AuthAttemptLimiter
from mokuro_bunko.server import create_app
from tests.integration.test_webdav_ops import make_auth_header

SERIES = "Dr Stone"
VOLUME = "Dr Stone 01"
CBZ_PATH = f"/mokuro-reader/{SERIES}/{VOLUME}.cbz"
MANIFEST_QUERY = urllib.parse.urlencode({"series": SERIES, "volume": VOLUME})
ORIGIN = "https://reader.mokuro.app"


def request(
    app: Any, method: str, path: str, query: str = "", headers: dict[str, str] | None = None
) -> tuple[int, dict[str, str], bytes]:
    environ: dict[str, Any] = {
        "REQUEST_METHOD": method,
        "SCRIPT_NAME": "",
        "PATH_INFO": path,
        "QUERY_STRING": query,
        "SERVER_NAME": "localhost",
        "SERVER_PORT": "8080",
        "SERVER_PROTOCOL": "HTTP/1.1",
        "HTTP_HOST": "localhost:8080",
        "wsgi.version": (1, 0),
        "wsgi.url_scheme": "http",
        "wsgi.input": io.BytesIO(b""),
        "wsgi.errors": io.StringIO(),
        "wsgi.multithread": False,
        "wsgi.multiprocess": False,
        "wsgi.run_once": False,
        "CONTENT_LENGTH": "0",
    }
    for key, value in (headers or {}).items():
        environ["HTTP_" + key.upper().replace("-", "_")] = value
    state: dict[str, Any] = {}

    def start_response(status: str, response_headers: list[tuple[str, str]], exc_info: Any = None) -> Any:
        state["status"] = int(status.split(" ", 1)[0])
        state["headers"] = dict(response_headers)
        return lambda _chunk: None

    result = app(environ, start_response)
    try:
        body = b"".join(result)
    finally:
        if hasattr(result, "close"):
            result.close()
    return state["status"], state["headers"], body


@pytest.fixture(autouse=True)
def _private_rate_limiter(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(auth_module, "AUTH_RATE_LIMITER", AuthAttemptLimiter())


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    base = tmp_path / "storage"
    series = base / "library" / SERIES
    series.mkdir(parents=True)
    (base / "inbox").mkdir()
    (base / "users").mkdir()
    (series / f"{VOLUME}.cbz").write_bytes(b"PK\x05\x06" + b"\x00" * 18)
    (series / f"{VOLUME}.mokuro").write_text("{}", encoding="utf-8")
    (series / f"{VOLUME}.hayai-nova.mokuro").write_text("{}", encoding="utf-8")
    db = Database(base / "mokuro.db")
    db.create_user("reader", "pass1234", "registered")
    return base


def build(storage: Path, *, anonymous_download: bool, catalog: bool) -> Any:
    return create_app(
        Config(
            storage=StorageConfig(base_path=storage),
            registration=RegistrationConfig(allow_anonymous_download=anonymous_download),
            catalog=CatalogConfig(enabled=catalog),
        )
    )


@pytest.fixture(params=[True, False], ids=["anon-download", "login-download"])
def app(request: pytest.FixtureRequest, storage: Path) -> Generator[Any, None, None]:
    yield build(storage, anonymous_download=request.param, catalog=True)


CALLERS = {
    "anonymous": {},
    "reader": {"Authorization": make_auth_header("reader", "pass1234")},
    "wrong-password": {"Authorization": make_auth_header("reader", "nope")},
    "garbage-header": {"Authorization": "Basic !!!"},
}


class TestSameRulesAsTheArchive:
    @pytest.mark.parametrize("caller", list(CALLERS))
    def test_status_matches_the_cbz(self, app: Any, caller: str) -> None:
        headers = CALLERS[caller]
        cbz_status, cbz_headers, _ = request(app, "GET", CBZ_PATH, headers=headers)
        status, manifest_headers, body = request(
            app, "GET", "/catalog/api/manifest", MANIFEST_QUERY, headers
        )

        assert status == cbz_status
        assert ("WWW-Authenticate" in manifest_headers) == ("WWW-Authenticate" in cbz_headers)
        if status == 200:
            manifest = json.loads(body)
            assert manifest["archive"]["url"] == urllib.parse.quote(CBZ_PATH)
            assert [layer["id"] for layer in manifest["layers"]] == ["hayai-nova"]
            assert manifest_headers["Cache-Control"] == "no-store"

    def test_the_catalog_page_toggle_does_not_hide_it(self, storage: Path) -> None:
        app = build(storage, anonymous_download=True, catalog=False)
        status, _, _ = request(app, "GET", "/catalog/api/manifest", MANIFEST_QUERY)
        assert status == 200

    def test_a_missing_volume_and_an_escape(self, storage: Path) -> None:
        app = build(storage, anonymous_download=True, catalog=True)
        missing = urllib.parse.urlencode({"series": SERIES, "volume": "nope"})
        escape = urllib.parse.urlencode({"series": "../users", "volume": "x"})
        assert request(app, "GET", "/catalog/api/manifest", missing)[0] == 404
        assert request(app, "GET", "/catalog/api/manifest", escape)[0] == 403


class TestSameCorsAsTheArchive:
    @pytest.mark.parametrize("origin", [ORIGIN, "http://localhost:5173", "https://evil.example"])
    def test_cors_headers_match_the_cbz(self, storage: Path, origin: str) -> None:
        app = build(storage, anonymous_download=True, catalog=True)
        headers = {"Origin": origin}
        _, cbz_headers, _ = request(app, "GET", CBZ_PATH, headers=headers)
        _, manifest_headers, _ = request(app, "GET", "/catalog/api/manifest", MANIFEST_QUERY, headers)

        def cors(h: dict[str, str]) -> dict[str, str]:
            return {k: v for k, v in h.items() if k.startswith("Access-Control-") or k == "Vary"}

        assert cors(manifest_headers) == cors(cbz_headers)
        if origin != "https://evil.example":
            assert manifest_headers["Access-Control-Allow-Origin"] == origin

    def test_preflight_matches_the_cbz(self, storage: Path) -> None:
        app = build(storage, anonymous_download=False, catalog=True)
        headers = {"Origin": ORIGIN, "Access-Control-Request-Method": "GET"}
        cbz = request(app, "OPTIONS", CBZ_PATH, headers=headers)
        manifest = request(app, "OPTIONS", "/catalog/api/manifest", MANIFEST_QUERY, headers)
        assert manifest[0] == cbz[0] == 204

        # What decides access is identical. A DAV path's preflight also says
        # X-Mokuro-Put (with an Expose-Headers list, which a browser ignores on
        # a preflight): that is about PUT, and the manifest is not a DAV file.
        def deciding(h: dict[str, str]) -> dict[str, str]:
            return {
                k: v for k, v in h.items()
                if k not in ("X-Mokuro-Put", "Access-Control-Expose-Headers")
            }

        assert deciding(manifest[1]) == deciding(cbz[1])
        assert cbz[1]["X-Mokuro-Put"] == "verified"

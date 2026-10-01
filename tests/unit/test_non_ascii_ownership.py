"""An uploader's own series under a non-ASCII title, through the real app.

PEP 3333 hands PATH_INFO over as the request bytes decoded latin-1, while
upload ownership is recorded in unicode. The auth gate compared the two as
they came, so an uploader could edit and delete their own series only when
its title was ASCII.
"""

from __future__ import annotations

import base64
import io
import json
import zipfile
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import Config, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.server import create_app
from tests.unit.test_upload_enqueue import call

PASSWORD = "uploader-pass-1"
AUTH = {"Authorization": "Basic " + base64.b64encode(f"upper:{PASSWORD}".encode()).decode()}


def _wsgi(path: str) -> str:
    """The path as a real WSGI server delivers it (PEP 3333)."""
    return path.encode("utf-8").decode("latin-1")


def _archive() -> bytes:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        archive.writestr("001.jpg", b"\xff\xd8\xff\xe0" + b"x" * 100)
    return buffer.getvalue()


@pytest.fixture
def app(tmp_path: Path) -> Any:
    base = tmp_path / "storage"
    for name in ("library", "inbox", "users"):
        (base / name).mkdir(parents=True)
    Database(base / "mokuro.db").create_user("upper", PASSWORD, "uploader")
    return create_app(Config(storage=StorageConfig(base_path=base)))


@pytest.mark.parametrize("series", ["Yotsuba", "よつばと！", "Café"])
def test_an_uploader_edits_and_deletes_their_own_series(app: Any, series: str) -> None:
    assert call(app, "MKCOL", _wsgi(f"/mokuro-reader/{series}"), headers=AUTH)[0] == 201
    status, _, _ = call(
        app,
        "PUT",
        _wsgi(f"/mokuro-reader/{series}/Vol 1.cbz"),
        _archive(),
        {**AUTH, "Content-Type": "application/zip"},
    )
    assert status == 201

    update = {
        "version": 2,
        "series_title": series,
        "external_ids": {},
        "titles": {},
        "synonyms": [],
        "tag": "x",
        "updated_at": "2026-10-01T00:00:00.000Z",
        "volumes": [],
    }
    status, _, body = call(
        app,
        "PUT",
        _wsgi(f"/mokuro-reader/{series}/series.json"),
        json.dumps(update).encode(),
        {**AUTH, "Content-Type": "application/json"},
    )
    assert status == 204, body

    status, _, body = call(app, "DELETE", _wsgi(f"/mokuro-reader/{series}/Vol 1.cbz"), headers=AUTH)
    assert status == 204, body

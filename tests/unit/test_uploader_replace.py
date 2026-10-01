"""ADD_FILES is adding: an uploader replaces only files of their own volumes.

Before, any uploader could PUT over any library file -- another account's
archive, or an untracked legacy one, which `record_volume_upload` then
handed to them along with its series' edit and delete rights.
"""

from __future__ import annotations

import base64
import io
import zipfile
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import Config, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.server import create_app
from tests.unit.test_upload_enqueue import call

PASSWORDS = {"alice": "alice-pass-123", "bob": "bob-pass-12345", "eddie": "eddie-pass-123"}


def _auth(user: str) -> dict[str, str]:
    raw = f"{user}:{PASSWORDS[user]}".encode()
    return {"Authorization": "Basic " + base64.b64encode(raw).decode()}


def _wsgi(path: str) -> str:
    return path.encode("utf-8").decode("latin-1")


def _archive(tag: bytes = b"x") -> bytes:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        archive.writestr("001.jpg", b"\xff\xd8\xff\xe0" + tag * 100)
    return buffer.getvalue()


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    base = tmp_path / "storage"
    for name in ("library", "inbox", "users"):
        (base / name).mkdir(parents=True)
    db = Database(base / "mokuro.db")
    db.create_user("alice", PASSWORDS["alice"], "uploader")
    db.create_user("bob", PASSWORDS["bob"], "uploader")
    db.create_user("eddie", PASSWORDS["eddie"], "editor")
    return base


@pytest.fixture
def app(storage: Path) -> Any:
    return create_app(Config(storage=StorageConfig(base_path=storage)))


def _put(app: Any, user: str, path: str, body: bytes, content_type: str = "application/zip") -> int:
    status, _, _ = call(
        app, "PUT", _wsgi(path), body, {**_auth(user), "Content-Type": content_type}
    )
    return status


def test_an_uploader_cannot_replace_another_accounts_archive(app: Any, storage: Path) -> None:
    assert call(app, "MKCOL", _wsgi("/mokuro-reader/よつばと！"), headers=_auth("alice"))[0] == 201
    assert _put(app, "alice", "/mokuro-reader/よつばと！/Vol 1.cbz", _archive()) == 201
    original = (storage / "library" / "よつばと！" / "Vol 1.cbz").read_bytes()

    assert _put(app, "bob", "/mokuro-reader/よつばと！/Vol 1.cbz", _archive(b"y")) == 403
    assert (storage / "library" / "よつばと！" / "Vol 1.cbz").read_bytes() == original
    # ...nor its OCR, though adding a file it lacks is still adding.
    folder = storage / "library" / "よつばと！"
    (folder / "Vol 1.mokuro").write_bytes(b"{}")
    assert _put(app, "bob", "/mokuro-reader/よつばと！/Vol 1.mokuro", b"{}", "application/json") == 403
    assert _put(app, "bob", "/mokuro-reader/よつばと！/Vol 2.cbz", _archive()) == 201

    # The owner replaces their own, sidecars included.
    assert _put(app, "alice", "/mokuro-reader/よつばと！/Vol 1.cbz", _archive(b"z")) in (201, 204)
    assert _put(app, "alice", "/mokuro-reader/よつばと！/Vol 1.mokuro", b"{}", "application/json") in (
        201,
        204,
    )


def test_an_untracked_legacy_file_is_neither_replaced_nor_captured(
    app: Any, storage: Path
) -> None:
    legacy = storage / "library" / "Legacy"
    legacy.mkdir()
    (legacy / "Vol 1.cbz").write_bytes(_archive())

    assert _put(app, "alice", "/mokuro-reader/Legacy/Vol 1.cbz", _archive(b"y")) == 403
    assert Database(storage / "mokuro.db").get_volume_owner("Legacy/Vol 1.cbz") is None
    status, _, _ = call(app, "DELETE", "/mokuro-reader/Legacy/Vol 1.cbz", headers=_auth("alice"))
    assert status == 403


def test_an_editor_still_replaces_anything(app: Any, storage: Path) -> None:
    call(app, "MKCOL", "/mokuro-reader/S", headers=_auth("alice"))
    assert _put(app, "alice", "/mokuro-reader/S/Vol 1.cbz", _archive()) == 201
    assert _put(app, "eddie", "/mokuro-reader/S/Vol 1.cbz", _archive(b"y")) in (201, 204)

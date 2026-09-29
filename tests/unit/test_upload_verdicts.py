"""Every archive PUT ends in a verdict the client can act on, never a silent 2xx.

A `.cbz` PUT is staged in a temporary file beside its destination, checked --
bytes received against `Content-Length`, then the zip's structure and every
member's CRC-32 (`processor.archives.verify_archive`) -- and only then moved
into place. Success says `X-Mokuro-Upload: verified` and `X-Mokuro-Size`;
any other PUT under the reader root gets the size check and
`X-Mokuro-Upload: stored`. A failure is a JSON body
`{ok, reason, detail, retry}` with 422 (damaged or short), 507 (disk full)
or the natural 4xx/5xx, and never touches the file that was there.
"""

from __future__ import annotations

import base64
import errno
import hashlib
import io
import json
import threading
import zipfile
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import Config, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.middleware import auth as auth_module
from mokuro_bunko.security import AuthAttemptLimiter
from mokuro_bunko.server import create_app
from tests.unit.test_upload_enqueue import call, cbz_bytes

UPLOADER = {"Authorization": "Basic " + base64.b64encode(b"uploader:pass1234").decode()}
READER = {"Authorization": "Basic " + base64.b64encode(b"reader:pass1234").decode()}
TARGET = "/mokuro-reader/S/V1.cbz"


@pytest.fixture(autouse=True)
def _private_rate_limiter(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(auth_module, "AUTH_RATE_LIMITER", AuthAttemptLimiter())


@pytest.fixture(autouse=True)
def _fresh_damage_memory(monkeypatch: pytest.MonkeyPatch) -> None:
    """The remembered damage signatures are process-wide; each test starts clean."""
    from mokuro_bunko.webdav import resources

    monkeypatch.setattr(resources, "_DAMAGE_MEMORY", resources.DamageMemory())


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    base = tmp_path / "storage"
    (base / "library" / "S").mkdir(parents=True)
    (base / "inbox").mkdir()
    (base / "users").mkdir()
    db = Database(base / "mokuro.db")
    db.create_user("uploader", "pass1234", "uploader")
    db.create_user("reader", "pass1234", "registered")
    return base


@pytest.fixture
def app(storage: Path) -> Any:
    return create_app(Config(storage=StorageConfig(base_path=storage)))


def put(
    app: Any,
    body: bytes,
    *,
    path: str = TARGET,
    headers: dict[str, str] | None = None,
    content_length: int | None = None,
) -> tuple[int, dict[str, str], bytes]:
    return call(
        app, "PUT", path, body, {**UPLOADER, **(headers or {})}, content_length=content_length
    )


def damaged_cbz() -> bytes:
    """A zip whose directory is intact but one member's bytes fail their CRC."""
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_STORED) as archive:
        archive.writestr("000.jpg", b"A" * 4000)
        archive.writestr("001.jpg", b"B" * 4000)
    data = bytearray(buffer.getvalue())
    at = data.index(b"B" * 100)
    data[at + 50] ^= 0xFF
    return bytes(data)


def verdict(body: bytes) -> dict[str, Any]:
    return json.loads(body)


def leftovers(folder: Path) -> list[str]:
    return sorted(p.name for p in folder.iterdir() if p.name.startswith("."))


class TestArchiveSuccess:
    def test_a_new_archive_is_verified(self, app: Any, storage: Path) -> None:
        body = cbz_bytes(5)
        status, headers, _ = put(app, body)

        assert status == 201
        assert headers["X-Mokuro-Upload"] == "verified"
        assert headers["X-Mokuro-Size"] == str(len(body))
        assert (storage / "library" / "S" / "V1.cbz").read_bytes() == body

    def test_a_replace_that_succeeds(self, app: Any, storage: Path) -> None:
        (storage / "library" / "S" / "V1.cbz").write_bytes(cbz_bytes(2))
        body = cbz_bytes(7)
        status, headers, _ = put(app, body)

        assert status == 204
        assert headers["X-Mokuro-Upload"] == "verified"
        assert headers["X-Mokuro-Size"] == str(len(body))
        assert (storage / "library" / "S" / "V1.cbz").read_bytes() == body
        assert leftovers(storage / "library" / "S") == []

    def test_headers_are_exposed_through_cors(self, app: Any) -> None:
        _, headers, _ = put(app, cbz_bytes(), headers={"Origin": "https://reader.mokuro.app"})
        exposed = {h.strip() for h in headers["Access-Control-Expose-Headers"].split(",")}
        assert {"X-Mokuro-Upload", "X-Mokuro-Size"} <= exposed


class TestArchiveFailures:
    def test_a_truncated_body(self, app: Any, storage: Path) -> None:
        body = cbz_bytes(5)
        status, headers, raw = put(app, body[: len(body) // 2], content_length=len(body))

        assert status == 422
        assert headers["Content-Type"] == "application/json"
        result = verdict(raw)
        assert result["ok"] is False
        assert result["reason"] == "truncated"
        assert result["retry"] is True
        assert str(len(body)) in result["detail"]
        assert not (storage / "library" / "S" / "V1.cbz").exists()
        assert leftovers(storage / "library" / "S") == []

    def test_a_damaged_zip(self, app: Any, storage: Path) -> None:
        status, _, raw = put(app, damaged_cbz())

        assert status == 422
        result = verdict(raw)
        # With no digest the server cannot tell damage in transit from a
        # damaged source, so the first time is worth a retry.
        assert (result["reason"], result["retry"]) == ("archive-damaged", True)
        assert "001.jpg" in result["detail"]
        assert not (storage / "library" / "S" / "V1.cbz").exists()

    def test_a_zip_cut_short_with_no_length_given(self, app: Any) -> None:
        """No Content-Length (chunked): the zip check still catches the cut."""
        body = cbz_bytes(5)
        status, _, raw = put(app, body[: len(body) - 30], content_length=-1)
        assert status == 422
        assert verdict(raw)["reason"] == "archive-damaged"

    def test_not_a_zip(self, app: Any, storage: Path) -> None:
        status, _, raw = put(app, b"<html>this is a login page</html>" * 20)

        assert status == 422
        result = verdict(raw)
        assert (result["reason"], result["retry"]) == ("not-an-archive", False)
        assert not (storage / "library" / "S" / "V1.cbz").exists()

    @pytest.mark.parametrize("kind", ["truncated", "damaged", "not-zip"])
    def test_a_failed_replace_keeps_the_old_file(self, app: Any, storage: Path, kind: str) -> None:
        live = storage / "library" / "S" / "V1.cbz"
        old = cbz_bytes(3)
        live.write_bytes(old)
        body = cbz_bytes(9)
        if kind == "truncated":
            status, _, _ = put(app, body[:100], content_length=len(body))
        elif kind == "damaged":
            status, _, _ = put(app, damaged_cbz())
        else:
            status, _, _ = put(app, b"garbage" * 100)

        assert status == 422
        assert live.read_bytes() == old
        assert leftovers(storage / "library" / "S") == []

    def test_disk_full(self, app: Any, storage: Path, monkeypatch: pytest.MonkeyPatch) -> None:
        from mokuro_bunko.webdav import resources

        live = storage / "library" / "S" / "V1.cbz"
        live.write_bytes(b"old bytes")

        def full(self: Any, data: bytes) -> int:
            raise OSError(errno.ENOSPC, "No space left on device")

        monkeypatch.setattr(resources._AtomicFileWriter, "_write", full)
        status, _, raw = put(app, cbz_bytes())

        assert status == 507
        result = verdict(raw)
        assert (result["reason"], result["retry"]) == ("disk-full", False)
        assert live.read_bytes() == b"old bytes"
        assert leftovers(storage / "library" / "S") == []

    def test_anonymous_is_forbidden_with_the_challenge_kept(self, app: Any) -> None:
        status, headers, raw = call(app, "PUT", TARGET, cbz_bytes())
        assert status == 401
        assert "WWW-Authenticate" in headers
        result = verdict(raw)
        assert (result["ok"], result["reason"], result["retry"]) == (False, "forbidden", False)

    def test_a_reader_may_not_add_files(self, app: Any) -> None:
        status, _, raw = call(app, "PUT", TARGET, cbz_bytes(), READER)
        assert status == 403
        assert verdict(raw)["reason"] == "forbidden"

    def test_a_server_error(self, app: Any, monkeypatch: pytest.MonkeyPatch) -> None:
        from mokuro_bunko.webdav import resources

        def broken(self: Any) -> None:
            raise OSError(errno.EIO, "I/O error")

        monkeypatch.setattr(resources._AtomicFileWriter, "_publish", broken)
        status, _, raw = put(app, cbz_bytes())
        assert status == 500
        result = verdict(raw)
        assert (result["reason"], result["retry"]) == ("server-error", True)


class TestOtherFiles:
    def test_a_sidecar_is_stored_with_its_size(self, app: Any, storage: Path) -> None:
        body = b'{"pages": []}'
        status, headers, _ = put(app, body, path="/mokuro-reader/S/V1.mokuro")
        assert status == 201
        assert headers["X-Mokuro-Upload"] == "stored"
        assert headers["X-Mokuro-Size"] == str(len(body))

    def test_a_short_sidecar_is_truncated(self, app: Any, storage: Path) -> None:
        live = storage / "library" / "S" / "V1.mokuro"
        live.write_text("{}", encoding="utf-8")
        status, _, raw = put(
            app, b'{"pages": [', path="/mokuro-reader/S/V1.mokuro", content_length=500
        )
        assert status == 422
        assert verdict(raw)["reason"] == "truncated"
        assert live.read_text(encoding="utf-8") == "{}"

    def test_a_progress_file_is_stored(self, app: Any) -> None:
        status, headers, _ = put(app, b"{}", path="/mokuro-reader/volume-data.json")
        assert status in (201, 204)
        assert headers["X-Mokuro-Upload"] == "stored"

    def test_a_series_file_put_is_left_to_the_metadata_api(self, app: Any) -> None:
        status, headers, raw = put(app, b"not json", path="/mokuro-reader/S/series.json")
        assert "X-Mokuro-Upload" not in headers
        assert b'"reason"' not in raw or status < 400


class TestConcurrentRead:
    def test_a_get_during_an_upload_sees_the_old_file(self, app: Any, storage: Path) -> None:
        live = storage / "library" / "S" / "V1.cbz"
        old = cbz_bytes(2)
        live.write_bytes(old)
        new = cbz_bytes(8)
        half = len(new) // 2
        halfway = threading.Event()
        release = threading.Event()

        class SlowBody(io.RawIOBase):
            def __init__(self) -> None:
                self.sent = 0

            def read(self, size: int = -1) -> bytes:
                if self.sent >= len(new):
                    return b""
                if self.sent >= half and not release.is_set():
                    halfway.set()
                    release.wait(10)
                end = len(new) if size < 0 else min(len(new), self.sent + size)
                end = min(end, half) if self.sent < half else end
                chunk = new[self.sent:end]
                self.sent = end
                return chunk

        results: dict[str, Any] = {}

        def upload() -> None:
            results["put"] = call(
                app, "PUT", TARGET, b"", UPLOADER, content_length=len(new), stream=SlowBody()
            )

        thread = threading.Thread(target=upload)
        thread.start()
        assert halfway.wait(10), "the upload never reached its halfway point"
        try:
            status, _, body = call(app, "GET", TARGET, headers=UPLOADER)
            assert status == 200
            assert body == old
        finally:
            release.set()
            thread.join(10)
        assert results["put"][0] == 204
        assert live.read_bytes() == new


class TestPutCapability:
    """`X-Mokuro-Put: verified` tells a client this server never needs a delete-first."""

    @pytest.mark.parametrize(
        "path", ["/", "/mokuro-reader", "/mokuro-reader/", "/mokuro-reader/S/", TARGET]
    )
    def test_on_every_dav_options(self, app: Any, path: str) -> None:
        _, headers, _ = call(app, "OPTIONS", path, headers=UPLOADER)
        assert headers.get("X-Mokuro-Put") == "verified"

    def test_on_a_cross_origin_options_and_exposed(self, app: Any) -> None:
        status, headers, _ = call(
            app, "OPTIONS", "/mokuro-reader/S/", headers={"Origin": "https://reader.mokuro.app"}
        )
        assert status == 204
        assert headers["X-Mokuro-Put"] == "verified"
        exposed = {h.strip() for h in headers["Access-Control-Expose-Headers"].split(",")}
        assert "X-Mokuro-Put" in exposed

    def test_not_on_other_routes(self, app: Any) -> None:
        _, headers, _ = call(app, "OPTIONS", "/catalog/api/manifest")
        assert "X-Mokuro-Put" not in headers
        _, headers, _ = call(
            app, "OPTIONS", "/_admin/api/x", headers={"Origin": "https://reader.mokuro.app"}
        )
        assert "X-Mokuro-Put" not in headers

    def test_on_cbz_puts_whatever_the_verdict(self, app: Any) -> None:
        _, ok, _ = put(app, cbz_bytes(), headers={"Origin": "https://reader.mokuro.app"})
        _, bad, _ = put(app, b"garbage" * 50)
        assert ok["X-Mokuro-Put"] == bad["X-Mokuro-Put"] == "verified"
        exposed = {h.strip() for h in ok["Access-Control-Expose-Headers"].split(",")}
        assert "X-Mokuro-Put" in exposed

    def test_not_on_other_puts(self, app: Any) -> None:
        _, headers, _ = put(app, b"{}", path="/mokuro-reader/S/V1.mokuro")
        assert "X-Mokuro-Put" not in headers


def digest_header(body: bytes, algorithm: str = "sha-256") -> str:
    hasher = hashlib.new(algorithm.replace("-", ""), body)
    return f"{algorithm}=:{base64.b64encode(hasher.digest()).decode()}:"


def damaged_cbz_other() -> bytes:
    """Damaged like `damaged_cbz`, but in member 000.jpg instead of 001.jpg."""
    data = bytearray(cbz_ab())
    at = data.index(b"A" * 100)
    data[at + 50] ^= 0xFF
    return bytes(data)


def cbz_ab() -> bytes:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_STORED) as archive:
        archive.writestr("000.jpg", b"A" * 4000)
        archive.writestr("001.jpg", b"B" * 4000)
    return buffer.getvalue()


class TestContentDigest:
    """RFC 9530 `Content-Digest`: tells damage in transit from a damaged source."""

    def test_a_mismatch_is_corrupted_in_transit(self, app: Any, storage: Path) -> None:
        live = storage / "library" / "S" / "V1.cbz"
        live.write_bytes(b"old")
        sent = cbz_bytes(5)
        status, _, raw = put(app, damaged_cbz(), headers={"Content-Digest": digest_header(sent)})

        assert status == 422
        result = verdict(raw)
        assert (result["ok"], result["reason"], result["retry"]) == (
            False, "corrupted-in-transit", True,
        )
        # The CRC check is skipped: the detail names no member.
        assert "001.jpg" not in result["detail"]
        assert live.read_bytes() == b"old"
        assert leftovers(storage / "library" / "S") == []

    def test_a_match_with_bad_crcs_is_the_clients_copy(self, app: Any) -> None:
        body = damaged_cbz()
        status, _, raw = put(app, body, headers={"Content-Digest": digest_header(body)})

        assert status == 422
        result = verdict(raw)
        assert (result["reason"], result["retry"]) == ("archive-damaged", False)
        assert "re-import this volume" in result["detail"].lower()
        assert "001.jpg" in result["detail"]

    @pytest.mark.parametrize("algorithm", ["sha-256", "sha-512"])
    def test_a_match_with_good_crcs_is_verified(self, app: Any, algorithm: str) -> None:
        body = cbz_bytes(4)
        status, headers, _ = put(
            app, body, headers={"Content-Digest": digest_header(body, algorithm)}
        )
        assert status == 201
        assert headers["X-Mokuro-Upload"] == "verified"
        assert headers["X-Mokuro-Digest-Verified"] == algorithm

    def test_unknown_algorithms_are_ignored(self, app: Any) -> None:
        body = cbz_bytes(4)
        header = "md5=:AAAAAAAAAAAAAAAAAAAAAA==:, " + digest_header(body)
        status, headers, _ = put(app, body, headers={"Content-Digest": header})
        assert status == 201
        assert headers["X-Mokuro-Digest-Verified"] == "sha-256"

    def test_only_unknown_algorithms_is_no_digest(self, app: Any) -> None:
        status, headers, _ = put(
            app, cbz_bytes(), headers={"Content-Digest": "md5=:AAAAAAAAAAAAAAAAAAAAAA==:"}
        )
        assert status == 201
        assert "X-Mokuro-Digest-Verified" not in headers

    @pytest.mark.parametrize(
        "header",
        [
            "sha-256=abc",  # not a byte sequence
            "sha-256=:not base64!:",
            "sha-256=:AAAA:",  # the wrong length for sha-256
            "sha-256=:{ok}:;param=1",  # parameters are not part of the field
            "sha-256",
            "",
        ],
    )
    def test_a_malformed_header_is_ignored_as_absent(self, app: Any, header: str) -> None:
        body = cbz_bytes(3)
        header = header.replace(
            "{ok}", base64.b64encode(hashlib.sha256(body).digest()).decode()
        )
        status, headers, _ = put(app, body, headers={"Content-Digest": header})
        assert status == 201
        assert headers["X-Mokuro-Upload"] == "verified"
        assert "X-Mokuro-Digest-Verified" not in headers

    def test_a_non_archive_is_checked_too(self, app: Any, storage: Path) -> None:
        live = storage / "library" / "S" / "V1.mokuro"
        live.write_text("{}", encoding="utf-8")
        status, _, raw = put(
            app, b'{"pages": [1]}', path="/mokuro-reader/S/V1.mokuro",
            headers={"Content-Digest": digest_header(b'{"pages": [2]}')},
        )
        assert status == 422
        assert verdict(raw)["reason"] == "corrupted-in-transit"
        assert live.read_text(encoding="utf-8") == "{}"

        body = b'{"pages": []}'
        status, headers, _ = put(
            app, body, path="/mokuro-reader/S/V1.mokuro",
            headers={"Content-Digest": digest_header(body)},
        )
        assert status == 204
        assert headers["X-Mokuro-Upload"] == "stored"
        assert headers["X-Mokuro-Digest-Verified"] == "sha-256"

    def test_cors_allows_and_exposes_it(self, app: Any) -> None:
        status, headers, _ = call(
            app, "OPTIONS", TARGET, headers={
                "Origin": "https://reader.mokuro.app",
                "Access-Control-Request-Method": "PUT",
                "Access-Control-Request-Headers": "content-digest",
            },
        )
        assert status == 204
        allowed = {h.strip().lower() for h in headers["Access-Control-Allow-Headers"].split(",")}
        assert "content-digest" in allowed
        body = cbz_bytes()
        _, headers, _ = put(app, body, headers={
            "Origin": "https://reader.mokuro.app", "Content-Digest": digest_header(body),
        })
        exposed = {h.strip() for h in headers["Access-Control-Expose-Headers"].split(",")}
        assert "X-Mokuro-Digest-Verified" in exposed


class TestRepeatedDamage:
    """Without a digest, the same damage arriving twice says the source is damaged."""

    def test_the_same_damage_twice_stops_the_retries(self, app: Any) -> None:
        first = verdict(put(app, damaged_cbz())[2])
        second = verdict(put(app, damaged_cbz())[2])

        assert (first["reason"], first["retry"]) == ("archive-damaged", True)
        assert (second["reason"], second["retry"]) == ("archive-damaged", False)
        assert "twice" in second["detail"]
        assert "re-import this volume" in second["detail"].lower()

    def test_different_damage_is_worth_another_try(self, app: Any) -> None:
        first = verdict(put(app, damaged_cbz())[2])
        second = verdict(put(app, damaged_cbz_other())[2])
        assert first["retry"] is True
        assert second["retry"] is True
        assert "000.jpg" in second["detail"]

    def test_the_same_damage_at_another_path_is_a_first(self, app: Any) -> None:
        put(app, damaged_cbz())
        result = verdict(put(app, damaged_cbz(), path="/mokuro-reader/S/V2.cbz")[2])
        assert result["retry"] is True

    def test_memory_is_bounded_and_expires(self) -> None:
        from mokuro_bunko.webdav.resources import DamageMemory

        clock = [1000.0]
        memory = DamageMemory(capacity=2, ttl=3600.0, clock=lambda: clock[0])
        a = ("a", 10, ("x",))
        assert memory.seen_before("/a", a) is False
        assert memory.seen_before("/a", a) is True
        memory.seen_before("/b", ("b", 1, ()))
        memory.seen_before("/c", ("c", 1, ()))  # evicts /a, the least recent
        assert memory.seen_before("/a", a) is False
        clock[0] += 3601.0
        assert memory.seen_before("/a", a) is False  # expired


def _declaring(body: bytes, uncompressed: int) -> bytes:
    """The same zip with every central-directory entry claiming ``uncompressed``."""
    data = bytearray(body)
    at = data.find(b"PK\x01\x02")
    while at != -1:
        data[at + 24 : at + 28] = uncompressed.to_bytes(4, "little")
        at = data.find(b"PK\x01\x02", at + 4)
    return bytes(data)


def _bzip2_cbz() -> bytes:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_BZIP2) as archive:
        archive.writestr("000.jpg", b"\0" * 4000)
    return buffer.getvalue()


class TestInflationBound:
    """A zip bomb costs a verdict, not a worker thread's hours of CPU.

    Verification reads each member to the size its directory entry DECLARES
    (`zipfile` stops there), so the declared total is the work to be done:
    refused up front when it is far beyond the archive's own size.
    """

    def test_a_declared_size_far_beyond_the_archive_is_refused_unread(
        self, app: Any, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from mokuro_bunko.processor import archives

        bomb = _declaring(cbz_bytes(3), 3 * 1024**3)
        opened: list[str] = []
        real_open = zipfile.ZipFile.open

        def spy(self: zipfile.ZipFile, name: Any, *args: Any, **kwargs: Any) -> Any:
            opened.append(str(name))
            return real_open(self, name, *args, **kwargs)

        monkeypatch.setattr(archives.zipfile.ZipFile, "open", spy)
        status, _, raw = put(app, bomb)

        assert status == 422
        result = verdict(raw)
        assert (result["reason"], result["retry"]) == ("archive-refused", False)
        assert "GiB" in result["detail"]
        assert opened == []
        assert not (storage / "library" / "S" / "V1.cbz").exists()

    def test_a_compression_a_reader_cannot_open_is_refused(self, app: Any) -> None:
        status, _, raw = put(app, _bzip2_cbz())

        assert status == 422
        result = verdict(raw)
        assert (result["reason"], result["retry"]) == ("archive-refused", False)
        assert "bzip2" in result["detail"]

    def test_an_ordinary_archive_is_well_inside_the_bound(self, app: Any) -> None:
        status, headers, _ = put(app, cbz_bytes(20))
        assert status == 201
        assert headers["X-Mokuro-Upload"] == "verified"

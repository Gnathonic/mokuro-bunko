"""Tests for nginx X-Accel-Redirect download offload on MokuroFileResource.

When the server runs behind nginx (MOKURO_NGINX_ACCEL=1), GET of a library
file should not stream bytes through Python. Instead the resource emits an
X-Accel-Redirect header pointing at an internal nginx location and returns an
empty body, so nginx serves the file with sendfile() and frees the WSGI thread.

Hardening requirements exercised here:
  - Only files under the library root are offloaded (path confinement).
  - The internal path is URL-encoded (spaces, unicode, special chars).
  - Range handling is delegated to nginx (support_ranges() -> False).
  - The declared Content-Length is dropped (nginx supplies the real one).
"""

from __future__ import annotations

from pathlib import Path

from mokuro_bunko.webdav.provider import MokuroDAVProvider
from mokuro_bunko.webdav.resources import MokuroFileResource


def _make_resource(
    storage_base: Path, physical_path: Path, *, accel: bool
) -> MokuroFileResource:
    provider = MokuroDAVProvider(storage_base)
    environ: dict[str, object] = {"wsgidav.provider": provider}
    if accel:
        environ["mokuro.nginx_accel"] = True
    virtual = "/mokuro-reader/Series/Vol.cbz"
    return MokuroFileResource(virtual, environ, physical_path)


def _library_file(
    storage_base: Path, rel: str = "Series/Vol 1.cbz", data: bytes = b"CBZDATA"
) -> Path:
    p = storage_base / "library" / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(data)
    return p


def _xaccel(headers: list[tuple[str, str]]) -> str | None:
    for k, v in headers:
        if k.lower() == "x-accel-redirect":
            return v
    return None


class TestNoAccel:
    """Without the flag, the resource streams bytes through Python as before."""

    def test_no_xaccel_header(self, temp_dir: Path) -> None:
        f = _library_file(temp_dir)
        res = _make_resource(temp_dir, f, accel=False)
        headers = [("Content-Length", "7"), ("Content-Type", "application/x-cbz")]
        res.finalize_headers({}, headers)
        assert _xaccel(headers) is None
        assert ("Content-Length", "7") in headers

    def test_support_ranges_true(self, temp_dir: Path) -> None:
        f = _library_file(temp_dir)
        res = _make_resource(temp_dir, f, accel=False)
        assert res.support_ranges() is True

    def test_get_content_returns_real_bytes(self, temp_dir: Path) -> None:
        f = _library_file(temp_dir, data=b"REALDATA")
        res = _make_resource(temp_dir, f, accel=False)
        with res.get_content() as fh:
            assert fh.read() == b"REALDATA"


class TestAccelLibraryFile:
    """With the flag, a library file is offloaded to nginx."""

    def test_emits_xaccel_redirect(self, temp_dir: Path) -> None:
        f = _library_file(temp_dir, rel="Series/Vol 1.cbz")
        res = _make_resource(temp_dir, f, accel=True)
        headers = [("Content-Length", "7"), ("Content-Type", "application/x-cbz")]
        res.finalize_headers({}, headers)
        assert _xaccel(headers) == "/internal-library/Series/Vol%201.cbz"

    def test_strips_content_length(self, temp_dir: Path) -> None:
        f = _library_file(temp_dir)
        res = _make_resource(temp_dir, f, accel=True)
        headers = [("Content-Length", "7"), ("Content-Type", "application/x-cbz")]
        res.finalize_headers({}, headers)
        assert all(k.lower() != "content-length" for k, _ in headers)

    def test_support_ranges_delegated_to_nginx(self, temp_dir: Path) -> None:
        f = _library_file(temp_dir)
        res = _make_resource(temp_dir, f, accel=True)
        assert res.support_ranges() is False

    def test_get_content_is_empty(self, temp_dir: Path) -> None:
        f = _library_file(temp_dir, data=b"SHOULD-NOT-BE-READ")
        res = _make_resource(temp_dir, f, accel=True)
        with res.get_content() as fh:
            assert fh.read() == b""

    def test_path_is_url_encoded(self, temp_dir: Path) -> None:
        # Spaces, unicode and reserved chars must be percent-encoded; the path
        # separator must be preserved so nginx resolves the right file.
        f = _library_file(temp_dir, rel="Series Ω/Vol #1.cbz")
        res = _make_resource(temp_dir, f, accel=True)
        headers: list[tuple[str, str]] = []
        res.finalize_headers({}, headers)
        assert _xaccel(headers) == "/internal-library/Series%20%CE%A9/Vol%20%231.cbz"


class TestAccelPathConfinement:
    """The flag must only offload files that resolve under the library root."""

    def test_non_library_file_streams_normally(self, temp_dir: Path) -> None:
        # A per-user file lives under <storage>/users, not the library root.
        outside = temp_dir / "users" / "alice" / "volume-data.json"
        outside.parent.mkdir(parents=True, exist_ok=True)
        outside.write_bytes(b"{}")
        res = _make_resource(temp_dir, outside, accel=True)

        headers = [("Content-Length", "2")]
        res.finalize_headers({}, headers)
        assert _xaccel(headers) is None
        assert res.support_ranges() is True
        with res.get_content() as fh:
            assert fh.read() == b"{}"

    def test_traversal_escape_streams_normally(self, temp_dir: Path) -> None:
        # Even if a file_path tries to climb out of the library, no redirect.
        escaped = temp_dir / "library" / ".." / "secret.cbz"
        (temp_dir / "secret.cbz").write_bytes(b"SECRET")
        res = _make_resource(temp_dir, escaped, accel=True)
        headers: list[tuple[str, str]] = []
        res.finalize_headers({}, headers)
        assert _xaccel(headers) is None

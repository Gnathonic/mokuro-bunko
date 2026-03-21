"""Integration tests for WebDAV operations."""

from __future__ import annotations

import base64
import io
import zipfile
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import Config, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.server import create_app


def make_auth_header(username: str, password: str) -> str:
    """Create Basic auth header value."""
    credentials = base64.b64encode(f"{username}:{password}".encode()).decode()
    return f"Basic {credentials}"


def make_valid_cbz_bytes() -> bytes:
    """Build a minimal valid CBZ archive payload."""
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", compression=zipfile.ZIP_STORED) as zf:
        zf.writestr("001.jpg", b"fake image bytes")
    return buffer.getvalue()


class WSGITestClient:
    """Simple WSGI test client."""

    def __init__(self, app: Callable[..., Any]) -> None:
        self.app = app

    def request(
        self,
        method: str,
        path: str,
        headers: dict[str, str] | None = None,
        content: bytes | None = None,
    ) -> WSGIResponse:
        """Make a request to the WSGI app."""
        headers = headers or {}

        environ = {
            "REQUEST_METHOD": method,
            "SCRIPT_NAME": "",
            "PATH_INFO": path,
            "QUERY_STRING": "",
            "SERVER_NAME": "localhost",
            "SERVER_PORT": "8080",
            "SERVER_PROTOCOL": "HTTP/1.1",
            "wsgi.version": (1, 0),
            "wsgi.url_scheme": "http",
            "wsgi.input": io.BytesIO(content or b""),
            "wsgi.errors": io.StringIO(),
            "wsgi.multithread": False,
            "wsgi.multiprocess": False,
            "wsgi.run_once": False,
            "CONTENT_LENGTH": str(len(content or b"")),
            "CONTENT_TYPE": headers.get("Content-Type", "application/octet-stream"),
        }

        # Add headers
        for key, value in headers.items():
            key_upper = key.upper().replace("-", "_")
            if key_upper not in ("CONTENT_TYPE", "CONTENT_LENGTH"):
                environ[f"HTTP_{key_upper}"] = value

        response = WSGIResponse()
        result = self.app(environ, response.start_response)

        # Collect response body
        body_parts = []
        try:
            for chunk in result:
                body_parts.append(chunk)
        finally:
            if hasattr(result, "close"):
                result.close()

        response.content = b"".join(body_parts)
        return response

    def get(
        self, path: str, headers: dict[str, str] | None = None
    ) -> WSGIResponse:
        """Make a GET request."""
        return self.request("GET", path, headers)

    def put(
        self,
        path: str,
        content: bytes,
        headers: dict[str, str] | None = None,
    ) -> WSGIResponse:
        """Make a PUT request."""
        return self.request("PUT", path, headers, content)

    def delete(
        self, path: str, headers: dict[str, str] | None = None
    ) -> WSGIResponse:
        """Make a DELETE request."""
        return self.request("DELETE", path, headers)


class WSGIResponse:
    """WSGI response wrapper."""

    def __init__(self) -> None:
        self.status: str = ""
        self.headers: list[tuple[str, str]] = []
        self.content: bytes = b""

    def start_response(
        self,
        status: str,
        headers: list[tuple[str, str]],
        exc_info: Any = None,
    ) -> Callable[[bytes], None]:
        self.status = status
        self.headers = headers
        return lambda data: None

    @property
    def status_code(self) -> int:
        """Extract status code from status string."""
        return int(self.status.split()[0])

    @property
    def text(self) -> str:
        """Return response body as text."""
        return self.content.decode("utf-8", errors="replace")


@pytest.fixture
def test_storage(temp_dir: Path) -> Path:
    """Create test storage directory with sample files."""
    storage = temp_dir / "storage"

    # Create directories
    (storage / "library").mkdir(parents=True)
    (storage / "library" / "thumbnails").mkdir()
    (storage / "inbox").mkdir()
    (storage / "users").mkdir()

    # Create sample library files
    (storage / "library" / "manga1.cbz").write_bytes(b"fake cbz content 1")
    (storage / "library" / "manga2.cbz").write_bytes(b"fake cbz content 2")
    (storage / "library" / "series").mkdir()
    (storage / "library" / "series" / "vol1.cbz").write_bytes(b"volume 1")

    return storage


@pytest.fixture
def test_db(test_storage: Path) -> Database:
    """Create test database with users."""
    db = Database(test_storage / "mokuro.db")
    db.create_user("reader", "pass1234", "registered")
    db.create_user("uploader", "pass1234", "uploader")
    db.create_user("editor", "pass1234", "editor")
    db.create_user("admin", "pass1234", "admin")

    # Create user directories with progress files
    (test_storage / "users" / "reader").mkdir(parents=True)
    (test_storage / "users" / "uploader").mkdir(parents=True)

    # Add per-user progress files (new format)
    (test_storage / "users" / "reader" / "volume-data.json").write_bytes(b"reader progress")

    return db


@pytest.fixture
def test_config(test_storage: Path) -> Config:
    """Create test configuration."""
    return Config(storage=StorageConfig(base_path=test_storage))


@pytest.fixture
def app(test_config: Config, test_db: Database) -> Any:
    """Create test application."""
    return create_app(test_config)


@pytest.fixture
def client(app: Any) -> WSGITestClient:
    """Create test client."""
    return WSGITestClient(app)


class TestPropfind:
    """Tests for PROPFIND operations."""

    def test_propfind_root(self, client: WSGITestClient) -> None:
        """Test PROPFIND on root returns only mokuro-reader."""
        response = client.request(
            "PROPFIND",
            "/",
            headers={"Depth": "1"},
        )
        assert response.status_code == 207
        content = response.text
        assert "mokuro-reader" in content
        assert "inbox" not in content

    def test_propfind_root_authenticated_shows_progress(
        self, client: WSGITestClient
    ) -> None:
        """Test PROPFIND on mokuro-reader shows user's progress files."""
        response = client.request(
            "PROPFIND",
            "/mokuro-reader",
            headers={
                "Depth": "1",
                "Authorization": make_auth_header("reader", "pass1234"),
            },
        )
        assert response.status_code == 207
        content = response.text
        assert "volume-data.json" in content

    def test_propfind_mokuro_reader(self, client: WSGITestClient) -> None:
        """Test PROPFIND on mokuro-reader returns manga files."""
        response = client.request(
            "PROPFIND",
            "/mokuro-reader",
            headers={"Depth": "1"},
        )
        assert response.status_code == 207
        content = response.text
        assert "manga1.cbz" in content
        assert "manga2.cbz" in content
        assert "series" in content

    def test_propfind_mokuro_reader_nested(self, client: WSGITestClient) -> None:
        """Test PROPFIND on nested folder under mokuro-reader."""
        response = client.request(
            "PROPFIND",
            "/mokuro-reader/series",
            headers={"Depth": "1"},
        )
        assert response.status_code == 207
        content = response.text
        assert "vol1.cbz" in content

    def test_propfind_inbox(self, client: WSGITestClient) -> None:
        """Test PROPFIND on inbox is not exposed."""
        response = client.request(
            "PROPFIND",
            "/inbox",
            headers={"Depth": "1"},
        )
        assert response.status_code == 404


class TestGet:
    """Tests for GET operations."""

    def test_get_library_file(self, client: WSGITestClient) -> None:
        """Test GET file from library via mokuro-reader."""
        response = client.get("/mokuro-reader/manga1.cbz")
        assert response.status_code == 200
        assert response.content == b"fake cbz content 1"

    def test_get_nested_library_file(self, client: WSGITestClient) -> None:
        """Test GET nested file from library via mokuro-reader."""
        response = client.get("/mokuro-reader/series/vol1.cbz")
        assert response.status_code == 200
        assert response.content == b"volume 1"

    def test_get_nonexistent_file(self, client: WSGITestClient) -> None:
        """Test GET nonexistent file returns 404."""
        response = client.get("/mokuro-reader/nonexistent.cbz")
        assert response.status_code == 404

    def test_get_progress_file_authenticated(self, client: WSGITestClient) -> None:
        """Test GET progress file when authenticated."""
        response = client.get(
            "/mokuro-reader/volume-data.json",
            headers={"Authorization": make_auth_header("reader", "pass1234")},
        )
        assert response.status_code == 200
        assert response.content == b"reader progress"


class TestPut:
    """Tests for PUT operations."""

    def test_put_progress_file(self, client: WSGITestClient, test_storage: Path) -> None:
        """Test PUT progress file creates in user directory."""
        response = client.put(
            "/mokuro-reader/volume-data.json",
            content=b"new progress data",
            headers={"Authorization": make_auth_header("reader", "pass1234")},
        )
        assert response.status_code in (200, 201, 204)

        # Verify file was created in user directory
        created_file = test_storage / "users" / "reader" / "volume-data.json"
        assert created_file.exists()
        assert created_file.read_bytes() == b"new progress data"

    def test_put_profiles_file(
        self, client: WSGITestClient, test_storage: Path
    ) -> None:
        """Test PUT profiles.json creates in user directory."""
        response = client.put(
            "/mokuro-reader/profiles.json",
            content=b"profiles data",
            headers={"Authorization": make_auth_header("reader", "pass1234")},
        )
        assert response.status_code in (200, 201, 204)

        created_file = test_storage / "users" / "reader" / "profiles.json"
        assert created_file.exists()
        assert created_file.read_bytes() == b"profiles data"

    def test_put_library_file_as_writer(
        self, client: WSGITestClient, test_storage: Path
    ) -> None:
        """Test PUT file to library as uploader via mokuro-reader."""
        content = make_valid_cbz_bytes()
        response = client.put(
            "/mokuro-reader/new_manga.cbz",
            content=content,
            headers={"Authorization": make_auth_header("uploader", "pass1234")},
        )
        assert response.status_code in (200, 201, 204)

        # Files are physically stored in library/
        created_file = test_storage / "library" / "new_manga.cbz"
        assert created_file.exists()
        assert created_file.read_bytes() == content

    def test_put_library_file_as_registered_denied(
        self, client: WSGITestClient
    ) -> None:
        """Test PUT file to library as registered user is denied."""
        response = client.put(
            "/mokuro-reader/denied.cbz",
            content=b"content",
            headers={"Authorization": make_auth_header("reader", "pass1234")},
        )
        assert response.status_code == 403

    def test_put_inbox_file_not_exposed(self, client: WSGITestClient) -> None:
        """PUT to /inbox is not supported over WebDAV."""
        content = make_valid_cbz_bytes()
        response = client.put(
            "/inbox/upload.cbz",
            content=content,
            headers={"Authorization": make_auth_header("uploader", "pass1234")},
        )
        assert response.status_code in (403, 404)

    def test_put_corrupted_library_cbz_rejected(
        self, client: WSGITestClient, test_storage: Path
    ) -> None:
        """Corrupted CBZ upload should be rejected and not written."""
        response = client.put(
            "/mokuro-reader/corrupt.cbz",
            content=b"not a zip archive",
            headers={"Authorization": make_auth_header("uploader", "pass1234")},
        )
        assert response.status_code == 400
        assert not (test_storage / "library" / "corrupt.cbz").exists()

    def test_put_corrupted_inbox_cbz_not_exposed(self, client: WSGITestClient) -> None:
        """Corrupted CBZ upload to /inbox is rejected because path is not exposed."""
        response = client.put(
            "/inbox/corrupt.cbz",
            content=b"not a zip archive",
            headers={"Authorization": make_auth_header("uploader", "pass1234")},
        )
        assert response.status_code in (403, 404)

    def test_put_library_path_traversal_rejected(
        self, client: WSGITestClient, test_storage: Path
    ) -> None:
        """Traversal attempts in library upload path are rejected."""
        outside = test_storage.parent / "escape-put.txt"
        if outside.exists():
            outside.unlink()

        response = client.put(
            "/mokuro-reader/../../escape-put.txt",
            content=b"escape",
            headers={"Authorization": make_auth_header("uploader", "pass1234")},
        )
        assert response.status_code in (400, 403, 404, 409)
        assert not outside.exists()


class TestDelete:
    """Tests for DELETE operations."""

    def test_delete_library_file_as_editor(
        self, client: WSGITestClient, test_storage: Path
    ) -> None:
        """Test DELETE file from library as editor via mokuro-reader."""
        # Verify file exists first
        file_path = test_storage / "library" / "manga1.cbz"
        assert file_path.exists()

        response = client.delete(
            "/mokuro-reader/manga1.cbz",
            headers={"Authorization": make_auth_header("editor", "pass1234")},
        )
        assert response.status_code in (200, 204)
        assert not file_path.exists()

    def test_delete_library_file_as_writer_denied(
        self, client: WSGITestClient
    ) -> None:
        """Test DELETE file from library as uploader is denied."""
        response = client.delete(
            "/mokuro-reader/manga2.cbz",
            headers={"Authorization": make_auth_header("uploader", "pass1234")},
        )
        assert response.status_code == 403

    def test_uploader_can_delete_own_uploaded_volume_and_sidecars(
        self, client: WSGITestClient, test_storage: Path
    ) -> None:
        """Uploader can delete their own uploaded volume and generated sidecars."""
        upload = client.put(
            "/mokuro-reader/owned.cbz",
            content=make_valid_cbz_bytes(),
            headers={"Authorization": make_auth_header("uploader", "pass1234")},
        )
        assert upload.status_code in (200, 201, 204)

        base = test_storage / "library" / "owned"
        (test_storage / "library" / "owned.mokuro").write_text("{}", encoding="utf-8")
        (Path(str(base) + ".mokuro.gz")).write_bytes(b"gz")
        (test_storage / "library" / "owned.webp").write_bytes(b"thumb")
        (test_storage / "library" / "owned.nocover").write_text("", encoding="utf-8")

        response = client.delete(
            "/mokuro-reader/owned.cbz",
            headers={"Authorization": make_auth_header("uploader", "pass1234")},
        )
        assert response.status_code in (200, 204)
        assert not (base.with_suffix(".cbz")).exists()
        assert not (base.with_suffix(".mokuro")).exists()
        assert not (Path(str(base) + ".mokuro.gz")).exists()
        assert not (base.with_suffix(".webp")).exists()
        assert not (base.with_suffix(".nocover")).exists()

    def test_delete_own_progress_file(
        self, client: WSGITestClient, test_storage: Path
    ) -> None:
        """Test DELETE own progress file."""
        file_path = test_storage / "users" / "reader" / "volume-data.json"
        assert file_path.exists()

        response = client.delete(
            "/mokuro-reader/volume-data.json",
            headers={"Authorization": make_auth_header("reader", "pass1234")},
        )
        assert response.status_code in (200, 204)
        assert not file_path.exists()

    def test_delete_library_path_traversal_rejected(
        self, client: WSGITestClient, test_storage: Path
    ) -> None:
        """Traversal attempts for DELETE in library path are rejected."""
        outside = test_storage.parent / "escape-del.txt"
        outside.write_text("do-not-delete", encoding="utf-8")

        response = client.delete(
            "/mokuro-reader/../../escape-del.txt",
            headers={"Authorization": make_auth_header("editor", "pass1234")},
        )
        assert response.status_code in (400, 403, 404, 409)
        assert outside.exists()


class TestMkcol:
    """Tests for MKCOL (create directory) operations."""

    def test_mkcol_in_library_as_writer(
        self, client: WSGITestClient, test_storage: Path
    ) -> None:
        """Test MKCOL in library as uploader via mokuro-reader."""
        response = client.request(
            "MKCOL",
            "/mokuro-reader/new_series",
            headers={"Authorization": make_auth_header("uploader", "pass1234")},
        )
        assert response.status_code in (200, 201)
        # Physically created in library/
        assert (test_storage / "library" / "new_series").is_dir()

    def test_mkcol_as_registered_denied(self, client: WSGITestClient) -> None:
        """Test MKCOL as registered user is denied."""
        response = client.request(
            "MKCOL",
            "/mokuro-reader/denied_folder",
            headers={"Authorization": make_auth_header("reader", "pass1234")},
        )
        assert response.status_code == 403


class TestUserIsolation:
    """Tests for user data isolation.

    Per-user files (volume-data.json, profiles.json) are accessed via the
    same /mokuro-reader/ path, but each user gets their own copy stored
    in their private directory on disk.
    """

    def test_user_sees_own_progress_in_propfind(
        self, client: WSGITestClient
    ) -> None:
        """Test user sees own progress files in PROPFIND."""
        response = client.request(
            "PROPFIND",
            "/mokuro-reader",
            headers={
                "Depth": "1",
                "Authorization": make_auth_header("reader", "pass1234"),
            },
        )
        assert response.status_code == 207
        content = response.text
        # Should see own volume-data.json
        assert "volume-data.json" in content

    def test_different_users_get_different_progress(
        self, client: WSGITestClient, test_storage: Path
    ) -> None:
        """Test different users get different progress data from same path."""
        # Write progress for uploader user
        (test_storage / "users" / "uploader" / "volume-data.json").write_bytes(
            b"uploader progress"
        )

        # Reader gets reader's progress
        response_reader = client.get(
            "/mokuro-reader/volume-data.json",
            headers={"Authorization": make_auth_header("reader", "pass1234")},
        )
        assert response_reader.status_code == 200
        assert response_reader.content == b"reader progress"

        # Uploader gets uploader's progress
        response_writer = client.get(
            "/mokuro-reader/volume-data.json",
            headers={"Authorization": make_auth_header("uploader", "pass1234")},
        )
        assert response_writer.status_code == 200
        assert response_writer.content == b"uploader progress"


class TestAnonymousAccess:
    """Tests for anonymous access."""

    def test_anonymous_can_propfind_root(self, client: WSGITestClient) -> None:
        """Test anonymous can PROPFIND root."""
        response = client.request("PROPFIND", "/", headers={"Depth": "1"})
        assert response.status_code == 207

    def test_anonymous_can_get_library(self, client: WSGITestClient) -> None:
        """Test anonymous can GET library files via mokuro-reader."""
        response = client.get("/mokuro-reader/manga1.cbz")
        assert response.status_code == 200

    def test_anonymous_cannot_put(self, client: WSGITestClient) -> None:
        """Test anonymous cannot PUT files."""
        response = client.put("/mokuro-reader/anon.cbz", content=b"data")
        assert response.status_code == 401

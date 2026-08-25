"""Contract §5/§6: who may write what under the compiled-metadata rules.

2026-08-24 ruling (overturns this task's original `WRITE_PROGRESS` design):
`series.json` PUT authorization is ownership-gated, mirroring the DELETE-time
precedent `can_user_delete_library_path` already sets for `uploader`.
`registered` never reaches it; `uploader` reaches it only for a series it
fully owns; any role holding `MODIFY_DELETE` (`inviter`/`editor`/`admin` —
see `ROLE_PERMISSIONS`) reaches it for every series.
"""

from __future__ import annotations

from typing import Any

import pytest

from mokuro_bunko.database import Database
from mokuro_bunko.middleware.auth import AuthMiddleware, AuthResult

SERIES_FILE = "/mokuro-reader/Dr Stone/series.json"
OTHER_SERIES_FILE = "/mokuro-reader/Aria/series.json"
CATALOG_FILE = "/mokuro-reader/catalog.json"
ARCHIVE = "/mokuro-reader/Dr Stone/Volume 01.cbz"
COVER = "/mokuro-reader/Dr Stone/Volume 01.webp"


@pytest.fixture
def middleware(temp_db: Database) -> AuthMiddleware:
    def app(environ: dict[str, Any], start_response: Any) -> list[bytes]:
        return [b""]

    return AuthMiddleware(app, temp_db)


def as_role(role: str) -> AuthResult:
    if role == "anonymous":
        return AuthResult(authenticated=False, role="anonymous")
    return AuthResult(
        authenticated=True,
        user={
            "id": 1,
            "username": role,
            "role": role,
            "status": "active",
            "notes": "",
            "created_at": "2026-01-01",
        },
        role=role,
    )


def authorize(middleware: AuthMiddleware, method: str, path: str, role: str) -> tuple[bool, int]:
    result = middleware.authorize(
        {"REQUEST_METHOD": method, "PATH_INFO": path}, as_role(role)
    )
    return result.authorized, result.status_code


class TestSeriesFilePutAnonymousAndModifyDelete:
    def test_anonymous_may_not(self, middleware: AuthMiddleware) -> None:
        assert authorize(middleware, "PUT", SERIES_FILE, "anonymous") == (False, 401)

    @pytest.mark.parametrize("role", ["inviter", "editor", "admin"])
    def test_every_modify_delete_role_may_edit_any_series_unconditionally(
        self, middleware: AuthMiddleware, role: str
    ) -> None:
        """`inviter` holds MODIFY_DELETE too (ROLE_PERMISSIONS) — it counts
        here even though it never uploads. Neither folder has an ownership
        row at all, and it is still authorized."""
        assert authorize(middleware, "PUT", SERIES_FILE, role) == (True, 200)
        assert authorize(middleware, "PUT", OTHER_SERIES_FILE, role) == (True, 200)

    def test_reading_it_is_unaffected(self, middleware: AuthMiddleware) -> None:
        assert authorize(middleware, "GET", SERIES_FILE, "anonymous")[0] is True
        assert authorize(middleware, "PROPFIND", SERIES_FILE, "anonymous")[0] is True


class TestSeriesFilePutRegisteredNeverReaches:
    def test_registered_is_403_for_an_untracked_series(
        self, middleware: AuthMiddleware
    ) -> None:
        assert authorize(middleware, "PUT", SERIES_FILE, "registered") == (False, 403)

    def test_registered_is_403_even_if_it_somehow_owns_the_series(
        self, middleware: AuthMiddleware, temp_db: Database
    ) -> None:
        """Ownership alone is never sufficient for this role: ADD_FILES-tier
        (uploader) or above is required before ownership is even checked."""
        temp_db.record_volume_upload("Dr Stone/Volume 01.cbz", "registered")
        assert authorize(middleware, "PUT", SERIES_FILE, "registered") == (False, 403)


class TestSeriesFilePutUploaderOwnership:
    def test_an_untracked_series_is_403_not_a_free_for_all(
        self, middleware: AuthMiddleware
    ) -> None:
        """No volume_uploads rows for the folder at all: the safe default."""
        assert authorize(middleware, "PUT", SERIES_FILE, "uploader") == (False, 403)

    def test_the_sole_owner_may_edit_its_series(
        self, middleware: AuthMiddleware, temp_db: Database
    ) -> None:
        temp_db.record_volume_upload("Dr Stone/Volume 01.cbz", "uploader")
        assert authorize(middleware, "PUT", SERIES_FILE, "uploader") == (True, 200)

    def test_a_non_owner_uploader_is_403(
        self, middleware: AuthMiddleware, temp_db: Database
    ) -> None:
        temp_db.record_volume_upload("Dr Stone/Volume 01.cbz", "someone-else")
        assert authorize(middleware, "PUT", SERIES_FILE, "uploader") == (False, 403)

    def test_ownership_is_per_series_not_global(
        self, middleware: AuthMiddleware, temp_db: Database
    ) -> None:
        temp_db.record_volume_upload("Dr Stone/Volume 01.cbz", "uploader")
        assert authorize(middleware, "PUT", SERIES_FILE, "uploader") == (True, 200)
        assert authorize(middleware, "PUT", OTHER_SERIES_FILE, "uploader") == (False, 403)

    def test_a_series_with_any_other_owner_is_403_for_this_uploader(
        self, middleware: AuthMiddleware, temp_db: Database
    ) -> None:
        """One volume owned by another user in the same folder blocks the
        whole series for this uploader — not a per-volume grant."""
        temp_db.record_volume_upload("Dr Stone/Volume 01.cbz", "uploader")
        temp_db.record_volume_upload("Dr Stone/Volume 02.cbz", "someone-else")
        assert authorize(middleware, "PUT", SERIES_FILE, "uploader") == (False, 403)


class TestCompiledFilesAreServerOwned:
    @pytest.mark.parametrize("role", ["registered", "uploader", "editor", "admin"])
    def test_nobody_may_put_the_catalog(self, middleware: AuthMiddleware, role: str) -> None:
        assert authorize(middleware, "PUT", CATALOG_FILE, role) == (False, 403)

    @pytest.mark.parametrize("method", ["DELETE", "MOVE", "COPY", "PROPPATCH"])
    @pytest.mark.parametrize("path", [SERIES_FILE, CATALOG_FILE])
    def test_nobody_may_delete_or_move_a_compiled_file(
        self, middleware: AuthMiddleware, method: str, path: str
    ) -> None:
        assert authorize(middleware, method, path, "admin") == (False, 403)

    def test_deleting_the_series_folder_itself_is_still_allowed(
        self, middleware: AuthMiddleware
    ) -> None:
        assert authorize(middleware, "DELETE", "/mokuro-reader/Dr Stone", "editor") == (
            True,
            200,
        )

    def test_a_nested_catalog_json_is_an_ordinary_library_file(
        self, middleware: AuthMiddleware
    ) -> None:
        assert authorize(
            middleware, "PUT", "/mokuro-reader/Dr Stone/catalog.json", "uploader"
        ) == (True, 200)


class TestScopedUsersStillCannotWriteContent:
    @pytest.mark.parametrize("path", [ARCHIVE, COVER])
    def test_a_registered_user_may_not_upload_archives_or_covers(
        self, middleware: AuthMiddleware, path: str
    ) -> None:
        """Contract §5 — already true via ADD_FILES; pinned so it stays true."""
        assert authorize(middleware, "PUT", path, "registered") == (False, 403)

    @pytest.mark.parametrize("path", [ARCHIVE, COVER])
    def test_an_uploader_still_may(self, middleware: AuthMiddleware, path: str) -> None:
        assert authorize(middleware, "PUT", path, "uploader") == (True, 200)

    def test_progress_writes_are_untouched(self, middleware: AuthMiddleware) -> None:
        assert authorize(
            middleware, "PUT", "/mokuro-reader/volume-data.json", "registered"
        ) == (True, 200)

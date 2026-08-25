"""Contract §5/§6: who may write what under the compiled-metadata rules.

2026-08-24 ruling (overturns this task's original `WRITE_PROGRESS` design):
`series.json` PUT authorization is ownership-gated, mirroring the DELETE-time
precedent `can_user_delete_library_path` already sets for `uploader`.
`registered` never reaches it; `uploader` reaches it only for a series it
fully owns; any role holding `MODIFY_DELETE` (`inviter`/`editor`/`admin` —
see `ROLE_PERMISSIONS`) reaches it for every series.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.database import Database
from mokuro_bunko.metadata.service import MetadataService
from mokuro_bunko.middleware.auth import AuthMiddleware, AuthResult

SERIES_FILE = "/mokuro-reader/Dr Stone/series.json"
OTHER_SERIES_FILE = "/mokuro-reader/Aria/series.json"
CATALOG_FILE = "/mokuro-reader/catalog.json"
ARCHIVE = "/mokuro-reader/Dr Stone/Volume 01.cbz"
COVER = "/mokuro-reader/Dr Stone/Volume 01.webp"


def _valid_update_payload(series_title: str) -> bytes:
    """A payload `parse_series_update` actually accepts.

    Task 11 review round 3 (F13): `b'{"version":2,"series_title":...}'` has
    no `updated_at`, so `parse_series_update` rejects it and
    `apply_series_update` returns `False` at that check — several
    statements before `_resolve_folder_title` is ever consulted. Tests that
    want to pin the FOLDER-resolution refusal must use a payload that
    clears validation first, or the refusal they observe is the wrong one.
    """
    return json.dumps(
        {
            "version": 2,
            "series_title": series_title,
            "external_ids": {},
            "titles": {},
            "synonyms": [],
            "updated_at": "2026-08-18T19:36:24.324Z",
            "volumes": [],
        }
    ).encode("utf-8")


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


def authorize_move(
    middleware: AuthMiddleware, method: str, path: str, destination: str, role: str
) -> tuple[bool, int]:
    """Like `authorize`, but for a MOVE/COPY carrying a `Destination` header."""
    result = middleware.authorize(
        {"REQUEST_METHOD": method, "PATH_INFO": path, "HTTP_DESTINATION": destination},
        as_role(role),
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
    @pytest.mark.parametrize("role", ["registered", "uploader", "inviter", "editor", "admin"])
    def test_nobody_may_put_the_catalog(self, middleware: AuthMiddleware, role: str) -> None:
        assert authorize(middleware, "PUT", CATALOG_FILE, role) == (False, 403)

    @pytest.mark.parametrize(
        "method", ["DELETE", "MOVE", "COPY", "PROPPATCH", "MKCOL", "LOCK", "UNLOCK"]
    )
    @pytest.mark.parametrize("path", [SERIES_FILE, CATALOG_FILE])
    @pytest.mark.parametrize("role", ["uploader", "editor", "admin"])
    def test_nobody_may_delete_move_mkcol_or_lock_a_compiled_file(
        self, middleware: AuthMiddleware, method: str, path: str, role: str
    ) -> None:
        """Review round 1 (F3/F8): pinned for uploader (lacks MODIFY_DELETE
        entirely) and a NON-admin MODIFY_DELETE holder (`editor`) too, not
        just `admin` — and MKCOL now belongs in this same gate: it used to
        fall through to the generic ADD_FILES check, letting any uploader
        plant a directory where a sidecar belongs. LOCK/UNLOCK were added by
        the final whole-branch review (F6): they used to fall through to the
        generic MODIFY_DELETE branch, letting `editor`/`admin` successfully
        LOCK a compiled file even though the compiler ignores DAV locks and
        will silently rewrite it anyway."""
        assert authorize(middleware, method, path, role) == (False, 403)

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


class TestCompiledFileDestinationIsAlsoGated:
    """Review round 1 (F4): the request path alone isn't enough — a
    MOVE/COPY whose `Destination` header resolves to a compiled path must
    be refused too, or a MODIFY_DELETE holder can clobber `catalog.json`/
    `series.json` with unvalidated bytes and bypass MetadataAPI entirely."""

    @pytest.mark.parametrize("method", ["MOVE", "COPY"])
    @pytest.mark.parametrize("role", ["editor", "admin"])
    def test_relocating_an_ordinary_file_onto_the_catalog_is_refused(
        self, middleware: AuthMiddleware, method: str, role: str
    ) -> None:
        assert authorize_move(
            middleware, method, "/mokuro-reader/Dr Stone/evil.json", CATALOG_FILE, role
        ) == (False, 403)

    @pytest.mark.parametrize("method", ["MOVE", "COPY"])
    @pytest.mark.parametrize("role", ["editor", "admin"])
    def test_relocating_an_ordinary_file_onto_a_series_file_is_refused(
        self, middleware: AuthMiddleware, method: str, role: str
    ) -> None:
        assert authorize_move(
            middleware, method, "/mokuro-reader/Dr Stone/evil.json", OTHER_SERIES_FILE, role
        ) == (False, 403)

    def test_an_ordinary_move_between_ordinary_paths_is_unaffected(
        self, middleware: AuthMiddleware
    ) -> None:
        assert authorize_move(
            middleware,
            "MOVE",
            "/mokuro-reader/Dr Stone/old.cbz",
            "/mokuro-reader/Dr Stone/new.cbz",
            "editor",
        ) == (True, 200)

    def test_absolute_uri_destination_is_parsed_the_same_way(
        self, middleware: AuthMiddleware
    ) -> None:
        """A real DAV client typically sends a full URI, not a bare path."""
        assert authorize_move(
            middleware,
            "MOVE",
            "/mokuro-reader/Dr Stone/evil.json",
            "http://example.com/mokuro-reader/catalog.json",
            "admin",
        ) == (False, 403)

    def test_malformed_destination_header_does_not_crash_authorize(
        self, middleware: AuthMiddleware
    ) -> None:
        """Review round 2 (N1): an unterminated IPv6 literal makes `urlparse`
        raise `ValueError`. That must not escape `authorize()` as an
        unhandled 500 — it sits above WsgiDAVApp's own error handling, so
        the exception would reach an anonymous client directly. Failing
        open at THIS layer (treat the Destination as unparseable, fall
        through to the ordinary MOVE/COPY check) is safe: wsgidav parses
        the identical header the identical way downstream and will itself
        fail to resolve a real destination from it."""
        assert authorize_move(
            middleware,
            "MOVE",
            "/mokuro-reader/Dr Stone/evil.json",
            "http://[::1/mokuro-reader/catalog.json",
            "editor",
        ) == (True, 200)


class TestCompiledFileWritesRequireAuthNotJustPermission:
    """Review round 1 (F7): an anonymous write to a compiled path must 401
    (with the WWW-Authenticate retry signal), same as every other
    unauthenticated-write branch in this middleware — not a bare 403."""

    @pytest.mark.parametrize(
        "method", ["DELETE", "MOVE", "COPY", "PROPPATCH", "MKCOL", "LOCK", "UNLOCK"]
    )
    def test_anonymous_gets_401_not_403(self, middleware: AuthMiddleware, method: str) -> None:
        assert authorize(middleware, method, SERIES_FILE, "anonymous") == (False, 401)
        assert authorize(middleware, method, CATALOG_FILE, "anonymous") == (False, 401)

    def test_anonymous_put_catalog_gets_401(self, middleware: AuthMiddleware) -> None:
        assert authorize(middleware, "PUT", CATALOG_FILE, "anonymous") == (False, 401)

    def test_anonymous_move_onto_a_compiled_destination_gets_401(
        self, middleware: AuthMiddleware
    ) -> None:
        assert authorize_move(
            middleware, "MOVE", "/mokuro-reader/Dr Stone/evil.json", CATALOG_FILE, "anonymous"
        ) == (False, 401)


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

    @pytest.mark.parametrize("progress_path", ["volume-data.json", "profiles.json"])
    def test_progress_writes_are_untouched(
        self, middleware: AuthMiddleware, progress_path: str
    ) -> None:
        assert authorize(
            middleware, "PUT", f"/mokuro-reader/{progress_path}", "registered"
        ) == (True, 200)


class TestUnknownSeriesPutIsRefusedRegardlessOfWhichRoleAuthorized:
    """Task 11 review round 3 (F9 / N2 residual, controller invariant):
    authorization only decides WHO may REACH the metadata service — it has
    no idea whether the series folder in the URL actually exists, and never
    checks. The service itself is the layer that must refuse to create or
    update a `series_facts` row for a title matching no real folder
    (`MetadataService.apply_series_update`), and that refusal must hold no
    matter which role's PUT got past authorization to reach it: a
    MODIFY_DELETE holder's unconditional grant, or an uploader's ownership
    grant, must not be able to conjure a row for a series that isn't there.
    """

    @pytest.mark.parametrize("role", ["inviter", "editor", "admin"])
    def test_a_modify_delete_holders_put_is_authorized_but_the_service_refuses(
        self, temp_db: Database, tmp_path: Path, role: str
    ) -> None:
        library = tmp_path / "library"
        library.mkdir()
        service = MetadataService(library, temp_db)

        # Auth alone would let this role's PUT through — it never looks at
        # the filesystem.
        middleware = AuthMiddleware(lambda e, s: [b""], temp_db)
        assert authorize(middleware, "PUT", "/mokuro-reader/No Such Series/series.json", role) == (
            True,
            200,
        )

        # But no "No Such Series" folder exists, so the service — the layer
        # `MetadataAPI` actually calls once authorization clears — refuses.
        # A VALID payload (F13): the refusal under test is the folder guard,
        # not `parse_series_update` rejecting a malformed body first.
        accepted = service.apply_series_update(
            "No Such Series", _valid_update_payload("No Such Series"), role
        )
        assert accepted is False
        assert temp_db.get_series_facts("no such series") is None
        assert temp_db.list_series_facts() == []

    def test_an_uploaders_owned_but_deleted_series_is_also_refused(
        self, temp_db: Database, tmp_path: Path
    ) -> None:
        """An uploader can be authorized via ownership (`can_user_edit_series`)
        for a series whose FOLDER was since removed — ownership tracking in
        `volume_uploads` outlives the folder, unlike a `series_facts` row's
        right to be created. The grant is real; the write still isn't."""
        library = tmp_path / "library"
        library.mkdir()
        temp_db.record_volume_upload("Dr Stone/Volume 01.cbz", "uploader")
        service = MetadataService(library, temp_db)

        middleware = AuthMiddleware(lambda e, s: [b""], temp_db)
        assert authorize(middleware, "PUT", SERIES_FILE, "uploader") == (True, 200)

        accepted = service.apply_series_update(
            "Dr Stone", _valid_update_payload("Dr Stone"), "uploader"
        )
        assert accepted is False
        assert temp_db.get_series_facts("dr stone") is None

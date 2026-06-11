"""Tests for role-based permission system."""

from __future__ import annotations

import pytest

from mokuro_bunko.middleware.auth import (
    Permission,
    ROLE_PERMISSIONS,
    check_permission,
    get_role_permissions,
    is_admin_path,
    is_inbox_path,
    is_library_path,
    is_progress_file,
    is_user_progress_path,
    parse_basic_auth,
    parse_basic_auth_checked,
)


class TestRolePermissions:
    """Tests for role permission mappings."""

    def test_anonymous_has_read_only(self) -> None:
        """Test anonymous role has only read permission."""
        perms = get_role_permissions("anonymous")
        assert Permission.READ in perms
        assert Permission.WRITE_PROGRESS not in perms
        assert Permission.ADD_FILES not in perms
        assert Permission.MODIFY_DELETE not in perms
        assert Permission.ADMIN not in perms

    def test_registered_has_read_and_write_progress(self) -> None:
        """Test registered role has read and write progress."""
        perms = get_role_permissions("registered")
        assert Permission.READ in perms
        assert Permission.WRITE_PROGRESS in perms
        assert Permission.ADD_FILES not in perms
        assert Permission.MODIFY_DELETE not in perms
        assert Permission.ADMIN not in perms

    def test_writer_has_add_files(self) -> None:
        """Test uploader role can add files."""
        perms = get_role_permissions("uploader")
        assert Permission.READ in perms
        assert Permission.WRITE_PROGRESS in perms
        assert Permission.ADD_FILES in perms
        assert Permission.MODIFY_DELETE not in perms
        assert Permission.ADMIN not in perms

    def test_editor_has_modify_delete(self) -> None:
        """Test editor role can modify and delete."""
        perms = get_role_permissions("editor")
        assert Permission.READ in perms
        assert Permission.WRITE_PROGRESS in perms
        assert Permission.ADD_FILES in perms
        assert Permission.MODIFY_DELETE in perms
        assert Permission.ADMIN not in perms

    def test_admin_has_all_permissions(self) -> None:
        """Test admin role has all permissions."""
        perms = get_role_permissions("admin")
        assert Permission.READ in perms
        assert Permission.WRITE_PROGRESS in perms
        assert Permission.ADD_FILES in perms
        assert Permission.MODIFY_DELETE in perms
        assert Permission.MANAGE_INVITES in perms
        assert Permission.ADMIN in perms

    def test_inviter_has_invite_management(self) -> None:
        """Test inviter role can manage invites without admin access."""
        perms = get_role_permissions("inviter")
        assert Permission.READ in perms
        assert Permission.WRITE_PROGRESS in perms
        assert Permission.MANAGE_INVITES in perms
        assert Permission.ADMIN not in perms

    def test_unknown_role_has_no_permissions(self) -> None:
        """Test unknown role has no permissions."""
        perms = get_role_permissions("unknown")
        assert len(perms) == 0

    def test_all_roles_defined(self) -> None:
        """Test all expected roles are defined."""
        expected_roles = {"anonymous", "registered", "uploader", "inviter", "editor", "admin"}
        assert set(ROLE_PERMISSIONS.keys()) == expected_roles


class TestCheckPermission:
    """Tests for check_permission function."""

    @pytest.mark.parametrize(
        "role,permission,expected",
        [
            # Anonymous
            ("anonymous", Permission.READ, True),
            ("anonymous", Permission.WRITE_PROGRESS, False),
            ("anonymous", Permission.ADD_FILES, False),
            ("anonymous", Permission.MODIFY_DELETE, False),
            ("anonymous", Permission.ADMIN, False),
            # Registered
            ("registered", Permission.READ, True),
            ("registered", Permission.WRITE_PROGRESS, True),
            ("registered", Permission.ADD_FILES, False),
            ("registered", Permission.MODIFY_DELETE, False),
            ("registered", Permission.ADMIN, False),
            # Uploader
            ("uploader", Permission.READ, True),
            ("uploader", Permission.WRITE_PROGRESS, True),
            ("uploader", Permission.ADD_FILES, True),
            ("uploader", Permission.MODIFY_DELETE, False),
            ("uploader", Permission.ADMIN, False),
            # Inviter
            ("inviter", Permission.READ, True),
            ("inviter", Permission.WRITE_PROGRESS, True),
            ("inviter", Permission.ADD_FILES, True),
            ("inviter", Permission.MODIFY_DELETE, True),
            ("inviter", Permission.MANAGE_INVITES, True),
            ("inviter", Permission.ADMIN, False),
            # Editor
            ("editor", Permission.READ, True),
            ("editor", Permission.WRITE_PROGRESS, True),
            ("editor", Permission.ADD_FILES, True),
            ("editor", Permission.MODIFY_DELETE, True),
            ("editor", Permission.ADMIN, False),
            # Admin
            ("admin", Permission.READ, True),
            ("admin", Permission.WRITE_PROGRESS, True),
            ("admin", Permission.ADD_FILES, True),
            ("admin", Permission.MODIFY_DELETE, True),
            ("admin", Permission.ADMIN, True),
        ],
    )
    def test_permission_matrix(
        self, role: str, permission: Permission, expected: bool
    ) -> None:
        """Test complete permission matrix."""
        assert check_permission(role, permission) == expected


class TestPathHelpers:
    """Tests for path helper functions."""

    class TestIsProgressFile:
        """Tests for is_progress_file."""

        def test_per_user_files_are_progress(self) -> None:
            """Test volume-data.json and profiles.json are progress files."""
            assert is_progress_file("/mokuro-reader/volume-data.json") is True
            assert is_progress_file("/mokuro-reader/profiles.json") is True

        def test_library_files_not_progress(self) -> None:
            """Test library files under /mokuro-reader/ are not progress files."""
            assert is_progress_file("/mokuro-reader/manga.cbz") is False
            assert is_progress_file("/mokuro-reader/series/vol1.cbz") is False

        def test_other_paths_not_progress(self) -> None:
            """Test paths outside /mokuro-reader/ are not progress files."""
            assert is_progress_file("/inbox/file.cbz") is False
            assert is_progress_file("/volume-data.json") is False
            assert is_progress_file("/") is False

    class TestIsUserProgressPath:
        """Tests for is_user_progress_path."""

        def test_per_user_files_belong_to_any_user(self) -> None:
            """Test per-user files under /mokuro-reader/ belong to the current user."""
            assert is_user_progress_path("/mokuro-reader/volume-data.json", "alice") is True
            assert is_user_progress_path("/mokuro-reader/profiles.json", "alice") is True
            assert is_user_progress_path("/mokuro-reader/volume-data.json", "bob") is True

        def test_library_path_not_user_progress(self) -> None:
            """Test library paths are not user progress."""
            assert is_user_progress_path("/mokuro-reader/manga.cbz", "alice") is False
            assert is_user_progress_path("/mokuro-reader/series/vol1.cbz", "alice") is False

        def test_non_reader_paths_not_user_progress(self) -> None:
            """Test non-reader paths are not user progress."""
            assert is_user_progress_path("/inbox/file.cbz", "alice") is False
            assert is_user_progress_path("/", "alice") is False

    class TestIsLibraryPath:
        """Tests for is_library_path."""

        def test_library_paths(self) -> None:
            """Test library paths under /mokuro-reader/."""
            assert is_library_path("/mokuro-reader/manga.cbz") is True
            assert is_library_path("/mokuro-reader/series/vol1.cbz") is True
            assert is_library_path("/mokuro-reader/series") is True

        def test_per_user_files_not_library(self) -> None:
            """Test per-user files are not library paths."""
            assert is_library_path("/mokuro-reader/volume-data.json") is False
            assert is_library_path("/mokuro-reader/profiles.json") is False

        def test_non_reader_paths_not_library(self) -> None:
            """Test paths outside /mokuro-reader/ are not library paths."""
            assert is_library_path("/") is False
            assert is_library_path("/inbox") is False
            assert is_library_path("/mokuro-reader") is False
            assert is_library_path("/library/manga.cbz") is False

    class TestIsInboxPath:
        """Tests for is_inbox_path."""

        def test_inbox_paths(self) -> None:
            """Test inbox paths."""
            assert is_inbox_path("/inbox") is True
            assert is_inbox_path("/inbox/") is True
            assert is_inbox_path("/inbox/file.cbz") is True

        def test_non_inbox_paths(self) -> None:
            """Test non-inbox paths."""
            assert is_inbox_path("/") is False
            assert is_inbox_path("/mokuro-reader/manga.cbz") is False
            assert is_inbox_path("/inboxfake") is False

    class TestIsAdminPath:
        """Tests for is_admin_path."""

        def test_admin_paths(self) -> None:
            """Test admin paths."""
            assert is_admin_path("/_admin") is True
            assert is_admin_path("/_admin/") is True
            assert is_admin_path("/_admin/users") is True
            assert is_admin_path("/_admin/api/users") is True

        def test_non_admin_paths(self) -> None:
            """Test non-admin paths."""
            assert is_admin_path("/") is False
            assert is_admin_path("/admin") is False
            assert is_admin_path("/mokuro-reader") is False


class TestParseBasicAuth:
    """Tests for parse_basic_auth function."""

    def test_valid_basic_auth(self) -> None:
        """Test parsing valid Basic auth header."""
        import base64

        credentials = base64.b64encode(b"alice:password123").decode()
        header = f"Basic {credentials}"

        username, password = parse_basic_auth(header)
        assert username == "alice"
        assert password == "password123"

    def test_password_with_colon(self) -> None:
        """Test password containing colon."""
        import base64

        credentials = base64.b64encode(b"alice:pass:word:123").decode()
        header = f"Basic {credentials}"

        username, password = parse_basic_auth(header)
        assert username == "alice"
        assert password == "pass:word:123"

    def test_empty_header(self) -> None:
        """Test empty header."""
        username, password = parse_basic_auth(None)
        assert username is None
        assert password is None

        username, password = parse_basic_auth("")
        assert username is None
        assert password is None

    def test_non_basic_auth(self) -> None:
        """Test non-Basic auth header."""
        username, password = parse_basic_auth("Bearer token123")
        assert username is None
        assert password is None

    def test_invalid_base64(self) -> None:
        """Test invalid base64 encoding."""
        username, password = parse_basic_auth("Basic !!!invalid!!!")
        assert username is None
        assert password is None

    def test_missing_colon(self) -> None:
        """Test credentials without colon."""
        import base64

        credentials = base64.b64encode(b"usernameonly").decode()
        header = f"Basic {credentials}"

        username, password = parse_basic_auth(header)
        assert username is None
        assert password is None

    def test_empty_username(self) -> None:
        """Test empty username."""
        import base64

        credentials = base64.b64encode(b":password").decode()
        header = f"Basic {credentials}"

        username, password = parse_basic_auth(header)
        assert username == ""
        assert password == "password"

    def test_empty_password(self) -> None:
        """Test empty password."""
        import base64

        credentials = base64.b64encode(b"alice:").decode()
        header = f"Basic {credentials}"

        username, password = parse_basic_auth(header)
        assert username == "alice"
        assert password == ""


class TestParseBasicAuthChecked:
    """Tests for parse_basic_auth_checked (UTF-8-only, absent vs malformed).

    Precomputed vectors (verified):
    - 'user:päss' UTF-8 -> Basic dXNlcjpww6Rzcw==; Latin-1 -> Basic dXNlcjpw5HNz
    - 'user:pässwörd' UTF-8 -> Basic dXNlcjpww6Rzc3fDtnJk; Latin-1 -> Basic dXNlcjpw5HNzd/ZyZA==
    """

    def test_none_and_empty_header(self) -> None:
        """No header at all -> no creds, no error (anonymous)."""
        assert parse_basic_auth_checked(None) == (None, None)
        assert parse_basic_auth_checked("") == (None, None)

    def test_non_basic_scheme_is_anonymous(self) -> None:
        """Non-Basic schemes are intentionally anonymous, not an error."""
        assert parse_basic_auth_checked("Bearer token123") == (None, None)

    def test_ascii_credentials(self) -> None:
        """Pure-ASCII credentials parse."""
        import base64

        header = "Basic " + base64.b64encode(b"alice:password123").decode()
        creds, error = parse_basic_auth_checked(header)
        assert error is None
        assert creds == ("alice", "password123")

    def test_utf8_nonascii_credentials(self) -> None:
        """Valid UTF-8 with non-ASCII bytes parses as UTF-8 only."""
        creds, error = parse_basic_auth_checked("Basic dXNlcjpww6Rzcw==")
        assert error is None
        assert creds == ("user", "päss")

    def test_latin1_bytes_are_an_error(self) -> None:
        """UTF-8-undecodable (Latin-1) bytes are malformed -> error, not anonymous.

        Legacy Latin-1 clients are NOT supported: they get a 401 with a
        charset="UTF-8" challenge instead of silently degrading to anonymous.
        """
        creds, error = parse_basic_auth_checked("Basic dXNlcjpw5HNz")
        assert creds is None
        assert error == "Invalid authorization header"

    def test_invalid_base64_returns_error(self) -> None:
        """Present-but-undecodable base64 is an error, not anonymous."""
        creds, error = parse_basic_auth_checked("Basic !!!notb64!!!")
        assert creds is None
        assert error == "Invalid authorization header"

    def test_no_colon_returns_error(self) -> None:
        """Decodable payload without a colon is an error."""
        import base64

        header = "Basic " + base64.b64encode(b"usernameonly").decode()
        creds, error = parse_basic_auth_checked(header)
        assert creds is None
        assert error == "Invalid authorization header"

    def test_empty_payload_returns_error(self) -> None:
        """'Basic ' with empty payload is an error."""
        creds, error = parse_basic_auth_checked("Basic ")
        assert creds is None
        assert error == "Invalid authorization header"

    def test_colon_in_password_preserved(self) -> None:
        """Only the first colon splits username from password."""
        import base64

        header = "Basic " + base64.b64encode(b"alice:pa:ss").decode()
        creds, error = parse_basic_auth_checked(header)
        assert error is None
        assert creds == ("alice", "pa:ss")

    def test_full_latin1_password_vector_is_error(self) -> None:
        """'user:pässwörd' Latin-1 header is malformed -> error."""
        creds, error = parse_basic_auth_checked("Basic dXNlcjpw5HNzd/ZyZA==")
        assert creds is None
        assert error == "Invalid authorization header"

    def test_full_utf8_password_vector(self) -> None:
        """'user:pässwörd' UTF-8 header parses."""
        creds, error = parse_basic_auth_checked("Basic dXNlcjpww6Rzc3fDtnJk")
        assert error is None
        assert creds == ("user", "pässwörd")


class TestPermissionHierarchy:
    """Tests for permission hierarchy and inheritance."""

    def test_higher_roles_have_lower_permissions(self) -> None:
        """Test that higher roles include permissions of lower roles."""
        # registered > anonymous
        anon_perms = get_role_permissions("anonymous")
        reg_perms = get_role_permissions("registered")
        assert anon_perms.issubset(reg_perms)

        # uploader > registered
        uploader_perms = get_role_permissions("uploader")
        assert reg_perms.issubset(uploader_perms)

        # editor > uploader
        editor_perms = get_role_permissions("editor")
        assert uploader_perms.issubset(editor_perms)

        # inviter > editor
        inviter_perms = get_role_permissions("inviter")
        assert editor_perms.issubset(inviter_perms)

        # admin > inviter
        admin_perms = get_role_permissions("admin")
        assert inviter_perms.issubset(admin_perms)

    def test_only_admin_has_admin_permission(self) -> None:
        """Test only admin role has ADMIN permission."""
        for role in ["anonymous", "registered", "uploader", "inviter", "editor"]:
            assert not check_permission(role, Permission.ADMIN)
        assert check_permission("admin", Permission.ADMIN)

"""The `processor` role: read the library, post events, nothing else."""

from __future__ import annotations

import base64
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.database import (
    INVITABLE_ROLES,
    VALID_ROLES,
    Database,
    normalize_role,
)
from mokuro_bunko.middleware import auth as auth_module
from mokuro_bunko.middleware.auth import (
    AuthMiddleware,
    AuthResult,
    Permission,
    check_permission,
    is_processor_path,
)
from mokuro_bunko.security import AuthAttemptLimiter

PASSWORD = "a-long-enough-password"


def _basic(username: str, password: str) -> str:
    """A Basic auth header value, as a real client would send it."""
    return "Basic " + base64.b64encode(f"{username}:{password}".encode()).decode()


@pytest.fixture
def database(tmp_path: Path) -> Database:
    return Database(tmp_path / "mokuro.db")


def test_processor_is_a_real_role() -> None:
    assert "processor" in VALID_ROLES
    assert normalize_role("processor") == "processor"


def test_an_account_can_be_created_with_it(database: Database) -> None:
    database.create_user("tower", PASSWORD, "processor")
    user = database.authenticate_user("tower", PASSWORD)
    assert user is not None
    assert user["role"] == "processor"


def test_it_can_read_and_process_and_nothing_else() -> None:
    assert check_permission("processor", Permission.READ)
    assert check_permission("processor", Permission.PROCESS)
    for denied in (
        Permission.WRITE_PROGRESS,
        Permission.ADD_FILES,
        Permission.MODIFY_DELETE,
        Permission.MANAGE_INVITES,
        Permission.ADMIN,
    ):
        assert not check_permission("processor", denied), denied


def test_no_other_role_gains_process() -> None:
    for role in ("anonymous", "registered", "uploader", "inviter", "editor", "admin"):
        assert not check_permission(role, Permission.PROCESS), role


def test_the_path_predicate_covers_the_whole_prefix_and_nothing_near_it() -> None:
    assert is_processor_path("/_processor")
    assert is_processor_path("/_processor/register")
    assert is_processor_path("/_processor/p1/stream")
    assert not is_processor_path("/_processors")
    assert not is_processor_path("/mokuro-reader/_processor/x.cbz")


def _user(role: str) -> dict[str, Any]:
    return {"id": 1, "username": "tower", "role": role, "status": "active",
            "notes": "", "created_at": ""}


def _authorize(middleware: AuthMiddleware, method: str, path: str, role: str) -> Any:
    result = AuthResult(
        authenticated=role != "anonymous",
        user=_user(role) if role != "anonymous" else None,
        role=role,
    )
    return middleware.authorize({"REQUEST_METHOD": method, "PATH_INFO": path}, result)


def test_a_processor_may_post_to_its_own_prefix_and_an_editor_may_not(
    database: Database,
) -> None:
    middleware = AuthMiddleware(lambda e, s: [b""], database)
    assert _authorize(middleware, "POST", "/_processor/register", "processor").authorized
    assert _authorize(middleware, "GET", "/_processor/p1/stream", "processor").authorized

    denied = _authorize(middleware, "POST", "/_processor/register", "editor")
    assert not denied.authorized
    assert denied.status_code == 403

    anonymous = _authorize(middleware, "POST", "/_processor/register", "anonymous")
    assert not anonymous.authorized
    assert anonymous.status_code == 401


def test_a_processor_cannot_write_a_library_file_or_reach_the_admin_api(
    database: Database,
) -> None:
    middleware = AuthMiddleware(lambda e, s: [b""], database)
    assert not _authorize(
        middleware, "PUT", "/mokuro-reader/Alpha/Volume 1.cbz", "processor"
    ).authorized
    assert not _authorize(middleware, "GET", "/_admin/api/users", "processor").authorized


def test_a_processor_may_read_a_library_archive(database: Database) -> None:
    middleware = AuthMiddleware(lambda e, s: [b""], database)
    assert _authorize(
        middleware, "GET", "/mokuro-reader/Alpha/Volume 1.cbz", "processor"
    ).authorized


class TestTheRefusedLoginHook:
    """A wrong password never reaches ProcessorAPI, so it is recorded here."""

    @staticmethod
    def _request(database: Database, path: str, password: str) -> list[tuple[str, str]]:
        import base64

        seen: list[tuple[str, str]] = []
        middleware = AuthMiddleware(
            lambda e, s: [b""],
            database,
            on_processor_login_refused=lambda user, ip: seen.append((user, ip)),
        )
        token = base64.b64encode(f"tower:{password}".encode()).decode()
        middleware(
            {
                "REQUEST_METHOD": "POST",
                "PATH_INFO": path,
                "HTTP_AUTHORIZATION": f"Basic {token}",
                "REMOTE_ADDR": "10.0.0.7",
            },
            lambda status, headers: None,
        )
        return seen

    def test_a_wrong_password_on_a_processor_path_is_reported(
        self, database: Database
    ) -> None:
        database.create_user("tower", PASSWORD, "processor")
        assert self._request(database, "/_processor/register", "wrong-password-here") == [
            ("tower", "10.0.0.7")
        ]

    def test_a_right_password_is_not_reported(self, database: Database) -> None:
        database.create_user("tower", PASSWORD, "processor")
        assert self._request(database, "/_processor/register", PASSWORD) == []

    def test_a_failure_somewhere_else_is_not_a_processor_login(
        self, database: Database
    ) -> None:
        database.create_user("tower", PASSWORD, "processor")
        assert self._request(database, "/_admin/api/users", "wrong-password-here") == []

    # --- the 429 arm -----------------------------------------------------
    #
    # Once the limiter blocks a key, `authenticate` returns BEFORE the
    # password is ever checked, on a different branch with a different
    # status. A processor whose credentials drifted hammers the endpoint and
    # lands here, so this is exactly the case the admin panel must show —
    # and it is a separate code path from the 401 above, not a variation of
    # it. Both tests install a private limiter: AUTH_RATE_LIMITER is a
    # module global shared by every test in the process, and these two
    # deliberately drive a key into its blocked state.

    @staticmethod
    def _refuse_repeatedly(
        database: Database, path: str, password: str, attempts: int
    ) -> tuple[list[tuple[str, str]], list[int]]:
        seen: list[tuple[str, str]] = []
        statuses: list[int] = []
        middleware = AuthMiddleware(
            lambda e, s: [b""],
            database,
            on_processor_login_refused=lambda user, ip: seen.append((user, ip)),
        )
        for _ in range(attempts):
            middleware(
                {
                    "REQUEST_METHOD": "POST",
                    "PATH_INFO": path,
                    "HTTP_AUTHORIZATION": _basic("tower", password),
                    "REMOTE_ADDR": "10.0.0.7",
                },
                lambda status, headers: statuses.append(int(status.split()[0])),
            )
        return seen, statuses

    def test_a_rate_limited_refusal_is_reported_too(
        self, database: Database, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        database.create_user("tower", PASSWORD, "processor")
        monkeypatch.setattr(
            auth_module,
            "AUTH_RATE_LIMITER",
            AuthAttemptLimiter(max_failures=2, window_seconds=300, block_seconds=900),
        )

        seen, statuses = self._refuse_repeatedly(
            database, "/_processor/register", "wrong-password-here", attempts=4
        )

        # The first two are ordinary bad-credential refusals; the limiter
        # then blocks the key and the rest never reach the password check.
        assert statuses == [401, 401, 429, 429]
        # Every refusal reached the listener, the rate-limited ones included.
        assert seen == [("tower", "10.0.0.7")] * 4

    def test_the_rate_limited_result_carries_the_attempted_name(
        self, database: Database, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """`attempted_username` is what the hook reports, so pin it directly."""
        database.create_user("tower", PASSWORD, "processor")
        monkeypatch.setattr(
            auth_module,
            "AUTH_RATE_LIMITER",
            AuthAttemptLimiter(max_failures=1, window_seconds=300, block_seconds=900),
        )
        middleware = AuthMiddleware(lambda e, s: [b""], database)
        environ = {
            "REQUEST_METHOD": "POST",
            "PATH_INFO": "/_processor/register",
            "HTTP_AUTHORIZATION": _basic("tower", "wrong-password-here"),
            "REMOTE_ADDR": "10.0.0.7",
        }

        refused = middleware.authenticate(dict(environ))
        assert refused.error == "Invalid credentials"
        assert refused.attempted_username == "tower"

        blocked = middleware.authenticate(dict(environ))
        assert blocked.error is not None
        assert "Too many failed attempts" in blocked.error
        assert blocked.attempted_username == "tower"
        # `username` stays None on a refusal — it reads the user row, which
        # a failed login never produces. The two fields are not redundant.
        assert blocked.username is None


class TestTheInviteRoster:
    """A processor is created deliberately by an admin, never redeemed into.

    The admin API and the CLI each present a four-role menu, but a menu is
    a caller-side courtesy: `create_invite` itself is what makes the rule
    hold for every caller, including `registration/invites.py` and any
    future one.
    """

    def test_an_invite_can_never_mint_a_processor(self, database: Database) -> None:
        with pytest.raises(ValueError, match="invite"):
            database.create_invite(role="processor")

    def test_an_invite_can_never_mint_an_admin_either(self, database: Database) -> None:
        with pytest.raises(ValueError, match="invite"):
            database.create_invite(role="admin")

    def test_the_four_invitable_roles_still_work(self, database: Database) -> None:
        assert INVITABLE_ROLES == {"registered", "uploader", "inviter", "editor"}
        for role in sorted(INVITABLE_ROLES):
            code = database.create_invite(role=role)  # type: ignore[arg-type]
            invite = database.get_invite(code)
            assert invite is not None
            assert invite["role"] == role

    def test_the_roster_is_tested_after_normalization(self, database: Database) -> None:
        """A legacy `writer` invite still works: it normalizes to uploader."""
        code = database.create_invite(role="writer")  # type: ignore[arg-type]
        invite = database.get_invite(code)
        assert invite is not None
        assert invite["role"] == "uploader"

    def test_the_invitable_roles_are_a_subset_of_the_real_ones(self) -> None:
        assert INVITABLE_ROLES < VALID_ROLES
        assert "processor" not in INVITABLE_ROLES

"""Integration tests for admin CLI commands."""

from __future__ import annotations

from pathlib import Path

import pytest
from click.testing import CliRunner

from mokuro_bunko.__main__ import cli
from mokuro_bunko.config import Config, StorageConfig, save_config
from mokuro_bunko.database import Database


@pytest.fixture
def test_storage(temp_dir: Path) -> Path:
    """Create test storage directory."""
    storage = temp_dir / "storage"
    (storage / "library").mkdir(parents=True)
    (storage / "inbox").mkdir()
    (storage / "users").mkdir()
    return storage


@pytest.fixture
def test_config(temp_dir: Path, test_storage: Path) -> Path:
    """Create test configuration file."""
    config_path = temp_dir / "config.yaml"
    config = Config(storage=StorageConfig(base_path=test_storage))
    save_config(config, config_path)
    return config_path


@pytest.fixture
def test_db(test_storage: Path) -> Database:
    """Create test database."""
    return Database(test_storage / "mokuro.db")


@pytest.fixture
def runner() -> CliRunner:
    """Create CLI runner."""
    return CliRunner()


class TestAddUser:
    """Tests for add-user command."""

    def test_add_user_success(
        self, runner: CliRunner, test_config: Path, test_db: Database
    ) -> None:
        """Test adding a user successfully."""
        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "add-user", "testuser", "--password", "pass1234"],
            input="pass1234\n",  # Confirmation
        )
        assert result.exit_code == 0
        assert "created" in result.output

        user = test_db.get_user("testuser")
        assert user is not None
        assert user["role"] == "registered"

    def test_add_user_with_role(
        self, runner: CliRunner, test_config: Path, test_db: Database
    ) -> None:
        """Test adding user with specific role."""
        result = runner.invoke(
            cli,
            [
                "-c", str(test_config),
                "admin", "add-user", "writer1",
                "--role", "uploader",
                "--password", "pass1234",
            ],
            input="pass1234\n",
        )
        assert result.exit_code == 0

        user = test_db.get_user("writer1")
        assert user is not None
        assert user["role"] == "uploader"

    def test_add_user_duplicate(
        self, runner: CliRunner, test_config: Path, test_db: Database
    ) -> None:
        """Test adding duplicate user fails."""
        # Create user first
        test_db.create_user("existing", "pass1234")

        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "add-user", "existing", "--password", "newpass"],
            input="newpass\n",
        )
        assert result.exit_code == 1
        assert "Error" in result.output

    def test_add_user_help(self, runner: CliRunner) -> None:
        """Test add-user help."""
        result = runner.invoke(cli, ["admin", "add-user", "--help"])
        assert result.exit_code == 0
        assert "--role" in result.output
        assert "--password" in result.output


class TestDeleteUser:
    """Tests for delete-user command."""

    def test_delete_user_success(
        self, runner: CliRunner, test_config: Path, test_db: Database
    ) -> None:
        """Test deleting a user successfully."""
        test_db.create_user("todelete", "pass1234")

        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "delete-user", "todelete", "--yes"],
        )
        assert result.exit_code == 0
        assert "deleted" in result.output

        user = test_db.get_user("todelete")
        assert user is not None
        assert user["status"] == "deleted"

    def test_delete_user_not_found(
        self, runner: CliRunner, test_config: Path
    ) -> None:
        """Test deleting nonexistent user fails."""
        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "delete-user", "nonexistent", "--yes"],
        )
        assert result.exit_code == 1
        assert "not found" in result.output

    def test_delete_user_confirmation(
        self, runner: CliRunner, test_config: Path, test_db: Database
    ) -> None:
        """Test delete confirmation."""
        test_db.create_user("confirm_delete", "pass1234")

        # Abort confirmation
        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "delete-user", "confirm_delete"],
            input="n\n",
        )
        assert result.exit_code == 1  # Aborted

        # User still exists
        user = test_db.get_user("confirm_delete")
        assert user is not None


class TestListUsers:
    """Tests for list-users command."""

    def test_list_users_empty(
        self, runner: CliRunner, test_config: Path
    ) -> None:
        """Test listing with no users."""
        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "list-users"],
        )
        assert result.exit_code == 0
        assert "No users found" in result.output

    def test_list_users_shows_all(
        self, runner: CliRunner, test_config: Path, test_db: Database
    ) -> None:
        """Test listing shows all users."""
        test_db.create_user("user1", "pass1234", role="registered")
        test_db.create_user("user2", "pass1234", role="uploader")
        test_db.create_user("admin1", "pass1234", role="admin")

        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "list-users"],
        )
        assert result.exit_code == 0
        assert "user1" in result.output
        assert "user2" in result.output
        assert "admin1" in result.output
        assert "registered" in result.output
        assert "uploader" in result.output
        assert "admin" in result.output

    def test_list_users_shows_status(
        self, runner: CliRunner, test_config: Path, test_db: Database
    ) -> None:
        """Test listing shows role and status."""
        test_db.create_user("active_user", "pass1234", status="active")
        test_db.create_user("pending_user", "pass1234", status="pending")

        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "list-users"],
        )
        assert result.exit_code == 0
        assert "active" in result.output
        assert "pending" in result.output

    def test_list_users_filter_status(
        self, runner: CliRunner, test_config: Path, test_db: Database
    ) -> None:
        """Test listing with status filter."""
        test_db.create_user("active1", "pass1234", status="active")
        test_db.create_user("pending1", "pass1234", status="pending")

        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "list-users", "--status", "pending"],
        )
        assert result.exit_code == 0
        assert "pending1" in result.output
        assert "active1" not in result.output


class TestChangeRole:
    """Tests for change-role command."""

    def test_change_role_success(
        self, runner: CliRunner, test_config: Path, test_db: Database
    ) -> None:
        """Test changing role successfully."""
        test_db.create_user("rolechange", "pass1234", role="registered")

        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "change-role", "rolechange", "editor"],
        )
        assert result.exit_code == 0
        assert "changed to 'editor'" in result.output

        user = test_db.get_user("rolechange")
        assert user is not None
        assert user["role"] == "editor"

    def test_change_role_not_found(
        self, runner: CliRunner, test_config: Path
    ) -> None:
        """Test changing role for nonexistent user fails."""
        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "change-role", "nonexistent", "admin"],
        )
        assert result.exit_code == 1
        assert "not found" in result.output


class TestGenerateInvite:
    """Tests for generate-invite command."""

    def test_generate_invite_success(
        self, runner: CliRunner, test_config: Path, test_db: Database
    ) -> None:
        """Test generating invite code."""
        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "generate-invite"],
        )
        assert result.exit_code == 0
        assert "Invite code:" in result.output
        assert "Role: registered" in result.output

    def test_generate_invite_with_role(
        self, runner: CliRunner, test_config: Path
    ) -> None:
        """Test generating invite with role."""
        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "generate-invite", "--role", "uploader"],
        )
        assert result.exit_code == 0
        assert "Role: uploader" in result.output

    def test_generate_invite_with_expiry(
        self, runner: CliRunner, test_config: Path
    ) -> None:
        """Test generating invite with expiry."""
        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "generate-invite", "--expires", "1d"],
        )
        assert result.exit_code == 0
        assert "Expires in: 1d" in result.output

    def test_generate_invite_creates_in_db(
        self, runner: CliRunner, test_config: Path, test_db: Database
    ) -> None:
        """Test generated invite exists in database."""
        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "generate-invite"],
        )
        assert result.exit_code == 0

        # Extract code from output
        for line in result.output.split("\n"):
            if line.startswith("Invite code:"):
                code = line.split(":")[1].strip()
                break

        invite = test_db.get_invite(code)
        assert invite is not None


class TestListInvites:
    """Tests for list-invites command."""

    def test_list_invites_empty(
        self, runner: CliRunner, test_config: Path
    ) -> None:
        """Test listing with no invites."""
        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "list-invites"],
        )
        assert result.exit_code == 0
        assert "No invites found" in result.output

    def test_list_invites_shows_all(
        self, runner: CliRunner, test_config: Path, test_db: Database
    ) -> None:
        """Test listing shows active invites."""
        code1 = test_db.create_invite(role="registered")
        test_db.create_invite(role="uploader")

        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "list-invites"],
        )
        assert result.exit_code == 0
        assert code1[:20] in result.output  # Codes may be truncated
        assert "registered" in result.output
        assert "uploader" in result.output


class TestDeleteInvite:
    """Tests for delete-invite command."""

    def test_delete_invite_success(
        self, runner: CliRunner, test_config: Path, test_db: Database
    ) -> None:
        """Test deleting an invite."""
        code = test_db.create_invite()

        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "delete-invite", code],
        )
        assert result.exit_code == 0
        assert "deleted" in result.output

        invite = test_db.get_invite(code)
        assert invite is None

    def test_delete_invite_not_found(
        self, runner: CliRunner, test_config: Path
    ) -> None:
        """Test deleting nonexistent invite fails."""
        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "delete-invite", "nonexistent"],
        )
        assert result.exit_code == 1
        assert "not found" in result.output


class TestApproveUser:
    """Tests for approve-user command."""

    def test_approve_user_success(
        self, runner: CliRunner, test_config: Path, test_db: Database
    ) -> None:
        """Test approving a pending user."""
        test_db.create_user("pending", "pass1234", status="pending")

        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "approve-user", "pending"],
        )
        assert result.exit_code == 0
        assert "approved" in result.output

        user = test_db.get_user("pending")
        assert user is not None
        assert user["status"] == "active"

    def test_approve_user_not_found(
        self, runner: CliRunner, test_config: Path
    ) -> None:
        """Test approving nonexistent user fails."""
        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "approve-user", "nonexistent"],
        )
        assert result.exit_code == 1
        assert "not found" in result.output

    def test_approve_user_already_active(
        self, runner: CliRunner, test_config: Path, test_db: Database
    ) -> None:
        """Test approving already active user fails."""
        test_db.create_user("active", "pass1234", status="active")

        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "approve-user", "active"],
        )
        assert result.exit_code == 1
        assert "not pending" in result.output


class TestDisableUser:
    """Tests for disable-user command."""

    def test_disable_user_success(
        self, runner: CliRunner, test_config: Path, test_db: Database
    ) -> None:
        """Test disabling a user."""
        test_db.create_user("todisable", "pass1234")

        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "disable-user", "todisable"],
        )
        assert result.exit_code == 0
        assert "disabled" in result.output

        user = test_db.get_user("todisable")
        assert user is not None
        assert user["status"] == "disabled"

    def test_disable_user_not_found(
        self, runner: CliRunner, test_config: Path
    ) -> None:
        """Test disabling nonexistent user fails."""
        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "disable-user", "nonexistent"],
        )
        assert result.exit_code == 1
        assert "not found" in result.output


class TestSetPassword:
    """Tests for set-password command."""

    def test_set_password_success(
        self, runner: CliRunner, test_config: Path, test_db: Database
    ) -> None:
        """Test setting a user's password."""
        test_db.create_user("pwuser", "oldpass12")

        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "set-password", "pwuser", "--password", "newpass12"],
            input="newpass12\n",  # Confirmation
        )
        assert result.exit_code == 0
        assert "updated" in result.output

        # Verify new password works
        user = test_db.authenticate_user("pwuser", "newpass12")
        assert user is not None

    def test_set_password_not_found(
        self, runner: CliRunner, test_config: Path
    ) -> None:
        """Test setting password for nonexistent user fails."""
        result = runner.invoke(
            cli,
            ["-c", str(test_config), "admin", "set-password", "nonexistent", "--password", "newpass12"],
            input="newpass12\n",
        )
        assert result.exit_code == 1
        assert "not found" in result.output


class TestHelp:
    """Tests for help messages."""

    def test_main_help(self, runner: CliRunner) -> None:
        """Test main help."""
        result = runner.invoke(cli, ["--help"])
        assert result.exit_code == 0
        assert "admin" in result.output
        assert "serve" in result.output

    def test_admin_help(self, runner: CliRunner) -> None:
        """Test admin help."""
        result = runner.invoke(cli, ["admin", "--help"])
        assert result.exit_code == 0
        assert "add-user" in result.output
        assert "delete-user" in result.output
        assert "list-users" in result.output
        assert "change-role" in result.output
        assert "generate-invite" in result.output
        assert "list-invites" in result.output
        assert "approve-user" in result.output

    def test_each_command_has_help(self, runner: CliRunner) -> None:
        """Test each admin command has help."""
        commands = [
            "add-user",
            "delete-user",
            "list-users",
            "change-role",
            "generate-invite",
            "list-invites",
            "approve-user",
            "disable-user",
            "set-password",
            "delete-invite",
        ]
        for cmd in commands:
            result = runner.invoke(cli, ["admin", cmd, "--help"])
            assert result.exit_code == 0, f"Help failed for {cmd}"
            assert "Usage:" in result.output, f"No usage in help for {cmd}"

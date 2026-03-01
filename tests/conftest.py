"""Shared test fixtures for mokuro-bunko tests."""

from __future__ import annotations

import tempfile
from pathlib import Path
from typing import TYPE_CHECKING, Generator

import pytest

from mokuro_bunko.config import Config, StorageConfig
from mokuro_bunko.database import Database

if TYPE_CHECKING:
    from playwright.sync_api import Page


# Playwright fixtures
@pytest.fixture(scope="session")
def browser_context_args() -> dict:
    """Browser context arguments for Playwright."""
    return {
        "ignore_https_errors": True,
    }


@pytest.fixture
def page(request: pytest.FixtureRequest) -> Generator["Page", None, None]:
    """Provide a Playwright page fixture."""
    try:
        from playwright.sync_api import sync_playwright
    except ImportError:
        pytest.skip("Playwright not installed")
        return

    with sync_playwright() as p:
        try:
            browser = p.chromium.launch(headless=True)
        except Exception as e:
            pytest.skip(f"Playwright browsers not available: {e}")
            return

        context = browser.new_context(ignore_https_errors=True)
        page = context.new_page()

        yield page

        context.close()
        browser.close()


@pytest.fixture
def tmp_path_factory_session(
    tmp_path_factory: pytest.TempPathFactory,
) -> Path:
    """Create a session-scoped temporary directory."""
    return tmp_path_factory.mktemp("mokuro_bunko")


@pytest.fixture
def temp_dir() -> Generator[Path, None, None]:
    """Create a temporary directory for tests."""
    with tempfile.TemporaryDirectory() as tmpdir:
        yield Path(tmpdir)


@pytest.fixture
def temp_config(temp_dir: Path) -> Config:
    """Create a test configuration with temporary paths."""
    return Config(
        storage=StorageConfig(base_path=temp_dir),
    )


@pytest.fixture
def temp_config_file(temp_dir: Path) -> Path:
    """Create a temporary config file path."""
    return temp_dir / "config.yaml"


@pytest.fixture
def temp_db(temp_dir: Path) -> Database:
    """Create a temporary database."""
    db_path = temp_dir / "test.db"
    return Database(db_path)


@pytest.fixture
def db_with_users(temp_db: Database) -> Database:
    """Create a database with test users."""
    temp_db.create_user("alice", "password123", "registered")
    temp_db.create_user("bob", "password456", "uploader")
    temp_db.create_user("charlie", "password789", "editor")
    temp_db.create_user("admin", "adminpass", "admin")
    temp_db.create_user("pending_user", "pending123", "registered", status="pending")
    return temp_db


@pytest.fixture
def db_with_invites(temp_db: Database) -> Database:
    """Create a database with test invites."""
    temp_db.create_invite("registered", "7d")
    temp_db.create_invite("uploader", "1d")
    temp_db.create_invite("editor", "30d")
    return temp_db


@pytest.fixture
def storage_dir(temp_dir: Path) -> Path:
    """Create a temporary storage directory structure."""
    storage = temp_dir / "storage"
    (storage / "library").mkdir(parents=True)
    (storage / "library" / "thumbnails").mkdir()
    (storage / "inbox").mkdir()
    (storage / "users").mkdir()
    return storage


@pytest.fixture
def sample_config_yaml() -> str:
    """Sample YAML configuration for testing."""
    return """
server:
  host: "127.0.0.1"
  port: 9090

storage:
  base_path: "/tmp/mokuro-test"

registration:
  mode: "invite"
  default_role: "uploader"

cors:
  enabled: true
  allowed_origins:
    - "https://example.com"
    - "http://localhost:3000"
  allow_credentials: true

ssl:
  enabled: false
  auto_cert: false

admin:
  enabled: true
  path: "/_admin"

ocr:
  backend: "cpu"
  poll_interval: 60
"""

"""Shared test fixtures for mokuro-bunko tests."""

from __future__ import annotations

import tempfile
from collections.abc import Generator
from pathlib import Path
from typing import TYPE_CHECKING, Any

import pytest

from mokuro_bunko import server as _server_module
from mokuro_bunko.config import Config, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.server import create_app as _create_app
from mokuro_bunko.server import shutdown_app

if TYPE_CHECKING:
    from playwright.sync_api import Page


# --------------------------------------------------------------------------
# Every app a test builds is shut down with its module. ``create_app`` arms a
# metadata pass, a periodic rescan, the library watcher, PROPFIND debounce
# timers and the community fetcher; only ``run_server``'s shutdown stopped
# them, so each test app left daemon timers that fired during interpreter
# finalization. Wrapped here, before any test module imports it (and
# ``create_ssl_server`` calls it through the module, so it is caught too).
# Module scope, not function scope: the web suites serve one app per module.
# --------------------------------------------------------------------------

_built_apps: list[Any] = []


def _tracking_create_app(*args: Any, **kwargs: Any) -> Any:
    app = _create_app(*args, **kwargs)
    _built_apps.append(app)
    return app


_server_module.create_app = _tracking_create_app  # type: ignore[assignment]


@pytest.fixture(scope="module", autouse=True)
def _shut_down_built_apps() -> Generator[None, None, None]:
    yield
    while _built_apps:
        shutdown_app(_built_apps.pop())


# Playwright fixtures
@pytest.fixture(scope="session")
def browser_context_args() -> dict:
    """Browser context arguments for Playwright."""
    return {
        "ignore_https_errors": True,
    }


@pytest.fixture
def page(request: pytest.FixtureRequest) -> Generator[Page, None, None]:
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


# --------------------------------------------------------------------------
# Thread survey: MOKURO_TEST_THREAD_SURVEY=<file> writes what is still running
# when pytest unconfigures -- just before the interpreter finalizes, where a
# daemon timer that fires writes to a stderr being torn down (the
# intermittent "Fatal Python error: _enter_buffered_busy" at exit). A file,
# because output capture is still in place at this point.
# --------------------------------------------------------------------------


def pytest_unconfigure(config: pytest.Config) -> None:
    import os
    import threading
    from collections import Counter

    target = os.environ.get("MOKURO_TEST_THREAD_SURVEY")
    if not target:
        return
    alive = [t for t in threading.enumerate() if t is not threading.main_thread()]
    daemons = [t for t in alive if t.daemon]

    def label(thread: threading.Thread) -> str:
        run = getattr(thread, "_target", None) or getattr(thread, "function", None)
        name = getattr(run, "__qualname__", None) or thread.name
        return f"{type(thread).__name__}:{name}"

    lines = [f"{len(alive)} threads alive at unconfigure, {len(daemons)} daemon"]
    lines += [f"  {count:4d}  {what}" for what, count in Counter(map(label, alive)).most_common()]
    Path(target).write_text("\n".join(lines) + "\n", encoding="utf-8")

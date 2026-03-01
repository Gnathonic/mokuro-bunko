"""Playwright UI tests for registration page."""

from __future__ import annotations

import multiprocessing
import socket
import time
from pathlib import Path
from typing import TYPE_CHECKING, Generator

import pytest

if TYPE_CHECKING:
    from playwright.sync_api import Page

# Check if playwright browsers are installed
try:
    from playwright.sync_api import sync_playwright

    def _check_browsers() -> bool:
        """Check if Playwright browsers are installed."""
        try:
            with sync_playwright() as p:
                browser = p.chromium.launch(headless=True)
                browser.close()
                return True
        except Exception:
            return False

    BROWSERS_AVAILABLE = _check_browsers()
except ImportError:
    BROWSERS_AVAILABLE = False

pytestmark = pytest.mark.skipif(
    not BROWSERS_AVAILABLE,
    reason="Playwright browsers not installed. Run: playwright install chromium"
)


def find_free_port() -> int:
    """Find a free port to use."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("", 0))
        return s.getsockname()[1]


def run_server(
    storage_path: str,
    port: int,
    ready_event: multiprocessing.Event,
    registration_mode: str = "self",
) -> None:
    """Run the server in a subprocess."""
    from cheroot.wsgi import Server as WSGIServer

    from mokuro_bunko.config import (
        AdminConfig,
        Config,
        CorsConfig,
        RegistrationConfig,
        ServerConfig,
        StorageConfig,
    )
    from mokuro_bunko.server import create_app

    config = Config(
        server=ServerConfig(host="127.0.0.1", port=port),
        storage=StorageConfig(base_path=Path(storage_path)),
        admin=AdminConfig(enabled=True, path="/_admin"),
        registration=RegistrationConfig(mode=registration_mode),
        cors=CorsConfig(enabled=True),
    )

    app = create_app(config)
    server = WSGIServer(("127.0.0.1", port), app)

    ready_event.set()

    try:
        server.start()
    except KeyboardInterrupt:
        pass
    finally:
        server.stop()


@pytest.fixture(scope="module")
def self_registration_server(
    tmp_path_factory: pytest.TempPathFactory,
) -> Generator[str, None, None]:
    """Start server with self registration mode."""
    storage = tmp_path_factory.mktemp("storage_self")
    (storage / "library").mkdir()
    (storage / "inbox").mkdir()
    (storage / "users").mkdir()

    from mokuro_bunko.database import Database

    db = Database(storage / "mokuro.db")
    db.create_user("admin", "adminpass", role="admin")

    port = find_free_port()
    ready_event = multiprocessing.Event()

    process = multiprocessing.Process(
        target=run_server,
        args=(str(storage), port, ready_event, "self"),
        daemon=True,
    )
    process.start()

    if not ready_event.wait(timeout=10):
        process.terminate()
        pytest.fail("Server failed to start")

    time.sleep(0.5)

    yield f"http://127.0.0.1:{port}"

    process.terminate()
    process.join(timeout=5)


@pytest.fixture(scope="module")
def invite_registration_server(
    tmp_path_factory: pytest.TempPathFactory,
) -> Generator[tuple[str, str], None, None]:
    """Start server with invite registration mode and return URL + invite code."""
    storage = tmp_path_factory.mktemp("storage_invite")
    (storage / "library").mkdir()
    (storage / "inbox").mkdir()
    (storage / "users").mkdir()

    from mokuro_bunko.database import Database

    db = Database(storage / "mokuro.db")
    db.create_user("admin", "adminpass", role="admin")
    invite_code = db.create_invite(role="registered", expires="7d")

    port = find_free_port()
    ready_event = multiprocessing.Event()

    process = multiprocessing.Process(
        target=run_server,
        args=(str(storage), port, ready_event, "invite"),
        daemon=True,
    )
    process.start()

    if not ready_event.wait(timeout=10):
        process.terminate()
        pytest.fail("Server failed to start")

    time.sleep(0.5)

    yield f"http://127.0.0.1:{port}", invite_code

    process.terminate()
    process.join(timeout=5)


@pytest.fixture(scope="module")
def disabled_registration_server(
    tmp_path_factory: pytest.TempPathFactory,
) -> Generator[str, None, None]:
    """Start server with disabled registration mode."""
    storage = tmp_path_factory.mktemp("storage_disabled")
    (storage / "library").mkdir()
    (storage / "inbox").mkdir()
    (storage / "users").mkdir()

    from mokuro_bunko.database import Database

    db = Database(storage / "mokuro.db")
    db.create_user("admin", "adminpass", role="admin")

    port = find_free_port()
    ready_event = multiprocessing.Event()

    process = multiprocessing.Process(
        target=run_server,
        args=(str(storage), port, ready_event, "disabled"),
        daemon=True,
    )
    process.start()

    if not ready_event.wait(timeout=10):
        process.terminate()
        pytest.fail("Server failed to start")

    time.sleep(0.5)

    yield f"http://127.0.0.1:{port}"

    process.terminate()
    process.join(timeout=5)


class TestRegistrationPageUI:
    """Tests for registration page UI."""

    def test_registration_page_loads(
        self, self_registration_server: str, page: Page
    ) -> None:
        """Test registration page loads correctly."""
        page.goto(f"{self_registration_server}/register")
        page.wait_for_load_state("networkidle")

        # Check title
        assert "Mokuro Bunko" in page.title()

        # Check form elements
        assert page.locator("#username").is_visible()
        assert page.locator("#password").is_visible()
        assert page.locator("#confirm-password").is_visible()
        assert page.locator("#submit-btn").is_visible()

    def test_successful_registration(
        self, self_registration_server: str, page: Page
    ) -> None:
        """Test successful self registration."""
        page.goto(f"{self_registration_server}/register")
        page.wait_for_load_state("networkidle")

        # Generate unique username
        import uuid
        username = f"user_{uuid.uuid4().hex[:8]}"

        # Fill form
        page.locator("#username").fill(username)
        page.locator("#password").fill("password123")
        page.locator("#confirm-password").fill("password123")

        # Submit
        page.locator("#submit-btn").click()

        # Wait for either success message or form to be hidden
        page.wait_for_function(
            """() => {
                const success = document.querySelector('#success-message');
                const form = document.querySelector('#register-form');
                return (success && success.classList.contains('visible')) ||
                       (form && form.style.display === 'none');
            }""",
            timeout=10000,
        )

        # Verify success
        success_msg = page.locator("#success-message")
        assert success_msg.is_visible() or not page.locator("#register-form").is_visible()

    def test_duplicate_username_error(
        self, self_registration_server: str, page: Page
    ) -> None:
        """Test duplicate username error."""
        import uuid
        username = f"dupuser_{uuid.uuid4().hex[:8]}"

        # First registration
        page.goto(f"{self_registration_server}/register")
        page.wait_for_load_state("networkidle")

        page.locator("#username").fill(username)
        page.locator("#password").fill("password123")
        page.locator("#confirm-password").fill("password123")
        page.locator("#submit-btn").click()

        # Wait for success
        page.wait_for_function(
            "() => document.querySelector('#success-message')?.classList.contains('visible')",
            timeout=10000,
        )

        # Try to register again with same username
        page.goto(f"{self_registration_server}/register")
        page.wait_for_load_state("networkidle")

        page.locator("#username").fill(username)
        page.locator("#password").fill("password456")
        page.locator("#confirm-password").fill("password456")
        page.locator("#submit-btn").click()

        # Wait for error
        page.wait_for_function(
            """() => {
                const error = document.querySelector('#username-error');
                return error && error.classList.contains('visible');
            }""",
            timeout=10000,
        )

        error = page.locator("#username-error")
        assert error.is_visible()


class TestInviteRegistration:
    """Tests for invite-based registration."""

    def test_invite_field_visible(
        self, invite_registration_server: tuple[str, str], page: Page
    ) -> None:
        """Test invite code field is visible in invite mode."""
        url, _ = invite_registration_server

        page.goto(f"{url}/register")
        page.wait_for_load_state("networkidle")

        # Wait for JS to show invite field
        page.wait_for_function(
            "() => document.querySelector('#invite-group')?.classList.contains('visible')",
            timeout=5000,
        )
        assert page.locator("#invite-code").is_visible()

    def test_registration_with_valid_invite(
        self, invite_registration_server: tuple[str, str], page: Page
    ) -> None:
        """Test registration with valid invite code."""
        url, invite_code = invite_registration_server

        page.goto(f"{url}/register")
        page.wait_for_load_state("networkidle")

        # Wait for invite field
        page.wait_for_function(
            "() => document.querySelector('#invite-group')?.classList.contains('visible')",
            timeout=5000,
        )

        import uuid
        username = f"invited_{uuid.uuid4().hex[:8]}"

        # Fill form
        page.locator("#username").fill(username)
        page.locator("#password").fill("password123")
        page.locator("#confirm-password").fill("password123")
        page.locator("#invite-code").fill(invite_code)

        # Submit
        page.locator("#submit-btn").click()

        # Wait for success
        page.wait_for_function(
            "() => document.querySelector('#success-message')?.classList.contains('visible')",
            timeout=10000,
        )
        assert page.locator("#success-message").is_visible()

    def test_registration_with_invalid_invite(
        self, invite_registration_server: tuple[str, str], page: Page
    ) -> None:
        """Test registration with invalid invite code."""
        url, _ = invite_registration_server

        page.goto(f"{url}/register")
        page.wait_for_load_state("networkidle")

        # Wait for invite field
        page.wait_for_function(
            "() => document.querySelector('#invite-group')?.classList.contains('visible')",
            timeout=5000,
        )

        import uuid
        username = f"badinvite_{uuid.uuid4().hex[:8]}"

        # Fill form with invalid invite
        page.locator("#username").fill(username)
        page.locator("#password").fill("password123")
        page.locator("#confirm-password").fill("password123")
        page.locator("#invite-code").fill("invalid-code-123")

        # Submit
        page.locator("#submit-btn").click()

        # Wait for error
        page.wait_for_function(
            "() => document.querySelector('#invite-error')?.classList.contains('visible')",
            timeout=10000,
        )
        error = page.locator("#invite-error")
        assert error.is_visible()


class TestDisabledRegistration:
    """Tests for disabled registration mode."""

    def test_disabled_message_shown(
        self, disabled_registration_server: str, page: Page
    ) -> None:
        """Test disabled message is shown."""
        page.goto(f"{disabled_registration_server}/register")
        page.wait_for_load_state("networkidle")

        # Wait for JS to show disabled message
        page.wait_for_function(
            "() => document.querySelector('#disabled-message')?.style.display !== 'none'",
            timeout=5000,
        )
        assert page.locator("#disabled-message").is_visible()

        # Form should be hidden
        form_visible = page.evaluate(
            "() => document.querySelector('#register-form')?.style.display !== 'none'"
        )
        assert not form_visible


class TestRegistrationPageResponsive:
    """Tests for responsive behavior."""

    def test_mobile_viewport(
        self, self_registration_server: str, page: Page
    ) -> None:
        """Test registration page on mobile viewport."""
        page.set_viewport_size({"width": 375, "height": 667})

        page.goto(f"{self_registration_server}/register")
        page.wait_for_load_state("networkidle")

        # Check page still loads
        assert page.locator("h1").is_visible()
        assert page.locator("#register-form").is_visible()

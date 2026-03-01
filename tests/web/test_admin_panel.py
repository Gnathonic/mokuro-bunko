"""Playwright UI tests for admin panel."""

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
                # Try to launch chromium
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


def run_server(storage_path: str, port: int, ready_event: multiprocessing.Event) -> None:
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
        registration=RegistrationConfig(mode="self"),
        cors=CorsConfig(enabled=True),
    )

    app = create_app(config)
    server = WSGIServer(("127.0.0.1", port), app)

    # Signal that we're ready
    ready_event.set()

    try:
        server.start()
    except KeyboardInterrupt:
        pass
    finally:
        server.stop()


@pytest.fixture(scope="module")
def server_url(tmp_path_factory: pytest.TempPathFactory) -> Generator[str, None, None]:
    """Start server and return URL."""
    storage = tmp_path_factory.mktemp("storage")
    (storage / "library").mkdir()
    (storage / "inbox").mkdir()
    (storage / "users").mkdir()

    # Create admin user in database
    from mokuro_bunko.database import Database

    db = Database(storage / "mokuro.db")
    db.create_user("admin", "adminpass", role="admin")
    # Database uses SQLite which auto-commits, no close needed

    port = find_free_port()
    ready_event = multiprocessing.Event()

    # Start server process
    process = multiprocessing.Process(
        target=run_server,
        args=(str(storage), port, ready_event),
        daemon=True,
    )
    process.start()

    # Wait for server to be ready
    if not ready_event.wait(timeout=10):
        process.terminate()
        pytest.fail("Server failed to start")

    # Give it a moment to fully initialize
    time.sleep(0.5)

    url = f"http://127.0.0.1:{port}"

    yield url

    # Cleanup
    process.terminate()
    process.join(timeout=5)


@pytest.fixture
def admin_page(server_url: str, page: Page) -> Page:
    """Navigate to admin panel with authentication."""
    # Set up HTTP basic auth credentials
    import base64

    credentials = base64.b64encode(b"admin:adminpass").decode()
    page.add_init_script(f"sessionStorage.setItem('mokuro_auth', '{credentials}');")

    # Add auth header via route interception
    def handle_route(route):
        headers = {**route.request.headers, "Authorization": f"Basic {credentials}"}
        route.continue_(headers=headers)

    page.route("**/*", handle_route)

    # Navigate to admin panel
    page.goto(f"{server_url}/_admin/")

    # Wait for the page to load
    page.wait_for_load_state("networkidle")

    return page


class TestAdminPanelUI:
    """Tests for admin panel UI."""

    def test_admin_panel_loads(self, admin_page: Page) -> None:
        """Test admin panel loads correctly."""
        # Check admin badge is visible (indicates admin panel loaded)
        assert admin_page.locator(".admin-badge").text_content() == "Admin"

        # Check tabs exist (use more specific selectors)
        assert admin_page.locator(".tab:has-text('Users')").is_visible()
        assert admin_page.locator(".tab:has-text('Invites')").is_visible()

    def test_users_tab_shows_admin(self, admin_page: Page) -> None:
        """Test users tab shows the admin user."""
        # Click Users tab if not already active
        admin_page.locator(".tab:has-text('Users')").click()

        # Wait for users to load
        admin_page.wait_for_selector("#users-body tr")

        # Check admin user is listed in the table body
        user_rows = admin_page.locator("#users-body tr")
        assert user_rows.count() >= 1

        # Check that "admin" appears in the table
        assert admin_page.locator("#users-body td:has-text('admin')").first.is_visible()

    def test_add_user_button_opens_modal(self, admin_page: Page) -> None:
        """Test Add User button opens modal."""
        # Click Add User button
        admin_page.locator("#add-user-btn").click()

        # Wait for modal to open (has .open class)
        admin_page.wait_for_selector("#add-user-modal.open")

        # Check modal is visible
        modal = admin_page.locator("#add-user-modal")
        assert modal.is_visible()

        # Check form fields in the modal
        assert admin_page.locator("#new-username").is_visible()
        assert admin_page.locator("#new-password").is_visible()

        # Close the modal (use .first to avoid strict mode with multiple close buttons)
        admin_page.locator("#add-user-modal [data-close-modal]").first.click()

    def test_create_user_flow(self, admin_page: Page) -> None:
        """Test creating a new user."""
        # Click Add User button
        admin_page.locator("#add-user-btn").click()

        # Wait for modal
        admin_page.wait_for_selector("#add-user-modal.open")

        # Fill form
        admin_page.locator("#new-username").fill("testuser")
        admin_page.locator("#new-password").fill("testpass123")

        # Submit
        admin_page.locator("#add-user-modal button[type='submit']").click()

        # Wait for modal to close
        admin_page.wait_for_selector("#add-user-modal.open", state="hidden", timeout=5000)

        # Check user appears in list
        admin_page.wait_for_selector("#users-body td:has-text('testuser')")
        assert admin_page.locator("#users-body td:has-text('testuser')").is_visible()

    def test_invites_tab(self, admin_page: Page) -> None:
        """Test switching to Invites tab."""
        # Click Invites tab
        admin_page.locator(".tab:has-text('Invites')").click()

        # Check invites content is visible
        assert admin_page.locator("h2:has-text('Invite Codes')").is_visible()
        assert admin_page.locator("#generate-invite-btn").is_visible()

    def test_generate_invite_flow(self, admin_page: Page) -> None:
        """Test generating an invite code."""
        # Go to Invites tab
        admin_page.locator(".tab:has-text('Invites')").click()

        # Click Generate Invite
        admin_page.locator("#generate-invite-btn").click()

        # Wait for modal
        admin_page.wait_for_selector("#generate-invite-modal.open")

        # Submit with defaults
        admin_page.locator("#generate-invite-modal button[type='submit']").click()

        # Wait for invite code modal
        admin_page.wait_for_selector("#invite-code-modal.open", timeout=5000)

        # Check code is displayed
        code_input = admin_page.locator("#invite-code-value")
        assert code_input.is_visible()
        code_value = code_input.input_value()
        assert len(code_value) > 10  # Invite codes are long

        # Close the modal (use .first to avoid strict mode with multiple close buttons)
        admin_page.locator("#invite-code-modal [data-close-modal]").first.click()

    def test_change_role_modal(self, admin_page: Page) -> None:
        """Test role change modal opens."""
        # Make sure we're on Users tab
        admin_page.locator(".tab:has-text('Users')").click()
        admin_page.wait_for_selector("#users-body tr")

        # Find the first role button in the table
        role_button = admin_page.locator("#users-body button:has-text('Role')").first
        role_button.click()

        # Check modal is visible
        admin_page.wait_for_selector("#change-role-modal.open")
        assert admin_page.locator("#change-role-modal").is_visible()
        assert admin_page.locator("#change-role-select").is_visible()

        # Close the modal (use .first to avoid strict mode with multiple close buttons)
        admin_page.locator("#change-role-modal [data-close-modal]").first.click()


class TestAdminPanelAccessControl:
    """Tests for admin panel access control."""

    def test_non_admin_cannot_access(self, server_url: str, page: Page) -> None:
        """Test non-admin users cannot access admin panel."""
        # First register a regular user
        import httpx

        response = httpx.post(
            f"{server_url}/api/register",
            json={"username": "regular", "password": "regular123"},
        )
        assert response.status_code == 201

        # Set up auth for regular user
        import base64

        credentials = base64.b64encode(b"regular:regular123").decode()

        def handle_route(route):
            headers = {**route.request.headers, "Authorization": f"Basic {credentials}"}
            route.continue_(headers=headers)

        page.route("**/*", handle_route)

        # Try to access admin panel
        page.goto(f"{server_url}/_admin/api/users")

        # Should get 403 error
        content = page.content()
        assert "403" in content or "Admin access required" in content

    def test_anonymous_cannot_access(self, server_url: str, page: Page) -> None:
        """Test anonymous users cannot access admin panel API."""
        # Try to access API without auth
        page.goto(f"{server_url}/_admin/api/users")

        # Should get 403 or authentication required
        content = page.content()
        assert "403" in content or "Admin access required" in content or "Authentication required" in content


class TestAdminPanelResponsive:
    """Tests for admin panel responsive behavior."""

    def test_mobile_viewport(self, server_url: str, page: Page) -> None:
        """Test admin panel on mobile viewport."""
        # Set mobile viewport
        page.set_viewport_size({"width": 375, "height": 667})

        # Set up auth
        import base64

        credentials = base64.b64encode(b"admin:adminpass").decode()
        page.add_init_script(f"sessionStorage.setItem('mokuro_auth', '{credentials}');")

        def handle_route(route):
            headers = {**route.request.headers, "Authorization": f"Basic {credentials}"}
            route.continue_(headers=headers)

        page.route("**/*", handle_route)

        # Navigate
        page.goto(f"{server_url}/_admin/")
        page.wait_for_load_state("networkidle")

        # Check page still loads (admin badge visible)
        assert page.locator(".admin-badge").is_visible()

        # Check tabs are still accessible
        assert page.locator(".tab:has-text('Users')").is_visible()

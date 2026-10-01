"""The web pages sign in with a bearer token and never keep the password.

A real server, a real browser, no header interception: the login page trades
the password for a token, the admin panel works on the token alone, logout
revokes it on the server, and a password left in sessionStorage by the
pages before tokens is wiped.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

import pytest

from tests.web.test_admin_panel import BROWSERS_AVAILABLE, server_url  # noqa: F401

if TYPE_CHECKING:
    from playwright.sync_api import Page

pytestmark = pytest.mark.skipif(not BROWSERS_AVAILABLE, reason="Playwright browsers not installed.")


def _sign_in(page: Page, server_url: str) -> None:  # noqa: F811
    page.goto(f"{server_url}/login")
    page.fill("#username", "admin")
    page.fill("#password", "adminpass")
    page.click("#submit-btn")
    page.wait_for_url(f"{server_url}/")


def test_signing_in_keeps_a_token_not_the_password(page: Page, server_url: str) -> None:  # noqa: F811
    _sign_in(page, server_url)
    stored = page.evaluate("() => Object.assign({}, sessionStorage)")
    assert stored.get("mokuro_token")
    assert "mokuro_auth" not in stored
    assert all("adminpass" not in str(value) for value in stored.values())


def test_the_admin_panel_works_on_the_token_alone(page: Page, server_url: str) -> None:  # noqa: F811
    _sign_in(page, server_url)
    page.goto(f"{server_url}/_admin")
    page.wait_for_selector("#users-body tr")
    assert "admin" in page.inner_text("#users-body")


def test_logout_revokes_the_token_on_the_server(page: Page, server_url: str) -> None:  # noqa: F811
    _sign_in(page, server_url)
    token = page.evaluate("() => sessionStorage.getItem('mokuro_token')")
    with page.expect_navigation():
        page.evaluate("() => { window.logout(); return null; }")
    page.wait_for_load_state("domcontentloaded")
    status = page.evaluate(
        """async (token) => (await fetch('/login/api/me',
            {headers: {Authorization: 'Bearer ' + token}})).status""",
        token,
    )
    assert status == 401


def test_a_password_left_by_the_old_pages_is_wiped(page: Page, server_url: str) -> None:  # noqa: F811
    page.add_init_script("sessionStorage.setItem('mokuro_auth', 'YWRtaW46YWRtaW5wYXNz');")
    page.goto(f"{server_url}/")
    page.wait_for_load_state("domcontentloaded")
    assert page.evaluate("() => sessionStorage.getItem('mokuro_auth')") is None


def test_an_expired_token_sends_the_admin_panel_to_login(page: Page, server_url: str) -> None:  # noqa: F811
    page.add_init_script(
        "sessionStorage.setItem('mokuro_token', 'revoked-or-expired');"
        "sessionStorage.setItem('mokuro_user', '{\"username\":\"admin\",\"role\":\"admin\"}');"
    )
    page.goto(f"{server_url}/_admin")
    page.wait_for_url(f"{server_url}/login")

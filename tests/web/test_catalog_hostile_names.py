"""A folder name is data on the catalog page, never script.

The cards used to open through an inline ``onclick="openSeries('<name>')"``
whose escaping left ``&`` alone -- so an uploader's folder named
``x&#39;-alert(1)-&#39;`` became ``openSeries('x'-alert(1)-'')`` once the
browser decoded the attribute, running in the page that holds a signed-in
admin's credentials. A real server, a real browser: hostile names render,
open, and run nothing.
"""

from __future__ import annotations

import threading
import urllib.parse
from collections.abc import Generator
from pathlib import Path
from typing import TYPE_CHECKING

import pytest

from tests.web.test_catalog_open_link import BROWSERS_AVAILABLE, READER_URL, _launch

if TYPE_CHECKING:
    from playwright.sync_api import Browser

pytestmark = pytest.mark.skipif(not BROWSERS_AVAILABLE, reason="Playwright browsers not installed.")

SERIES = "x&#39;-(window.__pwned=1)-&#39; <b>&amp;"
VOLUME = "v&#39;-(window.__pwned=2)-&#39;"


@pytest.fixture(scope="module")
def server(tmp_path_factory: pytest.TempPathFactory) -> Generator[str, None, None]:
    from cheroot.wsgi import Server as WSGIServer

    from mokuro_bunko.config import CatalogConfig, Config, ServerConfig, StorageConfig
    from mokuro_bunko.server import create_app

    storage = tmp_path_factory.mktemp("catalog_hostile")
    series = storage / "library" / SERIES
    series.mkdir(parents=True)
    (storage / "inbox").mkdir()
    (storage / "users").mkdir()
    (series / f"{VOLUME}.cbz").write_bytes(b"PK\x05\x06" + b"\x00" * 18)
    config = Config(
        server=ServerConfig(host="127.0.0.1", port=0),
        storage=StorageConfig(base_path=Path(storage)),
        catalog=CatalogConfig(enabled=True, reader_url=READER_URL),
    )
    httpd = WSGIServer(("127.0.0.1", 0), create_app(config))
    httpd.prepare()
    thread = threading.Thread(target=httpd.serve, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{httpd.bind_addr[1]}"
    finally:
        httpd.stop()
        thread.join(timeout=5)


@pytest.fixture(scope="module")
def browser() -> Generator[Browser, None, None]:
    from playwright.sync_api import sync_playwright

    with sync_playwright() as p:
        b = _launch(p)
        yield b
        b.close()


def test_hostile_names_open_their_cards_and_run_nothing(browser: Browser, server: str) -> None:
    ctx = browser.new_context()
    page = ctx.new_page()
    page.add_init_script("window.__opened = []; window.open = (u) => { window.__opened.push(u); };")
    try:
        page.goto(server + "/catalog")
        page.wait_for_selector(".volume-card")
        assert page.inner_text(".volume-card__title") == SERIES

        page.click(".volume-card")
        page.wait_for_function(
            "(s) => document.querySelector('.catalog-breadcrumb__item--active')"
            "?.textContent === s",
            arg=SERIES,
        )
        page.wait_for_function(
            "(v) => [...document.querySelectorAll('.volume-card__title')]"
            ".some((el) => el.textContent === v)",
            arg=VOLUME,
        )

        page.click(".volume-card")
        opened = page.evaluate("window.__opened")
        assert len(opened) == 1
        params = urllib.parse.parse_qs(opened[0].split("?", 1)[1])
        assert params["cbz"][0].endswith(
            "/" + urllib.parse.quote(VOLUME + ".cbz", safe="!*'()")
        )
        assert page.evaluate("window.__pwned") is None
    finally:
        ctx.close()

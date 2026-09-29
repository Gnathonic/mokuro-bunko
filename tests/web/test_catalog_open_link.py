"""The catalog's "read" link carries the volume's manifest, and the manifest resolves.

A real server (the assembled app on an OS-chosen port) and a real browser:
click a volume card, capture the reader URL `openVolume` opens, then fetch
the manifest it names and check it points back at the very archive the link
names. The names carry spaces, `#`, `?`, `&`, `%`, `+` and non-ASCII, so a
mismatch between the catalog's `encodeURIComponent` and the server's escaping
would show.

Set `MOKURO_TEST_CHROMIUM` to an existing Chromium binary to run these
against a browser Playwright did not download itself.
"""

from __future__ import annotations

import os
import threading
import urllib.parse
from collections.abc import Generator
from pathlib import Path
from typing import TYPE_CHECKING, Any

import pytest

if TYPE_CHECKING:
    from playwright.sync_api import Browser

CHROMIUM = os.environ.get("MOKURO_TEST_CHROMIUM") or None

try:
    from playwright.sync_api import sync_playwright

    def _launch(p: Any) -> Any:
        kwargs: dict[str, Any] = {"headless": True}
        if CHROMIUM:
            kwargs["executable_path"] = CHROMIUM
        return p.chromium.launch(**kwargs)

    def _check_browsers() -> bool:
        try:
            with sync_playwright() as p:
                _launch(p).close()
                return True
        except Exception:
            return False

    BROWSERS_AVAILABLE = _check_browsers()
except ImportError:
    BROWSERS_AVAILABLE = False

pytestmark = pytest.mark.skipif(
    not BROWSERS_AVAILABLE,
    reason="Playwright browsers not installed. Run: playwright install chromium,"
           " or set MOKURO_TEST_CHROMIUM to an existing binary.",
)

SERIES = "Dr Stone #1 & 2? 50%+ 世界"
VOLUME = "Dr Stone 01 (v1.5) 50%+"
READER_URL = "https://reader.example"


@pytest.fixture(scope="module")
def server(tmp_path_factory: pytest.TempPathFactory) -> Generator[str, None, None]:
    from cheroot.wsgi import Server as WSGIServer

    from mokuro_bunko.config import CatalogConfig, Config, ServerConfig, StorageConfig
    from mokuro_bunko.server import create_app

    storage = tmp_path_factory.mktemp("catalog_link")
    series = storage / "library" / SERIES
    series.mkdir(parents=True)
    (storage / "inbox").mkdir()
    (storage / "users").mkdir()
    (series / f"{VOLUME}.cbz").write_bytes(b"PK\x05\x06" + b"\x00" * 18)
    (series / f"{VOLUME}.mokuro").write_text("{}", encoding="utf-8")
    (series / f"{VOLUME}.hayai-nova.mokuro").write_text("{}", encoding="utf-8")

    config = Config(
        server=ServerConfig(host="127.0.0.1", port=0),
        storage=StorageConfig(base_path=Path(storage)),
        catalog=CatalogConfig(enabled=True, reader_url=READER_URL),
    )
    app = create_app(config)
    httpd = WSGIServer(("127.0.0.1", 0), app)
    httpd.prepare()
    port = httpd.bind_addr[1]
    thread = threading.Thread(target=httpd.serve, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{port}"
    finally:
        httpd.stop()
        thread.join(timeout=5)


@pytest.fixture(scope="module")
def browser() -> Generator[Browser, None, None]:
    with sync_playwright() as p:
        b = _launch(p)
        yield b
        b.close()


def test_the_open_link_carries_a_manifest_that_names_its_archive(
    browser: Browser, server: str
) -> None:
    ctx = browser.new_context()
    page = ctx.new_page()
    # Capture the reader URL instead of opening a tab.
    page.add_init_script("window.__opened = []; window.open = (u) => { window.__opened.push(u); };")
    try:
        page.goto(server + "/catalog#" + urllib.parse.quote(SERIES, safe=""))
        page.wait_for_selector(".volume-card")
        page.click(".volume-card")
        opened = page.evaluate("window.__opened")
        assert len(opened) == 1

        link = opened[0]
        assert link.startswith(READER_URL + "/#/upload?")
        params = urllib.parse.parse_qs(link.split("?", 1)[1])
        cbz = params["cbz"][0]
        manifest_url = params["manifest"][0]

        # cbz is unchanged from before the manifest existed.
        assert cbz == server + "/mokuro-reader/" + urllib.parse.quote(SERIES, safe="!*'()") + "/" + (
            urllib.parse.quote(VOLUME + ".cbz", safe="!*'()")
        )
        assert manifest_url == server + "/catalog/api/manifest?" + urllib.parse.urlencode(
            {"series": SERIES, "volume": VOLUME}, quote_via=urllib.parse.quote, safe="!*'()"
        )

        # The reader resolves each listed URL against the manifest's own URL.
        resolved = page.evaluate(
            """async (url) => {
                const r = await fetch(url, { cache: 'no-store' });
                const m = await r.json();
                return {
                  status: r.status,
                  archive: new URL(m.archive.url, url).toString(),
                  ocr: new URL(m.ocr.url, url).toString(),
                  layers: m.layers.map((l) => l.id),
                  archiveStatus: (await fetch(new URL(m.archive.url, url))).status,
                };
            }""",
            manifest_url,
        )
        assert resolved["status"] == 200
        assert resolved["archive"] == cbz
        assert resolved["archiveStatus"] == 200
        assert resolved["ocr"] == cbz[: -len(".cbz")] + ".mokuro"
        assert resolved["layers"] == ["hayai-nova"]
    finally:
        ctx.close()

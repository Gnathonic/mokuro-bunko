"""Nothing on the queue and admin pages is cut short or pushed off a phone.

Each of these was seen on a real screen: a state pill that read
"Configurin", a signed-in header wider than a 400px phone (the page scrolled
sideways and "Logout" was cut), an idle machine's "Auto configuring ..." cut
beside an empty layer box, an admin tab off the edge of the screen, a
benchmark that re-wrapped its row's summary line, a hint shown with its
Markdown backticks, a Workers placeholder cut to "auto (fo".

Set `MOKURO_TEST_CHROMIUM` to an existing Chromium binary to run these
against a browser Playwright did not download itself.
"""

from __future__ import annotations

import os
from collections.abc import Generator
from typing import TYPE_CHECKING, Any

import pytest

from .generations_stub import BENCH_RUN_SEQUENCE, StubServer, machine_phase, two_machines_status

if TYPE_CHECKING:
    from playwright.sync_api import Browser, Page

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

SIGNED_IN = (
    "sessionStorage.setItem('mokuro_token', 'YWRtaW46YWRtaW5wYXNz');"
    "sessionStorage.setItem('mokuro_user', '{\"username\":\"admin\",\"role\":\"admin\"}');"
)
# Every nav link a signed-in admin can be shown, the Queue link included.
NAV_ALL = {
    "home_enabled": True, "catalog_enabled": True, "queue_show_in_nav": True,
    "queue_public_access": True, "registration_enabled": True,
}


@pytest.fixture(scope="module")
def browser() -> Generator[Browser, None, None]:
    with sync_playwright() as p:
        b = _launch(p)
        yield b
        b.close()


@pytest.fixture
def stub() -> Generator[StubServer, None, None]:
    with StubServer() as server:
        server.state.queue_status = two_machines_status()
        yield server


def _page(browser: Browser, width: int, signed_in: bool = True) -> Page:
    ctx = browser.new_context(viewport={"width": width, "height": 900})
    page = ctx.new_page()
    if signed_in:
        page.add_init_script(SIGNED_IN)
    # Every link the header can hold, so the widest header is the one tested.
    page.route("**/api/nav/config", lambda route: route.fulfill(json=NAV_ALL))
    return page


def _no_sideways_scroll(page: Page) -> None:
    width, client = page.evaluate(
        "[document.documentElement.scrollWidth, document.documentElement.clientWidth]"
    )
    assert width <= client, f"the page scrolls sideways: {width} > {client}"


def _inside_viewport(page: Page, selector: str) -> None:
    right = page.evaluate(
        "s => Math.max(...[...document.querySelectorAll(s)].map(e => e.getBoundingClientRect().right))",
        selector,
    )
    assert right <= page.viewport_size["width"], f"{selector} ends at {right}px"


def _configuring() -> dict[str, Any]:
    status = machine_phase(two_machines_status(), "idle")
    status["connected_machines"] = [
        {"machine": "local", "slots": 1},
        {"machine": "tower", "slots": 1,
         "configuring": {"key": "g-2", "generation": "hayai-nova-ppocr", "auto": True}},
    ]
    return status


class TestHeader:
    @pytest.mark.parametrize("width", [360, 400])
    @pytest.mark.parametrize("path", ["/queue/", "/_admin/"])
    def test_a_signed_in_header_fits_a_phone(
        self, browser: Browser, stub: StubServer, width: int, path: str
    ) -> None:
        stub.state.queue_admin = True
        page = _page(browser, width)
        try:
            page.goto(stub.url + path)
            page.wait_for_selector("#header-nav button:has-text('Logout')")
            _no_sideways_scroll(page)
            _inside_viewport(page, "#header-nav > *")
            # The logo and the links never overlap.
            brand = page.locator(".mokuro-header__logo").bounding_box()
            first = page.locator("#header-nav > *").first.bounding_box()
            assert brand and first
            assert brand["x"] + brand["width"] <= first["x"] or brand["y"] + brand["height"] <= first["y"]
        finally:
            page.context.close()


class TestQueueCards:
    def test_every_state_label_fits_its_pill(self, browser: Browser, stub: StubServer) -> None:
        stub.state.queue_status = _configuring()
        for width in (1280, 400):
            page = _page(browser, width, signed_in=False)
            try:
                page.goto(stub.url + "/queue/")
                page.wait_for_selector(".state-pill[data-state='configuring']")
                clipped = page.evaluate("""() => {
                  const pill = document.querySelector('.state-pill');
                  const out = [];
                  for (const t of ['Running', 'Loading', 'Waiting', 'Idle', 'Held', 'Configuring']) {
                    pill.textContent = t;
                    if (pill.scrollWidth > pill.clientWidth) out.push(t);
                  }
                  return out; }""")
                assert clipped == [], f"cut at {width}px: {clipped}"
            finally:
                page.context.close()

    def test_an_empty_lane_says_what_it_does_in_full_on_a_phone(
        self, browser: Browser, stub: StubServer
    ) -> None:
        stub.state.queue_status = _configuring()
        page = _page(browser, 400, signed_in=False)
        try:
            page.goto(stub.url + "/queue/")
            page.wait_for_selector(".lane[data-state='configuring']")
            what = page.locator(".lane[data-state='configuring'] .job__volume")
            assert what.inner_text() == "Auto configuring hayai-nova-ppocr"
            assert what.evaluate("e => e.scrollWidth <= e.clientWidth"), "the title is cut"
            _no_sideways_scroll(page)
        finally:
            page.context.close()

    def test_failures_are_drawn_as_errors(self, browser: Browser, stub: StubServer) -> None:
        page = _page(browser, 1280, signed_in=False)
        try:
            page.goto(stub.url + "/queue/")
            page.wait_for_selector("#failed-list .badge--error")
            error = page.evaluate("getComputedStyle(document.documentElement).getPropertyValue('--error')")
            want = page.evaluate(
                "c => { const e = document.createElement('i'); e.style.color = c;"
                " document.body.appendChild(e); const v = getComputedStyle(e).color; e.remove(); return v; }",
                error.strip(),
            )
            for sel in ("#failed-count", "#failed-list .badge--error"):
                assert page.eval_on_selector(sel, "e => getComputedStyle(e).color") == want, sel
        finally:
            page.context.close()


def _settings(page: Page, stub: StubServer) -> None:
    page.goto(stub.url + "/_admin/")
    page.wait_for_selector(".admin-container")
    page.click(".tab[data-tab='settings']")
    page.wait_for_selector(".gen[data-idx='0']")


class TestAdmin:
    @pytest.mark.parametrize("width", [360, 400])
    def test_every_tab_is_on_screen(self, browser: Browser, stub: StubServer, width: int) -> None:
        page = _page(browser, width)
        try:
            page.goto(stub.url + "/_admin/")
            page.wait_for_selector(".tab[data-tab='connectivity']")
            _inside_viewport(page, ".tabs .tab")
            _no_sideways_scroll(page)
        finally:
            page.context.close()

    def test_a_row_keeps_remove_on_its_first_line(self, browser: Browser, stub: StubServer) -> None:
        page = _page(browser, 400)
        try:
            _settings(page, stub)
            remove = page.locator(".gen[data-idx='0'] [data-act='remove']").bounding_box()
            enabled = page.locator(".gen[data-idx='0'] .gen__check").first.bounding_box()
            assert remove and enabled
            assert abs((remove["y"] + remove["height"] / 2) - (enabled["y"] + enabled["height"] / 2)) < 8
        finally:
            page.context.close()

    def test_a_hint_sets_its_command_as_code(self, browser: Browser, stub: StubServer) -> None:
        page = _page(browser, 1280)
        try:
            _settings(page, stub)
            page.click("#ocr-env-details > summary")
            hint = page.locator("#ocr-cli-hint")
            hint.locator("code").first.wait_for()
            assert "`" not in hint.inner_text()
            assert hint.locator("code").first.inner_text().startswith("mokuro-bunko serve --ocr")
        finally:
            page.context.close()

    @pytest.mark.parametrize("width", [1280, 400])
    def test_a_benchmark_starting_does_not_rewrap_the_summary(
        self, browser: Browser, width: int
    ) -> None:
        heights = {}
        for running in (False, True):
            with StubServer() as stub:
                if running:
                    stub.state.script_bench("g-2", [BENCH_RUN_SEQUENCE[0]])
                page = _page(browser, width)
                try:
                    _settings(page, stub)
                    summary = page.locator(".gen[data-idx='1'] .gen__history-summary")
                    if running:
                        page.wait_for_selector(".gen[data-idx='1'] .bench-run")
                    box = summary.bounding_box()
                    assert box
                    heights[running] = round(box["height"])
                finally:
                    page.context.close()
        assert heights[True] == heights[False], heights

    @pytest.mark.parametrize("width", [1280, 400])
    def test_the_workers_placeholder_fits(self, browser: Browser, stub: StubServer, width: int) -> None:
        page = _page(browser, width)
        try:
            _settings(page, stub)
            page.click(".gen[data-idx='0'] .gen__tuning-summary")
            cell = page.locator(".gen[data-idx='0'] [data-act='workers'][data-stage='mokuro']")
            cell.wait_for()
            fits = cell.evaluate("""e => {
              const probe = document.createElement('span');
              const cs = getComputedStyle(e);
              probe.style.font = cs.font; probe.style.whiteSpace = 'pre';
              probe.textContent = e.placeholder; document.body.appendChild(probe);
              const need = probe.getBoundingClientRect().width; probe.remove();
              // Chromium's spin buttons take ~16px of a number box that has them.
              const spin = cs.appearance === 'textfield' ? 0 : 16;
              const room = e.clientWidth - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight) - spin;
              return [need, room]; }""")
            assert fits[0] <= fits[1], f"placeholder needs {fits[0]}px, has {fits[1]}px"
        finally:
            page.context.close()

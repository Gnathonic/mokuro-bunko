"""The queue page's clock: UTC in, the reader's own local time out.

The server never formats a local time -- it cannot know which zone anyone is
reading in -- so every instant crosses as an ISO-8601 Z string and becomes a
clock face here, in the browser, with the browser's own zone and its own
12/24-hour habit. These drive the REAL `queue/web/*` files against the stub,
on a FIXED `Date.now()` and an explicit `timezone_id`/`locale`, and assert the
exact rendered text: a test that computed the expected string with the same
`toLocaleTimeString` call the page uses would assert nothing at all.

Set `MOKURO_TEST_CHROMIUM` to an existing Chromium binary to run these
against a browser Playwright did not download itself.
"""

from __future__ import annotations

import os
from collections.abc import Generator
from datetime import datetime, timezone
from typing import TYPE_CHECKING, Any

import pytest

from .generations_stub import QUEUE_STATUS_ETA, StubServer

if TYPE_CHECKING:
    from playwright.sync_api import Page

CHROMIUM = os.environ.get("MOKURO_TEST_CHROMIUM") or None

# 2026-09-22 04:30 UTC = 13:30 in Asia/Tokyo. Every instant in
# `QUEUE_STATUS_ETA` is after it and on the same Tokyo day but one.
FIXED_NOW = datetime(2026, 9, 22, 4, 30, 0, tzinfo=timezone.utc)

try:
    from playwright.sync_api import sync_playwright

    def _launch(p: Any):
        kwargs: dict[str, Any] = {"headless": True}
        if CHROMIUM:
            kwargs["executable_path"] = CHROMIUM
        return p.chromium.launch(**kwargs)

    def _check_browsers() -> bool:
        try:
            with sync_playwright() as p:
                browser = _launch(p)
                browser.close()
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


@pytest.fixture
def stub() -> Generator[StubServer, None, None]:
    with StubServer() as server:
        yield server


def _queue_page(zone: str, locale: str) -> Generator[Page, None, None]:
    """A queue page in one named zone and locale, with time held still."""
    with sync_playwright() as p:
        browser = _launch(p)
        context = browser.new_context(
            viewport={"width": 1280, "height": 1200},
            timezone_id=zone,
            locale=locale,
        )
        page = context.new_page()
        # Only `Date.now()` / `new Date()` are pinned; `new Date(iso)` still
        # parses the server's instants normally, which is exactly the pair
        # the "is it today?" branch compares.
        page.clock.set_fixed_time(FIXED_NOW)
        yield page
        context.close()
        browser.close()


@pytest.fixture
def tokyo() -> Generator[Page, None, None]:
    yield from _queue_page("Asia/Tokyo", "en-GB")


@pytest.fixture
def new_york() -> Generator[Page, None, None]:
    yield from _queue_page("America/New_York", "en-US")


def _open(stub: StubServer, page: Page) -> Page:
    page.goto(stub.url + "/queue/")
    page.wait_for_selector(".machine .lane[data-job]")
    return page


class TestLocalTime:
    """The same instant, in two zones, in the words each reader expects."""

    def test_the_running_job_says_when_it_will_be_done(
        self, stub: StubServer, tokyo: Page
    ) -> None:
        stub.state.queue_status = QUEUE_STATUS_ETA
        _open(stub, tokyo)
        # 05:32 UTC is 14:32 in Tokyo. The countdown is from that instant on
        # the browser's own (pinned) clock, 04:30 UTC -- not the payload's
        # `eta_seconds`, which a cached payload would have frozen.
        assert (
            tokyo.locator(".lane[data-job]", has_text="Volume 07")
            .locator(".job__eta")
            .inner_text()
            == "done 14:32 (1h 2m left)"
        )

    def test_the_same_instant_in_another_zone_and_locale(
        self, stub: StubServer, new_york: Page
    ) -> None:
        stub.state.queue_status = QUEUE_STATUS_ETA
        _open(stub, new_york)
        # 05:32 UTC is 01:32 in New York, and a US reader is shown a
        # twelve-hour clock -- neither of which the server was told.
        assert (
            new_york.locator(".lane[data-job]", has_text="Volume 07")
            .locator(".job__eta")
            .inner_text()
            == "done 01:32 AM (1h 2m left)"
        )

    def test_a_volume_that_has_emitted_nothing_shows_the_startup(
        self, stub: StubServer, tokyo: Page
    ) -> None:
        """The owner's case, on the page.

        No page has come out of this volume, so there is no rate and the card
        must not extrapolate one from how long it has been waiting -- that is
        what opened a twelve-page volume at over a minute. It says what a
        session start costs instead, and marks it as the estimate it is.
        """
        stub.state.queue_status = QUEUE_STATUS_ETA
        _open(stub, tokyo)
        assert (
            tokyo.locator(".lane[data-job]", has_text="Volume 09")
            .locator(".job__eta")
            .inner_text()
            == "starting up (≈ 20s)"
        )

    def test_a_lookahead_volume_is_the_next_line(
        self, stub: StubServer, tokyo: Page
    ) -> None:
        """A session keeps the next volume submitted behind the one it is
        reading. It is not a volume in progress, so it gets no lane and no
        clock of its own: it is the machine's on-deck field."""
        stub.state.queue_status = QUEUE_STATUS_ETA
        _open(stub, tokyo)
        assert tokyo.locator(".lane[data-job]", has_text="Volume 11").count() == 0
        assert (
            tokyo.locator(".machine__next-text").inner_text()
            == "Dr STONE · Volume 11 · hayai-nova-ctd"
        )

    def test_each_pending_volume_carries_its_own_clock_time(
        self, stub: StubServer, tokyo: Page
    ) -> None:
        stub.state.queue_status = QUEUE_STATUS_ETA
        _open(stub, tokyo)
        assert tokyo.locator(
            ".pending-list__item", has_text="Volume 08"
        ).locator(".pending-list__eta").inner_text() == "~14:07"

    def test_a_guessed_length_is_marked_as_a_guess(
        self, stub: StubServer, tokyo: Page
    ) -> None:
        stub.state.queue_status = QUEUE_STATUS_ETA
        _open(stub, tokyo)
        row = tokyo.locator(".pending-list__item", has_text="Volume 10")
        # One mark for every prediction; the rough one is told apart by its
        # class (quieter, dotted) and its tooltip, not by a second symbol.
        assert row.locator(".pending-list__eta").inner_text() == "~14:40"
        assert row.locator(".pending-list__eta--rough").count() == 1

    def test_tomorrow_gets_its_date(self, stub: StubServer, tokyo: Page) -> None:
        # 2026-09-23 01:00 UTC is 10:00 on the 23rd in Tokyo -- a bare
        # "10:00" on a queue that finishes tomorrow morning would be the one
        # way this readout could actively mislead.
        stub.state.queue_status = QUEUE_STATUS_ETA
        _open(stub, tokyo)
        assert tokyo.locator(
            ".pending-list__item", has_text="Volume 13"
        ).locator(".pending-list__eta").inner_text() == "~23 Sept 10:00"

    def test_a_volume_nothing_can_predict_says_so(
        self, stub: StubServer, tokyo: Page
    ) -> None:
        stub.state.queue_status = QUEUE_STATUS_ETA
        _open(stub, tokyo)
        cell = tokyo.locator(
            ".pending-list__item", has_text="Volume 14"
        ).locator(".pending-list__eta")
        assert cell.inner_text() == "—"
        assert "paddle-manga" in (cell.get_attribute("title") or "")

    def test_the_header_says_when_the_whole_queue_ends(
        self, stub: StubServer, tokyo: Page
    ) -> None:
        stub.state.queue_status = QUEUE_STATUS_ETA
        _open(stub, tokyo)
        assert tokyo.locator("#queue-done").inner_text() == "everything done by 15:48"

    def test_no_end_is_shown_rather_than_a_wrong_one(
        self, stub: StubServer, tokyo: Page
    ) -> None:
        status = dict(QUEUE_STATUS_ETA)
        status["queue_done_at"] = None
        stub.state.queue_status = status
        _open(stub, tokyo)
        assert tokyo.locator("#queue-done").is_hidden()

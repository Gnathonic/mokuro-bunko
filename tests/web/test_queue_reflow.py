"""The queue page between jobs: nothing moves, nothing resizes, nothing blinks.

Owner: "The machine cards still flash out of existence triggering view reflow
animations between jobs." And, on how: "the machine cards are always there,
we replace the in-progress field, and the machine cards show what they are
doing" -- "Lock the machine card's on-deck section, if there's nothing say
none. Same for the currently processing data. Just render idle. Static shape
for the cards, only field data changes."

So these replay REAL payload sequences -- recorded from a library with two
processors reading volumes, switching rows (a new session: Loading) and going
idle and busy again (tests/web/fixtures/queue-replay-*.json) -- into the real
page, at every display level, on a desktop and a phone width, with and without
reduced motion, and hold it to this. For as long as a card is on the page:

* its SHAPE -- the tag and classes of every element in it, in order, and
  which are drawn -- never changes, and no element is ever added to it,
  removed from it, or hidden or shown in it;
* its fields say what the payload says: a lane with nothing to read says
  "Idle" (its pages, percent and time blank), a busy one names its
  volume, and the on-deck field names the next volume -- or says "None".

And while the set of connected machines stays the same:

* no card, line or lane is ever removed or inserted (not even moved);
* the browser records no layout shift -- save the queue's own: a volume that
  starts leaves the pending list, and the rows and sections under that list
  close up. That is the queue changing, not the page moving for nothing, and
  it is counted apart (`Findings.queue_shifts`);
* every card's, line's and lane's box stays exactly where and as big as it
  was, and so do the blocks under them (Speed, the "Pending OCR" heading, the
  top of the pending list) and the width the page lays out in (its
  scrollbar);
* no frame draws anything inside a card below full opacity.

Only text, attributes (the state) and the bar's fill may change.

`TestProbes` adds what the recorded runs never happened to send -- a row that
cannot start, a busy host, a held machine, an idle, a standby, a loading and a waiting
one, nothing and two things on deck, very long names, more volumes than
lanes, the machines in the other order, a longer pipeline and none, the speed
list growing and shrinking -- one change at a time, on the same terms.
`TestScrollbar` makes the page cross the fold both ways: a page scrollbar
coming and going moved every card sideways.

Set `MOKURO_TEST_CHROMIUM` to an existing Chromium binary to run these
against a browser Playwright did not download itself.
"""

from __future__ import annotations

import copy
import os
from collections.abc import Generator
from typing import TYPE_CHECKING, Any

import pytest

from .queue_replay import (
    ReplayServer,
    both_running,
    load_sequence,
    probe_sequence,
    replay,
    standby_sequence,
)

if TYPE_CHECKING:
    from playwright.sync_api import Browser

CHROMIUM = os.environ.get("MOKURO_TEST_CHROMIUM") or None

try:
    from playwright.sync_api import sync_playwright

    def _launch(p: Any) -> Any:
        kwargs: dict[str, Any] = {
            "headless": True,
            # Real scrollbars: Playwright hides them by default, and a page
            # scrollbar that comes and goes is one of the ways a card moves.
            "ignore_default_args": ["--hide-scrollbars"],
        }
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

LEVELS = ("minimal", "normal", "detailed")
WIDTHS = (1100, 400)
# Window heights inside the range of heights the page itself takes while the
# recorded sequence plays -- both before this fix and after it -- so its
# scrollbar comes and goes during the replay (a page scrollbar that comes and
# goes moved every card sideways). `TestScrollbar` crosses the fold on purpose.
HEIGHTS = {
    ("minimal", 1100): 600,
    ("minimal", 400): 600,
    ("normal", 1100): 1100,
    ("normal", 400): 1100,
    ("detailed", 1100): 1500,
    ("detailed", 400): 1800,
}
MOTION = {"motion": False, "reduced-motion": True}


@pytest.fixture(scope="module")
def browser() -> Generator[Browser, None, None]:
    with sync_playwright() as p:
        b = _launch(p)
        yield b
        b.close()


def _check(browser: Browser, sequence: dict[str, Any], level: str, width: int, motion: str) -> None:
    findings = replay(
        browser,
        sequence,
        width=width,
        height=HEIGHTS[(level, width)],
        reduced_motion=MOTION[motion],
    )
    problems = findings.problems()
    assert not problems, f"{level} {width}px {motion}: " + "\n  ".join(
        ["something moved between jobs:"] + problems
    )


@pytest.mark.parametrize("motion", list(MOTION))
@pytest.mark.parametrize("width", WIDTHS)
@pytest.mark.parametrize("level", LEVELS)
class TestRecordedSequences:
    def test_nothing_moves_resizes_or_blinks_between_jobs(
        self, browser: Browser, level: str, width: int, motion: str
    ) -> None:
        _check(browser, load_sequence(level), level, width, motion)


@pytest.mark.parametrize("motion", list(MOTION))
@pytest.mark.parametrize("width", WIDTHS)
@pytest.mark.parametrize("level", LEVELS)
class TestProbes:
    def test_one_change_at_a_time_moves_nothing(
        self, browser: Browser, level: str, width: int, motion: str
    ) -> None:
        _check(browser, probe_sequence(load_sequence(level)), level, width, motion)


@pytest.mark.parametrize("motion", list(MOTION))
@pytest.mark.parametrize("width", WIDTHS)
@pytest.mark.parametrize("level", LEVELS)
class TestStandby:
    def test_into_and_out_of_standby_moves_nothing(
        self, browser: Browser, level: str, width: int, motion: str
    ) -> None:
        """idle -> standby -> running -> idle -> standby -> running, and the
        other machine into standby and back: zero layout shift, no box moved,
        no card reshaped -- and the standby lanes really said so."""
        findings = replay(
            browser,
            standby_sequence(load_sequence(level)),
            width=width,
            height=HEIGHTS[(level, width)],
            reduced_motion=MOTION[motion],
        )
        problems = findings.problems()
        assert not problems, f"{level} {width}px {motion}: " + "\n  ".join(problems)
        assert findings.shift_score == 0 and not findings.shifts
        assert findings.standby_lanes > 0, findings.measurements()
        assert findings.idle_lanes > 0, findings.measurements()


def _page_height(browser: Browser, level: str, payload: dict[str, Any], width: int) -> int:
    sequence = {"level": level, "t0_wall": None, "steps": [{"t": 0.0, "payload": payload}]}
    with ReplayServer(sequence) as server:
        context = browser.new_context(viewport={"width": width, "height": 400})
        try:
            page = context.new_page()
            page.goto(server.url + "/queue/")
            page.wait_for_selector("body.queue-ready")
            return int(page.evaluate("() => document.documentElement.scrollHeight"))
        finally:
            context.close()


@pytest.mark.parametrize("width", WIDTHS)
@pytest.mark.parametrize("level", LEVELS)
class TestScrollbar:
    def test_the_page_crossing_the_fold_moves_no_card(
        self, browser: Browser, level: str, width: int
    ) -> None:
        """A volume starting takes a row out of the pending list. On a page
        just taller than the window that took the page scrollbar away, and
        every card moved sideways by its width (and back on a refill)."""
        base = both_running(load_sequence(level))
        long = copy.deepcopy(base)
        row = (
            base.get("pending")
            or [{"series": "Series P", "volume": "Volume 01", "generation": "gen-p"}]
        )[0]
        long["pending"] = [dict(row, volume=f"Volume {n:02d}") for n in range(10)]
        long["pending_count"] = 10
        short = copy.deepcopy(long)
        short["pending"], short["pending_count"] = [], 0
        tall, low = (_page_height(browser, level, p, width) for p in (long, short))
        assert tall > low + 40, (tall, low)
        sequence = {
            "level": level,
            "t0_wall": None,
            "steps": [{"t": 0.0, "payload": p} for p in (long, long, short, short, long)],
        }
        findings = replay(
            browser, sequence, width=width, height=(tall + low) // 2, reduced_motion=True
        )
        problems = findings.problems()
        assert not problems, f"{level} {width}px: " + "\n  ".join(problems)

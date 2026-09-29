"""The queue page at each display level, against the server's own shaping.

The stub feeds the page through `queue.shape.shape_status` -- the same
function the real endpoint uses -- so these check what a visitor and an admin
are really shown at `minimal`, `normal` and `detailed`: one card per machine
of one fixed shape (a "Processing" block per lane that says "Idle" when there
is nothing to read, and an "On deck" field that says "None" when nothing is
next), no 0% lookahead lane, redacted failures for a visitor, bars that keep
moving between polls (and do not with reduced motion), and polls answered 304.

Set `MOKURO_TEST_CHROMIUM` to an existing Chromium binary to run these
against a browser Playwright did not download itself.
"""

from __future__ import annotations

import copy
import os
from collections.abc import Generator
from typing import TYPE_CHECKING, Any

import pytest

from .generations_stub import StubServer, machine_phase, two_machines_status

if TYPE_CHECKING:
    from playwright.sync_api import Page

CHROMIUM = os.environ.get("MOKURO_TEST_CHROMIUM") or None

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


def _page(**context: Any) -> Generator[Page, None, None]:
    with sync_playwright() as p:
        browser = _launch(p)
        ctx = browser.new_context(viewport={"width": 1100, "height": 1400}, **context)
        yield ctx.new_page()
        ctx.close()
        browser.close()


@pytest.fixture
def page() -> Generator[Page, None, None]:
    yield from _page()


@pytest.fixture
def still_page() -> Generator[Page, None, None]:
    yield from _page(reduced_motion="reduce")


@pytest.fixture
def stub() -> Generator[StubServer, None, None]:
    with StubServer() as server:
        server.state.queue_status = two_machines_status()
        yield server


# A lane holding a volume (a lane with nothing to read has no `data-job`).
JOB = ".machine .lane[data-job]"
LOADING_BAR = ".machine .lane[data-state='loading'] .progress-bar"


def _open(stub: StubServer, page: Page, ready: str) -> Page:
    page.goto(stub.url + "/queue/")
    page.wait_for_selector(ready)
    return page


def _configuring(status: dict[str, Any]) -> dict[str, Any]:
    """tower with nothing running, being auto-benchmarked on the nova row."""
    status = machine_phase(status, "idle")
    status["connected_machines"] = [
        {"machine": "local", "slots": 1},
        {"machine": "tower", "slots": 1,
         "configuring": {"key": "g-2", "generation": "hayai-nova-ppocr", "auto": True}},
    ]
    return status


def _width(page: Page, selector: str) -> float:
    return float(page.eval_on_selector(selector, "el => parseFloat(el.style.width)"))


class TestNormal:
    def test_one_card_per_machine_with_its_on_deck_field(
        self, stub: StubServer, page: Page
    ) -> None:
        _open(stub, page, JOB)
        names = [n.lower() for n in page.locator(".machine__name").all_inner_texts()]
        # A visitor is never told a processor's (host)name.
        assert names == ["this server", "machine 1"]
        tower = page.locator(".machine").last
        assert tower.locator(".lane").count() == 1
        assert "Volume 07" in tower.locator(".lane[data-job]").inner_text()
        assert tower.locator(".lane__label").inner_text().lower() == "processing"
        assert tower.locator(".machine__next .field__label").inner_text().lower() == "on deck"
        assert (
            tower.locator(".machine__next-text").inner_text()
            == "Dr STONE · Volume 08 · hayai-nova-ctd"
        )
        # The on-deck volume never gets a lane, so never a 0% bar.
        assert page.locator(".lane", has_text="Volume 08").count() == 0
        # Nothing on deck on the other card: it says so.
        assert page.locator(".machine").first.locator(".machine__next-text").inner_text() == "None"
        # Pages -- and no speed: that is the admin panel's Processors card.
        assert tower.locator(".job__pages").inner_text().endswith(" / 192 pages")
        assert "pages/min" not in page.locator("#machines").inner_text()

    def test_no_speed_section_at_normal(self, stub: StubServer, page: Page) -> None:
        """Owner: the per-machine speed lines are too much below `detailed`."""
        _open(stub, page, JOB)
        page.wait_for_timeout(300)
        assert page.locator("#speed-section").is_hidden()
        assert page.locator(".speed__item").count() == 0
        assert "pages/min" not in page.locator("body").inner_text()

    def test_a_visitor_sees_the_reason_not_the_error(self, stub: StubServer, page: Page) -> None:
        _open(stub, page, "#failed-section:not([hidden])")
        failed = page.locator("#failed-list").inner_text()
        assert "engine error" in failed
        body = page.locator("body").inner_text()
        for secret in ("CUDA", "/srv/", "RTX 4090", "rocm", "tower"):
            assert secret not in body, secret
        assert page.locator("#backend-line").is_hidden()

    def test_an_admin_sees_the_error_the_log_and_the_hardware(
        self, stub: StubServer, page: Page
    ) -> None:
        stub.state.queue_admin = True
        _open(stub, page, "#failed-section:not([hidden])")
        failed = page.locator("#failed-list").inner_text()
        assert "CUDA out of memory" in failed
        assert "/srv/mokuro/logs/ocr/" in failed
        assert page.locator(".machine__label").last.inner_text() == "tower (RTX 4090)"
        assert page.locator(".machine__name").last.inner_text().lower() == "tower"
        assert page.locator("#backend").inner_text() == "ROCm"

    def test_a_returned_volume_says_who_gave_it_back_and_why(
        self, stub: StubServer, page: Page
    ) -> None:
        status = two_machines_status()
        status["pending_ocr"][0]["returned"] = {
            "count": 1, "class": "stalled", "machine": "tower", "at": 1790000000.0,
            "error": "no new byte for 120 s at byte 104,857,600",
        }
        stub.state.queue_status = status
        _open(stub, page, ".pending-list__returned:not([hidden])")
        line = page.locator(".pending-list__returned").first.inner_text()
        assert line == "returned by machine 1: download failed — will retry"
        assert "104,857,600" not in page.locator("body").inner_text()
        stub.state.queue_admin = True
        page.reload()
        page.wait_for_selector(".pending-list__returned:not([hidden])")
        line = page.locator(".pending-list__returned").first.inner_text()
        assert line.startswith("returned by tower: stalled: no new byte")

    def test_a_machine_held_for_failing_downloads_says_held(
        self, stub: StubServer, page: Page
    ) -> None:
        status = two_machines_status()
        status["current_jobs"] = [j for j in status["current_jobs"] if j["machine"] != "tower"]
        status["connected_machines"] = [
            {"machine": "local", "slots": 1},
            {"machine": "tower", "slots": 1, "held": "downloads",
             "held_until": 1790000600.0, "held_error": "missing: 404"},
        ]
        stub.state.queue_status = status
        _open(stub, page, ".machine .state-pill[data-state='held']")
        pill = page.locator(".machine").last.locator(".state-pill")
        assert pill.inner_text() == "Held"
        assert pill.get_attribute("title").startswith("downloads failing until ")
        assert "404" not in (pill.get_attribute("title") or "")
        # Said in the processing block's own slot, not in a row of its own.
        lane = page.locator(".machine").last.locator(".lane")
        assert lane.locator(".job__volume").inner_text() == "Held — its downloads keep failing"

    def test_a_machine_being_benchmarked_says_what_it_is_configuring(
        self, stub: StubServer, page: Page
    ) -> None:
        """Not "Idle": that read as a machine the library had lost."""
        stub.state.queue_status = _configuring(two_machines_status())
        _open(stub, page, ".machine .state-pill[data-state='configuring']")
        machine = page.locator(".machine").last
        assert machine.locator(".state-pill").inner_text() == "Configuring"
        lane = machine.locator(".lane")
        assert lane.get_attribute("data-state") == "configuring"
        assert lane.locator(".job__volume").inner_text() == "Auto configuring hayai-nova-ppocr"

    def test_a_row_that_cannot_start_on_a_machine_says_so(
        self, stub: StubServer, page: Page
    ) -> None:
        status = two_machines_status()
        status["connected_machines"] = [
            {"machine": "local", "slots": 1},
            {"machine": "tower", "slots": 1,
             "cannot_start": [{"generation": "paddle-manga", "until": 1790000600.0,
                               "failures": 2, "error": "no GPU execution provider"}]},
        ]
        stub.state.queue_status = status
        _open(stub, page, JOB)
        cannot = page.locator(".machine").last.locator(".machine__cannot")
        line = cannot.inner_text()
        assert line.startswith("paddle-manga cannot start here — next try ")
        assert "GPU" not in line, "the error is an admin's only"
        # One line, cut if it must be, and whole in the tooltip.
        assert cannot.get_attribute("title") == line
        # The line is there, empty, on a card with nothing failing: it holds
        # its height so the backoff coming and going never resizes a card.
        assert page.locator(".machine").first.locator(".machine__cannot").is_visible()

    def test_a_busy_host_says_so_on_its_card(self, stub: StubServer, page: Page) -> None:
        status = two_machines_status()
        status["current_jobs"][0]["host_busy"] = True
        stub.state.queue_status = status
        _open(stub, page, ".job__busy:has-text('host busy')")
        tower = page.locator(".machine").last
        assert tower.locator(".job__busy").first.is_visible()
        assert tower.locator(".job__busy").first.inner_text() == "host busy"
        # The other card's badge keeps its cell in the row, with nothing in
        # it: the badge coming and going must not add a row to the card.
        other = page.locator(".machine").first.locator(".job__busy").first
        assert other.inner_text() == ""
        assert other.evaluate("e => getComputedStyle(e).display") != "none"

    def test_no_pipeline_at_normal(self, stub: StubServer, page: Page) -> None:
        _open(stub, page, JOB)
        assert page.locator(".stages").count() == 0
        assert page.locator(".job__rate").count() == 0


class TestMinimal:
    def test_one_compact_card_per_machine_and_the_counts(
        self, stub: StubServer, page: Page
    ) -> None:
        stub.state.queue_level = "minimal"
        _open(stub, page, ".mline")
        lines = page.locator(".mline")
        assert lines.count() == 2
        assert page.locator(".machine").count() == 0
        first = lines.last.inner_text()
        assert "machine 1" in first and "Dr STONE · Volume 07" in first and "%" in first
        assert "tower" not in page.locator("body").inner_text()
        # The end of the whole queue, on the reader's clock.
        assert page.locator("#queue-done").inner_text().startswith("everything done by ")
        # The on-deck volume is its card's "Next" line, never a lane.
        assert page.locator(".mline__what", has_text="Volume 08").count() == 0
        assert lines.last.locator(".mline__next-text").inner_text() == "Dr STONE · Volume 08"
        assert lines.first.locator(".mline__next-text").inner_text() == "None"
        assert page.locator("#pending-ocr-count").inner_text() == "3"
        # The pending list is back, compact: one line a volume, no numbers,
        # no attempt badges, no explanatory paragraph.
        assert page.locator("#pending-ocr-list .pc").count() == 3
        assert page.locator("#pending-ocr-list .pending-list__position").count() == 0
        assert page.locator("#pending-ocr-list .badge--muted").count() == 0
        assert page.locator("#pending-ocr-order").is_hidden()
        first_row = page.locator("#pending-ocr-list .pc").first
        assert first_row.locator(".pc__what").inner_text() == "Dr STONE · Volume 09"
        assert first_row.locator(".pc__gen").inner_text() == "hayai-nova-ctd"
        assert first_row.bounding_box()["height"] <= 30
        assert page.locator("#failed-section").is_hidden()
        assert page.locator("#thumb-section").is_hidden()
        assert page.locator("#run-order").is_hidden()
        # No speed at all at minimal.
        assert page.locator("#speed-section").is_hidden()
        assert "pages/min" not in page.locator("body").inner_text()


    def test_a_machine_being_benchmarked_says_so_at_minimal(
        self, stub: StubServer, page: Page
    ) -> None:
        stub.state.queue_level = "minimal"
        stub.state.queue_status = _configuring(two_machines_status())
        _open(stub, page, ".mline .state-pill[data-state='configuring']")
        line = page.locator(".mline").last
        assert line.locator(".state-pill").inner_text() == "Configuring"
        assert line.locator(".mline__what").inner_text() == "Auto configuring hayai-nova-ppocr"


class TestDetailed:
    def test_stages_and_rate_details(self, stub: StubServer, page: Page) -> None:
        stub.state.queue_level = "detailed"
        _open(stub, page, ".stages")
        tower = page.locator(".machine").last
        assert "widen detect" in tower.locator(".stages__verdict").inner_text()
        rate = tower.locator(".job__rate").inner_text()
        # Its REAL throughput on this layer, not the ETA model's rate.
        assert "~48 pages/min here" in rate and "latency 2.4 s a volume" in rate
        assert "(session)" not in rate
        # Still one card per machine and no lookahead card.
        assert page.locator(".machine").count() == 2
        assert page.locator(".lane", has_text="Volume 08").count() == 0


class TestLiveness:
    def test_the_level_follows_the_setting_without_a_reload(
        self, stub: StubServer, page: Page
    ) -> None:
        _open(stub, page, JOB)
        stub.state.queue_level = "minimal"
        page.wait_for_selector(".mline", timeout=5000)
        assert page.locator(".machine").count() == 0

    def test_unchanged_polls_are_304s(self, stub: StubServer, page: Page) -> None:
        _open(stub, page, JOB)
        page.wait_for_timeout(3500)
        codes = [code for code, _ in stub.state.status_polls]
        assert codes[0] == 200
        assert len(codes) >= 3, codes
        assert set(codes[1:]) == {304}, codes
        # ...and a 304 leaves the page as it was.
        assert page.locator(".machine").count() == 2

    def test_the_bar_keeps_moving_between_polls(self, stub: StubServer, page: Page) -> None:
        _open(stub, page, JOB)
        fill = ".machine:last-child .lane .progress-bar__fill"
        before = _width(page, fill)
        page.wait_for_timeout(2500)
        after = _width(page, fill)
        assert after > before, (before, after)
        assert after < 100

    def test_reduced_motion_holds_still(self, stub: StubServer, still_page: Page) -> None:
        _open(stub, still_page, JOB)
        fill = ".machine:last-child .lane .progress-bar__fill"
        assert _width(still_page, fill) == 62
        still_page.wait_for_timeout(2000)
        assert _width(still_page, fill) == 62
        duration = still_page.eval_on_selector(
            fill, "el => getComputedStyle(el).transitionDuration"
        )
        assert duration == "0s"
        assert still_page.locator(".is-entering, .is-leaving").count() == 0

    def test_a_finished_volume_leaves_its_machine_idle_in_place(
        self, stub: StubServer, page: Page
    ) -> None:
        _open(stub, page, JOB)
        before = _geometry(page)
        stub.state.queue_status = machine_phase(two_machines_status(), "idle")
        page.wait_for_function(
            "() => document.querySelector('.machine:last-child .state-pill').textContent === 'Idle'",
            timeout=5000,
        )
        assert page.locator(".machine").count() == 2, "a connected machine keeps its card"
        assert _geometry(page) == before

    def test_idle_to_configuring_and_back_moves_nothing(self, stub: StubServer, page: Page) -> None:
        stub.state.queue_status = machine_phase(two_machines_status(), "idle")
        _open(stub, page, ".machine:last-child .state-pill[data-state='idle']")
        before = _geometry(page)
        stub.state.queue_status = _configuring(two_machines_status())
        page.wait_for_selector(".machine:last-child .state-pill[data-state='configuring']")
        assert _geometry(page) == before
        stub.state.queue_status = machine_phase(two_machines_status(), "idle")
        page.wait_for_selector(".machine:last-child .state-pill[data-state='idle']")
        assert _geometry(page) == before

    def test_a_machine_that_disconnects_takes_its_card(self, stub: StubServer, page: Page) -> None:
        _open(stub, page, JOB)
        status = machine_phase(two_machines_status(), "idle")
        status["connected_machines"] = [{"machine": "local", "slots": 1}]
        stub.state.queue_status = status
        page.wait_for_function("() => document.querySelectorAll('.machine').length === 1",
                               timeout=5000)


class TestPhone:
    def test_no_horizontal_scroll_at_400px(self, stub: StubServer) -> None:
        for level in ("minimal", "normal", "detailed"):
            stub.state.queue_level = level
            for p in _page():
                p.set_viewport_size({"width": 400, "height": 900})
                _open(stub, p, "#machines > *")
                overflow = p.evaluate(
                    "() => document.documentElement.scrollWidth - window.innerWidth"
                )
                assert overflow <= 0, (level, overflow)


class TestAdminSetting:
    SIGNED_IN = (
        "sessionStorage.setItem('mokuro_auth', 'YWRtaW46YWRtaW5wYXNz');"
        "sessionStorage.setItem('mokuro_user', '{\"username\":\"admin\",\"role\":\"admin\"}');"
    )

    def test_the_admin_panel_sets_the_level_and_the_queue_page_follows(
        self, stub: StubServer, page: Page
    ) -> None:
        queue = page.context.new_page()
        _open(stub, queue, JOB)

        page.add_init_script(self.SIGNED_IN)
        page.goto(stub.url + "/_admin/")
        page.wait_for_selector(".admin-container")
        page.click(".tab[data-tab='settings']")
        select = page.locator("#settings-queue-display")
        select.wait_for()
        # The select is on screen before the settings it shows have arrived
        # (its first option until then): wait for the server's level.
        page.wait_for_function(
            "() => document.getElementById('settings-queue-display').value === 'normal'",
            timeout=5000,
        )
        assert select.input_value() == "normal"
        select.select_option("minimal")
        page.click("#settings-queue button:has-text('Save queue settings')")
        page.wait_for_function("() => document.body.innerText.includes('Queue settings saved')")
        assert stub.state.settings["queue"]["display"] == "minimal"

        # The open queue page follows on its next poll, no reload.
        queue.wait_for_selector(".mline", timeout=5000)
        assert queue.locator(".machine").count() == 0


class TestStaleLogin:
    def test_a_login_the_server_refuses_is_forgotten(self, stub: StubServer, page: Page) -> None:
        stub.state.queue_auth_failed = True
        page.add_init_script(TestAdminSetting.SIGNED_IN)
        _open(stub, page, JOB)
        page.wait_for_function(
            "() => sessionStorage.getItem('mokuro_auth') === null", timeout=5000
        )
        page.wait_for_timeout(2500)
        assert stub.state.status_auth[0] is True
        assert stub.state.status_auth[-1] is False, "the stale login is not sent again"
        # ...and the visitor's page is still there.
        assert page.locator(".machine").count() == 2


class TestSpeedLayout:
    def test_detailed_has_one_compact_line_per_layer(
        self, stub: StubServer, page: Page
    ) -> None:
        stub.state.queue_level = "detailed"
        _open(stub, page, ".speed__item")
        rows = page.locator(".speed__item").all_inner_texts()
        assert len(rows) == 2
        assert "hayai-nova-ctd" in rows[0] and "~48 pages/min" in rows[0]
        assert "mokuro" in rows[1] and "~21 pages/min" in rows[1]
        # No per-machine breakdown on the public page.
        text = page.locator("#speed-section").inner_text()
        for word in ("machine 1", "this server", "idle", "average", "combined"):
            assert word not in text, word
        for item in page.locator(".speed__item").all():
            assert item.bounding_box()["height"] <= 30

    def test_every_speed_row_lays_out_alike_at_400px(self, stub: StubServer) -> None:
        stub.state.queue_level = "detailed"
        for p in _page():
            p.set_viewport_size({"width": 400, "height": 900})
            _open(stub, p, ".speed__item")
            lefts = p.eval_on_selector_all(
                ".speed__text", "els => els.map(e => Math.round(e.getBoundingClientRect().left))"
            )
            tops = p.eval_on_selector_all(
                ".speed__item",
                "els => els.map(e => Math.round(e.querySelector('.speed__text')"
                ".getBoundingClientRect().top - e.getBoundingClientRect().top))",
            )
            assert len(lefts) == 2
            assert len(set(tops)) == 1, tops



class TestDraftBenchmark:
    def test_a_draft_benchmark_is_not_named_to_a_visitor(
        self, stub: StubServer, page: Page
    ) -> None:
        stub.state.seed_bench_queue(["draft-aaaaaaaa"])
        _open(stub, page, ".machine, .mline")
        assert "draft-" not in page.locator("body").inner_text()


def _shape(page: Page) -> list[str]:
    """Every card's shape: the tag and classes of each element in it, in
    order, and whether it is drawn at all."""
    return page.evaluate(
        """() => [...document.querySelectorAll('.machine, .mline')].map(card =>
            [card, ...card.querySelectorAll('*')].map(n => n.tagName + '.' +
                n.className.trim().split(/\\s+/).join('.') +
                (n.hidden || getComputedStyle(n).display === 'none' ? '{none}' : '') +
                (getComputedStyle(n).visibility !== 'visible' ? '{unseen}' : '')
            ).join(' '))"""
    )


def _geometry(page: Page) -> list[tuple[float, float]]:
    """(top, height) of every card or line in the Now section, and of what follows."""
    return page.evaluate(
        """() => [...document.querySelectorAll('.machine, .mline, #speed-section')]
            .map(e => { const r = e.getBoundingClientRect(); return [r.top, r.height]; })"""
    )


class TestFixedSize:
    """Owner: the cards resizing between jobs is no good. A machine's card
    keeps one size and place while it is connected; only its state changes."""

    PHASES = ("running", "loading", "idle", "waiting", "running")
    PILL = {"running": "Running", "loading": "Loading", "waiting": "Waiting", "idle": "Idle"}

    def _walk(
        self, stub: StubServer, page: Page, level: str, width: int, start: str = "running"
    ) -> None:
        stub.state.queue_level = level
        page.set_viewport_size({"width": width, "height": 1400})
        base = two_machines_status()
        stub.state.queue_status = machine_phase(base, start)
        card = ".mline" if level == "minimal" else ".machine"
        _open(stub, page, card)
        page.wait_for_timeout(400)
        before = _geometry(page)
        shape = _shape(page)
        for phase in self.PHASES:
            stub.state.queue_status = machine_phase(base, phase)
            pill = page.locator(f"{card}:last-child .state-pill")
            page.wait_for_function(
                "([sel, text]) => document.querySelector(sel).textContent === text",
                arg=[f"{card}:last-child .state-pill", self.PILL[phase]],
                timeout=5000,
            )
            assert pill.inner_text() == self.PILL[phase]
            page.wait_for_timeout(400)
            assert _geometry(page) == before, (level, width, phase)
            # ...and every card keeps every element it has: only what they
            # say (and the state attribute) changed.
            assert _shape(page) == shape, (level, width, phase)

    def test_normal_cards_hold_their_size_through_every_state(
        self, stub: StubServer, page: Page
    ) -> None:
        for width in (1100, 400):
            self._walk(stub, page, "normal", width)

    def test_minimal_lines_hold_theirs(self, stub: StubServer, page: Page) -> None:
        for width in (1100, 400):
            self._walk(stub, page, "minimal", width)

    def test_detailed_cards_never_shrink(self, stub: StubServer, page: Page) -> None:
        self._walk(stub, page, "detailed", 1100)

    def test_detailed_cards_do_not_grow_either(self, stub: StubServer, page: Page) -> None:
        # A machine that connects idle -- or is idle when the page loads --
        # has no stage table yet. Its first job must not push the card, and
        # everything below it, down: the detail block is a fixed size.
        for width in (1100, 400):
            self._walk(stub, page, "detailed", width, start="idle")

    def test_a_second_lane_s_first_job_does_not_grow_the_card(
        self, stub: StubServer, page: Page
    ) -> None:
        stub.state.queue_level = "detailed"
        page.set_viewport_size({"width": 1100, "height": 1400})
        # This server has two slots; the second has run nothing since the load.
        one_lane = two_machines_status()
        one_lane["connected_machines"][0]["slots"] = 2
        both = copy.deepcopy(one_lane)
        (local,) = [job for job in both["current_jobs"] if job.get("machine") == "local"]
        both["current_jobs"].append(dict(copy.deepcopy(local), slot=1, volume="Volume 13"))
        stub.state.queue_status = one_lane
        _open(stub, page, ".machine")
        page.wait_for_timeout(400)
        before = _geometry(page)
        stub.state.queue_status = both
        page.wait_for_function(
            "() => document.querySelectorAll('.machine:first-child .lane[data-job]').length === 2",
            timeout=5000,
        )
        page.wait_for_timeout(400)
        assert _geometry(page) == before

    def test_the_loading_bar_shimmers_in_the_same_slot(self, stub: StubServer, page: Page) -> None:
        stub.state.queue_status = machine_phase(two_machines_status(), "loading")
        _open(stub, page, LOADING_BAR)
        bar = page.locator(".machine").last.locator(".progress-bar")
        assert bar.evaluate("e => getComputedStyle(e, '::after').animationName") == "queue-shimmer"
        # A long title is cut, not wrapped.
        volume = page.locator(".machine").last.locator(".job__volume")
        page.set_viewport_size({"width": 400, "height": 900})
        assert volume.evaluate("e => e.scrollWidth > e.clientWidth")

    def test_a_load_past_its_estimate_is_still_loading(
        self, stub: StubServer, page: Page
    ) -> None:
        # The estimate ran out (no `startup_seconds` left) but the runner has
        # not said it is ready: still Loading, still shimmering.
        status = machine_phase(two_machines_status(), "loading")
        (tower,) = [job for job in status["current_jobs"] if job.get("machine") == "tower"]
        tower["startup_seconds"] = None
        stub.state.queue_status = status
        _open(stub, page, LOADING_BAR)
        card = page.locator(".machine").last
        assert card.locator(".state-pill").inner_text() == "Loading"
        assert card.locator(".job__eta").inner_text() == "starting up"

    def test_nothing_on_a_card_or_a_list_animates(self, stub: StubServer, page: Page) -> None:
        """No fade, no slide, no growth: a card or a row is simply there. (A
        lane's content fading in from nothing at every job change read as
        the card flashing out of existence.) Only a bar's fill moves."""
        still = "e => [getComputedStyle(e).transitionDuration, getComputedStyle(e).animationName]"
        _open(stub, page, JOB)
        for selector in (".machine", ".lane", ".job__head", ".job__meta", ".pending-list__item"):
            assert page.eval_on_selector(selector, still) == ["0s", "none"], selector
        stub.state.queue_level = "detailed"
        page.wait_for_selector(".speed__item")
        for selector in (".speed__item", ".job__detail", ".stage"):
            assert page.eval_on_selector(selector, still) == ["0s", "none"], selector
        stub.state.queue_level = "minimal"
        page.wait_for_selector(".mline .state-pill")
        for selector in (".mline", ".mline__lane", ".mline__head", ".pc"):
            assert page.eval_on_selector(selector, still) == ["0s", "none"], selector

    def test_reduced_motion_has_no_shimmer_and_no_fades(
        self, stub: StubServer, still_page: Page
    ) -> None:
        stub.state.queue_status = machine_phase(two_machines_status(), "loading")
        _open(stub, still_page, LOADING_BAR)
        bar = still_page.locator(".machine").last.locator(".progress-bar")
        assert bar.evaluate("e => getComputedStyle(e, '::after').animationName") == "none"
        assert still_page.eval_on_selector(
            ".machine", "e => getComputedStyle(e).transitionDuration"
        ) == "0s"


class TestIdleAndNone:
    """Owner: "Lock the machine card's on-deck section, if there's nothing say
    none. Same for the currently processing data. Just render idle." A lane
    with nothing to read is the same lane, saying so in its own fields."""

    # An idle lane's cells are blank (a non-breaking space holds each one's
    # line): a dash in every one read as data gone missing.
    BLANK = "\u00a0"

    def test_an_idle_card_says_idle_and_none(self, stub: StubServer, page: Page) -> None:
        stub.state.queue_status = machine_phase(two_machines_status(), "idle")
        _open(stub, page, ".machine .lane:not([data-job])")
        tower = page.locator(".machine").last
        assert tower.locator(".machine__head .state-pill").inner_text() == "Idle"
        lane = tower.locator(".lane")
        assert lane.count() == 1
        assert lane.get_attribute("data-state") == "idle"
        assert lane.locator(".lane__label").inner_text().lower() == "processing"
        assert lane.locator(".job__volume").inner_text() == "Idle"
        for field in (".job__series", ".job__pages", ".job__pct", ".job__eta"):
            assert lane.locator(field).text_content() == self.BLANK, field
        assert lane.locator(".badge--gen").inner_text() == ""
        assert _width(page, ".machine:last-child .progress-bar__fill") == 0
        assert tower.locator(".machine__next-text").inner_text() == "None"

    def test_minimal_idle_says_idle_and_next_none(self, stub: StubServer, page: Page) -> None:
        stub.state.queue_level = "minimal"
        stub.state.queue_status = machine_phase(two_machines_status(), "idle")
        _open(stub, page, ".mline .lane:not([data-job])")
        line = page.locator(".mline").last
        assert line.locator(".state-pill").inner_text() == "Idle"
        assert line.locator(".mline__what").inner_text() == "Idle"
        assert line.locator(".job__pct").text_content() == self.BLANK
        assert line.locator(".job__eta").text_content() == self.BLANK
        assert line.locator(".mline__next-label").inner_text() == "On deck:"
        assert line.locator(".mline__next-text").inner_text() == "None"

    def test_detailed_idle_keeps_its_stage_table(self, stub: StubServer, page: Page) -> None:
        stub.state.queue_level = "detailed"
        stub.state.queue_status = machine_phase(two_machines_status(), "idle")
        _open(stub, page, ".machine .lane:not([data-job])")
        lane = page.locator(".machine").last.locator(".lane")
        assert lane.locator(".job__rate").text_content() == self.BLANK
        verdict = lane.locator(".stages__verdict").inner_text()
        assert verdict == "Stage timings show here while a volume is read."
        assert lane.locator(".stage").count() == 3
        assert lane.locator(".stage__name").all_inner_texts() == ["", "", ""]
        assert lane.locator(".stages__legend .stages__key").count() == 4


class TestMinimalPending:
    def test_capped_with_a_count_of_the_rest(self, stub: StubServer, page: Page) -> None:
        status = two_machines_status()
        item = status["pending_ocr"][0]
        status["pending_ocr"] = [dict(item, volume=f"Volume {n:02d}") for n in range(25)]
        stub.state.queue_status = status
        stub.state.queue_level = "minimal"
        _open(stub, page, ".pc")
        assert page.locator(".pc").count() == 10
        assert page.locator("#pending-more").inner_text() == "+15 more"
        assert page.locator("#pending-ocr-count").inner_text() == "25"



HELD = [{"generation": "paddle-manga", "reason": "No connected machine can run bf16"}]


class TestHeldRows:
    """A row no connected machine can run (a forced precision nobody's card
    supports) is held: an admin is told which, one plain line each."""

    def test_an_admin_is_told_which_rows_nobody_can_run(
        self, stub: StubServer, page: Page
    ) -> None:
        stub.state.queue_admin = True
        stub.state.queue_status["held_rows"] = copy.deepcopy(HELD)
        _open(stub, page, "#held-rows:not([hidden])")
        assert page.locator("#held-rows li").all_inner_texts() == [
            "paddle-manga: No connected machine can run bf16"
        ]
        # Near the processing-hold notice, in the Machines section.
        assert page.locator("#machines-section #held-rows").count() == 1

    def test_none_held_takes_no_room(self, stub: StubServer, page: Page) -> None:
        stub.state.queue_admin = True
        stub.state.queue_status["held_rows"] = []
        _open(stub, page, JOB)
        assert page.locator("#held-rows").is_hidden()
        assert page.locator("#held-rows").bounding_box() is None
        # And it goes again when the row can run: the next poll hides it.
        stub.state.queue_status["held_rows"] = copy.deepcopy(HELD)
        page.wait_for_selector("#held-rows:not([hidden])", timeout=5000)
        top = page.locator("#machines").bounding_box()
        stub.state.queue_status["held_rows"] = []
        page.wait_for_selector("#held-rows", state="hidden", timeout=5000)
        assert page.locator("#held-rows li").count() == 0
        assert top and page.locator("#machines").bounding_box()["y"] < top["y"]

    def test_a_visitor_is_never_told(self, stub: StubServer, page: Page) -> None:
        stub.state.queue_status["held_rows"] = copy.deepcopy(HELD)
        for level in ("minimal", "normal", "detailed"):
            stub.state.queue_level = level
            page.goto(stub.url + "/queue/")
            page.wait_for_function("() => document.body.classList.contains('queue-ready')")
            assert page.locator("#held-rows").is_hidden(), level
            assert "No connected machine can run" not in page.locator("body").inner_text()


STANDBY_TEXT = "Faster machines will finish the queue sooner"


def _standby(status: dict[str, Any], on: bool = True) -> dict[str, Any]:
    """tower connected with nothing running; `on`: the scheduler is leaving
    it the queued work it could run, for faster machines (raw
    ``connected_machines[i].standby``, which `shape_status` makes its state)."""
    status = machine_phase(status, "idle")
    status["connected_machines"] = [
        {"machine": "local", "slots": 1},
        {"machine": "tower", "slots": 1, **({"standby": True} if on else {})},
    ]
    return status


def _cards(page: Page, level: str):
    return page.locator(".mline" if level == "minimal" else ".machine")


def _what_slot(level: str) -> str:
    return ".mline__what" if level == "minimal" else ".job__volume"


class TestStandby:
    """A machine the earliest-finish scheduler is leaving idle on purpose:
    "Standby", and why, in its empty lane -- at every level, for anyone."""

    @pytest.mark.parametrize("admin", [False, True])
    @pytest.mark.parametrize("level", ["minimal", "normal", "detailed"])
    def test_standby_says_so_and_why(
        self, stub: StubServer, page: Page, level: str, admin: bool
    ) -> None:
        stub.state.queue_level = level
        stub.state.queue_admin = admin
        stub.state.queue_status = _standby(two_machines_status())
        _open(stub, page, ".state-pill[data-state='standby']")
        tower, local = _cards(page, level).last, _cards(page, level).first
        assert tower.locator(".state-pill").inner_text() == "Standby"
        what = tower.locator(_what_slot(level))
        assert what.inner_text() == STANDBY_TEXT
        assert what.get_attribute("title") == STANDBY_TEXT
        # The busy machine is not in standby.
        assert local.locator(".state-pill").inner_text() == "Running"
        assert page.locator(".state-pill[data-state='standby']").count() == 1

    @pytest.mark.parametrize("level", ["minimal", "normal", "detailed"])
    def test_an_idle_machine_is_idle(self, stub: StubServer, page: Page, level: str) -> None:
        stub.state.queue_level = level
        stub.state.queue_status = _standby(two_machines_status(), on=False)
        _open(stub, page, ".state-pill[data-state='idle']")
        tower = _cards(page, level).last
        assert tower.locator(".state-pill").inner_text() == "Idle"
        assert tower.locator(_what_slot(level)).inner_text() == "Idle"
        assert STANDBY_TEXT not in page.locator("body").inner_text()
        assert page.locator(".state-pill[data-state='standby']").count() == 0

    def test_standby_comes_and_goes_with_the_payload(self, stub: StubServer, page: Page) -> None:
        stub.state.queue_status = _standby(two_machines_status(), on=False)
        _open(stub, page, ".state-pill[data-state='idle']")
        tower = page.locator(".machine").last
        box = tower.bounding_box()
        stub.state.queue_status = _standby(two_machines_status())
        page.wait_for_selector(".state-pill[data-state='standby']", timeout=5000)
        assert tower.bounding_box() == box
        stub.state.queue_status = _standby(two_machines_status(), on=False)
        page.wait_for_selector(".state-pill[data-state='idle']", timeout=5000)
        assert page.locator(".state-pill[data-state='standby']").count() == 0
        assert tower.bounding_box() == box

    @pytest.mark.parametrize("width", [1280, 400])
    @pytest.mark.parametrize("level", ["minimal", "normal", "detailed"])
    def test_no_pill_label_is_clipped(
        self, stub: StubServer, page: Page, level: str, width: int
    ) -> None:
        page.set_viewport_size({"width": width, "height": 1400})
        stub.state.queue_level = level
        stub.state.queue_status = _standby(two_machines_status())
        _open(stub, page, ".state-pill[data-state='standby']")
        fits = page.eval_on_selector_all(
            ".state-pill",
            "pills => pills.map(p => [p.textContent, p.scrollWidth, p.clientWidth])",
        )
        assert any(text == "Standby" for text, _, _ in fits)
        clipped = [f for f in fits if f[1] > f[2]]
        assert not clipped, clipped

"""Playwright UI tests for the OCR generations list and the queue page.

These drive the REAL page files (`admin/web/*`, `queue/web/*`) against
`generations_stub`, which answers the `/api/ocr/generations` and
`/queue/api/status` contract. No config, no database, no OCR: what is under
test is the browser behaviour -- name validation, auto-derivation, reorder,
primary exclusivity, the PUT body, where a 400 lands, and whether the queue
page still renders against a server that predates generations.

Set `MOKURO_TEST_CHROMIUM` to an existing Chromium binary to run these against
a browser Playwright did not download itself; without it they launch the
bundled one, and skip when there is none -- the same gate the other tests in
this directory use.
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.request
from collections.abc import Generator
from typing import TYPE_CHECKING, Any

import pytest

from .generations_stub import (
    BENCH_AUTO_BEST,
    BENCH_CANCELLED,
    BENCH_FAILED,
    BENCH_RUN_SEQUENCE,
    BENCH_SHORT_WINDOW,
    BENCH_STALE_SPEC,
    BENCH_TUNED,
    PREEMPTED_SAMPLE,
    QUEUE_STATUS_SESSION,
    RUNTIME_MISMATCH,
    RUNTIME_NOT_INSTALLED,
    RUNTIME_SKIP,
    RUNTIME_UNAVAILABLE,
    StubServer,
)

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


# Overrides the shared `page` fixture for this module only, so a Chromium
# given by MOKURO_TEST_CHROMIUM is honoured without touching the other tests.
@pytest.fixture
def page() -> Generator[Page, None, None]:
    with sync_playwright() as p:
        browser = _launch(p)
        context = browser.new_context(viewport={"width": 1280, "height": 1000})
        yield context.new_page()
        context.close()
        browser.close()


@pytest.fixture
def stub() -> Generator[StubServer, None, None]:
    with StubServer() as server:
        yield server


def open_admin(stub: StubServer, page: Page) -> Page:
    """Sign in and land on the Settings tab with the generation list loaded.

    A test that has to set the stub up BEFORE the page loads -- a benchmark
    already running, a POST that will be refused -- calls this itself instead
    of taking the `admin` fixture, which loads the page too early for that.
    """
    page.add_init_script(
        "sessionStorage.setItem('mokuro_token', 'YWRtaW46YWRtaW5wYXNz');"
        "sessionStorage.setItem('mokuro_user', '{\"username\":\"admin\",\"role\":\"admin\"}');"
    )
    page.goto(stub.url + "/_admin/")
    page.wait_for_selector(".admin-container")
    page.click(".tab[data-tab='settings']")
    page.wait_for_selector(".gen[data-idx='0']")
    return page


@pytest.fixture
def admin(stub: StubServer, page: Page) -> Page:
    """The admin panel, signed in, on the Settings tab with the list loaded."""
    return open_admin(stub, page)


def row(page: Page, index: int):
    return page.locator(f".gen[data-idx='{index}']")


# What a closed select can actually show, measured rather than eyeballed: the
# selected option's text in the control's own font against the room inside its
# padding (which is what reserves the arrow).
CLIP_PROBE = """
() => Array.from(document.querySelectorAll('#gen-list select')).map((el) => {
  const opt = el.options[el.selectedIndex];
  const style = getComputedStyle(el);
  const ctx = document.createElement('canvas').getContext('2d');
  ctx.font = style.fontStyle + ' ' + style.fontWeight + ' ' + style.fontSize + ' ' + style.fontFamily;
  const pad = parseFloat(style.paddingLeft) + parseFloat(style.paddingRight);
  return {
    id: el.id,
    text: opt ? opt.textContent : '',
    needed: Math.ceil(ctx.measureText(opt ? opt.textContent : '').width),
    room: Math.floor(el.clientWidth - pad),
  };
})
"""


def assert_nothing_clipped(page: Page) -> None:
    probes = page.evaluate(CLIP_PROBE)
    assert probes, "no selects were measured"
    clipped = [p for p in probes if p["needed"] > p["room"]]
    assert not clipped, f"clipped: {clipped}"


class TestGenerationList:
    def test_rows_render_in_order(self, admin: Page) -> None:
        assert admin.locator(".gen").count() == 5
        names = [row(admin, i).locator(".gen__name").input_value() for i in range(5)]
        assert names == ["mokuro", "hayai-nova-ctd", "hayai-nova-ppocr-manga",
                         "paddle-manga", "ppocr-manga"]
        # No sample file name under the name any more.
        assert admin.locator(".gen__file").count() == 0

    def test_the_intro_is_one_sentence_and_the_arrows_explain_the_order(self, admin: Page) -> None:
        intro = admin.locator("#settings-ocr-generations-field .form-hint").first.inner_text()
        assert intro == ("Each generation writes one sidecar per volume, top row first; "
                         "readers open the primary by default.")
        up = row(admin, 1).locator("[data-act='up']")
        assert up.get_attribute("aria-label") == "Move hayai-nova-ctd up"
        assert "full priority" in (up.get_attribute("title") or "")

    def test_the_folded_row_is_its_history_in_one_line(self, admin: Page) -> None:
        """Owner: the status line is a folded History; the congestion verdict
        is gone from it (the Congestion details under the pools keep it)."""
        history = row(admin, 1).locator(".gen__history")
        assert history.get_attribute("open") is None
        summary = row(admin, 1).locator(".gen__history-summary")
        assert summary.inner_text() == "History — 31/34 volumes · 1 skipped"
        # The skipped count's reason is the folded line's tooltip.
        title = summary.get_attribute("title") or ""
        assert title.startswith(summary.inner_text()) and "uploaded with pages missing" in title
        # This server has run g-1 (the Processors card's numbers): its REAL
        # pages a minute, not a benchmark.
        admin.wait_for_function(
            "(document.querySelector(\".gen[data-idx='0'] .gen__history-summary\") || {})"
            ".textContent === 'History — 34/34 volumes · 21 pages/min'")
        for i in range(5):
            text = row(admin, i).inner_text()
            assert "widen detect" not in text and "singled out" not in text
            assert "No queue runs recorded yet" not in text
        summary.click()
        body = row(admin, 1).locator(".gen__history-body").inner_text()
        assert "volumes" not in body  # one count, on the folded line only
        assert "this server no volume of this generation finished here yet" in body

    def test_the_verdict_is_under_the_pools(self, admin: Page) -> None:
        row(admin, 1).locator(".gen__tuning-summary").click()
        row(admin, 1).locator(".gen__why-summary").click()
        assert "widen detect" in row(admin, 1).locator(".cong__verdict").inner_text()

    def test_locked_detector_is_disabled_and_shows_the_engines_own(self, admin: Page) -> None:
        detector = row(admin, 4).locator("[data-act='detector']")
        assert detector.is_disabled()
        assert detector.input_value() == "ppocr-manga"

    def test_patch_budget_only_where_the_engine_uses_it(self, admin: Page) -> None:
        assert row(admin, 1).locator("[data-act='patch']").count() == 1
        assert row(admin, 3).locator("[data-act='patch']").count() == 0

    def test_tuning_shows_stage_pools_and_congestion(self, admin: Page) -> None:
        row(admin, 1).locator(".gen__tuning-summary").click()
        body = row(admin, 1).locator(".gen__tuning-body")
        assert body.is_visible()
        # The stage keys come from the server, never from the client.
        stages = body.locator(".pools__table tbody tr th .mono").all_inner_texts()
        assert stages == ["detect", "engine", "post"]
        # A recognizer on the card is not fixed at one: its cell is how many
        # copies of the model to run there, blank for one.
        copies = body.locator("[data-act='workers'][data-stage='engine']")
        assert copies.count() == 1
        assert copies.get_attribute("placeholder") == "1"
        assert copies.get_attribute("max") == "8"
        assert "copies of the model on the card" in body.inner_text()
        # Derived widths are placeholders; an override is a value.
        detect_workers = body.locator("[data-act='workers'][data-stage='detect']")
        assert detect_workers.input_value() == "2"
        assert detect_workers.get_attribute("placeholder") == "1"
        # The congestion is folded under the table, the verdict with it.
        why = body.locator(".gen__why")
        assert why.get_attribute("open") is None
        assert "Averaged over 3 runs" not in body.inner_text()
        why.locator("summary").click()
        assert "Averaged over 3 runs" in body.inner_text()
        assert "widen detect" in why.locator(".cong__verdict").inner_text()

    def test_the_table_comes_first_and_says_what_a_blank_box_means(self, admin: Page) -> None:
        row(admin, 1).locator(".gen__tuning-summary").click()
        body = row(admin, 1).locator(".gen__tuning-body")
        order = body.evaluate(
            "b => [...b.querySelectorAll('.pools__table, .gen__why')].map(e => e.className)")
        assert order[0].startswith("pools__table") and order[-1].startswith("gen__why"), order
        tips = body.locator("thead .pools__th-tip")
        # Device's tip is where to place the stages; the other two say what
        # a blank box means.
        assert tips.all_inner_texts() == ["Device", "Workers", "Queue capacity"]
        for title in [tips.nth(i).get_attribute("title") or "" for i in (1, 2)]:
            assert title.startswith("Leave a box empty to let the server size it")
        assert "Leave a box empty" not in body.inner_text()


class TestTheDeviceColumn:
    """Addendum 7: the pools table is where a stage's device is chosen."""

    def test_a_stage_holding_a_model_gets_a_select_and_one_without_a_label(
        self, admin: Page
    ) -> None:
        row(admin, 1).locator(".gen__tuning-summary").click()
        body = row(admin, 1).locator(".gen__tuning-body")
        assert body.locator("[data-act='device'][data-stage='detect']").count() == 1
        assert body.locator("[data-act='device'][data-stage='engine']").count() == 1
        # post assembles the page and writes JSON: no model, so no choice.
        assert body.locator("[data-act='device'][data-stage='post']").count() == 0
        assert "CPU" in body.locator(".pools__table tbody tr:last-child").inner_text()

    def test_auto_says_what_it_resolved_to(self, admin: Page) -> None:
        row(admin, 1).locator(".gen__tuning-summary").click()
        select = row(admin, 1).locator("[data-act='device'][data-stage='engine']")
        assert select.input_value() == "auto"
        assert "Auto → GPU 0" in select.inner_text()

    def test_a_cpu_only_model_is_locked_with_the_reason(self, admin: Page) -> None:
        row(admin, 4).locator(".gen__tuning-summary").click()
        body = row(admin, 4).locator(".gen__tuning-body")
        assert body.locator("[data-act='device'][data-stage='detect']").count() == 0
        assert "onnxruntime" in body.inner_text()

    def test_choosing_a_card_fixes_the_workers_cell_at_one(
        self, stub: StubServer, admin: Page
    ) -> None:
        row(admin, 1).locator(".gen__tuning-summary").click()
        body = row(admin, 1).locator(".gen__tuning-body")
        assert body.locator("[data-act='workers'][data-stage='detect']").count() == 1
        body.locator("[data-act='device'][data-stage='detect']").select_option("gpu:0")
        # The server is asked what the table now is, and answers width one.
        admin.wait_for_function(
            "document.querySelectorAll(\".gen[data-idx='1'] [data-act='workers'][data-stage='detect']\").length === 0"
        )
        assert stub.state.derived[-1]["pools"]["stage_device"] == {"detect": "gpu:0"}
        # A detector is not a recognizer: on a card it is one model, fixed.
        assert "one model on one device" in body.inner_text()

    def test_engine_copies_ride_along_in_the_put(self, stub: StubServer, admin: Page) -> None:
        row(admin, 1).locator(".gen__tuning-summary").click()
        row(admin, 1).locator("[data-act='workers'][data-stage='engine']").fill("3")
        admin.click("#gen-save-btn")
        admin.wait_for_selector("#gen-save-note:not([hidden])")
        pools = stub.state.last_put["generations"][1]["pools"]
        assert pools["stage_workers"]["engine"] == 3

    def test_the_engine_on_the_cpu_has_no_copies(self, stub: StubServer, admin: Page) -> None:
        """On the CPU a recognizer's threads ARE its compute: one model, fixed."""
        row(admin, 1).locator(".gen__tuning-summary").click()
        body = row(admin, 1).locator(".gen__tuning-body")
        body.locator("[data-act='device'][data-stage='engine']").select_option("cpu")
        admin.wait_for_function(
            "!document.querySelector(\".gen[data-idx='1'] .gen__tuning-body\").innerText"
            ".includes('copies of the model on the card')"
        )
        assert body.locator("[data-act='workers'][data-stage='engine']").count() == 0
        assert "one model on one device" in body.inner_text()

    def test_the_choice_rides_along_in_the_put(self, stub: StubServer, admin: Page) -> None:
        row(admin, 1).locator(".gen__tuning-summary").click()
        row(admin, 1).locator("[data-act='device'][data-stage='detect']").select_option("cpu")
        admin.click("#gen-save-btn")
        admin.wait_for_selector("#gen-save-note:not([hidden])")
        pools = stub.state.last_put["generations"][1]["pools"]
        assert pools["stage_device"] == {"detect": "cpu"}
        # "auto" is the ABSENCE of a choice and is not sent as a value.
        assert "engine" not in pools["stage_device"]

    def test_the_served_rows_engine_stage_keeps_the_device_and_the_forks_pool(
        self, admin: Page
    ) -> None:
        """Addendum 7's cells on Addendum 8's shape.

        The served road has three stages; the Device select is on the one
        that IS the serve process, and its Workers cell is the fork's own
        ``--num_workers`` -- editable although one process holds one model,
        with no ceiling of ours and no derived number of ours to suggest.
        """
        row(admin, 0).locator(".gen__tuning-summary").click()
        body = row(admin, 0).locator(".gen__tuning-body")
        stages = body.locator(".pools__table tbody tr th .mono").all_inner_texts()
        assert stages == ["feed", "mokuro", "post"]
        # The one model of this road, and the only device cell that is a
        # control: feed spools a file and post assembles a dict.
        assert body.locator("[data-act='device'][data-stage='mokuro']").count() == 1
        assert body.locator("[data-act='device'][data-stage='feed']").count() == 0
        assert body.locator("[data-act='device'][data-stage='post']").count() == 0
        workers = body.locator("[data-act='workers'][data-stage='mokuro']")
        assert workers.count() == 1
        # "auto (fork default)" did not fit the cell; the fork part is on hover.
        assert workers.get_attribute("placeholder") == "auto"
        assert "own default" in (workers.get_attribute("title") or "")
        # Uncapped: our max_workers of 1 is structural, not a limit on the
        # fork's own pipeline.
        assert workers.get_attribute("max") is None
        assert "one model on one device" not in body.inner_text()

    def test_the_served_rows_device_choice_rides_along_in_the_put(
        self, stub: StubServer, admin: Page
    ) -> None:
        """``mokuro=cpu`` is what starts the serve process with --force_cpu."""
        row(admin, 0).locator(".gen__tuning-summary").click()
        row(admin, 0).locator("[data-act='device'][data-stage='mokuro']").select_option("cpu")
        admin.wait_for_function(
            "document.querySelector(\".gen[data-idx='0'] [data-act='device'][data-stage='mokuro']\")"
            ".value === 'cpu'"
        )
        admin.click("#gen-save-btn")
        admin.wait_for_selector("#gen-save-note:not([hidden])")
        assert stub.state.last_put["generations"][0]["pools"]["stage_device"] == {"mokuro": "cpu"}

    def test_the_placement_advice_is_the_device_headers_tooltip(self, admin: Page) -> None:
        """No link to the docs: the advice is short enough for a tooltip."""
        row(admin, 1).locator(".gen__tuning-summary").click()
        body = row(admin, 1).locator(".gen__tuning-body")
        device = body.locator(".pools__table thead th").nth(1)
        assert device.inner_text() == "Device"
        tip = device.locator(".pools__th-tip").get_attribute("title") or ""
        assert "Run detection on the CPU" in tip and "without a card" in tip
        assert body.locator(".pools__hint--devices").count() == 0
        assert "How to place stages" not in body.inner_text()
        assert body.locator("a[href*='configuration.md']").count() == 0


class TestNameRules:
    def test_grammar_error_blocks_save(self, admin: Page) -> None:
        row(admin, 0).locator(".gen__name").fill("Bad Name.x")
        assert "lowercase letters" in row(admin, 0).locator(".gen__msg").inner_text()
        assert admin.locator("#gen-save-btn").is_disabled()

    def test_duplicate_name_flags_both_rows(self, admin: Page) -> None:
        row(admin, 2).locator(".gen__name").fill("hayai-nova-ctd")
        assert "already uses this name" in row(admin, 1).locator(".gen__msg").inner_text()
        assert "already uses this name" in row(admin, 2).locator(".gen__msg").inner_text()
        assert admin.locator("#gen-save-btn").is_disabled()

    def test_reserved_names_and_prefix_rejected(self, admin: Page) -> None:
        name = row(admin, 2).locator(".gen__name")
        for value in ("original", "gcv", "tr-en"):
            name.fill(value)
            assert "reserved" in row(admin, 2).locator(".gen__msg").inner_text(), value
            assert admin.locator("#gen-save-btn").is_disabled()

    def test_name_follows_engine_and_detector_until_edited(self, admin: Page) -> None:
        # Row 3 is hayai-nova + ppocr-manga, named for both, so it still
        # follows (row 2 already holds hayai-nova-ctd, hence the -2).
        row(admin, 2).locator("[data-act='detector']").select_option("ctd")
        assert row(admin, 2).locator(".gen__name").input_value() == "hayai-nova-ctd-2"

        # Once edited by hand the name is the user's and stops following.
        row(admin, 2).locator(".gen__name").fill("my-nova")
        row(admin, 2).locator("[data-act='detector']").select_option("ctd")
        assert row(admin, 2).locator(".gen__name").input_value() == "my-nova"

        # ... with a way back.
        row(admin, 2).locator("[data-act='reset-name']").click()
        assert row(admin, 2).locator(".gen__name").input_value() == "hayai-nova-ctd-2"

    def test_default_name_collision_gets_a_suffix(self, admin: Page) -> None:
        # Row 3 -> hayai-nova + ctd, which row 2 already holds.
        row(admin, 2).locator("[data-act='detector']").select_option("ctd")
        assert row(admin, 2).locator(".gen__name").input_value() == "hayai-nova-ctd-2"
        assert row(admin, 2).locator(".gen__msg").is_hidden()

    def test_engine_change_re_derives_the_stages_without_a_save(
        self, stub: StubServer, admin: Page
    ) -> None:
        """Addendum 7: the server answers for the row as edited, at once."""
        row(admin, 2).locator(".gen__tuning-summary").click()
        row(admin, 2).locator("[data-act='engine']").select_option("paddle-manga")
        admin.wait_for_function(
            "document.querySelectorAll('.gen[data-idx=\"2\"] .pools__table tbody tr').length === 3"
        )
        # The stage keys came from the server, and the widths this row had set
        # went with the stages they named.
        assert stub.state.derived[-1]["engine"] == "paddle-manga"
        assert stub.state.derived[-1]["pools"]["stage_workers"] == {}
        stages = row(admin, 2).locator(".pools__table tbody tr th .mono").all_inner_texts()
        assert stages == ["detect", "engine", "post"]


class TestOrderingAndPrimary:
    def test_move_down_reorders_and_keeps_keyboard_focus(self, admin: Page) -> None:
        admin.locator(".gen[data-idx='0'] [data-act='down']").focus()
        admin.keyboard.press("Enter")
        assert row(admin, 0).locator(".gen__name").input_value() == "hayai-nova-ctd"
        assert row(admin, 1).locator(".gen__name").input_value() == "mokuro"
        # Focus followed the row, so a second press keeps moving the same row.
        focused = admin.evaluate("document.activeElement.dataset.idx")
        assert focused == "1"
        assert admin.evaluate("document.activeElement.dataset.act") == "down"

    def test_first_row_cannot_move_up_and_last_cannot_move_down(self, admin: Page) -> None:
        assert admin.locator(".gen[data-idx='0'] [data-act='up']").is_disabled()
        assert admin.locator(".gen[data-idx='4'] [data-act='down']").is_disabled()

    def test_primary_is_exclusive(self, admin: Page) -> None:
        row(admin, 1).locator("[data-act='primary']").check()
        assert row(admin, 1).locator("[data-act='primary']").is_checked()
        assert not row(admin, 0).locator("[data-act='primary']").is_checked()

    def test_disabling_the_primary_row_moves_the_flag(self, admin: Page) -> None:
        row(admin, 0).locator("[data-act='enabled']").uncheck()
        assert row(admin, 1).locator("[data-act='primary']").is_checked()
        assert admin.locator("#gen-banner").is_hidden()

    def test_a_list_with_no_primary_says_what_it_costs(self, admin: Page) -> None:
        admin.evaluate("genRows.forEach(r => { r.primary = false; }); renderGenerations();")
        banner = admin.locator("#gen-banner")
        assert banner.is_visible()
        assert "bare Volume 01.mokuro" in banner.inner_text()
        assert admin.locator("#gen-save-btn").is_disabled()

    def test_remove_confirms_and_says_files_stay(self, admin: Page) -> None:
        row(admin, 4).locator("[data-act='remove']").click()
        assert "stay on disk" in row(admin, 4).locator(".gen__confirm").inner_text()
        row(admin, 4).locator("[data-act='remove-yes']").click()
        assert admin.locator(".gen").count() == 4

    def test_add_generation_seeds_and_focuses_the_name(self, admin: Page) -> None:
        admin.click("#gen-add-btn")
        assert admin.locator(".gen").count() == 6
        new = row(admin, 5)
        # First non-monolithic engine, first detector, derived name.
        assert new.locator("[data-act='engine']").input_value() == "hayai-nova"
        assert new.locator("[data-act='detector']").input_value() == "ppocr-manga"
        assert new.locator(".gen__name").input_value() == "hayai-nova-ppocr-manga-2"
        assert admin.evaluate("document.activeElement.dataset.act") == "name"


class TestSaving:
    def test_put_body_is_the_contract_row_shape_in_order(self, stub: StubServer, admin: Page) -> None:
        admin.locator(".gen[data-idx='0'] [data-act='down']").click()
        admin.click("#gen-save-btn")
        admin.wait_for_selector("#gen-save-note:not([hidden])")

        sent = stub.state.last_put
        assert list(sent.keys()) == ["generations"]
        assert [g["id"] for g in sent["generations"]] == ["g-2", "g-1", "g-3", "g-4", "g-5"]

        first = sent["generations"][0]
        assert first == {
            "id": "g-2",
            "name": "hayai-nova-ctd",
            "primary": False,
            "enabled": True,
            "engine": "hayai-nova",
            "detector": "ctd",
            "patch_budget": 512,
            # The row's one precision mode: always said for an engine that
            # takes one, and never inside the (per-machine) pools.
            "precision": "auto-accuracy",
            "pools": {"stage_workers": {"detect": 2}, "queue_capacity": {}, "stage_device": {}},
        }
        # An engine that brings its own detector stores null, not the id.
        assert sent["generations"][1]["detector"] is None
        assert sent["generations"][4]["detector"] is None
        # mokuro takes a mode too; ppocr-manga fixes its own and says none.
        assert sent["generations"][1]["precision"] == "auto-accuracy"
        assert "precision" not in sent["generations"][4]
        assert all("precision" not in g["pools"] for g in sent["generations"])

    def test_a_new_row_is_sent_without_an_id(self, stub: StubServer, admin: Page) -> None:
        admin.click("#gen-add-btn")
        admin.click("#gen-save-btn")
        admin.wait_for_selector("#gen-save-note:not([hidden])")
        sent = stub.state.last_put["generations"]
        assert "id" not in sent[-1]
        assert sent[-1]["name"] == "hayai-nova-ppocr-manga-2"

    def test_pool_overrides_ride_along_and_blanks_mean_auto(self, stub: StubServer, admin: Page) -> None:
        row(admin, 1).locator(".gen__tuning-summary").click()
        row(admin, 1).locator("[data-act='workers'][data-stage='detect']").fill("")
        row(admin, 1).locator("[data-act='capacity'][data-stage='post']").fill("6")
        admin.click("#gen-save-btn")
        admin.wait_for_selector("#gen-save-note:not([hidden])")
        pools = stub.state.last_put["generations"][1]["pools"]
        assert pools == {"stage_workers": {}, "queue_capacity": {"post": 6}, "stage_device": {}}

    def test_unsaved_indicator_and_revert(self, admin: Page) -> None:
        assert admin.locator("#gen-dirty").is_hidden()
        row(admin, 2).locator(".gen__name").fill("nova-hd")
        assert admin.locator("#gen-dirty").is_visible()
        admin.click("#gen-revert-btn")
        admin.wait_for_function(
            "document.querySelector('.gen[data-idx=\"2\"] .gen__name').value === 'hayai-nova-ppocr-manga'"
        )
        assert admin.locator("#gen-dirty").is_hidden()

    def test_live_apply_outcome_is_reported(self, admin: Page) -> None:
        row(admin, 2).locator(".gen__name").fill("nova-hd")
        admin.click("#gen-save-btn")
        admin.wait_for_selector("#gen-save-note:not([hidden])")
        assert "applied" in admin.locator("#gen-save-note").inner_text()

    def test_a_400_lands_on_the_row_and_field_it_names(self, stub: StubServer, admin: Page) -> None:
        stub.state.put_response = {
            "error": "Generation 3 already uses the name “hayai-nova-ctd”.",
            "row": 2,
            "field": "name",
        }
        stub.state.put_status = 400
        row(admin, 2).locator(".gen__name").fill("nova-hd")
        admin.click("#gen-save-btn")
        admin.wait_for_selector(".gen[data-idx='2'] .gen__msg:not([hidden])")

        assert "already uses the name" in row(admin, 2).locator(".gen__msg").inner_text()
        assert row(admin, 2).locator(".gen__name").get_attribute("aria-invalid") == "true"
        # ... and nowhere else.
        assert row(admin, 1).locator(".gen__msg").is_hidden()
        assert admin.locator("#gen-save-note").is_hidden()

    def test_a_400_clears_when_the_row_is_edited(self, stub: StubServer, admin: Page) -> None:
        stub.state.put_response = {"error": "no", "row": 2, "field": "name"}
        stub.state.put_status = 400
        admin.click("#gen-save-btn")
        admin.wait_for_selector(".gen[data-idx='2'] .gen__msg:not([hidden])")
        row(admin, 2).locator(".gen__name").fill("nova-hd")
        assert row(admin, 2).locator(".gen__msg").is_hidden()


class TestBenchmarkButton:
    """ADDENDUM 5: the gates that made a benchmark wait on Save are gone. The
    button is enabled for every row, all the time -- the only thing that ever
    takes it away is THIS row's own benchmark already being queued or
    running, which is covered in `TestBenchmarkQueueing`."""

    def test_label_follows_the_engine(self, admin: Page) -> None:
        # A served row has pools to size like any other, so it is tunable and
        # the button says so. (Only an engine with no pipeline at all gets the
        # bare "Benchmark".)
        assert row(admin, 0).locator("[data-act='bench-start']").inner_text() == "Benchmark & tune"
        assert row(admin, 1).locator("[data-act='bench-start']").inner_text() == "Benchmark & tune"

    def test_says_what_pressing_it_will_do(self, admin: Page) -> None:
        """As the button's tooltip and accessible description -- not as a
        paragraph beside it on every row."""
        button = row(admin, 1).locator("[data-act='bench-start']")
        why = button.get_attribute("title") or ""
        described = admin.locator("#" + button.get_attribute("aria-describedby")).text_content()
        assert described == why
        box = row(admin, 1).locator(".bench-bar__why").bounding_box()
        assert box is None or box["width"] <= 1, "no visible paragraph"
        assert "real pages from your library" in why
        assert "row exactly as shown here" in why
        assert "OCR queue" in why
        assert "restart when the benchmarks finish" in why
        assert "queue more benchmarks behind" in why

    def test_enabled_with_unsaved_edits(self, admin: Page) -> None:
        button = row(admin, 1).locator("[data-act='bench-start']")
        row(admin, 2).locator(".gen__name").fill("nova-hd")
        assert not button.is_disabled()

    def test_enabled_on_a_disabled_row(self, admin: Page) -> None:
        assert not row(admin, 4).locator("[data-act='bench-start']").is_disabled()

    def test_enabled_on_a_brand_new_unsaved_row(self, admin: Page) -> None:
        admin.click("#gen-add-btn")
        assert not row(admin, 5).locator("[data-act='bench-start']").is_disabled()

    def test_enabled_while_another_row_is_running(self, stub: StubServer, page: Page) -> None:
        stub.state.script_bench("g-3", [BENCH_RUN_SEQUENCE[1]])
        admin = open_admin(stub, page)
        admin.wait_for_selector(".gen[data-idx='2'] .bench-run")
        # Row 2 (g-3) is running; every OTHER row's button is still live.
        assert not row(admin, 1).locator("[data-act='bench-start']").is_disabled()
        assert not row(admin, 0).locator("[data-act='bench-start']").is_disabled()
        assert not row(admin, 3).locator("[data-act='bench-start']").is_disabled()
        # ... and the row that IS running offers a way out instead of a
        # start button.
        assert row(admin, 2).locator("[data-act='bench-cancel']").count() == 1
        assert row(admin, 2).locator("[data-act='bench-start']").count() == 0


class TestBenchmarkRun:
    def test_progress_walks_from_the_queue_to_a_result(self, stub: StubServer, admin: Page) -> None:
        admin.locator(".gen[data-idx='1'] [data-act='bench-start']").click()

        # ADDENDUM 5: the POST body is `{"spec": ...}` built from row 1
        # (g-2, hayai-nova-ctd) exactly as it is shown -- the same shape the
        # PUT body's row carries, minus id/name/primary/enabled.
        assert stub.state.bench_posts == [(
            "g-2",
            {"spec": {
                "engine": "hayai-nova", "detector": "ctd", "patch_budget": 512,
                "precision": "auto-accuracy",
                "pools": {"stage_workers": {"detect": 2}, "queue_capacity": {},
                          "stage_device": {}},
            }},
        )]

        # The head of the line is already `state: "running"` (position 0);
        # pre-empting whatever the OCR queue was running is the first thing
        # it says, since nothing measured beside a running job would count.
        admin.wait_for_selector(
            ".gen[data-idx='1'] .bench-run__title:has-text('Pausing the OCR queue')"
        )

        admin.wait_for_selector(".gen[data-idx='1'] .bench-run__title:has-text('Trial 1 of 8')")
        running = row(admin, 1).locator(".bench-run")
        assert "detect ×1, engine ×1, post ×1" in running.inner_text()
        assert "12 of 32 pages" in running.inner_text()
        assert "1.48 pages/s" in running.inner_text()

        admin.wait_for_selector(".gen[data-idx='1'] .bench-run__title:has-text('Trial 3 of 8')")
        assert "detect ×3, engine ×1, post ×1" in row(admin, 1).locator(".bench-run").inner_text()

        # Done. The trials are the tell that the finished object itself
        # landed: the summary this row started with has them stripped.
        admin.wait_for_selector(".gen[data-idx='1'] .bench-trials")
        assert row(admin, 1).locator(".bench-run").count() == 0
        assert "126 pages/min" in row(admin, 1).locator(".bench-res__rate").inner_text()

    def test_cancel_stops_it_and_says_so(self, stub: StubServer, admin: Page) -> None:
        stub.state.bench_on_post["g-2"] = [BENCH_RUN_SEQUENCE[0]]
        admin.locator(".gen[data-idx='1'] [data-act='bench-start']").click()
        admin.wait_for_selector(".gen[data-idx='1'] [data-act='bench-cancel']")
        admin.locator(".gen[data-idx='1'] [data-act='bench-cancel']").click()
        admin.wait_for_selector(".gen[data-idx='1'] .bench-out:has-text('Benchmark cancelled')")
        assert stub.state.bench_deletes == ["g-2"]
        # The button is a button again.
        assert not row(admin, 1).locator("[data-act='bench-start']").is_disabled()

    def test_a_run_already_going_is_picked_up_on_load(self, stub: StubServer, page: Page) -> None:
        # The list only carries FINISHED benchmarks, so a reload mid-run has
        # to ask each row; without that the progress would vanish.
        stub.state.script_bench("g-2", [BENCH_RUN_SEQUENCE[2]])
        admin = open_admin(stub, page)
        admin.wait_for_selector(".gen[data-idx='1'] .bench-run__title:has-text('Trial 3 of 8')")
        assert stub.state.bench_posts == []

    def test_a_same_row_409_is_shown_in_the_servers_words(self, stub: StubServer, admin: Page) -> None:
        # ADDENDUM 6: 409 now means only "this SAME key is already queued or
        # running" -- the UI shows whatever the server said, verbatim, rather
        # than inventing a "one at a time" message of its own.
        stub.state.bench_post_response = {"error": "A benchmark for this generation is already running."}
        stub.state.bench_post_status = 409
        admin.locator(".gen[data-idx='1'] [data-act='bench-start']").click()
        admin.wait_for_selector(".gen[data-idx='1'] .bench-bar__note")
        note = row(admin, 1).locator(".bench-bar__note").inner_text()
        assert "already running" in note
        assert "one at a time" not in note

    def test_a_refusal_is_shown_in_the_words_the_server_used(self, stub: StubServer, admin: Page) -> None:
        stub.state.bench_post_response = {
            "error": "The engines environment is not installed, so hayai-nova cannot be benchmarked."
        }
        stub.state.bench_post_status = 400
        admin.locator(".gen[data-idx='1'] [data-act='bench-start']").click()
        admin.wait_for_selector(".gen[data-idx='1'] .bench-bar__note")
        assert ("engines environment is not installed"
                in row(admin, 1).locator(".bench-bar__note").inner_text())
        # Nothing is pretending to run.
        assert row(admin, 1).locator(".bench-run").count() == 0

    def test_failed_shows_the_error_and_keeps_the_last_result(self, stub: StubServer, page: Page) -> None:
        stub.state.script_bench("g-2", [BENCH_FAILED])
        admin = open_admin(stub, page)
        admin.wait_for_selector(".gen[data-idx='1'] .bench-out--failed")
        assert "HIP out of memory" in row(admin, 1).locator(".bench-out--failed").inner_text()
        assert "126 pages/min" in row(admin, 1).locator(".bench-res").inner_text()

    def test_cancelled_keeps_the_last_result_too(self, stub: StubServer, page: Page) -> None:
        stub.state.script_bench("g-2", [BENCH_CANCELLED])
        admin = open_admin(stub, page)
        admin.wait_for_selector(".gen[data-idx='1'] .bench-out:has-text('cancelled')")
        assert "126 pages/min" in row(admin, 1).locator(".bench-res").inner_text()


class TestBenchmarkResult:
    def test_the_folded_result_is_the_answer(self, admin: Page) -> None:
        """Owner: the folded result says what won, how fast and how long ago,
        on one line, whole in its tooltip."""
        summary = row(admin, 1).locator(".bench-res__summary")
        text = summary.inner_text()
        assert text == "Benchmark result — best: detect ×3 · 126 pages/min · 2 d ago"
        assert summary.get_attribute("title") == text
        assert summary.evaluate("e => getComputedStyle(e).textOverflow") == "ellipsis"
        # A one-pass engine has nothing to tune: just its speed.
        mono = row(admin, 0).locator(".bench-res__summary").inner_text()
        assert mono == "Benchmark result — 16 pages/min · 13 h ago"
        # A generation nobody has measured claims no speed at all.
        assert row(admin, 2).locator(".bench-res").count() == 0

    def test_numbers_are_formatted_for_a_person(self, admin: Page) -> None:
        body = row(admin, 1).locator(".bench-res").inner_text()
        assert "126 pages/min" in body
        assert "0.48 s per page" in body
        assert "1 m 35 s" in body          # a 200-page volume, reading only
        assert "2 h 10 m" in body          # the rest of the library
        assert "5,230 pages left" in body
        assert "First page after" in body  # startup, as information
        assert "11 s" in body
        assert "3.0 GB" in body            # peak VRAM
        assert "2.4 GB" in body            # peak RAM
        assert "AMD Ryzen 9 7950X (16 cores)" in body
        assert "AMD Radeon RX 9070 XT" in body
        assert "rocm backend" in body
        assert "2 days ago" in body
        # Never the raw seconds or the raw rate they were formatted from.
        for raw in ("0.476", "7810", "95 s", "2.1000", "3120", "2410"):
            assert raw not in body, raw

    def test_a_null_is_left_out_rather_than_printed(self, admin: Page) -> None:
        row(admin, 0).locator(".bench-res__summary").click()
        body = row(admin, 0).locator(".bench-res").inner_text()
        assert "null" not in body.lower()
        assert "NaN" not in body
        assert "undefined" not in body
        # This host cannot say what its GPU is, how much memory it used, or
        # how much library is left: each of those is simply absent.
        assert "Peak VRAM" not in body
        assert "Peak RAM" not in body
        assert "rest of the library" not in body
        assert "Intel Core i5-8250U (4 cores) · cpu backend" in body
        # ... and what it CAN say is all there.
        assert "16 pages/min" in body
        assert "12 m 21 s" in body

    def test_a_monolithic_result_has_nothing_to_tune(self, admin: Page) -> None:
        row(admin, 0).locator(".bench-res__summary").click()
        assert "no pools to tune" in row(admin, 0).locator(".bench-res__conclusion").inner_text()
        assert row(admin, 0).locator(".bench-trials").count() == 0
        assert row(admin, 0).locator("[data-act='bench-apply']").count() == 0

    def test_the_trials_tell_the_tuning_story(self, stub: StubServer, page: Page) -> None:
        # The trials only ever arrive from the bench endpoint: the list
        # carries the same result with them stripped.
        stub.state.script_bench("g-2", [BENCH_TUNED])
        admin = open_admin(stub, page)
        admin.wait_for_selector(".gen[data-idx='1'] .bench-trials")
        admin.locator(".gen[data-idx='1'] .bench-trials > summary").click()

        rows = row(admin, 1).locator(".bench-trials__table tbody tr")
        assert rows.count() == 4
        first = rows.first.inner_text()
        assert "detect ×1, engine ×1, post ×1" in first
        assert "auto" in first
        assert "1.50 pages/s" in first
        assert "widen detect" in first
        assert "kept" in first
        # The step that did not pay is in the story, marked as put back.
        last = rows.nth(3).inner_text()
        assert "detect ×4" in last
        assert "not worth it" in last
        assert "reverted" in last

        assert ("detect ×3 is 1.40× faster than auto on this machine"
                in row(admin, 1).locator(".bench-res__conclusion").inner_text())

    def test_auto_can_already_be_the_answer(self, stub: StubServer, page: Page) -> None:
        stub.state.script_bench("g-4", [BENCH_AUTO_BEST])
        admin = open_admin(stub, page)
        admin.wait_for_selector(".gen[data-idx='3'] .bench-res")
        assert ("Auto is already the best this machine can do"
                in row(admin, 3).locator(".bench-res__conclusion").inner_text())
        assert row(admin, 3).locator("[data-act='bench-apply']").count() == 0

    def test_a_stale_spec_says_so_and_still_shows_the_numbers(self, stub: StubServer, page: Page) -> None:
        # BENCH_STALE_SPEC measured g-2 with `detector: ppocr-manga, patch_budget:
        # 256`; the row on screen still says ctd / 512. That mismatch has to
        # be said plainly, without hiding what was actually measured.
        stub.state.script_bench("g-2", [BENCH_STALE_SPEC])
        admin = open_admin(stub, page)
        admin.wait_for_selector(".gen[data-idx='1'] .bench-res")
        body = row(admin, 1).locator(".bench-res").inner_text()
        assert "measured with different settings" in body.lower()
        assert "ppocr-manga" in body
        # The numbers are still there, not withheld because of the mismatch.
        assert "126 pages/min" in body

    def test_a_matching_spec_says_nothing_extra(self, admin: Page) -> None:
        # Row 1 (g-2) is unedited, so its own BENCH_TUNED_SUMMARY spec still
        # matches -- no stale notice for the ordinary case.
        body = row(admin, 1).locator(".bench-res").inner_text()
        assert "different settings" not in body.lower()

    def test_a_preempted_run_names_what_it_interrupted(self, stub: StubServer, page: Page) -> None:
        stub.state.script_bench("g-2", [BENCH_TUNED], preempted=True)
        admin = open_admin(stub, page)
        admin.wait_for_selector(".gen[data-idx='1'] .bench-res")
        body = row(admin, 1).locator(".bench-res").inner_text()
        assert "interrupted" in body.lower()
        assert "Volume 07" in body
        assert "restart when the benchmarks finish" in body

    def test_no_preempted_run_says_nothing_about_it(self, admin: Page) -> None:
        body = row(admin, 1).locator(".bench-res").inner_text()
        assert "interrupted" not in body.lower()


class TestTheWindowIsPartOfTheNumber:
    """ADDENDUM 9: a rate is a rate over a window, and the page says which.

    The tuner never decides anything on a window shorter than ten seconds,
    so neither may this page present one as though it had.
    """

    def test_a_result_says_what_window_it_was_timed_over(self, admin: Page) -> None:
        body = row(admin, 1).locator(".bench-res").inner_text()
        assert "Timed over 21 s of page results" in body
        assert "45 pages over 2 passes" in body
        assert row(admin, 1).locator(".bench-res__window--short").count() == 0

    def test_the_busy_numbers_sit_beside_the_rate(self, admin: Page) -> None:
        # "neither the GPU nor the CPU seemed tapped" is now answerable.
        assert "GPU 88% busy" in row(admin, 1).locator(".bench-res__sub").inner_text()
        assert "CPU 41% busy" in row(admin, 1).locator(".bench-res__sub").inner_text()

    def test_startup_is_information_not_a_term(self, admin: Page) -> None:
        body = row(admin, 1).locator(".bench-res__facts").inner_text()
        assert "First page after" in body
        assert "not counted in the speed above" in body
        # The per-volume estimate is reading only, and says so.
        assert "reading only" in body
        assert "once per session" in body

    def test_each_trial_shows_its_window_and_how_busy_it_was(
        self, stub: StubServer, page: Page
    ) -> None:
        stub.state.script_bench("g-2", [BENCH_TUNED])
        admin = open_admin(stub, page)
        admin.wait_for_selector(".gen[data-idx='1'] .bench-trials")
        admin.locator(".gen[data-idx='1'] .bench-trials > summary").click()
        headers = [
            text.lower()
            for text in row(admin, 1).locator(".bench-trials__table thead th").all_inner_texts()
        ]
        assert "window" in headers
        third = row(admin, 1).locator(".bench-trials__table tbody tr").nth(2).inner_text()
        assert "21 s × 2 passes" in third
        assert "GPU 88% busy" in third

    def test_a_short_window_is_flagged_and_nothing_is_offered_from_it(
        self, stub: StubServer, page: Page
    ) -> None:
        stub.state.script_bench("g-2", [BENCH_SHORT_WINDOW])
        admin = open_admin(stub, page)
        admin.wait_for_selector(".gen[data-idx='1'] .bench-res__window--short")
        body = row(admin, 1).locator(".bench-res").inner_text()
        assert "too short to compare settings on" in body
        assert "Benchmark more pages" in body
        # The rate is still shown -- it is what was measured -- but nothing
        # is offered to apply from it, and the story says why.
        assert "192 pages/min" in body
        assert row(admin, 1).locator("[data-act='bench-apply']").count() == 0
        assert "nothing was tuned" in row(admin, 1).locator(".bench-res__conclusion").inner_text()

        admin.locator(".gen[data-idx='1'] .bench-trials > summary").click()
        second = row(admin, 1).locator(".bench-trials__table tbody tr").nth(1).inner_text()
        # A trial nobody could read is not "reverted": it was never judged.
        assert "not decidable" in second
        assert "too short to decide on" in second


    def test_a_window_of_nothing_still_accounts_for_itself(
        self, stub: StubServer, page: Page
    ) -> None:
        """Every emission inside one instant: there IS no rate to show.

        The old window turned exactly this into 217052 pages/s. The panel
        must still render -- a result that says "I could not measure this"
        is the finding; a blank row would look like nothing ever ran.
        """
        burst = dict(
            BENCH_SHORT_WINDOW,
            best=dict(BENCH_SHORT_WINDOW["best"], pages_per_second=0.0,
                      seconds_per_page=None, window_seconds=0.0, pages_measured=54),
            baseline=dict(BENCH_SHORT_WINDOW["baseline"], pages_per_second=0.0,
                          seconds_per_page=None, window_seconds=0.0),
            estimates={"volume_200_pages_seconds": None, "remaining_pages": 5230,
                       "remaining_seconds": None},
        )
        stub.state.script_bench("g-2", [burst])
        admin = open_admin(stub, page)
        admin.wait_for_selector(".gen[data-idx='1'] .bench-res__window--short")
        body = row(admin, 1).locator(".bench-res").inner_text()
        assert "No usable rate" in body
        assert "too short to compare settings on" in body
        assert "pages/min" not in body
        for raw in ("217052", "Infinity", "NaN", "undefined", "null"):
            assert raw not in body, raw


    def test_a_zero_peak_vram_is_left_out_not_shown_as_zero(
        self, stub: StubServer, page: Page
    ) -> None:
        """A served engine's model is in another process, so the runner's
        own peak allocation is 0 while the card is fully busy (ADDENDUM 8).

        0 MB of VRAM is not a measurement of anything a reader wants, so the
        row is omitted entirely -- and the GPU busy percent, which DOES
        carry on that road, is still there.
        """
        served = dict(BENCH_TUNED, peak_vram_mb=0)
        stub.state.script_bench("g-2", [served])
        admin = open_admin(stub, page)
        admin.wait_for_selector(".gen[data-idx='1'] .bench-res")
        body = row(admin, 1).locator(".bench-res").inner_text()
        assert "Peak VRAM" not in body
        assert "0 MB" not in body
        assert "GPU 88% busy" in body
        assert "126 pages/min" in body


class TestBenchmarkApply:
    """ADDENDUM 5: Apply writes `best` into the row's pools IN THE EDITOR and
    marks the list dirty -- it never saves by itself any more."""

    def test_apply_fills_the_editor_marks_dirty_and_sends_no_put(self, stub: StubServer, page: Page) -> None:
        stub.state.script_bench("g-2", [BENCH_TUNED])
        admin = open_admin(stub, page)
        admin.wait_for_selector(".gen[data-idx='1'] [data-act='bench-apply']")
        assert admin.locator("#gen-dirty").is_hidden()
        # Before clicking: the button says what it is about to do, beside it.
        why = row(admin, 1).locator(".bench-res__apply-why").inner_text()
        assert "applied to this row" in why
        assert "save the list" in why.lower()

        admin.locator(".gen[data-idx='1'] [data-act='bench-apply']").click()

        # Nothing was sent to the server.
        assert stub.state.last_put is None
        # The row's own pool inputs now show `best`, exactly: the entries
        # that differ from derived.
        workers = row(admin, 1).locator("[data-act='workers'][data-stage='detect']")
        assert workers.is_visible()
        assert workers.input_value() == "3"
        # The list is dirty, and says so beside the button.
        assert admin.locator("#gen-dirty").is_visible()
        assert not admin.locator("#gen-save-btn").is_disabled()
        # ... and there is nothing left to apply.
        assert "already applied" in row(admin, 1).locator(".bench-res__conclusion").inner_text()
        assert row(admin, 1).locator("[data-act='bench-apply']").count() == 0

    def test_apply_rides_along_with_other_unsaved_edits(self, stub: StubServer, page: Page) -> None:
        # The old gate that refused to apply over unsaved edits elsewhere is
        # gone: applying never saves, so there is nothing left to protect.
        stub.state.script_bench("g-2", [BENCH_TUNED])
        admin = open_admin(stub, page)
        admin.wait_for_selector(".gen[data-idx='1'] [data-act='bench-apply']")
        row(admin, 2).locator(".gen__name").fill("nova-hd")
        admin.locator(".gen[data-idx='1'] [data-act='bench-apply']").click()

        assert row(admin, 1).locator(".bench-bar__note").is_hidden()
        assert stub.state.last_put is None
        # Both edits are still there, waiting on one Save.
        assert row(admin, 2).locator(".gen__name").input_value() == "nova-hd"
        workers = row(admin, 1).locator("[data-act='workers'][data-stage='detect']")
        assert workers.input_value() == "3"

        admin.click("#gen-save-btn")
        admin.wait_for_selector("#gen-save-note:not([hidden])")
        assert stub.state.last_put["generations"][1]["pools"] == {
            "stage_workers": {"detect": 3}, "queue_capacity": {}, "stage_device": {},
        }
        assert stub.state.last_put["generations"][2]["name"] == "nova-hd"


class TestQueuePage:
    def test_generation_names_are_the_run_order_and_the_job_label(
        self, stub: StubServer, page: Page
    ) -> None:
        page.goto(stub.url + "/queue/")
        page.wait_for_selector(".machine .lane[data-job]")

        order = page.locator("#run-order-list .queue-order__item").all_inner_texts()
        assert [o.split(None, 1)[1] for o in order] == [
            "mokuro", "hayai-nova-ctd", "hayai-nova-ppocr-manga", "paddle-manga",
        ]

        first = page.locator(".lane[data-job]").first
        assert first.locator(".badge--gen").inner_text() == "hayai-nova-ctd"
        # The recipe is a `detailed` field: at `normal` it is not even sent.
        assert first.locator(".job-recipe").count() == 0

        # Pending and failed jobs carry it too.
        assert page.locator(".pending-list__item .badge--gen").first.inner_text() == "hayai-nova-ctd"
        assert page.locator(".pending-list__item--failed .badge--gen").inner_text() == "paddle-manga"

    def test_two_jobs_on_one_machine_are_one_card(self, stub: StubServer, page: Page) -> None:
        page.goto(stub.url + "/queue/")
        page.wait_for_selector(".machine .lane[data-job]")
        assert page.locator(".machine").count() == 1
        assert page.locator(".machine__name").inner_text().lower() == "this server"
        assert page.locator(".machine .lane[data-job]").count() == 2

    def test_live_pipeline_readout_is_detailed_only(self, stub: StubServer, page: Page) -> None:
        page.goto(stub.url + "/queue/")
        page.wait_for_selector(".machine .lane[data-job]")
        assert page.locator(".stages").count() == 0

        stub.state.queue_level = "detailed"
        page.reload()
        page.wait_for_selector(".stages")
        first = page.locator(".lane[data-job]").first
        assert "widen detect" in first.locator(".stages__verdict").inner_text()
        assert first.locator(".stage").count() == 3
        assert first.locator(".job-recipe").inner_text() == "hayai-nova · ctd"
        # The three shares are on the element, not only in a tooltip.
        label = first.locator(".stage__bar").nth(1).get_attribute("aria-label")
        assert "busy 58%" in label and "starved 41%" in label
        # A run with no verdict names the busiest stage instead of shouting.
        second = page.locator(".lane[data-job]").nth(1)
        assert "No stage singled out" in second.locator(".stages__verdict").inner_text()
        # Its recipe says no more than its name: the slot is there, empty.
        assert second.locator(".job-recipe").inner_text() == ""


class TestQueuePausedForBenchmark:
    """Owner: the page-level "paused" line is gone -- it could say only one
    machine's benchmark and read wrong with several. Each machine's own card
    says what that machine is doing."""

    def test_no_page_level_paused_line_even_while_benchmarking(
        self, stub: StubServer, page: Page
    ) -> None:
        stub.state.seed_bench_queue(["g-2", "g-3"])
        page.goto(stub.url + "/queue/")
        page.wait_for_selector(".machine")
        assert page.locator("#paused-banner").count() == 0


class TestSelectLabels:
    """The user thinks in ids -- the file name is built from them -- and a
    closed select is too narrow for the catalog's product names. So the ids
    are the options, and the names ride along where they can be read whole."""

    def test_options_are_ids_and_the_name_rides_along_as_a_title(self, admin: Page) -> None:
        engine = row(admin, 1).locator("[data-act='engine']")
        assert engine.locator("option").all_inner_texts() == [
            "mokuro", "hayai-nova", "paddle-manga", "ppocr-manga",
        ]
        selected = engine.locator("option[value='hayai-nova']")
        assert selected.get_attribute("title") == "hayai-ocr v2.5 Nova"
        detector = row(admin, 1).locator("[data-act='detector']")
        assert detector.locator("option").all_inner_texts() == [
            "ppocr-manga", "ctd",
        ]
        assert (detector.locator("option[value='ctd']").get_attribute("title")
                == "comic-text-detector (via mokuro)")

    def test_the_select_describes_the_selection_in_full(self, admin: Page) -> None:
        """No caption under the select any more: the full name is the
        field's tooltip (label and select) and the select's accessible
        description."""
        engine = row(admin, 1).locator("[data-act='engine']")
        assert engine.get_attribute("title") == "hayai-ocr v2.5 Nova"
        desc = admin.locator("#" + engine.get_attribute("aria-describedby"))
        assert desc.text_content() == "hayai-ocr v2.5 Nova"
        label = row(admin, 1).locator("label[for='gen-engine-1']")
        assert label.get_attribute("title") == "hayai-ocr v2.5 Nova"
        assert "ⓘ" in label.inner_text()
        detector = row(admin, 1).locator("[data-act='detector']")
        assert (detector.get_attribute("title") or "").startswith("comic-text-detector (via mokuro) — ")
        # ... and it follows the select, rather than being written once.
        detector.select_option("ppocr-manga")
        detector = row(admin, 1).locator("[data-act='detector']")
        assert "PP-OCRv6 manga line detector (Kellenok)" in (detector.get_attribute("title") or "")
        patch = row(admin, 1).locator("[data-act='patch']")
        assert patch.get_attribute("title") == "How much of each line hayai-nova gets to look at."
        # Nothing is printed under the selects.
        assert row(admin, 1).locator(".gen__field p").count() == 0
        box = row(admin, 1).locator(".gen__desc").first.bounding_box()
        assert box is None or box["width"] <= 1, "the description is for screen readers only"

    def test_a_locked_detector_names_the_one_the_engine_brings(self, admin: Page) -> None:
        detector = row(admin, 4).locator("[data-act='detector']")
        desc = detector.get_attribute("title") or ""
        assert "PP-OCRv6 manga line detector (Kellenok)" in desc
        assert "brings its own detector" in desc
        assert admin.locator("#" + detector.get_attribute("aria-describedby")).text_content() == desc

    def test_no_select_clips_its_own_text(self, admin: Page) -> None:
        assert_nothing_clipped(admin)

    def test_no_select_clips_its_own_text_on_a_phone(self, admin: Page) -> None:
        admin.set_viewport_size({"width": 400, "height": 900})
        admin.wait_for_timeout(100)
        assert_nothing_clipped(admin)


class TestEnvironmentBlock:
    """One line (the backend) with the paths folded under Details -- and the
    line has to read right in every state, including the states where the
    server cannot answer at all."""

    def backend_line(self, admin: Page) -> str:
        return admin.locator("#ocr-backend").inner_text()

    def details(self, admin: Page) -> str:
        admin.locator("#ocr-env-details > summary").click()
        return admin.locator("#ocr-env-details").inner_text()

    def test_installed_and_configured_agree(self, admin: Page) -> None:
        line = self.backend_line(admin)
        assert line == "Auto → ROCm (available: CPU, ROCm)"
        assert admin.locator("#settings-ocr .ocr-env").inner_text() == "Backend: " + line

    def test_the_paths_are_folded_under_details(self, admin: Page) -> None:
        assert admin.locator("#ocr-env-details").get_attribute("open") is None
        assert admin.locator("#ocr-env-path").is_hidden()
        assert "chosen at launch" not in admin.locator("#settings-ocr").inner_text()
        text = self.details(admin)
        assert "/srv/mokuro/.venvs/mokuro" in text
        assert "/srv/mokuro/.venvs/engines" in text
        assert "mokuro, hayai-nova-ctd, hayai-nova-ppocr-manga, paddle-manga" in text

    def test_skip_says_what_skip_means(self, stub: StubServer, page: Page) -> None:
        stub.state.settings["ocr"]["backend"] = "skip"
        stub.state.settings["ocr_runtime"] = RUNTIME_SKIP
        admin = open_admin(stub, page)
        line = self.backend_line(admin)
        assert line == "off (set to skip) — this server reads no volumes"
        # Never the sentence that started all this.
        assert "installed installed" not in line

    def test_local_processing_off_says_who_reads_instead(self, stub: StubServer, page: Page) -> None:
        stub.state.settings["ocr_runtime"] = {
            "available": True, "launch_only": True, "configured_backend": "auto",
            "local_processing": False, "cli_hint": "", "driver_hint": "",
        }
        admin = open_admin(stub, page)
        line = self.backend_line(admin)
        assert line == "off here (local processing off) — processors read every volume"

    def test_a_server_that_cannot_look_does_not_claim_it_looked(
        self, stub: StubServer, page: Page
    ) -> None:
        stub.state.settings["ocr_runtime"] = RUNTIME_UNAVAILABLE
        admin = open_admin(stub, page)
        line = self.backend_line(admin)
        assert line == "Auto — install state not reported"
        # And the rows it could not fill say so instead of holding a dash.
        self.details(admin)
        assert admin.locator("#ocr-env-path").inner_text() == "not reported"
        assert admin.locator("#ocr-engines-env").inner_text() == "not reported"
        assert admin.locator("#ocr-active-engines").inner_text() == "not reported"

    def test_not_installed_yet(self, stub: StubServer, page: Page) -> None:
        stub.state.settings["ocr_runtime"] = RUNTIME_NOT_INSTALLED
        admin = open_admin(stub, page)
        assert self.backend_line(admin) == "Auto — not installed yet (available: CPU, ROCm)"
        self.details(admin)
        engines = admin.locator("#ocr-engines-env").inner_text()
        assert engines.startswith("/srv/mokuro/.venvs/engines")
        assert "installs on next start" in engines
        # The rows the server sent, named -- whatever shape it sent them in.
        assert admin.locator("#ocr-active-engines").inner_text() == "mokuro, hayai-nova-ctd"

    def test_configured_and_installed_can_disagree(self, stub: StubServer, page: Page) -> None:
        stub.state.settings["ocr_runtime"] = RUNTIME_MISMATCH
        admin = open_admin(stub, page)
        line = self.backend_line(admin)
        assert line == "CUDA set, but ROCm installed (available: CPU, ROCm)"
        self.details(admin)
        assert "detector extras install on next start" in admin.locator("#ocr-engines-env").inner_text()

    def test_no_state_produces_a_broken_sentence(self, stub: StubServer, page: Page) -> None:
        for runtime in (RUNTIME_UNAVAILABLE, RUNTIME_SKIP, RUNTIME_NOT_INSTALLED, RUNTIME_MISMATCH):
            stub.state.settings["ocr_runtime"] = runtime
            admin = open_admin(stub, page)
            text = self.backend_line(admin) + "\n" + self.details(admin)
            for wrong in ("installed installed", "- installed", "undefined", "null", "NaN", "- (",
                          "()", "(available: )"):
                assert wrong not in text, (wrong, runtime, text)


class TestSkippedVolumes:
    """The count is on the folded History line ("31/34 volumes · 1 skipped");
    the reason and the fix are its tooltip. There is no second progress line."""

    def test_the_count_says_what_it_will_never_reach(self, admin: Page) -> None:
        summary = row(admin, 1).locator(".gen__history-summary").inner_text()
        assert summary.startswith("History — 31/34 volumes · 1 skipped")
        assert row(admin, 1).locator(".gen__progress").count() == 0

    def test_the_reason_and_the_fix_are_on_the_count(self, admin: Page) -> None:
        why = row(admin, 1).locator(".gen__history-summary").get_attribute("title")
        assert "uploaded with pages missing" in why
        assert "no additional layers" in why
        assert "Replace the file with a complete volume" in why

    def test_nothing_extra_when_there_is_nothing_to_say(self, admin: Page) -> None:
        # Zero, and a server that does not send the field at all.
        for i in (0, 2):
            summary = row(admin, i).locator(".gen__history-summary")
            assert "skipped" not in summary.inner_text()
            assert summary.get_attribute("title") == summary.inner_text()
        assert row(admin, 0).locator(".gen__history-summary").inner_text().startswith(
            "History — 34/34 volumes")
        assert row(admin, 2).locator(".gen__history-summary").inner_text().startswith(
            "History — 12/34 volumes")


class TestBenchmarkSessions:
    """Models stay loaded across volumes, so the load is paid once per session
    -- except by a monolithic engine, which is still one run per volume.

    ADDENDUM 9 changed what is said about it, not what is true: the load is
    never in a rate or an estimate any more, so the page reports it as "first
    page after X s" and says plainly that it is not counted.
    """

    def test_a_staged_row_reports_the_load_without_counting_it(self, admin: Page) -> None:
        body = row(admin, 1).locator(".bench-res").inner_text()
        assert "First page after" in body
        assert "paid once per session — not counted in the speed above" in body
        assert "per volume" not in body.replace("not per volume", "")

    def test_the_volume_estimate_says_what_it_includes(self, admin: Page) -> None:
        body = row(admin, 1).locator(".bench-res").inner_text()
        assert "reading only" in body
        assert "paid once per session, not per volume" in body

    def test_a_monolithic_row_still_pays_it_every_volume(self, admin: Page) -> None:
        row(admin, 0).locator(".bench-res__summary").click()
        body = row(admin, 0).locator(".bench-res").inner_text()
        assert "First page after" in body
        assert "paid again for every volume — not counted in the speed above" in body
        assert "once per session" not in body
        # Its estimate is reading alone, and says so rather than implying the
        # load is in there.
        assert "reading only — this engine loads again for every volume, on top" in body


class TestBenchmarkUnsupported:
    """A server that predates benchmarks answers 404. That is not an error to
    report to anyone -- it is a feature this server does not have."""

    def test_no_button_no_message_and_no_retry_loop(self, stub: StubServer, page: Page) -> None:
        stub.state.bench_supported = False
        admin = open_admin(stub, page)
        admin.wait_for_function("document.querySelectorAll('.gen__bench').length === 5")
        admin.wait_for_timeout(300)

        assert admin.locator("[data-act='bench-start']").count() == 0
        assert admin.locator("[data-act='bench-cancel']").count() == 0
        assert admin.locator(".bench-bar__note").count() == 0
        assert admin.locator(".bench-out").count() == 0
        # The toast element is always in the page; it is never RAISED.
        assert "show" not in (admin.locator("#toast").get_attribute("class") or "")
        assert admin.locator("#toast").inner_text() == ""
        assert "404" not in admin.locator("#settings-ocr").inner_text()

        # One ask a row, and no poll behind it.
        asked = len(stub.state.bench_gets)
        assert asked == 5, stub.state.bench_gets
        admin.wait_for_timeout(2500)
        assert len(stub.state.bench_gets) == asked

    def test_the_rest_of_the_list_still_works(self, stub: StubServer, page: Page) -> None:
        stub.state.bench_supported = False
        admin = open_admin(stub, page)
        row(admin, 2).locator(".gen__name").fill("nova-hd")
        assert not admin.locator("#gen-save-btn").is_disabled()
        admin.click("#gen-save-btn")
        admin.wait_for_selector("#gen-save-note:not([hidden])")
        assert stub.state.last_put["generations"][2]["name"] == "nova-hd"


class TestQueueSkippedVolumes:
    def test_skipped_volumes_are_listed_with_the_fix(self, stub: StubServer, page: Page) -> None:
        page.goto(stub.url + "/queue/")
        page.wait_for_selector("#skipped-section:not([hidden])")
        assert page.locator("#skipped-count").inner_text() == "2"

        items = page.locator(".pending-list__item--skipped")
        assert items.count() == 2
        first = items.first.inner_text()
        assert "Volume 05" in first
        assert "1 page short of 194" in first
        assert "hayai-nova-ctd" in first and "paddle-manga" in first
        # Plural, and a server that does not say how long the volume is.
        assert "12 pages short" in items.nth(1).inner_text()

        # The fix is said once, over the list, rather than after every row.
        section = page.locator("#skipped-section").inner_text()
        assert "uploaded with pages missing" in section
        assert "Replace the file with a complete volume" in section
        assert "next scan picks it up" in section
        assert section.count("Replace the file") == 1

    def test_it_is_quieter_than_the_failed_list(self, stub: StubServer, page: Page) -> None:
        page.goto(stub.url + "/queue/")
        page.wait_for_selector("#skipped-section:not([hidden])")
        # Nothing in it is dressed as an error.
        assert page.locator("#skipped-list .badge--error").count() == 0
        assert page.locator("#skipped-list .failed-item__error").count() == 0
        assert page.locator("#skipped-list .pending-list__item--failed").count() == 0
        # The failed list is untouched beside it.
        assert page.locator(".pending-list__item--failed").count() == 1

    def test_an_empty_list_hides_the_whole_section(self, stub: StubServer, page: Page) -> None:
        stub.state.queue_status["skipped_missing_pages"] = []
        page.goto(stub.url + "/queue/")
        page.wait_for_selector(".machine .lane[data-job]")
        assert page.locator("#skipped-section").is_hidden()


class TestQueueOnDeck:
    """A session keeps the next volume submitted behind the one it reads. It
    used to get a card of its own that sat at 0% for the whole of the volume
    ahead of it; it is the machine's on-deck field now, and nothing more."""

    def test_the_lookahead_is_a_next_line_not_a_card(
        self, stub: StubServer, page: Page
    ) -> None:
        stub.state.queue_status = QUEUE_STATUS_SESSION
        page.goto(stub.url + "/queue/")
        page.wait_for_selector(".machine .lane[data-job]")

        assert page.locator(".machine").count() == 1
        assert page.locator(".lane").count() == 1
        job = page.locator(".lane[data-job]").inner_text()
        assert "Volume 07" in job and "98%" in job
        assert page.locator(".lane", has_text="Volume 08").count() == 0
        assert (
            page.locator(".machine__next-text").inner_text()
            == "Dr STONE · Volume 08 · hayai-nova-ctd"
        )
        # There was never a 0% bar for it.
        assert page.locator(".job__pct", has_text="0%").count() == 0

    def test_no_lookahead_says_none(self, stub: StubServer, page: Page) -> None:
        page.goto(stub.url + "/queue/")
        page.wait_for_selector(".machine .lane[data-job]")
        # The field keeps its place (the card never changes shape) and says so.
        assert page.locator(".machine__next-text").inner_text() == "None"


class TestBenchmarkQueueing:
    """ADDENDUM 6: several benchmarks can be posted at once and run strictly
    FIFO. Each row's own place in the line shows where its button was."""

    def test_three_queued_show_1st_2nd_3rd_and_run_in_order(
        self, stub: StubServer, page: Page
    ) -> None:
        # Seed the line as if g-1, g-2, g-3 had each been posted in turn:
        # g-1 runs, g-2 and g-3 wait.
        stub.state.seed_bench_queue(["g-1", "g-2", "g-3"])
        admin = open_admin(stub, page)

        # Each row polls its own bench key independently, so wait for EACH
        # one's settled text rather than asserting the instant any one of
        # them appears -- otherwise this races the slower row's poll.
        admin.wait_for_selector(".gen[data-idx='0'] [data-act='bench-cancel']")
        admin.wait_for_selector(".gen[data-idx='1'] .bench-run__title:has-text('1st in line')")
        admin.wait_for_selector(".gen[data-idx='2'] .bench-run__title:has-text('2nd in line')")
        assert "1st in line" in row(admin, 1).locator(".bench-run__title").inner_text()
        assert "2nd in line" in row(admin, 2).locator(".bench-run__title").inner_text()
        # The running one offers a Cancel, not a place in line.
        assert row(admin, 0).locator("[data-act='bench-cancel']").count() == 1

        # Advance the line: g-1 finishes, g-2 is promoted to head.
        stub.state.finish_head_bench()
        admin.wait_for_selector(".gen[data-idx='1'] [data-act='bench-cancel']")
        admin.wait_for_selector(".gen[data-idx='2'] .bench-run__title:has-text('1st in line')")

    def test_cancelling_a_queued_one_renumbers_the_rest(
        self, stub: StubServer, page: Page
    ) -> None:
        stub.state.seed_bench_queue(["g-1", "g-2", "g-3"])
        admin = open_admin(stub, page)
        admin.wait_for_selector(".gen[data-idx='1'] .bench-run__title:has-text('1st in line')")
        admin.wait_for_selector(".gen[data-idx='2'] .bench-run__title:has-text('2nd in line')")

        # Cancel the 1st-in-line (g-2, row 1): g-3 moves up to 1st.
        row(admin, 1).locator("[data-act='bench-cancel']").click()
        admin.wait_for_selector(".gen[data-idx='2'] .bench-run__title:has-text('1st in line')")
        assert row(admin, 1).locator(".bench-run").count() == 0
        assert row(admin, 1).locator("[data-act='bench-start']").count() == 1
        # g-1 is still running, untouched.
        assert row(admin, 0).locator("[data-act='bench-cancel']").count() == 1

    def test_a_draft_rows_key_is_stable_across_polls_and_its_spec_matches_the_row(
        self, stub: StubServer, admin: Page
    ) -> None:
        # A brand new, unsaved row: the button has to work without an id.
        admin.click("#gen-add-btn")
        new_row = row(admin, 5)
        assert new_row.locator("[data-act='engine']").input_value() == "hayai-nova"
        new_row.locator("[data-act='bench-start']").click()

        admin.wait_for_selector(".gen[data-idx='5'] [data-act='bench-cancel']")
        assert len(stub.state.bench_posts) == 1
        draft_key, body = stub.state.bench_posts[0]
        assert draft_key.startswith("draft-")
        # The spec sent is exactly what the new row shows: first
        # non-monolithic engine, first detector, derived name is irrelevant
        # to a spec (name/primary/enabled/id are not part of it).
        assert body == {"spec": {
            "engine": "hayai-nova", "detector": "ppocr-manga",
            "patch_budget": 512, "precision": "auto-accuracy",
            "pools": {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}},
        }}

        # A second poll interval later, the SAME draft key is asked again --
        # not a fresh one minted per poll.
        admin.wait_for_timeout(2200)
        keys_asked = {k for k, _ in stub.state.bench_posts} | set(stub.state.bench_gets)
        assert draft_key in keys_asked
        assert sum(1 for g in stub.state.bench_gets if g == draft_key) >= 1


class TestNameLength32:
    """ADDENDUM 6, item 8: 24 -> 32, read from the catalog's `name_pattern`
    rather than hardcoded."""

    def test_a_32_character_name_is_accepted(self, admin: Page) -> None:
        name = "a" * 32
        row(admin, 2).locator(".gen__name").fill(name)
        assert row(admin, 2).locator(".gen__msg").is_hidden()
        assert not admin.locator("#gen-save-btn").is_disabled()

    def test_a_33_character_name_is_refused(self, admin: Page) -> None:
        row(admin, 2).locator(".gen__name").fill("a" * 33)
        assert "1–32 characters" in row(admin, 2).locator(".gen__msg").inner_text()
        assert admin.locator("#gen-save-btn").is_disabled()

    def test_default_names_truncate_at_32(self, stub: StubServer, page: Page) -> None:
        # A server whose catalog pattern allows longer names than the built
        # in fallback still gets a name that fits it -- the truncation length
        # is read from the pattern the catalog sent, not hardcoded to 24.
        admin = open_admin(stub, page)
        admin.click("#gen-add-btn")
        name = row(admin, 5).locator(".gen__name").input_value()
        assert len(name) <= 32


class TestBenchStubContract:
    """Raw HTTP against the stub's bench endpoints -- no browser, no
    admin.js -- proving the FIFO/spec/preemption machinery ADDENDA 5 & 6
    describe, independent of how the UI happens to render it."""

    def _req(self, url: str, method: str = "GET", body: dict[str, Any] | None = None) -> tuple[int, dict[str, Any]]:
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(
            url, data=data, method=method, headers={"Content-Type": "application/json"}
        )
        try:
            resp = urllib.request.urlopen(req)
            return resp.status, json.loads(resp.read())
        except urllib.error.HTTPError as err:
            return err.code, json.loads(err.read())

    def test_three_posts_show_positions_0_1_2_and_promote_in_order(self, stub: StubServer) -> None:
        base = stub.url + "/_admin/api/ocr/generations"
        spec = {"engine": "hayai-nova", "detector": "ctd",
                "patch_budget": 512, "pools": {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}}}
        st1, b1 = self._req(base + "/g-1/bench", "POST", {"spec": spec})
        st2, b2 = self._req(base + "/g-2/bench", "POST", {"spec": spec})
        st3, b3 = self._req(base + "/g-3/bench", "POST", {"spec": spec})
        assert (st1, st2, st3) == (202, 202, 202)
        # Each POST answers with the queue as it stood AT THAT MOMENT --
        # only a fresh GET, after all three, reflects the full line.
        assert (b1["state"], b1["position"]) == ("running", 0)
        assert (b2["state"], b2["position"]) == ("queued", 1)
        assert (b3["state"], b3["position"]) == ("queued", 2)
        _, b1_now = self._req(base + "/g-1/bench")
        assert b1_now["queue"] == {"running": "g-1", "queued": ["g-2", "g-3"]}
        _, b3_now = self._req(base + "/g-3/bench")
        assert b3_now["queue"] == b1_now["queue"]

        key = stub.state.finish_head_bench()
        assert key == "g-1"
        _, b2b = self._req(base + "/g-2/bench")
        assert (b2b["state"], b2b["position"]) == ("running", 0)
        _, b3b = self._req(base + "/g-3/bench")
        assert (b3b["state"], b3b["position"]) == ("queued", 1)

    def test_same_key_conflict_is_409_only_for_that_key(self, stub: StubServer) -> None:
        base = stub.url + "/_admin/api/ocr/generations"
        spec = {"engine": "mokuro", "detector": None,
                "patch_budget": None, "pools": {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}}}
        self._req(base + "/g-1/bench", "POST", {"spec": spec})
        st, body = self._req(base + "/g-1/bench", "POST", {"spec": spec})
        assert st == 409
        assert "already" in body["error"].lower()
        # A DIFFERENT key is unaffected.
        st2, body2 = self._req(base + "/g-2/bench", "POST", {"spec": spec})
        assert st2 == 202

    def test_cancelling_a_queued_key_shifts_the_ones_behind_it(self, stub: StubServer) -> None:
        base = stub.url + "/_admin/api/ocr/generations"
        stub.state.seed_bench_queue(["g-1", "g-2", "g-3"])
        st, body = self._req(base + "/g-2/bench", "DELETE")
        assert body["state"] == "cancelled"
        _, g1 = self._req(base + "/g-1/bench")
        _, g3 = self._req(base + "/g-3/bench")
        assert g1["position"] == 0
        assert g3["position"] == 1
        assert g1["queue"] == {"running": "g-1", "queued": ["g-3"]}

    def test_spec_is_echoed_for_a_saved_id_and_for_a_draft_key(self, stub: StubServer) -> None:
        base = stub.url + "/_admin/api/ocr/generations"
        spec_saved = {"engine": "paddle-manga", "detector": "ppocr-manga",
                      "patch_budget": None, "pools": {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}}}
        spec_draft = {"engine": "ppocr-manga", "detector": None,
                      "patch_budget": None, "pools": {"stage_workers": {}, "queue_capacity": {"layout": 8}}}
        _, saved = self._req(base + "/g-4/bench", "POST", {"spec": spec_saved, "pages": 16})
        _, draft = self._req(base + "/draft-9f2c1a0b/bench", "POST", {"spec": spec_draft})
        assert saved["spec"] == spec_saved
        assert draft["spec"] == spec_draft
        assert saved["key"] == "g-4"
        assert draft["key"] == "draft-9f2c1a0b"

    def test_preempted_is_carried_only_by_the_run_that_triggered_it(self, stub: StubServer) -> None:
        base = stub.url + "/_admin/api/ocr/generations"
        spec = {"engine": "mokuro", "detector": None,
                "patch_budget": None, "pools": {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}}}
        _, first = self._req(base + "/g-1/bench", "POST", {"spec": spec})
        _, second = self._req(base + "/g-2/bench", "POST", {"spec": spec})
        assert first["preempted"] == PREEMPTED_SAMPLE
        assert second["preempted"] == []

    def test_a_drafts_result_does_not_survive_a_simulated_restart(self, stub: StubServer) -> None:
        base = stub.url + "/_admin/api/ocr/generations"
        spec = {"engine": "mokuro", "detector": None,
                "patch_budget": None, "pools": {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}}}
        self._req(base + "/draft-restart01/bench", "POST", {"spec": spec})
        self._req(base + "/g-1/bench", "POST", {"spec": spec})
        stub.state.finish_head_bench()  # settles the draft (it POSTed first)
        _, before = self._req(base + "/draft-restart01/bench")
        assert before["state"] == "done"

        stub.state.simulate_server_restart()
        st, after = self._req(base + "/draft-restart01/bench")
        # A 404 or a quiet "idle" are both acceptable -- never an error about
        # a benchmark that "was lost".
        assert st == 404 or after.get("state") == "idle"

    def test_queue_status_carries_paused_for_benchmark(self, stub: StubServer) -> None:
        stub.state.seed_bench_queue(["g-2", "g-3"])
        _, status = self._req(stub.url + "/queue/api/status")
        # A visitor's copy: the bench key is an admin's only.
        assert status["paused_for_benchmark"] == {
            "generation": "hayai-nova-ctd", "queued": 1, "processor": None,
        }
        stub.state.queue_admin = True
        _, status = self._req(stub.url + "/queue/api/status")
        assert status["paused_for_benchmark"]["key"] == "g-2"

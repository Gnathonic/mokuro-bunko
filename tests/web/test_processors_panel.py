"""The Processors card, per-processor pools and the queue page, in a real browser.

Driven against `generations_stub` like the rest of this directory: the REAL
page files, a stub answering the API contract.
"""

from __future__ import annotations

import os
import re
import time
from collections.abc import Generator
from copy import deepcopy
from typing import TYPE_CHECKING, Any

import pytest

from .generations_stub import (
    BENCH_RUN_SEQUENCE,
    BENCH_TUNED,
    PROCESSORS,
    QUEUE_STATUS_NO_PROCESSOR,
    QUEUE_STATUS_REMOTE,
    StubServer,
    processors_with_long_name,
)

if TYPE_CHECKING:
    from playwright.sync_api import Page

CHROMIUM = os.environ.get("MOKURO_TEST_CHROMIUM") or None

# Defined HERE rather than imported from test_ocr_generations: that module
# binds its own `_launch` inside a try/except ImportError, so importing it
# fails at COLLECTION when playwright is absent instead of skipping.
try:
    from playwright.sync_api import sync_playwright

    def _launch(p: Any) -> Any:
        kwargs: dict[str, Any] = {"headless": True}
        if CHROMIUM:
            kwargs["executable_path"] = CHROMIUM
        return p.chromium.launch(**kwargs)

    def _browsers_available() -> bool:
        try:
            with sync_playwright() as p:
                _launch(p).close()
                return True
        except Exception:
            return False

    BROWSERS_AVAILABLE = _browsers_available()
except ImportError:  # pragma: no cover - playwright is a dev extra
    BROWSERS_AVAILABLE = False

pytestmark = pytest.mark.skipif(
    not BROWSERS_AVAILABLE, reason="Playwright browsers not available"
)

SIGNED_IN = (
    "sessionStorage.setItem('mokuro_token', 'YWRtaW46YWRtaW5wYXNz');"
    "sessionStorage.setItem('mokuro_user', '{\"username\":\"admin\",\"role\":\"admin\"}');"
)


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


def open_settings(stub: StubServer, page: Page) -> Page:
    page.add_init_script(SIGNED_IN)
    page.goto(stub.url + "/_admin/")
    page.wait_for_selector(".admin-container")
    page.click(".tab[data-tab='settings']")
    page.wait_for_selector(".gen[data-idx='0']")
    page.wait_for_selector("#processors-body tr")
    return page


def open_users(stub: StubServer, page: Page) -> Page:
    page.add_init_script(SIGNED_IN)
    page.goto(stub.url + "/_admin/")
    page.wait_for_selector(".admin-container")
    return page


# --- the Processors card ------------------------------------------------------


def test_a_connected_processor_is_listed_with_its_hardware(
    stub: StubServer, page: Page
) -> None:
    open_settings(stub, page)
    rows = page.locator("#processors-body tr")
    # Three connected (this server included), and the one with numbers that
    # is not connected now: one table, one row a machine.
    assert rows.count() == 4
    tower = page.locator('#processors-body tr[data-processor="tower"]')
    assert tower.locator(".processors-machine__name").inner_text() == "tower"
    assert tower.locator(".processors-host").inner_text() == "Threadripper (48 cores) · RTX 4090"
    # Just "connected": how long ago is on hover, never in the row.
    since = tower.locator(".processors-machine__since")
    assert since.inner_text() == "connected"
    assert (since.get_attribute("title") or "").startswith("connected ")
    assert since.get_attribute("title") != "connected"


def test_there_is_no_sessions_column(stub: StubServer, page: Page) -> None:
    open_settings(stub, page)
    headers = page.locator("#processors-card thead th").all_inner_texts()
    assert [h.strip() for h in headers] == ["Machine", "Hardware", "Pages/min"]
    assert page.locator("#processors-card col").count() == 3
    assert page.locator("#processors-body tr").first.locator("td").count() == 3
    assert page.locator(".processor-sessions").count() == 0
    assert "1 of 2" not in page.locator("#processors-body").inner_text()


def test_this_server_shows_its_own_hardware(stub: StubServer, page: Page) -> None:
    open_settings(stub, page)
    local = page.locator('#processors-body tr[data-processor="this server"]')
    assert local.locator(".processors-machine__name").inner_text() == "this server"
    assert local.locator(".processors-host").inner_text() == (
        "Ryzen 9 7950X (16 cores) · Radeon RX 7900 XTX")
    assert local.locator(".processors-machine__since").count() == 0


def test_this_server_before_its_probe_answers_has_an_empty_hardware_cell(
    stub: StubServer, page: Page
) -> None:
    stub.state.processors["processors"][0]["host"] = {}
    stub.state.processors["speed"][0]["host"] = None
    open_settings(stub, page)
    local = page.locator('#processors-body tr[data-processor="this server"]')
    assert local.locator(".processors-host").inner_text() == ""


def test_an_offline_machine_shows_the_hardware_it_last_registered_with(
    stub: StubServer, page: Page
) -> None:
    open_settings(stub, page)
    laptop = page.locator('#processors-body tr[data-processor="old-laptop"]')
    assert laptop.locator(".processors-machine__offline").inner_text() == "offline"
    assert laptop.locator(".processors-host").inner_text() == "i5-8250U (4 cores) · MX150"
    assert laptop.locator(".processors-machine__since").count() == 0


def test_an_offline_machine_whose_hardware_was_never_kept_leaves_it_empty(
    stub: StubServer, page: Page
) -> None:
    stub.state.processors["speed"][2]["host"] = None
    open_settings(stub, page)
    laptop = page.locator('#processors-body tr[data-processor="old-laptop"]')
    assert laptop.locator(".processors-host").inner_text() == ""
    assert laptop.locator(".processors-machine__offline").inner_text() == "offline"


def test_a_processor_row_does_not_list_its_archive_downloads(
    stub: StubServer, page: Page
) -> None:
    tower = stub.state.processors["processors"][1]
    tower["transfer"] = {
        "volumes": 20, "mb_per_s": 98.4, "resumed": 1, "restarted": 0, "repaired": 0,
        "damaged": 0, "returned": 2, "returned_by_class": {}, "last_returned": None,
        "held_until": None, "held_error": None,
    }
    open_settings(stub, page)
    row = page.locator('#processors-body tr[data-processor="tower"]')
    assert row.locator(".processor-transfer").count() == 0
    text = row.inner_text()
    assert "Archives" not in text and "MB/s" not in text
    assert "resumed" not in text and "returned" not in text


def test_a_processor_held_for_failing_downloads_says_until_when(
    stub: StubServer, page: Page
) -> None:
    tower = stub.state.processors["processors"][1]
    tower["transfer"] = {
        "volumes": 0, "mb_per_s": None, "resumed": 0, "restarted": 0, "repaired": 0,
        "damaged": 0, "returned": 3, "returned_by_class": {"missing": 3},
        "last_returned": {"class": "missing", "error": "the library has no x (404)", "at": 1.0},
        "held_until": time.time() + 600, "held_error": "missing: the library has no x (404)",
    }
    open_settings(stub, page)
    lines = page.locator('#processors-body tr[data-processor="tower"] .processor-transfer')
    # Only the hold: the archive tallies are not on the card.
    assert lines.count() == 1
    held = lines.nth(0).inner_text()
    assert held.startswith("held until ") and "downloads failing" in held
    assert "(missing: the library has no x (404))" in held


def test_a_row_that_cannot_start_on_a_processor_says_when_it_is_tried_again(
    stub: StubServer, page: Page
) -> None:
    tower = stub.state.processors["processors"][1]
    tower["cannot_start"] = [{"generation": "paddle-animetext", "until": time.time() + 600,
                              "failures": 3, "error": "no GPU execution provider"}]
    open_settings(stub, page)
    line = page.locator('#processors-body tr[data-processor="tower"] .processor-transfer--held')
    text = line.inner_text()
    assert text.startswith("paddle-animetext cannot start here — next try ")
    assert text.endswith("(no GPU execution provider)")


def test_the_card_shows_one_speed_per_machine_and_layer(
    stub: StubServer, page: Page
) -> None:
    """Owner: ONE number per machine and generation -- the real rate of its
    recent finished volumes. No benchmark beside it, no "last ran"."""
    open_settings(stub, page)
    tower = page.locator('#processors-body tr[data-processor="tower"]')
    rates = tower.locator(".processors-rates li")
    assert rates.count() == 2
    mokuro = tower.locator('.processors-rates li[data-speed-generation="mokuro"]')
    assert mokuro.locator(".processors-rates__gen").inner_text() == "mokuro"
    assert mokuro.locator(".processors-rates__real").inner_text() == "1529"
    assert mokuro.inner_text().split() == ["mokuro", "1529"]
    assert "benchmark" not in (mokuro.get_attribute("title") or "")
    hayai = tower.locator('.processors-rates li[data-speed-generation="hayai-nova-ctd"]')
    assert hayai.locator(".processors-rates__real").inner_text() == "440"
    # This server, and a processor that is not connected but has numbers.
    local = page.locator('#processors-body tr[data-processor="this server"]')
    assert local.locator(".processors-rates__real").inner_text() == "21"
    laptop = page.locator('#processors-body tr[data-processor="old-laptop"]')
    assert laptop.locator(".processors-rates__real").inner_text() == "6.2"
    # Nowhere on the card: the faded benchmark and the last-ran time.
    card = page.locator("#processors-body")
    assert card.locator(".processors-rates__bench, .processors-rates__when").count() == 0
    assert "bench " not in card.inner_text() and "last ran" not in card.inner_text()
    # A connected machine with no numbers yet says so with a dash.
    box = page.locator('#processors-body tr[data-processor="box"]')
    assert box.locator(".processors-rates-cell").inner_text() == "—"


def test_the_benchmark_fills_in_where_there_is_no_history_yet(
    stub: StubServer, page: Page
) -> None:
    tower = stub.state.processors["speed"][1]
    tower["layers"][1].update(pages_per_minute=None, volumes=0, last_at=None)
    open_settings(stub, page)
    row = page.locator('#processors-body tr[data-processor="tower"]')
    hayai = row.locator('.processors-rates li[data-speed-generation="hayai-nova-ctd"]')
    figure = hayai.locator(".processors-rates__real")
    assert figure.inner_text() == "659"
    assert "processors-rates__real--bench" in (figure.get_attribute("class") or "")
    assert figure.get_attribute("title") == "from benchmark"
    # The history's own figure is not marked.
    mokuro = row.locator('.processors-rates li[data-speed-generation="mokuro"] .processors-rates__real')
    assert "processors-rates__real--bench" not in (mokuro.get_attribute("class") or "")
    assert mokuro.get_attribute("title") != "from benchmark"


def test_no_numbers_leaves_a_dash_and_no_offline_rows(stub: StubServer, page: Page) -> None:
    stub.state.processors = dict(stub.state.processors, speed=[])
    open_settings(stub, page)
    assert page.locator("#processors-body tr").count() == 3
    assert page.locator(".processors-rates").count() == 0
    assert set(page.locator(".processors-rates-cell").all_inner_texts()) == {"—"}


CLIP_WIDTHS = [1280, 1024, 768, 400, 360]


@pytest.mark.parametrize("width", CLIP_WIDTHS)
def test_no_processor_text_is_cut_at_any_width(stub: StubServer, page: Page, width: int) -> None:
    """The owner saw the throughput table run past its box and get clipped.
    With a real-length name and GPU in it, every cell's text is inside its
    cell and the table is inside its box: long text wraps, nothing scrolls
    and nothing is cut, from a desktop down to a 360px phone."""
    stub.state.processors = processors_with_long_name()
    page.set_viewport_size({"width": width, "height": 1000})
    open_settings(stub, page)
    page.locator("#processor-failed-summary").click()
    found = page.evaluate("""() => {
      const out = [];
      const box = document.querySelector('#processors-card .processors-box');
      const table = box.querySelector('table');
      if (table.getBoundingClientRect().right > box.getBoundingClientRect().right + 0.5 ||
          box.scrollWidth > box.clientWidth) {
        out.push(['table', box.scrollWidth, box.clientWidth]);
      }
      const card = document.getElementById('processors-card');
      const edge = card.getBoundingClientRect().right;
      for (const el of card.querySelectorAll('td, th, td *, summary, li')) {
        if (!el.getClientRects().length) continue;
        if (el.scrollWidth > el.clientWidth + 1 && getComputedStyle(el).display !== 'inline') {
          out.push(['cell', el.className || el.tagName, el.textContent.trim().slice(0, 50),
                    el.scrollWidth, el.clientWidth]);
        }
        if (el.getBoundingClientRect().right > edge + 0.5) {
          out.push(['past the card', el.className || el.tagName, el.textContent.trim().slice(0, 50)]);
        }
      }
      return out;
    }""")
    assert found == [], found
    width_now, client = page.evaluate(
        "[document.documentElement.scrollWidth, document.documentElement.clientWidth]"
    )
    assert width_now <= client, "the page scrolls sideways"
    # Wrapped, not dropped: the whole name and the whole GPU are there.
    row = page.locator('#processors-body tr[data-processor="very-long-processor-hostname-01"]')
    assert row.locator(".processors-machine__name").inner_text() == "very-long-processor-hostname-01"
    assert "NVIDIA GeForce RTX 4090 Laptop GPU" in row.locator(".processors-host").inner_text()


def test_a_processor_still_installing_says_so(stub: StubServer, page: Page) -> None:
    open_settings(stub, page)
    box = page.locator('#processors-body tr[data-processor="box"]')
    assert box.locator(".processors-machine__installing").inner_text() == "installing"
    tower = page.locator('#processors-body tr[data-processor="tower"]')
    assert tower.locator(".processors-machine__installing").count() == 0


def test_every_refused_login_is_listed_with_its_username_escaped(
    stub: StubServer, page: Page
) -> None:
    open_settings(stub, page)
    note = page.locator("#processor-failed-logins")
    assert note.is_visible()
    # Folded away behind its count.
    assert note.get_attribute("open") is None
    assert page.locator("#processor-failed-summary").inner_text() == "2 refused logins"
    assert page.locator("#processor-failed-list").is_hidden()
    page.locator("#processor-failed-summary").click()
    text = note.inner_text()
    assert "typo" in text
    assert "<img src=x onerror=alert(1)>" in text, "shown as text"
    assert note.locator("img").count() == 0, "never as markup"


def test_the_card_says_so_when_nothing_can_run(stub: StubServer, page: Page) -> None:
    stub.state.processors = {
        **deepcopy(PROCESSORS),
        "processors": [deepcopy(PROCESSORS["processors"][0])],
        "local_processing": False,
        "last_disconnect": {"name": "tower", "at": 1790000200.0},
        "processing_hold": {"reason": "no-processor", "since": 1790000200.0,
                            "last": {"name": "tower", "disconnected_at": 1790000200.0}},
        "failed_logins": [],
    }
    open_settings(stub, page)
    hold = page.locator("#processors-hold")
    assert hold.is_visible()
    assert "No processor connected since" in hold.inner_text()
    assert "tower" in hold.inner_text()
    assert "does no OCR of its own" in page.locator("#processors-hint").inner_text()


# --- per-processor pools ---------------------------------------------------------


def _with_processors(stub: StubServer) -> None:
    stub.state.gen_processors = [
        deepcopy(p) for p in PROCESSORS["processors"] if not p["local"]
    ]


def test_with_one_machine_there_is_no_machine_selector(stub: StubServer, page: Page) -> None:
    open_settings(stub, page)
    page.click(".gen[data-idx='1'] .gen__tuning-summary")
    assert page.locator(".gen[data-idx='1'] .pools__processor").count() == 0


def test_pools_are_edited_and_saved_for_one_processor(stub: StubServer, page: Page) -> None:
    _with_processors(stub)
    open_settings(stub, page)
    select = page.locator(".gen[data-idx='1'] .pools__processor")
    assert select.is_visible()
    options = select.locator("option").all_inner_texts()
    assert options[:2] == ["All machines", "this server"] and "tower (RTX 4090)" in options
    select.select_option("tower")
    page.click(".gen[data-idx='1'] .gen__tuning-summary")
    page.wait_for_function(
        "() => document.querySelector(\".gen[data-idx='1'] .pools__table\") !== null"
    )
    engine_options = page.locator(
        ".gen[data-idx='1'] select[data-act='device'][data-stage='engine'] option"
    ).all_inner_texts()
    assert "GPU 0 — NVIDIA GeForce RTX 4090 (25 GB)" in engine_options, (
        "the stages are tower's own, sent with the list"
    )
    assert "No entry for tower (RTX 4090) yet" in page.locator(
        ".gen[data-idx='1'] .pools__machine-note"
    ).inner_text()
    workers = page.locator(".gen[data-idx='1'] input[data-act='workers']").first
    stage = workers.get_attribute("data-stage")
    workers.fill("6")
    page.click(".gen[data-idx='1'] [data-act='pools-save']")
    page.wait_for_function("() => document.querySelector('.toast') !== null")
    assert len(stub.state.pools_puts) == 1
    generation, body = stub.state.pools_puts[0]
    assert generation == "g-2"
    assert body["processor"] == "tower"
    assert body["pools"]["stage_workers"] == {stage: 6}
    assert stub.state.last_put is None, "the row's own config was not touched"
    # Back on this server, the row's own table is what shows.
    page.locator(".gen[data-idx='1'] .pools__processor").select_option("local")
    value = page.locator(
        f".gen[data-idx='1'] input[data-act='workers'][data-stage='{stage}']"
    ).input_value()
    assert value != "6"


def _device_options(page: Page, idx: int, stage: str) -> list[str]:
    return page.locator(
        f".gen[data-idx='{idx}'] select[data-act='device'][data-stage='{stage}'] option"
    ).all_inner_texts()


def test_the_device_select_lists_the_chosen_machine_s_devices(
    stub: StubServer, page: Page
) -> None:
    """The owner's report: Machine set to tower, and the ``mokuro`` stage
    offered THIS library's Ryzen and Radeon. Each machine's select lists its
    own CPU and cards, the table is there the moment the machine is switched
    (no derive to wait on, nothing moves), and a machine that reported no
    devices offers only what it has: auto and its CPU."""
    _with_processors(stub)
    open_settings(stub, page)
    card = page.locator(".gen[data-idx='0']")
    machine = card.locator(".pools__processor")
    machine.select_option("local")
    page.click(".gen[data-idx='0'] .gen__tuning-summary")
    page.wait_for_function(
        "() => document.querySelector(\".gen[data-idx='0'] .pools__table\") !== null"
    )
    assert _device_options(page, 0, "mokuro") == [
        "Auto → GPU 0",
        "AMD Ryzen 9 7950X (16 cores)",
        "GPU 0 — AMD Radeon RX 9070 XT (16 GB)",
    ]
    table = card.locator(".pools__table")
    height = table.bounding_box()["height"]

    derives = len(stub.state.derive_processors)
    machine.select_option("tower")
    # Synchronously after the change: the table was never swapped for a
    # "Working out ..." line.
    assert page.evaluate(
        "() => document.querySelector(\".gen[data-idx='0'] .pools__table\") !== null"
    )
    tower = _device_options(page, 0, "mokuro")
    assert tower == [
        "Auto → GPU 0",
        "AMD Ryzen Threadripper 9960X 24-Cores (48 cores)",
        "GPU 0 — NVIDIA GeForce RTX 4090 (25 GB)",
    ]
    assert not any("Ryzen 9 7950X" in o or "Radeon" in o for o in tower)
    assert table.bounding_box()["height"] == height, "switching machines moves nothing"
    assert len(stub.state.derive_processors) == derives, "no derive was needed"

    machine.select_option("box")
    assert _device_options(page, 0, "mokuro") == ["Auto → CPU", "N100 (4 cores)"]


def test_a_benchmark_that_changed_nothing_is_not_called_no_entry(
    stub: StubServer, page: Page
) -> None:
    """tower's benchmark found the row's own table best ("best: auto"), so
    nothing was stored for it. "No entry for tower yet" read as though nothing
    had been done there; the note says what is true instead."""
    _with_processors(stub)
    unchanged = deepcopy(dict(BENCH_TUNED, processor="tower"))
    unchanged["best"] = dict(unchanged["best"], stage_workers={}, queue_capacity={},
                             speedup=1.0, same_as_spec=True)
    stub.state.bench_settled["g-2@tower"] = unchanged
    _open_tower_table(stub, page)
    page.wait_for_function(
        "() => (document.querySelector(\".gen[data-idx='1'] .bench-res__summary\") || {})"
        ".textContent && document.querySelector(\".gen[data-idx='1'] .bench-res__summary\")"
        ".textContent.includes('best: auto')"
    )
    note = page.locator(".gen[data-idx='1'] .pools__machine-note").inner_text()
    assert "tower runs this row's own table (its benchmark found nothing to change)." in note
    assert "No entry" not in note


def test_a_benchmark_result_not_applied_yet_is_still_no_entry(
    stub: StubServer, page: Page
) -> None:
    _with_processors(stub)
    stub.state.bench_settled["g-2@tower"] = deepcopy(dict(BENCH_TUNED, processor="tower"))
    _open_tower_table(stub, page)
    page.wait_for_function(
        "() => (document.querySelector(\".gen[data-idx='1'] .bench-res__summary\") || {})"
        ".textContent && document.querySelector(\".gen[data-idx='1'] .bench-res__summary\")"
        ".textContent.includes('best: detect')"
    )
    assert "No entry for tower (RTX 4090) yet" in page.locator(
        ".gen[data-idx='1'] .pools__machine-note"
    ).inner_text()


def _open_tower_table(stub: StubServer, page: Page) -> None:
    open_settings(stub, page)
    page.locator(".gen[data-idx='1'] .pools__processor").select_option("tower")
    page.click(".gen[data-idx='1'] .gen__tuning-summary")
    page.wait_for_function(
        "() => document.querySelector(\".gen[data-idx='1'] .pools__table\") !== null"
    )


def test_auto_against_a_row_s_pin_is_saved_as_auto(stub: StubServer, page: Page) -> None:
    """A machine's pools that name nothing are no opinion (the row's own
    table runs there), so a machine told to run a stage ``auto`` where the row
    PINS it has to say so: dropping the key would hand the stage back to the
    row's pin."""
    _with_processors(stub)
    stub.state.generations[1]["pools"] = {
        "stage_workers": {}, "queue_capacity": {}, "stage_device": {"detect": "cpu"},
    }
    _open_tower_table(stub, page)
    page.locator(
        ".gen[data-idx='1'] select[data-act='device'][data-stage='detect']"
    ).select_option("auto")
    page.wait_for_function(
        "() => document.querySelector(\".gen[data-idx='1'] .pools__table\") !== null"
    )
    page.click(".gen[data-idx='1'] [data-act='pools-save']")
    page.wait_for_function("() => document.querySelector('.toast') !== null")
    assert len(stub.state.pools_puts) == 1
    _generation, body = stub.state.pools_puts[0]
    assert body["pools"]["stage_device"] == {"detect": "auto"}


def test_a_stored_entry_of_empty_tables_shows_the_row_s_own(
    stub: StubServer, page: Page
) -> None:
    """What the incident's auto-benchmarks left: three empty tables. The
    server runs the row's own table there, so that is what the page shows."""
    _with_processors(stub)
    stub.state.generations[1]["processor_pools"] = {
        "tower": {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}},
    }
    _open_tower_table(stub, page)
    assert "No entry for tower (RTX 4090) yet" in page.locator(
        ".gen[data-idx='1'] .pools__machine-note"
    ).inner_text()
    value = page.locator(
        ".gen[data-idx='1'] input[data-act='workers'][data-stage='detect']"
    ).input_value()
    assert value == "2", "the row's own detect width"


def test_a_cleared_width_the_row_pins_is_saved_as_auto(
    stub: StubServer, page: Page
) -> None:
    """The row pins ``detect: 2``; tower should derive it. A table left empty
    is no opinion (the row's pin would run there), so the page says ``auto``."""
    _with_processors(stub)
    _open_tower_table(stub, page)
    cell = page.locator(".gen[data-idx='1'] input[data-act='workers'][data-stage='detect']")
    assert cell.input_value() == "2", "the row's own width, until tower says otherwise"
    cell.fill("")
    page.click(".gen[data-idx='1'] [data-act='pools-save']")
    page.wait_for_function("() => document.querySelector('.toast') !== null")
    assert len(stub.state.pools_puts) == 1
    _generation, body = stub.state.pools_puts[0]
    assert body["pools"]["stage_workers"] == {"detect": "auto"}


def test_a_stored_auto_width_shows_as_derived(stub: StubServer, page: Page) -> None:
    _with_processors(stub)
    stub.state.generations[1]["processor_pools"] = {
        "tower": {"stage_workers": {"detect": "auto"}, "queue_capacity": {},
                  "stage_device": {}},
    }
    _open_tower_table(stub, page)
    assert "Saved for tower (RTX 4090)" in page.locator(
        ".gen[data-idx='1'] .pools__machine-note"
    ).inner_text()
    cell = page.locator(".gen[data-idx='1'] input[data-act='workers'][data-stage='detect']")
    assert cell.input_value() == "", "derived there: the blank cell, not the row's 2"


def test_a_table_the_machine_leaves_empty_shows_the_row_s_own(
    stub: StubServer, page: Page
) -> None:
    """The server reads a machine's pools table by table: one it leaves empty
    runs the row's own there, so that is what the page shows."""
    _with_processors(stub)
    stub.state.generations[1]["pools"] = {
        "stage_workers": {"detect": 2}, "queue_capacity": {"post": 4}, "stage_device": {},
    }
    stub.state.generations[1]["processor_pools"] = {
        "tower": {"stage_workers": {"detect": 3}, "queue_capacity": {}, "stage_device": {}},
    }
    _open_tower_table(stub, page)
    workers = page.locator(".gen[data-idx='1'] input[data-act='workers'][data-stage='detect']")
    assert workers.input_value() == "3"
    capacity = page.locator(".gen[data-idx='1'] input[data-act='capacity'][data-stage='post']")
    assert capacity.input_value() == "4", "the row's own capacity"


def test_a_benchmark_runs_on_the_machine_the_table_is_for(
    stub: StubServer, page: Page
) -> None:
    _with_processors(stub)
    open_settings(stub, page)
    page.locator(".gen[data-idx='1'] .pools__processor").select_option("tower")
    page.click(".gen[data-idx='1'] .gen__tuning-summary")
    page.click(".gen[data-idx='1'] [data-act='bench-start']")
    deadline = time.monotonic() + 10
    posts: list[dict[str, Any]] = []
    while not posts and time.monotonic() < deadline:
        posts = [body for key, body in stub.state.bench_posts if key == "g-2"]
        time.sleep(0.05)
    assert posts, "no benchmark was asked for"
    assert posts[-1].get("processor") == "tower"


def _wait_for(predicate: Any, timeout: float = 10.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.05)
    return bool(predicate())


def test_a_benchmark_is_read_and_cancelled_on_the_machine_it_runs_on(
    stub: StubServer, page: Page
) -> None:
    """B6: one row can be queued on several machines (the worker queues
    automatic ones too), so every read and every Cancel names the machine
    -- never "whichever run of this row came first"."""
    _with_processors(stub)
    open_settings(stub, page)
    assert _wait_for(lambda: ("g-2", "local") in stub.state.bench_get_machines), (
        "the page reads this server's benchmark of each row by name"
    )
    page.locator(".gen[data-idx='1'] .pools__processor").select_option("tower")
    page.click(".gen[data-idx='1'] .gen__tuning-summary")
    assert _wait_for(lambda: ("g-2", "tower") in stub.state.bench_get_machines), (
        "switching the table to tower reads tower's benchmark"
    )
    page.click(".gen[data-idx='1'] [data-act='bench-start']")
    before = len(stub.state.bench_get_machines)
    assert _wait_for(lambda: len(stub.state.bench_get_machines) > before)
    polled = [m for key, m in stub.state.bench_get_machines[before:] if key == "g-2"]
    assert polled and set(polled) == {"tower"}, polled
    page.locator(".gen[data-idx='1'] [data-act='bench-cancel']").first.click()
    assert _wait_for(lambda: bool(stub.state.bench_delete_machines))
    assert stub.state.bench_delete_machines[-1] == ("g-2", "tower")


def test_a_processors_result_is_never_applied_to_the_rows_own_table(
    stub: StubServer, page: Page
) -> None:
    """B7: tower measured the row, then left. Its widths are tower's: they
    must not become the row's own table (the config default, and this
    server's pools), which the next Save would write to config.yaml."""
    stub.state.bench_settled["g-2"] = deepcopy(dict(BENCH_TUNED, processor="tower"))
    admin = open_settings(stub, page)
    admin.wait_for_selector(".gen[data-idx='1'] .bench-res")
    result = admin.locator(".gen[data-idx='1'] .bench-res")
    assert result.locator("[data-act='bench-apply']").count() == 0
    assert "not connected" in result.locator(".bench-res__gone").inner_text()
    # Even asked directly, the row's own pools are left alone.
    admin.evaluate("applyBench(1)")
    admin.fill(".gen[data-idx='2'] .gen__name", "renamed")
    admin.click("#gen-save-btn")
    admin.wait_for_selector("#gen-save-note:not([hidden])")
    assert stub.state.last_put is not None
    assert stub.state.last_put["generations"][1]["pools"]["stage_workers"] == {"detect": 2}, (
        "tower's detect x3 stayed out of this row's own table"
    )


def test_with_local_processing_off_all_machines_means_the_processors(
    stub: StubServer, page: Page
) -> None:
    """All machines is the default either way; with local processing off it
    adds up the processors alone, and benchmarks only them."""
    _with_processors(stub)
    stub.state.gen_local_processing = False
    open_settings(stub, page)
    card = page.locator(".gen[data-idx='1']")
    assert card.locator(".pools__processor").input_value() == "all"
    title = card.locator("[data-act='bench-start']").get_attribute("title") or ""
    assert "(tower (RTX 4090))" in title and "this server" not in title


# --- the card's Machine select: All machines ---------------------------------------


def _ppm_text(value: float) -> str:
    """Pages a minute as the card writes them (`genPpmText`)."""
    if value >= 1000:
        return f"{round(value):,}"
    return str(round(value)) if value >= 10 else f"{value:.1f}".rstrip("0").rstrip(".")


def _real_ppm(generation_id: str, machines: list[str]) -> list[float]:
    """Each machine's REAL pages a minute of one row, from the fixture the
    Processors card is sent ('local' is this server)."""
    out = []
    for entry in PROCESSORS["speed"]:
        name = "local" if entry["local"] else entry["name"]
        if name not in machines:
            continue
        for layer in entry["layers"]:
            if layer["generation_id"] == generation_id and layer["pages_per_minute"]:
                out.append(layer["pages_per_minute"])
    return out


def _history(page: Page, idx: int) -> str:
    return page.locator(f".gen[data-idx='{idx}'] .gen__history-summary").inner_text()


def _wait_history(page: Page, idx: int, text: str) -> None:
    page.wait_for_function(
        "([i, t]) => (document.querySelector(`.gen[data-idx='${i}'] .gen__history-summary`) || {})"
        ".textContent === t", arg=[idx, text], timeout=5000,
    )


def test_all_machines_is_the_default_and_adds_every_machine_up(
    stub: StubServer, page: Page
) -> None:
    """Owner: with All machines (the default) the card's figures are the sum
    over every machine, this server included -- pages a minute added up as
    combined throughput; the volume count is the library's, which every
    machine's sidecars make up together."""
    _with_processors(stub)
    open_settings(stub, page)
    card = page.locator(".gen[data-idx='1']")
    select = card.locator(".pools__processor")
    assert select.input_value() == "all"
    assert select.locator("option:checked").inner_text() == "All machines"
    machines = ["local", "tower", "box"]  # connected; old-laptop is not

    # g-1: this server and tower have both run it.
    g1 = stub.state.generations[0]
    rates = _real_ppm("g-1", machines)
    assert len(rates) == 2
    _wait_history(page, 0, f"History — {g1['volumes_done']}/{g1['volumes_total']} volumes · "
                           f"{_ppm_text(sum(rates))} pages/min combined")
    # g-2: only tower has; one skipped volume is part of the count.
    g2 = stub.state.generations[1]
    (tower,) = _real_ppm("g-2", machines)
    _wait_history(page, 1, f"History — {g2['volumes_done']}/{g2['volumes_total']} volumes · "
                           f"{g2['volumes_skipped']} skipped · {_ppm_text(tower)} pages/min")

    # The benchmark figures add up the same way: this server's own result
    # (2.10 pages a second) and tower's benchmark from its profile.
    local_bench = BENCH_TUNED["best"]["pages_per_second"] * 60
    tower_bench = next(
        layer["bench_pages_per_minute"] for entry in PROCESSORS["speed"] if entry["name"] == "tower"
        for layer in entry["layers"] if layer["generation_id"] == "g-2"
    )
    summary = card.locator(".bench-res__summary")
    assert summary.inner_text() == (
        f"Benchmark results — {_ppm_text(local_bench + tower_bench)} pages/min combined · "
        "2 of 3 machines"
    )
    summary.click()
    machines_list = card.locator(".bench-res__machines li")
    assert machines_list.all_inner_texts()[2] == "box not benchmarked"
    # Opened, the History says what each machine contributed.
    card.locator(".gen__history-summary").click()
    lines = card.locator(".gen__history-machines li")
    assert lines.count() == 3
    assert f"{_ppm_text(tower)} pages/min" in card.locator(
        ".gen__history-machines li[data-machine='tower']").inner_text()
    # Pools are per machine: the All view says where to find them.
    assert card.locator(".gen__tuning").inner_text() == "Pick a machine to see its pools"
    assert card.locator(".gen__tuning-summary").count() == 0


def test_one_machine_shows_only_its_own(stub: StubServer, page: Page) -> None:
    """A machine's History is its own contribution out of the library's total
    ("95/129 volumes") and its own pages a minute."""
    _with_processors(stub)
    open_settings(stub, page)
    card = page.locator(".gen[data-idx='1']")
    g2 = stub.state.generations[1]
    card.locator(".pools__processor").select_option("tower")
    (tower,) = _real_ppm("g-2", ["tower"])
    volumes = g2["volumes_by_machine"]["tower"]
    _wait_history(page, 1, f"History — tower (RTX 4090): {volumes}/{g2['volumes_total']} volumes · "
                           f"{_ppm_text(tower)} pages/min")
    assert card.locator(".bench-res").inner_text() == "Benchmark result — none on tower (RTX 4090) yet"
    card.locator(".pools__processor").select_option("local")
    _wait_history(page, 1, f"History — this server: 0/{g2['volumes_total']} volumes")
    summary = card.locator(".bench-res__summary").inner_text()
    assert summary.startswith("Benchmark result — best: detect ×3 · 126 pages/min · ")
    assert summary.endswith(" ago")
    assert card.locator(".gen__tuning-summary").inner_text() == "Pools and congestion"


@pytest.mark.parametrize("width", [1280, 400])
def test_switching_machines_moves_nothing(stub: StubServer, page: Page, width: int) -> None:
    """The owner's rule for status UIs: what a card shows changes in place.
    Folded, a card is exactly as tall whichever machine it shows."""
    _with_processors(stub)
    page.set_viewport_size({"width": width, "height": 1000})
    open_settings(stub, page)
    page.wait_for_selector("#processors-body tr")
    for index in (0, 1):
        card = page.locator(f".gen[data-idx='{index}']")
        heights = {}
        for machine in ["all", "local", "tower", "box", "all"]:
            card.locator(".pools__processor").select_option(machine)
            page.wait_for_timeout(250)
            box = card.bounding_box()
            assert box
            heights.setdefault(machine, set()).add(round(box["height"]))
            # The History summary is one line whatever it says.
            line = card.locator(".gen__history-summary").evaluate(
                "el => [el.getBoundingClientRect().height, "
                "parseFloat(getComputedStyle(el).lineHeight)]")
            assert round(line[0]) == round(line[1]), (index, machine, line)
        assert len(set().union(*heights.values())) == 1, (index, heights)


def test_benchmark_all_asks_every_machine_that_can_run_the_row(
    stub: StubServer, page: Page
) -> None:
    """Owner: in the All view, Benchmark & tune benchmarks and tunes every
    connected machine that can run the row, this server included, each in
    its own machine's line -- and says it is doing all of them."""
    _with_processors(stub)
    stub.state.bench_on_post["g-2"] = [BENCH_RUN_SEQUENCE[0]]  # stays pausing
    open_settings(stub, page)
    card = page.locator(".gen[data-idx='1']")
    button = card.locator("[data-act='bench-start']")
    assert button.inner_text() == "Benchmark & tune all"
    title = button.get_attribute("title") or ""
    assert "every machine that can run it (this server, tower (RTX 4090))" in title
    # box has no engines installed yet: not asked.
    assert "box" not in title
    button.click()
    assert _wait_for(lambda: len([k for k, _ in stub.state.bench_posts if k == "g-2"]) == 2)
    asked = sorted(str(body.get("processor") or "local") for key, body in stub.state.bench_posts
                   if key == "g-2")
    assert asked == ["local", "tower"]
    # Each machine measures the row with its own pools.
    assert all("spec" in body for _, body in stub.state.bench_posts)
    page.wait_for_selector(".gen[data-idx='1'] .bench-run__title:has-text('Benchmarking 2 machines')")
    nums = card.locator(".bench-run__nums").inner_text()
    # tower reads "queued" until its first poll answers, "pausing" after it.
    assert "this server: " in nums
    assert re.search(r"tower \(RTX 4090\): (queued|pausing)", nums), nums
    # Both are followed, each on its own machine.
    assert _wait_for(lambda: {("g-2", "tower"), ("g-2", "local")} <= set(stub.state.bench_get_machines))
    # One Cancel stops them all.
    card.locator("[data-act='bench-cancel']").click()
    assert _wait_for(lambda: {("g-2", "tower"), ("g-2", "local")}
                     <= set(stub.state.bench_delete_machines))
    page.wait_for_selector(".gen[data-idx='1'] [data-act='bench-start']")
    assert "Benchmark cancelled on tower (RTX 4090)." in card.locator(".gen__bench").inner_text()


def test_benchmark_all_with_nothing_able_to_run_it_says_so(stub: StubServer, page: Page) -> None:
    _with_processors(stub)
    stub.state.gen_local_processing = False
    stub.state.gen_processors = [p for p in stub.state.gen_processors if p["name"] == "box"]
    open_settings(stub, page)
    page.click(".gen[data-idx='1'] [data-act='bench-start']")
    page.wait_for_selector(".gen[data-idx='1'] .bench-bar__note")
    assert page.locator(".gen[data-idx='1'] .bench-bar__note").inner_text() == (
        "No connected machine can run this generation.")
    assert [k for k, _ in stub.state.bench_posts] == []


# --- the queue page ------------------------------------------------------------


def _queue(stub: StubServer, page: Page) -> Page:
    page.goto(stub.url + "/queue/")
    page.wait_for_load_state("networkidle")
    return page


def test_the_queue_page_names_the_machine_a_volume_is_running_on(
    stub: StubServer, page: Page
) -> None:
    stub.state.queue_status = deepcopy(QUEUE_STATUS_REMOTE)
    _queue(stub, page)
    page.wait_for_selector(".machine .lane[data-job]")
    # Its name (a hostname by default) and hardware are an admin's business.
    assert page.locator(".machine__name").inner_text().lower() == "machine 1"
    assert "RTX 4090" not in page.locator("body").inner_text()
    assert "tower" not in page.locator("body").inner_text()
    assert page.locator(".machine__label").is_hidden()


def test_an_admin_is_shown_the_hardware_too(stub: StubServer, page: Page) -> None:
    stub.state.queue_status = deepcopy(QUEUE_STATUS_REMOTE)
    stub.state.queue_admin = True
    _queue(stub, page)
    page.wait_for_selector(".machine .lane[data-job]")
    assert page.locator(".machine__label").inner_text() == "tower (RTX 4090)"


def test_the_queue_page_says_when_it_is_holding_for_a_processor(
    stub: StubServer, page: Page
) -> None:
    stub.state.queue_status = deepcopy(QUEUE_STATUS_NO_PROCESSOR)
    _queue(stub, page)
    banner = page.locator("#processing-hold")
    banner.wait_for(state="visible")
    assert "No processor connected since" in banner.inner_text()
    # A visitor is given the machine's alias, never its name.
    assert "last: machine 1" in banner.inner_text()
    assert "tower" not in banner.inner_text()


def test_the_queue_page_is_unchanged_on_a_single_machine(
    stub: StubServer, page: Page
) -> None:
    _queue(stub, page)
    page.wait_for_selector(".machine .lane[data-job]")
    assert page.locator("#processing-hold").is_hidden()
    assert [n.lower() for n in page.locator(".machine__name").all_inner_texts()] == [
        "this server"
    ]


# --- the processor role in the user menus ----------------------------------------


def test_a_processor_account_can_be_created(stub: StubServer, page: Page) -> None:
    open_users(stub, page)
    page.click("#add-user-btn")
    page.fill("#new-username", "tower")
    page.fill("#new-password", "a-long-enough-password")
    page.select_option("#new-role", "processor")
    page.click("#add-user-form button[type='submit']")
    page.wait_for_function("() => document.querySelector('.toast') !== null")
    assert stub.state.user_posts[-1]["role"] == "processor"


def test_a_processor_account_shows_its_role_and_keeps_it(
    stub: StubServer, page: Page
) -> None:
    stub.state.users = [
        {"username": "tower", "role": "processor", "status": "active", "notes": "",
         "created_at": "2026-09-22T00:00:00Z"},
    ]
    open_users(stub, page)
    page.wait_for_selector("text=tower")
    page.click("button:has-text('Role')")
    select = page.locator("#change-role-select")
    assert select.input_value() == "processor", "never a blank select"
    page.click("#change-role-form button[type='submit']")
    page.wait_for_function("() => document.querySelector('.toast') !== null")
    assert stub.state.role_puts[-1] == ("tower", {"role": "processor"})


def test_the_processor_role_is_never_offered_by_invite(stub: StubServer, page: Page) -> None:
    open_users(stub, page)
    values = page.locator("#invite-role option").evaluate_all(
        "options => options.map(o => o.value)"
    )
    assert "processor" not in values


def test_the_congestion_shown_is_the_machines_own(stub: StubServer, page: Page) -> None:
    _with_processors(stub)
    stub.state.generations[1]["processor_congestion"] = {
        "tower": {"runs": 3, "last_run_at": None, "verdict": None, "bottleneck": "detect",
                  "stages": [{"key": "detect", "workers": 3, "busy_pct": 90,
                              "starved_pct": 0, "blocked_pct": 5}],
                  "queues": []},
    }
    open_settings(stub, page)
    page.locator(".gen[data-idx='1'] .pools__processor").select_option("tower")
    page.click(".gen[data-idx='1'] .gen__tuning-summary")
    page.click(".gen[data-idx='1'] .gen__why-summary")
    stages = page.locator(".gen[data-idx='1'] .cong__stage-name").all_inner_texts()
    assert stages == ["detect"], "tower's own runs, not this server's"
    assert "3 runs" in page.locator(".gen[data-idx='1'] .cong__meta").inner_text()
    page.locator(".gen[data-idx='1'] .pools__processor").select_option("box")
    assert "No queue runs recorded on box yet" in page.locator(
        ".gen[data-idx='1'] .gen__tuning-body"
    ).inner_text()


# --- this server's own auto-benchmark ----------------------------------------------


def test_an_unconfigured_row_says_what_this_server_s_benchmark_found(
    stub: StubServer, page: Page
) -> None:
    stub.state.generations[3]["local_pools"] = {
        "stage_workers": {"detect": 3}, "queue_capacity": {},
        "stage_device": {"detect": "cpu"},
    }
    stub.state.generations[3]["local_bench"] = {"pages_per_second": 7.5}
    open_settings(stub, page)
    page.click(".gen[data-idx='3'] .gen__tuning-summary")
    note = page.locator(".gen[data-idx='3'] .pools__machine-note--local")
    assert note.inner_text() == (
        "Not configured by hand: this server runs what its benchmark found "
        "(detect ×3, detect on cpu). Benchmark: 7.5 pages a second. "
        "Set any value to configure it yourself."
    )


def test_a_configured_row_says_nothing_about_this_server_s_benchmark(
    stub: StubServer, page: Page
) -> None:
    stub.state.generations[1]["local_pools"] = {"stage_workers": {"detect": 3}}
    open_settings(stub, page)
    page.click(".gen[data-idx='1'] .gen__tuning-summary")
    page.wait_for_selector(".gen[data-idx='1'] .pools__table")
    assert page.locator(".gen[data-idx='1'] .pools__machine-note--local").count() == 0


def test_an_unmeasured_row_says_it_is_benchmarked_first(stub: StubServer, page: Page) -> None:
    open_settings(stub, page)
    page.click(".gen[data-idx='3'] .gen__tuning-summary")
    assert "benchmarks it before it runs it" in page.locator(
        ".gen[data-idx='3'] .pools__machine-note--local"
    ).inner_text()

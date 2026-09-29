"""A generation card's name row and its History line.

No sample file name under the Name any more; the "Reset to <name>" button
keeps a slot of its own, so the card is exactly as tall whether it is offered
or not. History is the library's total in All machines, and one machine's own
contribution (its lifetime volumes of the row, its pages a minute) when a
machine is chosen -- one line either way.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

import pytest

from .generations_stub import PROCESSORS, StubServer
from .test_processors_panel import (  # noqa: F401 - fixtures
    BROWSERS_AVAILABLE,
    _ppm_text,
    _wait_history,
    _with_processors,
    open_settings,
    page,
    stub,
)

if TYPE_CHECKING:
    from playwright.sync_api import Page

pytestmark = pytest.mark.skipif(
    not BROWSERS_AVAILABLE, reason="Playwright browsers not available"
)


def _card(page: Page, index: int):  # noqa: F811
    return page.locator(f".gen[data-idx='{index}']")


def _speed(machine: str, generation_id: str) -> float:
    for entry in PROCESSORS["speed"]:
        name = "local" if entry["local"] else entry["name"]
        if name != machine:
            continue
        for layer in entry["layers"]:
            if layer["generation_id"] == generation_id:
                return layer["pages_per_minute"]
    raise AssertionError((machine, generation_id))


# --- the name row ------------------------------------------------------------------


def test_there_is_no_sample_file_name(stub: StubServer, page: Page) -> None:  # noqa: F811
    open_settings(stub, page)
    assert page.locator(".gen__file").count() == 0
    assert "Volume 01." not in page.locator("#gen-list").inner_text()
    # The name field is described by what is still there: its message.
    name = _card(page, 2).locator(".gen__name")
    ids = (name.get_attribute("aria-describedby") or "").split()
    assert ids == ["gen-msg-2"]
    assert page.locator("#gen-msg-2").count() == 1


@pytest.mark.parametrize("width", [1280, 400])
def test_the_reset_button_coming_and_going_moves_nothing(
    stub: StubServer, page: Page, width: int  # noqa: F811
) -> None:
    page.set_viewport_size({"width": width, "height": 1000})
    open_settings(stub, page)
    card = _card(page, 2)
    name = card.locator(".gen__name")
    reset = card.locator("[data-act='reset-name']")
    default = name.input_value()

    def height() -> int:
        box = card.bounding_box()
        assert box
        return round(box["height"])

    assert not reset.is_visible()
    without = height()
    name.fill("my-nova")
    assert reset.is_visible()
    assert reset.inner_text() == f"Reset to {default}"
    with_reset = height()
    # Back to the default by hand: the offer goes, the slot stays.
    name.fill(default)
    assert not reset.is_visible()
    again = height()
    # ... and by the button itself.
    name.fill("my-nova")
    reset.click()
    assert name.input_value() == default
    assert not reset.is_visible()
    after_click = height()
    assert without == with_reset == again == after_click, (without, with_reset, again, after_click)


def test_a_hidden_reset_is_not_reachable(stub: StubServer, page: Page) -> None:  # noqa: F811
    open_settings(stub, page)
    reset = _card(page, 2).locator("[data-act='reset-name']")
    assert reset.get_attribute("tabindex") == "-1"
    assert reset.get_attribute("aria-hidden") == "true"
    _card(page, 2).locator(".gen__name").fill("my-nova")
    assert reset.get_attribute("tabindex") is None
    assert reset.get_attribute("aria-hidden") is None


# --- History, per machine ------------------------------------------------------------


def test_all_machines_shows_the_library_total(stub: StubServer, page: Page) -> None:  # noqa: F811
    _with_processors(stub)
    open_settings(stub, page)
    g1 = stub.state.generations[0]
    total = _speed("local", "g-1") + _speed("tower", "g-1")
    _wait_history(page, 0, f"History — {g1['volumes_done']}/{g1['volumes_total']} volumes · "
                           f"{_ppm_text(total)} pages/min combined")


def test_a_processor_shows_its_own_counts(stub: StubServer, page: Page) -> None:  # noqa: F811
    """The share is the machine's sidecars on disk (exact); its lifetime count,
    re-runs included, is only in the tooltip."""
    _with_processors(stub)
    open_settings(stub, page)
    _card(page, 0).locator(".pools__processor").select_option("tower")
    g1 = stub.state.generations[0]
    line = (f"History — tower (RTX 4090): {g1['volumes_by_machine']['tower']}/"
            f"{g1['volumes_total']} volumes · {_ppm_text(_speed('tower', 'g-1'))} pages/min")
    _wait_history(page, 0, line)
    lifetime = g1["processor_runs"]["tower"]["volumes"]
    assert _card(page, 0).locator(".gen__history-summary").get_attribute("title") == (
        line + f"\n{lifetime} volumes over its lifetime, including re-runs"
    )


def test_this_server_s_line_uses_its_exact_count(stub: StubServer, page: Page) -> None:  # noqa: F811
    _with_processors(stub)
    open_settings(stub, page)
    _card(page, 0).locator(".pools__processor").select_option("local")
    g1 = stub.state.generations[0]
    line = (f"History — this server: {g1['volumes_by_machine']['local']}/"
            f"{g1['volumes_total']} volumes · {_ppm_text(_speed('local', 'g-1'))} pages/min")
    _wait_history(page, 0, line)
    title = _card(page, 0).locator(".gen__history-summary").get_attribute("title")
    assert title == line + f"\n{g1['local_runs']['volumes']} volumes over its lifetime, including re-runs"


def test_a_lifetime_above_the_total_never_reaches_the_line(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    """The drift this fixes: 97 runs of a 34-volume row read "97/34"."""
    _with_processors(stub)
    stub.state.generations[0]["processor_runs"]["tower"]["volumes"] = 97
    open_settings(stub, page)
    _card(page, 0).locator(".pools__processor").select_option("tower")
    g1 = stub.state.generations[0]
    _wait_history(page, 0, f"History — tower (RTX 4090): 18/{g1['volumes_total']} volumes · "
                           f"{_ppm_text(_speed('tower', 'g-1'))} pages/min")
    title = _card(page, 0).locator(".gen__history-summary").get_attribute("title") or ""
    assert title.endswith("\n97 volumes over its lifetime, including re-runs")


def test_every_machine_s_share_sums_to_at_most_the_total(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    _with_processors(stub)
    open_settings(stub, page)
    g1 = stub.state.generations[0]
    shares = []
    for machine in ("local", "tower", "box"):
        _card(page, 0).locator(".pools__processor").select_option(machine)
        page.wait_for_function(
            "m => (document.querySelector(`.gen[data-idx='0'] .gen__history-summary`) || {})"
            ".textContent.includes(m)", arg="this server" if machine == "local" else machine,
            timeout=5000,
        )
        text = _card(page, 0).locator(".gen__history-summary").inner_text()
        shares.append(int(text.split(": ", 1)[1].split("/", 1)[0]))
    assert shares == [7, 18, 0]
    assert sum(shares) <= g1["volumes_total"]


def test_a_count_without_a_speed_is_still_said(stub: StubServer, page: Page) -> None:  # noqa: F811
    _with_processors(stub)
    stub.state.generations[2]["volumes_by_machine"] = {"local": 3}
    open_settings(stub, page)
    _card(page, 2).locator(".pools__processor").select_option("local")
    _wait_history(page, 2, f"History — this server: 3/{stub.state.generations[2]['volumes_total']} volumes")


def test_a_machine_that_has_read_none_says_so(stub: StubServer, page: Page) -> None:  # noqa: F811
    _with_processors(stub)
    open_settings(stub, page)
    _card(page, 0).locator(".pools__processor").select_option("box")
    _wait_history(page, 0, f"History — box: 0/{stub.state.generations[0]['volumes_total']} volumes")
    summary = _card(page, 0).locator(".gen__history-summary")
    assert summary.get_attribute("title") == summary.inner_text()


def test_one_machine_single_install_keeps_the_library_line(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    """No Machine select: the only machine IS all of them."""
    open_settings(stub, page)
    g1 = stub.state.generations[0]
    _wait_history(page, 0, f"History — {g1['volumes_done']}/{g1['volumes_total']} volumes · "
                           f"{_ppm_text(_speed('local', 'g-1'))} pages/min")

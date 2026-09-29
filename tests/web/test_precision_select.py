"""The row's one precision MODE, on the card (``precision``, ``precision_on``).

One mode per generation, for every machine: the select sits beside the
Machine select, in every view (All machines included) and on a single-machine
install; its options are the engine's own (mokuro never offers bf16,
ppocr-manga fixes its own and has none); the PUT carries it at the top of the
row and never in the pools; a processor's pools save never carries it. With
one machine shown, a quiet line says what the mode comes to there, from the
server's ``precision_on``; when no connected machine can run the mode the card
says so; Benchmark all leaves out a machine that cannot run it; and none of it
moves the card.
"""

from __future__ import annotations

import time
from copy import deepcopy
from typing import TYPE_CHECKING, Any

import pytest

from .generations_stub import BENCH_RUN_SEQUENCE, BENCH_TUNED, StubServer
from .test_processors_panel import (  # noqa: F401 - fixtures
    BROWSERS_AVAILABLE,
    _open_tower_table,
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

PRECISION = "[data-act='precision']"
MODES = ["auto-accuracy", "auto-balanced", "auto-speed", "fp32", "bf16", "fp16"]
LABELS = [
    "Auto: accuracy (default)",
    "Auto: balanced",
    "Auto: speed",
    "fp32 only",
    "bf16 only (cards that support it)",
    "fp16 only (GPUs)",
]


def _card(page: Page, index: int):  # noqa: F811
    return page.locator(f".gen[data-idx='{index}']")


def _line(page: Page, index: int) -> str:  # noqa: F811
    return _card(page, index).locator(".gen__precision-line").inner_text()


def _mode(page: Page, index: int, mode: str) -> None:  # noqa: F811
    _card(page, index).locator(PRECISION).select_option(mode)


def _machine(page: Page, index: int, machine: str) -> None:  # noqa: F811
    _card(page, index).locator(".pools__processor").select_option(machine)


def _save(page: Page) -> None:  # noqa: F811
    page.click("#gen-save-btn")
    page.wait_for_selector("#gen-save-note:not([hidden])")


def _wait_for(predicate: Any, timeout: float = 10.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.05)
    return bool(predicate())


def _ineligible(stub: StubServer, index: int, machine: str, mode: str) -> None:  # noqa: F811
    stub.state.generations[index]["precision_on"][machine][mode] = {
        "precision": None, "eligible": False, "why": f"{mode} not supported",
    }


# --- where it is, and what it offers ------------------------------------------


def test_the_mode_is_on_the_card_beside_the_machine_in_every_view(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    _with_processors(stub)
    open_settings(stub, page)
    card = _card(page, 1)  # hayai-nova
    row = card.locator(".gen__fields--machine")
    assert row.locator(".pools__processor").input_value() == "all"
    assert row.locator(PRECISION).is_visible(), "All machines has the mode too"
    for machine in ["local", "tower", "box", "all"]:
        _machine(page, 1, machine)
        assert card.locator(f".gen__fields--machine {PRECISION}").is_visible(), machine
    # The pools table is per machine, and the mode is not: none in there.
    _machine(page, 1, "local")
    card.locator(".gen__tuning-summary").click()
    page.wait_for_selector(".gen[data-idx='1'] .pools__table")
    assert card.locator(f".gen__tuning {PRECISION}").count() == 0
    assert card.locator(".gen__tuning select").count() > 0


def test_a_single_machine_still_has_the_mode(stub: StubServer, page: Page) -> None:  # noqa: F811
    open_settings(stub, page)
    card = _card(page, 1)
    assert card.locator(".pools__processor").count() == 0
    assert card.locator(f".gen__fields--machine {PRECISION}").is_visible()
    # One machine: the line speaks for it.
    assert _line(page, 1) == "runs bf16 here"


def test_the_options_are_the_engine_s_modes_with_the_default_marked(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    open_settings(stub, page)
    select = _card(page, 3).locator(PRECISION)  # paddle-manga
    assert select.locator("option").all_inner_texts() == LABELS
    assert select.locator("option").evaluate_all("os => os.map(o => o.value)") == MODES
    assert select.input_value() == "auto-accuracy"
    # mokuro never runs bf16, so it is never offered.
    mokuro = _card(page, 0).locator(PRECISION)
    values = mokuro.locator("option").evaluate_all("os => os.map(o => o.value)")
    assert values == [mode for mode in MODES if mode != "bf16"]
    # ppocr-manga fixes its own precision: no select, no line.
    assert _card(page, 4).locator(PRECISION).count() == 0
    assert _card(page, 4).locator(".gen__precision-line").count() == 0


def test_the_hint_is_a_few_words_and_the_rules_are_its_tooltip(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    open_settings(stub, page)
    card = _card(page, 1)
    hint = card.locator(".gen__precision-hint")
    assert hint.inner_text() == "One mode for every machine."
    assert len(hint.inner_text().split()) <= 6
    rules = hint.get_attribute("title") or ""
    select = card.locator(PRECISION)
    assert select.get_attribute("title") == rules
    described = select.get_attribute("aria-describedby") or ""
    assert page.locator("#" + described).text_content() == rules
    for said in (
        "on every machine",
        "picks what tested most accurate for each engine, and never fp16",
        "Auto: balanced gives up a little accuracy for a lot of speed",
        "Auto: speed takes the fastest format each card runs well",
        "each machine benchmarks this generation automatically before its first volume, "
        "and again when the mode changes, and keeps the fastest",
        "within 5%, the more accurate one",
        "Only with automatic benchmarks off, or when a machine's benchmark failed, does it "
        "use the first format its card supports",
        "is not eligible",
        "mokuro never runs bf16",
    ):
        assert said in rules, said
    # The fallback is not the normal case, and is never said as if it were.
    assert "until a machine has benchmarked" not in rules
    assert len(rules.split()) <= 120, "readable: shorter rather than longer"


# --- saving ----------------------------------------------------------------------


def test_the_put_carries_the_mode_at_the_top_of_the_row(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    open_settings(stub, page)
    _mode(page, 3, "fp16")
    assert page.locator("#gen-dirty").is_visible()
    _save(page)
    rows = stub.state.last_put["generations"]
    assert rows[3]["precision"] == "fp16"
    # Always said for an engine that takes one, the default included.
    assert rows[0]["precision"] == "auto-accuracy"
    assert rows[1]["precision"] == "auto-accuracy"
    assert "precision" not in rows[4], "ppocr-manga takes no mode"
    assert all("precision" not in row["pools"] for row in rows)


def test_an_absent_mode_is_the_default(stub: StubServer, page: Page) -> None:  # noqa: F811
    del stub.state.generations[1]["precision"]
    open_settings(stub, page)
    assert _card(page, 1).locator(PRECISION).input_value() == "auto-accuracy"
    assert page.locator("#gen-dirty").is_hidden()


def test_a_processor_s_pools_save_never_carries_the_mode(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    _with_processors(stub)
    _open_tower_table(stub, page)
    _mode(page, 1, "fp32")
    _card(page, 1).locator("input[data-act='workers']").first.fill("5")
    page.click(".gen[data-idx='1'] [data-act='pools-save']")
    page.wait_for_function("() => document.querySelector('.toast') !== null")
    assert len(stub.state.pools_puts) == 1
    _generation, body = stub.state.pools_puts[0]
    assert "precision" not in body
    assert "precision" not in body["pools"]
    assert stub.state.last_put is None
    # The mode is the row's, saved with the list.
    _save(page)
    assert stub.state.last_put["generations"][1]["precision"] == "fp32"


def test_changing_to_an_engine_without_the_mode_resets_it(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    open_settings(stub, page)
    card = _card(page, 1)
    _mode(page, 1, "fp16")
    card.locator("[data-act='engine']").select_option("paddle-manga")
    assert card.locator(PRECISION).input_value() == "fp16", "paddle-manga offers fp16"
    _mode(page, 1, "bf16")
    card.locator("[data-act='engine']").select_option("mokuro")
    assert card.locator(PRECISION).input_value() == "auto-accuracy", "mokuro has no bf16"
    # The name follows the engine as it always has (a derived name, and row
    # 0 already holds "mokuro"); the mode takes no part in it.
    assert card.locator(".gen__name").input_value() == "mokuro-2"


def test_applying_a_benchmark_never_touches_the_mode(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    """A result from an older runner may still carry the precision its phase
    chose: applying it takes the widths, never that."""
    stub.state.generations[1]["precision"] = "fp32"
    tuned = deepcopy(BENCH_TUNED)
    tuned["spec"] = dict(tuned["spec"], precision="fp32")
    tuned["best"].update(precision="fp16")
    stub.state.script_bench("g-2", [tuned])
    open_settings(stub, page)
    page.wait_for_selector(".gen[data-idx='1'] [data-act='bench-apply']")
    page.locator(".gen[data-idx='1'] [data-act='bench-apply']").click()
    page.wait_for_selector(".gen[data-idx='1'] .pools__table")
    assert _card(page, 1).locator(PRECISION).input_value() == "fp32"
    _save(page)
    sent = stub.state.last_put["generations"][1]
    assert sent["precision"] == "fp32"
    assert sent["pools"]["stage_workers"] == {"detect": 3}
    assert "precision" not in sent["pools"]


def test_a_result_that_moved_nothing_but_a_precision_offers_nothing(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    tuned = deepcopy(BENCH_TUNED)
    tuned["best"].update(stage_workers={}, queue_capacity={}, stage_device={}, precision="bf16")
    stub.state.script_bench("g-2", [tuned])
    open_settings(stub, page)
    page.wait_for_selector(".gen[data-idx='1'] .bench-res__conclusion", state="attached")
    assert page.locator(".gen[data-idx='1'] [data-act='bench-apply']").count() == 0
    assert "Auto is already the best" in page.locator(
        ".gen[data-idx='1'] .bench-res__conclusion"
    ).inner_text()


def test_a_result_measured_in_another_mode_says_so(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    open_settings(stub, page)
    result = _card(page, 1).locator(".bench-res")
    assert "different settings" not in result.inner_text().lower()
    _mode(page, 1, "fp32")
    page.wait_for_function(
        "() => document.querySelector(\".gen[data-idx='1'] .bench-res\").textContent"
        ".includes('different settings')"
    )
    assert "Auto: accuracy" in result.inner_text()


def test_a_result_from_before_the_mode_counts_as_the_default(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    """A spec measured before rows had a mode names none: it was the
    default, and is not "different settings" from a row still on it."""
    summary = stub.state.generations[1]["bench"]
    summary["spec"] = {k: v for k, v in summary["spec"].items() if k != "precision"}
    open_settings(stub, page)
    page.wait_for_selector(".gen[data-idx='1'] .bench-res")
    assert "different settings" not in _card(page, 1).locator(".bench-res").inner_text().lower()


# --- what the mode comes to, per machine ------------------------------------------


def test_the_line_says_what_the_mode_runs_on_the_machine_shown(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    _with_processors(stub)
    open_settings(stub, page)
    assert _line(page, 1) == "", "All machines: nothing to say while someone can run it"
    expected = {"local": "runs bf16 here", "tower": "runs bf16 here", "box": "runs fp32 here"}
    for machine, text in expected.items():
        _machine(page, 1, machine)
        assert _line(page, 1) == text, machine
    # A forced format box's CPU cannot run: box is not eligible.
    _mode(page, 1, "bf16")
    assert _line(page, 1) == "not eligible here: bf16 not supported"
    _machine(page, 1, "tower")
    assert _line(page, 1) == "runs bf16 here"
    # The why rides along on hover.
    title = _card(page, 1).locator(".gen__precision-line").get_attribute("title") or ""
    assert title == "runs bf16 here (bf16 only)"
    _machine(page, 1, "all")
    assert _line(page, 1) == ""


def test_a_measured_mode_shows_the_benchmark_s_numbers(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    open_settings(stub, page)
    assert _line(page, 3) == "runs fp32 here"  # paddle-manga, accuracy
    _mode(page, 3, "auto-balanced")
    assert _line(page, 3) == "fp32 (benchmark: 0.57 vs 0.32 p/s)"
    _mode(page, 3, "auto-speed")
    assert _line(page, 3) == "benchmark pending", "no pick yet: it is benchmarked first"


@pytest.mark.parametrize(
    ("bench", "said"),
    [
        ("pending", "benchmark pending"),
        ("off", "not benchmarked (automatic benchmarks off), using bf16"),
        ("failed", "benchmark failed, using bf16"),
    ],
)
def test_a_pick_not_yet_had_says_where_it_stands(
    stub: StubServer, page: Page, bench: str, said: str  # noqa: F811
) -> None:
    stub.state.generations[3]["precision_on"]["local"]["auto-speed"] = {
        "precision": "bf16", "eligible": True,
        "why": "not benchmarked yet: first supported candidate", "bench": bench,
    }
    open_settings(stub, page)
    _mode(page, 3, "auto-speed")
    assert _line(page, 3) == said


def test_a_pick_says_its_numbers_pick_first(stub: StubServer, page: Page) -> None:  # noqa: F811
    stub.state.generations[3]["precision_on"]["local"]["auto-speed"] = {
        "precision": "bf16", "eligible": True, "why": "benchmark", "bench": "done",
        "trials": [{"precision": "fp32", "pages_per_second": 1.5},
                   {"precision": "bf16", "pages_per_second": 3.1}],
    }
    open_settings(stub, page)
    _mode(page, 3, "auto-speed")
    assert _line(page, 3) == "bf16 (benchmark: 3.1 vs 1.5 p/s)"


@pytest.mark.parametrize("width", [1280, 400])
def test_the_pick_s_states_move_nothing(
    stub: StubServer, page: Page, width: int  # noqa: F811
) -> None:
    states = {
        "auto-balanced": {"precision": "bf16", "eligible": True, "why": "b", "bench": "done",
                          "trials": [{"precision": "bf16", "pages_per_second": 3.1},
                                     {"precision": "fp32", "pages_per_second": 1.5}]},
        "auto-speed": {"precision": "bf16", "eligible": True, "why": "b", "bench": "failed"},
        "fp32": {"precision": "fp32", "eligible": True, "why": "fp32"},
    }
    stub.state.generations[3]["precision_on"]["local"].update(states)
    page.set_viewport_size({"width": width, "height": 1000})
    open_settings(stub, page)
    card = _card(page, 3)
    heights = set()
    for mode in ["auto-accuracy", "auto-balanced", "auto-speed", "fp32", "auto-accuracy"]:
        _mode(page, 3, mode)
        page.wait_for_timeout(100)
        box = card.bounding_box()
        assert box
        heights.add(round(box["height"]))
    assert len(heights) == 1, heights


def test_a_machine_that_did_not_report_its_card_decides_at_start(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    stub.state.generations[1]["precision_on"]["local"]["auto-accuracy"] = {
        "precision": None, "eligible": True, "why": "decided at start (card not reported)",
    }
    open_settings(stub, page)
    assert _line(page, 1) == "decided when it starts here"


def test_after_an_engine_change_the_line_waits_for_a_save(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    """`precision_on` was worked out for the saved engine: it says nothing
    about another one."""
    open_settings(stub, page)
    _card(page, 1).locator("[data-act='engine']").select_option("paddle-manga")
    assert _line(page, 1) == ""


# --- held: nobody connected can run the mode ---------------------------------------


def test_a_mode_no_connected_machine_can_run_is_said_plainly(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    _with_processors(stub)
    stub.state.gen_local_processing = False
    stub.state.gen_processors = [p for p in stub.state.gen_processors if p["name"] == "box"]
    open_settings(stub, page)
    card = _card(page, 1)
    before = card.bounding_box()
    _mode(page, 1, "bf16")
    assert _line(page, 1) == "No connected machine can run bf16"
    hold = card.locator(".gen__precision-hold")
    assert hold.is_visible()
    after = card.bounding_box()
    assert before and after and round(before["height"]) == round(after["height"])
    # On a machine's own view the machine's line comes first, the hold after.
    _machine(page, 1, "box")
    assert _line(page, 1) == (
        "not eligible here: bf16 not supported · No connected machine can run bf16")
    _mode(page, 1, "fp32")
    assert _line(page, 1) == "runs fp32 here"
    assert card.locator(".gen__precision-hold").count() == 0


def test_the_saved_hold_is_shown_when_there_is_nothing_live(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    del stub.state.generations[1]["precision_on"]
    stub.state.generations[1]["precision"] = "bf16"
    stub.state.generations[1]["precision_hold"] = "No connected machine can run bf16"
    open_settings(stub, page)
    assert _line(page, 1) == "No connected machine can run bf16"
    _mode(page, 1, "fp32")
    assert _line(page, 1) == "", "the hold was the saved mode's"


# --- Benchmark & tune all ------------------------------------------------------------


def test_benchmark_all_leaves_out_a_machine_that_cannot_run_the_mode(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    _with_processors(stub)
    _ineligible(stub, 1, "tower", "fp16")
    stub.state.bench_on_post["g-2"] = [BENCH_RUN_SEQUENCE[0]]  # stays pausing
    open_settings(stub, page)
    card = _card(page, 1)
    button = card.locator("[data-act='bench-start']")
    assert "(this server, tower (RTX 4090))" in (button.get_attribute("title") or "")
    _mode(page, 1, "fp16")
    title = button.get_attribute("title") or ""
    assert "(this server)" in title and "tower" not in title
    button.click()
    assert _wait_for(lambda: len([k for k, _ in stub.state.bench_posts if k == "g-2"]) >= 1)
    time.sleep(0.3)
    asked = [str(body.get("processor") or "local") for key, body in stub.state.bench_posts
             if key == "g-2"]
    assert asked == ["local"]
    assert stub.state.bench_posts[0][1]["spec"]["precision"] == "fp16"


def test_benchmark_all_with_nobody_able_to_run_the_mode_says_so(
    stub: StubServer, page: Page  # noqa: F811
) -> None:
    _with_processors(stub)
    _ineligible(stub, 1, "tower", "fp16")
    _ineligible(stub, 1, "local", "fp16")
    open_settings(stub, page)
    _mode(page, 1, "fp16")
    page.click(".gen[data-idx='1'] [data-act='bench-start']")
    page.wait_for_selector(".gen[data-idx='1'] .bench-bar__note")
    assert page.locator(".gen[data-idx='1'] .bench-bar__note").inner_text() == (
        "No connected machine can run this generation.")
    assert [k for k, _ in stub.state.bench_posts] == []


# --- no layout motion -----------------------------------------------------------------


@pytest.mark.parametrize("width", [1280, 400])
def test_switching_machines_and_modes_moves_nothing(
    stub: StubServer, page: Page, width: int  # noqa: F811
) -> None:
    """The owner's rule for status UIs: the card is exactly as tall whichever
    machine it shows and whichever mode it is set to -- a benchmark's numbers,
    a machine that is not eligible, and the hold included."""
    _with_processors(stub)
    for machine in ("local", "tower", "box"):
        _ineligible(stub, 1, machine, "fp16")  # fp16 is held
    stub.state.generations[1]["precision_on"]["tower"]["auto-speed"] = {
        "precision": "bf16", "eligible": True,
        "why": "benchmark: bf16 1.23 p/s beat fp16 1.20 p/s", "bench": "done",
        "trials": [{"precision": "bf16", "pages_per_second": 1.23},
                   {"precision": "fp16", "pages_per_second": 1.2},
                   {"precision": "fp32", "pages_per_second": 0.8}],
    }
    page.set_viewport_size({"width": width, "height": 1000})
    open_settings(stub, page)
    card = _card(page, 1)
    heights: dict[tuple[str, str], int] = {}
    texts = set()
    for machine in ["all", "local", "tower", "box", "all"]:
        _machine(page, 1, machine)
        for mode in MODES:
            _mode(page, 1, mode)
            page.wait_for_timeout(60)
            box = card.bounding_box()
            assert box
            heights[(machine, mode)] = round(box["height"])
            texts.add(_line(page, 1))
    assert len(set(heights.values())) == 1, heights
    # The cycle really did show every kind of line.
    assert "" in texts
    assert "runs bf16 here" in texts
    assert "bf16 (benchmark: 1.23 vs 1.2 vs 0.8 p/s)" in texts
    assert "No connected machine can run fp16" in texts
    assert any(t.startswith("not eligible here: ") for t in texts)

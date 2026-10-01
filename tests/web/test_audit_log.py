"""The admin panel's Audit tab: search, filters, reading progress hidden by
default, "Load more" paging, filters kept across a refresh, no layout motion.

Driven against a real server and database, so the queries the page builds
are the ones the API answers.
"""

from __future__ import annotations

import base64
import multiprocessing
import time
from collections.abc import Generator
from typing import TYPE_CHECKING

import pytest

from .test_admin_panel import BROWSERS_AVAILABLE, find_free_port, run_server

if TYPE_CHECKING:
    from playwright.sync_api import Page

pytestmark = pytest.mark.skipif(
    not BROWSERS_AVAILABLE, reason="Playwright browsers not installed"
)

LIBRARY_EVENTS = 60  # more than one page of 50
PROGRESS_EVENTS = 30


@pytest.fixture(scope="module")
def server_url(tmp_path_factory: pytest.TempPathFactory) -> Generator[str, None, None]:
    from mokuro_bunko.database import Database

    storage = tmp_path_factory.mktemp("audit-storage")
    for name in ("library", "inbox", "users"):
        (storage / name).mkdir()
    db = Database(storage / "mokuro.db")
    db.create_user("admin", "adminpass", role="admin")

    def put(action: str, actor: str | None, kind: str, path: str, at: str,
            details: dict[str, object] | None = None) -> None:
        event_id = db.log_audit_event(action=action, actor_username=actor, target_type=kind,
                                      target_path=path, details=details)
        with db._connection() as conn:
            conn.execute("UPDATE audit_logs SET created_at = ? WHERE id = ?", (at, event_id))

    for index in range(LIBRARY_EVENTS):
        put("upload", "alice", "library", f"/mokuro-reader/Series/Vol {index:02}.cbz",
            f"2026-09-10 {index // 60:02}:{index % 60:02}:00")
    for index in range(PROGRESS_EVENTS):
        put("upload", "bob", "progress", "/mokuro-reader/volume-data.json",
            f"2026-09-11 01:{index:02}:00")
    put("ocr_sidecar_written", "tower-acct", "sidecar", "/mokuro-reader/Series/Vol 01.mokuro",
        "2026-09-12 10:00:00", {"machine": "tower", "pages": 180})
    put("ocr_sidecar_rejected", "tower-acct", "sidecar", "/mokuro-reader/Series/Vol 02.mokuro",
        "2026-09-12 11:00:00",
        {"machine": "tower", "reason": "the sidecar it wrote is not readable JSON"})
    put("invite_created", "alice", "invite", "CODE123", "2026-09-01 09:00:00", {"role": "uploader"})

    port = find_free_port()
    ready = multiprocessing.Event()
    process = multiprocessing.Process(target=run_server, args=(str(storage), port, ready),
                                      daemon=True)
    process.start()
    if not ready.wait(timeout=10):
        process.terminate()
        pytest.fail("Server failed to start")
    time.sleep(0.5)
    yield f"http://127.0.0.1:{port}"
    process.terminate()
    process.join(timeout=5)


def open_audit(page: Page, server_url: str, hash_part: str = "") -> Page:
    credentials = base64.b64encode(b"admin:adminpass").decode()
    page.add_init_script(f"sessionStorage.setItem('mokuro_token', '{credentials}');")
    page.route("**/*", lambda route: route.continue_(
        headers={**route.request.headers, "Authorization": f"Basic {credentials}"}
    ))
    page.goto(f"{server_url}/_admin/{hash_part}")
    page.wait_for_load_state("networkidle")
    if not hash_part:
        page.locator(".tab[data-tab='audit']").click()
    wait_count(page)
    return page


def wait_count(page: Page, text: str | None = None) -> None:
    if text is None:
        page.wait_for_function(
            "() => /\\d/.test(document.getElementById('audit-count').textContent)", timeout=5000
        )
    else:
        page.wait_for_function(
            "t => document.getElementById('audit-count').textContent === t", arg=text,
            timeout=5000,
        )


def rows(page: Page) -> list[str]:
    return page.locator("#audit-body tr[data-id]").evaluate_all(
        "rows => rows.map(r => r.dataset.id)"
    )


def actions(page: Page) -> list[str]:
    return page.locator("#audit-body tr[data-id] td:nth-child(3)").all_inner_texts()


NON_PROGRESS = LIBRARY_EVENTS + 3


def test_reading_progress_is_hidden_until_asked_for(server_url: str, page: Page) -> None:
    open_audit(page, server_url)
    wait_count(page, f"{NON_PROGRESS} events · reading-progress sync hidden")
    assert len(rows(page)) == 50
    assert "volume-data.json" not in page.locator("#audit-body").inner_text()
    page.locator("#audit-progress").check()
    wait_count(page, f"{NON_PROGRESS + PROGRESS_EVENTS} events")
    # Newest first: the OCR events, then the progress sync of the 11th.
    assert actions(page)[:2] == ["ocr_sidecar_rejected", "ocr_sidecar_written"]
    assert page.locator("#audit-body tr[data-id]").nth(2).inner_text().count(
        "volume-data.json") == 1


def test_load_more_appends_the_next_page_without_moving_anything(
    server_url: str, page: Page
) -> None:
    open_audit(page, server_url)
    wait_count(page, f"{NON_PROGRESS} events · reading-progress sync hidden")
    first = rows(page)
    widths = page.locator("#audit-table thead th").evaluate_all(
        "ths => ths.map(th => th.getBoundingClientRect().width)"
    )
    place = "t => { const r = t.getBoundingClientRect(); return [r.left, r.top + scrollY, r.width]; }"
    top = page.locator("#audit-table").evaluate(place)
    more = page.locator("#audit-more")
    assert more.get_attribute("aria-hidden") == "false"
    more_left = more.evaluate("b => b.getBoundingClientRect().left")
    more.click()
    page.wait_for_function(
        f"() => document.querySelectorAll('#audit-body tr[data-id]').length === {NON_PROGRESS}",
        timeout=5000,
    )
    everything = rows(page)
    assert everything[:50] == first
    assert len(set(everything)) == len(everything) == NON_PROGRESS
    # The last page: the pager keeps its place, only hidden.
    assert more.get_attribute("aria-hidden") == "true"
    assert more.evaluate("b => getComputedStyle(b).visibility") == "hidden"
    assert page.locator("#audit-table thead th").evaluate_all(
        "ths => ths.map(th => th.getBoundingClientRect().width)"
    ) == widths
    # It grew downwards only: same place on the page, same width.
    assert page.locator("#audit-table").evaluate(place) == top
    assert more.evaluate("b => b.getBoundingClientRect().left") == more_left


def test_actor_action_and_type_filters(server_url: str, page: Page) -> None:
    open_audit(page, server_url)
    page.locator("#audit-actor").select_option("tower-acct")
    wait_count(page, "2 events · reading-progress sync hidden")
    assert actions(page) == ["ocr_sidecar_rejected", "ocr_sidecar_written"]
    page.locator("#audit-actor").select_option("")
    page.locator("#audit-action").select_option("ocr_sidecar_written,ocr_sidecar_rejected")
    wait_count(page, "2 events · reading-progress sync hidden")
    page.locator("#audit-action").select_option("")
    page.locator("#audit-type").select_option("invite")
    wait_count(page, "1 event")
    assert actions(page) == ["invite_created"]
    # The selects list what the log holds.
    options = page.locator("#audit-actor option").all_inner_texts()
    assert options[0] == "All actors"
    assert {"alice", "tower-acct", "bob"} <= set(options[1:])


def test_search_is_debounced_and_reaches_details(server_url: str, page: Page) -> None:
    open_audit(page, server_url)
    requests: list[str] = []
    page.on("request", lambda r: requests.append(r.url) if "/api/audit" in r.url else None)
    page.locator("#audit-q").press_sequentially("readable", delay=20)
    wait_count(page, "1 event · reading-progress sync hidden")
    assert actions(page) == ["ocr_sidecar_rejected"]
    # One request for the whole word, not one a keystroke.
    assert len(requests) == 1, requests


def test_date_range(server_url: str, page: Page) -> None:
    open_audit(page, server_url)
    page.locator("#audit-since").fill("2026-09-12")
    page.locator("#audit-until").fill("2026-09-12")
    wait_count(page, "2 events · reading-progress sync hidden")


def test_a_filter_matching_nothing_says_so(server_url: str, page: Page) -> None:
    open_audit(page, server_url)
    page.locator("#audit-q").fill("nothing like this anywhere")
    wait_count(page, "0 events · reading-progress sync hidden")
    assert page.locator("#audit-body").inner_text().strip() == "No events match these filters"
    assert page.locator("#audit-more").get_attribute("aria-hidden") == "true"


def test_a_refresh_keeps_the_filters(server_url: str, page: Page) -> None:
    open_audit(page, server_url)
    page.locator("#audit-actor").select_option("tower-acct")
    page.locator("#audit-progress").check()
    wait_count(page, "2 events")
    assert page.evaluate("location.hash") == "#audit?actor=tower-acct&progress=1"
    page.reload()
    page.wait_for_load_state("networkidle")
    wait_count(page, "2 events")
    assert page.locator("#audit-tab").is_visible()
    assert page.locator("#audit-actor").input_value() == "tower-acct"
    assert page.locator("#audit-progress").is_checked()
    assert actions(page) == ["ocr_sidecar_rejected", "ocr_sidecar_written"]


def test_a_shared_link_opens_the_filtered_view(server_url: str, page: Page) -> None:
    open_audit(page, server_url, "#audit?q=invite")
    wait_count(page, "1 event · reading-progress sync hidden")
    assert page.locator("#audit-q").input_value() == "invite"


def test_leaving_the_tab_forgets_the_hash(server_url: str, page: Page) -> None:
    open_audit(page, server_url, "#audit?q=invite")
    page.locator(".tab[data-tab='users']").click()
    assert page.evaluate("location.hash") == ""

"""The audit log is searchable, pageable and filterable.

Live evidence: reading-progress sync (per-user progress files,
``target_type='progress'``) was ~6,500 of the events and buried everything
else, so it is left out unless asked for. Filters apply in SQL (never in
Python after a LIMIT), pages are keyset pages by (created_at, id), and a page
stays fast at 100k rows.
"""

from __future__ import annotations

import json
import time
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.database import AuditQueryError, Database


@pytest.fixture
def db(tmp_path: Path) -> Database:
    return Database(tmp_path / "audit.db")


def put(
    db: Database,
    action: str,
    actor: str | None,
    target_type: str | None,
    path: str | None = None,
    details: dict[str, Any] | None = None,
    at: str | None = None,
) -> int:
    event_id = db.log_audit_event(
        action=action, actor_username=actor, target_type=target_type,
        target_path=path, details=details,
    )
    if at is not None:
        with db._connection() as conn:
            conn.execute("UPDATE audit_logs SET created_at = ? WHERE id = ?", (at, event_id))
    return event_id


@pytest.fixture
def seeded(db: Database) -> Database:
    put(db, "upload", "alice", "library", "/mokuro-reader/S/V1.cbz", at="2026-09-01 10:00:00")
    put(db, "delete", "alice", "library", "/mokuro-reader/S/V2.cbz", at="2026-09-02 10:00:00")
    put(db, "upload", "bob", "progress", "/mokuro-reader/volume-data.json",
        at="2026-09-02 11:00:00")
    put(db, "delete", "bob", "progress", "/mokuro-reader/volume-data.json",
        at="2026-09-02 12:00:00")
    put(db, "ocr_sidecar_written", "tower-acct", "sidecar",
        "/mokuro-reader/S/V1.paddle.mokuro", {"machine": "tower", "engine": "paddle-manga"},
        at="2026-09-03 10:00:00")
    put(db, "ocr_sidecar_rejected", "tower-acct", "sidecar",
        "/mokuro-reader/S/V2.paddle.mokuro",
        {"machine": "tower", "reason": "the sidecar it wrote is not readable JSON"},
        at="2026-09-04 10:00:00")
    put(db, "ocr_sidecar_written", None, "sidecar", "/mokuro-reader/S/V3.mokuro",
        {"machine": "local", "note": "漢字"}, at="2026-09-05 10:00:00")
    put(db, "invite_created", "alice", "invite", "CODE123", {"role": "uploader"},
        at="2026-09-06 10:00:00")
    return db


def actions(page: dict[str, Any]) -> list[str]:
    return [e["action"] for e in page["events"]]


class TestFilters:
    def test_the_default_leaves_reading_progress_out(self, seeded: Database) -> None:
        page = seeded.query_audit_events()
        assert all(e["target_type"] != "progress" for e in page["events"])
        assert len(page["events"]) == 6
        # Newest first.
        assert actions(page)[0] == "invite_created"

    def test_the_flag_brings_progress_back(self, seeded: Database) -> None:
        page = seeded.query_audit_events(include_progress=True)
        assert len(page["events"]) == 8

    def test_asking_for_the_progress_type_is_asking_for_it(self, seeded: Database) -> None:
        page = seeded.query_audit_events(target_types=["progress"])
        assert [e["target_type"] for e in page["events"]] == ["progress", "progress"]

    def test_actor(self, seeded: Database) -> None:
        page = seeded.query_audit_events(actor="tower-acct")
        assert actions(page) == ["ocr_sidecar_rejected", "ocr_sidecar_written"]

    def test_one_action_and_several(self, seeded: Database) -> None:
        assert actions(seeded.query_audit_events(actions=["upload"])) == ["upload"]
        page = seeded.query_audit_events(
            actions=["ocr_sidecar_written", "ocr_sidecar_rejected"]
        )
        assert sorted(actions(page)) == [
            "ocr_sidecar_rejected", "ocr_sidecar_written", "ocr_sidecar_written"
        ]

    def test_target_types(self, seeded: Database) -> None:
        page = seeded.query_audit_events(target_types=["invite", "library"])
        assert sorted(actions(page)) == ["delete", "invite_created", "upload"]

    def test_date_range_since_inclusive_until_exclusive(self, seeded: Database) -> None:
        page = seeded.query_audit_events(since="2026-09-02", until="2026-09-04")
        assert actions(page) == ["ocr_sidecar_written", "delete"]
        # ISO with a zone (what a browser's toISOString sends) is UTC.
        page = seeded.query_audit_events(
            since="2026-09-03T10:00:00.000Z", until="2026-09-03T10:00:01Z"
        )
        assert actions(page) == ["ocr_sidecar_written"]

    def test_a_bad_date_or_cursor_is_refused(self, seeded: Database) -> None:
        with pytest.raises(AuditQueryError):
            seeded.query_audit_events(since="yesterday")
        with pytest.raises(AuditQueryError):
            seeded.query_audit_events(cursor="not-a-cursor")

    def test_search_hits_details_case_insensitively(self, seeded: Database) -> None:
        page = seeded.query_audit_events(search="NOT READABLE")
        assert actions(page) == ["ocr_sidecar_rejected"]

    def test_search_hits_actor_action_and_path(self, seeded: Database) -> None:
        assert actions(seeded.query_audit_events(search="TOWER-ACCT")) == [
            "ocr_sidecar_rejected", "ocr_sidecar_written"
        ]
        assert actions(seeded.query_audit_events(search="invite_cr")) == ["invite_created"]
        assert actions(seeded.query_audit_events(search="S/V2.cbz")) == ["delete"]

    def test_search_finds_non_ascii_text_stored_escaped(self, seeded: Database) -> None:
        """Details are stored as ASCII JSON: 漢字 is on disk as \\u6f22\\u5b57."""
        assert actions(seeded.query_audit_events(search="漢字")) == ["ocr_sidecar_written"]

    def test_search_wildcards_are_literal(self, seeded: Database) -> None:
        assert seeded.query_audit_events(search="%")["events"] == []
        assert seeded.query_audit_events(search="_")["events"] != []  # in action names

    def test_everything_combined(self, seeded: Database) -> None:
        page = seeded.query_audit_events(
            actor="tower-acct", actions=["ocr_sidecar_written", "ocr_sidecar_rejected"],
            target_types=["sidecar"], since="2026-09-01", until="2026-09-10", search="json",
        )
        assert actions(page) == ["ocr_sidecar_rejected"]
        assert page["total"] == 1


class TestPaging:
    def test_pages_have_no_duplicates_or_gaps_under_inserts(self, db: Database) -> None:
        # Many events inside one second: the id breaks the created_at tie.
        for index in range(23):
            put(db, "upload", "alice", "library", f"/p/{index}", at="2026-09-01 10:00:00")
        seen: list[int] = []
        page = db.query_audit_events(limit=5)
        pages = 0
        while True:
            seen += [e["id"] for e in page["events"]]
            pages += 1
            # New events land between page loads, at the same second and later.
            put(db, "upload", "alice", "library", "/new", at="2026-09-01 10:00:00")
            put(db, "upload", "alice", "library", "/newer")
            if page["next_cursor"] is None:
                break
            page = db.query_audit_events(limit=5, cursor=page["next_cursor"])
        assert pages == 5
        assert len(seen) == len(set(seen)) == 23
        assert seen == sorted(seen, reverse=True)

    def test_page_size_default_and_bounds(self, db: Database) -> None:
        for index in range(260):
            put(db, "upload", "alice", "library", f"/p/{index}")
        assert len(db.query_audit_events()["events"]) == 50
        assert len(db.query_audit_events(limit=1000)["events"]) == 200
        assert len(db.query_audit_events(limit=0)["events"]) == 1

    def test_the_last_page_has_no_cursor(self, seeded: Database) -> None:
        assert seeded.query_audit_events(limit=6)["next_cursor"] is None
        assert seeded.query_audit_events(limit=5)["next_cursor"] is not None


class TestFacets:
    def test_distinct_actors_actions_and_target_types(self, seeded: Database) -> None:
        facets = seeded.audit_facets()
        assert facets == {
            "actors": ["alice", "bob", "tower-acct"],
            "actions": ["delete", "invite_created", "ocr_sidecar_rejected",
                        "ocr_sidecar_written", "upload"],
            "target_types": ["invite", "library", "progress", "sidecar"],
        }


class TestSchema:
    def test_the_indexes_it_needs_exist(self, db: Database) -> None:
        with db._connection() as conn:
            names = {row[1] for row in conn.execute("PRAGMA index_list(audit_logs)")}
        assert {"idx_audit_type_created", "idx_audit_action_created",
                "idx_audit_actor_created"} <= names

    def test_an_existing_database_gains_them(self, tmp_path: Path) -> None:
        path = tmp_path / "old.db"
        old = Database(path)
        put(old, "upload", "alice", "library", "/x")
        with old._connection() as conn:
            for name in ("idx_audit_type_created", "idx_audit_action_created",
                         "idx_audit_actor_created"):
                conn.execute(f"DROP INDEX {name}")
        old._conn.close()
        reopened = Database(path)
        with reopened._connection() as conn:
            names = {row[1] for row in conn.execute("PRAGMA index_list(audit_logs)")}
        assert "idx_audit_type_created" in names
        assert reopened.query_audit_events()["total"] == 1


def seed_big(db: Database, count: int = 100_000) -> Database:
    """``count`` events shaped like the live log: mostly reading progress."""
    rows = []
    for index in range(count):
        kind = index % 20
        if kind < 15:
            row = (f"reader{index % 40}", ("upload", "delete", "edit")[index % 3],
                   "progress", "/mokuro-reader/volume-data.json", None)
        elif kind < 19:
            row = (f"uploader{index % 7}", "upload", "library",
                   f"/mokuro-reader/Series {index % 300}/Vol {index}.cbz",
                   json.dumps({"existed_before": False}))
        else:
            row = ("tower-acct", "ocr_sidecar_written", "sidecar",
                   f"/mokuro-reader/Series {index % 300}/Vol {index}.mokuro",
                   json.dumps({"machine": "tower", "pages": 200}))
        rows.append((*row, "2026-08-01 00:00:00", index * 20))  # ~23 days
    with db._connection() as conn:
        conn.executemany(
            "INSERT INTO audit_logs (actor_username, action, target_type, target_path,"
            " details, created_at) VALUES (?, ?, ?, ?, ?, datetime(?, '+' || ? || ' seconds'))",
            rows,
        )
        conn.execute("ANALYZE")
    return db


@pytest.fixture(scope="module")
def big(tmp_path_factory: pytest.TempPathFactory) -> Database:
    return seed_big(Database(tmp_path_factory.mktemp("big") / "audit.db"))


class TestPerformance:
    """A page stays well under the agreed bound at 100k rows."""

    BOUND_SECONDS = 0.050

    def timed(self, fn: Any) -> tuple[float, Any]:
        best = float("inf")
        result = None
        for _ in range(3):
            start = time.perf_counter()
            result = fn()
            best = min(best, time.perf_counter() - start)
        return best, result

    @pytest.mark.parametrize(
        "query",
        [
            {},
            {"include_progress": True},
            {"actor": "tower-acct"},
            {"actions": ["ocr_sidecar_written", "ocr_sidecar_rejected"]},
            {"target_types": ["library"]},
            {"since": "2026-08-10", "until": "2026-08-12"},
            {"search": "Series 17/"},
            {"search": "no such text anywhere"},
            {"actor": "nobody-at-all"},
            {"actor": "tower-acct", "search": "Vol 99", "since": "2026-08-05"},
        ],
    )
    def test_first_and_deep_pages(self, big: Database, query: dict[str, Any]) -> None:
        seconds, first = self.timed(lambda: big.query_audit_events(**query))
        assert seconds < self.BOUND_SECONDS, (query, seconds)
        cursor = first["next_cursor"]
        for _ in range(5):
            if cursor is None:
                break
            page = big.query_audit_events(**query, cursor=cursor)
            cursor = page["next_cursor"]
        if cursor is not None:
            seconds, _ = self.timed(lambda: big.query_audit_events(**query, cursor=cursor))
            assert seconds < self.BOUND_SECONDS, (query, "deep", seconds)

    def test_facets(self, big: Database) -> None:
        seconds, facets = self.timed(big.audit_facets)
        assert seconds < self.BOUND_SECONDS, seconds
        assert "tower-acct" in facets["actors"]

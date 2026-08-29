"""Community enrichment: AniList/MAL normalization and the fetch loop."""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.catalog.community import (
    CommunityFetcher,
    normalize_anilist,
    normalize_jikan,
)
from mokuro_bunko.database import Database, SeriesFactsRow


def facts_row(series_key: str, external_ids: dict[str, int]) -> SeriesFactsRow:
    return {
        "series_key": series_key,
        "series_title": series_key.title(),
        "external_ids": external_ids,
        "titles": {},
        "synonyms": [],
        "tag": None,
        "unit": None,
        "facts_updated_at": "2026-08-18T19:36:24.324Z",
        "spine_offset": None,
        "volume_offsets": {},
        "updated_by": None,
        "updated_at": "",
    }


class TestNormalizeAnilist:
    MEDIA = {
        "id": 30013,
        "meanScore": 92,
        "genres": ["Adventure", "Fantasy"],
        "tags": [
            {"name": "Pirates", "rank": 95},
            {"name": "Ensemble Cast", "rank": 61},
            {"name": "Barely There", "rank": 20},
        ],
    }

    def test_takes_mean_score_and_genres_verbatim(self) -> None:
        score, tags, genres = normalize_anilist(self.MEDIA)
        assert score == 92.0
        assert genres == ["Adventure", "Fantasy"]

    def test_keeps_only_tags_at_or_above_the_relevance_floor(self) -> None:
        _score, tags, _genres = normalize_anilist(self.MEDIA)
        assert tags == ["Pirates", "Ensemble Cast"]

    def test_missing_score_is_none_not_zero(self) -> None:
        score, _tags, _genres = normalize_anilist({"id": 1, "genres": [], "tags": []})
        assert score is None


class TestNormalizeJikan:
    DATA = {
        "score": 8.21,
        "genres": [{"name": "Action"}],
        "themes": [{"name": "Pirates"}],
        "demographics": [{"name": "Shounen"}],
    }

    def test_score_normalizes_to_0_100(self) -> None:
        score, _tags, genres = normalize_jikan(self.DATA)
        assert score == pytest.approx(82.1)
        assert genres == ["Action", "Pirates", "Shounen"]

    def test_missing_score_is_none(self) -> None:
        score, _tags, _genres = normalize_jikan({"genres": []})
        assert score is None


class TestFetcher:
    def _fetcher(self, tmp_path: Path, http: Any) -> tuple[Database, CommunityFetcher]:
        db = Database(tmp_path / "test.db")
        fetcher = CommunityFetcher(db, http=http, request_gap_seconds=0.0)
        return db, fetcher

    def test_fetches_anilist_linked_series_in_one_batch(self, tmp_path: Path) -> None:
        requests: list[tuple[str, Any]] = []

        def http(url: str, json_body: Any = None) -> dict[str, Any]:
            requests.append((url, json_body))
            return {
                "data": {
                    "Page": {
                        "media": [
                            {"id": 111, "meanScore": 80, "genres": ["Drama"], "tags": []},
                            {"id": 222, "meanScore": 70, "genres": [], "tags": []},
                        ]
                    }
                }
            }

        db, fetcher = self._fetcher(tmp_path, http)
        db.put_series_facts(facts_row("alpha", {"anilist": 111}))
        db.put_series_facts(facts_row("beta", {"anilist": 222, "mal": 5}))

        assert fetcher.run_once() == 2
        assert len(requests) == 1  # one GraphQL batch, not one request per series
        rows = {r["series_key"]: r for r in db.list_community_details()}
        assert rows["alpha"]["score"] == 80.0
        assert rows["alpha"]["source"] == "anilist"
        assert rows["beta"]["score"] == 70.0

    def test_mal_only_series_go_through_jikan(self, tmp_path: Path) -> None:
        def http(url: str, json_body: Any = None) -> dict[str, Any]:
            assert "jikan" in url and json_body is None
            return {"data": {"score": 9.0, "genres": [{"name": "Action"}]}}

        db, fetcher = self._fetcher(tmp_path, http)
        db.put_series_facts(facts_row("gamma", {"mal": 42}))

        assert fetcher.run_once() == 1
        row = db.list_community_details()[0]
        assert row["series_key"] == "gamma"
        assert row["score"] == 90.0
        assert row["source"] == "mal"

    def test_fresh_rows_are_not_refetched_and_stale_ones_are(self, tmp_path: Path) -> None:
        calls: list[Any] = []

        def http(url: str, json_body: Any = None) -> dict[str, Any]:
            calls.append(url)
            return {
                "data": {"Page": {"media": [{"id": 111, "meanScore": 80, "genres": [], "tags": []}]}}
            }

        db, fetcher = self._fetcher(tmp_path, http)
        db.put_series_facts(facts_row("alpha", {"anilist": 111}))

        assert fetcher.run_once() == 1
        assert fetcher.run_once() == 0  # just fetched: fresh, skipped

        stale = db.list_community_details()[0]
        db.upsert_community_details({**stale, "fetched_at": "2020-01-01T00:00:00Z"})
        assert fetcher.run_once() == 1  # stale again: refetched

    def test_a_network_failure_stores_nothing_and_does_not_raise(self, tmp_path: Path) -> None:
        def http(url: str, json_body: Any = None) -> dict[str, Any]:
            raise OSError("network down")

        db, fetcher = self._fetcher(tmp_path, http)
        db.put_series_facts(facts_row("alpha", {"anilist": 111}))

        assert fetcher.run_once() == 0
        assert db.list_community_details() == []

    def test_unlinked_series_are_ignored(self, tmp_path: Path) -> None:
        def http(url: str, json_body: Any = None) -> dict[str, Any]:
            raise AssertionError("no request should be made")

        db, fetcher = self._fetcher(tmp_path, http)
        db.put_series_facts(facts_row("alpha", {}))
        assert fetcher.run_once() == 0


class TestRequestIdentity:
    def test_requests_carry_a_product_user_agent(self) -> None:
        # Cloudflare in front of AniList rejects urllib's default UA with 403.
        from mokuro_bunko.catalog.community import _build_request

        get = _build_request("https://api.jikan.moe/v4/manga/1")
        post = _build_request("https://graphql.anilist.co", json_body={"query": "q"})
        for request in (get, post):
            agent = request.get_header("User-agent", "")
            assert agent.startswith("mokuro-bunko/")


class TestNudge:
    def test_targeted_run_fetches_only_named_keys_and_ignores_freshness(
        self, tmp_path: Path
    ) -> None:
        calls: list[Any] = []

        def http(url: str, json_body: Any = None) -> dict[str, Any]:
            ids = json_body["variables"]["ids"]
            calls.append(ids)
            return {
                "data": {
                    "Page": {
                        "media": [
                            {"id": i, "meanScore": 80, "genres": [], "tags": []} for i in ids
                        ]
                    }
                }
            }

        db = Database(tmp_path / "test.db")
        fetcher = CommunityFetcher(db, http=http, request_gap_seconds=0.0)
        db.put_series_facts(facts_row("alpha", {"anilist": 111}))
        db.put_series_facts(facts_row("beta", {"anilist": 222}))
        # alpha already has a FRESH row: a plain cycle would skip it, but a
        # nudge means its id may have just changed — force the refetch.
        assert fetcher.run_once() == 2
        calls.clear()

        assert fetcher.run_once(only={"alpha"}, force=True) == 1
        assert calls == [[111]]

    def test_request_fetch_wakes_the_running_loop_promptly(self, tmp_path: Path) -> None:
        import time as time_mod

        def http(url: str, json_body: Any = None) -> dict[str, Any]:
            return {
                "data": {"Page": {"media": [{"id": 111, "meanScore": 80, "genres": [], "tags": []}]}}
            }

        db = Database(tmp_path / "test.db")
        db.put_series_facts(facts_row("alpha", {"anilist": 111}))
        fetcher = CommunityFetcher(db, http=http, poll_seconds=3600.0, request_gap_seconds=0.0)
        fetcher.start()
        try:
            # The loop's first FULL cycle waits ~60s; a nudge must not.
            fetcher.request_fetch("alpha")
            deadline = time_mod.monotonic() + 5.0
            while not db.list_community_details() and time_mod.monotonic() < deadline:
                time_mod.sleep(0.05)
            rows = db.list_community_details()
            assert rows and rows[0]["series_key"] == "alpha"
        finally:
            fetcher.stop()

    def test_stop_returns_promptly_while_the_loop_is_idle(self, tmp_path: Path) -> None:
        import time as time_mod

        fetcher = CommunityFetcher(
            Database(tmp_path / "test.db"), http=lambda *a, **k: {}, poll_seconds=3600.0
        )
        fetcher.start()
        started = time_mod.monotonic()
        fetcher.stop()
        assert time_mod.monotonic() - started < 5.0

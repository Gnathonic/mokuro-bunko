"""Community enrichment: ratings, tags and genres from AniList / MAL.

A background fetcher fills `community_details` for every series whose client
facts carry an external id. AniList is primary (batched GraphQL, richer
tags); MAL — via the public Jikan API — covers series linked only there.
Scores normalize to one 0–100 scale (AniList `meanScore` is already that;
Jikan's 0–10 is multiplied by ten) so the catalog can sort across sources.

Everything here fails quietly: the catalog renders without community data,
and the next cycle retries whatever a network hiccup skipped.
"""

from __future__ import annotations

import json
import sys
import threading
import time
import urllib.request
from collections.abc import Callable
from datetime import datetime, timedelta, timezone
from typing import Any, cast

from mokuro_bunko.database import CommunityDetailsRow, Database

ANILIST_GRAPHQL_URL = "https://graphql.anilist.co"
JIKAN_MANGA_URL = "https://api.jikan.moe/v4/manga/{id}"

#: AniList tags below this relevance rank are noise for filtering.
TAG_RANK_FLOOR = 40
#: At most this many tags per series keep the payload and the UI sane.
TAG_LIMIT = 10
#: Ids per GraphQL batch (AniList page maximum is 50).
ANILIST_BATCH_SIZE = 50
#: Refetch details older than this.
REFRESH_AGE = timedelta(days=7)
#: Pause between HTTP requests — well inside AniList's and Jikan's limits.
DEFAULT_REQUEST_GAP_SECONDS = 2.0
#: How often the background loop looks for missing/stale rows.
DEFAULT_POLL_SECONDS = 3600.0

_ANILIST_QUERY = """
query ($ids: [Int]) {
  Page(page: 1, perPage: 50) {
    media(id_in: $ids, type: MANGA) {
      id
      meanScore
      genres
      tags { name rank }
    }
  }
}
"""

Normalized = tuple[float | None, list[str], list[str]]  # score, tags, genres


def _log(message: str) -> None:
    print(f"[COMMUNITY] {message}", file=sys.stderr, flush=True)


def _build_request(url: str, json_body: Any = None) -> urllib.request.Request:
    """The one place request headers are set.

    AniList sits behind Cloudflare, which answers urllib's default
    `Python-urllib/…` User-Agent with 403 — a real product identity is
    required (verified live: default UA 403, this UA 200).
    """
    data = None
    headers = {
        "Accept": "application/json",
        "User-Agent": f"mokuro-bunko/{_version()}",
    }
    if json_body is not None:
        data = json.dumps(json_body).encode("utf-8")
        headers["Content-Type"] = "application/json"
    return urllib.request.Request(url, data=data, headers=headers)


def _version() -> str:
    try:
        import importlib.metadata as metadata

        return metadata.version("mokuro-bunko")
    except Exception:  # noqa: BLE001 - identity only, any fallback works
        return "dev"


def _http_json(url: str, json_body: Any = None) -> dict[str, Any]:
    """POST *json_body* (or GET when None) and parse the JSON response."""
    request = _build_request(url, json_body)
    with urllib.request.urlopen(request, timeout=30) as response:
        return cast("dict[str, Any]", json.loads(response.read().decode("utf-8")))


def normalize_anilist(media: dict[str, Any]) -> Normalized:
    raw_score = media.get("meanScore")
    score = float(raw_score) if isinstance(raw_score, (int, float)) else None
    genres = [g for g in media.get("genres") or [] if isinstance(g, str) and g]
    tags: list[str] = []
    for tag in media.get("tags") or []:
        if not isinstance(tag, dict):
            continue
        name = tag.get("name")
        rank = tag.get("rank")
        if not isinstance(name, str) or not name:
            continue
        if isinstance(rank, (int, float)) and rank >= TAG_RANK_FLOOR:
            tags.append(name)
    return score, tags[:TAG_LIMIT], genres


def normalize_jikan(data: dict[str, Any]) -> Normalized:
    raw_score = data.get("score")
    score = round(float(raw_score) * 10, 1) if isinstance(raw_score, (int, float)) else None
    genres: list[str] = []
    for group in ("genres", "themes", "demographics"):
        for entry in data.get(group) or []:
            name = entry.get("name") if isinstance(entry, dict) else None
            if isinstance(name, str) and name:
                genres.append(name)
    return score, [], genres


def _now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _is_fresh(fetched_at: str, now: datetime) -> bool:
    try:
        stamp = datetime.fromisoformat(fetched_at.replace("Z", "+00:00"))
    except ValueError:
        return False
    if stamp.tzinfo is None:
        stamp = stamp.replace(tzinfo=timezone.utc)
    return now - stamp < REFRESH_AGE


class CommunityFetcher:
    """Background loop keeping `community_details` filled and fresh."""

    def __init__(
        self,
        database: Database,
        *,
        http: Callable[..., dict[str, Any]] = _http_json,
        poll_seconds: float = DEFAULT_POLL_SECONDS,
        request_gap_seconds: float = DEFAULT_REQUEST_GAP_SECONDS,
    ) -> None:
        self.database = database
        self._http = http
        self._poll_seconds = poll_seconds
        self._request_gap = request_gap_seconds
        self._stop_event = threading.Event()
        self._wake_event = threading.Event()
        self._nudge_lock = threading.Lock()
        self._nudged: set[str] = set()
        self._thread: threading.Thread | None = None

    # --- candidates ------------------------------------------------------

    def request_fetch(self, series_key: str) -> None:
        """Nudge: fetch this series' details soon, freshness be damned.

        Called when a client's metadata PUT introduces or changes an external
        id — the existing row (if any) may describe the OLD link, so the
        7-day freshness window must not defer the correction. Thread-safe;
        wakes the loop immediately, including during its start-up delay.
        """
        with self._nudge_lock:
            self._nudged.add(series_key)
        self._wake_event.set()

    def _candidates(
        self, only: set[str] | None = None, force: bool = False
    ) -> tuple[dict[str, int], dict[str, int]]:
        """(anilist-linked, mal-only) series needing a fetch, key → external id."""
        now = datetime.now(timezone.utc)
        fresh = (
            set()
            if force
            else {
                row["series_key"]
                for row in self.database.list_community_details()
                if _is_fresh(row["fetched_at"], now)
            }
        )
        anilist: dict[str, int] = {}
        mal_only: dict[str, int] = {}
        for facts in self.database.list_series_facts():
            key = facts["series_key"]
            if key in fresh or (only is not None and key not in only):
                continue
            ids = facts["external_ids"] or {}
            anilist_id = ids.get("anilist")
            mal_id = ids.get("mal")
            if isinstance(anilist_id, int):
                anilist[key] = anilist_id
            elif isinstance(mal_id, int):
                mal_only[key] = mal_id
        return anilist, mal_only

    # --- one cycle -------------------------------------------------------

    def run_once(self, only: set[str] | None = None, force: bool = False) -> int:
        """Fetch and store details for every stale/missing linked series.

        `only` restricts the cycle to those series keys; `force` ignores the
        freshness window (both together = a nudge's targeted mini-cycle).
        Returns the number of series updated. Network errors skip the batch
        or series and leave it for the next cycle.
        """
        anilist, mal_only = self._candidates(only=only, force=force)
        updated = 0

        keys_by_id = {media_id: key for key, media_id in anilist.items()}
        ids = sorted(keys_by_id)
        for start in range(0, len(ids), ANILIST_BATCH_SIZE):
            batch = ids[start : start + ANILIST_BATCH_SIZE]
            try:
                payload = self._http(
                    ANILIST_GRAPHQL_URL,
                    json_body={"query": _ANILIST_QUERY, "variables": {"ids": batch}},
                )
                media_list = payload["data"]["Page"]["media"]
            except Exception as error:  # noqa: BLE001 - retried next cycle
                _log(f"AniList batch failed ({len(batch)} ids): {error}")
                continue
            for media in media_list:
                media_id = media.get("id")
                key = keys_by_id.get(media_id)
                if key is None:
                    continue
                score, tags, genres = normalize_anilist(media)
                self._store(key, score, tags, genres, "anilist")
                updated += 1
            self._pause()

        for key, mal_id in sorted(mal_only.items()):
            try:
                payload = self._http(JIKAN_MANGA_URL.format(id=mal_id))
                data = payload["data"]
            except Exception as error:  # noqa: BLE001 - retried next cycle
                _log(f"Jikan fetch failed (mal {mal_id}): {error}")
                continue
            score, tags, genres = normalize_jikan(data)
            self._store(key, score, tags, genres, "mal")
            updated += 1
            self._pause()

        if updated:
            _log(f"updated community details for {updated} series")
        return updated

    def _store(
        self,
        series_key: str,
        score: float | None,
        tags: list[str],
        genres: list[str],
        source: str,
    ) -> None:
        self.database.upsert_community_details(
            CommunityDetailsRow(
                series_key=series_key,
                score=score,
                tags=tags,
                genres=genres,
                source=source,
                fetched_at=_now_iso(),
            )
        )

    def _pause(self) -> None:
        if self._request_gap > 0:
            self._stop_event.wait(self._request_gap)

    # --- lifecycle -------------------------------------------------------

    def start(self) -> None:
        if self._thread is not None:
            return
        self._thread = threading.Thread(
            target=self._loop, name="community-fetcher", daemon=True
        )
        self._thread.start()

    def _loop(self) -> None:
        # The first FULL cycle waits for startup to settle (the initial
        # metadata pass needs a chance to land client facts), but a nudge is
        # served the moment it arrives — including during that delay.
        next_full = time.monotonic() + 60.0
        while True:
            timeout = max(0.0, next_full - time.monotonic())
            woke = self._wake_event.wait(timeout=timeout)
            if self._stop_event.is_set():
                return
            if woke:
                self._wake_event.clear()
                with self._nudge_lock:
                    keys, self._nudged = self._nudged, set()
                if keys:
                    try:
                        self.run_once(only=keys, force=True)
                    except Exception as error:  # noqa: BLE001 - the loop must survive
                        _log(f"nudge cycle failed: {error}")
                continue
            try:
                self.run_once()
            except Exception as error:  # noqa: BLE001 - the loop must survive
                _log(f"cycle failed: {error}")
            next_full = time.monotonic() + self._poll_seconds

    def stop(self) -> None:
        self._stop_event.set()
        self._wake_event.set()
        if self._thread is not None:
            self._thread.join(timeout=5.0)
            self._thread = None

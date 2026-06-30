"""Tests for HomePageAPI /api/stats (real counts) and /api/health."""

from __future__ import annotations

import io
import json
from types import SimpleNamespace
from typing import Any, Callable

import pytest

from mokuro_bunko.database import Database
from mokuro_bunko.home.api import HomePageAPI


def _passthrough(environ: dict[str, Any], start_response: Callable[..., Any]) -> list[bytes]:
    start_response("404 Not Found", [("Content-Type", "text/plain")])
    return [b""]


def _fake_index(total_volumes: int) -> Any:
    series = SimpleNamespace(volumes=tuple(range(total_volumes)))
    return SimpleNamespace(get_snapshot=lambda: SimpleNamespace(series=(series,)))


def _get_json(app: Callable[..., Any], path: str) -> tuple[str, dict[str, Any]]:
    captured: dict[str, Any] = {}

    def start_response(
        status: str, headers: list[tuple[str, str]], exc_info: Any = None
    ) -> Callable[[bytes], None]:
        captured["status"] = status
        return lambda data: None

    environ = {"REQUEST_METHOD": "GET", "PATH_INFO": path, "wsgi.input": io.BytesIO(b"")}
    body = b"".join(app(environ, start_response))
    return captured["status"], json.loads(body or b"{}")


@pytest.fixture
def home_app(db_with_users: Database) -> HomePageAPI:
    return HomePageAPI(
        _passthrough,
        database=db_with_users,
        library_index=_fake_index(3),
    )


def test_stats_returns_real_counts(home_app: HomePageAPI) -> None:
    status, data = _get_json(home_app, "/api/stats")
    assert status.startswith("200")
    assert data["total_users"] == 5  # db_with_users seeds 5 non-deleted users
    assert data["total_volumes"] == 3
    assert data["last_updated"] > 0


def test_health_reports_ok_with_counts(home_app: HomePageAPI) -> None:
    status, data = _get_json(home_app, "/api/health")
    assert status.startswith("200")
    assert data["status"] == "ok"
    assert data["db_status"] == "ok"
    assert data["library_status"] == "ok"
    assert data["total_users"] == 5
    assert data["total_volumes"] == 3
    assert "uptime_seconds" in data


def test_health_degraded_when_library_unavailable(db_with_users: Database) -> None:
    broken = SimpleNamespace(get_snapshot=lambda: (_ for _ in ()).throw(RuntimeError("boom")))
    app = HomePageAPI(_passthrough, database=db_with_users, library_index=broken)
    status, data = _get_json(app, "/api/health")
    assert status.startswith("200")
    assert data["status"] == "degraded"
    assert data["library_status"] == "error"
    assert data["db_status"] == "ok"

"""StaticMiddleware routes: robots.txt."""

from __future__ import annotations

from typing import Any

from mokuro_bunko.static import StaticMiddleware


def _call(path: str, method: str = "GET") -> tuple[str, list[tuple[str, str]], bytes]:
    captured: dict[str, Any] = {}

    def downstream(environ: dict[str, Any], start_response: Any) -> list[bytes]:
        start_response("404 Not Found", [("Content-Type", "text/plain")])
        return [b"downstream"]

    def start_response(status: str, headers: list[tuple[str, str]]) -> None:
        captured["status"] = status
        captured["headers"] = headers

    app = StaticMiddleware(downstream)
    body = b"".join(app({"REQUEST_METHOD": method, "PATH_INFO": path}, start_response))
    return captured["status"], captured["headers"], body


def test_robots_txt_disallows_everything() -> None:
    status, headers, body = _call("/robots.txt")
    assert status == "200 OK"
    assert ("Content-Type", "text/plain; charset=utf-8") in headers
    text = body.decode("utf-8")
    assert "User-agent: *" in text
    assert "Disallow: /" in text


def test_other_paths_still_fall_through() -> None:
    status, _headers, body = _call("/something-else")
    assert status == "404 Not Found"
    assert body == b"downstream"

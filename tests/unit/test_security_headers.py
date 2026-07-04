"""Tests for SecurityHeadersMiddleware."""

from __future__ import annotations

from typing import Any, Callable

from mokuro_bunko.middleware.security_headers import SecurityHeadersMiddleware


def _capture_headers(
    app: Callable[..., Any],
    environ: dict[str, Any] | None = None,
) -> list[tuple[str, str]]:
    """Drive a WSGI app and return the response headers it emitted."""
    captured: dict[str, Any] = {}

    def start_response(
        status: str, headers: list[tuple[str, str]], exc_info: Any = None
    ) -> Callable[[bytes], None]:
        captured["headers"] = headers
        return lambda data: None

    env = {"REQUEST_METHOD": "GET", "PATH_INFO": "/x"}
    if environ:
        env.update(environ)
    body = app(env, start_response)
    for _ in body:
        pass
    if hasattr(body, "close"):
        body.close()
    return captured["headers"]


def _fake_app(content_type: str, extra: list[tuple[str, str]] | None = None) -> Callable[..., Any]:
    def app(environ: dict[str, Any], start_response: Callable[..., Any]) -> list[bytes]:
        headers = [("Content-Type", content_type)] + (extra or [])
        start_response("200 OK", headers)
        return [b"data"]

    return app


def _values(headers: list[tuple[str, str]], name: str) -> list[str]:
    return [v for k, v in headers if k.lower() == name.lower()]


def test_safe_headers_added_to_every_response() -> None:
    headers = _capture_headers(SecurityHeadersMiddleware(_fake_app("application/octet-stream")))
    assert _values(headers, "X-Content-Type-Options") == ["nosniff"]
    assert _values(headers, "X-Frame-Options") == ["DENY"]
    assert _values(headers, "Referrer-Policy") == ["no-referrer"]
    assert _values(headers, "X-XSS-Protection") == ["1; mode=block"]


def test_no_store_added_only_to_json_responses() -> None:
    json_headers = _capture_headers(SecurityHeadersMiddleware(_fake_app("application/json")))
    assert any("no-store" in v for v in _values(json_headers, "Cache-Control"))


def test_downloads_stay_cacheable_no_store_not_forced() -> None:
    # Library file downloads must not be made uncacheable by the middleware.
    dl_headers = _capture_headers(
        SecurityHeadersMiddleware(_fake_app("application/vnd.comicbook+zip"))
    )
    assert _values(dl_headers, "Cache-Control") == []


def test_does_not_clobber_header_a_handler_already_set() -> None:
    headers = _capture_headers(
        SecurityHeadersMiddleware(
            _fake_app("text/html", extra=[("X-Frame-Options", "SAMEORIGIN")])
        )
    )
    assert _values(headers, "X-Frame-Options") == ["SAMEORIGIN"]

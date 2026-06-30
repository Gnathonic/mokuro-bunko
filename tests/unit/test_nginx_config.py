"""Regression tests for the in-container nginx config (X-Accel download offload).

These pin two properties that a CORS/503 incident exposed (PR #5 follow-up):

  1. The internal nginx must NOT throttle requests per-IP. A `limit_req`/
     `limit_conn` here rejected the reader's normal burst of concurrent
     thumbnail GETs with a 503 that nginx generates *itself* -- before the
     request reaches Python -- so it carried no `Access-Control-Allow-Origin`
     and the browser saw an opaque CORS block. Abuse control belongs to the
     front proxy; X-Accel already keeps downloads off Python's thread pool.

  2. Errors nginx generates itself (backend down/timeout) must still carry CORS,
     so a real failure surfaces to the reader as its status instead of an opaque
     cross-origin block. Successful downloads inherit CORS from Python across the
     X-Accel-Redirect, so CORS must NOT be added unconditionally (that would
     duplicate the header and the browser would reject the response).
"""

from __future__ import annotations

import re
from pathlib import Path

_TEMPLATE = (
    Path(__file__).parents[2] / "deploy" / "nginx-internal.conf.template"
)


def _directive_lines() -> list[str]:
    """Config lines with comments stripped, so we match directives not prose."""
    lines = []
    for raw in _TEMPLATE.read_text().splitlines():
        line = raw.split("#", 1)[0].strip()
        if line:
            lines.append(line)
    return lines


def _location_body(text: str, marker: str) -> str:
    """The brace-balanced body of the block introduced by `marker` (e.g.
    "location / {"). Lets a test scope assertions to one location."""
    # `marker` may or may not include the "{"; find the opening brace at/after it.
    open_idx = text.index("{", text.index(marker))
    depth = 0
    for i in range(open_idx, len(text)):
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
            if depth == 0:
                return text[open_idx + 1 : i]
    raise AssertionError(f"unbalanced braces after {marker!r}")


def test_no_per_ip_throttle() -> None:
    # No limit_req/limit_conn (or their *_zone declarations) anywhere -- those
    # produced the CORS-less 503 on the reader's legitimate download burst.
    offenders = [
        line
        for line in _directive_lines()
        if re.match(r"limit_(req|conn)(_zone)?\b", line)
    ]
    assert offenders == [], f"unexpected per-IP throttle directives: {offenders}"


def test_errors_carry_cors() -> None:
    directives = _directive_lines()
    # nginx-generated 5xx is routed to the CORS-bearing error handler...
    assert any(
        line.startswith("error_page") and "@cors_error" in line
        for line in directives
    ), "error_page must route nginx-generated 5xx to @cors_error"
    # ...and that handler reflects the request Origin.
    text = _TEMPLATE.read_text()
    assert "location @cors_error" in text
    assert re.search(
        r"add_header\s+Access-Control-Allow-Origin\s+\$cors_origin\s+always",
        text,
    ), "@cors_error must add Access-Control-Allow-Origin: $cors_origin (always)"


def test_xaccel_download_reattaches_cors() -> None:
    # nginx does NOT carry Python's response headers onto the file it serves via
    # the X-Accel internal redirect, so CORS must be re-attached on the
    # internal-library location or every successful (200/206) download lacks
    # Access-Control-Allow-Origin and the reader's cross-origin fetch is blocked.
    text = _TEMPLATE.read_text()
    body = _location_body(text, "location /internal-library/")
    assert re.search(
        r"add_header\s+Access-Control-Allow-Origin\s+\$cors_origin\s+always",
        body,
    ), "the X-Accel internal-library location must re-attach ACAO: $cors_origin"


def test_cors_not_duplicated_on_proxy_path() -> None:
    # ACAO is added only by nginx contexts that lack Python's header: the
    # X-Accel file location and the error handler. It must NOT be added on the
    # `location /` proxy path -- those responses already carry Python's ACAO, and
    # a second value makes the browser reject the response.
    proxy_body = _location_body(_TEMPLATE.read_text(), "location / {")
    assert "Access-Control-Allow-Origin" not in proxy_body, (
        "the proxy location must not add ACAO (Python already supplies it)"
    )
    acao = [
        line
        for line in _directive_lines()
        if re.match(r"add_header\s+Access-Control-Allow-Origin\b", line)
    ]
    # Exactly two: /internal-library/ (success) and @cors_error (failure).
    assert len(acao) == 2, f"expected exactly two ACAO add_headers, got: {acao}"

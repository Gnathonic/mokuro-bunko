"""Every reverse proxy the project documents must carry a processor's channels.

A processor's events channel is ONE chunked request body that lives as long as
its session, and its assignment stream is one chunked response that lives as
long as the connection. A proxy that buffers the request body delivers no
event until the body ends (the library then waits out its wedge timer), and a
proxy that caps the body's size cuts a long session off after a few hundred
sidecars (the cap is cumulative over the one body). So each documented proxy
must route ``/_processor/`` with request buffering off, no body limit, HTTP/1.1
to the backend, and timeouts well above the 15 s heartbeat.
"""

from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).parents[2]


def _strip_comments(text: str) -> str:
    return "\n".join(raw.split("#", 1)[0] for raw in text.splitlines())


def _block(text: str, marker: str) -> str:
    """The brace-balanced body of the first block introduced by ``marker``."""
    open_idx = text.index("{", text.index(marker))
    depth = 0
    for index in range(open_idx, len(text)):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                return text[open_idx + 1 : index]
    raise AssertionError(f"unbalanced braces after {marker!r}")


def _nginx_processor_location(text: str) -> str:
    body = _strip_comments(text)
    assert "location /_processor/" in body, "no location /_processor/ block"
    return _block(body, "location /_processor/")


def _assert_nginx_carries_processors(location: str) -> None:
    assert re.search(r"\bproxy_request_buffering\s+off\s*;", location), (
        "nginx must not buffer an events body: nothing reaches the library until it ends"
    )
    assert re.search(r"\bclient_max_body_size\s+0\s*;", location), (
        "an events body is cumulative: any size cap cuts a long session off"
    )
    assert re.search(r"\bproxy_http_version\s+1\.1\s*;", location)
    assert re.search(r"\bproxy_buffering\s+off\s*;", location), (
        "the assignment stream must reach the processor op by op"
    )
    for directive in ("proxy_read_timeout", "proxy_send_timeout"):
        match = re.search(rf"\b{directive}\s+(\d+)s?\s*;", location)
        assert match is not None, f"{directive} must be set for /_processor/"
        assert int(match.group(1)) >= 60, f"{directive} must be well above the heartbeat"


def test_the_internal_nginx_template_carries_processor_channels() -> None:
    text = (ROOT / "deploy" / "nginx-internal.conf.template").read_text()
    _assert_nginx_carries_processors(_nginx_processor_location(text))


def test_the_nginx_example_carries_processor_channels() -> None:
    text = (ROOT / "deploy" / "nginx.conf.example").read_text()
    _assert_nginx_carries_processors(_nginx_processor_location(text))


def test_the_deployment_guide_nginx_example_carries_processor_channels() -> None:
    guide = (ROOT / "docs" / "deployment.md").read_text()
    blocks = re.findall(r"```nginx\n(.*?)```", guide, flags=re.S)
    assert blocks, "docs/deployment.md has no nginx example"
    carrying = [block for block in blocks if "location /_processor/" in block]
    assert carrying, "the nginx example in docs/deployment.md has no /_processor/ block"
    _assert_nginx_carries_processors(_nginx_processor_location(carrying[0]))


def test_the_caddy_example_gives_processors_an_unlimited_body() -> None:
    text = _strip_comments((ROOT / "deploy" / "caddy.example").read_text())
    assert re.search(r"@processor\s+path\s+/_processor/\*", text), (
        "the Caddy example must match /_processor/* on its own"
    )
    processor = _block(text, "handle @processor")
    assert "max_size" not in processor, "no body cap on the processor channels"
    assert "reverse_proxy" in processor
    assert re.search(r"\bflush_interval\s+-1\b", processor), (
        "the assignment stream must be flushed op by op"
    )
    # The upload cap still applies everywhere else.
    rest = text.replace(processor, "")
    assert re.search(r"max_size\s+\d+", rest), "uploads keep their body cap"


def test_the_deployment_guide_states_the_requirements() -> None:
    guide = (ROOT / "docs" / "deployment.md").read_text()
    assert "### Remote OCR processors behind a proxy" in guide
    section = guide.split("### Remote OCR processors behind a proxy", 1)[1]
    section = section.split("\n### ", 1)[0]
    for needle in (
        "proxy_request_buffering off",
        "client_max_body_size 0",
        "HTTP/1.1",
        "heartbeat",
    ):
        assert needle in section, f"the section must say {needle!r}"

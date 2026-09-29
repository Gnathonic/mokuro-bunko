"""A Windows console (or redirected output) on a legacy code page cannot
encode every character the CLI prints. Seen on Windows 11 over SSH (cp1252):
`processor setup` died with UnicodeEncodeError on the "→" of its hardware line,
after the login check and before writing anything."""

from __future__ import annotations

import io

from mokuro_bunko.__main__ import tolerant_console_streams


def _stream(encoding: str) -> io.TextIOWrapper:
    return io.TextIOWrapper(io.BytesIO(), encoding=encoding, errors="strict")


def test_a_legacy_code_page_stream_stops_raising_on_characters_it_lacks() -> None:
    out, err = _stream("cp1252"), _stream("cp1252")
    tolerant_console_streams(out, err)
    out.write("Hardware: NVIDIA GeForce RTX 3070 → cuda\n")
    err.write("テスト\n")
    out.flush()
    assert b"RTX 3070" in out.buffer.getvalue()  # type: ignore[attr-defined]


def test_a_utf8_stream_is_left_alone() -> None:
    out = _stream("utf-8")
    tolerant_console_streams(out, out)
    assert out.errors == "strict"
    out.write("→\n")


def test_a_stream_that_cannot_be_reconfigured_is_left_alone() -> None:
    class Plain:
        encoding = "cp1252"

    tolerant_console_streams(Plain(), Plain())  # no exception

"""The two codecs the remote protocol is made of, and its name sets."""

from __future__ import annotations

import io

import pytest

from mokuro_bunko.ocr.remote.protocol import (
    EVENTS,
    MAX_PAYLOAD_BYTES,
    OPS,
    PROTOCOL_VERSION,
    ProtocolError,
    decode_line,
    encode_frame,
    encode_line,
    is_event,
    is_op,
    read_exactly,
    read_frame,
)


def test_the_version_and_the_names_are_the_contract() -> None:
    assert PROTOCOL_VERSION == 2
    assert OPS == frozenset(
        {"open_session", "volume", "cancel", "close_session", "bench", "heartbeat"}
    )
    assert {"page", "volume_done", "sidecar", "ping", "exit", "spawn_failed"} <= EVENTS
    # Protocol 2: a claim's archive on its way, and a claim given back.
    assert {"fetch", "volume_returned"} <= EVENTS


def test_the_validators_are_what_callers_gate_on() -> None:
    assert is_op("volume") is True
    assert is_op("volumes") is False
    assert is_op(None) is False
    assert is_event("bench_trial") is True
    assert is_event("nonsense") is False
    assert is_event(7) is False


def test_a_line_round_trips_and_carries_no_newline() -> None:
    raw = encode_line({"op": "heartbeat"})
    assert raw.endswith(b"\n")
    assert raw.count(b"\n") == 1
    assert decode_line(raw) == {"op": "heartbeat"}


def test_a_blank_or_unreadable_line_is_none_not_an_exception() -> None:
    assert decode_line(b"\n") is None
    assert decode_line(b"not json\n") is None
    assert decode_line(b'"a string"\n') is None
    assert decode_line("  ") is None


def test_a_frame_carries_a_binary_payload_beside_its_json() -> None:
    blob = bytes(range(256)) * 4
    raw = encode_frame({"event": "sidecar", "id": "v1"}, blob)
    head, payload = read_frame(io.BytesIO(raw).read)
    assert head == {"event": "sidecar", "id": "v1", "payload": len(blob)}
    assert payload == blob


def test_frames_pack_back_to_back_and_read_back_in_order() -> None:
    stream = io.BytesIO(
        encode_frame({"event": "page", "done": 1})
        + encode_frame({"event": "page", "done": 2})
    )
    assert read_frame(stream.read)[0]["done"] == 1
    assert read_frame(stream.read)[0]["done"] == 2
    assert read_frame(stream.read) is None


def test_read_exactly_returns_empty_at_eof_not_a_short_read() -> None:
    stream = io.BytesIO(b"abc")
    assert read_exactly(stream.read, 3) == b"abc"
    assert read_exactly(stream.read, 1) == b""


def test_an_over_range_frame_header_is_refused_rather_than_allocated() -> None:
    stream = io.BytesIO(b"ffffffff" + b"{}")
    with pytest.raises(ProtocolError):
        read_frame(stream.read)


def test_a_declared_payload_bigger_than_the_cap_is_refused_rather_than_allocated() -> None:
    head = encode_line({"event": "sidecar", "payload": MAX_PAYLOAD_BYTES + 1})[:-1]
    stream = io.BytesIO(b"%08x" % len(head) + head)
    with pytest.raises(ProtocolError):
        read_frame(stream.read)


def test_a_stream_cut_right_after_the_header_raises_not_none() -> None:
    # A full 8-byte header declaring a 16-byte head, then nothing.
    stream = io.BytesIO(b"00000010")
    with pytest.raises(ProtocolError):
        read_frame(stream.read)


def test_a_stream_cut_mid_payload_raises_not_none() -> None:
    raw = encode_frame({"event": "sidecar"}, b"0123456789")
    torn = raw[:-5]  # the head arrived whole; only 5 of the 10 payload bytes did
    stream = io.BytesIO(torn)
    with pytest.raises(ProtocolError):
        read_frame(stream.read)


def test_an_empty_stream_returns_none_not_an_error() -> None:
    stream = io.BytesIO(b"")
    assert read_frame(stream.read) is None


def test_a_non_numeric_payload_size_is_refused_not_coalesced_to_zero() -> None:
    for bad in ('""', "false"):
        head = b'{"event": "ping", "payload": ' + bad.encode() + b"}"
        stream = io.BytesIO(b"%08x" % len(head) + head)
        with pytest.raises(ProtocolError):
            read_frame(stream.read)


def test_a_caller_supplied_payload_key_is_dropped_not_trusted() -> None:
    raw = encode_frame({"event": "page", "payload": 99})
    stream = io.BytesIO(raw + encode_frame({"event": "page", "done": 1}))
    head, payload = read_frame(stream.read)
    assert head == {"event": "page"}
    assert payload == b""
    # and the next frame reads back intact -- no desync from the lie above.
    next_head, _ = read_frame(stream.read)
    assert next_head["done"] == 1

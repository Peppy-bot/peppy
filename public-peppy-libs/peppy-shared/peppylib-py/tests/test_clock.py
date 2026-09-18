"""Round-trip tests for the `clock` capnp wire-type bindings.

These tests don't touch the messenger or a core node — they verify the Rust
encode/decode boundary so Python users can rely on `ClockTick.decode(payload)`
inside a topic subscriber loop and `ClockResponse.decode(payload)` after a
`ServiceMessenger.poll`.
"""

import pytest

from peppylib.clock import ClockRequest, ClockResponse, ClockTick


def test_clock_request_round_trip() -> None:
    request = ClockRequest(1_700_000_000_123_456_789)
    decoded = ClockRequest.decode(request.encode())

    assert decoded.client_send_time == 1_700_000_000_123_456_789


def test_clock_request_decode_rejects_garbage() -> None:
    with pytest.raises(ValueError):
        ClockRequest.decode(b"not a capnp message")


def test_clock_response_round_trip_preserves_all_fields() -> None:
    response = ClockResponse(
        client_send_time=10,
        server_recv_time=110,
        server_send_time=115,
    )
    decoded = ClockResponse.decode(response.encode())

    assert decoded.client_send_time == 10
    assert decoded.server_recv_time == 110
    assert decoded.server_send_time == 115


def test_clock_response_decode_rejects_garbage() -> None:
    with pytest.raises(ValueError):
        ClockResponse.decode(b"not a capnp message")


def test_clock_tick_round_trip_preserves_all_fields() -> None:
    tick = ClockTick(time=42)
    decoded = ClockTick.decode(tick.encode())

    assert decoded.time == 42


def test_clock_tick_decode_rejects_garbage() -> None:
    with pytest.raises(ValueError):
        ClockTick.decode(b"\x00\x01\x02")

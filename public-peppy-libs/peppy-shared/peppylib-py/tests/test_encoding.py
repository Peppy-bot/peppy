"""Tests for peppylib encoding helpers used by generated topic code."""

import pytest

from peppylib import encoding


def test_convert_time_round_trip() -> None:
    original = 1_700_000_000.25
    timestamp = encoding.convert_time(original)

    assert timestamp.sec == 1_700_000_000
    assert timestamp.nsec == 250_000_000
    assert encoding.convert_time_from_capnp(timestamp.sec, timestamp.nsec) == pytest.approx(
        original
    )


def test_convert_time_negative_value() -> None:
    timestamp = encoding.convert_time(-0.25)

    assert timestamp.sec == -1
    assert timestamp.nsec == 750_000_000
    assert encoding.convert_time_from_capnp(timestamp.sec, timestamp.nsec) == pytest.approx(
        -0.25
    )


def test_convert_time_from_capnp_rejects_invalid_nanos() -> None:
    with pytest.raises(ValueError):
        encoding.convert_time_from_capnp(1, 1_000_000_000)

"""Time conversion helpers used by generated Python topic code."""

from __future__ import annotations

from dataclasses import dataclass
import math

NANOS_PER_SEC = 1_000_000_000


@dataclass(frozen=True)
class CapnpTimestamp:
    sec: int
    nsec: int


def convert_time(timestamp: float) -> CapnpTimestamp:
    """Convert Unix epoch seconds into a Cap'n Proto-style timestamp."""
    value = float(timestamp)
    if not math.isfinite(value):
        raise ValueError("timestamp must be finite")

    sec = math.floor(value)
    frac = value - sec
    nsec = int(round(frac * NANOS_PER_SEC))

    # Guard against rounding to the next second (e.g. 0.9999999996).
    if nsec >= NANOS_PER_SEC:
        sec += 1
        nsec -= NANOS_PER_SEC
    elif nsec < 0:
        sec -= 1
        nsec += NANOS_PER_SEC

    return CapnpTimestamp(sec=int(sec), nsec=nsec)


def convert_time_from_capnp(sec: int, nsec: int) -> float:
    """Convert Cap'n Proto timestamp fields into Unix epoch seconds."""
    sec_i = int(sec)
    nsec_i = int(nsec)
    if not 0 <= nsec_i < NANOS_PER_SEC:
        raise ValueError(f"nsec must be in [0, {NANOS_PER_SEC}), got {nsec_i}")
    return float(sec_i) + (float(nsec_i) / NANOS_PER_SEC)

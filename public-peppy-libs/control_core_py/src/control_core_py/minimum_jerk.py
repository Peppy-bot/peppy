"""Minimum-jerk point-to-point profiles for discrete move actions.

The quintic s(u) = 10u^3 - 15u^4 + 6u^5 has peak velocity 15/8 * |delta| / T,
so the duration floor per joint is |delta| * 15 / (8 * v_max) and a requested
duration below it is stretched rather than violated.
"""

from __future__ import annotations

import math
from dataclasses import dataclass

# Peak velocity of the quintic blend relative to |delta| / duration; sizing a
# duration from a velocity budget multiplies the travel ratio by this.
QUINTIC_PEAK_VELOCITY = 15.0 / 8.0


def _shape(u: float) -> float:
    return 10.0 * u**3 - 15.0 * u**4 + 6.0 * u**5


def min_duration_s(
    start: tuple[float, ...], end: tuple[float, ...], velocity_caps: tuple[float, ...]
) -> float:
    if not (len(start) == len(end) == len(velocity_caps)):
        raise ValueError("start, end, and velocity_caps must be the same length")
    if not all(math.isfinite(cap) and cap > 0.0 for cap in velocity_caps):
        raise ValueError("velocity caps must be finite and positive")
    return max(
        (
            abs(e - s) * QUINTIC_PEAK_VELOCITY / cap
            for s, e, cap in zip(start, end, velocity_caps, strict=True)
        ),
        default=0.0,
    )


@dataclass(frozen=True)
class Profile:
    start: tuple[float, ...]
    end: tuple[float, ...]
    duration_s: float

    def sample(self, t_s: float) -> tuple[float, ...]:
        if not math.isfinite(t_s):
            raise ValueError("sample time must be finite")
        if t_s >= self.duration_s or self.duration_s == 0.0:
            return self.end
        s = _shape(max(t_s, 0.0) / self.duration_s)
        return tuple(a + (b - a) * s for a, b in zip(self.start, self.end, strict=True))

    def done(self, t_s: float) -> bool:
        return t_s >= self.duration_s


def plan(
    start: tuple[float, ...],
    end: tuple[float, ...],
    requested_duration_s: float,
    velocity_caps: tuple[float, ...],
) -> Profile:
    """A profile honoring the requested duration, stretched to the velocity
    caps' floor; a requested 0 means fastest allowed. Non-finite inputs are
    refused here: a NaN endpoint would silently zero the duration floor and
    bypass the caps."""
    values = (*start, *end, requested_duration_s)
    if not all(math.isfinite(v) for v in values):
        raise ValueError("non-finite trajectory endpoint or duration")
    if requested_duration_s < 0.0:
        raise ValueError("requested duration must be non-negative")
    floor = min_duration_s(start, end, velocity_caps)
    return Profile(start=start, end=end, duration_s=max(requested_duration_s, floor))

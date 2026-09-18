"""Validation helpers shared by Python peppy nodes' parameter parsing."""

from __future__ import annotations

import math


def require_positive(name: str, value: float) -> float:
    """The value as a float. Coerced, not just checked: a whole-number f64
    parameter can arrive as a Python int, and int-typed floats leak into
    isinstance-checking APIs downstream."""
    if not math.isfinite(value) or value <= 0.0:
        raise ValueError(f"{name} must be positive and finite, got {value}")
    return float(value)


def require_rate(name: str, value: int, max_hz: int) -> int:
    """A whole-hertz rate within the caller's ceiling.

    `max_hz` is the fastest the thing being paced can actually run. Only the
    caller knows it, so only the caller can state it, and the refusal quotes
    the bound it was given rather than one invented here.
    """
    if not 0 < value <= max_hz:
        raise ValueError(f"{name} must be in 1..={max_hz}, got {value}")
    return value


def require_non_empty(name: str, value: str) -> str:
    if not value:
        raise ValueError(f"{name} must not be empty")
    return value

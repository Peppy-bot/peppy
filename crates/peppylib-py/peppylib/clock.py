"""Clock helpers: align a node's notion of time with the core node.

This module is the Python face of `peppylib::clock`. It exposes the one-shot
NTP-style `synchronize`, the long-lived `subscribe` to the periodic ``clock``
topic, and `for_node` (which builds a pre-bound `PeppyClock` that reads the
daemon-resolved time without caring whether the node runs in wall or sim mode),
plus the clock wire/value types.
"""

from __future__ import annotations

from ._peppylib.core_node import (  # type: ignore[import-not-found]
    ClockRequest,
    ClockResponse,
    ClockSubscription,
    ClockSync,
    ClockTick,
    PeppyClock,
    clock_for_node as for_node,
    subscribe_clock as subscribe,
    synchronize,
)

__all__ = [
    "subscribe",
    "synchronize",
    "for_node",
    "PeppyClock",
    "ClockSync",
    "ClockSubscription",
    "ClockRequest",
    "ClockResponse",
    "ClockTick",
]

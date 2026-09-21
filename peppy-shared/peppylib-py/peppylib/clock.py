"""Clock helpers: align a node's notion of time with the core node.

This module is the Python face of `peppylib::clock`. It exposes the one-shot
NTP-style `synchronize`, the long-lived `subscribe` to the periodic ``clock``
topic, `for_node` (which builds a pre-bound `PeppyClock` that reads the
daemon-resolved time without caring whether the node runs in wall or sim mode),
and `ClockPublisher` (the instance that supplies one simulated clock domain),
plus the clock wire/value types. ``ClockPublisher.for_node`` returns ``None``
on a node whose deployment did not name it a domain's publisher, so holding a
publisher is the same fact as being one.
"""

from __future__ import annotations

from ._peppylib.core_node import (  # type: ignore[import-not-found]
    ClockRequest,
    ClockResponse,
    ClockSubscription,
    ClockSync,
    ClockTick,
    ClockBinding,
    ClockPublisher,
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
    "ClockPublisher",
    "ClockBinding",
    "ClockSync",
    "ClockSubscription",
    "ClockRequest",
    "ClockResponse",
    "ClockTick",
]

"""Pre-bound clock for the generated peppygen module.

Wraps :func:`peppylib.clock.for_node` so user code can read the
daemon-resolved time with a single call::

    await peppygen.clock.init(node_runner)
    t = peppygen.clock.now_ns()

``init`` must be called once before any ``now_ns`` call. Subsequent
``init`` calls are no-ops, so it is safe to call from both top-level setup
and helper functions that may be invoked first.

In wall mode ``init`` is a no-op wrapper. In sim mode it opens a
subscription to the ``clock`` topic so the first ``now_ns`` after a tick
is delivered returns immediately.
"""

import asyncio
from typing import Optional

import peppylib

_clock: Optional["peppylib.clock.PeppyClock"] = None
_clock_lock = asyncio.Lock()


async def init(node_runner: "peppylib.NodeRunner") -> None:
    """Build the pre-bound clock for ``node_runner``. Idempotent."""
    global _clock
    if _clock is not None:
        return
    async with _clock_lock:
        if _clock is not None:
            return
        _clock = await peppylib.clock.for_node(node_runner)


def now_ns() -> int:
    """Read the current core-node-aligned time in nanoseconds since the
    Unix epoch.

    Raises ``RuntimeError`` if ``init`` has not run, or in sim mode if no
    ``ClockTick`` has been observed yet.
    """
    if _clock is None:
        raise RuntimeError(
            "peppygen.clock.init must be called before now_ns"
        )
    return _clock.now_ns()

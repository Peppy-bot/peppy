"""Pre-bound clock for the generated peppygen module.

Wraps :func:`peppylib.clock.for_node` so user code can read the
daemon-resolved time with a single call::

    await peppygen.clock.init(node_runner)
    t = peppygen.clock.now_ns()

``init`` must be called before any ``now_ns`` call. It binds the module
clock to the initializing node: calling it again for the same node is a
no-op, so it is safe to call from both top-level setup and helper functions
that may be invoked first. Initializing a different node rebinds, which is
how consecutive test-harness boots in one process (wall- and sim-time
alike) each read their own clock; the harness serializes boots, so a
rebind never races a live node.

In wall mode ``init`` is a no-op wrapper. In sim mode it opens a
subscription to the ``clock`` topic so the first ``now_ns`` after a tick
is delivered returns immediately.
"""

from typing import Optional, Tuple

import peppylib

_clock: Optional["peppylib.clock.PeppyClock"] = None
#: The owning node's wire identity, so a second ``init`` can tell "same
#: node again" (no-op) from "new node" (rebind).
_clock_key: Optional[Tuple[str, str]] = None


async def init(node_runner: "peppylib.NodeRunner") -> None:
    """Build the pre-bound clock for ``node_runner``. Idempotent per node.

    Two helpers of one node racing their first ``init`` both resolve the
    same clock and the later assignment wins, so the race is benign; no
    lock is held across the await (an ``asyncio`` lock would pin the loop
    of whichever test booted first).
    """
    global _clock, _clock_key
    key = (node_runner.bound_core_node(), node_runner.bound_instance_id())
    if _clock is not None and _clock_key == key:
        return
    _clock = await peppylib.clock.for_node(node_runner)
    _clock_key = key


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

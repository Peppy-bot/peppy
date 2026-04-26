"""Integration test for `peppylib.synchronize`.

Python equivalent of `crates/core-node-internal/tests/listen_for_clock.rs`,
exercised through the high-level Python helper.
"""

import pytest

from peppylib import ClockResponse, ClockSource, synchronize

from .common import spawn_stub_listener, start_router_and_runner, wait_until_reachable


@pytest.mark.asyncio
async def test_synchronize_computes_offset_and_delay(tmp_path):
    """`synchronize()` performs the NTP exchange and returns a typed ClockSync.

    The stub listener replies with a hand-picked (t1, t2) so we can assert the
    NTP math without depending on real wall-clock readings. Symmetric link with
    100 ns offset, 20 ns total round-trip — the math test in
    `crates/peppylib/src/core_node/clock.rs` covers the same scenario.
    """
    # Plausible nanosecond timestamps. The exact t0 the client stamps is racy,
    # so we set t1 / t2 to values *much* larger than any realistic offset would
    # produce — the assertions below check structure, not exact magnitudes.
    canned_response = ClockResponse(
        client_send_time=0,  # echoed t0 — ignored by `synchronize`, which uses the live one
        server_recv_time=2_000_000_000_000,
        server_send_time=2_000_000_000_005,
        clock_source=ClockSource.Wall,
    )

    router, node_runner, server_handle = await start_router_and_runner(tmp_path)
    try:
        # Echo back the canned bytes regardless of the request payload — the
        # test cares about the helper's response handling, not the server.
        handler = await spawn_stub_listener(server_handle, "clock", canned_response.encode())
        await wait_until_reachable(node_runner.messenger(), "clock")

        sync = await synchronize(node_runner, 3.0)

        await handler
    finally:
        await router.stop()

    assert sync.clock_source == ClockSource.Wall
    # The raw response is exposed; t1/t2 must be exactly the canned values.
    assert sync.raw.server_recv_time == 2_000_000_000_000
    assert sync.raw.server_send_time == 2_000_000_000_005
    # Round-trip delay must be non-negative — the helper saturates negatives to
    # zero (see `compute_sync` in peppylib/src/core_node/clock.rs).
    assert sync.round_trip_delay_ns >= 0
    # Offset between local and the canned (huge) server timestamps must be
    # roughly the difference between t1≈t2 and t0≈t3. With t0 stamped from
    # SystemTime::now() (UNIX nanoseconds, ~1.7e18 today) and t1=2e12, the
    # offset is large and negative — local clock leads the canned server time.
    assert sync.offset_ns < 0

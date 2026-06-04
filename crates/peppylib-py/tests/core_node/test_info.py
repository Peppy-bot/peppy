"""Integration tests for `peppylib.info`.

Python equivalent of `crates/peppylib/tests/core_node/info.rs`.
"""

import pytest

from peppylib import ContainerInfo, InfoResponse, info

from .common import spawn_stub_listener, start_router_and_runner, wait_until_reachable


@pytest.mark.asyncio
async def test_info_returns_typed_response_fields(tmp_path):
    """`info()` decodes capnp bytes into the typed InfoResponse wrapper."""
    response = InfoResponse(
        1234,
        "standalone-core",
        "core-instance-1",
        "test-host",
        7,
        "v0.9.9",
        ContainerInfo("1.3.0", "0.20.0"),
        7447,
    )

    router, node_runner, server_handle = await start_router_and_runner(tmp_path)
    try:
        handler = await spawn_stub_listener(server_handle, "info", response.encode())
        await wait_until_reachable(node_runner.messenger(), "info")

        result = await info(node_runner, 3.0)

        await handler
    finally:
        await router.stop()

    assert result.uptime_secs == 1234
    assert result.core_node_name == "standalone-core"
    assert result.core_node_instance_id == "core-instance-1"
    assert result.host_name == "test-host"
    assert result.node_count == 7
    assert result.git_version == "v0.9.9"
    assert result.container_info.apptainer_version == "1.3.0"
    assert result.container_info.lima_version == "0.20.0"
    assert result.messaging_port == 7447

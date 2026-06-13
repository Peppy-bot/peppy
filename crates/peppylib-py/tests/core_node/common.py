"""Shared test fixtures for `core_node` integration tests: router/runner
setup, reachability polling, and a minimal `peppy.json5` writer. Per-test
modules provide their own stub-listener body because request/response types
differ per service.

Python equivalent of `crates/peppylib/tests/core_node/common.rs`.
"""

import asyncio
from pathlib import Path

import pytest

from peppylib import (
    MessengerHandle,
    NodeRunner,
    ProducerRef,
    SenderTarget,
    ServiceMessenger,
    StandaloneConfig,
    ZenohdInstance,
)

CORE_NODE = "standalone-core"
# Mirrors `core_node_api::names::CORE_NODE_TAG` — the core node always uses
# this tag on the wire, so reachability probes and stub listeners must too.
CORE_NODE_TAG = "core"
CLIENT_INSTANCE = "test_caller"
SERVER_INSTANCE = "test_server"

_PEPPY_CONFIG = """{
    peppy_schema: "node_v1",
    manifest: { name: "test_node", tag: "v1" },
    execution: { language: "rust", run_cmd: ["./target/debug/test_node"] },
}"""


def write_standalone_peppy_config(tmp_path: Path) -> Path:
    """Write a minimal peppy.json5 into `tmp_path` suitable for standalone mode."""
    path = tmp_path / "peppy.json5"
    path.write_text(_PEPPY_CONFIG)
    return path


async def wait_until_reachable(messenger, service_name: str) -> None:
    """Poll `is_reachable` until the service responds, bounded by a 5s deadline."""
    deadline = asyncio.get_event_loop().time() + 5.0
    while True:
        if await ServiceMessenger.is_reachable(
            messenger,
            CORE_NODE,
            CLIENT_INSTANCE,
            SenderTarget.node(CORE_NODE, CORE_NODE_TAG),
            service_name,
            ProducerRef(CORE_NODE, SERVER_INSTANCE),):
            return
        if asyncio.get_event_loop().time() >= deadline:
            pytest.fail(f"{service_name} stub did not become reachable within 5s")
        await asyncio.sleep(0.025)


async def start_router_and_runner(tmp_path: Path):
    """Start an ephemeral zenoh router, build a `NodeRunner` pointed at it, and
    return the router, the runner, and a server-side `MessengerHandle` the
    caller uses to spawn its stub listener.

    Returns `(router, node_runner, server_handle)`. The caller must hold the
    router and runner for the duration of the test — dropping them tears down
    the messaging fabric.
    """
    router = await ZenohdInstance.start_ephemeral("127.0.0.1")
    server_handle = await MessengerHandle.from_host_port(router.host, router.port)

    peppy_config_path = write_standalone_peppy_config(tmp_path)
    standalone_config = (
        StandaloneConfig()
        .with_messaging(router.host, router.port)
        .with_instance_id(CLIENT_INSTANCE)
    )
    node_runner = await NodeRunner.new_standalone(
        str(peppy_config_path), standalone_config
    )

    return router, node_runner, server_handle


async def spawn_stub_listener(server_handle, service_name: str, response_bytes: bytes):
    """Spin up a single-shot service listener that replies with `response_bytes`.

    Returns the `asyncio.Future` running the handler; the caller awaits it to
    ensure the handler completed before tearing down the test.
    """
    endpoint = await ServiceMessenger.listen(
        server_handle,
        CORE_NODE,
        SERVER_INSTANCE,
        SenderTarget.node(CORE_NODE, CORE_NODE_TAG),
        service_name,
    )
    return asyncio.ensure_future(
        endpoint.handle_next_request(lambda _request: response_bytes)
    )

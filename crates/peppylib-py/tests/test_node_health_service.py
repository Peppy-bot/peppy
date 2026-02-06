"""
Tests for peppylib node health service.

Python equivalent of crates/peppylib/tests/node_health_service.rs.

Note: The Rust test calls `listen_for_node_health` directly with a mock
messenger.  That function is not exposed to Python, so instead we start a
full node via `NodeBuilder` (which registers the health service internally)
and exercise it through `ServiceMessenger.poll`.
"""

import queue
import tempfile
import threading
from pathlib import Path

import pytest

from peppylib import MessengerHandle, ServiceMessenger, ZenohdInstance
from peppylib.config import (
    NODE_CONFIG_FILE,
    NODE_HEALTH_SERVICE,
    PEPPYGEN_OUTPUT_PATH,
    RUNTIME_CONFIG_VAR_NAME,
    SHUTDOWN_SERVICE,
)
from peppylib.runtime import NodeBuilder

from common import (
    PEPPY_CONFIG,
    TEST_FREQUENCY_HZ,
    TEST_INSTANCE_ID,
    TEST_NODE_NAME,
    create_codegen_fingerprint,
    create_runtime_config,
    wait_for_service,
)

TEST_MASTER_NODE_NAME = "test_master_node"
CALLER_INSTANCE_ID = "caller_instance"


@pytest.mark.asyncio
async def test_node_health_request_response_roundtrip(monkeypatch):
    """Health service responds to a poll request with the correct instance_id.

    Mirrors the Rust test `node_health_request_response_roundtrip` in
    crates/peppylib/tests/node_health_service.rs.
    """
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        with tempfile.TemporaryDirectory() as temp_dir:
            peppy_config_path = Path(temp_dir) / NODE_CONFIG_FILE
            peppy_config_path.write_text(PEPPY_CONFIG)
            create_codegen_fingerprint(str(peppy_config_path), PEPPYGEN_OUTPUT_PATH)

            runtime_config_path = str(Path(temp_dir) / "peppy_runtime.json5")
            create_runtime_config(
                runtime_config_path,
                router.host,
                router.port,
                TEST_NODE_NAME,
                TEST_MASTER_NODE_NAME,
                TEST_INSTANCE_ID,
                {"frequency_hz": TEST_FREQUENCY_HZ},
            )

            monkeypatch.setenv(RUNTIME_CONFIG_VAR_NAME, runtime_config_path)
            monkeypatch.chdir(temp_dir)

            error_queue: queue.Queue = queue.Queue()

            def run_node():
                try:
                    NodeBuilder().run(lambda _params, _runner: None)
                except Exception as e:
                    error_queue.put(e)

            runner_thread = threading.Thread(target=run_node, daemon=True)
            runner_thread.start()

            messenger = await MessengerHandle.from_host_port(router.host, router.port)

            # Wait for the health service to be ready
            await wait_for_service(
                messenger,
                NODE_HEALTH_SERVICE,
                TEST_MASTER_NODE_NAME,
                CALLER_INSTANCE_ID,
                TEST_NODE_NAME,
                TEST_MASTER_NODE_NAME,
                TEST_INSTANCE_ID,
                runner_thread,
                error_queue,
            )

            # Build and send the health request
            request_payload = b"health"

            response = await ServiceMessenger.poll(
                messenger,
                TEST_MASTER_NODE_NAME,
                CALLER_INSTANCE_ID,
                TEST_NODE_NAME,
                NODE_HEALTH_SERVICE,
                TEST_MASTER_NODE_NAME,
                TEST_INSTANCE_ID,
                request_payload,
                2.0,
            )

            # Verify the response
            assert response is not None
            assert response.instance_id == TEST_INSTANCE_ID

            # Clean shutdown
            await ServiceMessenger.poll(
                messenger,
                TEST_MASTER_NODE_NAME,
                CALLER_INSTANCE_ID,
                TEST_NODE_NAME,
                SHUTDOWN_SERVICE,
                TEST_MASTER_NODE_NAME,
                TEST_INSTANCE_ID,
                b"shutdown",
                2.0,
            )
            runner_thread.join(timeout=10.0)

    assert not runner_thread.is_alive(), "Runner should have exited"
    assert error_queue.empty(), f"Runner error: {error_queue.get_nowait()}"

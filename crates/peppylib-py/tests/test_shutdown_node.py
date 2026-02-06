"""
Tests for peppylib node shutdown service.

Python equivalent of crates/peppylib/tests/shutdown_node.rs.

Note: The Rust test calls `listen_for_shutdown` directly with a mock
messenger.  That function is not exposed to Python, so instead we start a
full node via `NodeBuilder` (which registers the shutdown service internally)
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
async def test_shutdown_node(monkeypatch):
    """Shutdown service responds with the payload and causes the node to exit.

    Mirrors the Rust test `shutdown_node` in crates/peppylib/tests/shutdown_node.rs.
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

            # Wait for the shutdown service to be reachable
            await wait_for_service(
                messenger,
                SHUTDOWN_SERVICE,
                TEST_MASTER_NODE_NAME,
                CALLER_INSTANCE_ID,
                TEST_NODE_NAME,
                TEST_MASTER_NODE_NAME,
                TEST_INSTANCE_ID,
                runner_thread,
                error_queue,
            )

            # Send a shutdown request
            request_payload = b"shutdown"

            response = await ServiceMessenger.poll(
                messenger,
                TEST_MASTER_NODE_NAME,
                CALLER_INSTANCE_ID,
                TEST_NODE_NAME,
                SHUTDOWN_SERVICE,
                TEST_MASTER_NODE_NAME,
                TEST_INSTANCE_ID,
                request_payload,
                2.0,
            )

            # Verify the response echoes back the payload
            assert response.payload == request_payload
            assert response.instance_id == TEST_INSTANCE_ID

            # The node should exit after receiving the shutdown signal
            runner_thread.join(timeout=10.0)

    assert not runner_thread.is_alive(), "Runner should have exited after shutdown"
    assert error_queue.empty(), f"Runner error: {error_queue.get_nowait()}"

"""
Tests for peppylib node ready service.

Python equivalent of crates/peppylib/tests/ready_node.rs.

Note: The Rust test calls `listen_for_node_ready` directly with a mock
messenger.  That function is not exposed to Python, so instead we start a
full node via `NodeBuilder` (which registers the ready service internally)
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
    NODE_READY_SERVICE,
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
async def test_ready_node(monkeypatch):
    """Ready service accepts all valid targeting modes and echoes back the payload.

    Mirrors the Rust test `ready_node` in crates/peppylib/tests/ready_node.rs.
    The Rust test validates four targeting combinations:
    - specific master + specific instance
    - specific master + broadcast instance
    - broadcast master + specific instance
    - full broadcast (master + instance)
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

            # Wait for the ready service to be reachable
            await wait_for_service(
                messenger,
                NODE_READY_SERVICE,
                TEST_MASTER_NODE_NAME,
                CALLER_INSTANCE_ID,
                TEST_NODE_NAME,
                TEST_MASTER_NODE_NAME,
                TEST_INSTANCE_ID,
                runner_thread,
                error_queue,
            )

            request_payload = b"ready"

            # The ready service should accept all valid targeting modes:
            # - specific master + specific instance
            # - specific master + broadcast instance
            # - broadcast master + specific instance
            # - full broadcast (master + instance)
            target_combinations = [
                ("specific+specific", TEST_MASTER_NODE_NAME, TEST_INSTANCE_ID),
                ("specific+broadcast", TEST_MASTER_NODE_NAME, None),
                ("broadcast+specific", None, TEST_INSTANCE_ID),
                ("broadcast+broadcast", None, None),
            ]

            for label, target_master_node, target_instance_id in target_combinations:
                try:
                    response = await ServiceMessenger.poll(
                        messenger,
                        TEST_MASTER_NODE_NAME,
                        CALLER_INSTANCE_ID,
                        TEST_NODE_NAME,
                        NODE_READY_SERVICE,
                        target_master_node,
                        target_instance_id,
                        request_payload,
                        2.0,
                    )
                except RuntimeError as exc:
                    pytest.fail(f"[{label}] poll failed: {exc}")

                assert response.payload == request_payload
                assert response.master_node == TEST_MASTER_NODE_NAME
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

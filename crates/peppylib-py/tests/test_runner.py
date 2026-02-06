"""
Tests for peppylib NodeBuilder runner lifecycle.

Python equivalent of crates/peppylib/tests/runner.rs.
"""

import hashlib
import json
import queue
import tempfile
import threading
import asyncio
from pathlib import Path

import pytest

from peppylib import MessengerHandle, ServiceMessenger, ZenohdInstance
from peppylib.config import (
    NODE_CONFIG_FILE,
    NODE_HEALTH_SERVICE,
    NODE_READY_SERVICE,
    PEPPYGEN_OUTPUT_PATH,
    RUNTIME_CONFIG_VAR_NAME,
    SHUTDOWN_SERVICE,
)
from peppylib.runtime import CancellationToken, NodeBuilder, StandaloneConfig

TEST_MASTER_NODE = "test_master"
TEST_NODE_NAME = "test_node"
TEST_INSTANCE_ID = "test_instance"
SHUTDOWN_SENDER_INSTANCE_ID = "test_shutdown_sender"
TEST_FREQUENCY_HZ = 10.0

PEPPY_CONFIG = """{
  schema_version: 1,
  manifest: {
    name: "test_node",
    tag: "0.1.0",
    language: "rust",
    start_cmd: ["cargo", "run"]
  },
  parameters: {
    frequency_hz: "f64"
  }
}"""


def create_codegen_fingerprint(config_path: str, output_path: str) -> None:
    """Create a SHA256 fingerprint of the config file (pure Python equivalent)."""
    config = Path(config_path)
    config_dir = config.parent
    fingerprint_dir = config_dir / output_path
    fingerprint_dir.mkdir(parents=True, exist_ok=True)

    config_bytes = config.read_bytes()
    fingerprint = hashlib.sha256(config_bytes).hexdigest()
    (fingerprint_dir / "peppy.json5.sha256").write_text(f"{fingerprint}\n")


def create_runtime_config(
    path: str,
    host: str,
    port: int,
    node_name: str,
    master_node: str,
    instance_id: str,
    arguments: dict,
) -> None:
    """Write a runtime config JSON file."""
    config = {
        "messaging_host": host,
        "messaging_port": port,
        "node_name": node_name,
        "bound_master_node": master_node,
        "node_instance": {
            "instance_id": instance_id,
            "arguments": arguments,
        },
    }
    Path(path).write_text(json.dumps(config))


async def _wait_for_service(
    messenger,
    service_name: str,
    runner_thread: threading.Thread,
    error_queue: queue.Queue,
    timeout_secs: float = 10.0,
):
    """Poll until a service becomes reachable, or fail."""
    deadline = asyncio.get_event_loop().time() + timeout_secs
    while True:
        if not runner_thread.is_alive():
            error = error_queue.get_nowait() if not error_queue.empty() else None
            pytest.fail(f"Runner exited early: {error}")

        if await ServiceMessenger.is_reachable(
            messenger,
            TEST_MASTER_NODE,
            SHUTDOWN_SENDER_INSTANCE_ID,
            TEST_NODE_NAME,
            service_name,
            TEST_MASTER_NODE,
            TEST_INSTANCE_ID,
        ):
            return

        if asyncio.get_event_loop().time() >= deadline:
            pytest.fail(f"{service_name} service did not become reachable")

        await asyncio.sleep(0.05)


@pytest.mark.asyncio
async def test_daemon_runner_succeed(monkeypatch):
    """Node starts in daemon mode, parameters are deserialized, services work, shutdown exits."""
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
                TEST_MASTER_NODE,
                TEST_INSTANCE_ID,
                {"frequency_hz": TEST_FREQUENCY_HZ},
            )

            monkeypatch.setenv(RUNTIME_CONFIG_VAR_NAME, runtime_config_path)
            monkeypatch.chdir(temp_dir)

            result_queue: queue.Queue = queue.Queue()
            error_queue: queue.Queue = queue.Queue()

            def run_node():
                try:

                    def setup_fn(params, _node_runner):
                        result_queue.put(params["frequency_hz"])

                    NodeBuilder().run(setup_fn)
                except Exception as e:
                    error_queue.put(e)

            runner_thread = threading.Thread(target=run_node, daemon=True)
            runner_thread.start()

            frequency_hz = await asyncio.to_thread(result_queue.get, timeout=5.0)
            assert frequency_hz == TEST_FREQUENCY_HZ

            messenger = await MessengerHandle.from_host_port(router.host, router.port)

            # Wait for shutdown service to become reachable
            await _wait_for_service(
                messenger,
                SHUTDOWN_SERVICE,
                runner_thread,
                error_queue,
            )

            # Poll health service
            health_response = await ServiceMessenger.poll(
                messenger,
                TEST_MASTER_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                TEST_NODE_NAME,
                NODE_HEALTH_SERVICE,
                TEST_MASTER_NODE,
                TEST_INSTANCE_ID,
                b"health",
                2.0,
            )
            assert health_response is not None

            # Send shutdown
            shutdown_response = await ServiceMessenger.poll(
                messenger,
                TEST_MASTER_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                TEST_NODE_NAME,
                SHUTDOWN_SERVICE,
                TEST_MASTER_NODE,
                TEST_INSTANCE_ID,
                b"shutdown",
                2.0,
            )
            # Wait for runner to exit
            runner_thread.join(timeout=10.0)

    assert shutdown_response.payload == b"shutdown"
    assert shutdown_response.instance_id == TEST_INSTANCE_ID

    assert not runner_thread.is_alive(), "Runner should have exited"
    assert error_queue.empty(), f"Runner error: {error_queue.get_nowait()}"


@pytest.mark.asyncio
async def test_standalone_runner_succeed(monkeypatch):
    """Node starts in standalone mode, parameters correct, cancellation token shuts it down."""
    monkeypatch.delenv(RUNTIME_CONFIG_VAR_NAME, raising=False)
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        with tempfile.TemporaryDirectory() as temp_dir:
            peppy_config_path = str(Path(temp_dir) / NODE_CONFIG_FILE)
            Path(peppy_config_path).write_text(PEPPY_CONFIG)

            standalone_config = (
                StandaloneConfig()
                .with_parameters_json({"frequency_hz": TEST_FREQUENCY_HZ})
                .with_messaging(router.host, router.port)
                .with_instance_id(TEST_INSTANCE_ID)
            )

            token_queue: queue.Queue = queue.Queue()
            error_queue: queue.Queue = queue.Queue()

            def run_node():
                try:

                    def setup_fn(params, node_runner):
                        assert params["frequency_hz"] == TEST_FREQUENCY_HZ
                        token_queue.put(node_runner.cancellation_token())

                    (
                        NodeBuilder()
                        .with_config_path(peppy_config_path)
                        .standalone(standalone_config)
                        .run(setup_fn)
                    )
                except Exception as e:
                    error_queue.put(e)

            runner_thread = threading.Thread(target=run_node, daemon=True)
            runner_thread.start()

            cancellation_token: CancellationToken = await asyncio.to_thread(
                token_queue.get, timeout=5.0
            )

            # Signal shutdown via cancellation token
            cancellation_token.cancel()

            # Runner should exit after cancellation
            runner_thread.join(timeout=10.0)
    assert not runner_thread.is_alive(), "Runner should have exited"
    assert error_queue.empty(), f"Runner error: {error_queue.get_nowait()}"


@pytest.mark.asyncio
async def test_node_ready_but_not_healthy(monkeypatch):
    """Ready service available before setup completes; health service only after."""
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
                TEST_MASTER_NODE,
                TEST_INSTANCE_ID,
                {"frequency_hz": TEST_FREQUENCY_HZ},
            )

            monkeypatch.setenv(RUNTIME_CONFIG_VAR_NAME, runtime_config_path)
            monkeypatch.chdir(temp_dir)

            setup_started = threading.Event()
            setup_continue = threading.Event()
            error_queue: queue.Queue = queue.Queue()

            def run_node():
                try:

                    def setup_fn(_params, _node_runner):
                        setup_started.set()
                        setup_continue.wait(timeout=30.0)

                    NodeBuilder().run(setup_fn)
                except Exception as e:
                    error_queue.put(e)

            runner_thread = threading.Thread(target=run_node, daemon=True)
            runner_thread.start()

            await asyncio.to_thread(setup_started.wait, timeout=5.0)
            assert setup_started.is_set(), "Setup should have started"

            messenger = await MessengerHandle.from_host_port(router.host, router.port)

            # Wait for ready service
            await _wait_for_service(
                messenger,
                NODE_READY_SERVICE,
                runner_thread,
                error_queue,
            )

            # Poll ready service — should echo back
            ready_response = await ServiceMessenger.poll(
                messenger,
                TEST_MASTER_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                TEST_NODE_NAME,
                NODE_READY_SERVICE,
                TEST_MASTER_NODE,
                TEST_INSTANCE_ID,
                b"ready",
                2.0,
            )
            assert ready_response.payload == b"ready"
            assert ready_response.instance_id == TEST_INSTANCE_ID

            # Wait for shutdown service
            await _wait_for_service(
                messenger,
                SHUTDOWN_SERVICE,
                runner_thread,
                error_queue,
            )

            # Health service should NOT be reachable while setup is blocked
            health_reachable = await ServiceMessenger.is_reachable(
                messenger,
                TEST_MASTER_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                TEST_NODE_NAME,
                NODE_HEALTH_SERVICE,
                TEST_MASTER_NODE,
                TEST_INSTANCE_ID,
            )
            assert not health_reachable, (
                "Health service should not be reachable while setup is blocked"
            )

            # Polling health should fail
            with pytest.raises(RuntimeError):
                await ServiceMessenger.poll(
                    messenger,
                    TEST_MASTER_NODE,
                    SHUTDOWN_SENDER_INSTANCE_ID,
                    TEST_NODE_NAME,
                    NODE_HEALTH_SERVICE,
                    TEST_MASTER_NODE,
                    TEST_INSTANCE_ID,
                    b"health",
                    0.2,
                )

            # Unblock setup
            setup_continue.set()

            # Wait for health service to become reachable
            await _wait_for_service(
                messenger,
                NODE_HEALTH_SERVICE,
                runner_thread,
                error_queue,
            )

            # Poll health service — should now succeed
            health_response = await ServiceMessenger.poll(
                messenger,
                TEST_MASTER_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                TEST_NODE_NAME,
                NODE_HEALTH_SERVICE,
                TEST_MASTER_NODE,
                TEST_INSTANCE_ID,
                b"health",
                2.0,
            )
            assert health_response is not None

            # Send shutdown
            shutdown_response = await ServiceMessenger.poll(
                messenger,
                TEST_MASTER_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                TEST_NODE_NAME,
                SHUTDOWN_SERVICE,
                TEST_MASTER_NODE,
                TEST_INSTANCE_ID,
                b"shutdown",
                2.0,
            )
            # Wait for runner to exit
            runner_thread.join(timeout=10.0)

    assert shutdown_response.payload == b"shutdown"
    assert shutdown_response.instance_id == TEST_INSTANCE_ID

    assert not runner_thread.is_alive(), "Runner should have exited"
    assert error_queue.empty(), f"Runner error: {error_queue.get_nowait()}"


@pytest.mark.asyncio
async def test_daemon_cancellation_token_cancelled_on_shutdown(monkeypatch):
    """Shutdown causes the cancellation token to be cancelled."""
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
                TEST_MASTER_NODE,
                TEST_INSTANCE_ID,
                {"frequency_hz": TEST_FREQUENCY_HZ},
            )

            monkeypatch.setenv(RUNTIME_CONFIG_VAR_NAME, runtime_config_path)
            monkeypatch.chdir(temp_dir)

            token_queue: queue.Queue = queue.Queue()
            error_queue: queue.Queue = queue.Queue()

            def run_node():
                try:

                    def setup_fn(_params, node_runner):
                        token_queue.put(node_runner.cancellation_token())

                    NodeBuilder().run(setup_fn)
                except Exception as e:
                    error_queue.put(e)

            runner_thread = threading.Thread(target=run_node, daemon=True)
            runner_thread.start()

            cancellation_token: CancellationToken = await asyncio.to_thread(
                token_queue.get, timeout=5.0
            )

            # Token should NOT be cancelled before shutdown
            assert not cancellation_token.is_cancelled(), (
                "Cancellation token should not be cancelled before shutdown"
            )

            messenger = await MessengerHandle.from_host_port(router.host, router.port)

            # Wait for shutdown service
            await _wait_for_service(
                messenger,
                SHUTDOWN_SERVICE,
                runner_thread,
                error_queue,
            )

            # Send shutdown
            await ServiceMessenger.poll(
                messenger,
                TEST_MASTER_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                TEST_NODE_NAME,
                SHUTDOWN_SERVICE,
                TEST_MASTER_NODE,
                TEST_INSTANCE_ID,
                b"shutdown",
                2.0,
            )

            # Wait for runner to exit
            runner_thread.join(timeout=10.0)
    assert not runner_thread.is_alive(), "Runner should have exited"

    # Token SHOULD be cancelled after shutdown
    assert cancellation_token.is_cancelled(), (
        "Cancellation token should be cancelled after shutdown"
    )
    assert error_queue.empty(), f"Runner error: {error_queue.get_nowait()}"

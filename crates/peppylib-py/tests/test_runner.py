"""
Tests for peppylib NodeBuilder runner lifecycle.

Python equivalent of crates/peppylib/tests/runner.rs.
"""

import faulthandler
import os
import queue
import sys
import tempfile
import threading
import asyncio
from pathlib import Path

import pytest

from peppylib import (
    MessengerHandle,
    ProducerRef,
    QoSProfile,
    SenderTarget,
    ServiceMessenger,
    TopicMessenger,
    ZenohdInstance,
)

# Dump a per-thread traceback if a fatal signal kills the test process, so a
# regression in the shutdown teardown surfaces as a stack trace, not a bare
# SIGSEGV.
faulthandler.enable()
from peppylib.config import (
    NODE_CONFIG_FILE,
    NODE_HEALTH_SERVICE,
    NODE_READY_SERVICE,
    PEPPYGEN_OUTPUT_PATH,
    RUNTIME_CONFIG_VAR_NAME,
    SHUTDOWN_SERVICE,
)
from peppylib.runtime import (
    CancellationToken,
    NodeBuilder,
    NodeRunner,
    StandaloneConfig,
)

from common import (
    PEPPY_CONFIG,
    TEST_FREQUENCY_HZ,
    TEST_INSTANCE_ID,
    TEST_NODE_NAME,
    TEST_NODE_TAG,
    create_codegen_fingerprint,
    create_runtime_config,
    wait_for_service,
    write_peppygen_stub,
)

TEST_CORE_NODE = "test_core"
SHUTDOWN_SENDER_INSTANCE_ID = "test_shutdown_sender"


async def _wait_for_service(
    messenger,
    service_name: str,
    runner_thread: threading.Thread,
    error_queue: queue.Queue,
    timeout_secs: float = 10.0,
):
    """Poll until a service becomes reachable, or fail."""
    await wait_for_service(
        messenger,
        service_name,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        TEST_NODE_NAME,
        ProducerRef(TEST_CORE_NODE, TEST_INSTANCE_ID),
        runner_thread,
        error_queue,
        timeout_secs,
    )


class NonCoroutineAwaitable:
    """Awaitable that is not a coroutine object. A shutdown hook returning one
    exercises the runtime's coercion path: asyncio's schedulers accept only
    coroutines, so the runtime must wrap other awaitables before scheduling."""

    def __init__(self, markers: list, label: str):
        self._markers = markers
        self._label = label

    def __await__(self):
        async def record():
            self._markers.append(self._label)

        return record().__await__()


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
                TEST_CORE_NODE,
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
                        result_queue.put(params.frequency_hz)

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
                TEST_CORE_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                SenderTarget.node(TEST_NODE_NAME, TEST_NODE_TAG),
                NODE_HEALTH_SERVICE,
                ProducerRef(TEST_CORE_NODE, TEST_INSTANCE_ID),
                b"health",
                2.0,)
            assert health_response is not None

            # Send shutdown
            shutdown_response = await ServiceMessenger.poll(
                messenger,
                TEST_CORE_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                SenderTarget.node(TEST_NODE_NAME, TEST_NODE_TAG),
                SHUTDOWN_SERVICE,
                ProducerRef(TEST_CORE_NODE, TEST_INSTANCE_ID),
                b"shutdown",
                2.0,)
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
                .with_parameters({"frequency_hz": TEST_FREQUENCY_HZ})
                .with_messaging(router.host, router.port)
                .with_instance_id(TEST_INSTANCE_ID)
            )

            token_queue: queue.Queue = queue.Queue()
            error_queue: queue.Queue = queue.Queue()

            def run_node():
                try:

                    def setup_fn(params, node_runner):
                        assert params.frequency_hz == TEST_FREQUENCY_HZ
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
async def test_standalone_with_parameters_dataclass(monkeypatch):
    """with_parameters() accepts a dataclass instance, not just a dict."""
    monkeypatch.delenv(RUNTIME_CONFIG_VAR_NAME, raising=False)
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        with tempfile.TemporaryDirectory() as temp_dir:
            peppy_config_path = str(Path(temp_dir) / NODE_CONFIG_FILE)
            Path(peppy_config_path).write_text(PEPPY_CONFIG)

            from peppygen.parameters import Parameters

            standalone_config = (
                StandaloneConfig()
                .with_parameters(Parameters(frequency_hz=TEST_FREQUENCY_HZ))
                .with_messaging(router.host, router.port)
                .with_instance_id(TEST_INSTANCE_ID)
            )

            token_queue: queue.Queue = queue.Queue()
            error_queue: queue.Queue = queue.Queue()

            def run_node():
                try:

                    def setup_fn(params, node_runner):
                        assert params.frequency_hz == TEST_FREQUENCY_HZ
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
async def test_async_setup_with_background_task(monkeypatch):
    """Async setup with asyncio.create_task() background task survives after setup returns."""
    monkeypatch.delenv(RUNTIME_CONFIG_VAR_NAME, raising=False)
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        with tempfile.TemporaryDirectory() as temp_dir:
            peppy_config_path = str(Path(temp_dir) / NODE_CONFIG_FILE)
            Path(peppy_config_path).write_text(PEPPY_CONFIG)

            standalone_config = (
                StandaloneConfig()
                .with_parameters({"frequency_hz": TEST_FREQUENCY_HZ})
                .with_messaging(router.host, router.port)
                .with_instance_id(TEST_INSTANCE_ID)
            )

            token_queue: queue.Queue = queue.Queue()
            started_queue: queue.Queue = queue.Queue()
            error_queue: queue.Queue = queue.Queue()

            def run_node():
                try:

                    async def setup_fn(_params, node_runner):
                        token_queue.put(node_runner.cancellation_token())

                        async def background_task():
                            started_queue.put("started")
                            while not node_runner.cancellation_token().is_cancelled():
                                await asyncio.sleep(0.05)

                        return [asyncio.create_task(background_task())]

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

            await asyncio.to_thread(started_queue.get, timeout=5.0)
            cancellation_token: CancellationToken = await asyncio.to_thread(
                token_queue.get, timeout=5.0
            )

            cancellation_token.cancel()
            runner_thread.join(timeout=10.0)

    assert not runner_thread.is_alive(), "Runner should have exited"
    assert error_queue.empty(), f"Runner error: {error_queue.get_nowait()}"


@pytest.mark.asyncio
async def test_setup_exception_propagates_to_run(monkeypatch):
    """Exceptions from setup propagate out of NodeBuilder.run."""
    monkeypatch.delenv(RUNTIME_CONFIG_VAR_NAME, raising=False)
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        with tempfile.TemporaryDirectory() as temp_dir:
            peppy_config_path = str(Path(temp_dir) / NODE_CONFIG_FILE)
            Path(peppy_config_path).write_text(PEPPY_CONFIG)

            standalone_config = (
                StandaloneConfig()
                .with_parameters({"frequency_hz": TEST_FREQUENCY_HZ})
                .with_messaging(router.host, router.port)
                .with_instance_id(TEST_INSTANCE_ID)
            )

            error_queue: queue.Queue = queue.Queue()

            def run_node():
                try:

                    def setup_fn(_params, _node_runner):
                        raise RuntimeError("setup boom")

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

            error = await asyncio.to_thread(error_queue.get, timeout=5.0)
            assert isinstance(error, RuntimeError)
            assert "setup boom" in str(error)
            runner_thread.join(timeout=10.0)

    assert not runner_thread.is_alive(), "Runner should have exited"


@pytest.mark.asyncio
async def test_run_accepts_async_setup(monkeypatch):
    """NodeBuilder.run auto-detects and supports async setup callbacks."""
    monkeypatch.delenv(RUNTIME_CONFIG_VAR_NAME, raising=False)
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        with tempfile.TemporaryDirectory() as temp_dir:
            peppy_config_path = str(Path(temp_dir) / NODE_CONFIG_FILE)
            Path(peppy_config_path).write_text(PEPPY_CONFIG)

            standalone_config = (
                StandaloneConfig()
                .with_parameters({"frequency_hz": TEST_FREQUENCY_HZ})
                .with_messaging(router.host, router.port)
                .with_instance_id(TEST_INSTANCE_ID)
            )

            token_queue: queue.Queue = queue.Queue()
            error_queue: queue.Queue = queue.Queue()

            def run_node():
                try:

                    async def setup_fn(params, node_runner):
                        assert params.frequency_hz == TEST_FREQUENCY_HZ
                        await asyncio.sleep(0.01)
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

            cancellation_token.cancel()
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
                TEST_CORE_NODE,
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
                TEST_CORE_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                SenderTarget.node(TEST_NODE_NAME, TEST_NODE_TAG),
                NODE_READY_SERVICE,
                ProducerRef(TEST_CORE_NODE, TEST_INSTANCE_ID),
                b"ready",
                2.0,)
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
                TEST_CORE_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                SenderTarget.node(TEST_NODE_NAME, TEST_NODE_TAG),
                NODE_HEALTH_SERVICE,
                ProducerRef(TEST_CORE_NODE, TEST_INSTANCE_ID),)
            assert not health_reachable, (
                "Health service should not be reachable while setup is blocked"
            )

            # Polling health should fail (service is unreachable)
            with pytest.raises(ConnectionError):
                await ServiceMessenger.poll(
                    messenger,
                    TEST_CORE_NODE,
                    SHUTDOWN_SENDER_INSTANCE_ID,
                    SenderTarget.node(TEST_NODE_NAME, TEST_NODE_TAG),
                    NODE_HEALTH_SERVICE,
                    ProducerRef(TEST_CORE_NODE, TEST_INSTANCE_ID),
                    b"health",
                    0.2,)

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
                TEST_CORE_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                SenderTarget.node(TEST_NODE_NAME, TEST_NODE_TAG),
                NODE_HEALTH_SERVICE,
                ProducerRef(TEST_CORE_NODE, TEST_INSTANCE_ID),
                b"health",
                2.0,)
            assert health_response is not None

            # Send shutdown
            shutdown_response = await ServiceMessenger.poll(
                messenger,
                TEST_CORE_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                SenderTarget.node(TEST_NODE_NAME, TEST_NODE_TAG),
                SHUTDOWN_SERVICE,
                ProducerRef(TEST_CORE_NODE, TEST_INSTANCE_ID),
                b"shutdown",
                2.0,)
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
                TEST_CORE_NODE,
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
                TEST_CORE_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                SenderTarget.node(TEST_NODE_NAME, TEST_NODE_TAG),
                SHUTDOWN_SERVICE,
                ProducerRef(TEST_CORE_NODE, TEST_INSTANCE_ID),
                b"shutdown",
                2.0,)

            # Wait for runner to exit
            runner_thread.join(timeout=10.0)
    assert not runner_thread.is_alive(), "Runner should have exited"

    # Token SHOULD be cancelled after shutdown
    assert cancellation_token.is_cancelled(), (
        "Cancellation token should be cancelled after shutdown"
    )
    assert error_queue.empty(), f"Runner error: {error_queue.get_nowait()}"


@pytest.mark.asyncio
async def test_daemon_shutdown_during_setup_exits_after_setup_completes(monkeypatch):
    """A shutdown received while setup is still running is honored once setup
    returns, without needing a second shutdown request.

    Python equivalent of the Rust `daemon_shutdown_during_setup_cancels_token_and_exits`
    test, with one binding-specific difference: the runtime cannot preempt a
    Python callback that is still on the stack, so the node keeps running until
    the setup callback returns, then exits from the already-received shutdown.
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
                TEST_CORE_NODE,
                TEST_INSTANCE_ID,
                {"frequency_hz": TEST_FREQUENCY_HZ},
            )

            monkeypatch.setenv(RUNTIME_CONFIG_VAR_NAME, runtime_config_path)
            monkeypatch.chdir(temp_dir)

            token_queue: queue.Queue = queue.Queue()
            setup_continue = threading.Event()
            error_queue: queue.Queue = queue.Queue()

            def run_node():
                try:

                    def setup_fn(_params, node_runner):
                        token_queue.put(node_runner.cancellation_token())
                        setup_continue.wait(timeout=30.0)

                    NodeBuilder().run(setup_fn)
                except Exception as e:
                    error_queue.put(e)

            runner_thread = threading.Thread(target=run_node, daemon=True)
            runner_thread.start()

            cancellation_token: CancellationToken = await asyncio.to_thread(
                token_queue.get, timeout=5.0
            )

            messenger = await MessengerHandle.from_host_port(router.host, router.port)

            # The shutdown service is registered pre-setup, so it must be
            # reachable while setup is still blocked
            await _wait_for_service(
                messenger,
                SHUTDOWN_SERVICE,
                runner_thread,
                error_queue,
            )

            # Send shutdown while setup is still blocked
            await ServiceMessenger.poll(
                messenger,
                TEST_CORE_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                SenderTarget.node(TEST_NODE_NAME, TEST_NODE_TAG),
                SHUTDOWN_SERVICE,
                ProducerRef(TEST_CORE_NODE, TEST_INSTANCE_ID),
                b"shutdown",
                2.0,)

            # The runner cannot exit while the setup callback is still on the
            # stack: run() only returns after setup does
            await asyncio.sleep(0.2)
            assert runner_thread.is_alive(), (
                "Runner must keep running while the setup callback is blocked"
            )

            # Unblock setup: the already-received shutdown must now take effect
            # without any further shutdown request
            setup_continue.set()
            runner_thread.join(timeout=10.0)

    assert not runner_thread.is_alive(), (
        "Runner should exit after setup returns, honoring the earlier shutdown"
    )
    assert cancellation_token.is_cancelled(), (
        "Cancellation token should be cancelled by a shutdown received during setup"
    )
    assert error_queue.empty(), f"Runner error: {error_queue.get_nowait()}"


@pytest.mark.asyncio
async def test_daemon_shutdown_interrupts_blocked_async_setup(monkeypatch):
    """A shutdown received while an async setup coroutine is still awaiting
    interrupts the runner immediately. Unlike the sync-setup case above, where
    the Python callback holds the stack, the runner waits on async setup
    without blocking, so it observes the request, runs hooks registered before
    the block, and exits without setup ever completing."""
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
                TEST_CORE_NODE,
                TEST_INSTANCE_ID,
                {"frequency_hz": TEST_FREQUENCY_HZ},
            )

            monkeypatch.setenv(RUNTIME_CONFIG_VAR_NAME, runtime_config_path)
            monkeypatch.chdir(temp_dir)

            token_queue: queue.Queue = queue.Queue()
            error_queue: queue.Queue = queue.Queue()
            hook_markers: list = []

            def run_node():
                try:

                    async def setup_fn(_params, node_runner):
                        def cleanup_hook():
                            hook_markers.append("cleanup")

                        node_runner.on_shutdown(cleanup_hook)
                        token_queue.put(node_runner.cancellation_token())
                        # Never completes: only the shutdown can end the node,
                        # by cancelling this await. The sentinel below must
                        # stay unreached; it trips the exact-list assertion if
                        # setup ever resumes instead of being interrupted.
                        await asyncio.Event().wait()
                        hook_markers.append("resumed")

                    NodeBuilder().run(setup_fn)
                except Exception as e:
                    error_queue.put(e)

            runner_thread = threading.Thread(target=run_node, daemon=True)
            runner_thread.start()

            cancellation_token: CancellationToken = await asyncio.to_thread(
                token_queue.get, timeout=5.0
            )

            messenger = await MessengerHandle.from_host_port(router.host, router.port)
            await _wait_for_service(
                messenger,
                SHUTDOWN_SERVICE,
                runner_thread,
                error_queue,
            )

            await ServiceMessenger.poll(
                messenger,
                TEST_CORE_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                SenderTarget.node(TEST_NODE_NAME, TEST_NODE_TAG),
                SHUTDOWN_SERVICE,
                ProducerRef(TEST_CORE_NODE, TEST_INSTANCE_ID),
                b"shutdown",
                2.0,
            )

            runner_thread.join(timeout=10.0)

    assert not runner_thread.is_alive(), (
        "Runner should exit while the async setup coroutine is still blocked"
    )
    assert cancellation_token.is_cancelled(), (
        "Cancellation token should be cancelled by a shutdown received during setup"
    )
    assert hook_markers == ["cleanup"], "Hook registered during setup should run"
    assert error_queue.empty(), f"Runner error: {error_queue.get_nowait()}"


@pytest.mark.asyncio
async def test_cancellation_token_cancelled_awaitable(monkeypatch):
    """token.cancelled() is awaitable: pending until cancel(), resolved after.

    Uses a standalone NodeRunner directly (no NodeBuilder.run) so the await
    runs on the test's own event loop, with no node shutdown machinery
    cancelling tasks underneath the assertions.
    """
    monkeypatch.delenv(RUNTIME_CONFIG_VAR_NAME, raising=False)
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        with tempfile.TemporaryDirectory() as temp_dir:
            peppy_config_path = str(Path(temp_dir) / NODE_CONFIG_FILE)
            Path(peppy_config_path).write_text(PEPPY_CONFIG)

            standalone_config = (
                StandaloneConfig()
                .with_parameters({"frequency_hz": TEST_FREQUENCY_HZ})
                .with_messaging(router.host, router.port)
                .with_instance_id(TEST_INSTANCE_ID)
            )
            node_runner = await NodeRunner.new_standalone(
                peppy_config_path, standalone_config
            )
            token = node_runner.cancellation_token()

            waiter = asyncio.ensure_future(token.cancelled())
            await asyncio.sleep(0.1)
            assert not waiter.done(), (
                "cancelled() must stay pending while the token is not cancelled"
            )

            token.cancel()
            await asyncio.wait_for(waiter, timeout=5.0)

            # An already-cancelled token resolves immediately
            await asyncio.wait_for(token.cancelled(), timeout=5.0)


@pytest.mark.asyncio
async def test_node_runner_exposes_messenger_and_metadata(monkeypatch):
    """NodeRunner exposes messenger(), bound_core_node(), bound_instance_id(), node_name()."""
    monkeypatch.delenv(RUNTIME_CONFIG_VAR_NAME, raising=False)
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        with tempfile.TemporaryDirectory() as temp_dir:
            peppy_config_path = str(Path(temp_dir) / NODE_CONFIG_FILE)
            Path(peppy_config_path).write_text(PEPPY_CONFIG)

            standalone_config = (
                StandaloneConfig()
                .with_parameters({"frequency_hz": TEST_FREQUENCY_HZ})
                .with_messaging(router.host, router.port)
                .with_instance_id(TEST_INSTANCE_ID)
                .with_node_name(TEST_NODE_NAME)
            )

            result_queue: queue.Queue = queue.Queue()
            error_queue: queue.Queue = queue.Queue()

            def run_node():
                try:

                    def setup_fn(_params, node_runner: NodeRunner):
                        result_queue.put(
                            {
                                "messenger": node_runner.messenger(),
                                "bound_core_node": node_runner.bound_core_node(),
                                "bound_instance_id": node_runner.bound_instance_id(),
                                "node_name": node_runner.node_name(),
                                "token": node_runner.cancellation_token(),
                            }
                        )

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

            result = await asyncio.to_thread(result_queue.get, timeout=5.0)

            assert result["bound_core_node"] == "standalone-core"
            assert result["bound_instance_id"] == TEST_INSTANCE_ID
            assert result["node_name"] == TEST_NODE_NAME

            messenger = result["messenger"]
            assert isinstance(messenger, MessengerHandle)
            port = await messenger.messaging_port()
            assert port == router.port

            # Shut down the runner
            result["token"].cancel()
            runner_thread.join(timeout=10.0)

    assert not runner_thread.is_alive(), "Runner should have exited"
    assert error_queue.empty(), f"Runner error: {error_queue.get_nowait()}"


EVENT_LOOP_THREAD_NAME = "peppy-asyncio-loop"
SHUTDOWN_REPEAT = 5


@pytest.mark.asyncio
async def test_shutdown_joins_event_loop_thread(monkeypatch):
    """run() must cancel background tasks and JOIN the asyncio event-loop
    thread before returning.

    Regression test for a shutdown SIGSEGV: the event loop runs on a daemon
    thread, and a generated emit loop drives native code every tick (pycapnp
    serialization plus a pyo3 future). A daemon thread that is not joined before
    run() returns can be killed mid-native-call during interpreter
    finalization. We assert the thread is gone the instant run() returns, and
    repeat to shake out the race. The finalization crash itself only reproduces
    across a process exit and is covered by
    `test_process_exit_with_pending_service_await` below.
    """
    monkeypatch.delenv(RUNTIME_CONFIG_VAR_NAME, raising=False)
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        with tempfile.TemporaryDirectory() as temp_dir:
            peppy_config_path = str(Path(temp_dir) / NODE_CONFIG_FILE)
            Path(peppy_config_path).write_text(PEPPY_CONFIG)

            standalone_config = (
                StandaloneConfig()
                .with_parameters({"frequency_hz": TEST_FREQUENCY_HZ})
                .with_messaging(router.host, router.port)
                .with_instance_id(TEST_INSTANCE_ID)
                .with_node_name(TEST_NODE_NAME)
            )

            for iteration in range(SHUTDOWN_REPEAT):
                token_queue: queue.Queue = queue.Queue()
                started_queue: queue.Queue = queue.Queue()
                lingering_queue: queue.Queue = queue.Queue()
                error_queue: queue.Queue = queue.Queue()

                def run_node():
                    try:

                        async def setup_fn(_params, node_runner):
                            token = node_runner.cancellation_token()
                            token_queue.put(token)
                            messenger = node_runner.messenger()
                            core_node = node_runner.bound_core_node()
                            instance_id = node_runner.bound_instance_id()
                            node_name = node_runner.node_name()
                            node_tag = node_runner.node_tag()

                            async def emit_loop():
                                started_queue.put("started")
                                publisher = await TopicMessenger.declare_publisher(
                                    messenger,
                                    core_node,
                                    instance_id,
                                    SenderTarget.node(node_name, node_tag),
                                    "regression_topic",
                                    QoSProfile.Reliable,
                                )
                                while not token.is_cancelled():
                                    await publisher.publish(b"frame")
                                    await asyncio.sleep(0.001)

                            return [asyncio.create_task(emit_loop())]

                        (
                            NodeBuilder()
                            .with_config_path(peppy_config_path)
                            .standalone(standalone_config)
                            .run(setup_fn)
                        )

                        # run() has returned: the daemon event-loop thread must
                        # already be joined, otherwise interpreter finalization
                        # could kill it mid-native-call.
                        lingering_queue.put(
                            [
                                t.name
                                for t in threading.enumerate()
                                if t.name == EVENT_LOOP_THREAD_NAME
                            ]
                        )
                    except Exception as e:
                        error_queue.put(e)

                runner_thread = threading.Thread(target=run_node, daemon=True)
                runner_thread.start()

                await asyncio.to_thread(started_queue.get, timeout=5.0)
                token = await asyncio.to_thread(token_queue.get, timeout=5.0)
                token.cancel()
                runner_thread.join(timeout=10.0)

                assert not runner_thread.is_alive(), (
                    f"Runner should have exited (iteration {iteration})"
                )
                assert error_queue.empty(), (
                    f"Runner error (iteration {iteration}): {error_queue.get_nowait()}"
                )
                lingering = await asyncio.to_thread(lingering_queue.get, timeout=5.0)
                assert lingering == [], (
                    f"event-loop thread still alive after run() returned "
                    f"(iteration {iteration}): {lingering}"
                )


@pytest.mark.asyncio
async def test_failed_async_setup_does_not_leak_event_loop_thread(monkeypatch):
    """If async setup fails AFTER the event-loop thread has started but before
    the teardown handle is returned, run() must still stop and join that daemon
    thread before raising.

    The loop thread runs native code; left unjoined it can be killed
    mid-native-call during interpreter finalization (the same SIGSEGV
    `test_shutdown_joins_event_loop_thread` guards on the success path). Such a
    failure is rare in practice (e.g. resource exhaustion submitting the setup
    coroutine or spawning the shutdown monitor), so we inject it by making
    `asyncio.run_coroutine_threadsafe` raise, which fails the setup-coroutine
    submission right after the loop thread is started.
    """
    monkeypatch.delenv(RUNTIME_CONFIG_VAR_NAME, raising=False)

    real_run_coroutine_threadsafe = asyncio.run_coroutine_threadsafe

    def boom(coro, *_args, **_kwargs):
        # Close the setup coroutine we are refusing to schedule so it does not
        # raise a "coroutine was never awaited" warning, then fail the submission.
        coro.close()
        raise RuntimeError("injected post-start setup failure")

    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        with tempfile.TemporaryDirectory() as temp_dir:
            peppy_config_path = str(Path(temp_dir) / NODE_CONFIG_FILE)
            Path(peppy_config_path).write_text(PEPPY_CONFIG)

            standalone_config = (
                StandaloneConfig()
                .with_parameters({"frequency_hz": TEST_FREQUENCY_HZ})
                .with_messaging(router.host, router.port)
                .with_instance_id(TEST_INSTANCE_ID)
                .with_node_name(TEST_NODE_NAME)
            )

            error_queue: queue.Queue = queue.Queue()
            lingering_queue: queue.Queue = queue.Queue()

            def run_node():
                async def setup_fn(_params, _node_runner):
                    # Never runs: the patched run_coroutine_threadsafe raises
                    # when start_async_setup submits this coroutine.
                    return None

                try:
                    (
                        NodeBuilder()
                        .with_config_path(peppy_config_path)
                        .standalone(standalone_config)
                        .run(setup_fn)
                    )
                except Exception as e:
                    error_queue.put(e)
                finally:
                    # Runs whether or not run() raised: the daemon event-loop
                    # thread must be gone the instant run() returns.
                    lingering_queue.put(
                        [
                            t.name
                            for t in threading.enumerate()
                            if t.name == EVENT_LOOP_THREAD_NAME
                        ]
                    )

            # Restore eagerly in finally so the router teardown below (which uses
            # asyncio) does not hit the patched function.
            asyncio.run_coroutine_threadsafe = boom
            try:
                runner_thread = threading.Thread(target=run_node, daemon=True)
                runner_thread.start()
                runner_thread.join(timeout=10.0)
            finally:
                asyncio.run_coroutine_threadsafe = real_run_coroutine_threadsafe

    assert not runner_thread.is_alive(), (
        "run() did not return after a post-start async-setup failure"
    )
    assert not error_queue.empty(), "expected run() to raise the injected failure"
    lingering = lingering_queue.get_nowait()
    assert lingering == [], (
        f"event-loop thread leaked after async setup failed post-start: {lingering}"
    )


PENDING_SERVICE_NODE_SCRIPT = '''\
import asyncio
import sys

sys.path.insert(0, sys.argv[3])  # peppygen stub package root

from peppylib import SenderTarget, ServiceMessenger
from peppylib.runtime import NodeBuilder, StandaloneConfig


async def setup(parameters, node_runner):
    token = node_runner.cancellation_token()
    messenger = node_runner.messenger()
    endpoint = await ServiceMessenger.listen(
        messenger,
        node_runner.bound_core_node(),
        node_runner.bound_instance_id(),
        SenderTarget.node(node_runner.node_name(), node_runner.node_tag()),
        "regression_pending_service",
    )

    async def await_request_forever():
        # Never receives a request: at shutdown this task is cancelled while
        # the native future is still pending, the exact shape that raced
        # interpreter finalization.
        await endpoint.handle_next_request(lambda _ctx: b"")

    async def cancel_soon():
        await asyncio.sleep(0.2)
        token.cancel()

    return [
        asyncio.create_task(await_request_forever()),
        asyncio.create_task(cancel_soon()),
    ]


def main():
    config = (
        StandaloneConfig()
        .with_parameters({"frequency_hz": 10.0})
        .with_messaging(sys.argv[1], int(sys.argv[2]))
        .with_instance_id("pending_service_instance")
        .with_node_name("test_node")
    )
    NodeBuilder().with_config_path(sys.argv[4]).standalone(config).run(setup)


if __name__ == "__main__":
    main()
'''

PENDING_AWAIT_EXIT_REPEAT = 5


@pytest.mark.asyncio
async def test_process_exit_with_pending_service_await(tmp_path):
    """A node must exit cleanly when run() returns with a service await still
    pending.

    Regression test for a shutdown SIGSEGV (exit 139): the native half of a
    pending ``handle_next_request`` lives on the pyo3-async-runtimes global
    tokio runtime, and its result delivery used to attach to the interpreter
    with no shutdown guard. A delivery scheduled by the shutdown cancellation
    could attach mid-finalization, and CPython 3.13 and older kill such a
    thread via pthread_exit, segfaulting the process. The fix gates every
    delivery attach behind run()'s shutdown (see py_future.rs). The race only
    exists across a real process exit, so the node runs as a subprocess;
    repetition shakes out the timing, though a pre-fix failure is
    probabilistic rather than guaranteed.
    """
    script_path = tmp_path / "pending_service_node.py"
    script_path.write_text(PENDING_SERVICE_NODE_SCRIPT)
    config_path = tmp_path / NODE_CONFIG_FILE
    config_path.write_text(PEPPY_CONFIG)
    peppygen_root = tmp_path / "peppygen_root"
    write_peppygen_stub(peppygen_root)

    env = dict(os.environ)
    env.pop(RUNTIME_CONFIG_VAR_NAME, None)  # force standalone mode

    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        for iteration in range(PENDING_AWAIT_EXIT_REPEAT):
            proc = await asyncio.create_subprocess_exec(
                sys.executable,
                str(script_path),
                router.host,
                str(router.port),
                str(peppygen_root),
                str(config_path),
                env=env,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            try:
                stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=30.0)
            except TimeoutError:
                proc.kill()
                await proc.communicate()
                pytest.fail(f"node subprocess hung (iteration {iteration})")
            assert proc.returncode == 0, (
                f"node subprocess died with {proc.returncode} (iteration {iteration})\n"
                f"stdout:\n{stdout.decode(errors='replace')}\n"
                f"stderr:\n{stderr.decode(errors='replace')}"
            )


@pytest.mark.asyncio
async def test_daemon_shutdown_hooks_run_lifo_with_messaging(monkeypatch):
    """on_shutdown hooks run on an in-band shutdown, in reverse registration
    order, with one hook's exception contained, a non-coroutine awaitable
    coerced onto the loop, the asyncio loop still serving coroutines, and the
    messenger still connected (the ds_lock_probe pattern: a datastore lock
    release must be able to use messaging)."""
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
                TEST_CORE_NODE,
                TEST_INSTANCE_ID,
                {"frequency_hz": TEST_FREQUENCY_HZ},
            )

            monkeypatch.setenv(RUNTIME_CONFIG_VAR_NAME, runtime_config_path)
            monkeypatch.chdir(temp_dir)

            setup_done_queue: queue.Queue = queue.Queue()
            error_queue: queue.Queue = queue.Queue()
            hook_markers: list = []

            def run_node():
                try:

                    async def setup_fn(params, node_runner):
                        token = node_runner.cancellation_token()
                        messenger = node_runner.messenger()

                        def first_hook():
                            hook_markers.append("first")

                        def second_hook_raises():
                            # The marker proves the hook ran before raising;
                            # without it, a hook that never executes would be
                            # indistinguishable from a contained failure.
                            hook_markers.append("second:raised")
                            raise RuntimeError("intentional hook failure")

                        async def third_hook():
                            # The token is already cancelled when hooks run;
                            # awaiting it proves the py_future bridge still
                            # works during the hook phase.
                            await token.cancelled()
                            port = await messenger.messaging_port()
                            hook_markers.append(f"third:port_{port != 0}")

                        def fourth_hook_returns_awaitable():
                            return NonCoroutineAwaitable(hook_markers, "fourth:awaitable")

                        node_runner.on_shutdown(first_hook)
                        node_runner.on_shutdown(second_hook_raises)
                        node_runner.on_shutdown(third_hook)
                        node_runner.on_shutdown(fourth_hook_returns_awaitable)
                        setup_done_queue.put(True)

                    NodeBuilder().run(setup_fn)
                except Exception as e:
                    error_queue.put(e)

            runner_thread = threading.Thread(target=run_node, daemon=True)
            runner_thread.start()

            await asyncio.to_thread(setup_done_queue.get, timeout=5.0)

            messenger = await MessengerHandle.from_host_port(router.host, router.port)
            await _wait_for_service(
                messenger,
                SHUTDOWN_SERVICE,
                runner_thread,
                error_queue,
            )

            await ServiceMessenger.poll(
                messenger,
                TEST_CORE_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                SenderTarget.node(TEST_NODE_NAME, TEST_NODE_TAG),
                SHUTDOWN_SERVICE,
                ProducerRef(TEST_CORE_NODE, TEST_INSTANCE_ID),
                b"shutdown",
                2.0,
            )
            runner_thread.join(timeout=10.0)

    assert not runner_thread.is_alive(), "Runner should have exited"
    assert error_queue.empty(), f"Runner error: {error_queue.get_nowait()}"
    # Reverse registration order; the raising hook runs, its error contained.
    assert hook_markers == ["fourth:awaitable", "third:port_True", "second:raised", "first"]


# A grace far larger than the join bound below: a node whose drain hook
# deadlocks blocks for the whole grace and clearly blows the join, while a node
# that drains promptly exits well within it. The gap is the discriminator, not a
# tuned timeout.
DEADLOCK_REGRESSION_GRACE_SECS = 30


@pytest.mark.asyncio
async def test_async_node_shuts_down_promptly_not_at_grace_boundary(monkeypatch):
    """An async-setup node with a live background task must drain and exit
    promptly on a cooperative shutdown, not block until the grace window ends.

    Regression test for the asyncio drain-hook deadlock: the loop-teardown hook
    awaited a `run_coroutine_threadsafe` future that the drain's final
    `event_loop.stop()` orphaned, so shutdown hung for the entire grace window
    and the daemon force-killed the node. We pin a long grace and assert the
    node exits well inside it after a real SHUTDOWN_SERVICE request. This runs
    end-to-end through the real `make_loop_teardown` (the drain hook awaits the
    drain future; `quiesce` fires the stop trigger); the asyncio mechanic is
    pinned in isolation by `test_shutdown_drain_deadlock.py`.
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
                TEST_CORE_NODE,
                TEST_INSTANCE_ID,
                {"frequency_hz": TEST_FREQUENCY_HZ},
                shutdown_grace_secs=DEADLOCK_REGRESSION_GRACE_SECS,
            )

            monkeypatch.setenv(RUNTIME_CONFIG_VAR_NAME, runtime_config_path)
            monkeypatch.chdir(temp_dir)

            setup_done_queue: queue.Queue = queue.Queue()
            error_queue: queue.Queue = queue.Queue()
            cleanup_ran = threading.Event()
            background_finally_ran = threading.Event()

            def run_node():
                try:

                    async def setup_fn(_params, node_runner):
                        async def background_loop():
                            # A long-lived task like a generated emit loop that
                            # does NOT poll the token, so the drain hook is what
                            # cancels it. Its finally must run (cancelled and
                            # gathered) while the loop is still alive, before
                            # quiesce stops the loop.
                            try:
                                while True:
                                    await asyncio.sleep(0.01)
                            finally:
                                background_finally_ran.set()

                        async def on_shutdown():
                            # Cooperative cleanup; proves the hook phase ran
                            # before the loop was torn down.
                            cleanup_ran.set()

                        node_runner.on_shutdown(on_shutdown)
                        setup_done_queue.put(True)
                        return [asyncio.create_task(background_loop())]

                    NodeBuilder().run(setup_fn)
                except Exception as e:
                    error_queue.put(e)

            runner_thread = threading.Thread(target=run_node, daemon=True)
            runner_thread.start()

            await asyncio.to_thread(setup_done_queue.get, timeout=5.0)

            messenger = await MessengerHandle.from_host_port(router.host, router.port)
            await _wait_for_service(
                messenger,
                SHUTDOWN_SERVICE,
                runner_thread,
                error_queue,
            )

            await ServiceMessenger.poll(
                messenger,
                TEST_CORE_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                SenderTarget.node(TEST_NODE_NAME, TEST_NODE_TAG),
                SHUTDOWN_SERVICE,
                ProducerRef(TEST_CORE_NODE, TEST_INSTANCE_ID),
                b"shutdown",
                2.0,
            )

            # Well below the grace: a deadlocked drain blocks the full
            # DEADLOCK_REGRESSION_GRACE_SECS and leaves the thread alive here.
            runner_thread.join(timeout=10.0)

    assert not runner_thread.is_alive(), (
        "node did not exit within 10s of a cooperative shutdown despite a "
        f"{DEADLOCK_REGRESSION_GRACE_SECS}s grace: the drain hook likely "
        "deadlocked and shutdown blocked until the grace window elapsed"
    )
    assert error_queue.empty(), f"Runner error: {error_queue.get_nowait()}"
    assert cleanup_ran.is_set(), "cooperative on_shutdown hook did not run"
    assert background_finally_ran.is_set(), (
        "the drain hook must cancel the background task and gather it (running "
        "its finally) while the loop is alive, before quiesce stops the loop"
    )


@pytest.mark.asyncio
async def test_sync_setup_shutdown_hooks_run(monkeypatch):
    """A node with synchronous setup has no persistent asyncio loop; sync
    hooks run directly and async hooks run on a one-off asyncio.run loop,
    including hooks returning non-coroutine awaitables (coerced before
    asyncio.run, which accepts only coroutines)."""
    monkeypatch.delenv(RUNTIME_CONFIG_VAR_NAME, raising=False)
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        with tempfile.TemporaryDirectory() as temp_dir:
            peppy_config_path = str(Path(temp_dir) / NODE_CONFIG_FILE)
            Path(peppy_config_path).write_text(PEPPY_CONFIG)

            standalone_config = (
                StandaloneConfig()
                .with_parameters({"frequency_hz": TEST_FREQUENCY_HZ})
                .with_messaging(router.host, router.port)
                .with_instance_id(TEST_INSTANCE_ID)
            )

            token_queue: queue.Queue = queue.Queue()
            error_queue: queue.Queue = queue.Queue()
            hook_markers: list = []

            def run_node():
                try:

                    def setup_fn(params, node_runner):
                        def sync_hook():
                            hook_markers.append("sync")

                        async def async_hook():
                            await asyncio.sleep(0.05)
                            hook_markers.append("async")

                        def awaitable_hook():
                            return NonCoroutineAwaitable(hook_markers, "awaitable")

                        node_runner.on_shutdown(sync_hook)
                        node_runner.on_shutdown(async_hook)
                        node_runner.on_shutdown(awaitable_hook)
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
            cancellation_token.cancel()
            runner_thread.join(timeout=10.0)

    assert not runner_thread.is_alive(), "Runner should have exited"
    assert error_queue.empty(), f"Runner error: {error_queue.get_nowait()}"
    # Reverse registration order: the hook registered last runs first.
    assert hook_markers == ["awaitable", "async", "sync"]

"""
Tests for peppylib NodeBuilder runner lifecycle.

Python equivalent of crates/peppylib/tests/runner.rs.
"""

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
from peppylib.runtime import (
    CancellationToken,
    NodeBuilder,
    NodeRunner,
    SpawnedAsyncTask,
    StandaloneConfig,
)

from common import (
    PEPPY_CONFIG,
    TEST_FREQUENCY_HZ,
    TEST_INSTANCE_ID,
    TEST_NODE_NAME,
    create_codegen_fingerprint,
    create_runtime_config,
    wait_for_service,
)

TEST_DAEMON_NODE = "test_daemon"
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
        TEST_DAEMON_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        TEST_NODE_NAME,
        TEST_DAEMON_NODE,
        TEST_INSTANCE_ID,
        runner_thread,
        error_queue,
        timeout_secs,
    )


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
                TEST_DAEMON_NODE,
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
                TEST_DAEMON_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                TEST_NODE_NAME,
                NODE_HEALTH_SERVICE,
                TEST_DAEMON_NODE,
                TEST_INSTANCE_ID,
                b"health",
                2.0,
            )
            assert health_response is not None

            # Send shutdown
            shutdown_response = await ServiceMessenger.poll(
                messenger,
                TEST_DAEMON_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                TEST_NODE_NAME,
                SHUTDOWN_SERVICE,
                TEST_DAEMON_NODE,
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
                .with_parameters({"frequency_hz": TEST_FREQUENCY_HZ})
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
async def test_spawn_async_runs_background_task(monkeypatch):
    """NodeRunner.spawn_async starts an async task and runner exits on cancellation."""
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

                    def setup_fn(_params, node_runner):
                        token_queue.put(node_runner.cancellation_token())

                        async def background_task():
                            started_queue.put("started")
                            while not node_runner.cancellation_token().is_cancelled():
                                await asyncio.sleep(0.05)

                        node_runner.spawn_async("background-task", background_task)

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
async def test_spawn_async_returns_task_handle_and_failure(monkeypatch):
    """spawn_async returns a handle and propagates uncaught task errors to excepthook."""
    monkeypatch.delenv(RUNTIME_CONFIG_VAR_NAME, raising=False)
    hook_queue: queue.Queue = queue.Queue()
    original_excepthook = threading.excepthook

    def recording_excepthook(args):
        hook_queue.put((args.exc_type, str(args.exc_value), args.thread.name))

    threading.excepthook = recording_excepthook
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        try:
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
                task_queue: queue.Queue = queue.Queue()
                error_queue: queue.Queue = queue.Queue()

                def run_node():
                    try:

                        def setup_fn(_params, node_runner):
                            token_queue.put(node_runner.cancellation_token())

                            async def failing_task():
                                await asyncio.sleep(0.01)
                                raise RuntimeError("spawned task boom")

                            task_queue.put(
                                node_runner.spawn_async(
                                    "failing-task",
                                    failing_task,
                                    cancel_on_error=False,
                                )
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

                task_handle: SpawnedAsyncTask = await asyncio.to_thread(
                    task_queue.get, timeout=5.0
                )
                assert task_handle.thread.daemon is True

                finished = await asyncio.to_thread(task_handle.join, 5.0)
                assert finished, "spawned task should complete"
                assert not task_handle.is_alive()

                traceback_text = task_handle.exception()
                assert traceback_text is not None
                assert "RuntimeError: spawned task boom" in traceback_text
                with pytest.raises(RuntimeError):
                    task_handle.raise_if_failed()

                exc_type, exc_message, thread_name = await asyncio.to_thread(
                    hook_queue.get, timeout=5.0
                )
                assert exc_type is RuntimeError
                assert "spawned task boom" in exc_message
                assert thread_name == "failing-task"

                cancellation_token: CancellationToken = await asyncio.to_thread(
                    token_queue.get, timeout=5.0
                )
                cancellation_token.cancel()
                runner_thread.join(timeout=10.0)
        finally:
            threading.excepthook = original_excepthook

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
async def test_async_setup_requires_run_async(monkeypatch):
    """NodeBuilder.run rejects async setup callbacks and points to run_async."""
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

                    async def setup_fn(_params, _node_runner):
                        await asyncio.sleep(0)

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
            assert "run_async" in str(error)
            runner_thread.join(timeout=10.0)

    assert not runner_thread.is_alive(), "Runner should have exited"


@pytest.mark.asyncio
async def test_run_async_accepts_async_setup(monkeypatch):
    """NodeBuilder.run_async supports async setup callbacks."""
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
                        assert params["frequency_hz"] == TEST_FREQUENCY_HZ
                        await asyncio.sleep(0.01)
                        token_queue.put(node_runner.cancellation_token())

                    (
                        NodeBuilder()
                        .with_config_path(peppy_config_path)
                        .standalone(standalone_config)
                        .run_async(setup_fn)
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
                TEST_DAEMON_NODE,
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
                TEST_DAEMON_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                TEST_NODE_NAME,
                NODE_READY_SERVICE,
                TEST_DAEMON_NODE,
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
                TEST_DAEMON_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                TEST_NODE_NAME,
                NODE_HEALTH_SERVICE,
                TEST_DAEMON_NODE,
                TEST_INSTANCE_ID,
            )
            assert not health_reachable, (
                "Health service should not be reachable while setup is blocked"
            )

            # Polling health should fail (service is unreachable)
            with pytest.raises(ConnectionError):
                await ServiceMessenger.poll(
                    messenger,
                    TEST_DAEMON_NODE,
                    SHUTDOWN_SENDER_INSTANCE_ID,
                    TEST_NODE_NAME,
                    NODE_HEALTH_SERVICE,
                    TEST_DAEMON_NODE,
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
                TEST_DAEMON_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                TEST_NODE_NAME,
                NODE_HEALTH_SERVICE,
                TEST_DAEMON_NODE,
                TEST_INSTANCE_ID,
                b"health",
                2.0,
            )
            assert health_response is not None

            # Send shutdown
            shutdown_response = await ServiceMessenger.poll(
                messenger,
                TEST_DAEMON_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                TEST_NODE_NAME,
                SHUTDOWN_SERVICE,
                TEST_DAEMON_NODE,
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
                TEST_DAEMON_NODE,
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
                TEST_DAEMON_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                TEST_NODE_NAME,
                SHUTDOWN_SERVICE,
                TEST_DAEMON_NODE,
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


@pytest.mark.asyncio
async def test_node_runner_exposes_messenger_and_metadata(monkeypatch):
    """NodeRunner exposes messenger(), bound_daemon_node(), bound_instance_id(), node_name()."""
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
                                "bound_daemon_node": node_runner.bound_daemon_node(),
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

            assert result["bound_daemon_node"] == "standalone-daemon"
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

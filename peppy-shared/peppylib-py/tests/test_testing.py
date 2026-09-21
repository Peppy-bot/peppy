"""Suite for ``peppylib.testing`` — the Python twin of the Rust cores behind
generated per-node mock/fixtures code (mirrors
``peppylib-rs/tests/testing.rs`` case for case, so a semantic difference
between the two languages surfaces here, not in node suites)."""

import asyncio
import gc
from pathlib import Path

import pytest

import peppylib.testing as peppy_testing
from peppylib import (
    ActionMessenger,
    PeerInfo,
    ProducerRef,
    QoSProfile,
    SenderTarget,
    ServiceMessenger,
    StandaloneConfig,
    TopicMessenger,
)
from peppylib.testing import (
    MOCK_CLOCK_INSTANCE_ID,
    STANDALONE_CORE_NODE,
    EphemeralRouter,
    HarnessCore,
    MockActionServerCore,
    MockClock,
    MockServiceCore,
    PublisherReadiness,
    TestTopicPublisher,
    wait_action_reachable,
    wait_service_reachable,
)

MOCK_CORE = "mock_core"
MOCK_INSTANCE = "mock_1"
CALLER_CORE = "caller_core"
CALLER_INSTANCE = "caller_1"

_PEPPY_CONFIG = """{
    peppy_schema: "node/v1",
    manifest: { name: "test_node", tag: "v1" },
    execution: { language: "python", run_cmd: ["uv", "run"] },
}"""


def _node_target(name: str) -> SenderTarget:
    return SenderTarget.node(name, "v1")


def _write_peppy_config(tmp_path: Path) -> Path:
    path = tmp_path / "peppy.json5"
    path.write_text(_PEPPY_CONFIG)
    return path


@pytest.mark.asyncio
async def test_mock_service_core_scripted_then_manual():
    """Scripted responses serve automatically in FIFO order; unscripted
    requests park for ``next_request``; every request is captured."""
    async with await EphemeralRouter.start() as router:
        mock_handle = await router.connect()
        caller_handle = await router.connect()

        mock = await MockServiceCore.listen(
            mock_handle, MOCK_CORE, MOCK_INSTANCE, _node_target("dep_node"), "get_info"
        )
        try:
            mock.enqueue_response(b"scripted-response")
            producer = ProducerRef(MOCK_CORE, MOCK_INSTANCE)
            # Cold-start gate: the caller session is fresh, so its first query
            # can race gossip discovery of the mock's queryable.
            await wait_service_reachable(
                caller_handle,
                CALLER_CORE,
                CALLER_INSTANCE,
                _node_target("dep_node"),
                "get_info",
                producer,
            )

            scripted = await ServiceMessenger.poll(
                caller_handle,
                CALLER_CORE,
                CALLER_INSTANCE,
                _node_target("dep_node"),
                "get_info",
                producer,
                b"request-1",
                5.0,
            )
            assert scripted.payload == b"scripted-response"

            # Manual path: the caller's poll parks until the test receives
            # the request, asserts on it, and responds.
            manual_poll = asyncio.ensure_future(
                ServiceMessenger.poll(
                    caller_handle,
                    CALLER_CORE,
                    CALLER_INSTANCE,
                    _node_target("dep_node"),
                    "get_info",
                    producer,
                    b"request-2",
                    5.0,
                )
            )
            context, responder = await mock.next_request(timeout=5.0)
            assert context.payload == b"request-2"
            assert context.core_node == CALLER_CORE
            assert context.instance_id == CALLER_INSTANCE
            await responder.respond(b"manual-response")

            manual = await asyncio.wait_for(manual_poll, timeout=5.0)
            assert manual.payload == b"manual-response"

            captured = mock.captured()
            assert [c.payload for c in captured] == [b"request-1", b"request-2"]
            assert captured[0].core_node == CALLER_CORE
        finally:
            await mock.close()


@pytest.mark.asyncio
async def test_mock_action_stop_yields_producer_gone_deterministically():
    """``stop()`` on a mock action with a live, user-held goal context must
    surface to the consumer as the producer-gone ``ConnectionError`` — never
    a clean feedback close and never a hang — because the disarmed context
    suppresses the drop-time sentinel that would race the liveliness latch."""
    async with await EphemeralRouter.start() as router:
        mock_handle = await router.connect()
        client_handle = await router.connect()

        mock = await MockActionServerCore.expose(
            mock_handle, MOCK_CORE, MOCK_INSTANCE, _node_target("arm_node"), "move_arm", True
        )

        producer = ProducerRef(MOCK_CORE, MOCK_INSTANCE)
        await wait_action_reachable(
            client_handle,
            CALLER_CORE,
            CALLER_INSTANCE,
            _node_target("arm_node"),
            "move_arm",
            producer,
        )

        # send_goal resolves only once the mock answers admission, so it runs
        # concurrently with next_goal → accept below.
        goal_task = asyncio.ensure_future(
            ActionMessenger.send_goal(
                client_handle,
                CALLER_CORE,
                CALLER_INSTANCE,
                _node_target("arm_node"),
                "move_arm",
                producer,
                b"goal",
                QoSProfile.Reliable,
                5.0,
            )
        )

        pending = await mock.next_goal(timeout=5.0)
        assert pending.request_bytes == b"goal"
        context = await pending.accept(b"accepted")
        goal_handle = await asyncio.wait_for(goal_task, timeout=5.0)
        assert goal_handle.accepted

        await context.publish_feedback(b"working")
        feedback = await asyncio.wait_for(goal_handle.on_next_feedback(), timeout=5.0)
        assert feedback.payload == b"working"

        # Stop the mock mid-goal with the context still held (as a test's
        # MockGoal handle would be) and tear down its session — the
        # producer-loss shape.
        mock.stop()
        del mock, mock_handle
        gc.collect()

        with pytest.raises(ConnectionError):
            await asyncio.wait_for(goal_handle.on_next_feedback(), timeout=15.0)

        # Releasing the disarmed context afterwards is inert: no late
        # sentinel (nothing to receive it anyway — the session is gone).
        del context
        gc.collect()


@pytest.mark.asyncio
async def test_topic_publisher_first_publish_is_delivered():
    """With the subscriber already up, the very first publish must be
    delivered (no sleeps anywhere); with no subscriber, the publish fails
    loudly instead of dropping silently."""
    async with await EphemeralRouter.start() as router:
        sub_handle = await router.connect()
        pub_handle = await router.connect()

        producer = ProducerRef(MOCK_CORE, MOCK_INSTANCE)
        subscription = await TopicMessenger.subscribe(
            sub_handle,
            CALLER_CORE,
            CALLER_INSTANCE,
            _node_target("camera"),
            "video_stream",
            producer,
            QoSProfile.Reliable,
        )

        publisher = await TestTopicPublisher.declare(
            pub_handle,
            MOCK_CORE,
            MOCK_INSTANCE,
            _node_target("camera"),
            "video_stream",
            QoSProfile.Reliable,
        )
        await publisher.publish(b"frame-1")

        received = await asyncio.wait_for(subscription.on_next_message(), timeout=5.0)
        assert received is not None
        assert received.payload == b"frame-1"

        orphan = await TestTopicPublisher.declare(
            pub_handle,
            MOCK_CORE,
            MOCK_INSTANCE,
            _node_target("camera"),
            "nobody_listens",
            QoSProfile.Reliable,
            readiness_timeout=0.25,
        )
        with pytest.raises(RuntimeError, match="nobody_listens"):
            await orphan.publish(b"lost")


@pytest.mark.asyncio
async def test_pairing_publisher_to_a_peer_reaches_its_pinned_subscriber():
    """A mock playing one end of a pair publishes to the node's slot, and
    the node's pinned subscription on that slot is what the first publish
    waits for."""
    async with await EphemeralRouter.start() as router:
        node_handle = await router.connect()
        mock_handle = await router.connect()
        pairing = SenderTarget.pairing("arm_link", "v1")

        mock = ProducerRef(MOCK_CORE, MOCK_INSTANCE)
        subscription = await TopicMessenger.subscribe_peer_pinned(
            node_handle,
            CALLER_CORE,
            CALLER_INSTANCE,
            "arm",
            pairing,
            mock,
            "controller",
            "joint_commands",
            QoSProfile.Reliable,
        )

        publisher = await TestTopicPublisher.declare_to_peer(
            mock_handle,
            MOCK_CORE,
            MOCK_INSTANCE,
            pairing,
            "controller",
            "joint_commands",
            QoSProfile.Reliable,
            PeerInfo(ProducerRef(CALLER_CORE, CALLER_INSTANCE), "arm"),
        )
        await publisher.publish(b"cmd-1")

        received = await asyncio.wait_for(subscription.on_next_message(), timeout=5.0)
        assert received is not None
        assert received.payload == b"cmd-1"


@pytest.mark.asyncio
async def test_harness_core_boots_node_observes_first_publish_and_converges(tmp_path):
    """Full harness lifecycle: readiness barrier → setup spawn → the node's
    very first publish is observed → shutdown convergence runs the
    registered shutdown hooks."""
    async with await EphemeralRouter.start() as router:
        observer_handle = await router.connect()
        peppy_config_path = _write_peppy_config(tmp_path)
        instance_id = "harness_test_instance"

        # The harness-side observation subscription exists before the node
        # does; the barrier below guarantees the node's session discovered it
        # before setup's first publish.
        status_sub = await TopicMessenger.subscribe(
            observer_handle,
            CALLER_CORE,
            CALLER_INSTANCE,
            _node_target("test_node"),
            "status",
            ProducerRef("standalone-core", instance_id),
            QoSProfile.Reliable,
        )

        standalone_config = (
            StandaloneConfig()
            .with_messaging(router.host, router.port)
            .with_instance_id(instance_id)
        )
        readiness = [PublisherReadiness(target=_node_target("test_node"), topic="status")]

        hook_ran = asyncio.Event()
        async_hook_finished = asyncio.Event()

        async def setup(params, node_runner):
            assert params is None
            publisher = await TopicMessenger.declare_publisher(
                node_runner.messenger(),
                node_runner.bound_core_node(),
                node_runner.bound_instance_id(),
                SenderTarget.node(node_runner.node_name(), node_runner.node_tag()),
                "status",
                QoSProfile.Reliable,
            )
            # First publish, immediately: only the pre-setup barrier makes
            # this deliverable.
            await publisher.publish(b"alive")
            node_runner.on_shutdown(hook_ran.set)

            # A worker task on the harness loop plus an async hook awaiting it:
            # the production shape of a decoder/pump teardown hook. Regression:
            # the hook coroutine must run on the loop setup ran on (where this
            # task lives), not on a one-off `asyncio.run` loop.
            stop = asyncio.Event()

            async def worker():
                await stop.wait()

            worker_task = asyncio.create_task(worker())

            async def stop_worker():
                stop.set()
                await worker_task
                async_hook_finished.set()

            node_runner.on_shutdown(stop_worker)

        # A mock service declared before the node exists: the reachability
        # half of the barrier must make it visible to the node's session
        # pre-setup (mirrors the Rust harness test).
        mock_handle = await router.connect()
        mock_service = await MockServiceCore.listen(
            mock_handle, MOCK_CORE, MOCK_INSTANCE, _node_target("dep_node"), "get_info"
        )
        service_readiness = [
            peppy_testing.ServiceReadiness(
                target=_node_target("dep_node"),
                name="get_info",
                producer=ProducerRef(MOCK_CORE, MOCK_INSTANCE),
            )
        ]

        harness = await HarnessCore.start(
            peppy_config_path,
            standalone_config,
            readiness,
            setup,
            service_readiness=service_readiness,
        )
        assert harness.instance_id() == instance_id
        assert harness.bound_core_node() == "standalone-core"

        first = await asyncio.wait_for(status_sub.on_next_message(), timeout=5.0)
        assert first is not None
        assert first.payload == b"alive"

        await harness.shutdown()
        assert hook_ran.is_set(), "shutdown() must run the node's registered shutdown hooks"
        assert async_hook_finished.is_set(), (
            "async shutdown hooks must run on the setup loop, where setup's tasks live"
        )
        await mock_service.close()


@pytest.mark.asyncio
async def test_harness_core_shutdown_propagates_setup_error(tmp_path):
    """A setup error is not swallowed by teardown: ``shutdown()`` raises it
    so the test fails even when its assertions never noticed."""
    async with await EphemeralRouter.start() as router:
        peppy_config_path = _write_peppy_config(tmp_path)
        standalone_config = (
            StandaloneConfig()
            .with_messaging(router.host, router.port)
            .with_instance_id("failing_setup_instance")
        )

        async def setup(params, node_runner):
            raise RuntimeError("setup boom")

        harness = await HarnessCore.start(peppy_config_path, standalone_config, [], setup)
        with pytest.raises(RuntimeError, match="setup boom"):
            await harness.shutdown()


async def _standalone_node_runner(router, tmp_path, clock_binding):
    """A standalone ``NodeRunner`` against ``router``, exactly what a
    harness-booted node's runtime looks like to ``peppylib.clock``."""
    from peppylib import NodeRunner

    peppy_config_path = _write_peppy_config(tmp_path)
    standalone_config = (
        StandaloneConfig()
        .with_messaging(router.host, router.port)
        .with_instance_id(CALLER_INSTANCE)
        .with_clock(clock_binding)
    )
    return await NodeRunner.new_standalone(str(peppy_config_path), standalone_config)


async def _wait_clock_reachable(node_runner, clock: MockClock) -> None:
    """Gates the node's session on the mock clock's queryable, the exact
    entry the generated harness feeds its pre-setup barrier."""
    readiness = clock.readiness()
    await wait_service_reachable(
        node_runner.messenger(),
        node_runner.bound_core_node(),
        node_runner.bound_instance_id(),
        readiness.target,
        readiness.name,
        readiness.producer,
    )


@pytest.mark.asyncio
async def test_mock_clock_wall_serves_synchronize_ticks_and_scripted_skew(tmp_path):
    """A wall-mode mock clock is a wall-mode daemon to the node:
    ``synchronize`` completes the NTP exchange, the ``clock`` topic ticks by
    itself, and a scripted skew shows up in both, so offset-handling code is
    testable without touching a host clock."""
    import time

    from peppylib import clock as peppy_clock

    async with await EphemeralRouter.start() as router:
        clock_handle = await router.connect()
        clock = await MockClock.start(
            clock_handle, STANDALONE_CORE_NODE, MOCK_CLOCK_INSTANCE_ID, "wall"
        )
        node_runner = await _standalone_node_runner(router, tmp_path, clock.binding())
        try:
            await _wait_clock_reachable(node_runner, clock)

            sync = await peppy_clock.synchronize(node_runner, response_timeout_secs=5.0)
            assert sync.raw.server_recv_time <= sync.raw.server_send_time, (
                "t1 must be stamped before t2"
            )

            # Script the daemon's clock an hour ahead. The measured offset
            # must land near it: the slack prices a full round trip plus
            # scheduling noise, four orders of magnitude below the skew, so
            # the assertion cannot flake on a slow host.
            hour_ns = 3_600_000_000_000
            clock.set_offset_ns(hour_ns)
            skewed = await peppy_clock.synchronize(node_runner, response_timeout_secs=5.0)
            assert abs(skewed.offset_ns - hour_ns) < hour_ns / 2, (
                f"expected ~1h offset, got {skewed.offset_ns} ns"
            )

            # The periodic tick publisher carries the same skewed source.
            subscription = await peppy_clock.subscribe(node_runner)
            tick = await asyncio.wait_for(subscription.on_next_tick(), timeout=10.0)
            assert tick is not None, "subscription should be open"
            assert tick.time > time.time_ns() + hour_ns / 2, (
                f"tick {tick.time} should carry the scripted skew"
            )

            # Driving a domain at a wall-time node is a test bug surfaced
            # loudly.
            with pytest.raises(RuntimeError, match="consumer"):
                await clock.tick(42)
        finally:
            await clock.close()
            del node_runner
            gc.collect()


@pytest.mark.asyncio
async def test_mock_clock_consumer_drives_the_nodes_own_clock(tmp_path):
    """A node booted onto the harness's simulated domain reads nothing until
    the test supplies an instant, then reads exactly what it supplied. The
    ``clock`` service keeps answering from wall time, which is what a daemon
    serves whatever its instances read."""
    from peppylib import clock as peppy_clock

    async with await EphemeralRouter.start() as router:
        clock_handle = await router.connect()
        clock = await MockClock.start(
            clock_handle, STANDALONE_CORE_NODE, MOCK_CLOCK_INSTANCE_ID, "consumer"
        )
        node_runner = await _standalone_node_runner(router, tmp_path, clock.binding())
        try:
            await _wait_clock_reachable(node_runner, clock)

            # The daemon's clock service serves wall time in every domain, so
            # this answers before any tick.
            sync = await peppy_clock.synchronize(node_runner, response_timeout_secs=5.0)
            assert sync.raw.server_recv_time > 0

            node_clock = await peppy_clock.for_node(node_runner)
            with pytest.raises(RuntimeError):
                node_clock.now_ns()

            sim_ns = 42_000_000_000
            await clock.tick(sim_ns)

            # Wait on observation, not on a fixed delay.
            deadline = asyncio.get_running_loop().time() + 10.0
            while True:
                try:
                    assert node_clock.now_ns() == sim_ns
                    break
                except RuntimeError:
                    # `from None`: "clock not ready" is the expected state
                    # while waiting, so chaining it onto the deadline failure
                    # only buries the message that matters.
                    if asyncio.get_running_loop().time() >= deadline:
                        raise AssertionError("the tick never reached PeppyClock") from None
                    await asyncio.sleep(0.01)

            # `0` is the wire's not-ready sentinel; ticking it stores the
            # clamped 1.
            await clock.tick(0)
        finally:
            await clock.close()
            del node_runner
            gc.collect()


@pytest.mark.asyncio
async def test_mock_clock_publisher_leaves_the_domain_to_the_node(tmp_path):
    """A node booted as its domain's publisher supplies the time itself: the
    harness publishes nothing, and driving the domain from the test is
    refused."""
    from peppylib import clock as peppy_clock

    async with await EphemeralRouter.start() as router:
        clock_handle = await router.connect()
        clock = await MockClock.start(
            clock_handle, STANDALONE_CORE_NODE, MOCK_CLOCK_INSTANCE_ID, "publisher"
        )
        node_runner = await _standalone_node_runner(router, tmp_path, clock.binding())
        try:
            await _wait_clock_reachable(node_runner, clock)

            publisher = await peppy_clock.ClockPublisher.for_node(node_runner)
            assert publisher is not None, "the harness named this node the publisher"

            sim_ns = 5_000_000_000
            await publisher.publish(sim_ns)

            node_clock = await peppy_clock.for_node(node_runner)
            assert node_clock.now_ns() == sim_ns, "a source reads what it committed"

            with pytest.raises(RuntimeError, match="consumer"):
                await clock.tick(1)
        finally:
            await clock.close()
            del node_runner
            gc.collect()


@pytest.mark.asyncio
async def test_prepare_test_process_sets_zenoh_runtime():
    """Importing peppylib.testing configures the zenoh Net runtime workers
    unless the operator already chose a value."""
    import os

    assert os.environ.get("ZENOH_RUNTIME"), (
        "peppylib.testing must ensure ZENOH_RUNTIME is set before the native "
        "runtime initializes"
    )
    # Module import already ran; calling again is idempotent.
    peppy_testing.prepare_test_process()


# --- resolve_node_dir / Mocks ------------------------------------------------
#
# Both are Python-only members of `peppylib.testing` (the Rust harness resolves
# its config path at compile time and drops its mock struct), so unlike every
# case above these have no counterpart in `peppylib-rs/tests/testing.rs`. They
# used to be re-emitted into every generated harness, where the only thing that
# could exercise them was a full node sync.


def test_resolve_node_dir_prefers_the_explicit_argument(tmp_path):
    explicit = tmp_path / "explicit"
    explicit.mkdir()
    (explicit / "peppy.json5").write_text("{}")
    stale = str(tmp_path / "stale")

    resolved = peppy_testing.resolve_node_dir(explicit, stale, "peppy.json5")

    assert resolved == str(explicit.resolve())


def test_resolve_node_dir_rejects_an_explicit_dir_without_the_config(tmp_path):
    empty = tmp_path / "empty"
    empty.mkdir()

    with pytest.raises(RuntimeError) as excinfo:
        peppy_testing.resolve_node_dir(empty, str(tmp_path), "peppy.json5")

    # Names the directory it was handed, so the caller can see what it passed.
    assert str(empty) in str(excinfo.value)
    assert "peppy.json5" in str(excinfo.value)


def test_resolve_node_dir_walks_up_from_the_working_directory(tmp_path, monkeypatch):
    node = tmp_path / "node"
    nested = node / "tests" / "deep"
    nested.mkdir(parents=True)
    (node / "peppy.json5").write_text("{}")
    monkeypatch.chdir(nested)

    resolved = peppy_testing.resolve_node_dir(None, str(tmp_path / "gone"), "peppy.json5")

    assert Path(resolved).resolve() == node.resolve()


def test_resolve_node_dir_falls_back_to_the_sync_time_path(tmp_path, monkeypatch):
    node = tmp_path / "node"
    node.mkdir()
    (node / "peppy.json5").write_text("{}")
    # An unrelated tree with no config anywhere above it, so the walk fails.
    elsewhere = tmp_path / "elsewhere"
    elsewhere.mkdir()
    monkeypatch.chdir(elsewhere)
    monkeypatch.setattr(peppy_testing.os, "getcwd", lambda: str(elsewhere))
    monkeypatch.setattr(
        peppy_testing.os.path,
        "isfile",
        lambda path: Path(path).resolve() == (node / "peppy.json5").resolve(),
    )

    resolved = peppy_testing.resolve_node_dir(None, str(node), "peppy.json5")

    assert resolved == str(node)


def test_resolve_node_dir_names_all_three_sources_when_none_resolve(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(peppy_testing.os.path, "isfile", lambda path: False)

    with pytest.raises(RuntimeError) as excinfo:
        peppy_testing.resolve_node_dir(None, "/gone/node", "peppy.json5")

    message = str(excinfo.value)
    assert "no explicit node_dir=" in message
    assert "walking up from the current" in message
    assert "/gone/node" in message


def test_resolve_node_dir_without_a_sync_time_path_still_walks_up(tmp_path, monkeypatch):
    node = tmp_path / "node"
    (node / "src").mkdir(parents=True)
    (node / "peppy.json5").write_text("{}")
    monkeypatch.chdir(node / "src")

    resolved = peppy_testing.resolve_node_dir(None, None, "peppy.json5")

    assert Path(resolved).resolve() == node.resolve()


def test_resolve_node_dir_without_a_sync_time_path_says_so_when_nothing_resolves(
    tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(peppy_testing.os.path, "isfile", lambda path: False)

    with pytest.raises(RuntimeError) as excinfo:
        peppy_testing.resolve_node_dir(None, None, "peppy.json5")

    message = str(excinfo.value)
    assert "no explicit node_dir=" in message
    assert "walking up from the current" in message
    assert "staged copy of the node and carries no sync-time path" in message


class _StubMock:
    def __init__(self) -> None:
        self.stopped = 0

    async def stop(self) -> None:
        self.stopped += 1


class _Group:
    def __init__(self, **members) -> None:
        self.__dict__.update(members)


@pytest.mark.asyncio
async def test_mocks_stop_all_covers_scalars_lists_and_skips_vacant_slots():
    scalar = _StubMock()
    members = [_StubMock(), _StubMock()]
    observed = _StubMock()
    mocks = peppy_testing.Mocks(
        deps=_Group(camera=scalar, camera_bank=members),
        # A vacant optional pairing slot holds None and must not be stopped.
        pairings=_Group(controller=None),
        observed=_Group(watchers=observed),
    )

    await mocks.stop_all()

    assert scalar.stopped == 1
    assert [member.stopped for member in members] == [1, 1]
    assert observed.stopped == 1


@pytest.mark.asyncio
async def test_mocks_stop_all_is_idempotent_over_an_empty_namespace():
    mocks = peppy_testing.Mocks(deps=_Group(), pairings=_Group(), observed=_Group())

    await mocks.stop_all()
    await mocks.stop_all()

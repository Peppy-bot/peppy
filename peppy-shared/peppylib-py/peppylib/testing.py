"""Node-invariant test machinery — the Python twin of Rust's ``peppylib::testing``.

Everything here is untyped (``bytes`` in, ``bytes`` out) on purpose: this module
is the single Python implementation of the semantics that generated per-node
test code (peppygen's ``mock`` / ``fixtures`` surfaces) and peppylib's own test
suites share. Generated code contributes only a typed veneer over these cores:
message-type reuse, codecs, identity constants, per-link aggregation. If a
helper here ever wants to name a generated type, that logic belongs in the
veneer instead.

Deliberately not imported by ``peppylib/__init__.py``: the wheel ships it
inert, and only test code ever imports ``peppylib.testing``.

Semantics mirror ``peppylib-rs/src/testing.rs`` — mirror, not re-design; a
behavior difference between the two is a bug here, and both sides carry
equivalent test suites in public-peppy-libs to surface one early.

Two members are deliberately Python-only, because the Rust harness gets the
same guarantee from a language affordance rather than from a helper:
:func:`resolve_node_dir` (Rust bakes the path at compile time via
``concat!(env!("CARGO_MANIFEST_DIR"), …)``, so it never searches) and
:class:`Mocks` (Rust drops its mock struct, so it needs no explicit
``stop_all``). Neither has a Rust counterpart to drift from. Anything else
appearing on one side only is a bug, not a precedent.
"""

from __future__ import annotations

import asyncio
import itertools
import os
import time
import warnings
import weakref
from collections import deque
from dataclasses import dataclass
from typing import Any, Awaitable, Callable, Sequence

from peppylib import (
    ActionMessenger,
    ConcurrentAction,
    GoalContext,
    MessengerHandle,
    NodeRunner,
    PeerInfo,
    PendingGoal,
    ProducerRef,
    QoSProfile,
    SenderTarget,
    ServiceMessenger,
    StandaloneConfig,
    TopicMessenger,
    TopicPublisher,
    ZenohdInstance,
)
from peppylib.clock import ClockRequest, ClockResponse, ClockTick
from peppylib.messaging.services import ServiceRequestContext, ServiceResponder

#: How long readiness waits (subscriber matching, reachability) may take
#: before failing loudly. Generous on purpose: every wait returns the moment
#: its condition is observed, so the bound only prices the failure path.
READINESS_TIMEOUT = 10.0

_CONNECT_RETRIES = 5
_CONNECT_RETRY_DELAY = 0.2
_REACHABILITY_POLL_INTERVAL = 0.025

#: The identity segment generated test surfaces pin the node-under-test with;
#: the Python twin of the Rust runtime's ``STANDALONE_CORE_NODE`` constant, so
#: peppygen's veneers reference it instead of embedding the literal.
STANDALONE_CORE_NODE = "standalone-core"

_INSTANCE_COUNTER = itertools.count()


def unique_test_instance_id() -> str:
    """A process-unique test instance id (``test-<pid>-<counter>``): the
    default identity for a harness-booted node when no explicit id is
    supplied. Generated harnesses call this rather than each carrying their
    own counter, so ids from different nodes in one process cannot collide."""
    return f"test-{os.getpid()}-{next(_INSTANCE_COUNTER)}"


def resolve_node_dir(
    node_dir: str | os.PathLike[str] | None,
    sync_time_node_dir: str | None,
    config_file: str,
) -> str:
    """The node directory holding ``config_file``, resolved from three
    sources in order: the explicit ``node_dir`` argument, else the nearest
    ``config_file`` walking up from the current working directory, else
    ``sync_time_node_dir`` (the absolute path baked into the generated
    harness at sync time). A harness generated into a staged copy of the
    node carries ``None`` there: the copy is gone once the build is over,
    so only the first two sources apply.

    Python-only: the Rust harness resolves the same path at compile time from
    ``CARGO_MANIFEST_DIR`` and never searches, so there is nothing here to
    mirror. The generated veneer supplies the two per-node values; the search
    itself is identical for every node, which is why it lives here rather
    than being re-emitted into each one.

    Raises ``RuntimeError`` naming every source it tried, so a harness that
    cannot find its node says which of the three fixes applies."""
    if node_dir is not None:
        candidate = os.path.abspath(os.fspath(node_dir))
        if os.path.isfile(os.path.join(candidate, config_file)):
            return candidate
        raise RuntimeError(
            f"node_dir {candidate!r} does not contain {config_file}; pass "
            f"the directory that holds the node's {config_file}"
        )
    current = os.getcwd()
    while True:
        if os.path.isfile(os.path.join(current, config_file)):
            return current
        parent = os.path.dirname(current)
        if parent == current:
            break
        current = parent
    if sync_time_node_dir is not None and os.path.isfile(
        os.path.join(sync_time_node_dir, config_file)
    ):
        return sync_time_node_dir
    if sync_time_node_dir is None:
        sync_time_clause = (
            "(3) this harness was generated into a staged copy of the node "
            "and carries no sync-time path"
        )
    else:
        sync_time_clause = (
            f"(3) the sync-time path {sync_time_node_dir!r} no longer holds "
            "one, so the node has moved since generation; re-run peppy node "
            "sync"
        )
    raise RuntimeError(
        f"could not locate the node's {config_file} from any source: "
        "(1) no explicit node_dir= was passed to start(), so pass the node "
        "directory explicitly; "
        f"(2) no {config_file} was found walking up from the current "
        f"working directory {os.getcwd()!r}, so run the tests from inside the "
        f"node directory; {sync_time_clause}"
    )


class Mocks:
    """Every started mock, grouped by namespace (``deps`` / ``pairings`` /
    ``observed``), one attribute per link. A test can consume an individual
    mock (``await mock.stop()`` is producer-loss) without giving up the
    harness.

    The three groups are the generated per-node namespace objects; this class
    only aggregates them and knows how to tear the whole set down. Python-only
    for that last reason: Rust's ``Mocks`` is a plain struct whose fields drop,
    so it needs no explicit stop pass."""

    def __init__(self, deps: Any, pairings: Any, observed: Any) -> None:
        self.deps = deps
        self.pairings = pairings
        self.observed = observed

    async def stop_all(self) -> None:
        """Stops every mock in every group, multi-instance slots included.
        Each mock's own ``stop()`` is idempotent, so calling this after
        stopping some by hand is safe; the harness calls it on shutdown."""
        for group in (self.deps, self.pairings, self.observed):
            for value in vars(group).values():
                if value is None:
                    continue
                if isinstance(value, list):
                    for mock in value:
                        await mock.stop()
                else:
                    await value.stop()


def prepare_test_process() -> None:
    """One-time test-process environment setup; idempotent.

    - Gives zenoh's global Net runtime more worker threads (stock zenoh 1.9.0
      through at least 1.10.0 can deadlock its routing layer under
      peer-session churn on the single-worker default; an operator-provided
      ``ZENOH_RUNTIME`` wins). Must run before the first session or router
      lazily initializes the native runtime — importing this module (or
      calling :meth:`EphemeralRouter.start`) is early enough for test code.
    - Raises the soft ``nofile`` limit toward the hard limit, best effort:
      parallel routers/sessions can exhaust the macOS default soft limit of
      256, surfacing as flaky EMFILE errors.
    """
    os.environ.setdefault("ZENOH_RUNTIME", "(net: (worker_threads: 4))")
    try:
        import resource

        soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
        ceiling = 8192 if hard == resource.RLIM_INFINITY else min(8192, hard)
        if soft < ceiling:
            resource.setrlimit(resource.RLIMIT_NOFILE, (ceiling, hard))
    except (ImportError, ValueError, OSError):
        pass


# Run at import time so the environment is set before the native runtime can
# lazily initialize — the Python stand-in for the Rust module's pre-main ctor.
prepare_test_process()


# One mesh-serialization lock per event loop (pytest-asyncio gives each test
# its own loop, and asyncio primitives refuse cross-loop reuse). Sequential
# tests never overlap meshes anyway; the lock exists for tests that start
# several routers/harnesses concurrently on one loop — one mesh at a time
# keeps gossip discovery fast and deterministic.
_MESH_SERIAL_LOCKS: "weakref.WeakKeyDictionary[asyncio.AbstractEventLoop, asyncio.Lock]" = (
    weakref.WeakKeyDictionary()
)


def _mesh_serial_lock() -> asyncio.Lock:
    loop = asyncio.get_running_loop()
    lock = _MESH_SERIAL_LOCKS.get(loop)
    if lock is None:
        lock = asyncio.Lock()
        _MESH_SERIAL_LOCKS[loop] = lock
    return lock


async def acquire_mesh_serial() -> asyncio.Lock:
    """Acquire this loop's mesh-serialization lock directly, for tests that
    build their own mesh without an :class:`EphemeralRouter`. The caller must
    ``release()`` it; :meth:`EphemeralRouter.start` manages it internally."""
    lock = _mesh_serial_lock()
    await lock.acquire()
    return lock


async def connect_messenger(host: str, port: int) -> MessengerHandle:
    """Open a gossip-peer session against ``host:port``, retrying the
    first-connect races a just-ready router can still lose."""
    last_error: Exception | None = None
    for attempt in range(_CONNECT_RETRIES):
        try:
            return await MessengerHandle.from_host_port(host, port)
        except Exception as error:  # noqa: BLE001 — retried, then re-raised
            last_error = error
            if attempt + 1 < _CONNECT_RETRIES:
                await asyncio.sleep(_CONNECT_RETRY_DELAY)
    assert last_error is not None
    raise last_error


class EphemeralRouter:
    """An external zenohd on an ephemeral port, wrapped for tests: started
    with a real readiness probe (no sleeps), stopped by :meth:`shutdown` /
    ``async with``.

    :meth:`start` also holds this loop's mesh-serialization lock for the
    router's lifetime, so concurrent harnesses on one loop run one mesh at a
    time. The lock is not reentrant: a test that needs two routers at once
    must use :meth:`start_unserialized` for the second (or both) and accept
    the discovery-contention flake risk.
    """

    def __init__(self, instance: ZenohdInstance, serial: asyncio.Lock | None) -> None:
        self._instance = instance
        self._serial = serial

    @classmethod
    async def start(cls) -> "EphemeralRouter":
        return await cls.start_on("127.0.0.1", None)

    @classmethod
    async def start_on(cls, host: str, port: int | None = None) -> "EphemeralRouter":
        prepare_test_process()
        serial = await acquire_mesh_serial()
        try:
            instance = await ZenohdInstance.start_ephemeral(host, port)
        except BaseException:
            serial.release()
            raise
        return cls(instance, serial)

    @classmethod
    async def start_unserialized(
        cls, host: str = "127.0.0.1", port: int | None = None
    ) -> "EphemeralRouter":
        prepare_test_process()
        instance = await ZenohdInstance.start_ephemeral(host, port)
        return cls(instance, None)

    @property
    def host(self) -> str:
        return self._instance.host

    @property
    def port(self) -> int:
        return self._instance.port

    def connection_target(self) -> tuple[str, int]:
        return (self._instance.host, self._instance.port)

    async def connect(self) -> MessengerHandle:
        """A fresh gossip-peer session against this router, with retries."""
        return await connect_messenger(self.host, self.port)

    async def shutdown(self) -> None:
        try:
            await self._instance.stop()
        finally:
            if self._serial is not None:
                self._serial.release()
                self._serial = None

    async def stop(self) -> None:
        """Alias for :meth:`shutdown`, matching ``ZenohdInstance.stop`` so the
        wrapper drops into call sites written against the raw instance."""
        await self.shutdown()

    async def __aenter__(self) -> "EphemeralRouter":
        return self

    async def __aexit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        await self.shutdown()


class TestTopicPublisher:
    """A publisher whose **first** publish deterministically waits until the
    publishing session sees a matching subscriber, then publishes.

    In gossip mode a freshly-connected publisher learns about existing
    subscribers asynchronously, so an unguarded first publish can be dropped
    before routing propagates; the wait is keyed by this publisher's exact
    wire identity (link_id segment included). Subsequent publishes skip the
    wait. No subscriber within the readiness timeout is an error, not a
    silent drop: the node under test never subscribed where the test
    publishes.
    """

    # The name starts with "Test" (mirroring the Rust core), which pytest
    # would otherwise try to collect as a test class when imported into a
    # test module.
    __test__ = False

    def __init__(
        self,
        publisher: TopicPublisher,
        messenger: MessengerHandle,
        as_core_node: str,
        as_instance_id: str,
        as_target: SenderTarget,
        link_id: str | None,
        topic: str,
        readiness_timeout: float,
        peer: PeerInfo | None = None,
    ) -> None:
        self._publisher = publisher
        self._messenger = messenger
        self._as_core_node = as_core_node
        self._as_instance_id = as_instance_id
        self._as_target = as_target
        self._link_id = link_id
        #: The peer a pairing publisher addresses; ``None`` on every other target.
        self._peer = peer
        self._topic = topic
        self._matched = False
        self.readiness_timeout = readiness_timeout

    @classmethod
    async def declare(
        cls,
        messenger: MessengerHandle,
        as_core_node: str,
        as_instance_id: str,
        as_target: SenderTarget,
        topic: str,
        qos: QoSProfile,
        link_id: str | None = None,
        readiness_timeout: float = READINESS_TIMEOUT,
    ) -> "TestTopicPublisher":
        publisher = await TopicMessenger.declare_publisher(
            messenger, as_core_node, as_instance_id, as_target, topic, qos, link_id
        )
        return cls(
            publisher,
            messenger,
            as_core_node,
            as_instance_id,
            as_target,
            link_id,
            topic,
            readiness_timeout,
        )

    @classmethod
    async def declare_to_peer(
        cls,
        messenger: MessengerHandle,
        as_core_node: str,
        as_instance_id: str,
        as_target: SenderTarget,
        link_id: str,
        topic: str,
        qos: QoSProfile,
        peer: PeerInfo,
        readiness_timeout: float = READINESS_TIMEOUT,
    ) -> "TestTopicPublisher":
        """Declare a pairing publisher from the mock's slot ``link_id`` to
        ``peer`` (the node under test, on its slot), for a mock playing the
        other end of a pair."""
        publisher = await TopicMessenger.declare_pairing_publisher(
            messenger, as_core_node, as_instance_id, as_target, link_id, topic, qos, peer
        )
        return cls(
            publisher,
            messenger,
            as_core_node,
            as_instance_id,
            as_target,
            link_id,
            topic,
            readiness_timeout,
            peer=peer,
        )

    async def wait_for_subscriber(self, timeout: float) -> bool:
        """Wait until a subscriber matching this publisher's exact wire
        identity is visible, or ``timeout`` elapses; marks the publisher
        matched on success so later publishes skip the wait."""
        if self._peer is not None and self._link_id is not None:
            matched = await TopicMessenger.wait_for_pairing_subscriber(
                self._messenger,
                self._as_core_node,
                self._as_instance_id,
                self._as_target,
                self._link_id,
                self._topic,
                self._peer,
                timeout,
            )
        else:
            matched = await TopicMessenger.wait_for_subscriber_with_link_id(
                self._messenger,
                self._as_core_node,
                self._as_instance_id,
                self._as_target,
                self._topic,
                timeout,
                self._link_id,
            )
        if matched:
            self._matched = True
        return bool(matched)

    async def publish(self, payload: bytes) -> None:
        if not self._matched and not await self.wait_for_subscriber(self.readiness_timeout):
            raise RuntimeError(
                f"no subscriber for topic `{self._topic}` (link_id {self._link_id!r}) "
                f"appeared within {self.readiness_timeout}s: the node under test never "
                "opened a matching subscription — check that the link is seeded in the "
                "harness config and that the node subscribes to this topic"
            )
        await self._publisher.publish(payload)


async def wait_service_reachable(
    messenger: MessengerHandle,
    bound_core_node: str,
    as_instance_id: str,
    to_target: SenderTarget,
    to_service_name: str,
    producer: ProducerRef | None,
    timeout: float = READINESS_TIMEOUT,
) -> None:
    """Wait until the pinned producer's service answers reachability probes.

    The cold-start counterpart of the topic-side subscriber wait: a fresh
    session's first ``poll`` can race gossip discovery of an already-declared
    queryable, and a service query that misses is a hard unreachable error,
    so callers gate on this first.
    """
    await _wait_reachable(
        "service",
        messenger,
        bound_core_node,
        as_instance_id,
        to_target,
        to_service_name,
        producer,
        timeout,
    )


async def wait_action_reachable(
    messenger: MessengerHandle,
    bound_core_node: str,
    as_instance_id: str,
    to_target: SenderTarget,
    to_action_name: str,
    producer: ProducerRef | None,
    timeout: float = READINESS_TIMEOUT,
) -> None:
    """:func:`wait_service_reachable` for an action's goal service."""
    await _wait_reachable(
        "action",
        messenger,
        bound_core_node,
        as_instance_id,
        to_target,
        to_action_name,
        producer,
        timeout,
    )


async def _wait_reachable(
    kind: str,
    messenger: MessengerHandle,
    bound_core_node: str,
    as_instance_id: str,
    to_target: SenderTarget,
    name: str,
    producer: ProducerRef | None,
    timeout: float,
) -> None:
    """The shared probe loop behind the two ``wait_*_reachable`` helpers:
    poll the matching ``is_reachable`` probe until it answers or ``timeout``
    expires."""
    probe = ActionMessenger.is_reachable if kind == "action" else ServiceMessenger.is_reachable
    deadline = asyncio.get_running_loop().time() + timeout
    while True:
        if await probe(messenger, bound_core_node, as_instance_id, to_target, name, producer):
            return
        if asyncio.get_running_loop().time() >= deadline:
            raise TimeoutError(
                f"{kind} `{name}` on {producer!r} did not become reachable "
                f"within {timeout}s"
            )
        await asyncio.sleep(_REACHABILITY_POLL_INTERVAL)


@dataclass
class CapturedServiceRequest:
    """One request captured by a :class:`MockServiceCore`, kept for later
    assertions regardless of whether the response was scripted or manual."""

    core_node: str
    instance_id: str
    link_id: str
    payload: bytes


class MockServiceCore:
    """Server side of one mocked service: a background pump owns the endpoint
    and captures every inbound request; a request is answered from the
    scripted response queue when one is enqueued, and handed to
    :meth:`next_request` otherwise. Requests neither path consumed are
    reported loudly at :meth:`close`."""

    def __init__(self, service_name: str) -> None:
        self._service_name = service_name
        self._requests: asyncio.Queue[tuple[ServiceRequestContext, ServiceResponder]] = (
            asyncio.Queue()
        )
        self._scripted: deque[bytes] = deque()
        self._captured: list[CapturedServiceRequest] = []
        self._pump: asyncio.Task[None] | None = None

    @classmethod
    async def listen(
        cls,
        messenger: MessengerHandle,
        as_core_node: str,
        as_instance_id: str,
        as_identity: SenderTarget,
        as_service_name: str,
    ) -> "MockServiceCore":
        endpoint = await ServiceMessenger.listen(
            messenger, as_core_node, as_instance_id, as_identity, as_service_name
        )
        core = cls(as_service_name)
        core._pump = asyncio.create_task(core._run_pump(endpoint))
        return core

    async def _run_pump(self, endpoint: Any) -> None:
        while True:
            pair = await endpoint.recv_next_request()
            if pair is None:
                return
            context, responder = pair
            self._captured.append(
                CapturedServiceRequest(
                    core_node=context.core_node,
                    instance_id=context.instance_id,
                    link_id=context.link_id,
                    payload=bytes(context.payload),
                )
            )
            # Scripted responses win over manual receives: a test that
            # enqueued N responses gets them served in order as requests
            # arrive, and only unscripted requests park for next_request.
            if self._scripted:
                response = self._scripted.popleft()
                try:
                    await responder.respond(response)
                except Exception as error:  # noqa: BLE001 — mock keeps serving
                    warnings.warn(
                        f"mock service `{self._service_name}` failed to send a scripted "
                        f"response: {error}",
                        stacklevel=1,
                    )
            else:
                await self._requests.put((context, responder))

    async def next_request(
        self, timeout: float = READINESS_TIMEOUT
    ) -> tuple[ServiceRequestContext, ServiceResponder]:
        """The next unscripted request, with the responder the test must use
        to answer it. Errors after ``timeout`` — a node that never called is a
        test failure surfaced here, not a hang."""
        try:
            return await asyncio.wait_for(self._requests.get(), timeout)
        except asyncio.TimeoutError:
            raise TimeoutError(
                f"mock service `{self._service_name}` received no request within {timeout}s"
            ) from None

    def enqueue_response(self, response: bytes) -> None:
        """Enqueue one response to be served automatically to the next inbound
        request (FIFO across repeated calls)."""
        self._scripted.append(response)

    def captured(self) -> list[CapturedServiceRequest]:
        """Every request captured so far (scripted and manual alike), in
        arrival order."""
        return list(self._captured)

    async def close(self) -> None:
        """Stop the pump and report anything the test left dangling. Python
        has no reliable drop hook for async teardown, so the veneer/harness
        must call this (Rust's core does the same reporting in ``Drop``)."""
        if self._pump is not None:
            self._pump.cancel()
            try:
                await self._pump
            except asyncio.CancelledError:
                pass
            self._pump = None
        unconsumed = self._requests.qsize()
        if unconsumed:
            warnings.warn(
                f"mock service `{self._service_name}` closed with {unconsumed} unconsumed "
                "request(s): the node called this service and the test neither scripted "
                "a response nor received the request",
                stacklevel=1,
            )
        if self._scripted:
            warnings.warn(
                f"mock service `{self._service_name}` closed with {len(self._scripted)} "
                "scripted response(s) never requested by the node",
                stacklevel=1,
            )


class MockPendingGoal:
    """A goal received by a :class:`MockActionServerCore`, awaiting the
    test's admission decision."""

    def __init__(self, pending: PendingGoal, live: list[Any]) -> None:
        self._pending = pending
        self._live = live

    @property
    def goal_id(self) -> str:
        return self._pending.goal_id

    @property
    def request_bytes(self) -> bytes:
        return bytes(self._pending.request_bytes)

    async def accept(self, response: bytes) -> GoalContext:
        """Accept the goal. The returned context drives feedback/completion;
        it is registered with the owning mock so
        :meth:`MockActionServerCore.stop` can disarm it for deterministic
        producer-loss."""
        context = await self._pending.accept(response)
        self._live.append(weakref.ref(context))
        return context

    async def reject(self, reason: str | None, response: bytes) -> None:
        await self._pending.reject(reason, response)


class MockActionServerCore:
    """Server side of one mocked action, on the real ``ConcurrentAction``
    engine: the full goal lifecycle (admission ack, cancel routing, feedback
    stream, result retention) behaves exactly as a production action
    server's.

    :meth:`stop` is the deterministic producer-loss primitive: it disarms
    every live goal's close-on-drop transition (so no clean feedback-end
    sentinel races the loss signal) and releases the engine, whose liveliness
    token going absent is what consumers observe as the producer-gone error.
    The caller must also drop the mock's session for the loss to be complete;
    the generated veneer owns that ordering.
    """

    def __init__(self, action_name: str, engine: ConcurrentAction) -> None:
        self._action_name = action_name
        self._engine: ConcurrentAction | None = engine
        self._live: list[Any] = []

    @classmethod
    async def expose(
        cls,
        messenger: MessengerHandle,
        bound_core_node: str,
        as_instance_id: str,
        as_identity: SenderTarget,
        as_action_name: str,
        has_feedback: bool,
    ) -> "MockActionServerCore":
        engine = await ConcurrentAction.expose(
            messenger, bound_core_node, as_instance_id, as_identity, as_action_name, has_feedback
        )
        return cls(as_action_name, engine)

    async def next_goal(self, timeout: float = READINESS_TIMEOUT) -> MockPendingGoal:
        """Park until the node under test sends a goal, bounded by
        ``timeout``."""
        if self._engine is None:
            raise RuntimeError(f"mock action `{self._action_name}` is stopped")
        try:
            pending = await asyncio.wait_for(self._engine.recv_next_goal(), timeout)
        except asyncio.TimeoutError:
            raise TimeoutError(
                f"mock action `{self._action_name}` received no goal within {timeout}s"
            ) from None
        if pending is None:
            raise RuntimeError(
                f"mock action `{self._action_name}`'s goal stream closed unexpectedly"
            )
        return MockPendingGoal(pending, self._live)

    def stop(self) -> None:
        """Simulate this producer disappearing mid-goal, deterministically:
        every live goal context is disarmed (its eventual release emits
        neither the abandon transition nor the feedback-end sentinel), then
        the engine is released — CPython's refcounting drops it immediately,
        stopping its routing loops and removing the producer liveliness token
        consumers latch on."""
        for ref in self._live:
            context = ref()
            if context is not None:
                context.disarm_close_on_drop()
        self._live.clear()
        self._engine = None


#: The instance identity a :class:`MockClock` serves under by default. Each
#: test owns its own mesh (one router per harness), so a constant cannot
#: collide across concurrently running harnesses.
MOCK_CLOCK_INSTANCE_ID = "standalone-clock"

#: Tick cadence of a wall-mode :class:`MockClock`, in seconds: the daemon's
#: production default (10 Hz), so ``peppylib.clock.subscribe`` observes the
#: same drumbeat under the harness as under a real stack.
MOCK_CLOCK_TICK_INTERVAL = 0.1

#: Tag of a core node's own service/topic identity (core-node-api's
#: ``names::CORE_NODE_TAG``); the clock lives on the core-node target.
_CORE_NODE_TAG = "core"

#: The core node's clock service and topic names (``ServiceId::Clock`` /
#: ``TopicId::Clock`` in core-node-api).
_CLOCK_SERVICE = "clock"
_CLOCK_TOPIC = "clock"

#: The simulated domain a harness offers, named once so a test reading a log
#: or a key sees the same word the harness config uses.
HARNESS_CLOCK_DOMAIN = "harness"

#: One incarnation: a harness boots one node once, so its domain has one
#: lifetime.
HARNESS_CLOCK_INCARNATION = 1

#: The clocks a harness can boot a node on, the test-side spelling of a
#: deployment's `framework.clock` and of the publisher a domain declaration
#: names.
HARNESS_CLOCKS = ("wall", "consumer", "publisher")


class MockClock:
    """The daemon's clock surface under the harness: a ``clock`` service
    queryable answering ``peppylib.clock.synchronize``'s NTP-style exchange
    with the daemon's t1-first/t2-last stamping discipline, plus whichever
    tick stream the node's clock calls for.

    ``"wall"`` mirrors an instance reading its machine's clock: ticks are
    published automatically at :data:`MOCK_CLOCK_TICK_INTERVAL`. ``"consumer"``
    mirrors one bound to a simulated domain, with the test playing the
    simulator: nothing ticks until it calls :meth:`tick`. ``"publisher"``
    mirrors the instance a domain declaration names, and the harness publishes
    nothing at all.

    The service answers from OS wall time in every mode, skewable via
    :meth:`set_offset_ns`, because that is what a daemon serves whatever its
    instances read.
    """

    def __init__(self, core_node: str, instance_id: str, clock: str) -> None:
        self._core_node = core_node
        self._instance_id = instance_id
        self._clock = clock
        self._offset_ns = 0
        self._pump: asyncio.Task[None] | None = None
        self._ticker: asyncio.Task[None] | None = None
        self._domain_publisher: TestTopicPublisher | None = None

    @classmethod
    async def start(
        cls,
        messenger: MessengerHandle,
        core_node: str,
        instance_id: str,
        clock: str = "wall",
    ) -> "MockClock":
        """Serve the clock surface for ``core_node`` (under the harness:
        :data:`STANDALONE_CORE_NODE`) in the shape ``clock`` asks for, one of
        :data:`HARNESS_CLOCKS`."""
        if clock not in HARNESS_CLOCKS:
            raise ValueError(
                f"unknown harness clock {clock!r}; choose one of "
                f"{', '.join(sorted(HARNESS_CLOCKS))}"
            )
        self = cls(core_node, instance_id, clock)
        await self._listen(messenger)
        if clock == "wall":
            publisher = await TopicMessenger.declare_publisher(
                messenger,
                core_node,
                instance_id,
                SenderTarget.node(core_node, _CORE_NODE_TAG),
                _CLOCK_TOPIC,
                QoSProfile.SensorData,
            )
            self._ticker = asyncio.create_task(self._run_ticker(publisher))
        elif clock == "consumer":
            self._domain_publisher = await TestTopicPublisher.declare(
                messenger,
                core_node,
                instance_id,
                SenderTarget.node(core_node, _CORE_NODE_TAG),
                _CLOCK_TOPIC,
                QoSProfile.SensorData,
                link_id=self.binding().link_id,
            )
        return self

    def binding(self) -> Any:
        """The binding the node under test boots with, which the generated
        harness hands to ``StandaloneConfig.with_clock``."""
        from .clock import ClockBinding

        if self._clock == "wall":
            return ClockBinding.wall()
        if self._clock == "consumer":
            return ClockBinding.consumer(
                HARNESS_CLOCK_DOMAIN,
                self._core_node,
                HARNESS_CLOCK_INCARNATION,
                self._core_node,
                self._instance_id,
            )
        return ClockBinding.publisher(
            HARNESS_CLOCK_DOMAIN, self._core_node, HARNESS_CLOCK_INCARNATION
        )

    async def _listen(self, messenger: MessengerHandle) -> None:
        endpoint = await ServiceMessenger.listen(
            messenger,
            self._core_node,
            self._instance_id,
            SenderTarget.node(self._core_node, _CORE_NODE_TAG),
            _CLOCK_SERVICE,
        )
        self._pump = asyncio.create_task(self._run_pump(endpoint))

    def _source_now_ns(self) -> int:
        """The served timestamp: skewed OS time, in every mode. A daemon
        serves wall time whatever its instances read."""
        # Negative skews clamp at the epoch, matching the Rust core's
        # saturating arithmetic on the unsigned wire type.
        return max(0, time.time_ns() + self._offset_ns)

    async def _run_pump(self, endpoint: Any) -> None:
        while True:
            pair = await endpoint.recv_next_request()
            if pair is None:
                return
            context, responder = pair
            # Stamp t1 first: every line after this point inflates server
            # processing time and corrupts the offset estimate the client
            # computes.
            server_recv_time = self._source_now_ns()
            try:
                request = ClockRequest.decode(bytes(context.payload))
            except ValueError as error:
                await responder.respond_error(f"invalid clock request: {error}")
                continue
            # Stamp t2 last: the response encode + send happens after this
            # point and is part of the round-trip delay the client measures,
            # not server time.
            server_send_time = self._source_now_ns()
            response = ClockResponse(
                request.client_send_time, server_recv_time, server_send_time
            )
            await responder.respond(response.encode())

    async def _run_ticker(self, publisher: TopicPublisher) -> None:
        # Same loop shape as the daemon's wall-mode publisher: SensorData QoS
        # (stale time is useless) and a failed publish skips the tick rather
        # than killing the stream.
        while True:
            await asyncio.sleep(MOCK_CLOCK_TICK_INTERVAL)
            payload = ClockTick(self._source_now_ns()).encode()
            try:
                await publisher.publish(payload)
            except Exception as error:  # noqa: BLE001, mock keeps ticking
                warnings.warn(f"mock clock tick emit failed: {error}", stacklevel=1)

    async def tick(self, time_ns: int) -> None:
        """Advance the harness domain to ``time_ns`` and publish it, for a
        node booted ``"consumer"``.

        The first publish waits until the node's clock subscription is
        visible (:class:`TestTopicPublisher` semantics): ticking a domain no
        node reads is a wiring bug surfaced as a loud error, not a silent
        drop. ``0`` is the wire's not-ready sentinel and is clamped to ``1``.

        Raises ``RuntimeError`` in the other modes: a wall-time clock ticks
        itself, and a publisher node supplies its own domain.
        """
        if self._domain_publisher is None:
            raise RuntimeError(
                "tick() drives a node booted clock=\"consumer\"; this harness booted "
                f'clock="{self._clock}". A wall-time clock ticks itself, and a '
                "publisher node supplies its own domain"
            )
        await self._domain_publisher.publish(ClockTick(max(1, time_ns)).encode())

    def set_offset_ns(self, offset_ns: int) -> None:
        """Skew every timestamp the ``clock`` service answers with, by a
        signed offset from the OS clock: the scripted stand-in for a daemon
        host whose clock drifted from the node's, so offset-handling code is
        testable without touching a real clock. Also skews a wall-mode
        clock's published ticks, which come from the same source."""
        self._offset_ns = offset_ns

    def producer_ref(self) -> ProducerRef:
        """The wire identity the clock serves under, as a probe-able
        producer."""
        return ProducerRef(self._core_node, self._instance_id)

    def readiness(self) -> ServiceReadiness:
        """This clock's entry for the harness's pre-setup reachability
        barrier, so a ``synchronize`` in the node's ``setup`` cannot race
        gossip discovery of the queryable."""
        return ServiceReadiness(
            target=SenderTarget.node(self._core_node, _CORE_NODE_TAG),
            name=_CLOCK_SERVICE,
            producer=self.producer_ref(),
            kind="service",
        )

    async def close(self) -> None:
        """Stop the service pump and (in wall mode) the tick publisher.
        Python has no reliable drop hook for async teardown, so the
        veneer/harness must call this (Rust's core does the same in
        ``Drop``).

        Both tasks are cancelled before either is awaited, so the ticker
        cannot keep publishing while the pump drains. A task that ended in an
        error is reported as a warning rather than raised: ``close`` runs in a
        test's teardown, where raising would skip the rest of the cleanup and
        mask the failure the test was actually reporting.
        """
        tasks = [task for task in (self._pump, self._ticker) if task is not None]
        # Cleared up front: a task that refuses to die must not leave the
        # clock looking closeable again.
        self._pump = None
        self._ticker = None
        for task in tasks:
            task.cancel()
        for task in tasks:
            try:
                await task
            except asyncio.CancelledError:
                pass
            except Exception as error:  # noqa: BLE001 — teardown reports, never raises
                warnings.warn(f"mock clock task failed: {error}", stacklevel=1)


@dataclass
class PublisherReadiness:
    """One entry of the harness's pre-setup readiness barrier: before the
    node's ``setup`` runs, the harness waits — on the node's own session —
    until a subscriber matching the exact keyexpr the node's publisher will
    emit on is visible. Without this, the node's very first publish can be
    dropped while its fresh session is still discovering the harness's
    already-declared subscriptions (subscribe-first alone is not
    sufficient in gossip mode)."""

    target: SenderTarget
    topic: str
    #: The node's own producer-side link_id for slot-scoped publishers
    #: (pairing slots); ``None`` for plain emitted topics.
    link_id: str | None = None
    #: The mock peer a pairing publisher addresses; ``None`` for plain
    #: emitted topics.
    peer: PeerInfo | None = None


@dataclass
class ServiceReadiness:
    """One entry of the harness's pre-setup reachability barrier for mocked
    services and actions: the node under test is a *fresh caller*, so its very
    first ``poll``/``send_goal`` inside ``setup`` can race gossip discovery of
    a mock queryable that was declared long before the node's session existed.
    The harness therefore waits — on the node's own session — until each
    mock's queryable answers reachability probes, before ``setup`` runs.
    Mirrors Rust's ``ServiceReadiness``."""

    #: The identity the mock serves under (the dependency's node/contract
    #: target).
    target: SenderTarget
    #: Service or action name.
    name: str
    #: The mock's wire identity, as seeded into the node's bound set.
    producer: ProducerRef
    #: ``"service"`` for a plain service queryable, ``"action"`` for an
    #: action's goal service.
    kind: str = "service"


SetupFn = Callable[[Any, NodeRunner], Awaitable[None]]


class HarnessCore:
    """The node-invariant half of the generated test harness: builds the node
    in-process from an already-seeded ``StandaloneConfig``, runs the
    pre-setup readiness barrier, spawns the node's ``setup``, and owns
    teardown convergence. The generated veneer contributes what is per-node:
    mock construction, config seeding, parameter hydration, and typed
    observation clients.

    Teardown contract (:meth:`shutdown`): cancel the node's token → await
    ``setup`` bounded by the shutdown grace (propagating its error if it
    raised; a long-running ``setup`` that parks on the token's subscriptions
    is cancelled like production drops it) → run the registered shutdown
    hooks. Mirrors Rust's ``HarnessCore``.
    """

    def __init__(self, node_runner: NodeRunner, setup_task: asyncio.Task[None]) -> None:
        self._node_runner = node_runner
        self._setup_task: asyncio.Task[None] | None = setup_task

    @classmethod
    async def start(
        cls,
        peppy_config_path: str | os.PathLike[str],
        standalone_config: StandaloneConfig,
        publisher_readiness: Sequence[PublisherReadiness],
        setup: SetupFn,
        parameters: Any = None,
        service_readiness: Sequence[ServiceReadiness] = (),
    ) -> "HarnessCore":
        """Build and start the node under test. ``standalone_config`` must
        already carry the messaging endpoint, instance id, parameters, and one
        seeding call per mock. ``setup`` is the node's real entry point (the
        exact ``async def setup(params, node_runner)`` shape ``NodeBuilder``
        runs), spawned only after the readiness barrier passes; ``parameters``
        is forwarded to it verbatim (the veneer passes the hydrated typed
        Parameters).

        ``new_standalone`` never consults ``PEPPY_RUNTIME_CONFIG``, so an
        inherited daemon environment cannot hijack the harness; a set variable
        still draws a warning because the author probably didn't intend it.
        """
        if os.environ.get("PEPPY_RUNTIME_CONFIG"):
            warnings.warn(
                "PEPPY_RUNTIME_CONFIG is set but ignored: the test harness always runs "
                "standalone (it must not be hijacked into daemon mode by an inherited "
                "environment)",
                stacklevel=2,
            )
        node_runner = await NodeRunner.new_standalone(
            str(os.fspath(peppy_config_path)), standalone_config
        )

        # Pre-setup readiness barrier: a publisher-side matching wait per
        # observed topic, on the node's session, with the identical keyexpr
        # the node's publisher will use.
        for probe in publisher_readiness:
            if probe.peer is not None and probe.link_id is not None:
                matched = await TopicMessenger.wait_for_pairing_subscriber(
                    node_runner.messenger(),
                    node_runner.bound_core_node(),
                    node_runner.bound_instance_id(),
                    probe.target,
                    probe.link_id,
                    probe.topic,
                    probe.peer,
                    READINESS_TIMEOUT,
                )
            else:
                matched = await TopicMessenger.wait_for_subscriber_with_link_id(
                    node_runner.messenger(),
                    node_runner.bound_core_node(),
                    node_runner.bound_instance_id(),
                    probe.target,
                    probe.topic,
                    READINESS_TIMEOUT,
                    probe.link_id,
                )
            if not matched:
                raise RuntimeError(
                    f"readiness barrier: the harness subscription for topic "
                    f"`{probe.topic}` (link_id {probe.link_id!r}) was not visible to the "
                    f"node's session within {READINESS_TIMEOUT}s; the mesh never routed "
                    "it — this is a harness/mock wiring bug, not a node bug"
                )

        # Reachability barrier for mocked services/actions: the node is a
        # fresh caller, so gate its session's discovery of each mock
        # queryable before setup's first poll/send_goal can race it.
        for probe in service_readiness:
            await _wait_reachable(
                probe.kind,
                node_runner.messenger(),
                node_runner.bound_core_node(),
                node_runner.bound_instance_id(),
                probe.target,
                probe.name,
                probe.producer,
                READINESS_TIMEOUT,
            )

        async def setup_then_seal() -> None:
            # Setup ends with the same check production runs: every endpoint
            # the manifest declares was announced, and nothing can be
            # announced after.
            await setup(parameters, node_runner)
            node_runner.seal_endpoints()

        setup_task = asyncio.create_task(setup_then_seal())
        return cls(node_runner, setup_task)

    @property
    def node_runner(self) -> NodeRunner:
        return self._node_runner

    def instance_id(self) -> str:
        return self._node_runner.bound_instance_id()

    def bound_core_node(self) -> str:
        return self._node_runner.bound_core_node()

    def setup_finished(self) -> bool:
        """Whether the spawned ``setup`` has already returned (many setups
        register their loops and return immediately; long-running ones never
        do until shutdown)."""
        return self._setup_task is None or self._setup_task.done()

    async def shutdown(self) -> None:
        """Converge the node: see the class-level teardown contract. Raises
        the ``setup`` error if it failed — a test whose node errored during
        setup should fail even when its assertions never noticed."""
        self._node_runner.cancellation_token().cancel()
        grace = self._node_runner.shutdown_grace_secs()
        setup_error: BaseException | None = None
        task = self._setup_task
        self._setup_task = None
        if task is not None:
            try:
                await asyncio.wait_for(task, grace)
            except (asyncio.TimeoutError, asyncio.CancelledError):
                # A setup that parks forever without watching the node's
                # cancellation token; production drops it at shutdown the
                # same way (wait_for already cancelled it).
                pass
            except BaseException as error:  # noqa: BLE001 — re-raised after hooks
                setup_error = error
        await self._node_runner.run_shutdown_hooks(grace)
        if setup_error is not None:
            raise setup_error

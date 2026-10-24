"""
Tests for peppylib ActionMessenger.

Python equivalent of `action_messenger_communication` in
peppy-shared/peppylib-rs/tests/actions.rs.
"""

import asyncio
import gc

import pytest

from peppylib import (
    ActionMessenger,
    ConcurrentAction,
    FailureError,
    FeedbackOverflowError,
    MessengerHandle,
    ProducerRef,
    SenderTarget,
    ZenohdInstance,
)

# Wire tags returned by the typed action replies (see peppylib::messaging).
RESULT_STATUS_COMPLETED = 0
RESULT_STATUS_CANCELLED = 1
RESULT_STATUS_ABANDONED = 2
RESULT_STATUS_FAILED = 4
CANCEL_STATE_SIGNALLED = 0

CORE_NODE = "test_core"
INSTANCE_ID = "test_instance"
NODE_NAME = "test_node"
NODE_TAG = "v1"
ACTION_NAME = "test_action"
GOAL_PAYLOAD = b"goal data"
GOAL_RESPONSE_PAYLOAD = b"goal accepted"
FEEDBACK_PAYLOAD = b"50% done"
RESULT_PAYLOAD = b"action result"
TEST_PRODUCER = ProducerRef(CORE_NODE, INSTANCE_ID)


def wrap_goal_accepted(response: bytes) -> bytes:
    """Frame the answer of an accepted goal the way the engine does: [1][response].

    Raw goal-service handlers bypass PendingGoal (which applies this framing
    itself), so tests driving the service directly must wrap their replies.
    Mirrors peppylib's Rust wrap_goal_accepted and independently pins the
    layout.
    """
    return bytes([1]) + response


async def _expose_test_action(server_handle: MessengerHandle) -> ConcurrentAction:
    """Expose the test action, with feedback, as the test instance of the test
    node."""
    return await ConcurrentAction.expose(
        server_handle,
        CORE_NODE,
        INSTANCE_ID,
        SenderTarget.node(NODE_NAME, NODE_TAG),
        ACTION_NAME,
        True,  # has_feedback
    )


async def _send_test_goal(
    client_handle: MessengerHandle,
    payload: bytes = GOAL_PAYLOAD,
    target: ProducerRef = TEST_PRODUCER,
    timeout_secs: float = 2.0,
    feedback_bound: int | None = None,
):
    """Send a goal for the test action to `target`. No caller needs a settle
    sleep first: send_goal retries a cold-start miss within its timeout until
    the goal service queryable propagates. `feedback_bound` None takes the
    default bound of peppylib."""
    return await ActionMessenger.send_goal(
        client_handle,
        CORE_NODE,
        INSTANCE_ID,
        SenderTarget.node(NODE_NAME, NODE_TAG),
        ACTION_NAME,
        target,
        payload,
        feedback_bound=feedback_bound,
        goal_timeout_secs=timeout_secs,
    )


@pytest.mark.asyncio
async def test_action_messenger_communication():
    """Full action lifecycle (goal, feedback, result) driven via ConcurrentAction.

    Using the engine (rather than the raw services) means the result reply is
    framed with the typed result-outcome envelope, so the client gets back a
    typed `status` plus the raw result `body`.
    """

    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        action = await _expose_test_action(server_handle)

        async def server():
            pending = await action.recv_next_goal()
            assert pending is not None
            ctx = await pending.accept(GOAL_RESPONSE_PAYLOAD)
            await ctx.publish_feedback(FEEDBACK_PAYLOAD)
            await ctx.complete(RESULT_PAYLOAD)

        server_task = asyncio.create_task(server())

        goal_handle = await _send_test_goal(client_handle)

        assert goal_handle.accepted
        assert goal_handle.refusal_kind is None
        assert goal_handle.goal_reply_body == GOAL_RESPONSE_PAYLOAD
        assert goal_handle.core_node == CORE_NODE
        assert goal_handle.instance_id == INSTANCE_ID

        # Client: receive feedback
        feedback = await asyncio.wait_for(
            goal_handle.on_next_feedback(),
            timeout=2.0,
        )

        assert feedback.payload == FEEDBACK_PAYLOAD

        # Client: request result (typed status + raw body).
        result = await ActionMessenger.request_result(
            client_handle,
            goal_handle,
            2.0,
        )

        assert result.status == RESULT_STATUS_COMPLETED
        assert result.body == RESULT_PAYLOAD

        await server_task


@pytest.mark.asyncio
async def test_cancel_goal_concurrent_with_feedback():
    """cancel_goal must not deadlock when on_next_feedback is waiting."""

    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        action = await _expose_test_action(server_handle)

        # Server: accept the goal and hold it open (never publish feedback and
        # never complete), so the client's on_next_feedback stays pending while
        # we fire a cancel concurrently.
        async def server():
            pending = await action.recv_next_goal()
            assert pending is not None
            _ctx = await pending.accept(GOAL_RESPONSE_PAYLOAD)
            await asyncio.sleep(3600)

        server_task = asyncio.create_task(server())

        goal_handle = await _send_test_goal(client_handle)

        # Start waiting for feedback (will block: the server never sends any).
        feedback_task = asyncio.ensure_future(goal_handle.on_next_feedback())

        # The cancel of a live goal must resolve promptly to the typed Signalled
        # state, without deadlocking against the pending feedback wait.
        cancel_reply = await asyncio.wait_for(
            ActionMessenger.cancel_goal(client_handle, goal_handle, 2.0),
            timeout=3.0,
        )

        assert cancel_reply.state == CANCEL_STATE_SIGNALLED

        feedback_task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await feedback_task

        server_task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await server_task


@pytest.mark.asyncio
async def test_producer_gone_unblocks_feedback_and_yields_abandoned():
    """Hard producer death mid-goal: ConnectionError + typed Abandoned result.

    Python equivalent of
    `concurrent_action_producer_death_unblocks_feedback_and_yields_abandoned`
    in peppy-shared/peppylib-rs/tests/actions.rs. No session teardown is
    exposed to Python, so death is simulated by dropping the engine (the sole
    holder of the producer's liveliness token) while its GoalContext stays
    referenced: the end-of-stream sentinel is never published (the exact race
    a SIGKILL / OOM loses), so only the token disappearing can unblock the
    consumer.
    """

    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        action = await _expose_test_action(server_handle)

        # Server: accept the goal, emit one feedback, and hand the live
        # GoalContext out. The engine is passed as a parameter (not captured
        # from the enclosing scope) so the test body holds the only reference
        # left to drop.
        async def server(engine):
            pending = await engine.recv_next_goal()
            assert pending is not None
            ctx = await pending.accept(GOAL_RESPONSE_PAYLOAD)
            await ctx.publish_feedback(FEEDBACK_PAYLOAD)
            return ctx

        server_task = asyncio.create_task(server(action))

        goal_handle = await _send_test_goal(client_handle)

        assert goal_handle.accepted
        assert goal_handle.refusal_kind is None
        assert goal_handle.goal_reply_body == GOAL_RESPONSE_PAYLOAD

        # The goal is live: first feedback arrives normally.
        feedback = await asyncio.wait_for(
            goal_handle.on_next_feedback(),
            timeout=2.0,
        )
        assert feedback.payload == FEEDBACK_PAYLOAD

        # Keep the context alive past the engine drop below: its
        # abandon-on-drop sentinel must never fire during this test.
        ctx = await asyncio.wait_for(server_task, timeout=2.0)

        # Kill the producer: drop the engine, undeclaring the liveliness
        # token while `ctx` keeps the goal open (no sentinel).
        del action, server_task
        gc.collect()

        # The drain must fail over to the typed producer-gone error
        # (liveliness DELETE → confirmation probes), surfaced as
        # ConnectionError: never hang, never the clean-close RuntimeError.
        with pytest.raises(ConnectionError):
            await asyncio.wait_for(goal_handle.on_next_feedback(), timeout=15.0)

        # The result poll fails against the dead producer; the follow-up
        # liveliness probe converts it to the typed Abandoned reply.
        result = await asyncio.wait_for(
            ActionMessenger.request_result(client_handle, goal_handle, 2.0),
            timeout=15.0,
        )
        assert result.status == RESULT_STATUS_ABANDONED
        assert result.body == b""

        del ctx


@pytest.mark.asyncio
async def test_send_goal_rejects_invalid_timeout():
    """send_goal validates timeout input and raises ValueError for invalid values."""
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        with pytest.raises(ValueError, match="goal_timeout_secs"):
            await _send_test_goal(client_handle, timeout_secs=-1.0)


@pytest.mark.asyncio
async def test_send_goal_honors_target_pair():
    """send_goal routes by the full pinned (core_node, instance_id) pair.

    A pinned target skips discovery, so the pair must match the producer as a
    whole: the correct pair reaches it, while a pair whose core_node is wrong
    must fail unreachable even though the instance_id alone would match
    (cross-core safety).
    """
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        action = await _expose_test_action(server_handle)

        async def server():
            pending = await action.recv_next_goal()
            assert pending is not None
            ctx = await pending.accept(GOAL_RESPONSE_PAYLOAD)
            await ctx.complete(RESULT_PAYLOAD)

        server_task = asyncio.create_task(server())

        # Pinned to the producer's exact pair: the goal goes through.
        goal_handle = await _send_test_goal(client_handle)
        assert goal_handle.accepted
        assert goal_handle.refusal_kind is None
        assert goal_handle.goal_reply_body == GOAL_RESPONSE_PAYLOAD

        await server_task

        # Pinned to the wrong core_node with the correct instance_id: the
        # pair is honored as a unit, so the producer must be unreachable.
        with pytest.raises(ConnectionError):
            await _send_test_goal(
                client_handle,
                target=ProducerRef("wrong_core", INSTANCE_ID),
                timeout_secs=0.5,
            )


@pytest.mark.asyncio
async def test_action_iface_scoped_native_and_conformed_do_not_collide():
    """Same action name exposed natively AND under an implemented contract must wire to distinct paths."""
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        native_handle = await MessengerHandle.from_host_port(router.host, router.port)
        iface_handle = await MessengerHandle.from_host_port(router.host, router.port)
        caller_handle = await MessengerHandle.from_host_port(router.host, router.port)

        native_goal_response = b"native_goal_ack"
        iface_goal_response = b"iface_goal_ack"

        native_action = await ActionMessenger.expose(
            native_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            "move",
        )
        iface_action = await ActionMessenger.expose(
            iface_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.contract("arm", "v1"),
            "move",
        )

        async def goal_handler(action, response: bytes):
            """Server-side: unwrap envelope, declare feedback publisher (kept), return response."""
            captured = [None]

            async def on_goal(req):
                (
                    publisher,
                    _goal_id,
                    _user_payload,
                ) = await action.feedback_publisher_factory.declare_from_wire(
                    req.link_id,
                    bytes(req.message.payload),
                )
                captured[0] = publisher
                return wrap_goal_accepted(response)

            await action.goal_service.handle_next_request(on_goal)
            return captured[0]

        native_task = asyncio.ensure_future(
            goal_handler(native_action, native_goal_response)
        )
        iface_task = asyncio.ensure_future(
            goal_handler(iface_action, iface_goal_response)
        )

        native_goal = await ActionMessenger.send_goal(
            caller_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            "move",
            ProducerRef(CORE_NODE, INSTANCE_ID),
            b"native_goal",
            goal_timeout_secs=2.0,
        )
        assert native_goal.accepted
        assert native_goal.goal_reply_body == native_goal_response

        iface_goal = await ActionMessenger.send_goal(
            caller_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.contract("arm", "v1"),
            "move",
            ProducerRef(CORE_NODE, INSTANCE_ID),
            b"iface_goal",
            goal_timeout_secs=2.0,
        )
        assert iface_goal.accepted
        assert iface_goal.goal_reply_body == iface_goal_response

        await asyncio.wait_for(native_task, timeout=2.0)
        await asyncio.wait_for(iface_task, timeout=2.0)


@pytest.mark.asyncio
async def test_refuse_then_accept_through_concurrent_action():
    """A refused goal is answered with its failure kind and message, and no
    goal response, without creating a context; the server keeps serving so
    a later goal is accepted and its typed result routes back by goal_id.

    Python mirror of `concurrent_action_refuse_then_accept` in
    peppy-shared/peppylib-rs/tests/actions.rs. Guards the binding's
    accept/refuse mapping and the typed-result framing for the engine path,
    which the Rust tests do not cover from Python.
    """
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        action = await _expose_test_action(server_handle)

        async def server():
            # First goal is refused; the second is accepted and completed.
            refused = await action.recv_next_goal()
            assert refused is not None
            assert refused.request_bytes == b"refuse"
            await refused.refuse("resource_unavailable", "resource is busy")

            accepted = await action.recv_next_goal()
            assert accepted is not None
            request = accepted.request_bytes
            ctx = await accepted.accept(b"accepted")
            await ctx.complete(b"result:" + request)

        server_task = asyncio.create_task(server())

        # A refused goal resolves with its failure and no goal response.
        goal_a = await _send_test_goal(client_handle, payload=b"refuse")
        assert not goal_a.accepted
        assert goal_a.refusal_kind == "resource_unavailable"
        assert goal_a.refusal_message == "resource is busy"
        assert goal_a.goal_reply_body == b""

        # The server kept serving past the refusal: the next goal is accepted
        # and its typed Completed result routes back.
        goal_b = await _send_test_goal(client_handle, payload=b"B")
        assert goal_b.accepted
        assert goal_b.refusal_kind is None
        assert goal_b.refusal_message is None
        assert goal_b.goal_reply_body == b"accepted"
        result_b = await ActionMessenger.request_result(client_handle, goal_b, 2.0)
        assert result_b.status == RESULT_STATUS_COMPLETED
        assert result_b.body == b"result:B"
        assert result_b.failure_kind is None
        assert result_b.cancel_reason is None

        await asyncio.wait_for(server_task, timeout=2.0)


@pytest.mark.asyncio
async def test_refusal_with_an_invalid_kind_name_raises_before_the_wire():
    """A kind that is not a failure kind name raises ValueError in the
    provider: nothing reaches the wire, and the goal stays pending until the
    provider answers it with a valid kind."""
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        action = await _expose_test_action(server_handle)

        async def server():
            pending = await action.recv_next_goal()
            assert pending is not None
            with pytest.raises(ValueError, match="invalid failure kind name"):
                await pending.refuse("Not A Kind", "never sent")
            await pending.refuse("rejected", "the goal asks too much")

        server_task = asyncio.create_task(server())

        goal = await _send_test_goal(client_handle)
        assert not goal.accepted
        assert goal.refusal_kind == "rejected"
        assert goal.refusal_message == "the goal asks too much"

        await asyncio.wait_for(server_task, timeout=2.0)


@pytest.mark.asyncio
async def test_failed_goal_gives_its_kind_and_message_and_no_result():
    """`fail` ends the goal with the status failed: the result reply carries
    the failure kind and message, and no result body."""
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        action = await _expose_test_action(server_handle)

        async def server():
            pending = await action.recv_next_goal()
            assert pending is not None
            ctx = await pending.accept(GOAL_RESPONSE_PAYLOAD)
            await ctx.fail("unreachable", "no plan to the target")

        server_task = asyncio.create_task(server())

        goal = await _send_test_goal(client_handle)
        assert goal.accepted
        result = await ActionMessenger.request_result(client_handle, goal, 2.0)
        assert result.status == RESULT_STATUS_FAILED
        assert result.failure_kind == "unreachable"
        assert result.failure_message == "no plan to the target"
        assert result.body == b""
        assert result.cancel_reason is None

        await asyncio.wait_for(server_task, timeout=2.0)


@pytest.mark.asyncio
async def test_cancelled_goal_gives_its_reason_and_result():
    """A cancel request reaches the worker with the cause "caller" and the
    reason of that cause; the worker ends the goal cancelled for a reason of
    its own, and the client reads the reason with the result."""
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        action = await _expose_test_action(server_handle)

        async def server():
            pending = await action.recv_next_goal()
            assert pending is not None
            ctx = await pending.accept(GOAL_RESPONSE_PAYLOAD)
            assert ctx.cancel_cause is None
            assert ctx.cancel_reason is None
            await ctx.cancel_signal()
            assert ctx.cancel_cause == "caller"
            assert ctx.cancel_reason == "cancelled by its caller"
            await ctx.complete_cancelled("stopped by the operator", b"partial")

        server_task = asyncio.create_task(server())

        goal = await _send_test_goal(client_handle)
        cancel_reply = await ActionMessenger.cancel_goal(client_handle, goal, 2.0)
        assert cancel_reply.state == CANCEL_STATE_SIGNALLED

        result = await ActionMessenger.request_result(client_handle, goal, 2.0)
        assert result.status == RESULT_STATUS_CANCELLED
        assert result.cancel_reason == "stopped by the operator"
        assert result.body == b"partial"
        assert result.failure_kind is None

        await asyncio.wait_for(server_task, timeout=2.0)


@pytest.mark.asyncio
async def test_the_goal_of_a_sender_that_is_gone_receives_its_cancel_signal():
    """A provider cancels the goal of a sender that is gone: once the client
    releases its goal handle and its session, the goal receives its cancel
    signal with the cause "sender_gone", after the sender grace of the action
    (zero here), and the reason of that cause. Python mirror of
    `the_goal_of_a_sender_that_is_gone_receives_its_cancel_signal` in
    peppy-shared/peppylib-rs/src/messaging/actions.rs."""
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        action = await _expose_test_action(server_handle)
        await action.set_sender_grace(0.0)

        async def server():
            pending = await action.recv_next_goal()
            assert pending is not None
            ctx = await pending.accept(GOAL_RESPONSE_PAYLOAD)
            await ctx.cancel_signal()
            cause = ctx.cancel_cause
            reason = ctx.cancel_reason
            await ctx.complete_cancelled(reason, b"")
            return cause, reason

        server_task = asyncio.create_task(server())

        goal = await _send_test_goal(client_handle)
        assert goal.accepted

        # The client goes away: its goal handle and its session.
        del goal, client_handle
        gc.collect()

        cause, reason = await asyncio.wait_for(server_task, timeout=30.0)
        assert cause == "sender_gone"
        assert reason == "its sender is gone"


@pytest.mark.asyncio
async def test_feedback_is_kept_in_full_up_to_the_bound():
    """The handle keeps every feedback message until it is read, up to its
    bound: the server publishes 1500 messages, more than any buffer of the
    newest 128 or 1024 messages holds, and ends the goal before the client
    reads one. The client then reads all of them, in order, and the end of
    the stream."""
    message_count = 1500
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        action = await _expose_test_action(server_handle)

        async def server():
            pending = await action.recv_next_goal()
            assert pending is not None
            ctx = await pending.accept(GOAL_RESPONSE_PAYLOAD)
            for index in range(message_count):
                await ctx.publish_feedback(f"feedback-{index}".encode())
            await ctx.complete(RESULT_PAYLOAD)

        server_task = asyncio.create_task(server())

        goal = await _send_test_goal(client_handle, feedback_bound=2 * message_count)
        result = await ActionMessenger.request_result(client_handle, goal, 30.0)
        assert result.status == RESULT_STATUS_COMPLETED

        for index in range(message_count):
            feedback = await asyncio.wait_for(goal.on_next_feedback(), timeout=10.0)
            assert feedback.payload == f"feedback-{index}".encode()
        with pytest.raises(RuntimeError):
            await asyncio.wait_for(goal.on_next_feedback(), timeout=10.0)

        await asyncio.wait_for(server_task, timeout=2.0)


@pytest.mark.asyncio
async def test_a_caller_that_reads_no_feedback_keeps_its_goal_past_its_bound():
    """A caller that never reads the feedback of its goal keeps the goal when
    the feedback goes past its bound: only a caller that reads the feedback
    follows it, and cancels the goal when it falls behind. The server publishes more messages than the bound,
    then completes the goal, and the caller reads the completed result."""
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        action = await _expose_test_action(server_handle)

        async def server():
            pending = await action.recv_next_goal()
            assert pending is not None
            ctx = await pending.accept(GOAL_RESPONSE_PAYLOAD)
            for index in range(20):
                await ctx.publish_feedback(f"feedback-{index}".encode())
            await ctx.complete(b"done")
            return ctx.cancel_cause

        server_task = asyncio.create_task(server())

        goal = await _send_test_goal(client_handle, feedback_bound=5)
        result = await ActionMessenger.request_result(client_handle, goal, 30.0)
        assert result.status == RESULT_STATUS_COMPLETED
        assert result.body == b"done"

        assert await asyncio.wait_for(server_task, timeout=2.0) is None


@pytest.mark.asyncio
async def test_send_goal_rejects_a_zero_feedback_bound():
    """A feedback bound holds one message or more."""
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        with pytest.raises(ValueError, match="feedback_bound"):
            await _send_test_goal(client_handle, feedback_bound=0)


@pytest.mark.asyncio
async def test_failure_error_carries_the_kind_and_the_message():
    """FailureError is a RuntimeError whose args are the failure kind and the
    message, so a service handler can raise it and a caller can read it."""
    error = FailureError("unreachable", "no plan to the target")
    assert isinstance(error, RuntimeError)
    assert error.args == ("unreachable", "no plan to the target")


def test_a_feedback_overflow_is_a_runtime_error():
    """FeedbackOverflowError is a RuntimeError, so a bare drain loop that
    catches RuntimeError ends on it as on a clean close."""
    assert issubclass(FeedbackOverflowError, RuntimeError)

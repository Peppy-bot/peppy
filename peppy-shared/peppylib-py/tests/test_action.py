"""
Tests for peppylib ActionMessenger.

Python equivalent of `action_messenger_communication` in
crates/peppylib/tests/actions.rs.
"""

import asyncio
import gc

import pytest

from peppylib import (
    ActionMessenger,
    ConcurrentAction,
    MessengerHandle,
    ProducerRef,
    QoSProfile,
    SenderTarget,
    ZenohdInstance,
)

# Wire tags returned by the typed action replies (see peppylib::messaging).
RESULT_STATUS_COMPLETED = 0
RESULT_STATUS_ABANDONED = 2
CANCEL_STATE_SIGNALLED = 0

CORE_NODE = "test_core"
INSTANCE_ID = "test_instance"
NODE_NAME = "test_node"
NODE_TAG = "v1"
ACTION_NAME = "test_action"
GOAL_PAYLOAD = b"goal data"
GOAL_RESPONSE_PAYLOAD = b"goal accepted"


def wrap_goal_ack(body: bytes, accepted: bool = True, reason: str = "") -> bytes:
    """Frame a goal reply the way the engine does: [accepted][reason_len u16 BE][reason][body].

    Raw goal-service handlers bypass PendingGoal (which applies this framing
    itself), so tests driving the service directly must wrap their replies.
    Mirrors peppylib's Rust wrap_goal_ack and independently pins the layout.
    """
    reason_bytes = reason.encode()
    return bytes([1 if accepted else 0]) + len(reason_bytes).to_bytes(2, "big") + reason_bytes + body
FEEDBACK_PAYLOAD = b"50% done"
RESULT_PAYLOAD = b"action result"


@pytest.mark.asyncio
async def test_action_messenger_communication():
    """Full action lifecycle: goal, feedback, result — driven via ConcurrentAction.

    Using the engine (rather than the raw services) means the result reply is
    framed with the typed result-outcome envelope, so the client gets back a
    typed `status` plus the raw result `body`.
    """

    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        action = await ConcurrentAction.expose(
            server_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            ACTION_NAME,
            True,  # has_feedback
        )


        async def server():
            pending = await action.recv_next_goal()
            assert pending is not None
            ctx = await pending.accept(GOAL_RESPONSE_PAYLOAD)
            await ctx.publish_feedback(FEEDBACK_PAYLOAD)
            await ctx.complete(RESULT_PAYLOAD)

        server_task = asyncio.create_task(server())

        goal_handle = await ActionMessenger.send_goal(
            client_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            ACTION_NAME,
            ProducerRef(CORE_NODE, INSTANCE_ID),
            GOAL_PAYLOAD,
            QoSProfile.Reliable,
            2.0,)

        assert goal_handle.accepted
        assert goal_handle.reason is None
        assert goal_handle.goal_reply_body == GOAL_RESPONSE_PAYLOAD
        assert goal_handle.core_node == CORE_NODE
        assert goal_handle.instance_id == INSTANCE_ID

        # Client: receive feedback
        feedback = await asyncio.wait_for(
            goal_handle.on_next_feedback(),
            timeout=2.0,
        )

        assert feedback.payload == FEEDBACK_PAYLOAD

        # Client: request result — typed status + raw body.
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

        action = await ConcurrentAction.expose(
            server_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            ACTION_NAME,
            True,  # has_feedback
        )


        # Server: accept the goal and hold it open — never publish feedback and
        # never complete — so the client's on_next_feedback stays pending while
        # we fire a cancel concurrently.
        async def server():
            pending = await action.recv_next_goal()
            assert pending is not None
            _ctx = await pending.accept(GOAL_RESPONSE_PAYLOAD)
            await asyncio.sleep(3600)

        server_task = asyncio.create_task(server())

        goal_handle = await ActionMessenger.send_goal(
            client_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            ACTION_NAME,
            ProducerRef(CORE_NODE, INSTANCE_ID),
            GOAL_PAYLOAD,
            QoSProfile.Reliable,
            2.0,)

        # Start waiting for feedback (will block — server never sends any).
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
    in crates/peppylib/tests/actions.rs. No session teardown is exposed to
    Python, so death is simulated by dropping the engine — the sole holder of
    the producer's liveliness token — while its GoalContext stays referenced:
    the end-of-stream sentinel is never published (the exact race a SIGKILL /
    OOM loses), so only the token disappearing can unblock the consumer.
    """

    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        action = await ConcurrentAction.expose(
            server_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            ACTION_NAME,
            True,  # has_feedback
        )


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

        goal_handle = await ActionMessenger.send_goal(
            client_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            ACTION_NAME,
            ProducerRef(CORE_NODE, INSTANCE_ID),
            GOAL_PAYLOAD,
            QoSProfile.Reliable,
            2.0,)

        assert goal_handle.accepted
        assert goal_handle.reason is None
        assert goal_handle.goal_reply_body == GOAL_RESPONSE_PAYLOAD

        # The goal is live: first feedback arrives normally.
        feedback = await asyncio.wait_for(
            goal_handle.on_next_feedback(),
            timeout=2.0,
        )
        assert feedback.payload == FEEDBACK_PAYLOAD

        # Keep the context alive past the engine drop below — its
        # abandon-on-drop sentinel must never fire during this test.
        ctx = await asyncio.wait_for(server_task, timeout=2.0)

        # Kill the producer: drop the engine, undeclaring the liveliness
        # token while `ctx` keeps the goal open (no sentinel).
        del action, server_task
        gc.collect()

        # The drain must fail over to the typed producer-gone error
        # (liveliness DELETE → confirmation probes), surfaced as
        # ConnectionError — never hang, never the clean-close RuntimeError.
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
            await ActionMessenger.send_goal(
                client_handle,
                CORE_NODE,
                INSTANCE_ID,
                SenderTarget.node(NODE_NAME, NODE_TAG),
                ACTION_NAME,
                ProducerRef(CORE_NODE, INSTANCE_ID),
                GOAL_PAYLOAD,
                QoSProfile.Reliable,
                -1.0,)


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

        action = await ConcurrentAction.expose(
            server_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            ACTION_NAME,
            True,  # has_feedback
        )


        async def server():
            pending = await action.recv_next_goal()
            assert pending is not None
            ctx = await pending.accept(GOAL_RESPONSE_PAYLOAD)
            await ctx.complete(RESULT_PAYLOAD)

        server_task = asyncio.create_task(server())

        # Pinned to the producer's exact pair: the goal goes through.
        goal_handle = await ActionMessenger.send_goal(
            client_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            ACTION_NAME,
            ProducerRef(CORE_NODE, INSTANCE_ID),
            GOAL_PAYLOAD,
            QoSProfile.Reliable,
            2.0,)
        assert goal_handle.accepted
        assert goal_handle.reason is None
        assert goal_handle.goal_reply_body == GOAL_RESPONSE_PAYLOAD

        await server_task

        # Pinned to the wrong core_node with the correct instance_id: the
        # pair is honored as a unit, so the producer must be unreachable.
        with pytest.raises(ConnectionError):
            await ActionMessenger.send_goal(
                client_handle,
                CORE_NODE,
                INSTANCE_ID,
                SenderTarget.node(NODE_NAME, NODE_TAG),
                ACTION_NAME,
                ProducerRef("wrong_core", INSTANCE_ID),
                GOAL_PAYLOAD,
                QoSProfile.Reliable,
                0.5,)


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
                publisher, _goal_id, _user_payload = await action.feedback_publisher_factory.declare_from_wire(
                    req.link_id,
                    bytes(req.message.payload),
                )
                captured[0] = publisher
                return wrap_goal_ack(response)

            await action.goal_service.handle_next_request(on_goal)
            return captured[0]

        native_task = asyncio.ensure_future(goal_handler(native_action, native_goal_response))
        iface_task = asyncio.ensure_future(goal_handler(iface_action, iface_goal_response))


        native_goal = await ActionMessenger.send_goal(
            caller_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            "move",
            ProducerRef(CORE_NODE, INSTANCE_ID),
            b"native_goal",
            QoSProfile.Reliable,
            2.0,
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
            QoSProfile.Reliable,
            2.0,
        )
        assert iface_goal.accepted
        assert iface_goal.goal_reply_body == iface_goal_response

        await asyncio.wait_for(native_task, timeout=2.0)
        await asyncio.wait_for(iface_task, timeout=2.0)


@pytest.mark.asyncio
async def test_reject_then_accept_through_concurrent_action():
    """A rejected goal is still answered (the client gets its goal response)
    without creating a context, and the server keeps serving so a later goal is
    accepted and its typed result routes back by goal_id.

    Python mirror of `concurrent_action_reject_then_accept` in
    crates/peppylib/tests/actions.rs. Guards the binding's accept/reject mapping
    and the typed-result framing for the engine path, which the Rust-only tests
    did not cover from Python.
    """
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        action = await ConcurrentAction.expose(
            server_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            ACTION_NAME,
            True,  # has_feedback
        )

        async def server():
            # First goal is rejected; the second is accepted and completed.
            rejected = await action.recv_next_goal()
            assert rejected is not None
            assert rejected.request_bytes == b"reject"
            await rejected.reject("resource is busy", b"rejected")

            accepted = await action.recv_next_goal()
            assert accepted is not None
            request = accepted.request_bytes
            ctx = await accepted.accept(b"accepted")
            await ctx.complete(b"result:" + request)

        server_task = asyncio.create_task(server())

        async def send(payload: bytes):
            # No settle sleep: send_goal self-retries on a cold-start miss within
            # its timeout until the goal service queryable propagates.
            return await ActionMessenger.send_goal(
                client_handle,
                CORE_NODE,
                INSTANCE_ID,
                SenderTarget.node(NODE_NAME, NODE_TAG),
                ACTION_NAME,
                ProducerRef(CORE_NODE, INSTANCE_ID),
                payload,
                QoSProfile.Reliable,
                2.0,
            )

        # A rejected goal still resolves with the server's goal response.
        goal_a = await send(b"reject")
        assert not goal_a.accepted
        assert goal_a.reason == "resource is busy"
        assert goal_a.goal_reply_body == b"rejected"

        # The server kept serving past the rejection: the next goal is accepted
        # and its typed Completed result routes back.
        goal_b = await send(b"B")
        assert goal_b.accepted
        assert goal_b.reason is None
        assert goal_b.goal_reply_body == b"accepted"
        result_b = await ActionMessenger.request_result(client_handle, goal_b, 2.0)
        assert result_b.status == RESULT_STATUS_COMPLETED
        assert result_b.body == b"result:B"

        await asyncio.wait_for(server_task, timeout=2.0)

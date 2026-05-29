"""
Tests for peppylib ActionMessenger.

Python equivalent of `action_messenger_communication` in
crates/peppylib/tests/actions.rs.
"""

import asyncio

import pytest

from peppylib import (
    ActionMessenger,
    ConcurrentAction,
    MessengerHandle,
    QoSProfile,
    SenderTarget,
    ZenohdInstance,
)

# Wire tags returned by the typed action replies (see peppylib::messaging).
RESULT_STATUS_COMPLETED = 0
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

        # Allow subscriptions to propagate
        await asyncio.sleep(0.05)

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
            CORE_NODE,
            INSTANCE_ID,
            GOAL_PAYLOAD,
            QoSProfile.Reliable,
            2.0,)

        assert goal_handle.goal_response.payload == GOAL_RESPONSE_PAYLOAD

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

        await asyncio.sleep(0.05)

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
            CORE_NODE,
            INSTANCE_ID,
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
                CORE_NODE,
                INSTANCE_ID,
                GOAL_PAYLOAD,
                QoSProfile.Reliable,
                -1.0,)


@pytest.mark.asyncio
async def test_send_goal_honors_target_core_node():
    """send_goal should route to the explicit target daemon when provided."""
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        await ActionMessenger.expose(
            server_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            ACTION_NAME,
        )

        await asyncio.sleep(0.05)

        with pytest.raises(ConnectionError):
            await ActionMessenger.send_goal(
                client_handle,
                CORE_NODE,
                INSTANCE_ID,
                SenderTarget.node(NODE_NAME, NODE_TAG),
                ACTION_NAME,
                "wrong_core_node",
                INSTANCE_ID,
                GOAL_PAYLOAD,
                QoSProfile.Reliable,
                0.5,)


@pytest.mark.asyncio
async def test_action_iface_scoped_native_and_conformed_do_not_collide():
    """Same action name exposed natively AND under a conformed interface must wire to distinct paths."""
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
            SenderTarget.interface("arm", "v1"),
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
                return response

            await action.goal_service.handle_next_request(on_goal)
            return captured[0]

        native_task = asyncio.ensure_future(goal_handler(native_action, native_goal_response))
        iface_task = asyncio.ensure_future(goal_handler(iface_action, iface_goal_response))

        await asyncio.sleep(0.1)

        native_goal = await ActionMessenger.send_goal(
            caller_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            "move",
            CORE_NODE,
            INSTANCE_ID,
            b"native_goal",
            QoSProfile.Reliable,
            2.0,
        )
        assert native_goal.goal_response.payload == native_goal_response

        iface_goal = await ActionMessenger.send_goal(
            caller_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.interface("arm", "v1"),
            "move",
            CORE_NODE,
            INSTANCE_ID,
            b"iface_goal",
            QoSProfile.Reliable,
            2.0,
        )
        assert iface_goal.goal_response.payload == iface_goal_response

        await asyncio.wait_for(native_task, timeout=2.0)
        await asyncio.wait_for(iface_task, timeout=2.0)

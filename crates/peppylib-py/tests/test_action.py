"""
Tests for peppylib ActionMessenger.

Python equivalent of `action_messenger_communication` in
crates/peppylib/tests/actions.rs.
"""

import asyncio

import peppylib
import pytest

from peppylib import ActionMessenger, MessengerHandle, QoSProfile, ZenohdInstance

CORE_NODE = "test_core"
INSTANCE_ID = "test_instance"
NODE_NAME = "test_node"
ACTION_NAME = "test_action"
GOAL_PAYLOAD = b"goal data"
GOAL_RESPONSE_PAYLOAD = b"goal accepted"
FEEDBACK_PAYLOAD = b"50% done"
RESULT_PAYLOAD = b"action result"


@pytest.mark.asyncio
async def test_action_messenger_communication():
    """Full action lifecycle: goal, feedback, result."""

    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        # Expose the action server
        action = await ActionMessenger.expose(
            server_handle,
            CORE_NODE,
            INSTANCE_ID,
            NODE_NAME,
            ACTION_NAME,
        )

        # Allow subscriptions to propagate
        await asyncio.sleep(0.05)

        # Run the server side in a spawned task. The factory's
        # declare_from_wire absorbs the envelope unwrap + per-goal publisher
        # declaration in one call.
        captured_publisher: list = [None]

        async def _on_goal(req):
            (
                publisher,
                _goal_id,
                _user_payload,
            ) = await action.feedback_publisher_factory.declare_from_wire(
                bytes(req.message.payload)
            )
            captured_publisher[0] = publisher
            return GOAL_RESPONSE_PAYLOAD

        async def server():
            await action.goal_service.handle_next_request(_on_goal)

            assert captured_publisher[0] is not None
            await captured_publisher[0].publish(FEEDBACK_PAYLOAD)

            # Handle the result request
            await action.result_service.handle_next_request(lambda _req: RESULT_PAYLOAD)

        server_task = asyncio.create_task(server())

        # Client: wrap the user payload with a fresh goal_id and send.
        client_goal_id = peppylib.messaging.actions.generate_goal_id()
        wrapped_goal_payload = peppylib.messaging.actions.wrap_goal_payload(
            client_goal_id, GOAL_PAYLOAD
        )
        goal_handle = await ActionMessenger.send_goal(
            client_handle,
            CORE_NODE,
            INSTANCE_ID,
            NODE_NAME,
            ACTION_NAME,
            CORE_NODE,
            INSTANCE_ID,
            client_goal_id,
            wrapped_goal_payload,
            QoSProfile.Reliable,
            2.0,
        )

        assert goal_handle.goal_response.payload == GOAL_RESPONSE_PAYLOAD

        # Client: receive feedback
        feedback = await asyncio.wait_for(
            goal_handle.on_next_feedback(),
            timeout=2.0,
        )

        assert feedback.payload == FEEDBACK_PAYLOAD

        # Client: request result
        result = await ActionMessenger.request_result(
            client_handle,
            goal_handle,
            2.0,
        )

        assert result.payload == RESULT_PAYLOAD

        await server_task


@pytest.mark.asyncio
async def test_cancel_goal_concurrent_with_feedback():
    """cancel_goal must not deadlock when on_next_feedback is waiting."""

    CANCEL_RESPONSE_PAYLOAD = b"cancelled"

    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        action = await ActionMessenger.expose(
            server_handle,
            CORE_NODE,
            INSTANCE_ID,
            NODE_NAME,
            ACTION_NAME,
        )

        await asyncio.sleep(0.05)

        # Server: accept goal then handle cancel (never send feedback)
        async def server():
            await action.goal_service.handle_next_request(
                lambda _req: GOAL_RESPONSE_PAYLOAD
            )
            await action.cancel_service.handle_next_request(
                lambda _req: CANCEL_RESPONSE_PAYLOAD
            )

        server_task = asyncio.create_task(server())

        client_goal_id = peppylib.messaging.actions.generate_goal_id()
        wrapped_goal_payload = peppylib.messaging.actions.wrap_goal_payload(
            client_goal_id, GOAL_PAYLOAD
        )
        goal_handle = await ActionMessenger.send_goal(
            client_handle,
            CORE_NODE,
            INSTANCE_ID,
            NODE_NAME,
            ACTION_NAME,
            CORE_NODE,
            INSTANCE_ID,
            client_goal_id,
            wrapped_goal_payload,
            QoSProfile.Reliable,
            2.0,
        )

        # Start waiting for feedback (will block — server never sends any).
        feedback_task = asyncio.ensure_future(goal_handle.on_next_feedback())

        cancel_response = await asyncio.wait_for(
            ActionMessenger.cancel_goal(client_handle, goal_handle, 2.0),
            timeout=3.0,
        )

        assert cancel_response.payload == CANCEL_RESPONSE_PAYLOAD

        feedback_task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await feedback_task

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
                NODE_NAME,
                ACTION_NAME,
                CORE_NODE,
                INSTANCE_ID,
                peppylib.messaging.actions.generate_goal_id(),
                peppylib.messaging.actions.wrap_goal_payload(
                    peppylib.messaging.actions.generate_goal_id(), GOAL_PAYLOAD
                ),
                QoSProfile.Reliable,
                -1.0,
            )


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
            NODE_NAME,
            ACTION_NAME,
        )

        await asyncio.sleep(0.05)

        with pytest.raises(ConnectionError):
            await ActionMessenger.send_goal(
                client_handle,
                CORE_NODE,
                INSTANCE_ID,
                NODE_NAME,
                ACTION_NAME,
                "wrong_core_node",
                INSTANCE_ID,
                peppylib.messaging.actions.generate_goal_id(),
                peppylib.messaging.actions.wrap_goal_payload(
                    peppylib.messaging.actions.generate_goal_id(), GOAL_PAYLOAD
                ),
                QoSProfile.Reliable,
                0.5,
            )

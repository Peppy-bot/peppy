"""
Tests for peppylib ActionMessenger.

Python equivalent of `action_messenger_communication` in
crates/peppylib/tests/actions.rs.
"""

import asyncio

import pytest

from peppylib import ActionMessenger, MessengerHandle, QoSProfile, ZenohdInstance

DAEMON_NODE = "test_daemon"
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
            DAEMON_NODE,
            INSTANCE_ID,
            NODE_NAME,
            ACTION_NAME,
        )

        # Allow subscriptions to propagate
        await asyncio.sleep(0.05)

        # Run the server side in a spawned task
        async def server():
            # Handle the goal request
            await action.goal_service.handle_next_request(
                lambda _req: GOAL_RESPONSE_PAYLOAD
            )

            # Publish feedback
            await action.feedback_publisher.publish(FEEDBACK_PAYLOAD)

            # Handle the result request
            await action.result_service.handle_next_request(lambda _req: RESULT_PAYLOAD)

        server_task = asyncio.create_task(server())

        # Client: send goal
        goal_handle = await ActionMessenger.send_goal(
            client_handle,
            DAEMON_NODE,
            INSTANCE_ID,
            NODE_NAME,
            ACTION_NAME,
            DAEMON_NODE,
            INSTANCE_ID,
            GOAL_PAYLOAD,
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
            DAEMON_NODE,
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

        goal_handle = await ActionMessenger.send_goal(
            client_handle,
            DAEMON_NODE,
            INSTANCE_ID,
            NODE_NAME,
            ACTION_NAME,
            DAEMON_NODE,
            INSTANCE_ID,
            GOAL_PAYLOAD,
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
                DAEMON_NODE,
                INSTANCE_ID,
                NODE_NAME,
                ACTION_NAME,
                DAEMON_NODE,
                INSTANCE_ID,
                GOAL_PAYLOAD,
                QoSProfile.Reliable,
                -1.0,
            )

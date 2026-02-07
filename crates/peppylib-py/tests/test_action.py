"""
Tests for peppylib ActionMessenger.

Python equivalent of `action_messenger_communication` in
crates/peppylib/tests/actions.rs.
"""

import asyncio

import pytest

from peppylib import ActionMessenger, MessengerHandle, QoSProfile, ZenohdInstance

MASTER_NODE = "test_master"
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
            MASTER_NODE,
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
            MASTER_NODE,
            INSTANCE_ID,
            NODE_NAME,
            ACTION_NAME,
            MASTER_NODE,
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

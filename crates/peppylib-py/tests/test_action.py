"""
Tests for peppylib ActionMessenger.

Python equivalent of `action_messenger_communication` in
crates/peppylib/tests/actions.rs. Drives the concurrent ActionServer /
GoalContext API: a server accepts each goal via `recv_next_goal`, then owns
that goal's feedback stream, cancel signal, and result delivery.
"""

import asyncio

import pytest

from peppylib import (
    ActionMessenger,
    MessengerHandle,
    QoSProfile,
    SenderTarget,
    ZenohdInstance,
    actions,
)

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
    """Full action lifecycle: goal, feedback, result."""

    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        # Expose the action server (background cancel/result pumps spawn here).
        action = await ActionMessenger.expose(
            server_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            ACTION_NAME,
        )

        # Allow subscriptions to propagate
        await asyncio.sleep(0.05)

        # Accept loop: a real server keeps running so its results stay
        # fetchable while the client drains feedback and requests the result.
        async def server():
            while True:
                goal_request = await action.recv_next_goal()
                if goal_request is None:
                    break
                ctx = await goal_request.accept(GOAL_RESPONSE_PAYLOAD)
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

        server_task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await server_task


@pytest.mark.asyncio
async def test_cancel_goal_concurrent_with_feedback():
    """cancel_goal must not deadlock when on_next_feedback is waiting.

    Cancellation is an SDK-driven signal now: the framework acks the cancel
    (accepted == True while the goal is in flight) and fires the worker's
    cancel_signal(); there is no user cancel handler.
    """

    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        action = await ActionMessenger.expose(
            server_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            ACTION_NAME,
        )

        await asyncio.sleep(0.05)

        # Server: accept the goal then observe the cancel signal (never sends
        # feedback). Stays alive so the SDK cancel pump can ack the client.
        async def server():
            goal_request = await action.recv_next_goal()
            ctx = await goal_request.accept(GOAL_RESPONSE_PAYLOAD)
            await ctx.cancel_signal()

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
            2.0,
        )

        # Start waiting for feedback (will block — server never sends any).
        feedback_task = asyncio.ensure_future(goal_handle.on_next_feedback())

        cancel_response = await asyncio.wait_for(
            ActionMessenger.cancel_goal(client_handle, goal_handle, 2.0),
            timeout=3.0,
        )

        # The SDK acks the cancel with a one-byte payload decoded via
        # decode_cancel_ack; True means the goal was in flight.
        assert actions.decode_cancel_ack(bytes(cancel_response.payload)) is True

        feedback_task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await feedback_task

        await asyncio.wait_for(server_task, timeout=2.0)


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
            """Server-side: accept the next goal and return its context."""
            goal_request = await action.recv_next_goal()
            return await goal_request.accept(response)

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

        # Keep each context alive past the assertions (awaiting the task hands
        # it back); the routing check is the goal_response payloads above.
        await asyncio.wait_for(native_task, timeout=2.0)
        await asyncio.wait_for(iface_task, timeout=2.0)

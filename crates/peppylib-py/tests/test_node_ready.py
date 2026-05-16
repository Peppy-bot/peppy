"""
Tests for peppylib node ready service.

Python equivalent of crates/peppylib/tests/ready_node.rs.
"""

import asyncio

import pytest

from peppylib import MessengerHandle, ServiceMessenger, ZenohdInstance
from peppylib.config import NODE_READY_SERVICE
from peppylib.services import NodeReadyService

from common import TEST_INSTANCE_ID, TEST_NODE_NAME

TEST_CORE_NODE_NAME = "test_core_node"
CALLER_INSTANCE_ID = "caller_instance"


@pytest.mark.asyncio
async def test_ready_node():
    """Ready service accepts all valid targeting modes and echoes back the payload.
    The test validates four targeting combinations:
    - specific core node + specific instance
    - specific core node + broadcast instance
    - broadcast core node + specific instance
    - full broadcast (core node + instance)
    """
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        messenger = await MessengerHandle.from_host_port(router.host, router.port)

        # Start the ready service directly
        task = await NodeReadyService.listen(
            messenger,
            TEST_CORE_NODE_NAME,
            TEST_INSTANCE_ID,
            TEST_NODE_NAME,
        )

        # Allow the service to fully establish its listeners
        await asyncio.sleep(0.05)

        request_payload = b"ready"

        # The ready service should accept all valid targeting modes
        target_combinations = [
            ("specific+specific", TEST_CORE_NODE_NAME, TEST_INSTANCE_ID),
            ("specific+broadcast", TEST_CORE_NODE_NAME, None),
            ("broadcast+specific", None, TEST_INSTANCE_ID),
            ("broadcast+broadcast", None, None),
        ]

        # Each poll uses a fresh MessengerHandle (Zenoh session) because
        # Zenoh client-mode routing tables become unreliable when a session
        # rapidly creates/drops wildcard subscribers (the response
        # subscription in poll_service) interleaved with put() calls to
        # varying key prefixes. A fresh session avoids this interference.
        for label, target_core_node, target_instance_id in target_combinations:
            poll_messenger = await MessengerHandle.from_host_port(
                router.host, router.port
            )
            try:
                response = await ServiceMessenger.poll(
                    poll_messenger,
                    TEST_CORE_NODE_NAME,
                    CALLER_INSTANCE_ID,
                    TEST_NODE_NAME,
                    None,  # iface_name (None = native)
                    None,  # iface_tag (None = native)
                    NODE_READY_SERVICE,
                    target_core_node,
                    target_instance_id,
                    request_payload,
                    2.0,)
            except RuntimeError as exc:
                pytest.fail(f"[{label}] poll failed: {exc}")

            assert response.payload == request_payload
            assert response.core_node == TEST_CORE_NODE_NAME
            assert response.instance_id == TEST_INSTANCE_ID

        # The ready task should still be running
        assert not task.is_finished()
        task.abort()

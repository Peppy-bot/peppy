"""
Tests for peppylib node ready service.

Python equivalent of crates/peppylib/tests/ready_node.rs.
"""

import asyncio

import pytest

from peppylib import (
    MessengerHandle,
    ProducerRef,
    SenderTarget,
    ServiceMessenger,
    ZenohdInstance,
)
from peppylib.config import NODE_READY_SERVICE
from peppylib.services import NodeReadyService

from common import TEST_INSTANCE_ID, TEST_NODE_NAME, TEST_NODE_TAG

TEST_CORE_NODE_NAME = "test_core_node"
CALLER_INSTANCE_ID = "caller_instance"


@pytest.mark.asyncio
async def test_ready_node():
    """Ready service accepts both valid targeting modes and echoes back the payload.
    The test validates the two representable producer targets:
    - fully pinned producer ((core_node, instance_id) pair)
    - full broadcast (None: discover-then-pin wildcard)
    """
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        messenger = await MessengerHandle.from_host_port(router.host, router.port)

        # Start the ready service directly
        task = await NodeReadyService.listen(
            messenger,
            TEST_CORE_NODE_NAME,
            TEST_INSTANCE_ID,
            SenderTarget.node(TEST_NODE_NAME, TEST_NODE_TAG),
        )


        request_payload = b"ready"

        # The ready service should accept both valid targeting modes
        target_combinations = [
            ("pinned", ProducerRef(TEST_CORE_NODE_NAME, TEST_INSTANCE_ID)),
            ("broadcast", None),
        ]

        # Each poll uses a fresh MessengerHandle (Zenoh session) because
        # Zenoh client-mode routing tables become unreliable when a session
        # rapidly creates/drops wildcard subscribers (the response
        # subscription in poll_service) interleaved with put() calls to
        # varying key prefixes. A fresh session avoids this interference.
        for label, target in target_combinations:
            poll_messenger = await MessengerHandle.from_host_port(
                router.host, router.port
            )
            try:
                response = await ServiceMessenger.poll(
                    poll_messenger,
                    TEST_CORE_NODE_NAME,
                    CALLER_INSTANCE_ID,
                    SenderTarget.node(TEST_NODE_NAME, TEST_NODE_TAG),
                    NODE_READY_SERVICE,
                    target,
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

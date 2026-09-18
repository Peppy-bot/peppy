"""
Tests for peppylib node shutdown service.

Python equivalent of crates/peppylib/tests/shutdown_node.rs.
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
from peppylib.config import SHUTDOWN_SERVICE
from peppylib.services import ShutdownService

from common import TEST_INSTANCE_ID, TEST_NODE_NAME, TEST_NODE_TAG

TEST_CORE_NODE_NAME = "test_core_node"
CALLER_INSTANCE_ID = "caller_instance"


@pytest.mark.asyncio
async def test_shutdown_node():
    """Shutdown service responds with the payload and sends the shutdown signal."""
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        messenger = await MessengerHandle.from_host_port(router.host, router.port)

        # Start the shutdown service directly
        task, receiver = await ShutdownService.listen(
            messenger,
            TEST_CORE_NODE_NAME,
            TEST_INSTANCE_ID,
            SenderTarget.node(TEST_NODE_NAME, TEST_NODE_TAG),
        )


        # Send a shutdown request
        request_payload = b"shutdown"

        response = await ServiceMessenger.poll(
            messenger,
            TEST_CORE_NODE_NAME,
            CALLER_INSTANCE_ID,
            SenderTarget.node(TEST_NODE_NAME, TEST_NODE_TAG),
            SHUTDOWN_SERVICE,
            ProducerRef(TEST_CORE_NODE_NAME, TEST_INSTANCE_ID),
            request_payload,
            2.0,)

        # Verify the response echoes back the payload
        assert response.payload == request_payload
        assert response.instance_id == TEST_INSTANCE_ID

        # Verify the shutdown signal was sent
        result = await asyncio.wait_for(receiver.wait(), timeout=1.0)
        assert result is True

        # The shutdown task should still be running (it handles multiple requests)
        assert not task.is_finished()
        task.abort()

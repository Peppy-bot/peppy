"""
Tests for peppylib node health service.

Python equivalent of crates/peppylib/tests/node_health_service.rs.
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
from peppylib.config import NODE_HEALTH_SERVICE
from peppylib.services import NodeHealthService

from common import TEST_INSTANCE_ID, TEST_NODE_NAME, TEST_NODE_TAG

TEST_CORE_NODE_NAME = "test_core_node"
CALLER_INSTANCE_ID = "caller_instance"


@pytest.mark.asyncio
async def test_node_health_request_response_roundtrip():
    """Health service responds to a poll request with the correct instance_id."""
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        messenger = await MessengerHandle.from_host_port(router.host, router.port)

        # Start the health service directly
        task = await NodeHealthService.listen(
            messenger,
            TEST_CORE_NODE_NAME,
            TEST_INSTANCE_ID,
            SenderTarget.node(TEST_NODE_NAME, TEST_NODE_TAG),
        )


        # Build and send the health request
        request_payload = b"health"

        response = await ServiceMessenger.poll(
            messenger,
            TEST_CORE_NODE_NAME,
            CALLER_INSTANCE_ID,
            SenderTarget.node(TEST_NODE_NAME, TEST_NODE_TAG),
            NODE_HEALTH_SERVICE,
            ProducerRef(TEST_CORE_NODE_NAME, TEST_INSTANCE_ID),
            request_payload,
            2.0,)

        # Verify the response
        assert response is not None
        assert response.instance_id == TEST_INSTANCE_ID

        # The health task should still be running
        assert not task.is_finished()
        task.abort()

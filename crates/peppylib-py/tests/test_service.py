"""
Tests for peppylib ServiceMessenger.

Python equivalent of `service_messenger_communication` in
crates/peppylib/tests/services.rs.

Note: ServiceMessenger.listen and handle_next_request are not yet exposed
to Python.  This test will not pass until those bindings are added.
"""

import asyncio

import pytest

from peppylib import MessengerHandle, ServiceMessenger, ZenohdInstance

MASTER_NODE = "test_master"
INSTANCE_ID = "test_instance"
NODE_NAME = "test_node"
SERVICE_NAME = "test_service"
REQUEST_PAYLOAD = b"Hello request"
RESPONSE_PAYLOAD = b"Hello response"


@pytest.mark.asyncio
async def test_service_messenger_communication():
    """A service listener receives a request and sends back a response."""

    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        # Start the service listener
        service = await ServiceMessenger.listen(
            server_handle,
            MASTER_NODE,
            INSTANCE_ID,
            NODE_NAME,
            SERVICE_NAME,
        )

        # Allow listener to propagate
        await asyncio.sleep(0.05)

        # Spawn the handler so we can poll concurrently
        async def handle():
            await service.handle_next_request(lambda _request: RESPONSE_PAYLOAD)

        handler = asyncio.create_task(handle())

        # Poll the service as a client
        response = await ServiceMessenger.poll(
            client_handle,
            MASTER_NODE,
            INSTANCE_ID,
            NODE_NAME,
            SERVICE_NAME,
            MASTER_NODE,
            INSTANCE_ID,
            REQUEST_PAYLOAD,
            2.0,
        )

        await handler

        assert response.payload == RESPONSE_PAYLOAD
        assert response.instance_id == INSTANCE_ID
        assert response.master_node == MASTER_NODE

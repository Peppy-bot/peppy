"""
Tests for peppylib ServiceMessenger.

Python equivalent of `service_messenger_communication` in
crates/peppylib/tests/services.rs.
"""

import asyncio

import pytest

from peppylib import MessengerHandle, ServiceMessenger, ZenohdInstance

DAEMON_NODE = "test_daemon"
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
            DAEMON_NODE,
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
            DAEMON_NODE,
            INSTANCE_ID,
            NODE_NAME,
            SERVICE_NAME,
            DAEMON_NODE,
            INSTANCE_ID,
            REQUEST_PAYLOAD,
            2.0,
        )

        await handler

        assert response.payload == RESPONSE_PAYLOAD
        assert response.instance_id == INSTANCE_ID
        assert response.daemon_node == DAEMON_NODE


@pytest.mark.asyncio
async def test_service_poll_rejects_invalid_timeout():
    """poll validates timeout input and raises ValueError for invalid values."""
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        with pytest.raises(ValueError, match="response_timeout_secs"):
            await ServiceMessenger.poll(
                client_handle,
                DAEMON_NODE,
                INSTANCE_ID,
                NODE_NAME,
                SERVICE_NAME,
                DAEMON_NODE,
                INSTANCE_ID,
                REQUEST_PAYLOAD,
                -1.0,
            )


@pytest.mark.asyncio
async def test_service_handler_exception_returns_service_error():
    """Handler exceptions should be returned as protocol service errors, not timeouts."""
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        service = await ServiceMessenger.listen(
            server_handle,
            DAEMON_NODE,
            INSTANCE_ID,
            NODE_NAME,
            SERVICE_NAME,
        )

        await asyncio.sleep(0.05)

        def failing_handler(_request):
            raise RuntimeError("handler boom")

        handler = asyncio.ensure_future(service.handle_next_request(failing_handler))

        with pytest.raises(RuntimeError, match="handler boom"):
            await ServiceMessenger.poll(
                client_handle,
                DAEMON_NODE,
                INSTANCE_ID,
                NODE_NAME,
                SERVICE_NAME,
                DAEMON_NODE,
                INSTANCE_ID,
                REQUEST_PAYLOAD,
                2.0,
            )

        handled = await asyncio.wait_for(handler, timeout=2.0)
        assert handled is True

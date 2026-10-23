"""
Tests for peppylib ServiceMessenger.

Python equivalent of `service_messenger_communication` in
peppy-shared/peppylib-rs/tests/services.rs.
"""

import asyncio
import re

import pytest

from peppylib import (
    FailureError,
    MessengerHandle,
    ProducerRef,
    SenderTarget,
    ServiceMessenger,
    ZenohdInstance,
)

CORE_NODE = "test_core"
INSTANCE_ID = "test_instance"
NODE_NAME = "test_node"
NODE_TAG = "v1"
SERVICE_NAME = "test_service"
REQUEST_PAYLOAD = b"Hello request"
RESPONSE_PAYLOAD = b"Hello response"
# The reserved link_id segment a service queryable is declared under
# (`config::consts::DEFAULT_LINK_ID_SENTINEL`).
DEFAULT_LINK_ID_SENTINEL = "_"


@pytest.mark.asyncio
async def test_service_messenger_communication():
    """A service listener receives a request and sends back a response."""

    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        # Start the service listener
        service = await ServiceMessenger.listen(
            server_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            SERVICE_NAME,
        )

        # Spawn the handler so we can poll concurrently
        async def handle():
            await service.handle_next_request(lambda _request: RESPONSE_PAYLOAD)

        handler = asyncio.create_task(handle())

        # Poll the service as a client
        response = await ServiceMessenger.poll(
            client_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            SERVICE_NAME,
            ProducerRef(CORE_NODE, INSTANCE_ID),
            REQUEST_PAYLOAD,
            2.0,
        )

        await handler

        assert response.payload == RESPONSE_PAYLOAD
        assert response.instance_id == INSTANCE_ID
        assert response.core_node == CORE_NODE


@pytest.mark.asyncio
async def test_service_request_context_exposes_caller_identity_and_body():
    """Every ServiceRequestContext getter reads through to the received request.

    The context's own `link_id` names the producer-side queryable the query
    arrived on, so it differs from the `link_id` of the message it wraps: a query
    keyexpr's caller slots encode no producer link_id.
    """

    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        service = await ServiceMessenger.listen(
            server_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            SERVICE_NAME,
        )

        seen = {}

        def capture(request):
            message = request.message
            seen.update(
                request_id=request.request_id,
                link_id=request.link_id,
                payload=request.payload,
                core_node=request.core_node,
                instance_id=request.instance_id,
                message_payload=message.payload,
                message_core_node=message.core_node,
                message_instance_id=message.instance_id,
                message_link_id=message.link_id,
                message_producer=message.producer,
            )
            return RESPONSE_PAYLOAD

        handler = asyncio.ensure_future(service.handle_next_request(capture))
        await ServiceMessenger.poll(
            client_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            SERVICE_NAME,
            ProducerRef(CORE_NODE, INSTANCE_ID),
            REQUEST_PAYLOAD,
            2.0,
        )
        await handler

        assert seen["payload"] == REQUEST_PAYLOAD
        assert seen["core_node"] == CORE_NODE
        assert seen["instance_id"] == INSTANCE_ID
        # A correlation id minted per request; its shape is opaque to callers.
        assert seen["request_id"]
        assert seen["link_id"] == DEFAULT_LINK_ID_SENTINEL

        # The wrapped message repeats the body and the caller's identity...
        assert seen["message_payload"] == REQUEST_PAYLOAD
        assert seen["message_core_node"] == CORE_NODE
        assert seen["message_instance_id"] == INSTANCE_ID
        assert seen["message_producer"] == ProducerRef(CORE_NODE, INSTANCE_ID)
        # ...but carries no link_id of its own.
        assert seen["message_link_id"] == ""


@pytest.mark.asyncio
async def test_service_poll_rejects_invalid_timeout():
    """poll validates timeout input and raises ValueError for invalid values."""
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        with pytest.raises(ValueError, match="response_timeout_secs"):
            await ServiceMessenger.poll(
                client_handle,
                CORE_NODE,
                INSTANCE_ID,
                SenderTarget.node(NODE_NAME, NODE_TAG),
                SERVICE_NAME,
                ProducerRef(CORE_NODE, INSTANCE_ID),
                REQUEST_PAYLOAD,
                -1.0,
            )


async def _poll_once_against(handler, timeout: float = 2.0):
    """Answers one request with `handler` and polls the service once. Returns
    the poll's exception, or raises when the poll succeeds."""
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        server_handle = await MessengerHandle.from_host_port(router.host, router.port)
        client_handle = await MessengerHandle.from_host_port(router.host, router.port)

        service = await ServiceMessenger.listen(
            server_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            SERVICE_NAME,
        )
        handled = asyncio.ensure_future(service.handle_next_request(handler))

        with pytest.raises(FailureError) as raised:
            await ServiceMessenger.poll(
                client_handle,
                CORE_NODE,
                INSTANCE_ID,
                SenderTarget.node(NODE_NAME, NODE_TAG),
                SERVICE_NAME,
                ProducerRef(CORE_NODE, INSTANCE_ID),
                REQUEST_PAYLOAD,
                timeout,
            )

        assert await asyncio.wait_for(handled, timeout=timeout) is True
        return raised.value


@pytest.mark.asyncio
async def test_a_raised_failure_error_gives_an_error_reply_of_its_kind():
    """A handler that raises FailureError(kind, message) answers the caller
    with an error reply of that kind and message, which the caller's poll
    raises as a FailureError."""

    def refusing_handler(_request):
        raise FailureError("unreachable", "no plan to the target")

    error = await _poll_once_against(refusing_handler)
    assert error.args == ("unreachable", "no plan to the target")


def _raise_in_handler(_request):
    raise RuntimeError("handler boom")


async def _raise_in_async_handler(_request):
    raise RuntimeError("async handler boom")


async def _return_text_from_async_handler(_request):
    return "not bytes"


def _raise_an_invalid_kind(_request):
    raise FailureError("Not A Kind", "never sent")


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("handler", "message"),
    [
        (_raise_in_handler, "^RuntimeError: handler boom$"),
        (_raise_in_async_handler, "^RuntimeError: async handler boom$"),
        (
            _return_text_from_async_handler,
            "^TypeError: Can't extract `str` to `Vec`$",
        ),
        (_raise_an_invalid_kind, "invalid failure kind name"),
    ],
    ids=["raises", "async_raises", "async_returns_text", "invalid_kind"],
)
async def test_any_other_handler_failure_gives_an_attempt_failed_error_reply(
    handler, message
):
    """A handler that raises anything but a valid FailureError, or resolves to
    something other than bytes, answers the caller with an error reply of the
    kind `attempt_failed` that names what went wrong, as fast as a response:
    never a timeout."""
    error = await _poll_once_against(handler)
    kind, text = error.args
    assert kind == "attempt_failed"
    assert re.search(message, text), text


@pytest.mark.asyncio
async def test_service_iface_scoped_native_and_conformed_do_not_collide():
    """Same service name exposed natively AND under an implemented contract must wire to distinct paths."""
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        native_handle = await MessengerHandle.from_host_port(router.host, router.port)
        iface_handle = await MessengerHandle.from_host_port(router.host, router.port)
        caller_handle = await MessengerHandle.from_host_port(router.host, router.port)

        native_response = b"native_ack"
        iface_response = b"iface_ack"

        native_service = await ServiceMessenger.listen(
            native_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            "control",
        )
        iface_service = await ServiceMessenger.listen(
            iface_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.contract("camera", "v1"),
            "control",
        )

        native_handler = asyncio.ensure_future(
            native_service.handle_next_request(lambda _req: native_response)
        )
        iface_handler = asyncio.ensure_future(
            iface_service.handle_next_request(lambda _req: iface_response)
        )

        # Native poll → native handler.
        from_native = await ServiceMessenger.poll(
            caller_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.node(NODE_NAME, NODE_TAG),
            "control",
            ProducerRef(CORE_NODE, INSTANCE_ID),
            b"ping_native",
            2.0,
        )
        assert from_native.payload == native_response

        # Contract poll → contract handler.
        from_iface = await ServiceMessenger.poll(
            caller_handle,
            CORE_NODE,
            INSTANCE_ID,
            SenderTarget.contract("camera", "v1"),
            "control",
            ProducerRef(CORE_NODE, INSTANCE_ID),
            b"ping_iface",
            2.0,
        )
        assert from_iface.payload == iface_response

        await asyncio.wait_for(native_handler, timeout=2.0)
        await asyncio.wait_for(iface_handler, timeout=2.0)

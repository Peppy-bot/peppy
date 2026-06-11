"""
Tests for peppylib TopicMessenger.
"""

import asyncio
import uuid

import pytest

from peppylib import MessengerHandle, QoSProfile, SenderTarget, TopicMessenger, ZenohdInstance

NODE_TAG = "v1"


@pytest.mark.asyncio
async def test_messenger_communication():
    """Check that a topic exposer and subscriber can communicate."""
    # Start an ephemeral router for this test
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        test_id = uuid.uuid4().hex[:8]
        core_node = f"test_core_{test_id}"
        instance_id = f"test_instance_{test_id}"
        node_name = f"test_node_{test_id}"
        topic_name = f"test_topic_{test_id}"
        qos = QoSProfile.Reliable
        payload = b"Hello world"

        receiver_handle = await MessengerHandle.from_host_port(router.host, router.port)
        sender_handle = await MessengerHandle.from_host_port(router.host, router.port)

        # Subscribe to the topic first
        subscription = await TopicMessenger.subscribe(
            receiver_handle,
            core_node,
            instance_id,
            SenderTarget.node(node_name, NODE_TAG),
            topic_name,
            None,  # Accept messages from any core node
            None,  # Accept messages from any instance
            qos,
        )

        # Allow subscription to propagate
        await asyncio.sleep(0.05)

        # Emit a message. Void async bindings resolve to `None` (not the empty
        # tuple a bare `Ok(())` would yield under PyO3 0.28).
        emit_result = await TopicMessenger.emit(
            sender_handle,
            core_node,
            instance_id,
            SenderTarget.node(node_name, NODE_TAG),
            topic_name,
            qos,
            payload,
        )
        assert emit_result is None

        # Receive the message with a timeout
        message = await asyncio.wait_for(
            subscription.on_next_message(),
            timeout=2.0,
        )

        assert message is not None, "Expected to receive a message"
        assert message.payload == payload, (
            f"Expected payload {payload!r}, got {message.payload!r}"
        )
        assert message.instance_id == instance_id
        assert message.core_node == core_node


@pytest.mark.asyncio
async def test_loaned_publish_round_trip():
    """A loan filled through a writable memoryview round-trips end to end.

    With shared memory on (the default) and a payload at or above the publish
    threshold the bytes are written once into the transport's shared-memory
    segment and never copied again; the same code transparently uses a heap
    buffer when shared memory is off or unavailable.
    """
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        test_id = uuid.uuid4().hex[:8]
        core_node = f"test_core_{test_id}"
        instance_id = f"test_instance_{test_id}"
        node_name = f"test_node_{test_id}"
        topic_name = f"test_topic_{test_id}"
        qos = QoSProfile.Reliable
        # Above the SHM publish threshold (4 KiB), non-uniform fill.
        payload = bytes(i % 251 for i in range(64 * 1024))

        receiver_handle = await MessengerHandle.from_host_port(router.host, router.port)
        sender_handle = await MessengerHandle.from_host_port(router.host, router.port)

        subscription = await TopicMessenger.subscribe(
            receiver_handle,
            core_node,
            instance_id,
            SenderTarget.node(node_name, NODE_TAG),
            topic_name,
            None,
            None,
            qos,
        )
        await asyncio.sleep(0.05)

        publisher = await TopicMessenger.declare_publisher(
            sender_handle,
            core_node,
            instance_id,
            SenderTarget.node(node_name, NODE_TAG),
            topic_name,
            qos,
        )

        loan = publisher.loan(len(payload))
        assert len(loan) == len(payload)
        loan_is_shm = loan.is_shm
        view = memoryview(loan)
        assert not view.readonly
        view[:] = payload
        if loan_is_shm:
            assert loan.is_shm
        else:
            assert bytes(view) == payload

        # Publishing while a view is exported must refuse loudly rather than
        # free the buffer out from under the view.
        with pytest.raises(BufferError):
            await publisher.publish_loaned(loan)

        view.release()
        publish_result = await publisher.publish_loaned(loan)
        assert publish_result is None

        # The loan is consumed: a second publish is a clean error.
        with pytest.raises(ValueError):
            await publisher.publish_loaned(loan)

        message = await asyncio.wait_for(subscription.on_next_message(), timeout=2.0)
        assert message is not None
        assert message.payload == payload


@pytest.mark.asyncio
async def test_loan_view_released_on_worker_thread_still_publishes():
    """A memoryview released on a worker thread must not brick the loan.

    Worker-thread fills (`asyncio.to_thread`, numpy wrappers) routinely drop
    the last view reference off the main thread; the buffer-protocol export
    count must stay correct and the subsequent publish must succeed.
    """
    import threading

    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        test_id = uuid.uuid4().hex[:8]
        core_node = f"test_core_{test_id}"
        instance_id = f"test_instance_{test_id}"
        node_name = f"test_node_{test_id}"
        topic_name = f"test_topic_{test_id}"
        qos = QoSProfile.Reliable
        payload = bytes(i % 251 for i in range(64 * 1024))

        receiver_handle = await MessengerHandle.from_host_port(router.host, router.port)
        sender_handle = await MessengerHandle.from_host_port(router.host, router.port)

        subscription = await TopicMessenger.subscribe(
            receiver_handle,
            core_node,
            instance_id,
            SenderTarget.node(node_name, NODE_TAG),
            topic_name,
            None,
            None,
            qos,
        )
        await asyncio.sleep(0.05)

        publisher = await TopicMessenger.declare_publisher(
            sender_handle,
            core_node,
            instance_id,
            SenderTarget.node(node_name, NODE_TAG),
            topic_name,
            qos,
        )

        loan = publisher.loan(len(payload))
        view = memoryview(loan)
        view[:] = payload
        releaser = threading.Thread(target=view.release)
        releaser.start()
        releaser.join()
        del view

        await publisher.publish_loaned(loan)
        message = await asyncio.wait_for(subscription.on_next_message(), timeout=2.0)
        assert message is not None
        assert message.payload == payload


@pytest.mark.asyncio
async def test_truncated_loan_sends_only_the_prefix():
    """Over-allocate, fill a prefix, truncate: only the prefix travels."""
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        test_id = uuid.uuid4().hex[:8]
        core_node = f"test_core_{test_id}"
        instance_id = f"test_instance_{test_id}"
        node_name = f"test_node_{test_id}"
        topic_name = f"test_topic_{test_id}"
        qos = QoSProfile.Reliable
        prefix = bytes(i % 251 for i in range(5000))

        receiver_handle = await MessengerHandle.from_host_port(router.host, router.port)
        sender_handle = await MessengerHandle.from_host_port(router.host, router.port)

        subscription = await TopicMessenger.subscribe(
            receiver_handle,
            core_node,
            instance_id,
            SenderTarget.node(node_name, NODE_TAG),
            topic_name,
            None,
            None,
            qos,
        )
        await asyncio.sleep(0.05)

        publisher = await TopicMessenger.declare_publisher(
            sender_handle,
            core_node,
            instance_id,
            SenderTarget.node(node_name, NODE_TAG),
            topic_name,
            qos,
        )

        loan = publisher.loan(2 * len(prefix))
        loan_is_shm = loan.is_shm
        if loan_is_shm:
            assert loan.is_shm
        with memoryview(loan) as view:
            view[: len(prefix)] = prefix
        loan.truncate(len(prefix))
        assert len(loan) == len(prefix)
        assert loan.is_shm == loan_is_shm
        await publisher.publish_loaned(loan)

        message = await asyncio.wait_for(subscription.on_next_message(), timeout=2.0)
        assert message is not None
        assert message.payload == prefix

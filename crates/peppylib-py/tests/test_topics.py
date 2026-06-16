"""
Tests for peppylib TopicMessenger.
"""

import asyncio
import uuid

import pytest

from peppylib import (
    MessengerHandle,
    ProducerRef,
    QoSProfile,
    SenderTarget,
    TopicMessenger,
    ZenohdInstance,
)

NODE_TAG = "v1"


def test_producer_ref_is_structured_and_hashable():
    """`ProducerRef` exposes named fields and works as a dict key.

    A `from_any` consumer keys per-producer state on the returned identity, so
    the type must be hashable and compare by value (mirrors the Rust
    `HashMap<ProducerRef, _>` idiom).
    """
    producer = ProducerRef("core_a", "inst_1")
    assert producer.core_node == "core_a"
    assert producer.instance_id == "inst_1"

    # Value equality + hashing, so equal identities collapse to one dict key.
    same = ProducerRef("core_a", "inst_1")
    other = ProducerRef("core_a", "inst_2")
    assert producer == same
    assert producer != other
    assert hash(producer) == hash(same)

    frames_by_producer = {producer: "frame"}
    assert frames_by_producer[ProducerRef("core_a", "inst_1")] == "frame"

    assert repr(producer) == 'ProducerRef("core_a", "inst_1")'


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
            None,  # Accept messages from any producer (discover-then-pin)
            qos,
        )

        # Allow subscription to propagate
        await asyncio.sleep(0.05)

        # Declare the publisher (the only topic-publish path) and publish a
        # message. Void async bindings resolve to `None` (not the empty tuple a
        # bare `Ok(())` would yield under PyO3 0.28).
        publisher = await TopicMessenger.declare_publisher(
            sender_handle,
            core_node,
            instance_id,
            SenderTarget.node(node_name, NODE_TAG),
            topic_name,
            qos,
        )
        publish_result = await publisher.publish(payload)
        assert publish_result is None

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

        # The structured producer identity mirrors the flat accessors and is
        # what generated consumed-topic callbacks return.
        assert message.producer == ProducerRef(core_node, instance_id)
        assert message.producer.core_node == core_node
        assert message.producer.instance_id == instance_id

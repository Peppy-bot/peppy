"""
Tests for peppylib TopicMessenger.
"""

import asyncio
import uuid

import pytest

from peppylib import MessengerHandle, TopicMessenger, QoSProfile, ZenohdInstance


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
            node_name,
            "_",  # iface_name (native)
            "_",  # iface_tag (native)
            topic_name,
            None,  # Accept messages from any core node
            None,  # Accept messages from any instance
            qos,
        )

        # Allow subscription to propagate
        await asyncio.sleep(0.05)

        # Emit a message
        await TopicMessenger.emit(
            sender_handle,
            core_node,
            instance_id,
            node_name,
            "_",  # iface_name (native)
            "_",  # iface_tag (native)
            topic_name,
            qos,
            payload,
        )

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

"""
Tests for peppylib TopicMessenger.
"""

import asyncio
import uuid

import pytest

from peppylib import (
    MessengerHandle,
    ObservedSource,
    PeerInfo,
    PeerMember,
    ProducerRef,
    QoSProfile,
    SenderTarget,
    TopicMessenger,
    ZenohdInstance,
)

NODE_TAG = "v1"
# The producer-side link_id segment a publisher declared without a link_id
# carries on the wire (`config::consts::DEFAULT_LINK_ID_SENTINEL`).
DEFAULT_LINK_ID_SENTINEL = "_"


def test_producer_ref_is_structured_and_hashable():
    """`ProducerRef` exposes named fields and works as a dict key.

    Consumers key per-slot state on the returned identity, so the type must
    be hashable and compare by value (mirrors the Rust
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


def test_observed_source_is_structured_and_hashable():
    """`ObservedSource` exposes named fields and works as a dict key.

    An observed subscription tags every message with it, and two members of
    one slot can share an instance and differ only in the source link_id, so
    per-member demux keys a dict on the full identity (mirrors the Rust
    `HashMap<ObservedSource, _>` idiom).
    """
    left = ObservedSource(ProducerRef("core_a", "backbone_1"), "left_arm")
    right = ObservedSource(ProducerRef("core_a", "backbone_1"), "right_arm")
    assert left.producer == ProducerRef("core_a", "backbone_1")
    assert left.source_link_id == "left_arm"

    same = ObservedSource(ProducerRef("core_a", "backbone_1"), "left_arm")
    assert left == same
    assert left != right
    assert hash(left) == hash(same)

    handlers = {left: "left", right: "right"}
    assert len(handlers) == 2
    assert handlers[same] == "left"

    assert repr(left) == (
        'ObservedSource(producer=ProducerRef("core_a", "backbone_1"), '
        'source_link_id="left_arm", peer=None)'
    )

    # A member pinned to one pair of a slot carries the pair's other end, and
    # is distinct from the member observing the whole slot.
    alpha = PeerInfo(ProducerRef("core_a", "alpha_backbone_inst"), "left_arm_link")
    pinned = ObservedSource(ProducerRef("core_a", "engine_1"), "left_arm", alpha)
    assert pinned.peer == alpha
    assert left.peer is None
    assert pinned != ObservedSource(ProducerRef("core_a", "engine_1"), "left_arm")
    assert repr(pinned) == (
        'ObservedSource(producer=ProducerRef("core_a", "engine_1"), '
        'source_link_id="left_arm", peer=PeerInfo(producer=ProducerRef("core_a", '
        '"alpha_backbone_inst"), peer_link_id="left_arm_link"))'
    )


def test_peer_info_is_structured_and_hashable():
    """`PeerInfo` exposes named fields and works as a dict key.

    A peer subscription tags every message with it, the same identity
    `PeerSlot.paired()` returns.
    """
    peer = PeerInfo(ProducerRef("core_a", "arm_1"), "controller")
    assert peer.producer == ProducerRef("core_a", "arm_1")
    assert peer.peer_link_id == "controller"

    same = PeerInfo(ProducerRef("core_a", "arm_1"), "controller")
    other = PeerInfo(ProducerRef("core_a", "arm_1"), "follower")
    assert peer == same
    assert peer != other
    assert hash(peer) == hash(same)

    state_by_peer = {peer: "paired"}
    assert state_by_peer[same] == "paired"

    assert repr(peer) == (
        'PeerInfo(producer=ProducerRef("core_a", "arm_1"), '
        'peer_link_id="controller")'
    )


def test_peer_member_carries_the_copy_beside_the_identity():
    """`PeerMember` is a pair as a slot holds it: the peer's identity plus
    the copy the peer belongs to, which is `None` outside a copy."""
    info = PeerInfo(ProducerRef("core_a", "bravo_backbone_inst"), "left_arm_link")
    member = PeerMember(info, "bravo")
    assert member.info == info
    assert member.copy == "bravo"
    assert PeerMember(info).copy is None
    assert member == PeerMember(info, "bravo")
    assert member != PeerMember(info)
    assert repr(member) == (
        'PeerMember(info=PeerInfo(producer=ProducerRef("core_a", "bravo_backbone_inst"), '
        'peer_link_id="left_arm_link"), copy="bravo")'
    )
    assert repr(PeerMember(info)).endswith("copy=None)")


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

        # Subscribe to the topic first, pinned to the publishing producer.
        subscription = await TopicMessenger.subscribe(
            receiver_handle,
            core_node,
            instance_id,
            SenderTarget.node(node_name, NODE_TAG),
            topic_name,
            ProducerRef(core_node, instance_id),
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

        # No link_id was bound on the publisher, so the keyexpr carries the
        # default sentinel rather than a slot name.
        assert message.link_id == DEFAULT_LINK_ID_SENTINEL


@pytest.mark.asyncio
async def test_message_exposes_the_producers_bound_link_id():
    """A publisher bound to a link_id surfaces it on the received message.

    The Rust pairing forwarding path re-checks a message's core_node,
    instance_id and link_id against the slot's pin before delivering it. This
    is the accessor that lets a Python node make the same check, so the two
    language bindings surface identical message identity.
    """
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        test_id = uuid.uuid4().hex[:8]
        core_node = f"test_core_{test_id}"
        instance_id = f"test_instance_{test_id}"
        node_name = f"test_node_{test_id}"
        topic_name = f"test_topic_{test_id}"
        link_id = "arm"
        qos = QoSProfile.Reliable

        receiver_handle = await MessengerHandle.from_host_port(router.host, router.port)
        sender_handle = await MessengerHandle.from_host_port(router.host, router.port)

        subscription = await TopicMessenger.subscribe(
            receiver_handle,
            core_node,
            instance_id,
            SenderTarget.node(node_name, NODE_TAG),
            topic_name,
            ProducerRef(core_node, instance_id),
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
            link_id,
        )
        await publisher.publish(b"joint states")

        message = await asyncio.wait_for(subscription.on_next_message(), timeout=2.0)
        assert message is not None, "Expected to receive a message"
        assert message.link_id == link_id
        # The rest of the identity is unchanged by binding a link_id.
        assert message.core_node == core_node
        assert message.instance_id == instance_id
        assert message.producer == ProducerRef(core_node, instance_id)


@pytest.mark.asyncio
async def test_subscribe_rejects_producer_list():
    """A list of producers raises `TypeError`.

    A slot binds exactly one producer; fan-in is N declared slots. The
    subscribe seam takes a single `ProducerRef`, so a list — even a
    single-element one — must fail loudly instead of silently degrading.
    """
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        test_id = uuid.uuid4().hex[:8]
        handle = await MessengerHandle.from_host_port(router.host, router.port)
        core_node = f"test_core_{test_id}"
        instance_id = f"test_instance_{test_id}"

        for producers in ([], [ProducerRef(core_node, instance_id)]):
            with pytest.raises(TypeError):
                await TopicMessenger.subscribe(
                    handle,
                    core_node,
                    instance_id,
                    SenderTarget.node(f"test_node_{test_id}", NODE_TAG),
                    f"test_topic_{test_id}",
                    producers,
                    QoSProfile.Reliable,
                )

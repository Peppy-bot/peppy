@0xc4e8a17d92b5f3d1;

# Pairing-slot delivery for the framework `peer_update` service.
#
# The daemon pushes ABSOLUTE slot state (never deltas) for one pairing slot:
# every pair the slot holds, in establishment order. A delivery replaces the
# slot wholesale, so a peer the delivery omits is gone from the slot; a scalar
# slot carries zero or one member. `sequence` orders deliveries so a retried
# request can never roll a slot back: the node rejects strictly-smaller
# sequences (`staleSequence = true`) and treats an equal sequence as an
# idempotent retry.

# The node replies with the shared `SlotUpdateResponse` (see slot_update.capnp).
struct PeerUpdateRequest {
    linkId @0 :Text;
    # The receiving node's own pairing-slot link_id being updated.
    sequence @1 :UInt64;
    members @2 :List(PeerMember);
}

struct PeerMember {
    peerCoreNode @0 :Text;
    peerInstanceId @1 :Text;
    peerLinkId @2 :Text;
    # The link_id of the peer's complementary slot (its producer-side wire
    # segment). With `peerCoreNode` and `peerInstanceId` it is this member's
    # identity within the slot.
    copy @3 :Text;
    # The copy the peer's instance belongs to; empty for an instance run
    # outside a copy.
}

@0xd94b2f8a61c7e350;

# Observation-slot delivery for the framework `observation_update` service.
#
# The daemon pushes ABSOLUTE observation state (never deltas) for one observer
# slot: the slot's complete ordered member set, in the order the plan listed it.
# A delivery replaces the slot wholesale, so a member the delivery omits is gone
# from the slot. `sequence` orders deliveries so a retried request can never roll
# a slot back: the node rejects strictly-smaller sequences (`staleSequence =
# true`) and treats an equal sequence as an idempotent retry.

# The node replies with the shared `SlotUpdateResponse` (see slot_update.capnp).
struct ObservationUpdateRequest {
    linkId @0 :Text;
    # The receiving node's own observer-slot link_id being updated.
    sequence @1 :UInt64;
    # Every pairing this slot observes right now, in plan order. Empty both
    # before the daemon has resolved the slot and for a `zero_or_more` slot the
    # plan left with nothing to observe.
    members @2 :List(ObservedMember);
}

struct ObservedMember {
    sourceCoreNode @0 :Text;
    sourceInstanceId @1 :Text;
    # The producer-side link_id of the observed pairing slot (its wire segment).
    # With `sourceCoreNode` and `sourceInstanceId` it is this member's identity
    # within the slot, so one source instance observed through two of its own
    # pairing slots is two members.
    sourceLinkId @2 :Text;
    # A semantic counter, separate from `sequence`. It advances only when this
    # member's source changes incarnation (never on the source's own peer
    # transitions), and is the sole discriminator between an old and a new
    # incarnation of the same source instance, whose publishes are
    # byte-identical on the wire.
    sourceGeneration @3 :UInt64;
    # Whether this member's source instance is currently in a non-terminal
    # state.
    sourceLive @4 :Bool;
    # The pair's other end when the plan named the pair by it: the peer the
    # source publishes to and the link_id of the peer's slot. Absent, the
    # member observes every pair of the source's slot.
    peer @5 :ObservedPeer;
}

struct ObservedPeer {
    coreNode @0 :Text;
    instanceId @1 :Text;
    linkId @2 :Text;
}

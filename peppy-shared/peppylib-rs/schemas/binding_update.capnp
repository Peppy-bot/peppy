@0xb7c4e2a91f3d6058;

# Producer-binding delivery for the framework `binding_update` service.
#
# The daemon pushes ABSOLUTE binding state (never deltas) for one consumer
# slot: the slot's complete ordered producer set, in the order the plan listed
# it. A delivery replaces the slot wholesale, so a producer the delivery omits
# is gone from the slot. `sequence` orders deliveries so a retried request can
# never roll a slot back: the node rejects strictly-smaller sequences
# (`staleSequence = true`) and treats an equal sequence as an idempotent retry.

# The node replies with the shared `SlotUpdateResponse` (see slot_update.capnp).
struct BindingUpdateRequest {
    linkId @0 :Text;
    # The receiving node's own consumer-slot link_id being updated.
    sequence @1 :UInt64;
    producers @2 :List(BoundMember);
    # Every member bound to this slot right now, in plan order.
}

struct BoundMember {
    coreNode @0 :Text;
    instanceId @1 :Text;
    copy @2 :Text;
    # The copy the producer's instance belongs to; empty for an instance run
    # outside a copy.
    instanceIdInCopy @3 :Text;
    # The id the copy's fragment wrote for the instance, which the copy runs
    # as `<copy>_<instanceIdInCopy>`. Set exactly when `copy` is; the node
    # refuses a delivery that names one without the other.
}

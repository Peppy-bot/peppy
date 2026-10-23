@0xe5c1a7f39b2d84f0;

# Shared node-side reply for the framework slot-update services
# (`peer_update`, `observation_update` and `binding_update`). Each delivers
# ABSOLUTE slot state with a sequence number and takes the same reply. A node
# that refuses a delivery (an unknown slot, a member count the slot's
# cardinality forbids, a sequence it already holds with other state, a
# caller that is not its daemon) gives an error reply with the failure kind
# `rejected` and the reason.

struct SlotUpdateResponse {
    staleSequence @0 :Bool;
    # True when the request's sequence was strictly older than the slot's
    # current one (a delayed retry): the node kept the state it holds, and
    # the daemon treats the delivery as already superseded.
}

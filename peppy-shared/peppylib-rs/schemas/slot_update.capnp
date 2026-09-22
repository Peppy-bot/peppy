@0xe5c1a7f39b2d84f0;

# Shared node-side ack for the framework slot-update services (`peer_update`,
# `observation_update` and `binding_update`). Each delivers ABSOLUTE slot state
# with a sequence number and takes the same reply: `accepted = false` with
# `staleSequence = true` means the request's sequence was strictly older than
# the slot's current one (a delayed retry), which the daemon treats as already
# superseded and reports as delivered.

struct SlotUpdateResponse {
    accepted @0 :Bool;
    staleSequence @1 :Bool;
    message @2 :Text;
    # Human-readable rejection reason when `accepted = false` (unknown slot,
    # stale sequence). Empty on success.
}

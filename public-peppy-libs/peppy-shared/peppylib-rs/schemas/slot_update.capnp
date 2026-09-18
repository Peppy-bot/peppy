@0xe5c1a7f39b2d84f0;

# Shared node-side ack for the framework slot-update services (`peer_update`
# and `observation_update`). Both deliver ABSOLUTE slot state with a sequence
# number and take the same reply: `accepted = false` with `staleSequence = true`
# means the request's sequence was strictly older than the slot's current one (a
# delayed retry), which the daemon treats as already-superseded rather than a
# failure to revert.

struct SlotUpdateResponse {
    accepted @0 :Bool;
    staleSequence @1 :Bool;
    message @2 :Text;
    # Human-readable rejection reason when `accepted = false` (unknown slot,
    # stale sequence). Empty on success.
}

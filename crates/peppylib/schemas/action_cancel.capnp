@0x9b3d7e1c5a4f2086;

# Fixed cancel-ack response for the concurrent-action engine.
#
# The engine answers cancel requests itself (the worker reacts to a cancel
# signal; it does not produce this payload), so the bytes are encoded here in
# peppylib once and reused for both Rust and Python servers.
#
# The single `state` field carries the CancelState tag
#     state @0 :UInt8 (CancelState: 0=Signalled, 1=AlreadyTerminal, 2=Unknown).
# Both Rust and Python generated clients decode this via peppylib
# (`decode_cancel_ack`), so there is no separate per-action cancel schema to
# keep positionally compatible. A round-trip test pins encode/decode.

struct ActionCancelResponse {
    state @0 :UInt8;
}

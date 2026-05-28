@0x9b3d7e1c5a4f2086;

# Fixed cancel-ack response for the concurrent-action engine.
#
# The engine answers cancel requests itself (the worker reacts to a cancel
# signal; it does not produce this payload), so the bytes are encoded here in
# peppylib once and reused for both Rust and Python servers.
#
# Field layout MUST stay positionally wire-compatible with the codegen's
# per-action `CancelResponse`, whose single source of truth is
# generator-internal `cancel_action_response_format()`:
#     accepted @0 :Bool, error_message @1 :Text (optional).
# Cap'n Proto's wire format is positional (no schema id / field names are
# serialized), so identical field order + types is sufficient for the
# generated client's reader to decode it. A round-trip test pins this.

struct ActionCancelResponse {
    accepted @0 :Bool;
    errorMessage @1 :Text;
}

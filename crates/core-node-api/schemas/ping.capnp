@0xa8f5c2b9e4d3f1a7;

# Ping message structures for core-node services

struct PingRequest {
    # Optional timestamp for round-trip time measurement
    timestamp @0 :UInt64;
}

struct PingResponse {
    # Echo back the timestamp if provided
    timestamp @0 :UInt64;
    # The response message
    message @1 :Text;
}

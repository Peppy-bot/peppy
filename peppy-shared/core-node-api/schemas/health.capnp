@0xbf2e9c4a7d1e6b83;

# Health message structures for the core-node /health service.

struct HealthRequest {
    # No fields: the probe is a liveness check, so a well-formed reply is the
    # signal. Kept as a struct so the request type can grow without a wire break.
}

struct HealthResponse {
    # Daemon health status, "healthy" today.
    status @0 :Text;
    # Core-node uptime in whole seconds since the daemon started.
    uptimeSecs @1 :UInt64;
}

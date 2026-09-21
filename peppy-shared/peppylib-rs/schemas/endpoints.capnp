@0xb7c3e1d94a6f2c15;

# The `node_endpoints` framework service: the sockets a node bound during
# setup, one per label its manifest declares under `execution.endpoints`.

struct NodeEndpointsRequest {}

struct EndpointBinding {
    label @0 :Text;
    # URI scheme token the node serves under, e.g. "http" or "https".
    scheme @1 :Text;
    # IP literal as bound, e.g. "0.0.0.0", "::", "127.0.0.1".
    host @2 :Text;
    port @3 :UInt16;
    # "" or a path starting with '/'.
    path @4 :Text;
}

struct NodeEndpointsResponse {
    endpoints @0 :List(EndpointBinding);
}

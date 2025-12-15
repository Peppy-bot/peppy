@0xb9c4e2a1f3d5a8b7;

# Info message structures for master-node services

struct InfoRequest {
}

struct InfoResponse {
    uptimeSecs @0 :UInt64;
    masterNodeName @1 :Text;
    masterNodeInstanceId @2 :Text;
    hostName @3 :Text;
    nodeCount @4 :UInt32;
}

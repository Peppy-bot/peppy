@0xb9c4e2a1f3d5a8b7;

# Info message structures for daemon-node services

struct InfoRequest {
}

struct ContainerInfo {
    apptainerVersion @0 :Text;
    limaVersion @1 :Text;
}

struct InfoResponse {
    uptimeSecs @0 :UInt64;
    daemonNodeName @1 :Text;
    daemonNodeInstanceId @2 :Text;
    hostName @3 :Text;
    nodeCount @4 :UInt32;
    gitVersion @5 :Text;
    containerInfo @6 :ContainerInfo;
}

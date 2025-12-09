@0xb9c4e2a1f3d5a8b7;

# Info message structures for master-node services

enum InfoType {
    uptime @0;
    masterNodeName @1;
    masterNodeInstanceId @2;
}

struct InfoRequest {
    # Type of info requested
    infoType @0 :InfoType;
}

struct InfoResponse {
    # Type of info returned
    infoType @0 :InfoType;
    # The value of the requested info (string representation)
    value @1 :Text;
}

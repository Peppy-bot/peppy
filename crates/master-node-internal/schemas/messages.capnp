@0xa8f5c2b9e4d3f1a6;

# Common message structures for master-node services

struct StatusRequest {
    # Optional: request specific status information
    includeDetails @0 :Bool;
}

struct StatusResponse {
    # Overall status: "ok", "degraded", "error"
    status @0 :Text;
    # Optional details about the status
    details @1 :Text;
    # Uptime in seconds
    uptimeSeconds @2 :UInt64;
}

struct LauncherRequest {
    # JSON5-encoded PeppyLauncher configuration
    peppyLauncherJson5 @0 :Text;
}

struct LauncherResponse {
    # Whether the launcher was successful
    success @0 :Bool;
    # Error message if failed (empty if successful)
    errorMessage @1 :Text;
}

struct AddNodeRequest {
    # Name of the node to add
    nodeName @0 :Text;
    # Node type/role
    nodeType @1 :Text;
    # Node address
    address @2 :Text;
}

struct AddNodeResponse {
    # Whether the node was added successfully
    success @0 :Bool;
    # Assigned node ID
    nodeId @1 :Text;
    # Error message if failed
    errorMessage @2 :Text;
}

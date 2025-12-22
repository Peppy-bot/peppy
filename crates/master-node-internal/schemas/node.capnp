@0xa8f5c2b9e4d3f1aa;

# Node message structures for master-node services

# Node List service
struct NodeListRequest {
    # Empty for now - could add filters later
}

struct NodeListResponse {
    # DOT graph representation of the node stack
    dotGraph @0 :Text;
}

# Node Add service
struct NodeAddRequest {
    # Peppy configuration in JSON5 format
    peppyJson5 @0 :Text;
    # Directory the configuration was loaded from
    fromDir @1 :Text;
    # Optional instance ID to use for the node
    instanceId @2 :Text;
    # Whether to run the node immediately after adding
    runImmediately @3 :Bool;
}

struct NodeAddResponse {
    # Whether the node was added successfully
    success @0 :Bool;
    # Assigned node instance ID
    nodeInstanceId @1 :Text;
    # Error message if failed
    errorMessage @2 :Text;
    # Whether the node is currently running
    isRunning @3 :Bool;
}

# Node Init service
struct NodeInitRequest {
    # Root directory where the node will be created
    nodeRootDir @0 :Text;
    # Build system ("rust", "cargo", "python", or "uv")
    buildSystem @1 :Text;
    # Name of the node (used for directory and package name)
    nodeName @2 :Text;
}

struct NodeInitResponse {
    # Whether the init was successful
    success @0 :Bool;
    # Error message if failed
    errorMessage @1 :Text;
}

# Node Sync service
struct NodeSyncRequest {
    # Root directory of the node/workspace
    nodeRootDir @0 :Text;
    # Generator language ("rust" or "python")
    language @1 :Text;
}

struct NodeSyncResponse {
    # Whether the sync was successful
    success @0 :Bool;
    # Error message if failed
    errorMessage @1 :Text;
}

# Node Run service
struct NodeRunRequest {
    # Instance ID of the node to run
    instanceId @0 :Text;
}

struct NodeRunResponse {
    # Whether the run was successful
    success @0 :Bool;
    # Error message if failed (optional)
    errorMessage @1 :Text;
}

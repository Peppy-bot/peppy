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
}

struct NodeAddResponse {
    # Whether the node was added successfully
    success @0 :Bool;
    # Assigned node ID
    nodeId @1 :Text;
    # Error message if failed
    errorMessage @2 :Text;
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

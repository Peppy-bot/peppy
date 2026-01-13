@0xa8f5c2b9e4d3f1aa;

# Node message structures for master-node services

# Node List service
struct NodeListRequest {
    # If true, include DOT graph representation in the response
    withDotGraph @0 :Bool;
}

struct NodeListResponse {
    # DOT graph representation of the node stack
    dotGraph @0 :Text;
    # JSON-serialized graph representation (SerializedNodeGraph)
    graphJson @1 :Text;
}

# Node Add Action (streaming version with feedback)
struct NodeAddGoal {
    # Directory the configuration was loaded from
    fromDir @0 :Text;
}

struct NodeAddFeedback {
    # Type of output: "stdout" or "stderr"
    stream @0 :Text;
    # The line of output
    line @1 :Text;
}

struct NodeAddResult {
    # Whether the node was added successfully
    success @0 :Bool;
    # Error message if failed
    errorMessage @1 :Text;
    # Path where the node was copied (empty on failure)
    snapshotPath @2 :Text;
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

# Node Generate service
struct NodeGenerateRequest {
    # Root directory of the node/workspace
    nodeRootDir @0 :Text;
    # Generator language ("rust" or "python")
    language @1 :Text;
}

struct NodeGenerateResponse {
    # Whether the generate was successful
    success @0 :Bool;
    # Error message if failed
    errorMessage @1 :Text;
}

struct NodeStartGoal {
    # Runtime configuration in JSON5 format (PEPPY_RUNTIME_CONFIG)
    runtimeConfigJson5 @0 :Text;
    # Name of the node to start
    nodeName @1 :Text;
    # Tag of the node to start
    tag @2 :Text;
}

struct NodeStartFeedback {
    # Type of output: "stdout" or "stderr"
    stream @0 :Text;
    # The line of output
    line @1 :Text;
}

struct NodeStartResult {
    # Whether the run was successful
    success @0 :Bool;
    # Error message if failed (optional)
    errorMessage @1 :Text;
}

# Node Stop service
struct NodeStopRequest {
    # Instance ID of the node to stop
    instanceId @0 :Text;
}

struct NodeStopResponse {
    # Whether the stop was successful
    success @0 :Bool;
    # Error message if failed (optional)
    errorMessage @1 :Text;
}

# Node Remove service
struct NodeRemoveRequest {
    # Name of the node to remove
    nodeName @0 :Text;
    # If set, will attempt to stop all the instances associated with this node first before removing the node
    stopInstances @1 :Bool;
    # Tag of the node to remove
    tag @2 :Text;
}

struct NodeRemoveResponse {
    # Whether the removal was successful
    success @0 :Bool;
    # Error message if failed (optional)
    errorMessage @1 :Text;
}

# Node Reset service
struct NodeResetRequest {
}

struct NodeResetResponse {
    # Whether the reset was successful
    success @0 :Bool;
    # Error message if failed (optional)
    errorMessage @1 :Text;
}

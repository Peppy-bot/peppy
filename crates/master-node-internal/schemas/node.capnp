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
    # Git commit hash of the node being added
    gitHash @0 :Text;
    # Source of the node (filesystem path or git repository)
    source :union {
        # Filesystem path to the node directory
        fs @1 :Text;
        # Git repository source
        git @2 :NodeAddGitSource;
        # HTTP URL source
        http @3 :Text;
    }
}

struct NodeAddGitSource {
    # URL of the git repository
    repoUrl @0 :Text;
    # Path within the repository to the node
    repoPath @1 :Text;
}

struct NodeAddGoalResponse {
    # Whether the goal was accepted
    accepted @0 :Bool;
    # Path to the log file (empty if rejected)
    logPath @1 :Text;
    # Rejection reason (empty if accepted)
    rejectionReason @2 :Text;
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
    # Path to the log file containing stdout/stderr output
    logPath @3 :Text;
}

# Node Init service
struct NodeInitRequest {
    # Root directory where the node will be created
    nodeRootDir @0 :Text;
    # Name of the node (used for directory and package name)
    nodeName @1 :Text;
    # Git commit hash of the node being initialized
    gitHash @2 :Text;
    # Toolchain to use for the node ("cargo" or "uv")
    toolchain @3 :Text;
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
    # Git commit hash of the node being synced
    gitHash @1 :Text;
}

struct NodeSyncResponse {
    # Whether the sync was successful
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

struct NodeStartGoalResponse {
    # Whether the goal was accepted
    accepted @0 :Bool;
    # Path to the log file (empty if rejected)
    logPath @1 :Text;
    # Rejection reason (empty if accepted)
    rejectionReason @2 :Text;
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
    # Process ID of the started node (0 if not available or failed)
    pid @2 :UInt32;
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

@0xa8f5c2b9e4d3f1aa;

# Node message structures for core-node services

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
    # Optional SHA256 checksum for HTTP sources
    httpSha256 @7 :Text;
    # Environment variables to apply when executing build_cmd (e.g. PATH)
    envVars @4 :List(EnvVar);
    # Timeout in seconds for the add operation (used to report remaining time when busy)
    timeoutSecs @5 :UInt64;
    # Optional variant source — when set, the main source points to the root node
    # and this identifies which variant to resolve and build.
    # Fs = variant name (lookup in manifest), Git/Http = direct source.
    variant @6 :NodeAddVariantSource;
    # When true, cancel any in-progress add action and start a new one
    force @8 :Bool;
}

struct EnvVar {
    key @0 :Text;
    value @1 :Text;
}

struct NodeAddGitSource {
    # URL of the git repository
    repoUrl @0 :Text;
    # Path within the repository to the node
    repoPath @1 :Text;
    # Optional git ref (tag/branch/commit) to checkout before reading repoPath
    repoRef @2 :Text;
}

struct NodeAddVariantSource {
    source :union {
        # Variant name (lookup in root manifest)
        fs @0 :Text;
        # Direct git source for the variant
        git @1 :NodeAddGitSource;
        # Direct HTTP URL for the variant
        http @2 :Text;
    }
    # Optional SHA256 checksum for HTTP variant sources
    httpSha256 @3 :Text;
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
    # Path to the log file containing stdout/stderr output
    logPath @2 :Text;
    # Name of the added node (empty on failure)
    nodeName @3 :Text;
    # Tag of the added node (empty on failure)
    nodeTag @4 :Text;
}

# Node Build Action — drives the build of a previously-added node
struct NodeBuildGoal {
    nodeName @0 :Text;
    nodeTag @1 :Text;
    envVars @2 :List(EnvVar);
    timeoutSecs @3 :UInt64;
    force @4 :Bool;
}

struct NodeBuildGoalResponse {
    accepted @0 :Bool;
    logPath @1 :Text;
    rejectionReason @2 :Text;
}

struct NodeBuildFeedback {
    stream @0 :Text;
    line @1 :Text;
}

struct NodeBuildResult {
    success @0 :Bool;
    errorMessage @1 :Text;
    # Path to the resulting .sif/archive in storage (empty on failure)
    artifactPath @2 :Text;
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
    # Whether to initialize the node with container support
    withContainer @4 :Bool;
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
    # Optional peer node root directories that should also be considered
    # when resolving this node's dependencies. Used by `node sync -a` so the
    # daemon can find sibling nodes that have not been added to the persistent
    # node stack yet. The list is consumed only for this single request and
    # never persisted.
    localPeers @2 :List(Text);
}

struct NodeSyncResponse {
    # Whether the sync was successful
    success @0 :Bool;
    # Error message if failed
    errorMessage @1 :Text;
}

struct NodeRunGoal {
    # Runtime configuration in JSON5 format (PEPPY_RUNTIME_CONFIG)
    runtimeConfigJson5 @0 :Text;
    # Name of the node to run
    nodeName @1 :Text;
    # Tag of the node to run
    tag @2 :Text;
    # Environment variables to apply when executing run_cmd (e.g. PATH)
    envVars @3 :List(EnvVar);
    # Timeout in seconds for the run operation (used to report remaining time when busy)
    timeoutSecs @4 :UInt64;
}

struct NodeRunGoalResponse {
    # Whether the goal was accepted
    accepted @0 :Bool;
    # Path to the log file (empty if rejected)
    logPath @1 :Text;
    # Rejection reason (empty if accepted)
    rejectionReason @2 :Text;
}

struct NodeRunFeedback {
    # Type of output: "stdout" or "stderr"
    stream @0 :Text;
    # The line of output
    line @1 :Text;
}

struct NodeRunResult {
    # Whether the run was successful
    success @0 :Bool;
    # Error message if failed (optional)
    errorMessage @1 :Text;
    # Process ID of the running node (0 if not available or failed)
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

# Node Info service
struct NodeInfoRequest {
    # Name of the node to look up in the stack
    nodeName @0 :Text;
    # Tag of the node to look up in the stack
    nodeTag @1 :Text;
}

struct NodeInstanceInfo {
    # Instance identifier
    instanceId @0 :Text;
    # Per-instance state: "starting" or "running"
    state @1 :Text;
}

# Node info lookup result.
#
# The response is a union so that "no such node in the stack" is a
# first-class successful outcome rather than a protocol-level error. This
# lets the `peppy node add` preflight check, `peppy node info`, and any
# other caller disambiguate "not found" from a real fault without having
# to sniff the error-string payload of an `InvalidServiceRequest`.
struct NodeInfoResponse {
    union {
        # The node is not in the stack. Carries no payload — the caller
        # already knows which `(name, tag)` it asked about.
        notInStack @0 :Void;
        # The node is in the stack. All of the node metadata is grouped
        # under this arm; callers must match on the union before reading.
        found :group {
            # JSON5-serialized NodeConfig as stored in the node stack
            configJson5 @1 :Text;
            # SHA256 of the entire NodeConfig file
            configSha256 @2 :Text;
            # Lifecycle stage of the in-stack entity ("Added"/"Building"/"Ready"/"Root").
            stage @3 :Text;
            # All tracked instances of this entity, including in-flight `Starting` ones.
            instances @4 :List(NodeInstanceInfo);
            # Path to the most-recent add/build log file for this entity.
            # Empty string when no add log has been produced yet.
            addLogPath @5 :Text;
            # Per-instance run log paths, aligned with `instances` (same order).
            runLogPaths @6 :List(Text);
        }
    }
}
